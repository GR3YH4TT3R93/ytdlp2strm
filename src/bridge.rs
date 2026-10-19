use std::collections::HashMap;
use std::sync::atomic::Ordering;

use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use serde::Deserialize;
use serde_json::Value;

use crate::hls;
use crate::state::AppState;

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Video,
    Audio,
}

impl StreamKind {
    fn best<'a>(&self, json: &'a Value) -> Option<&'a Value> {
        let formats = json.get("formats")?.as_array()?;

        let mut candidates: Vec<&Value> = formats
            .iter()
            .filter(|f| match self {
                StreamKind::Video => {
                    is_hls(f)
                        && f.get("vcodec")
                            .and_then(|v| v.as_str())
                            .map_or(false, |v| v != "none")
                        && f.get("acodec")
                            .and_then(|v| v.as_str())
                            .map_or(true, |a| a == "none")
                }
                StreamKind::Audio => {
                    is_hls(f) && f.get("vcodec").and_then(|v| v.as_str()) == Some("none")
                }
            })
            .collect();

        candidates.sort_by_key(|f| match self {
            StreamKind::Video => {
                let is_avc = f
                    .get("vcodec")
                    .and_then(|v| v.as_str())
                    .map_or(false, |v| v.starts_with("avc1")) as u64;
                let h = f.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                (is_avc, h)
            }
            StreamKind::Audio => {
                let abr = f.get("abr").and_then(|v| v.as_u64()).unwrap_or(0);
                (abr, 0)
            }
        });

        candidates.pop()
    }
}

fn is_hls(f: &Value) -> bool {
    f.get("protocol")
        .and_then(|v| v.as_str())
        .map_or(false, |p| p.contains("m3u8"))
        && f.get("manifest_url").and_then(|v| v.as_str()).is_some()
}

fn all_video_variants(json: &Value) -> Vec<&Value> {
    let Some(formats) = json.get("formats").and_then(|f| f.as_array()) else {
        return Vec::new();
    };

    let mut by_height: HashMap<u64, &Value> = HashMap::new();
    for f in formats {
        if !is_hls(f) {
            continue;
        }
        let vcodec = f.get("vcodec").and_then(|v| v.as_str()).unwrap_or("none");
        if vcodec == "none" {
            continue;
        }
        let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("none");
        if acodec != "none" {
            continue;
        }
        let height = f.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
        if height == 0 {
            continue;
        }

        let is_avc = vcodec.starts_with("avc1");
        match by_height.get(&height) {
            None => {
                by_height.insert(height, f);
            }
            Some(existing) => {
                let existing_avc = existing
                    .get("vcodec")
                    .and_then(|v| v.as_str())
                    .map(|s| s.starts_with("avc1"))
                    .unwrap_or(false);
                if is_avc && !existing_avc {
                    by_height.insert(height, f);
                }
            }
        }
    }

    let mut variants: Vec<&Value> = by_height.into_values().collect();
    variants.sort_by_key(|f| f.get("height").and_then(|v| v.as_u64()).unwrap_or(0));
    variants
}

#[derive(Debug, Deserialize)]
pub struct MediaQuery {
    fmt: Option<String>,
}

async fn build_filtered_playlist(
    state: &AppState,
    video_id: &str,
    kind: StreamKind,
    forced_format_id: Option<&str>,
) -> anyhow::Result<String> {
    let json = state
        .resolve_cache
        .get_or_resolve(&state.ytdlp, video_id)
        .await?;

    let format_id: String = if let Some(fid) = forced_format_id {
        fid.to_string()
    } else {
        let fmt = kind.best(&json).ok_or_else(|| {
            let total = json
                .get("formats")
                .and_then(|f| f.as_array())
                .map_or(0, |a| a.len());
            let hls = json
                .get("formats")
                .and_then(|f| f.as_array())
                .map(|a| a.iter().filter(|f| is_hls(f)).count())
                .unwrap_or(0);
            anyhow!(
                "no HLS {kind:?} format for {video_id} \
                 (total={total} formats, {hls} HLS with manifest)"
            )
        })?;
        fmt.get("format_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("format missing format_id"))?
            .to_string()
    };

    let video_url = format!("https://www.youtube.com/watch?v={video_id}");
    let media_manifest_url = state.ytdlp.stream_url(&format_id, &video_url).await?;

    let (playlist, raw_source, media_url) =
        hls::fetch_media_playlist(&state.http, &media_manifest_url).await?;

    let skips = state
        .sponsorblock
        .fetch(video_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(video_id, error = %e, "SponsorBlock fetch failed; serving unfiltered");
            Vec::new()
        });

    if !skips.is_empty() {
        state
            .metrics
            .sb_skips_applied
            .fetch_add(skips.len() as u64, Ordering::Relaxed);
    }

    tracing::info!(
        video_id, ?kind, format_id = %format_id,
        total_segments = playlist.segments.len(),
        skip_intervals = skips.len(),
        forced = forced_format_id.is_some(),
        "filtering playlist"
    );

    let proxy = format!("/bridge/{video_id}/segment");
    hls::filter_and_rewrite(&playlist, &raw_source, &media_url, &skips, &proxy)
}

async fn build_master_body(
    state: &AppState,
    video_id: &str,
) -> Result<String, (StatusCode, String)> {
    let json = state
        .resolve_cache
        .get_or_resolve(&state.ytdlp, video_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    let variants = all_video_variants(&json);
    if variants.is_empty() {
        return Err((StatusCode::NOT_FOUND, "no HLS video formats".to_string()));
    }

    let audio_fmt = StreamKind::Audio
        .best(&json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no HLS audio format".to_string()))?;
    let audio_format_id = audio_fmt
        .get("format_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "audio format missing id".to_string(),
            )
        })?;
    let acodec = audio_fmt
        .get("acodec")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let base = format!("/bridge/{video_id}");
    let mut body = String::new();
    body.push_str("#EXTM3U\n");
    body.push_str("#EXT-X-VERSION:3\n");
    body.push_str(&format!(
        "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",DEFAULT=YES,AUTOSELECT=YES,URI=\"{base}/audio.m3u8?fmt={audio_format_id}\"\n"
    ));

    for v in &variants {
        let Some(video_format_id) = v.get("format_id").and_then(|x| x.as_str()) else {
            continue;
        };
        let vcodec = v.get("vcodec").and_then(|x| x.as_str()).unwrap_or("");
        let height = v.get("height").and_then(|x| x.as_u64()).unwrap_or(0);
        let width = v.get("width").and_then(|x| x.as_u64()).unwrap_or(0);

        let bandwidth = v
            .get("tbr")
            .and_then(|x| x.as_f64())
            .map(|tbr| (tbr * 1000.0) as u64)
            .filter(|b| *b > 0)
            .unwrap_or_else(|| height.max(360) * 5000);

        let resolution = if width > 0 && height > 0 {
            format!(",RESOLUTION={width}x{height}")
        } else {
            String::new()
        };

        let codecs = if !vcodec.is_empty() && !acodec.is_empty() {
            format!(",CODECS=\"{vcodec},{acodec}\"")
        } else {
            String::new()
        };

        body.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth}{resolution}{codecs},AUDIO=\"aud\"\n\
             {base}/video.m3u8?fmt={video_format_id}\n"
        ));
    }

    Ok(body)
}

pub async fn bridge_master(
    AxumPath(video_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    let body = build_master_body(&state, &video_id).await?;
    state
        .metrics
        .playlists_served
        .fetch_add(1, Ordering::Relaxed);
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .body(Body::from(body))
        .unwrap())
}

pub async fn play_alias(
    AxumPath(video_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    let body = build_master_body(&state, &video_id).await?;
    state
        .metrics
        .playlists_served
        .fetch_add(1, Ordering::Relaxed);
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .body(Body::from(body))
        .unwrap())
}

pub async fn bridge_video(
    AxumPath(video_id): AxumPath<String>,
    Query(q): Query<MediaQuery>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    let body = build_filtered_playlist(&state, &video_id, StreamKind::Video, q.fmt.as_deref())
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    state
        .metrics
        .playlists_served
        .fetch_add(1, Ordering::Relaxed);
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .body(Body::from(body))
        .unwrap())
}

pub async fn bridge_audio(
    AxumPath(video_id): AxumPath<String>,
    Query(q): Query<MediaQuery>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    let body = build_filtered_playlist(&state, &video_id, StreamKind::Audio, q.fmt.as_deref())
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    state
        .metrics
        .playlists_served
        .fetch_add(1, Ordering::Relaxed);
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .body(Body::from(body))
        .unwrap())
}

pub async fn bridge_segment(
    AxumPath(_video_id): AxumPath<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    let url = params
        .get("u")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing u parameter".to_string()))?;

    let mut req = state
        .http
        .get(url)
        .header(reqwest::header::REFERER, "https://www.youtube.com/")
        .header(reqwest::header::ORIGIN, "https://www.youtube.com");

    if let Some(range) = headers.get(header::RANGE) {
        if let Ok(range_str) = range.to_str() {
            req = req.header(reqwest::header::RANGE, range_str);
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            state
                .metrics
                .segments_failed
                .fetch_add(1, Ordering::Relaxed);
            return Err((StatusCode::BAD_GATEWAY, format!("{e:#}")));
        }
    };

    let status = resp.status();
    if !status.is_success() {
        state
            .metrics
            .segments_failed
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(upstream = %status, url = %url, "segment proxy upstream error");
        return Err((StatusCode::BAD_GATEWAY, format!("upstream {status}")));
    }

    state
        .metrics
        .segments_proxied
        .fetch_add(1, Ordering::Relaxed);

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type);

    for h in [
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::CONTENT_RANGE,
        reqwest::header::ACCEPT_RANGES,
    ] {
        if let Some(v) = resp.headers().get(&h) {
            if let Ok(v) = v.to_str() {
                builder = builder.header(h.as_str(), v);
            }
        }
    }

    Ok(builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap())
}

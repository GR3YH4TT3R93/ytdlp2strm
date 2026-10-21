use std::collections::HashMap;
use std::sync::atomic::Ordering;

use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use serde_json::Value;

use crate::files::{read_cache_file, write_cache_file};
use crate::hls;
use crate::state::AppState;

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Video,
    Audio,
}

impl StreamKind {
    fn best<'a>(&self, json: &'a Value, is_twitch: bool) -> Option<&'a Value> {
        let formats = json.get("formats")?.as_array()?;

        let mut candidates: Vec<&Value> = formats
            .iter()
            .filter(|f| match self {
                StreamKind::Video => {
                    is_hls(f)
                        && f.get("vcodec")
                            .and_then(|v| v.as_str())
                            .is_some_and(|v| v != "none")
                        // Allow muxed streams for Twitch, but force video-only for YouTube
                        && (is_twitch || f.get("acodec").and_then(|v| v.as_str()).is_none_or(|a| a == "none"))
                }
                StreamKind::Audio => {
                    is_hls(f) && f.get("vcodec").and_then(|v| v.as_str()) == Some("none")
                }
            })
            .collect();

        candidates.sort_by_key(|f| match self {
            StreamKind::Video => {
                let h = f.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                let vcodec = f.get("vcodec").and_then(|v| v.as_str()).unwrap_or("");
                let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("");

                let is_video_only = (acodec == "none") as u64;
                // Force H.264 over VP9 for maximum Android compatibility
                let is_h264 = vcodec.starts_with("avc") as u64;
                (is_video_only, is_h264, h)
            }
            StreamKind::Audio => {
                let fid = f.get("format_id").and_then(|v| v.as_str()).unwrap_or("");
                let abr = f.get("abr").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
                // FIX: Explicitly prioritize AAC (140, 234) over Opus (251, 233)
                // so it works in MPEG-TS containers on Android.
                let is_aac = (fid == "140" || fid == "234") as u64;
                (is_aac, abr, 0)
            }
        });

        candidates.pop()
    }
}

fn is_hls(f: &Value) -> bool {
    f.get("protocol")
        .and_then(|v| v.as_str())
        .is_some_and(|p| p.contains("m3u8"))
        && f.get("manifest_url").and_then(|v| v.as_str()).is_some()
}

#[derive(Debug, Deserialize)]
pub struct MediaQuery {
    fmt: Option<String>,
}

pub async fn direct_redirect(
    AxumPath(video_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    let cache_key = format!("direct_{}", video_id);

    if let Some(url) = state.playlist_cache.get(&cache_key) {
        return Ok(Redirect::temporary(&url).into_response());
    }

    let format = "best";
    let url = state
        .ytdlp
        .stream_url(format, &video_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("yt-dlp failed: {e}")))?;

    state.playlist_cache.set(cache_key, url.clone());
    Ok(Redirect::temporary(&url).into_response())
}

async fn build_filtered_playlist(
    state: &AppState,
    video_id: &str,
    kind: StreamKind,
    forced_format_id: Option<&str>,
) -> anyhow::Result<String> {
    let cache_key = format!("playlist_{:?}_{:?}_{:?}", video_id, kind, forced_format_id);

    if let Some(cached) = state.playlist_cache.get(&cache_key) {
        return Ok(cached);
    }

    let disk_filename = format!(
        "playlist_{}_{:?}_{:?}.m3u8",
        video_id, kind, forced_format_id
    );
    if let Some(cached) = read_cache_file(&state.cache_dir, &disk_filename, state.cache_ttl).await {
        state.playlist_cache.set(cache_key, cached.clone());
        return Ok(cached);
    }

    let json = state
        .resolve_cache
        .get_or_resolve(&state.ytdlp, video_id, None)
        .await?;

    let is_twitch = video_id.starts_with('v');
    let format_id: String = if let Some(fid) = forced_format_id {
        fid.to_string()
    } else {
        let fmt = kind
            .best(&json, is_twitch)
            .ok_or_else(|| anyhow!("no HLS {kind:?} format"))?;
        fmt.get("format_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    let url_cache_key = format!("streamurl_{}_{}", video_id, format_id);
    let media_manifest_url = if let Some(cached_url) = state.playlist_cache.get(&url_cache_key) {
        cached_url
    } else {
        let url = state.ytdlp.stream_url(&format_id, video_id).await?;
        state.playlist_cache.set(url_cache_key, url.clone());
        url
    };

    let (manifest_result, skips_result) = tokio::join!(
        hls::fetch_media_playlist(&state.http, &media_manifest_url),
        state.sponsorblock.fetch(video_id)
    );

    let (playlist, raw_source, media_url) = manifest_result?;
    let skips = skips_result.unwrap_or_else(|e| {
        tracing::warn!(video_id, error = %e, "SponsorBlock fetch failed; serving unfiltered");
        Vec::new()
    });

    let proxy = format!("/bridge/{video_id}/segment");
    let filtered = hls::filter_and_rewrite(&playlist, &raw_source, &media_url, &skips, &proxy)?;

    state.playlist_cache.set(cache_key, filtered.clone());
    write_cache_file(&state.cache_dir, &disk_filename, &filtered).await;

    Ok(filtered)
}

async fn build_master_body(
    state: &AppState,
    video_id: &str,
) -> Result<String, (StatusCode, String)> {
    let cache_key = format!("master_{}", video_id);

    if let Some(cached) = state.playlist_cache.get(&cache_key) {
        return Ok(cached);
    }

    let disk_filename = format!("master_{}.m3u8", video_id);
    if let Some(cached) = read_cache_file(&state.cache_dir, &disk_filename, state.cache_ttl).await {
        state.playlist_cache.set(cache_key, cached.clone());
        return Ok(cached);
    }

    let json = state
        .resolve_cache
        .get_or_resolve(&state.ytdlp, video_id, None)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    let is_twitch = video_id.starts_with('v');

    let video_fmt = StreamKind::Video
        .best(&json, is_twitch)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no HLS video format".to_string()))?;

    let video_format_id = video_fmt
        .get("format_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let vcodec = video_fmt
        .get("vcodec")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let height = video_fmt
        .get("height")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let width = video_fmt.get("width").and_then(|v| v.as_u64()).unwrap_or(0);

    let acodec = video_fmt
        .get("acodec")
        .and_then(|v| v.as_str())
        .unwrap_or("none");
    let is_muxed = acodec != "none";

    let bandwidth = video_fmt
        .get("tbr")
        .and_then(|x| x.as_f64())
        .map(|tbr| (tbr * 1000.0) as u64)
        .filter(|b| *b > 0)
        .unwrap_or(5_000_000);
    let resolution = if width > 0 && height > 0 {
        format!(",RESOLUTION={width}x{height}")
    } else {
        String::new()
    };

    let base = format!("/bridge/{video_id}");

    let body = if is_muxed {
        // Twitch (muxed video + audio)
        let codecs = format!(",CODECS=\"{},{}\"", vcodec, acodec);
        format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-STREAM-INF:BANDWIDTH={bandwidth}{resolution}{codecs}\n\
             {base}/video.m3u8?fmt={video_format_id}\n"
        )
    } else {
        // YouTube (separate audio/video streams)
        let audio_fmt = StreamKind::Audio
            .best(&json, is_twitch)
            .ok_or_else(|| (StatusCode::NOT_FOUND, "no HLS audio format".to_string()))?;
        let audio_format_id = audio_fmt
            .get("format_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // FIX: Map the codec correctly based on the selected ID
        let acodec_out = if audio_format_id == "251" || audio_format_id == "233" {
            "opus"
        } else {
            "mp4a.40.2"
        };

        let mut codec_parts = Vec::new();
        if !vcodec.is_empty() {
            codec_parts.push(vcodec);
        }
        codec_parts.push(acodec_out);
        let codecs = format!(",CODECS=\"{}\"", codec_parts.join(","));

        format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",DEFAULT=YES,AUTOSELECT=YES,URI=\"{base}/audio.m3u8?fmt={audio_format_id}\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH={bandwidth}{resolution}{codecs},AUDIO=\"aud\"\n\
             {base}/video.m3u8?fmt={video_format_id}\n"
        )
    };

    state.playlist_cache.set(cache_key, body.clone());
    write_cache_file(&state.cache_dir, &disk_filename, &body).await;

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
    bridge_master(AxumPath(video_id), State(state)).await
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
    let mut req = state.http.get(url);

    if url.contains("youtube.com") || url.contains("googlevideo.com") {
        req = req
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .header(reqwest::header::ORIGIN, "https://www.youtube.com");
    } else if url.contains("twitch.tv") || url.contains("cloudfront.net") {
        req = req
            .header(reqwest::header::REFERER, "https://www.twitch.tv/")
            .header(reqwest::header::ORIGIN, "https://www.twitch.tv");
    }

    if let Some(range) = headers.get(header::RANGE)
        && let Ok(range_str) = range.to_str()
    {
        req = req.header(reqwest::header::RANGE, range_str);
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
        reqwest::header::CONTENT_RANGE,
        reqwest::header::ACCEPT_RANGES,
    ] {
        if let Some(v) = resp.headers().get(&h)
            && let Ok(v) = v.to_str()
        {
            builder = builder.header(h.as_str(), v);
        }
    }

    Ok(builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap())
}

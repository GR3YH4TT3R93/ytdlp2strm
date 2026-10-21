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

        // 1. FILTER: This only returns TRUE or FALSE to decide which streams are valid
        let mut candidates: Vec<&Value> = formats
            .iter()
            .filter(|f| match self {
                StreamKind::Video => {
                    is_hls(f)
                        && f.get("vcodec")
                            .and_then(|v| v.as_str())
                            .is_some_and(|v| v != "none")
                        && (is_twitch
                            || f.get("acodec")
                                .and_then(|v| v.as_str())
                                .is_none_or(|a| a == "none"))
                }
                StreamKind::Audio => {
                    is_hls(f) && f.get("vcodec").and_then(|v| v.as_str()) == Some("none")
                }
            })
            .collect();

        // 2. SORT: This returns the tuple to prioritize HEVC/EAC3 first
        candidates.sort_by_key(|f| match self {
            StreamKind::Video => {
                let h = f.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                let vcodec = f.get("vcodec").and_then(|v| v.as_str()).unwrap_or("");
                let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("");

                let is_video_only = (acodec == "none") as u64;

                // Priority: HEVC (2) > H.264 (1) > Others (0)
                let is_hevc = (vcodec.starts_with("hev") || vcodec.starts_with("hvc")) as u64;
                let is_h264 = vcodec.starts_with("avc") as u64;
                let codec_priority = if is_hevc == 1 {
                    2
                } else if is_h264 == 1 {
                    1
                } else {
                    0
                };

                (is_video_only, codec_priority, h)
            }
            StreamKind::Audio => {
                let fid = f.get("format_id").and_then(|v| v.as_str()).unwrap_or("");
                let abr = f.get("abr").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
                let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("");

                // Priority: EAC3 (4) > Opus (3) > AAC (2) > Others (1)
                let is_eac3 = (acodec.starts_with("eac3") || acodec.starts_with("ec-3")) as u64;
                let is_opus = (acodec.starts_with("opus") || fid == "251" || fid == "233") as u64;
                let is_aac = (acodec.starts_with("mp4a") || fid == "140" || fid == "234") as u64;

                let codec_priority = if is_eac3 == 1 {
                    4
                } else if is_opus == 1 {
                    3
                } else if is_aac == 1 {
                    2
                } else {
                    1
                };

                (codec_priority, abr, 0)
            }
        });

        candidates.pop()
    }

    fn all_audio<'a>(json: &'a Value) -> Vec<&'a Value> {
        let mut tracks = Vec::new();
        if let Some(formats) = json.get("formats").and_then(|f| f.as_array()) {
            for f in formats {
                if is_hls(f) && f.get("vcodec").and_then(|v| v.as_str()) == Some("none") {
                    tracks.push(f);
                }
            }
        }

        tracks.sort_by_key(|f| {
            let fid = f.get("format_id").and_then(|v| v.as_str()).unwrap_or("");
            let abr = f.get("abr").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
            let acodec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("");

            let is_eac3 = (acodec.starts_with("eac3")
                || acodec.starts_with("ec-3")
                || fid.contains("eac3")) as u64;
            let is_opus = (acodec.starts_with("opus") || fid == "251" || fid == "233") as u64;
            let is_aac =
                (acodec.starts_with("mp4a") || fid.starts_with("140") || fid == "234") as u64;

            let codec_priority = if is_eac3 == 1 {
                4
            } else if is_opus == 1 {
                3
            } else if is_aac == 1 {
                2
            } else {
                1
            };

            (codec_priority, abr)
        });
        tracks.reverse();

        let mut unique = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for t in tracks {
            let lang = t.get("language").and_then(|v| v.as_str()).unwrap_or("und");
            let note = t
                .get("format_note")
                .and_then(|v| v.as_str())
                .unwrap_or("Default");
            let key = format!("{}:{}", lang, note);
            if seen.insert(key) {
                unique.push(t);
            }
        }
        unique
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

    // Cap the reported bandwidth at 15 Mbps.
    // If YouTube reports an insane bitrate (e.g., 200 Mbps), Jellyfin will
    // force a transcode. By capping it, we tell Jellyfin "this is totally safe to Direct Play".
    let bandwidth = video_fmt
        .get("tbr")
        .and_then(|x| x.as_f64())
        .map(|tbr| (tbr * 1000.0) as u64)
        .filter(|b| *b > 0 && *b < 20_000_000) // Ignore insane bitrates
        .unwrap_or(55_000_000); // Default to 55 Mbps
    let resolution = if width > 0 && height > 0 {
        format!(",RESOLUTION={width}x{height}")
    } else {
        String::new()
    };

    let base = format!("/bridge/{video_id}");

    let body = if is_muxed {
        // Twitch (muxed video + audio)
        let codecs = format!(",CODECS=\"{},{}\"", vcodec, acodec);

        // Background prefetch for Twitch video
        let state_clone = state.clone();
        let video_id_clone = video_id.to_string();
        let v_fmt = video_format_id.clone();
        tokio::spawn(async move {
            let _ = build_filtered_playlist(
                &state_clone,
                &video_id_clone,
                StreamKind::Video,
                Some(&v_fmt),
            )
            .await;
        });

        format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-STREAM-INF:BANDWIDTH={bandwidth}{resolution}{codecs}\n\
             {base}/video.m3u8?fmt={video_format_id}\n"
        )
    } else {
        // YouTube (separate audio/video streams)
        let audio_tracks = StreamKind::all_audio(&json);
        let mut media_lines = Vec::new();

        let mut default_audio_codec = "mp4a.40.2";

        for (i, track) in audio_tracks.iter().enumerate() {
            let fmt_id = track
                .get("format_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lang = track
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("und");
            let note = track
                .get("format_note")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let name = if note.is_empty() || note == "medium" {
                format!("Track {} ({})", i + 1, lang)
            } else {
                note.to_string()
            };

            let is_default = if i == 0 { "YES" } else { "NO" };

            // Map the codec correctly so Jellyfin knows exactly what it's getting
            let is_eac3 = fmt_id.contains("eac3") || fmt_id.contains("ec-3");
            let is_opus = fmt_id == "251" || fmt_id == "233" || fmt_id.contains("opus");

            let track_codec = if is_eac3 {
                "ec-3"
            } else if is_opus {
                "opus"
            } else {
                "mp4a.40.2"
            };

            if i == 0 {
                default_audio_codec = track_codec;
            }

            media_lines.push(format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"{}\",LANGUAGE=\"{}\",DEFAULT={},AUTOSELECT=YES,URI=\"{base}/audio.m3u8?fmt={}\"",
                name, lang, is_default, fmt_id
            ));

            // ASYNC PREFETCH: Spawn a task to fetch EVERY audio track immediately
            let state_audio = state.clone();
            let video_id_audio = video_id.to_string();
            let a_fmt = fmt_id.to_string();
            tokio::spawn(async move {
                let _ = build_filtered_playlist(
                    &state_audio,
                    &video_id_audio,
                    StreamKind::Audio,
                    Some(&a_fmt),
                )
                .await;
            });
        }

        // Background prefetch for YouTube video
        let state_clone = state.clone();
        let video_id_clone = video_id.to_string();
        let v_fmt = video_format_id.clone();
        tokio::spawn(async move {
            let _ = build_filtered_playlist(
                &state_clone,
                &video_id_clone,
                StreamKind::Video,
                Some(&v_fmt),
            )
            .await;
        });

        let mut codec_parts = Vec::new();
        if !vcodec.is_empty() {
            // YouTube already provides the exact RFC 6381 codec string (e.g., avc1.640028 or hvc1.1.6.L120.90).
            // We must pass it exactly as-is so Jellyfin knows the exact profile.
            codec_parts.push(vcodec);
        }

        // Map the audio codec to a valid HLS string
        let audio_codec = if default_audio_codec.starts_with("mp4a") {
            "mp4a.40.2"
        } else if default_audio_codec.starts_with("opus") {
            "opus"
        } else if default_audio_codec.starts_with("ec-3") {
            "ec-3"
        } else {
            "mp4a.40.2" // fallback
        };
        codec_parts.push(audio_codec);

        let codecs = format!(",CODECS=\"{}\"", codec_parts.join(","));

        format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             {}\n\
             #EXT-X-STREAM-INF:BANDWIDTH={bandwidth}{resolution}{codecs},AUDIO=\"aud\"\n\
             {base}/video.m3u8?fmt={video_format_id}\n",
            media_lines.join("\n")
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

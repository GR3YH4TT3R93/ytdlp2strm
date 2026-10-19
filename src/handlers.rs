use std::sync::atomic::Ordering;

use crate::config::StrmMode;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::Json,
};
use serde_json::{Value, json};

use crate::files::{StrmOutcome, sanitize_filename, write_strm_file};
use crate::images::{cleanup_image_variants, download_image};
use crate::metadata::{build_episode_nfo, metadata_from_json};
use crate::state::AppState;

pub async fn debug_resolve(
    AxumPath(video_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let json = state
        .ytdlp
        .resolve(&url)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    Ok(Json(summarize_resolution(&json)))
}

fn summarize_resolution(json: &Value) -> Value {
    let formats = json.get("formats").and_then(|f| f.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|f| {
                let protocol = f.get("protocol")?.as_str().unwrap_or("unknown");
                Some(json!({
                    "format_id": f.get("format_id"),
                    "protocol": protocol,
                    "is_hls": protocol.contains("m3u8"),
                    "has_manifest_url": f.get("manifest_url").is_some(),
                    "vcodec": f.get("vcodec"),
                    "acodec": f.get("acodec"),
                    "height": f.get("height"),
                }))
            })
            .collect::<Vec<_>>()
    });

    json!({
        "id": json.get("id"),
        "title": json.get("title"),
        "duration": json.get("duration"),
        "is_live": json.get("is_live"),
        "formats": formats,
    })
}

pub async fn debug_sponsorblock(
    AxumPath(video_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let segments = state
        .sponsorblock
        .fetch(&video_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    Ok(Json(json!({
        "video_id": video_id,
        "categories": state.sponsorblock.categories(),
        "segment_count": segments.len(),
        "segments": segments
            .iter()
            .map(|s| json!({
                "start": s.start,
                "end": s.end,
                "duration": s.end - s.start,
                "category": s.category,
            }))
            .collect::<Vec<_>>(),
    })))
}

pub async fn strm_generate(
    AxumPath(video_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let json = state
        .resolve_cache
        .get_or_resolve(&state.ytdlp, &video_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    let meta = metadata_from_json(&json);
    let title = &meta.title;
    let base_url = state.config.server.address.base_url();
    let mode = state.config.strm.mode;
    let endpoint = match mode {
        StrmMode::Stream => "play",
        StrmMode::Download => "download",
    };
    let play_url = format!("{base_url}/{endpoint}/{video_id}");

    let dir = &state.config.strm.output_dir;
    let safe_title = sanitize_filename(title);

    let outcome = write_strm_file(dir, &video_id, title, &play_url, true)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write strm: {e}"),
            )
        })?;
    let strm_path = match outcome {
        StrmOutcome::Written(p) => {
            state
                .metrics
                .strm_files_written
                .fetch_add(1, Ordering::Relaxed);
            p
        }
        StrmOutcome::Skipped(p) => p,
    };

    let mut thumb_filename: Option<String> = None;
    let mut thumb_path: Option<std::path::PathBuf> = None;
    if state.config.metadata.write_thumbnails {
        if let Some(url) = json.get("thumbnail").and_then(|v| v.as_str()) {
            let base_path = dir.join(format!("{safe_title} [{video_id}]-thumb"));
            match download_image(&state.http, url, &base_path).await {
                Ok(p) => {
                    state
                        .metrics
                        .thumbnails_written
                        .fetch_add(1, Ordering::Relaxed);
                    thumb_filename = p.file_name().map(|n| n.to_string_lossy().to_string());
                    cleanup_image_variants(&base_path, &p).await;
                    thumb_path = Some(p);
                }
                Err(e) => {
                    tracing::warn!(video_id = %video_id, error = %e, "thumbnail download failed");
                }
            }
        }
    }

    let mut nfo_path: Option<std::path::PathBuf> = None;
    if state.config.metadata.write_nfo {
        let nfo_content = build_episode_nfo(&meta, 1, 1, &video_id, thumb_filename.as_deref());
        let path = dir.join(format!("{safe_title} [{video_id}].nfo"));
        tokio::fs::write(&path, nfo_content)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write nfo: {e}")))?;
        state
            .metrics
            .nfo_files_written
            .fetch_add(1, Ordering::Relaxed);
        nfo_path = Some(path);
    }

    Ok(Json(json!({
        "video_id": video_id,
        "title": title,
        "mode": mode.to_string(),
        "filename": strm_path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "path": strm_path.display().to_string(),
        "url": play_url,
        "nfo": nfo_path.map(|p| p.display().to_string()),
        "thumb": thumb_path.map(|p| p.display().to_string()),
    })))
}

pub async fn list_channels(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let root = &state.config.strm.output_dir;
    let mut rd = tokio::fs::read_dir(root)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read_dir: {e}")))?;

    let mut channels: Vec<Value> = Vec::new();

    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let mut strm_count = 0usize;
        let mut nfo_count = 0usize;
        let mut thumb_count = 0usize;
        let mut size: u64 = 0;
        let mut files: Vec<Value> = Vec::new();
        let mut has_poster = false;
        let mut has_tvshow_nfo = false;

        if let Ok(mut rd2) = tokio::fs::read_dir(&path).await {
            while let Ok(Some(e)) = rd2.next_entry().await {
                let n = e.file_name().to_string_lossy().to_string();
                let p = e.path();
                if n.starts_with("poster.") {
                    has_poster = true;
                }
                if n == "tvshow.nfo" {
                    has_tvshow_nfo = true;
                }
                if n.ends_with(".strm") {
                    strm_count += 1;
                } else if n.ends_with(".nfo") && n != "tvshow.nfo" {
                    nfo_count += 1;
                } else if n.contains("-thumb.") {
                    thumb_count += 1;
                }
                if let Ok(m) = tokio::fs::metadata(&p).await {
                    size += m.len();
                }
                files.push(json!({ "name": n }));
            }
        }

        files.sort_by(|a, b| {
            a.get("name")
                .and_then(|v| v.as_str())
                .cmp(&b.get("name").and_then(|v| v.as_str()))
        });

        let last_modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        channels.push(json!({
            "name": name,
            "path": path.display().to_string(),
            "strm_count": strm_count,
            "nfo_count": nfo_count,
            "thumb_count": thumb_count,
            "has_poster": has_poster,
            "has_tvshow_nfo": has_tvshow_nfo,
            "size_bytes": size,
            "last_modified": last_modified,
            "files": files,
        }));
    }

    channels.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .cmp(&b.get("name").and_then(|v| v.as_str()))
    });

    Ok(Json(json!({
        "output_dir": root.display().to_string(),
        "channels": channels,
    })))
}

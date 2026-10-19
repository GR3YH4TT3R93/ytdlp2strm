use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;

use crate::state::AppState;
use crate::ytdlp::DownloadOptions;

fn is_valid_video_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn find_downloaded(state: &AppState, video_id: &str) -> Option<PathBuf> {
    let cfg = state.config();
    let dir = &cfg.downloads.output_dir;
    let mut rd = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.split('.').next() else {
            continue;
        };
        if stem != video_id {
            continue;
        }
        if name.ends_with(".part") || name.ends_with(".ytdl") {
            continue;
        }
        if name.ends_with(".info.json") || name.ends_with(".temp") {
            continue;
        }
        let path = entry.path();
        if let Ok(meta) = entry.metadata().await {
            if meta.is_file() && meta.len() > 0 {
                return Some(path);
            }
        }
    }
    None
}

async fn with_video_lock<F, T>(state: &AppState, video_id: &str, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let lock = {
        let mut locks = state.video_locks.lock().unwrap();
        locks
            .entry(video_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let result = {
        let _guard = lock.lock().await;
        f.await
    };
    let mut locks = state.video_locks.lock().unwrap();
    if let Some(existing) = locks.get(video_id) {
        if Arc::strong_count(existing) <= 2 {
            locks.remove(video_id);
        }
    }
    result
}

pub async fn ensure_downloaded(state: &AppState, video_id: &str) -> anyhow::Result<PathBuf> {
    if !is_valid_video_id(video_id) {
        bail!("invalid video id: {video_id:?}");
    }

    if let Some(p) = find_downloaded(state, video_id).await {
        return Ok(p);
    }

    with_video_lock(state, video_id, async {
        if let Some(p) = find_downloaded(state, video_id).await {
            return Ok(p);
        }

        let _permit = state
            .download_sem
            .acquire()
            .await
            .context("download semaphore closed")?;

        if let Some(p) = find_downloaded(state, video_id).await {
            return Ok(p);
        }

        state
            .metrics
            .downloads_started
            .fetch_add(1, Ordering::Relaxed);

        let started = Instant::now();
        tracing::info!(video_id, "download starting");

        let cfg = state.config();
        let dir = &cfg.downloads.output_dir;
        tokio::fs::create_dir_all(dir).await.ok();
        let template = dir.join("%(id)s.%(ext)s");

        let opts = DownloadOptions {
            output_template: &template,
            sponsorblock_categories: state.sponsorblock.categories(),
            sponsorblock_api: cfg.sponsorblock.api.as_deref(),
        };

        let path = match state.ytdlp.download_video(video_id, opts).await {
            Ok(p) => p,
            Err(e) => {
                state
                    .metrics
                    .downloads_failed
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(video_id, error = %e, "download failed");
                return Err(e);
            }
        };

        let size = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        state
            .metrics
            .downloads_completed
            .fetch_add(1, Ordering::Relaxed);
        state
            .metrics
            .downloads_bytes
            .fetch_add(size, Ordering::Relaxed);

        tracing::info!(
            video_id,
            path = %path.display(),
            size_mb = size / (1024 * 1024),
            elapsed_secs = started.elapsed().as_secs(),
            "download complete"
        );

        Ok(path)
    })
    .await
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()) {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("m4a") => "audio/mp4",
        Some("opus") => "audio/opus",
        Some("mp3") => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let rest = header.trim().strip_prefix("bytes=")?;
    let range = rest.split(',').next()?.trim();
    let mut parts = range.splitn(2, '-');
    let start_str = parts.next()?.trim();
    let end_str = parts.next().unwrap_or("").trim();

    if start_str.is_empty() {
        let n: u64 = end_str.parse().ok()?;
        if n == 0 || n > size {
            return None;
        }
        Some((size - n, size - 1))
    } else if end_str.is_empty() {
        let start: u64 = start_str.parse().ok()?;
        if start >= size {
            return None;
        }
        Some((start, size - 1))
    } else {
        let start: u64 = start_str.parse().ok()?;
        let end: u64 = end_str.parse().ok()?;
        if start > end || start >= size {
            return None;
        }
        Some((start, end.min(size - 1)))
    }
}

async fn serve_file(path: &Path, headers: &HeaderMap) -> anyhow::Result<Response> {
    let meta = tokio::fs::metadata(path).await?;
    let size = meta.len();
    let content_type = mime_for(path);
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video.mp4");

    let disposition = format!("inline; filename=\"{filename}\"");

    if let Some(range_str) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        if let Some((start, end)) = parse_range(range_str, size) {
            let len = end - start + 1;
            let mut file = tokio::fs::File::open(path).await?;
            file.seek(SeekFrom::Start(start)).await?;
            let stream = ReaderStream::new(file.take(len));
            return Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, len.to_string())
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_DISPOSITION, disposition)
                .body(Body::from_stream(stream))?);
        }
    }

    let file = tokio::fs::File::open(path).await?;
    let stream = ReaderStream::new(file);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, size.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(Body::from_stream(stream))?)
}

pub async fn download_endpoint(
    AxumPath(video_id): AxumPath<String>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    if !is_valid_video_id(&video_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid video id".to_string()));
    }

    let path = ensure_downloaded(&state, &video_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    serve_file(&path, &headers)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

pub async fn cleanup_old(state: &AppState) -> anyhow::Result<()> {
    let cfg = state.config();
    let days = cfg.downloads.retention_days;
    if days == 0 {
        return Ok(());
    }
    let dir = &cfg.downloads.output_dir;
    if tokio::fs::metadata(dir).await.is_err() {
        return Ok(());
    }
    let cutoff = SystemTime::now() - Duration::from_secs(days * 86_400);
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut removed = 0usize;
    let mut freed_bytes = 0u64;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".part") || name.ends_with(".ytdl") {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if mtime < cutoff && tokio::fs::remove_file(&path).await.is_ok() {
            removed += 1;
            freed_bytes += meta.len();
        }
    }
    if removed > 0 {
        tracing::info!(
            removed,
            freed_mb = freed_bytes / (1024 * 1024),
            retention_days = days,
            "cleaned up old downloads"
        );
    }
    Ok(())
}

pub fn spawn_pre_download(state: AppState, video_id: String) {
    tokio::spawn(async move {
        if let Err(e) = ensure_downloaded(&state, &video_id).await {
            tracing::debug!(video_id = %video_id, error = %e, "pre-download failed");
        }
    });
}

#[allow(dead_code)]
pub async fn stats(state: &AppState) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let cfg = state.config();
    let dir = &cfg.downloads.output_dir;
    let mut count = 0u64;
    let mut bytes = 0u64;
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if let Ok(m) = e.metadata().await {
                if m.is_file() {
                    count += 1;
                    bytes += m.len();
                }
            }
        }
    }
    out.insert("files".to_string(), count);
    out.insert("bytes".to_string(), bytes);
    out
}

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::StrmMode;
use crate::downloads;
use crate::files::{StrmOutcome, sanitize_filename, scan_existing_files, write_strm_file};
use crate::images::{cleanup_image_variants, download_image, find_existing_image};
use crate::metadata::{build_episode_nfo, build_tvshow_nfo, get_video_metadata, xml_escape};
use crate::state::AppState;
use crate::ytdlp::ChannelInfo;

fn emit_log(logs: Option<&Arc<Mutex<Vec<String>>>>, msg: String) {
    if let Some(logs) = logs {
        if let Ok(mut l) = logs.lock() {
            l.push(msg);
            if l.len() > 200 {
                l.remove(0);
            }
        }
    }
}

struct ChannelMetaResult {
    nfo_written: usize,
    nfo_skipped: usize,
    poster_written: usize,
    poster_skipped: usize,
}

async fn write_channel_metadata(
    state: &AppState,
    channel_dir: &Path,
    info: &ChannelInfo,
    force: bool,
    logs: Option<&Arc<Mutex<Vec<String>>>>,
) -> ChannelMetaResult {
    let mut result = ChannelMetaResult {
        nfo_written: 0,
        nfo_skipped: 0,
        poster_written: 0,
        poster_skipped: 0,
    };

    let cfg = state.config();

    let mut poster_filename: Option<String> = None;
    if cfg.metadata.write_thumbnails {
        let base_path = channel_dir.join("poster");
        let existing = find_existing_image(&base_path);
        if !force && existing.is_some() {
            result.poster_skipped = 1;
            if let Some(p) = existing {
                poster_filename = p.file_name().map(|n| n.to_string_lossy().to_string());
            }
        } else if let Some(url) = &info.thumbnail_url {
            emit_log(logs, "Downloading channel poster...".to_string());
            match download_image(&state.http, url, &base_path).await {
                Ok(final_path) => {
                    result.poster_written = 1;
                    poster_filename = final_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string());
                    state
                        .metrics
                        .thumbnails_written
                        .fetch_add(1, Ordering::Relaxed);
                    cleanup_image_variants(&base_path, &final_path).await;
                }
                Err(e) => {
                    emit_log(logs, format!("Poster download failed: {e}"));
                    tracing::warn!(channel = %info.name, error = %e, "channel poster download failed");
                }
            }
        }
    }

    if cfg.metadata.write_nfo {
        let nfo_path = channel_dir.join("tvshow.nfo");
        let exists = tokio::fs::metadata(&nfo_path).await.is_ok();
        if !force && exists {
            result.nfo_skipped = 1;
        } else {
            let nfo = build_tvshow_nfo(
                &info.name,
                info.description.as_deref(),
                info.channel_id.as_deref(),
                poster_filename.as_deref(),
            );
            match tokio::fs::write(&nfo_path, nfo).await {
                Ok(_) => {
                    result.nfo_written = 1;
                    state
                        .metrics
                        .nfo_files_written
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!(channel = %info.name, error = %e, "channel nfo write failed");
                }
            }
        }
    }

    result
}

async fn strm_matches(path: &Path, expected_url: &str) -> bool {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => content.trim() == expected_url,
        Err(_) => false,
    }
}

async fn nfo_thumb_matches(nfo_path: &Path, expected: Option<&str>) -> bool {
    let Ok(content) = tokio::fs::read_to_string(nfo_path).await else {
        return false;
    };
    let existing = content.find("<thumb>").and_then(|start| {
        let rest = &content[start + 7..];
        rest.find("</thumb>").map(|end| &rest[..end])
    });
    let expected_escaped = expected.map(xml_escape);
    existing == expected_escaped.as_deref()
}

pub struct ScanResult {
    pub channel: String,
    pub channel_dir: std::path::PathBuf,
    pub total: usize,
    pub mode: StrmMode,
    pub strm_written: usize,
    pub strm_skipped: usize,
    pub nfo_written: usize,
    pub nfo_skipped: usize,
    pub thumb_written: usize,
    pub thumb_skipped: usize,
    pub channel_nfo_written: usize,
    pub channel_poster_written: usize,
    pub failures: Vec<String>,
}

pub async fn run_scan(
    state: &AppState,
    url: &str,
    limit: usize,
    force: bool,
    mode: StrmMode,
    logs: Option<&Arc<Mutex<Vec<String>>>>,
) -> anyhow::Result<ScanResult> {
    state.metrics.scans_started.fetch_add(1, Ordering::Relaxed);
    let result = run_scan_inner(state, url, limit, force, mode, logs).await;
    match &result {
        Ok(_) => {
            state
                .metrics
                .scans_completed
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            state.metrics.scans_failed.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

async fn run_scan_inner(
    state: &AppState,
    url: &str,
    limit: usize,
    force: bool,
    mode: StrmMode,
    logs: Option<&Arc<Mutex<Vec<String>>>>,
) -> anyhow::Result<ScanResult> {
    let cfg = state.config();

    emit_log(logs, format!("Resolving URL: {url}"));
    let info = state.ytdlp.list_channel(url, limit).await?;
    let total = info.entries.len();

    let msg = format!("Found {} videos in channel: {}", total, info.name);
    emit_log(logs, msg.clone());
    tracing::info!(channel = %info.name, total, limit, force, mode = %mode, "channel enumerated");

    let base_url = cfg.server.address.base_url();
    let endpoint = match mode {
        StrmMode::Stream => "play",
        StrmMode::Download => "download",
    };

    let channel_dir = cfg.strm.output_dir.join(sanitize_filename(&info.name));

    tokio::fs::create_dir_all(&channel_dir)
        .await
        .with_context(|| format!("mkdir {}", channel_dir.display()))?;

    let channel_meta = if cfg.metadata.write_nfo || cfg.metadata.write_thumbnails {
        let phase_started = Instant::now();
        let meta = write_channel_metadata(state, &channel_dir, &info, force, logs).await;
        tracing::info!(
            nfo_written = meta.nfo_written,
            nfo_skipped = meta.nfo_skipped,
            poster_written = meta.poster_written,
            poster_skipped = meta.poster_skipped,
            elapsed_ms = phase_started.elapsed().as_millis() as u64,
            "channel metadata done"
        );
        meta
    } else {
        ChannelMetaResult {
            nfo_written: 0,
            nfo_skipped: 0,
            poster_written: 0,
            poster_skipped: 0,
        }
    };

    let existing = scan_existing_files(&channel_dir).await;

    let mut strm_written = 0usize;
    let mut strm_skipped = 0usize;
    let mut nfo_written = 0usize;
    let mut nfo_skipped = 0usize;
    let mut thumb_written = 0usize;
    let mut thumb_skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    let scan_started = Instant::now();
    let pre_download = mode == StrmMode::Download && cfg.downloads.pre_download;

    for (idx, entry) in info.entries.iter().enumerate() {
        let progress = idx + 1;
        let video_started = Instant::now();

        let episode_num = progress as u32;
        let play_url = format!("{base_url}/{endpoint}/{}", entry.id);
        let safe_title = sanitize_filename(&entry.title);

        // --- STRM ---
        let new_strm_path = channel_dir.join(format!("{safe_title} [{}].strm", entry.id));
        let strm_fresh = if force {
            false
        } else if let Some(existing_path) = existing.strms.get(&entry.id) {
            strm_matches(existing_path, &play_url).await
        } else {
            false
        };

        if strm_fresh {
            strm_skipped += 1;
        } else {
            if let Some(old_path) = existing.strms.get(&entry.id) {
                if old_path != &new_strm_path {
                    let _ = tokio::fs::remove_file(old_path).await;
                }
            }
            match write_strm_file(&channel_dir, &entry.id, &entry.title, &play_url, force).await {
                Ok(outcome) => {
                    if matches!(outcome, StrmOutcome::Written(_)) {
                        strm_written += 1;
                        state
                            .metrics
                            .strm_files_written
                            .fetch_add(1, Ordering::Relaxed);
                    } else {
                        strm_skipped += 1;
                    }
                }
                Err(e) => {
                    failures.push(format!("strm {}: {e}", entry.id));
                }
            }
        }

        // --- Thumbnail (before NFO) ---
        let mut thumb_filename: Option<String> = None;
        let mut thumb_changed = false;
        if cfg.metadata.write_thumbnails {
            let old_thumb = existing.thumbs.get(&entry.id).cloned();
            if !force && old_thumb.is_some() {
                thumb_skipped += 1;
                if let Some(p) = old_thumb {
                    thumb_filename = p.file_name().map(|n| n.to_string_lossy().to_string());
                }
            } else if let Some(url) = &entry.thumbnail_url {
                let base_path = channel_dir.join(format!("{safe_title} [{}]-thumb", entry.id));
                match download_image(&state.http, url, &base_path).await {
                    Ok(final_path) => {
                        thumb_written += 1;
                        thumb_changed = true;
                        thumb_filename = final_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string());
                        state
                            .metrics
                            .thumbnails_written
                            .fetch_add(1, Ordering::Relaxed);
                        if let Some(old) = &old_thumb {
                            if old != &final_path {
                                let _ = tokio::fs::remove_file(old).await;
                            }
                        }
                        cleanup_image_variants(&base_path, &final_path).await;
                    }
                    Err(e) => {
                        failures.push(format!("thumb {}: {e}", entry.id));
                    }
                }
            }
        }

        // --- NFO (after thumbnail) ---
        if cfg.metadata.write_nfo {
            let new_nfo_path = channel_dir.join(format!("{safe_title} [{}].nfo", entry.id));
            let nfo_fresh = if force || thumb_changed {
                false
            } else if let Some(existing_nfo) = existing.nfos.get(&entry.id) {
                nfo_thumb_matches(existing_nfo, thumb_filename.as_deref()).await
            } else {
                false
            };

            if nfo_fresh {
                nfo_skipped += 1;
            } else {
                if let Some(old_path) = existing.nfos.get(&entry.id) {
                    if old_path != &new_nfo_path {
                        let _ = tokio::fs::remove_file(old_path).await;
                    }
                }
                let meta = get_video_metadata(state, entry).await;
                let nfo_content =
                    build_episode_nfo(&meta, 1, episode_num, &entry.id, thumb_filename.as_deref());
                match tokio::fs::write(&new_nfo_path, nfo_content).await {
                    Ok(_) => {
                        nfo_written += 1;
                        state
                            .metrics
                            .nfo_files_written
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        failures.push(format!("nfo {}: {e}", entry.id));
                    }
                }
            }
        }

        if pre_download {
            downloads::spawn_pre_download(state.clone(), entry.id.clone());
        }

        let elapsed_ms = video_started.elapsed().as_millis() as u64;
        tracing::info!(progress = %format!("{progress}/{total}"), video_id = %entry.id, elapsed_ms, "video processed");

        let msg = format!(
            "[{}/{}] Processed: {} ({}ms)",
            progress, total, entry.title, elapsed_ms
        );
        emit_log(logs, msg);
    }

    let elapsed_secs = scan_started.elapsed().as_secs();
    tracing::info!(
        channel = %info.name,
        total,
        mode = %mode,
        strm_written,
        strm_skipped,
        nfo_written,
        nfo_skipped,
        thumb_written,
        thumb_skipped,
        failed = failures.len(),
        elapsed_secs,
        "scan complete"
    );

    let summary = format!(
        "Scan complete in {}s! {} STRMs written, {} skipped, {} failed.",
        elapsed_secs,
        strm_written,
        strm_skipped,
        failures.len()
    );
    emit_log(logs, summary);

    Ok(ScanResult {
        channel: info.name,
        channel_dir,
        total,
        mode,
        strm_written,
        strm_skipped,
        nfo_written,
        nfo_skipped,
        thumb_written,
        thumb_skipped,
        channel_nfo_written: channel_meta.nfo_written,
        channel_poster_written: channel_meta.poster_written,
        failures,
    })
}

// --- HTTP endpoints ---

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub url: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub mode: Option<StrmMode>,
    #[serde(default)]
    pub add_to_schedule: bool,
}

const DEFAULT_SCAN_LIMIT: usize = 100;
const MAX_SCAN_LIMIT: usize = 1000;

type ScanError = (StatusCode, Json<Value>);

fn scan_err(status: StatusCode, msg: impl Into<String>) -> ScanError {
    (status, Json(json!({ "error": msg.into() })))
}

pub async fn scan_channel(
    State(state): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<Value>, ScanError> {
    let limit = req.limit.unwrap_or(DEFAULT_SCAN_LIMIT);
    if limit == 0 || limit > MAX_SCAN_LIMIT {
        return Err(scan_err(
            StatusCode::BAD_REQUEST,
            format!("limit must be 1..={MAX_SCAN_LIMIT}"),
        ));
    }
    let mode = req.mode.unwrap_or_else(|| state.config().strm.mode);

    let result = run_scan(&state, &req.url, limit, req.force, mode, None)
        .await
        .map_err(|e| scan_err(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    Ok(Json(json!({
        "channel": result.channel,
        "channel_dir": result.channel_dir.display().to_string(),
        "mode": result.mode.to_string(),
        "total": result.total,
        "strm_written": result.strm_written,
        "strm_skipped": result.strm_skipped,
        "nfo_written": result.nfo_written,
        "nfo_skipped": result.nfo_skipped,
        "thumb_written": result.thumb_written,
        "thumb_skipped": result.thumb_skipped,
        "channel_nfo_written": result.channel_nfo_written,
        "channel_poster_written": result.channel_poster_written,
        "failed": result.failures.len(),
        "errors": result.failures,
    })))
}

struct ScanGuard(Arc<Mutex<bool>>);
impl Drop for ScanGuard {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = false;
    }
}

pub async fn scan_background(
    State(state): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<Value>, ScanError> {
    let limit = req.limit.unwrap_or(DEFAULT_SCAN_LIMIT);
    if limit == 0 || limit > MAX_SCAN_LIMIT {
        return Err(scan_err(
            StatusCode::BAD_REQUEST,
            format!("limit must be 1..={MAX_SCAN_LIMIT}"),
        ));
    }
    let mode = req.mode.unwrap_or_else(|| state.config().strm.mode);

    {
        let mut running = state.scan_running.lock().unwrap();
        if *running {
            return Err(scan_err(StatusCode::CONFLICT, "A scan is already running"));
        }
        *running = true;
    }

    {
        let mut logs = state.scan_logs.lock().unwrap();
        logs.clear();
    }

    if req.add_to_schedule {
        match crate::handlers::add_schedule_source(&state, &req.url, limit).await {
            Ok(true) => emit_log(
                Some(&state.scan_logs),
                format!("Added {} to auto-schedule.", req.url),
            ),
            Ok(false) => emit_log(
                Some(&state.scan_logs),
                format!("{} is already in the auto-schedule.", req.url),
            ),
            Err(e) => emit_log(
                Some(&state.scan_logs),
                format!("Failed to update config.toml: {}", e),
            ),
        }
    }

    let guard = ScanGuard(state.scan_running.clone());
    let state_clone = state.clone();
    let url = req.url.clone();
    let force = req.force;

    tokio::spawn(async move {
        let _ = run_scan(
            &state_clone,
            &url,
            limit,
            force,
            mode,
            Some(&state_clone.scan_logs),
        )
        .await;
        drop(guard);
    });

    Ok(Json(json!({"status": "started"})))
}

pub async fn get_scan_logs(State(state): State<AppState>) -> Json<Value> {
    let running = *state.scan_running.lock().unwrap();
    let logs = state.scan_logs.lock().unwrap().clone();
    Json(json!({
        "running": running,
        "logs": logs,
    }))
}

use crate::config::StrmMode;
use crate::downloads;
use crate::files::{StrmOutcome, sanitize_filename, scan_existing_files, write_strm_file};
use crate::images::{cleanup_image_variants, download_image, find_existing_image};
use crate::metadata::{
    build_episode_nfo, build_tvshow_nfo, from_entry, get_video_metadata,
};
use crate::state::AppState;
use crate::ytdlp::ChannelInfo;
use anyhow::Context;
use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn emit_log(logs: Option<&Arc<Mutex<Vec<String>>>>, msg: String) {
    println!("{msg}");
    if let Some(logs) = logs
        && let Ok(mut l) = logs.lock() {
            l.push(msg);
            if l.len() > 200 {
                l.remove(0);
            }
        }
}

fn parse_date(date_opt: Option<&str>) -> Option<(i32, i32)> {
    let s = date_opt?;
    if s.len() >= 7 && s.contains('-') {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() >= 2 {
            let y = parts[0].parse().ok()?;
            let m = parts[1].parse().ok()?;
            return Some((y, m));
        }
    }
    if s.len() == 8 {
        let y = s[0..4].parse().ok()?;
        let m = s[4..6].parse().ok()?;
        return Some((y, m));
    }
    None
}

fn season_dir_name(season: u32) -> String {
    format!("Season {season}")
}

fn build_season_nfo(season: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<season>
  <seasonnumber>{season}</seasonnumber>
  <title>Season {season}</title>
</season>
"#
    )
}

async fn ensure_season_folder(
    channel_dir: &Path,
    season: u32,
    force: bool,
    written: &mut HashSet<u32>,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = channel_dir.join(season_dir_name(season));
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("mkdir {}", dir.display()))?;

    if written.insert(season) {
        let nfo_path = dir.join("season.nfo");
        let exists = tokio::fs::metadata(&nfo_path).await.is_ok();
        if force || !exists {
            tokio::fs::write(&nfo_path, build_season_nfo(season))
                .await
                .with_context(|| format!("write {}", nfo_path.display()))?;
        }
    }

    Ok(dir)
}

async fn load_anchor_date(channel_dir: &Path) -> Option<(i32, i32)> {
    let path = channel_dir.join("anchor_date.txt");
    if let Ok(content) = tokio::fs::read_to_string(&path).await
        && let Some(date) = parse_date(Some(content.trim()))
    {
        return Some(date);
    }
    None
}

async fn save_anchor_date(channel_dir: &Path, date: (i32, i32)) -> anyhow::Result<()> {
    let path = channel_dir.join("anchor_date.txt");
    tokio::fs::write(&path, format!("{:04}-{:02}", date.0, date.1)).await?;
    Ok(())
}

async fn earliest_nfo_date(
    nfo_paths: &std::collections::HashMap<String, std::path::PathBuf>,
) -> Option<(i32, i32)> {
    let mut earliest: Option<(i32, i32)> = None;
    for path in nfo_paths.values() {
        if let Ok(content) = tokio::fs::read_to_string(path).await
            && let Some(start) = content.find("<aired>")
            && let Some(end) = content[start..].find("</aired>")
            && let Some(date) = parse_date(content.get(start + 7..start + end).map(str::trim))
        {
            earliest = Some(match earliest {
                Some((y, m)) if date.0 < y || (date.0 == y && date.1 < m) => date,
                Some((y, m)) => (y, m),
                None => date,
            });
        }
    }
    earliest
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
    let mut fanart_filename: Option<String> = None;

    if cfg.metadata.write_thumbnails {
        let poster_base = channel_dir.join("poster");
        let existing_poster = find_existing_image(&poster_base);
        if !force && existing_poster.is_some() {
            result.poster_skipped += 1;
            if let Some(p) = existing_poster {
                poster_filename = p.file_name().map(|n| n.to_string_lossy().to_string());
            }
        } else if let Some(url) = &info.thumbnail_url {
            emit_log(logs, "Downloading high-res channel poster...".to_string());
            match download_image(&state.http, url, &poster_base).await {
                Ok(final_path) => {
                    result.poster_written += 1;
                    poster_filename = final_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string());
                    state
                        .metrics
                        .thumbnails_written
                        .fetch_add(1, Ordering::Relaxed);
                    cleanup_image_variants(&poster_base, &final_path).await;
                }
                Err(e) => {
                    emit_log(logs, format!("Poster download failed: {e}"));
                    tracing::warn!(channel = %info.name, error = %e, "channel poster download failed");
                }
            }
        }

        let fanart_base = channel_dir.join("fanart");
        let existing_fanart = find_existing_image(&fanart_base);
        if !force && existing_fanart.is_some() {
            if let Some(p) = existing_fanart {
                fanart_filename = p.file_name().map(|n| n.to_string_lossy().to_string());
            }
        } else if let Some(url) = &info.banner_url {
            emit_log(logs, "Downloading high-res channel fanart...".to_string());
            match download_image(&state.http, url, &fanart_base).await {
                Ok(final_path) => {
                    fanart_filename = final_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string());
                    state
                        .metrics
                        .thumbnails_written
                        .fetch_add(1, Ordering::Relaxed);
                    cleanup_image_variants(&fanart_base, &final_path).await;
                }
                Err(e) => {
                    emit_log(logs, format!("Fanart download failed: {e}"));
                    tracing::warn!(channel = %info.name, error = %e, "channel fanart download failed");
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
                fanart_filename.as_deref(),
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

fn path_in_dir(path: &Path, dir: &Path) -> bool {
    path.parent().map(|p| p == dir).unwrap_or(false)
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

    let mut unique_entries = Vec::new();
    let mut seen_ids = HashSet::new();
    for entry in &info.entries {
        if seen_ids.insert(entry.id.clone()) {
            unique_entries.push(entry.clone());
        }
    }
    let total = unique_entries.len();
    let msg = format!("Found {} unique videos in channel: {}", total, info.name);
    emit_log(logs, msg.clone());
    tracing::info!(channel = %info.name, total, limit, force, mode = %mode, "channel enumerated");

    let base_url = cfg.server.address.base_url();
    let endpoint = match mode {
        StrmMode::Stream => "play",
        StrmMode::Download => "download",
        StrmMode::Direct => "direct",
    };

    let channel_dir = cfg.strm.output_dir.join(sanitize_filename(&info.name));
    tokio::fs::create_dir_all(&channel_dir)
        .await
        .with_context(|| format!("mkdir {}", channel_dir.display()))?;

    // --- Twitch Live Check ---
    if url.to_lowercase().contains("twitch.tv") && !url.contains("/videos/") {
        emit_log(logs, "Checking Twitch live status...".to_string());
        if let Ok(Some(live_info)) = state.ytdlp.check_twitch_live(url).await {
            emit_log(logs, format!("Channel is LIVE: {}", live_info.title));

            let live_strm_path = channel_dir.join("!000-live-channel.strm");

            if let Err(e) = tokio::fs::write(&live_strm_path, &live_info.stream_url).await {
                tracing::warn!("Failed to write live STRM: {}", e);
            } else {
                emit_log(logs, "Wrote live stream STRM (sorts to top).".to_string());
            }
        } else {
            emit_log(logs, "Channel is not currently live.".to_string());
        }
    }

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
    let mut used_titles = HashSet::new();

    let persisted_anchor = load_anchor_date(&channel_dir).await;
    let earliest_nfo = earliest_nfo_date(&existing.nfos).await;
    let mut anchor_date: Option<(i32, i32)> = persisted_anchor.or(earliest_nfo);

    if let (Some(anchor), Some(earliest)) = (anchor_date, earliest_nfo) {
        let a_total = anchor.0 * 12 + anchor.1;
        let e_total = earliest.0 * 12 + earliest.1;
        if a_total > e_total {
            tracing::warn!(
                anchor = ?anchor,
                earliest = ?earliest,
                "persisted anchor newer than earliest NFO; correcting from library"
            );
            anchor_date = Some(earliest);
        }
    }
    if anchor_date != persisted_anchor
        && let Some(a) = anchor_date
    {
        let _ = save_anchor_date(&channel_dir, a).await;
    }

    let mut current_season = 1;
    let mut episode_in_season = 1;
    let mut seasons_written: HashSet<u32> = HashSet::new();

    for idx in (0..total).rev() {
        if state.scan_cancel.load(Ordering::Relaxed) {
            emit_log(
                logs,
                format!("Scan cancelled by user at [{}/{}].", total - idx, total),
            );
            break;
        }

        let entry = &unique_entries[idx];
        let progress = total - idx;
        let video_started = Instant::now();

        // 1. Try to read metadata directly from the existing NFO (0ms)
        let mut date = None;
        let mut cached_meta = None;
        let mut existing_nfo_content = None;

        if !force && let Some(nfo_path) = existing.nfos.get(&entry.id)
            && let Ok(content) = tokio::fs::read_to_string(nfo_path).await {
                let meta = crate::metadata::from_nfo(&content);
                if let Some(d) = meta.upload_date.as_deref() {
                    date = parse_date(Some(d));
                }
                cached_meta = Some(meta);
                existing_nfo_content = Some(content);
            }

        // 2. If we couldn't get the date from the NFO, call yt-dlp (slow)
        if date.is_none() {
            match get_video_metadata(state, entry).await {
                Ok(meta) => {
                    date = parse_date(meta.upload_date.as_deref());
                    cached_meta = Some(meta);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Sign in to confirm your age")
                        || msg.contains("Video unavailable")
                        || msg.contains("Private video")
                        || msg.contains("has been removed")
                        || msg.contains("This video is only available for registered users")
                    {
                        tracing::warn!(video_id = %entry.id, "Skipping permanent failure: {}", entry.title);
                        continue;
                    }
                    return Err(anyhow::anyhow!(
                        "no upload date for video {}: {}. Aborting scan.",
                        entry.id,
                        msg
                    ));
                }
            }
        }

        let date = date.ok_or_else(|| {
            anyhow::anyhow!(
                "yt-dlp returned no upload date for video {}. Aborting scan.",
                entry.id
            )
        })?;

        // 3. Establish / persist anchor if still missing
        if anchor_date.is_none() {
            anchor_date = Some(date);
            if let Some(a) = anchor_date {
                let _ = save_anchor_date(&channel_dir, a).await;
            }
        }

        // 4. Calculate Season
        if let Some(anchor) = anchor_date {
            let months = (date.0 - anchor.0) * 12 + (date.1 - anchor.1);
            let target_season = (months / 6).max(0) as u32 + 1;
            if target_season > current_season {
                current_season = target_season;
                episode_in_season = 1;
            }
        }

        let season = current_season;
        let episode = episode_in_season;
        episode_in_season += 1;

        let season_dir = ensure_season_folder(&channel_dir, season, force, &mut seasons_written)
            .await
            .with_context(|| format!("ensure season folder for S{season}"))?;

        let play_url = match mode {
            StrmMode::Direct => {
                let target = entry.url.as_deref().unwrap_or(&entry.id);
                state.ytdlp.stream_url("best", target).await?
            }
            _ => format!("{base_url}/{endpoint}/{}", entry.id),
        };

        let mut safe_title = sanitize_filename(&entry.title);
        if used_titles.contains(&safe_title) {
            safe_title = format!("{} [{}]", safe_title, entry.id);
        }
        used_titles.insert(safe_title.clone());

        // ---------------- STRM ----------------
        let new_strm_path = season_dir.join(format!("{safe_title}.strm"));
        let mut strm_fresh = false;
        if !force && let Some(existing_path) = existing.strms.get(&entry.id) {
            strm_fresh = path_in_dir(existing_path, &season_dir)
                && strm_matches(existing_path, &play_url).await;
        }

        if strm_fresh {
            strm_skipped += 1;
        } else {
            if let Some(old_path) = existing.strms.get(&entry.id)
                && old_path != &new_strm_path {
                    let _ = tokio::fs::remove_file(old_path).await;
                }
            match write_strm_file(&season_dir, &entry.id, &entry.title, &play_url, force).await {
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

        // ---------------- Thumbnail ----------------
        let mut thumb_filename: Option<String> = None;
        let mut thumb_changed = false;
        if cfg.metadata.write_thumbnails {
            let old_thumb = existing.thumbs.get(&entry.id).cloned();
            let thumb_in_place = old_thumb
                .as_ref()
                .map(|p| path_in_dir(p, &season_dir))
                .unwrap_or(false);

            if !force && old_thumb.is_some() && thumb_in_place {
                thumb_skipped += 1;
                if let Some(p) = old_thumb {
                    thumb_filename = p.file_name().map(|n| n.to_string_lossy().to_string());
                }
            } else if let Some(url) = &entry.thumbnail_url {
                let base_path = season_dir.join(format!("{safe_title}-thumb"));
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
                        if let Some(old) = &old_thumb
                            && old != &final_path {
                                let _ = tokio::fs::remove_file(old).await;
                            }
                        cleanup_image_variants(&base_path, &final_path).await;
                    }
                    Err(e) => {
                        failures.push(format!("thumb {}: {e}", entry.id));
                    }
                }
            }
        }

        // ---------------- Episode NFO (Healing Logic) ----------------
        if cfg.metadata.write_nfo {
            let new_nfo_path = season_dir.join(format!("{safe_title}.nfo"));

            // Fetch SponsorBlock (fast, cached 24h) to heal missing chapters
            let mut sb_segments = Vec::new();
            if let Ok(segments) = state.sponsorblock.fetch(&entry.id).await {
                sb_segments = segments;
            }

            let meta = cached_meta.unwrap_or_else(|| from_entry(entry));
            let nfo_content = build_episode_nfo(
                &meta,
                season,
                episode,
                &entry.id,
                thumb_filename.as_deref(),
                &sb_segments,
            );

            let mut needs_write = force || thumb_changed;
            let mut should_move = false;

            if !needs_write {
                if let Some(old_path) = existing.nfos.get(&entry.id) {
                    if !path_in_dir(old_path, &season_dir) {
                        needs_write = true;
                        should_move = true;
                    } else if let Some(ref content) = existing_nfo_content {
                        // Compare generated content to existing. If they differ, heal it!
                        if content.trim() != nfo_content.trim() {
                            needs_write = true;
                        }
                    } else {
                        needs_write = true;
                    }
                } else {
                    needs_write = true;
                }
            }

            if !needs_write {
                nfo_skipped += 1;
            } else {
                if should_move
                    && let Some(old_path) = existing.nfos.get(&entry.id) {
                        let _ = tokio::fs::remove_file(old_path).await;
                    }
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
        tracing::info!(
            progress = %format!("{progress}/{total}"),
            video_id = %entry.id,
            season = season,
            episode = episode,
            elapsed_ms,
            "video processed"
        );
        let msg = format!(
            "[{}/{}] Processed: {} (S{}E{})",
            progress, total, entry.title, season, episode
        );
        emit_log(logs, msg);
    }

    let channel_fully_scanned = total < limit;
    if channel_fully_scanned {
        let new_ids: HashSet<String> = unique_entries.iter().map(|e| e.id.clone()).collect();
        for (id, path) in &existing.strms {
            if !new_ids.contains(id) {
                let _ = tokio::fs::remove_file(path).await;
                if let Some(nfo_path) = existing.nfos.get(id) {
                    let _ = tokio::fs::remove_file(nfo_path).await;
                }
                if let Some(thumb_path) = existing.thumbs.get(id) {
                    let _ = tokio::fs::remove_file(thumb_path).await;
                }
            }
        }
    } else {
        tracing::warn!(
            total,
            limit,
            "skipping orphan cleanup: scan may have been truncated by limit"
        );
    }

    if let Ok(mut entries) = tokio::fs::read_dir(&channel_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Some(season_str) = name_str.strip_prefix("Season ") else {
                continue;
            };
            let Ok(season_num) = season_str.parse::<u32>() else {
                continue;
            };
            if seasons_written.contains(&season_num) {
                continue;
            }
            let mut has_media = false;
            if let Ok(mut sub) = tokio::fs::read_dir(entry.path()).await {
                while let Ok(Some(sf)) = sub.next_entry().await {
                    if let Some(fname) = sf.file_name().to_str()
                        && fname != "season.nfo" {
                            has_media = true;
                            break;
                        }
                }
            }
            if !has_media {
                let _ = tokio::fs::remove_file(entry.path().join("season.nfo")).await;
                let _ = tokio::fs::remove_dir(entry.path()).await;
            }
        }
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
        "Scan stopped or complete in {}s! {} STRMs written, {} skipped, {} failed.",
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

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

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
const MAX_SCAN_LIMIT: usize = 10_000_000;

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
            format!("limit must be between 1 and {MAX_SCAN_LIMIT}"),
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
            format!("limit must be between 1 and {MAX_SCAN_LIMIT}"),
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
    state.scan_cancel.store(false, Ordering::Relaxed);

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

pub async fn stop_scan(State(state): State<AppState>) -> Json<Value> {
    state.scan_cancel.store(true, Ordering::Relaxed);
    Json(json!({"status": "stopping"}))
}

pub async fn get_scan_logs(State(state): State<AppState>) -> Json<Value> {
    let running = *state.scan_running.lock().unwrap();
    let logs = state.scan_logs.lock().unwrap().clone();
    Json(json!({
        "running": running,
        "logs": logs,
    }))
}

use std::sync::atomic::Ordering;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{Html, Json},
};
use serde_json::{Value, json};

use crate::config::AppConfig;
use crate::files::{StrmOutcome, sanitize_filename, write_strm_file};
use crate::images::{cleanup_image_variants, download_image};
use crate::metadata::{build_episode_nfo, metadata_from_json};
use crate::state::AppState;

// --- Web UI & Config API ---------------------------------------------------

pub async fn serve_ui() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}

pub async fn get_config_api() -> Result<Json<Value>, (StatusCode, String)> {
    let path = std::path::Path::new("config.toml");
    match tokio::fs::read_to_string(path).await {
        Ok(toml_str) => {
            let cfg = toml::from_str::<AppConfig>(&toml_str).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("invalid config on disk: {e}"),
                )
            })?;
            Ok(Json(json!({
                "json": cfg,
                "toml": toml_str
            })))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read config: {e}"),
        )),
    }
}

fn update_toml_key(
    doc: &mut toml_edit::DocumentMut,
    table: &str,
    key: &str,
    value: impl Into<toml_edit::Value>,
) {
    if !doc.contains_key(table) {
        doc[table] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    if let Some(t) = doc[table].as_table_mut() {
        t[key] = toml_edit::Item::Value(value.into());
    }
}

pub async fn save_config_api(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = std::path::Path::new("config.toml");
    let old_toml_str = tokio::fs::read_to_string(path).await.unwrap_or_default();

    let new_config: AppConfig;
    let toml_out: String;

    if let Some(toml_str) = payload.get("toml").and_then(|v| v.as_str()) {
        new_config = toml::from_str(toml_str)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid TOML:\n{e}")))?;
        toml_out = toml_str.to_string();
    } else {
        new_config = serde_json::from_value(payload)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid JSON:\n{e}")))?;

        let mut doc = old_toml_str
            .parse::<toml_edit::DocumentMut>()
            .unwrap_or_default();

        update_toml_key(
            &mut doc,
            "server",
            "address",
            new_config.server.address.to_string(),
        );
        update_toml_key(
            &mut doc,
            "auth",
            "username",
            new_config.auth.username.clone(),
        );
        update_toml_key(
            &mut doc,
            "auth",
            "password",
            new_config.auth.password.clone(),
        );
        update_toml_key(
            &mut doc,
            "sponsorblock",
            "enabled",
            new_config.sponsorblock.enabled,
        );
        update_toml_key(
            &mut doc,
            "sponsorblock",
            "remove",
            new_config.sponsorblock.remove.clone(),
        );
        update_toml_key(
            &mut doc,
            "strm",
            "output_dir",
            new_config.strm.output_dir.to_string_lossy().to_string(),
        );
        update_toml_key(&mut doc, "strm", "mode", new_config.strm.mode.to_string());
        update_toml_key(
            &mut doc,
            "downloads",
            "output_dir",
            new_config
                .downloads
                .output_dir
                .to_string_lossy()
                .to_string(),
        );
        update_toml_key(
            &mut doc,
            "deps",
            "bin_dir",
            new_config.deps.bin_dir.to_string_lossy().to_string(),
        );

        toml_out = doc.to_string();
    }

    let old_config = state.config();

    if old_config.strm.output_dir != new_config.strm.output_dir {
        crate::files::move_directory(&old_config.strm.output_dir, &new_config.strm.output_dir)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to move strm dir: {e}"),
                )
            })?;
    }
    if old_config.downloads.output_dir != new_config.downloads.output_dir {
        crate::files::move_directory(
            &old_config.downloads.output_dir,
            &new_config.downloads.output_dir,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to move downloads dir: {e}"),
            )
        })?;
    }
    if old_config.deps.bin_dir != new_config.deps.bin_dir {
        crate::files::move_directory(&old_config.deps.bin_dir, &new_config.deps.bin_dir)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to move bin dir: {e}"),
                )
            })?;
    }

    if let Err(e) = tokio::fs::write(path, &toml_out).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to write config: {e}"),
        ));
    }

    *state.config.write().unwrap() = new_config;

    tracing::info!("configuration updated and applied via web UI");

    Ok(Json(json!({
        "status": "ok",
        "message": "Configuration applied immediately.",
        "toml": toml_out
    })))
}

pub async fn add_schedule_source(
    state: &AppState,
    url: &str,
    limit: usize,
) -> anyhow::Result<bool> {
    let mut current_config = state.config();
    if current_config.schedule.sources.iter().any(|s| s.url == url) {
        return Ok(false);
    }

    current_config
        .schedule
        .sources
        .push(crate::config::ScheduleSource {
            url: url.to_string(),
            limit,
        });

    let path = std::path::Path::new("config.toml");
    let existing_toml = tokio::fs::read_to_string(path).await?;
    let mut doc = existing_toml.parse::<toml_edit::DocumentMut>()?;

    if !doc.contains_key("schedule") {
        doc["schedule"] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    if let Some(schedule) = doc["schedule"].as_table_mut() {
        if !schedule.contains_key("sources") {
            schedule["sources"] =
                toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new()));
        }

        if let Some(arr) = schedule["sources"].as_array_mut() {
            let mut new_source = toml_edit::InlineTable::new();
            new_source.insert("url", toml_edit::Value::from(url));
            new_source.insert("limit", toml_edit::Value::from(limit as i64));
            arr.push(toml_edit::Value::InlineTable(new_source));
        } else if let Some(aot) = schedule["sources"].as_array_of_tables_mut() {
            let mut new_table = toml_edit::Table::new();
            new_table["url"] = toml_edit::Item::Value(toml_edit::Value::from(url));
            new_table["limit"] = toml_edit::Item::Value(toml_edit::Value::from(limit as i64));
            aot.push(new_table);
        }
    }

    tokio::fs::write(path, doc.to_string()).await?;
    *state.config.write().unwrap() = current_config;

    Ok(true)
}

// --- Debug Handlers --------------------------------------------------------

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

// --- STRM & Channels Handlers ----------------------------------------------

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
    let base_url = state.config().server.address.base_url();
    let mode = state.config().strm.mode;
    let endpoint = match mode {
        crate::config::StrmMode::Stream => "play",
        crate::config::StrmMode::Download => "download",
    };
    let play_url = format!("{base_url}/{endpoint}/{video_id}");

    let dir = &state.config().strm.output_dir;
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
    if state.config().metadata.write_thumbnails {
        if let Some(url) = json.get("thumbnail").and_then(|v| v.as_str()) {
            let base_path = dir.join(format!("{safe_title}-thumb"));
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
    if state.config().metadata.write_nfo {
        let nfo_content = build_episode_nfo(&meta, 1, 1, &video_id, thumb_filename.as_deref());
        let path = dir.join(format!("{safe_title}.nfo"));
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
    let root = &state.config().strm.output_dir;
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

mod auth;
mod bridge;
mod config;
mod deps;
mod downloads;
mod files;
mod handlers;
mod hls;
mod images;
mod metadata;
mod scan;
mod schedule;
mod sponsorblock;
mod state;
mod ytdlp;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use axum::{
    Router, middleware,
    routing::{get, post},
};
use clap::Parser;
use directories::ProjectDirs;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::config::AppConfig;
use crate::sponsorblock::SponsorBlock;
use crate::state::{AppState, Metrics, ResolveCache};
use crate::ytdlp::Ytdlp;

#[derive(Parser, Debug)]
#[command(version, about = "yt-dlp to STRM bridge server")]
struct Cli {
    /// Path to the TOML config file.
    /// Overrides standard system/user/portable resolution.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

fn resolve_paths(cli_config: Option<PathBuf>) -> (PathBuf, PathBuf) {
    // 1. Explicit CLI Argument
    if let Some(p) = cli_config {
        let abs_p = std::path::absolute(&p).unwrap_or(p.clone());
        let parent = abs_p
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        return (abs_p, parent);
    }

    // 2. Local / Portable (Current Working Directory)
    let cwd_cfg = PathBuf::from("config.toml");
    if cwd_cfg.exists() {
        return (cwd_cfg, PathBuf::from("."));
    }

    let proj_dirs = ProjectDirs::from("", "", "ytdlp2strm");

    // 3. User Config (XDG)
    if let Some(proj) = &proj_dirs {
        let xdg_cfg = proj.config_dir().join("config.toml");
        if xdg_cfg.exists() {
            return (xdg_cfg, proj.data_dir().to_path_buf());
        }
    }

    // 4. System-wide POSIX Config
    let etc_cfg = PathBuf::from("/etc/ytdlp2strm/config.toml");
    if etc_cfg.exists() {
        return (etc_cfg, PathBuf::from("/var/lib/ytdlp2strm"));
    }

    // 5. No config exists anywhere. Generate standard defaults based on user privileges.
    #[cfg(unix)]
    let is_root = unsafe { libc::geteuid() == 0 };
    #[cfg(not(unix))]
    let is_root = false;

    if is_root {
        // Generate System-wide (e.g. for a systemd service)
        (
            PathBuf::from("/etc/ytdlp2strm/config.toml"),
            PathBuf::from("/var/lib/ytdlp2strm"),
        )
    } else {
        // Generate Local/Portable (e.g. user just ran the downloaded binary)
        (PathBuf::from("config.toml"), PathBuf::from("."))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let (config_path, data_dir) = resolve_paths(cli.config);

    // Ensure data directory exists and change into it so relative paths (./strm, ./bin) resolve correctly.
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!(
            "Warning: could not create data directory at {}: {e}",
            data_dir.display()
        );
    } else if let Err(e) = std::env::set_current_dir(&data_dir) {
        eprintln!(
            "Warning: could not change working directory to {}: {e}",
            data_dir.display()
        );
    }

    // Generate default config if missing
    if !config_path.exists() {
        println!("Configuration file not found at: {}", config_path.display());
        println!("Generating a default configuration template...");

        let default_config = include_str!("default_config.toml");

        if let Some(parent) = config_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        if let Err(e) = std::fs::write(&config_path, default_config) {
            eprintln!("Failed to write default config: {e}");
            return Ok(());
        }

        println!("✅ Created {}.", config_path.display());
        println!("👉 Please edit this file to set your auth and preferences, then restart.");
        return Ok(());
    }

    // Load the config
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading config at {}", config_path.display()))?;
    let config: AppConfig = toml::from_str(&raw)
        .with_context(|| format!("parsing config at {}", config_path.display()))?;

    // Ensure Dependencies (yt-dlp, ffmpeg)
    let deps = deps::ensure_binaries(&config.deps.bin_dir, config.deps.auto_download).await?;
    let yt_extra_args = config.youtube.extra_args.clone();

    let bind = config.server.address.bind_addr();
    let base_url = config.server.address.base_url();

    let http = reqwest::Client::builder()
        .user_agent("ytdlp2strm/0.1")
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("building HTTP client")?;

    let sponsorblock = SponsorBlock::new(
        config.sponsorblock.effective_categories(),
        config.sponsorblock.api.clone(),
    );
    tracing::info!(
        enabled = sponsorblock.is_enabled(),
        categories = ?sponsorblock.categories(),
        "SponsorBlock configured"
    );

    if config.auth.is_enabled() {
        tracing::info!(user = %config.auth.username, "HTTP Basic auth enabled on admin routes");
    } else {
        tracing::info!("HTTP Basic auth disabled (no username configured)");
    }

    tracing::info!(
        strm_mode = %config.strm.mode,
        write_nfo = config.metadata.write_nfo,
        write_thumbnails = config.metadata.write_thumbnails,
        metadata_level = ?config.metadata.metadata_level,
        "configuration"
    );

    // STRM directory
    let strm_dir = config.strm.output_dir.clone();
    tokio::fs::create_dir_all(&strm_dir)
        .await
        .with_context(|| format!("creating strm dir at {}", strm_dir.display()))?;
    tracing::info!(strm_dir = %strm_dir.display(), "strm output ready");

    // Downloads directory + concurrency
    let downloads_dir = config.downloads.output_dir.clone();
    tokio::fs::create_dir_all(&downloads_dir)
        .await
        .with_context(|| format!("creating downloads dir at {}", downloads_dir.display()))?;
    let download_concurrency = config.downloads.concurrency.max(1);
    tracing::info!(
        downloads_dir = %downloads_dir.display(),
        concurrency = download_concurrency,
        retention_days = config.downloads.retention_days,
        pre_download = config.downloads.pre_download,
        "downloads configured"
    );

    let schedule_status_path = config.state.schedule_file.clone();
    let schedule_status = schedule::load_schedule_status(&schedule_status_path).await;

    let state = AppState {
        config: Arc::new(config),
        ytdlp: Ytdlp::from_path_with_args(deps.yt_dlp_binary, deps.bin_dir, yt_extra_args),
        http,
        sponsorblock,
        resolve_cache: ResolveCache::new(),
        schedule_status: Arc::new(Mutex::new(schedule_status)),
        refresh_running: Arc::new(Mutex::new(false)),
        metrics: Arc::new(Metrics::default()),
        video_locks: Arc::new(Mutex::new(HashMap::new())),
        download_sem: Arc::new(Semaphore::new(download_concurrency)),
    };

    // Startup cleanup of old downloads
    if let Err(e) = downloads::cleanup_old(&state).await {
        tracing::warn!(error = %e, "startup download cleanup failed");
    }

    tracing::info!(%bind, %base_url, "starting server");

    let _scheduler = schedule::setup(state.clone()).await?;

    let public = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(state::metrics_handler))
        .route("/bridge/{video_id}/master.m3u8", get(bridge::bridge_master))
        .route("/bridge/{video_id}/video.m3u8", get(bridge::bridge_video))
        .route("/bridge/{video_id}/audio.m3u8", get(bridge::bridge_audio))
        .route("/bridge/{video_id}/segment", get(bridge::bridge_segment))
        .route("/play/{video_id}", get(bridge::play_alias))
        .route("/download/{video_id}", get(downloads::download_endpoint))
        .route("/schedule/status", get(schedule::schedule_status_handler));

    let auth_state = state.clone();
    let protected = Router::new()
        .route("/debug/resolve/{video_id}", get(handlers::debug_resolve))
        .route(
            "/debug/sponsorblock/{video_id}",
            get(handlers::debug_sponsorblock),
        )
        .route("/strm/{video_id}", get(handlers::strm_generate))
        .route("/scan", post(scan::scan_channel))
        .route("/schedule/trigger", post(schedule::schedule_trigger))
        .route("/channels", get(handlers::list_channels))
        .route_layer(middleware::from_fn_with_state(
            auth_state,
            auth::basic_auth_mw,
        ));

    let app = public.merge(protected).with_state(state);

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding to {bind}"))?;

    axum::serve(listener, app).await?;
    Ok(())
}

mod auth;
mod bridge;
mod config;
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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use axum::{
    Router, middleware,
    routing::{get, post},
};
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::config::AppConfig;
use crate::schedule::load_schedule_status;
use crate::sponsorblock::SponsorBlock;
use crate::state::{AppState, Metrics, ResolveCache};
use crate::ytdlp::Ytdlp;

#[derive(Parser, Debug)]
#[command(version, about = "yt-dlp to STRM bridge server")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let raw = std::fs::read_to_string(&cli.config)
        .with_context(|| format!("reading config at {}", cli.config.display()))?;
    let config: AppConfig = toml::from_str(&raw)
        .with_context(|| format!("parsing config at {}", cli.config.display()))?;

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

    let yt_extra_args = config.youtube.extra_args.clone();
    if !yt_extra_args.is_empty() {
        tracing::info!(args = ?yt_extra_args, "yt-dlp extra args configured");
    }

    let schedule_status_path = config.state.schedule_file.clone();
    let schedule_status = load_schedule_status(&schedule_status_path).await;

    let state = AppState {
        config: Arc::new(config),
        ytdlp: Ytdlp::from_path_with_args(yt_extra_args),
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

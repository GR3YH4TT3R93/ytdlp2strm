mod config;
mod hls;
mod sponsorblock;
mod ytdlp;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use axum::{
    Router,
    body::Body,
    extract::{Path as AxumPath, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::Utc;
use clap::Parser;
use config::AppConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sponsorblock::SponsorBlock;
use tokio::net::TcpListener;
use tokio_cron_scheduler::{Job, JobScheduler};
use ytdlp::Ytdlp;

#[derive(Parser, Debug)]
#[command(version, about = "yt-dlp to STRM bridge server")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,
}

// --- resolve cache ---------------------------------------------------------

const RESOLVE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
struct ResolveCache(Arc<Mutex<HashMap<String, (Value, Instant)>>>);

impl ResolveCache {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    async fn get_or_resolve(&self, ytdlp: &Ytdlp, video_id: &str) -> anyhow::Result<Value> {
        {
            let cache = self.0.lock().unwrap();
            if let Some((json, ts)) = cache.get(video_id) {
                if ts.elapsed() < RESOLVE_TTL {
                    return Ok(json.clone());
                }
            }
        }
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let json = ytdlp.resolve(&url).await?;
        {
            let mut cache = self.0.lock().unwrap();
            cache.insert(video_id.to_string(), (json.clone(), Instant::now()));
        }
        Ok(json)
    }
}

// --- schedule status -------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SourceStatus {
    last_run: Option<String>,
    last_duration_ms: u64,
    last_total: usize,
    last_written: usize,
    last_skipped: usize,
    last_failed: usize,
    last_error: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ScheduleStatus {
    sources: HashMap<String, SourceStatus>,
}

async fn load_schedule_status(path: &Path) -> ScheduleStatus {
    if path.as_os_str().is_empty() {
        return ScheduleStatus::default();
    }
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "resetting schedule status (corrupt)");
            ScheduleStatus::default()
        }),
        Err(_) => ScheduleStatus::default(),
    }
}

async fn save_schedule_status(path: &Path, status: &ScheduleStatus) {
    if path.as_os_str().is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match serde_json::to_vec_pretty(status) {
        Ok(bytes) => {
            if let Err(e) = tokio::fs::write(path, bytes).await {
                tracing::warn!(path = %path.display(), error = %e, "failed to save schedule status");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to serialize schedule status"),
    }
}

// --- metrics ---------------------------------------------------------------

#[derive(Default)]
struct Metrics {
    scans_started: AtomicU64,
    scans_completed: AtomicU64,
    scans_failed: AtomicU64,
    strm_files_written: AtomicU64,
    segments_proxied: AtomicU64,
    segments_failed: AtomicU64,
    sb_skips_applied: AtomicU64,
    playlists_served: AtomicU64,
}

fn emit_metric(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
    ));
}

async fn metrics_handler(State(state): State<AppState>) -> String {
    let m = &state.metrics;
    let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mut out = String::new();
    emit_metric(
        &mut out,
        "ytdlp2strm_scans_started_total",
        "Total scans started",
        load(&m.scans_started),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_scans_completed_total",
        "Total scans completed successfully",
        load(&m.scans_completed),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_scans_failed_total",
        "Total scans that raised an error",
        load(&m.scans_failed),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_strm_files_written_total",
        "Total STRM files written",
        load(&m.strm_files_written),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_segments_proxied_total",
        "Total HLS segments proxied",
        load(&m.segments_proxied),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_segments_failed_total",
        "Total HLS segment proxy failures",
        load(&m.segments_failed),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_sb_skips_applied_total",
        "Total SponsorBlock skip intervals applied",
        load(&m.sb_skips_applied),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_playlists_served_total",
        "Total HLS playlists served",
        load(&m.playlists_served),
    );
    out
}

// --- app state -------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    ytdlp: Ytdlp,
    http: reqwest::Client,
    sponsorblock: SponsorBlock,
    resolve_cache: ResolveCache,
    schedule_status: Arc<Mutex<ScheduleStatus>>,
    refresh_running: Arc<Mutex<bool>>,
    metrics: Arc<Metrics>,
}

// --- main ------------------------------------------------------------------

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

    let strm_dir = config.strm.output_dir.clone();
    tokio::fs::create_dir_all(&strm_dir)
        .await
        .with_context(|| format!("creating strm dir at {}", strm_dir.display()))?;
    tracing::info!(strm_dir = %strm_dir.display(), "strm output ready");

    let yt_extra_args = config.youtube.extra_args.clone();
    if !yt_extra_args.is_empty() {
        tracing::info!(args = ?yt_extra_args, "yt-dlp extra args configured");
    }

    let schedule_status_path = config.state.schedule_file.clone();
    let schedule_status = load_schedule_status(&schedule_status_path).await;
    if !schedule_status_path.as_os_str().is_empty() {
        tracing::info!(
            path = %schedule_status_path.display(),
            entries = schedule_status.sources.len(),
            "schedule status loaded"
        );
    }

    let schedule_enabled = config.schedule.is_enabled();
    let state = AppState {
        config: Arc::new(config),
        ytdlp: Ytdlp::from_path_with_args(yt_extra_args),
        http,
        sponsorblock,
        resolve_cache: ResolveCache::new(),
        schedule_status: Arc::new(Mutex::new(schedule_status)),
        refresh_running: Arc::new(Mutex::new(false)),
        metrics: Arc::new(Metrics::default()),
    };

    tracing::info!(%bind, %base_url, "starting server");

    let _scheduler: Option<JobScheduler> = if schedule_enabled {
        let sched = JobScheduler::new()
            .await
            .context("initializing cron scheduler")?;

        let raw_cron = state.config.schedule.refresh_cron.clone();
        let cron = if raw_cron.split_whitespace().count() == 5 {
            format!("0 {raw_cron}")
        } else {
            raw_cron.clone()
        };

        let state_for_job = state.clone();
        let job = Job::new_async(cron.as_str(), move |_uuid, _l| {
            let state = state_for_job.clone();
            Box::pin(async move {
                run_scheduled_refresh(state).await;
            })
        })
        .with_context(|| format!("invalid cron expression: {raw_cron:?}"))?;

        let job_id = job.guid();
        sched.add(job).await.context("adding cron job")?;
        sched.start().await.context("starting cron scheduler")?;

        tracing::info!(
            cron = %cron,
            raw_cron = %raw_cron,
            sources = state.config.schedule.sources.len(),
            delay_seconds = state.config.schedule.per_source_delay_seconds,
            job_id = %job_id,
            "cron scheduler started"
        );
        Some(sched)
    } else {
        tracing::info!("cron scheduler disabled (no sources or empty refresh_cron)");
        None
    };

    // Public routes — no auth. Media servers and players hit these.
    let public = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(metrics_handler))
        .route("/bridge/{video_id}/master.m3u8", get(bridge_master))
        .route("/bridge/{video_id}/video.m3u8", get(bridge_video))
        .route("/bridge/{video_id}/audio.m3u8", get(bridge_audio))
        .route("/bridge/{video_id}/segment", get(bridge_segment))
        .route("/play/{video_id}", get(play_alias))
        .route("/schedule/status", get(schedule_status_handler));

    // Protected routes — HTTP Basic when auth is configured.
    let auth_state = state.clone();
    let protected = Router::new()
        .route("/debug/resolve/{video_id}", get(debug_resolve))
        .route("/debug/sponsorblock/{video_id}", get(debug_sponsorblock))
        .route("/strm/{video_id}", get(strm_generate))
        .route("/scan", post(scan_channel))
        .route("/schedule/trigger", post(schedule_trigger))
        .route("/channels", get(list_channels))
        .route_layer(middleware::from_fn_with_state(auth_state, basic_auth_mw));

    let app = public.merge(protected).with_state(state);

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding to {bind}"))?;

    axum::serve(listener, app).await?;
    Ok(())
}

// --- auth middleware -------------------------------------------------------

fn check_basic_auth(headers: &HeaderMap, username: &str, password: &str) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Some(value) = value.to_str().ok() else {
        return false;
    };
    let Some(b64) = value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = BASE64_STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Ok(creds) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((u, p)) = creds.split_once(':') else {
        return false;
    };
    u == username && p == password
}

async fn basic_auth_mw(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if !state.config.auth.is_enabled() {
        return next.run(req).await;
    }
    if check_basic_auth(
        req.headers(),
        &state.config.auth.username,
        &state.config.auth.password,
    ) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"ytdlp2strm\"")],
            "unauthorized",
        )
            .into_response()
    }
}

// --- debug handlers --------------------------------------------------------

async fn debug_resolve(
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

async fn debug_sponsorblock(
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

// --- bridge handlers -------------------------------------------------------

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

/// Enumerate every distinct video height among HLS video-only formats,
/// preferring AVC at each height when both AVC and non-AVC exist.
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
struct MediaQuery {
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

        // BANDWIDTH is required by the spec; derive from tbr (kbps) if
        // available, else from height as a rough heuristic.
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

async fn bridge_master(
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

async fn play_alias(
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

async fn bridge_video(
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

async fn bridge_audio(
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

async fn bridge_segment(
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

    // Forward the client's Range header so partial-segment seeks work.
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

    // Mirror upstream length/range headers so the client's player can seek.
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

// --- strm generation + scan ------------------------------------------------

#[derive(Debug)]
enum StrmOutcome {
    Written(PathBuf),
    Skipped(PathBuf),
}

async fn write_strm_file(
    dir: &Path,
    video_id: &str,
    title: &str,
    play_url: &str,
    force: bool,
) -> std::io::Result<StrmOutcome> {
    let safe_title = sanitize_filename(title);
    let filename = format!("{safe_title} [{video_id}].strm");
    let path = dir.join(filename);

    if !force && tokio::fs::metadata(&path).await.is_ok() {
        return Ok(StrmOutcome::Skipped(path));
    }

    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::write(&path, format!("{play_url}\n")).await?;
    Ok(StrmOutcome::Written(path))
}

async fn existing_strm_files(dir: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return map,
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if let Some(name) = entry.file_name().to_str() {
            if name.ends_with(".strm") {
                if let (Some(open), Some(close)) = (name.rfind(" ["), name.rfind("].strm")) {
                    if close > open + 2 {
                        map.insert(name[open + 2..close].to_string(), entry.path());
                    }
                }
            }
        }
    }
    map
}

struct ScanResult {
    channel: String,
    channel_dir: PathBuf,
    total: usize,
    written: usize,
    skipped: usize,
    failures: Vec<String>,
}

async fn run_scan(
    state: &AppState,
    url: &str,
    limit: usize,
    force: bool,
) -> anyhow::Result<ScanResult> {
    state.metrics.scans_started.fetch_add(1, Ordering::Relaxed);

    let result = run_scan_inner(state, url, limit, force).await;
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
) -> anyhow::Result<ScanResult> {
    let info = state.ytdlp.list_channel(url, limit).await?;

    let base_url = state.config.server.address.base_url();
    let channel_dir = state
        .config
        .strm
        .output_dir
        .join(sanitize_filename(&info.name));

    tokio::fs::create_dir_all(&channel_dir)
        .await
        .with_context(|| format!("mkdir {}", channel_dir.display()))?;

    let existing = existing_strm_files(&channel_dir).await;

    let mut written = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for entry in &info.entries {
        let play_url = format!("{base_url}/play/{}", entry.id);
        let target_filename = format!(
            "{} [{id}].strm",
            sanitize_filename(&entry.title),
            id = entry.id
        );
        let target_path = channel_dir.join(&target_filename);

        if let Some(existing_path) = existing.get(&entry.id) {
            if !force {
                skipped += 1;
                continue;
            }
            // Force mode: if the title changed, remove the old file so we
            // don't end up with two files for the same video.
            if existing_path != &target_path {
                if let Err(e) = tokio::fs::remove_file(existing_path).await {
                    tracing::warn!(
                        path = %existing_path.display(),
                        error = %e,
                        "failed to remove stale strm file during force rewrite"
                    );
                }
            }
        }

        match write_strm_file(&channel_dir, &entry.id, &entry.title, &play_url, force).await {
            Ok(StrmOutcome::Written(_)) => {
                written += 1;
                state
                    .metrics
                    .strm_files_written
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(StrmOutcome::Skipped(_)) => skipped += 1,
            Err(e) => failures.push(format!("{}: {e}", entry.id)),
        }
    }

    Ok(ScanResult {
        channel: info.name,
        channel_dir,
        total: info.entries.len(),
        written,
        skipped,
        failures,
    })
}

async fn strm_generate(
    AxumPath(video_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let json = state
        .resolve_cache
        .get_or_resolve(&state.ytdlp, &video_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    let title = json
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or(&video_id);

    let base_url = state.config.server.address.base_url();
    let play_url = format!("{base_url}/play/{video_id}");

    let dir = &state.config.strm.output_dir;
    let outcome = write_strm_file(dir, &video_id, title, &play_url, true)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write strm: {e}"),
            )
        })?;

    let path = match outcome {
        StrmOutcome::Written(p) => {
            state
                .metrics
                .strm_files_written
                .fetch_add(1, Ordering::Relaxed);
            p
        }
        StrmOutcome::Skipped(p) => p,
    };

    tracing::info!(
        video_id,
        path = %path.display(),
        url = %play_url,
        "strm written"
    );

    Ok(Json(json!({
        "video_id": video_id,
        "title": title,
        "filename": path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "path": path.display().to_string(),
        "url": play_url,
    })))
}

// --- channels listing ------------------------------------------------------

async fn list_channels(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
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

        let mut count = 0usize;
        let mut size: u64 = 0;
        let mut files: Vec<Value> = Vec::new();

        if let Ok(mut rd2) = tokio::fs::read_dir(&path).await {
            while let Ok(Some(e)) = rd2.next_entry().await {
                let n = e.file_name().to_string_lossy().to_string();
                if !n.ends_with(".strm") {
                    continue;
                }
                count += 1;
                if let Ok(m) = e.metadata().await {
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
            "count": count,
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

// --- scan HTTP endpoint ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct ScanRequest {
    url: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    force: bool,
}

const DEFAULT_SCAN_LIMIT: usize = 100;
const MAX_SCAN_LIMIT: usize = 1000;

type ScanError = (StatusCode, Json<Value>);

fn scan_err(status: StatusCode, msg: impl Into<String>) -> ScanError {
    (status, Json(json!({ "error": msg.into() })))
}

async fn scan_channel(
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

    tracing::info!(url = %req.url, limit, force = req.force, "scan starting");

    let result = run_scan(&state, &req.url, limit, req.force)
        .await
        .map_err(|e| scan_err(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    tracing::info!(
        channel = %result.channel,
        total = result.total,
        written = result.written,
        skipped = result.skipped,
        failed = result.failures.len(),
        "scan complete"
    );

    Ok(Json(json!({
        "channel": result.channel,
        "channel_dir": result.channel_dir.display().to_string(),
        "total": result.total,
        "written": result.written,
        "skipped": result.skipped,
        "failed": result.failures.len(),
        "errors": result.failures,
    })))
}

// --- scheduled refresh -----------------------------------------------------

struct RefreshGuard(Arc<Mutex<bool>>);

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = false;
    }
}

fn try_acquire_refresh(state: &AppState) -> Option<RefreshGuard> {
    let mut running = state.refresh_running.lock().unwrap();
    if *running {
        return None;
    }
    *running = true;
    Some(RefreshGuard(state.refresh_running.clone()))
}

async fn do_refresh(state: AppState, guard: RefreshGuard) {
    let sources = state.config.schedule.sources.clone();
    let delay = Duration::from_secs(state.config.schedule.per_source_delay_seconds);

    tracing::info!(sources = sources.len(), "refresh starting");

    for (i, source) in sources.iter().enumerate() {
        if i > 0 && !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        let started = Instant::now();
        let result = run_scan(&state, &source.url, source.limit, false).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (total, written, skipped, failed, error_msg) = match &result {
            Ok(r) => (r.total, r.written, r.skipped, r.failures.len(), None),
            Err(e) => (0, 0, 0, 0, Some(format!("{e:#}"))),
        };

        let snapshot = {
            let mut status = state.schedule_status.lock().unwrap();
            let entry = status.sources.entry(source.url.clone()).or_default();
            entry.last_run = Some(Utc::now().to_rfc3339());
            entry.last_duration_ms = elapsed_ms;
            entry.last_total = total;
            entry.last_written = written;
            entry.last_skipped = skipped;
            entry.last_failed = failed;
            entry.last_error = error_msg.clone();
            status.clone()
        };
        save_schedule_status(&state.config.state.schedule_file, &snapshot).await;

        match error_msg {
            None => tracing::info!(
                url = %source.url,
                total, written, skipped, failed,
                elapsed_ms,
                "scheduled source complete"
            ),
            Some(msg) => tracing::warn!(
                url = %source.url,
                error = %msg,
                elapsed_ms,
                "scheduled source failed"
            ),
        }
    }

    drop(guard);
    tracing::info!("refresh complete");
}

async fn run_scheduled_refresh(state: AppState) {
    match try_acquire_refresh(&state) {
        Some(guard) => do_refresh(state, guard).await,
        None => tracing::warn!("scheduled refresh skipped — previous run still in progress"),
    }
}

async fn schedule_status_handler(State(state): State<AppState>) -> Json<Value> {
    let status = state.schedule_status.lock().unwrap();
    let running = *state.refresh_running.lock().unwrap();

    let sources: Vec<Value> = state
        .config
        .schedule
        .sources
        .iter()
        .map(|s| {
            let st = status.sources.get(&s.url).cloned().unwrap_or_default();
            json!({
                "url": s.url,
                "limit": s.limit,
                "last_run": st.last_run,
                "last_duration_ms": st.last_duration_ms,
                "last_total": st.last_total,
                "last_written": st.last_written,
                "last_skipped": st.last_skipped,
                "last_failed": st.last_failed,
                "last_error": st.last_error,
            })
        })
        .collect();

    Json(json!({
        "enabled": state.config.schedule.is_enabled(),
        "refresh_cron": state.config.schedule.refresh_cron,
        "per_source_delay_seconds": state.config.schedule.per_source_delay_seconds,
        "currently_refreshing": running,
        "sources": sources,
    }))
}

async fn schedule_trigger(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    if !state.config.schedule.is_enabled() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "triggered": false,
                "reason": "schedule disabled (no sources or empty refresh_cron)",
            })),
        );
    }

    let guard = match try_acquire_refresh(&state) {
        Some(g) => g,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "triggered": false,
                    "reason": "already_refreshing",
                })),
            );
        }
    };

    let sources = state.config.schedule.sources.len();
    let state_clone = state.clone();
    tokio::spawn(async move {
        do_refresh(state_clone, guard).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "triggered": true,
            "sources": sources,
        })),
    )
}

// --- helpers ---------------------------------------------------------------

fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    let trimmed = cleaned.trim().trim_matches('.').trim();
    let capped: String = trimmed.chars().take(200).collect();
    if capped.is_empty() {
        "untitled".to_string()
    } else {
        capped
    }
}

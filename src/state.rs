use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::extract::State;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::config::AppConfig;
use crate::schedule::ScheduleStatus;
use crate::sponsorblock::SponsorBlock;
use crate::ytdlp::Ytdlp;

const RESOLVE_TTL: Duration = Duration::from_secs(300);
const MAX_CACHE_SIZE: usize = 50; // <-- Add this

#[derive(Clone)]
pub struct ResolveCache(Arc<Mutex<HashMap<String, (Value, Instant)>>>);

impl ResolveCache {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    pub async fn get_or_resolve(&self, ytdlp: &Ytdlp, video_id: &str) -> anyhow::Result<Value> {
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

            // <-- Add this eviction logic
            if cache.len() >= MAX_CACHE_SIZE {
                cache.clear();
            }

            cache.insert(video_id.to_string(), (json.clone(), Instant::now()));
        }
        Ok(json)
    }
}

#[derive(Clone)]
pub struct PlaylistCache(Arc<Mutex<HashMap<String, (String, Instant)>>>);

impl PlaylistCache {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let cache = self.0.lock().unwrap();
        if let Some((val, ts)) = cache.get(key) {
            if ts.elapsed() < Duration::from_secs(7200) {
                return Some(val.clone());
            }
        }
        None
    }

    pub fn set(&self, key: String, val: String) {
        let mut cache = self.0.lock().unwrap();
        cache.insert(key, (val, Instant::now()));
    }
}

#[derive(Default)]
pub struct Metrics {
    pub scans_started: AtomicU64,
    pub scans_completed: AtomicU64,
    pub scans_failed: AtomicU64,
    pub strm_files_written: AtomicU64,
    pub nfo_files_written: AtomicU64,
    pub thumbnails_written: AtomicU64,
    pub segments_proxied: AtomicU64,
    pub segments_failed: AtomicU64,
    pub sb_skips_applied: AtomicU64,
    pub playlists_served: AtomicU64,
    pub downloads_started: AtomicU64,
    pub downloads_completed: AtomicU64,
    pub downloads_failed: AtomicU64,
    pub downloads_bytes: AtomicU64,
}

pub type VideoLocks = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub ytdlp: Ytdlp,
    pub http: reqwest::Client,
    pub sponsorblock: SponsorBlock,
    pub resolve_cache: ResolveCache,
    pub playlist_cache: PlaylistCache,
    pub cache_dir: PathBuf,
    pub cache_ttl: Duration,
    pub schedule_status: Arc<Mutex<ScheduleStatus>>,
    pub refresh_running: Arc<Mutex<bool>>,
    pub metrics: Arc<Metrics>,
    pub video_locks: VideoLocks,
    pub download_sem: Arc<Semaphore>,
    pub scan_logs: Arc<Mutex<Vec<String>>>,
    pub scan_running: Arc<Mutex<bool>>,
    pub scan_cancel: Arc<AtomicBool>,
}

impl AppState {
    pub fn config(&self) -> AppConfig {
        self.config.read().unwrap().clone()
    }
}

fn emit_metric(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
    ));
}

pub async fn metrics_handler(State(state): State<AppState>) -> String {
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
        "ytdlp2strm_nfo_files_written_total",
        "Total NFO files written",
        load(&m.nfo_files_written),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_thumbnails_written_total",
        "Total thumbnails downloaded",
        load(&m.thumbnails_written),
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
    emit_metric(
        &mut out,
        "ytdlp2strm_downloads_started_total",
        "Total yt-dlp downloads started",
        load(&m.downloads_started),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_downloads_completed_total",
        "Total yt-dlp downloads completed",
        load(&m.downloads_completed),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_downloads_failed_total",
        "Total yt-dlp downloads that failed",
        load(&m.downloads_failed),
    );
    emit_metric(
        &mut out,
        "ytdlp2strm_downloads_bytes_total",
        "Total bytes written by downloads",
        load(&m.downloads_bytes),
    );
    out
}

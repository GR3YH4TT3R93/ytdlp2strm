use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::{extract::State, http::StatusCode, response::Json};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::scan::run_scan;
use crate::state::AppState;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub last_run: Option<String>,
    pub last_duration_ms: u64,
    pub last_total: usize,
    pub last_written: usize,
    pub last_skipped: usize,
    pub last_failed: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScheduleStatus {
    pub sources: HashMap<String, SourceStatus>,
}

pub async fn load_schedule_status(path: &Path) -> ScheduleStatus {
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

struct RefreshGuard(std::sync::Arc<Mutex<bool>>);

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
        let result = run_scan(
            &state,
            &source.url,
            source.limit,
            false,
            state.config.strm.mode,
        )
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (total, written, skipped, failed, error_msg) = match &result {
            Ok(r) => (
                r.total,
                r.strm_written,
                r.strm_skipped,
                r.failures.len(),
                None,
            ),
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

    if let Err(e) = crate::downloads::cleanup_old(&state).await {
        tracing::warn!(error = %e, "scheduled download cleanup failed");
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

pub async fn setup(state: AppState) -> anyhow::Result<Option<JobScheduler>> {
    if !state.config.schedule.is_enabled() {
        tracing::info!("cron scheduler disabled (no sources or empty refresh_cron)");
        return Ok(None);
    }

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

    Ok(Some(sched))
}

pub async fn schedule_status_handler(State(state): State<AppState>) -> Json<Value> {
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

pub async fn schedule_trigger(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
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

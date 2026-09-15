use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use crate::config::Config;
use crate::events;
use crate::forge;
use crate::github::PrRef;
use crate::llm::Llm;
use crate::metrics::METRICS;
use crate::pipeline::review::{self, ReviewArgs, RunSummary};
use crate::telemetry::Telemetry;
use crate::webhook;

/// How long a PR gets to settle after an event before its review starts —
/// rapid pushes collapse into one run (the newest event wins).
const DEBOUNCE_SECS: u64 = 5;

/// A scheduled review task plus everything needed to account for it if a
/// newer event supersedes it (the dead task can't speak for itself).
struct InflightEntry {
    handle: tokio::task::JoinHandle<()>,
    telemetry: Telemetry,
    action: String,
    force: bool,
}
/// Remembered delivery GUIDs (GitHub redelivers on timeouts/retries).
const DEDUPE_WINDOW: usize = 256;

struct ServerState {
    cfg: Config,
    webhook_secret: String,
    bot_login: String,
    log_path: String,
    seen_deliveries: Mutex<VecDeque<String>>,
    /// One in-flight review per PR; a newer event aborts and replaces it.
    /// Telemetry and trigger metadata ride along so a superseded run can
    /// still be accounted honestly after the task is gone.
    inflight: Mutex<HashMap<String, InflightEntry>>,
    /// Global cap on concurrent reviews (Bedrock throttling + spend control).
    review_slots: Arc<tokio::sync::Semaphore>,
    /// Circuit breaker: (utc_day_number, reviews_started_today).
    daily: Mutex<(u64, u32)>,
    /// Per-user last @-mention forced review (cooldown).
    mention_last: Mutex<HashMap<String, Instant>>,
}

pub async fn serve(cfg: Config, port: u16) -> Result<()> {
    // Fail at startup, not per-webhook, if the configured forge has no backend.
    forge::ensure_supported(cfg.forge)?;
    // The webhook secret: GitHub's HMAC key, or GitLab's plain X-Gitlab-Token.
    let webhook_secret = std::env::var("GREYBEARD_WEBHOOK_SECRET")
        .context("GREYBEARD_WEBHOOK_SECRET is required for serve mode")?;
    // Used to ignore our own comments on the issue_comment command channel.
    let bot_login =
        std::env::var("GREYBEARD_BOT_LOGIN").unwrap_or_else(|_| "greybeard-bot[bot]".to_string());
    let log_path =
        std::env::var("GREYBEARD_LOG_FILE").unwrap_or_else(|_| "greybeard-runs.jsonl".to_string());

    let slots = Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent_reviews));
    let state = Arc::new(ServerState {
        cfg,
        webhook_secret,
        bot_login,
        log_path,
        seen_deliveries: Mutex::new(VecDeque::new()),
        inflight: Mutex::new(HashMap::new()),
        review_slots: slots,
        daily: Mutex::new((0, 0)),
        mention_last: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/webhook", post(webhook))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("greybeard: serving on :{port}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Kubernetes and `docker stop` send SIGTERM; ctrl_c covers SIGINT.
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("installing SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = sigterm.recv() => {},
            }
        })
        .await?;
    Ok(())
}

async fn health() -> (StatusCode, axum::Json<Value>) {
    // Always 200: /health is the ALB target-group check, and a failing
    // *review* must not get the pod killed. "degraded" is for dashboards.
    (StatusCode::OK, axum::Json(METRICS.health_json(env!("CARGO_PKG_VERSION"))))
}

async fn metrics() -> (StatusCode, String) {
    (StatusCode::OK, METRICS.render())
}

async fn webhook(
    State(st): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    let forge = st.cfg.forge;
    if !webhook::verify(forge, &st.webhook_secret, &headers, &body) {
        return (StatusCode::UNAUTHORIZED, "bad signature");
    }
    // Redelivery dedupe (GitHub retries on timeout; GitLab on failure). An
    // absent id is not deduped — an empty key would collapse unrelated events.
    let delivery = webhook::delivery_id(forge, &headers);
    if !delivery.is_empty() {
        let mut seen = st.seen_deliveries.lock().unwrap();
        if seen.contains(&delivery) {
            return (StatusCode::ACCEPTED, "duplicate delivery");
        }
        seen.push_back(delivery);
        if seen.len() > DEDUPE_WINDOW {
            seen.pop_front();
        }
    }

    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "bad json");
    };

    match webhook::parse(forge, &st.bot_login, &headers, &v) {
        webhook::Verdict::Ping => (StatusCode::OK, "pong"),
        webhook::Verdict::Ignore(reason) => (StatusCode::ACCEPTED, reason),
        webhook::Verdict::Review(t) => {
            // The mention command channel is a spend lever — per-user cooldown.
            if t.force {
                let mut last = st.mention_last.lock().unwrap();
                if let Some(prev) = last.get(&t.sender) {
                    if prev.elapsed() < Duration::from_secs(st.cfg.mention_cooldown_secs) {
                        return (StatusCode::ACCEPTED, "rate limited — try again later");
                    }
                }
                last.insert(t.sender.clone(), Instant::now());
                drop(last);
                schedule(&st, t.pr, &t.action, t.force, t.installation);
                (StatusCode::ACCEPTED, "queued (forced)")
            } else {
                schedule(&st, t.pr, &t.action, t.force, t.installation);
                (StatusCode::ACCEPTED, "queued")
            }
        }
    }
}

/// Webhook payloads name their installation; prefer it over the env pin so a
/// second installation (another org) works without a config change.
pub fn installation_from_payload(v: &Value) -> Option<u64> {
    v["installation"]["id"].as_u64()
}

/// Debounced, per-PR-exclusive scheduling: a newer event for the same PR
/// aborts the in-flight run and restarts against the newest head.
fn schedule(st: &Arc<ServerState>, pr: PrRef, action: &str, force: bool, installation: Option<u64>) {
    let key = format!("{}/{}#{}", pr.owner, pr.repo, pr.number);
    let action = action.to_string();
    let action_ev = action.clone();
    let st2 = Arc::clone(st);
    let key2 = key.clone();
    // Created out here (and cloned into the inflight map) so the tokens a
    // superseded run burned survive the task's death.
    let telemetry = Telemetry::new();
    let task_telemetry = telemetry.clone();

    let handle = tokio::spawn(async move {
        let telemetry = task_telemetry;
        tokio::time::sleep(Duration::from_secs(DEBOUNCE_SECS)).await;

        // Daily circuit breaker: bounded worst-case spend no matter what the
        // webhook firehose does.
        {
            let day = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 86_400;
            let mut daily = st2.daily.lock().unwrap();
            if daily.0 != day {
                *daily = (day, 0);
            }
            if daily.1 >= st2.cfg.daily_review_limit {
                eprintln!("greybeard: daily review limit ({}) reached — skipping {key2}", st2.cfg.daily_review_limit);
                append_log(&st2.log_path, &json!({"pr": key2, "action": action, "outcome": "daily-limit"}));
                METRICS.record_run("daily-limit", 0, None, None);
                events::emit(&events::RunEvent {
                    pr: &key2, action: &action, force,
                    outcome: "daily-limit", error: None,
                    duration_s: 0.0, summary: None,
                    usage: &Default::default(), cost_usd: None,
                });
                drop(daily);
                remove_own_entry(&st2, &key2);
                return;
            }
            daily.1 += 1;
            METRICS.daily_reviews.store(daily.1 as u64, std::sync::atomic::Ordering::Relaxed);
        }

        // Global concurrency cap (never closed, so acquire cannot fail).
        let _permit = st2.review_slots.clone().acquire_owned().await.expect("semaphore open");
        let _inflight = InflightGuard::new();
        let started = Instant::now();
        // catch_unwind: a panicked review must still hit the log/event/metrics
        // paths and release its inflight entry — before this, a panic was
        // swallowed whole and the entry leaked until the next event.
        use futures::FutureExt;
        let result =
            std::panic::AssertUnwindSafe(run_review(&st2.cfg, &pr, force, installation, &telemetry))
                .catch_unwind()
                .await;
        let duration = started.elapsed();

        let (outcome, error, summary): (&str, Option<String>, Option<RunSummary>) = match result {
            Ok(Ok(s)) => (s.outcome, None, Some(s)),
            Ok(Err(e)) => ("error", Some(e.to_string()), None),
            Err(p) => {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|m| m.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown".to_string());
                ("error", Some(format!("panic: {msg}")), None)
            }
        };
        let usage = telemetry.totals();
        let cost_usd = st2.cfg.prices.map(|p| p.cost_usd(&usage));
        METRICS.record_run(outcome, duration.as_millis() as u64, Some(&usage), cost_usd);
        events::emit(&events::RunEvent {
            pr: &key2, action: &action, force,
            outcome, error: error.as_deref(),
            duration_s: duration.as_secs_f64(),
            summary: summary.as_ref(),
            usage: &usage,
            cost_usd,
        });

        let line = json!({
            "pr": key2,
            "action": action,
            "force": force,
            "outcome": outcome,
            "error": error,
        });
        eprintln!("greybeard: run {line}");
        append_log(&st2.log_path, &line);
        remove_own_entry(&st2, &key2);
    });

    let entry = InflightEntry { handle, telemetry, action: action_ev, force };
    let superseded = st.inflight.lock().unwrap().insert(key.clone(), entry);
    if let Some(old) = superseded {
        // A cancelled task's future is dropped mid-await: its InflightGuard
        // Drop runs, but none of its telemetry paths do — account for the run
        // here so superseded work is visible, not silently vanished. abort()
        // is only a REQUEST though: a task past its last await completes its
        // accounting untouched, so awaiting the handle is the one honest
        // signal — Err(cancelled) means the run died unaccounted, Ok means it
        // finished and recorded itself (recording here too would double-count).
        old.handle.abort();
        let st3 = Arc::clone(st);
        tokio::spawn(async move {
            if old.handle.await.is_err_and(|e| e.is_cancelled()) {
                // The dead run's Telemetry still holds what it burned —
                // superseded work is exactly the spend worth watching.
                let usage = old.telemetry.totals();
                let cost_usd = st3.cfg.prices.map(|p| p.cost_usd(&usage));
                METRICS.record_run("superseded", 0, Some(&usage), cost_usd);
                events::emit(&events::RunEvent {
                    pr: &key, action: &old.action, force: old.force,
                    outcome: "superseded", error: None,
                    duration_s: 0.0, summary: None,
                    usage: &usage, cost_usd,
                });
                append_log(&st3.log_path, &json!({
                    "pr": key, "action": old.action, "force": old.force,
                    "outcome": "superseded",
                }));
            }
        });
    }
}

/// RAII inflight gauge: the debounce-supersede `abort()` drops the review
/// future at an await point, so Drop is the only decrement path that
/// survives cancellation — a bare fetch_sub after the call would leak.
struct InflightGuard;

impl InflightGuard {
    fn new() -> Self {
        METRICS.inflight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        METRICS.inflight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Remove this task's `inflight` entry ONLY if it is still ours — a newer
/// event may have replaced it, and evicting the successor's handle would let
/// two reviews of the same PR run concurrently. Every task exit path must use
/// this (never a bare remove).
fn remove_own_entry(st: &ServerState, key: &str) {
    let mut inflight = st.inflight.lock().unwrap();
    if inflight.get(key).map(|e| e.handle.id()) == Some(tokio::task::id()) {
        inflight.remove(key);
    }
}

async fn run_review(
    cfg: &Config,
    pr: &PrRef,
    force: bool,
    installation: Option<u64>,
    telemetry: &Telemetry,
) -> Result<RunSummary> {
    let gh = forge::connect_installation(cfg, installation).await?;
    let llm = Llm::new(cfg.clone(), telemetry.clone()).await?;
    review::run(&gh, &llm, cfg, telemetry, pr, &ReviewArgs { dry_run: false, force }).await
}

fn append_log(path: &str, line: &Value) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// HMAC-SHA256 (RFC 2104) over the raw body; header form "sha256=<hex>".
pub fn verify_signature(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hex_sig) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Some(expected) = hex_decode(hex_sig) else {
        return false;
    };
    // verify_slice is constant-time.
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

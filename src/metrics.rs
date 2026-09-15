use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::llm::Usage;

/// Health flips to "degraded" at this many consecutive failed reviews.
const DEGRADED_AFTER: u64 = 3;

/// Process-wide counters rendered as Prometheus text on /metrics. Hand-rolled:
/// a dozen atomics don't justify a metrics crate, and the match-all Prometheus
/// in the cluster scrapes anything a ServiceMonitor points at.
pub struct Metrics {
    posted: AtomicU64,
    skipped: AtomicU64,
    errored: AtomicU64,
    daily_limited: AtomicU64,
    superseded: AtomicU64,
    duration_ms_sum: AtomicU64,
    duration_count: AtomicU64,
    tokens_input: AtomicU64,
    tokens_output: AtomicU64,
    tokens_cache_read: AtomicU64,
    tokens_cache_write: AtomicU64,
    cost_microusd: AtomicU64,
    pub inflight: AtomicU64,
    consecutive_failures: AtomicU64,
    last_error_unix: AtomicU64,
    pub daily_reviews: AtomicU64,
}

pub static METRICS: Metrics = Metrics::new();

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub const fn new() -> Self {
        Self {
            posted: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            errored: AtomicU64::new(0),
            daily_limited: AtomicU64::new(0),
            superseded: AtomicU64::new(0),
            duration_ms_sum: AtomicU64::new(0),
            duration_count: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
            tokens_cache_read: AtomicU64::new(0),
            tokens_cache_write: AtomicU64::new(0),
            cost_microusd: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            consecutive_failures: AtomicU64::new(0),
            last_error_unix: AtomicU64::new(0),
            daily_reviews: AtomicU64::new(0),
        }
    }

    /// Fold one finished run into the counters. `outcome` matches the run
    /// event vocabulary: posted | dry-run | skipped | daily-limit | superseded | error.
    pub fn record_run(
        &self,
        outcome: &str,
        duration_ms: u64,
        usage: Option<&Usage>,
        cost_usd: Option<f64>,
    ) {
        match outcome {
            "posted" | "dry-run" => self.posted.fetch_add(1, Relaxed),
            "skipped" => self.skipped.fetch_add(1, Relaxed),
            "daily-limit" => self.daily_limited.fetch_add(1, Relaxed),
            "superseded" => self.superseded.fetch_add(1, Relaxed),
            _ => self.errored.fetch_add(1, Relaxed),
        };
        match outcome {
            // Only a completed model run proves the pipeline end-to-end; a
            // pre-model skip (draft, bot PR, already-reviewed) exercises none
            // of the LLM path and must not mask real failures between events.
            "posted" | "dry-run" => self.consecutive_failures.store(0, Relaxed),
            // Neither ran nor failed — says nothing about pipeline health.
            "skipped" | "daily-limit" | "superseded" => {}
            _ => {
                self.consecutive_failures.fetch_add(1, Relaxed);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                self.last_error_unix.store(now.as_secs(), Relaxed);
            }
        }
        if outcome != "daily-limit" && outcome != "superseded" {
            self.duration_ms_sum.fetch_add(duration_ms, Relaxed);
            self.duration_count.fetch_add(1, Relaxed);
        }
        if let Some(u) = usage {
            self.tokens_input.fetch_add(u.input_tokens, Relaxed);
            self.tokens_output.fetch_add(u.output_tokens, Relaxed);
            self.tokens_cache_read.fetch_add(u.cache_read_input_tokens, Relaxed);
            self.tokens_cache_write.fetch_add(u.cache_creation_input_tokens, Relaxed);
        }
        if let Some(c) = cost_usd {
            self.cost_microusd.fetch_add((c * 1e6) as u64, Relaxed);
        }
    }

    /// Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut o = String::new();
        o.push_str("# TYPE greybeard_reviews_total counter\n");
        for (label, v) in [
            ("posted", &self.posted),
            ("skipped", &self.skipped),
            ("error", &self.errored),
            ("daily-limit", &self.daily_limited),
            ("superseded", &self.superseded),
        ] {
            o.push_str(&format!(
                "greybeard_reviews_total{{outcome=\"{label}\"}} {}\n",
                v.load(Relaxed)
            ));
        }
        o.push_str("# TYPE greybeard_review_duration_seconds summary\n");
        o.push_str(&format!(
            "greybeard_review_duration_seconds_sum {}\n",
            self.duration_ms_sum.load(Relaxed) as f64 / 1000.0
        ));
        o.push_str(&format!(
            "greybeard_review_duration_seconds_count {}\n",
            self.duration_count.load(Relaxed)
        ));
        o.push_str("# TYPE greybeard_tokens_total counter\n");
        for (kind, v) in [
            ("input", &self.tokens_input),
            ("output", &self.tokens_output),
            ("cache_read", &self.tokens_cache_read),
            ("cache_write", &self.tokens_cache_write),
        ] {
            o.push_str(&format!(
                "greybeard_tokens_total{{kind=\"{kind}\"}} {}\n",
                v.load(Relaxed)
            ));
        }
        o.push_str("# TYPE greybeard_cost_microusd_total counter\n");
        o.push_str(&format!("greybeard_cost_microusd_total {}\n", self.cost_microusd.load(Relaxed)));
        for (name, v) in [
            ("greybeard_inflight_reviews", &self.inflight),
            ("greybeard_consecutive_failures", &self.consecutive_failures),
            ("greybeard_last_error_unix", &self.last_error_unix),
            ("greybeard_daily_reviews", &self.daily_reviews),
        ] {
            o.push_str(&format!("# TYPE {name} gauge\n{name} {}\n", v.load(Relaxed)));
        }
        o
    }

    /// /health body. Always served with HTTP 200 — review failures must never
    /// make the ALB kill the pod; "degraded" is for humans and dashboards.
    pub fn health_json(&self, version: &str) -> Value {
        let failures = self.consecutive_failures.load(Relaxed);
        let last_error = self.last_error_unix.load(Relaxed);
        json!({
            "status": if failures >= DEGRADED_AFTER { "degraded" } else { "ok" },
            "version": version,
            "consecutive_failures": failures,
            "last_error_unix": if last_error == 0 { Value::Null } else { last_error.into() },
        })
    }
}

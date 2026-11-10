//! Prometheus counters for provider calls, served as text at `GET /metrics`.
//!
//! The series set is small (brand × model × step × group × outcome/reason), so a plain
//! mutex-guarded map is enough and avoids a metrics dependency. Label values come from the
//! catalog (brand, model) or from callers (step, group); every value is reduced to a bounded
//! token so one bad caller cannot inflate cardinality, and each family stops adding series at
//! [`MAX_SERIES_PER_FAMILY`] (counted in `proviz_metrics_series_dropped_total`).
//!
//! No message content, provider body or credential ever becomes a label.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::failure::FailureReason;

const MAX_SERIES_PER_FAMILY: usize = 5000;
const MAX_LABEL_LEN: usize = 96;

struct Family {
    name: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
    series: Mutex<BTreeMap<Vec<String>, u64>>,
}

impl Family {
    const fn new(name: &'static str, help: &'static str, labels: &'static [&'static str]) -> Self {
        Self {
            name,
            help,
            labels,
            series: Mutex::new(BTreeMap::new()),
        }
    }

    fn inc(&self, values: &[&str], dropped: &AtomicU64) {
        debug_assert_eq!(values.len(), self.labels.len());
        let key: Vec<String> = values.iter().map(|v| sanitize(v)).collect();
        let mut series = self.series.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = series.get_mut(&key) {
            *n += 1;
        } else if series.len() < MAX_SERIES_PER_FAMILY {
            series.insert(key, 1);
        } else {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn render(&self, out: &mut String) {
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help);
        let _ = writeln!(out, "# TYPE {} counter", self.name);
        let series = self.series.lock().unwrap_or_else(|e| e.into_inner());
        for (values, n) in series.iter() {
            let labels: Vec<String> = self
                .labels
                .iter()
                .zip(values)
                .map(|(k, v)| format!("{k}=\"{v}\""))
                .collect();
            let _ = writeln!(out, "{}{{{}}} {n}", self.name, labels.join(","));
        }
    }
}

/// Keep `[A-Za-z0-9_.:/@+-]`, map anything else to `_`, cap the length. The result needs no
/// escaping inside a Prometheus label value.
fn sanitize(v: &str) -> String {
    let mut s: String = v
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '/' | '@' | '+' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_LABEL_LEN)
        .collect();
    if s.is_empty() {
        s.push_str("none");
    }
    s
}

pub struct Metrics {
    calls: Family,
    failures: Family,
    completes: Family,
    dropped: AtomicU64,
}

/// The result of one `/complete` request as its caller saw it.
#[derive(Clone, Copy, Debug)]
pub enum CompleteResult {
    Ok,
    AllModelsExhausted,
    AllProvidersFailed,
    GroupNotFound,
    BadRequest,
    Error,
}

impl CompleteResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::AllModelsExhausted => "all_models_exhausted",
            Self::AllProvidersFailed => "all_providers_failed",
            Self::GroupNotFound => "group_not_found",
            Self::BadRequest => "bad_request",
            Self::Error => "error",
        }
    }
}

/// Identifies who asked and what served a call, for labels.
pub struct CallLabels<'a> {
    pub brand: &'a str,
    pub model: &'a str,
    pub step: &'a str,
    pub group: &'a str,
}

pub static METRICS: LazyLock<Metrics> = LazyLock::new(|| Metrics {
    calls: Family::new(
        "proviz_provider_calls_total",
        "Upstream provider calls made by /complete, by outcome (ok|error).",
        &["brand", "model", "step", "group", "outcome"],
    ),
    failures: Family::new(
        "proviz_provider_failures_total",
        "Failed upstream provider calls made by /complete, by failure reason.",
        &["brand", "model", "step", "group", "reason"],
    ),
    completes: Family::new(
        "proviz_complete_requests_total",
        "/complete requests by final result as returned to the caller.",
        &["step", "group", "result"],
    ),
    dropped: AtomicU64::new(0),
});

impl Metrics {
    pub fn record_success(&self, l: &CallLabels) {
        self.calls
            .inc(&[l.brand, l.model, l.step, l.group, "ok"], &self.dropped);
    }

    pub fn record_failure(&self, l: &CallLabels, reason: FailureReason) {
        self.calls
            .inc(&[l.brand, l.model, l.step, l.group, "error"], &self.dropped);
        self.failures.inc(
            &[l.brand, l.model, l.step, l.group, reason.as_str()],
            &self.dropped,
        );
    }

    pub fn record_complete(&self, step: &str, group: &str, result: CompleteResult) {
        self.completes
            .inc(&[step, group, result.as_str()], &self.dropped);
    }

    /// Prometheus text exposition format 0.0.4.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.calls.render(&mut out);
        self.failures.render(&mut out);
        self.completes.render(&mut out);
        let _ = writeln!(
            out,
            "# HELP proviz_metrics_series_dropped_total Increments not recorded because a family reached its series cap."
        );
        let _ = writeln!(out, "# TYPE proviz_metrics_series_dropped_total counter");
        let _ = writeln!(
            out,
            "proviz_metrics_series_dropped_total {}",
            self.dropped.load(Ordering::Relaxed)
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_bounds_label_values() {
        assert_eq!(sanitize("typesafe/jev"), "typesafe/jev");
        assert_eq!(sanitize("a\"b\\c\nd é"), "a_b_c_d__");
        assert_eq!(sanitize(""), "none");
        assert_eq!(sanitize(&"x".repeat(500)).len(), MAX_LABEL_LEN);
    }

    #[test]
    fn renders_counters_and_caps_series() {
        let m = Metrics {
            calls: Family::new(
                "t_calls_total",
                "h",
                &["brand", "model", "step", "group", "outcome"],
            ),
            failures: Family::new(
                "t_fail_total",
                "h",
                &["brand", "model", "step", "group", "reason"],
            ),
            completes: Family::new("t_complete_total", "h", &["step", "group", "result"]),
            dropped: AtomicU64::new(0),
        };
        let l = CallLabels {
            brand: "cloudflare",
            model: "typesafe/jev",
            step: "scope_check",
            group: "jev",
        };
        m.record_failure(&l, FailureReason::CreditsExhausted);
        m.record_failure(&l, FailureReason::CreditsExhausted);
        m.record_success(&l);
        m.record_complete("scope_check", "jev", CompleteResult::AllProvidersFailed);
        let text = m.render();
        assert!(text.contains(
            "t_fail_total{brand=\"cloudflare\",model=\"typesafe/jev\",step=\"scope_check\",group=\"jev\",reason=\"credits_exhausted\"} 2"
        ), "{text}");
        assert!(text.contains("outcome=\"error\"} 2"), "{text}");
        assert!(text.contains("outcome=\"ok\"} 1"), "{text}");
        assert!(
            text.contains("result=\"all_providers_failed\"} 1"),
            "{text}"
        );

        for i in 0..MAX_SERIES_PER_FAMILY + 3 {
            m.record_complete(&format!("s{i}"), "g", CompleteResult::Ok);
        }
        assert_eq!(m.dropped.load(Ordering::Relaxed), 4);
    }
}

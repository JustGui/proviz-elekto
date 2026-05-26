use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use dashmap::DashMap;
use uuid::Uuid;

use crate::models::Model;

struct WindowEntry {
    at: Instant,
    value: u64,
}

struct ModelWindows {
    rpm: VecDeque<WindowEntry>,
    tpm: VecDeque<WindowEntry>,
    rpd: VecDeque<WindowEntry>,
    tpd: VecDeque<WindowEntry>,
    /// Last value of `x-ratelimit-remaining-requests` reported by the provider.
    /// Used as a floor for effective window usage so our estimates can't drift below reality.
    provider_remaining_requests: Option<u32>,
    /// Last value of `x-ratelimit-remaining-tokens` reported by the provider.
    provider_remaining_tokens: Option<u64>,
}

impl Default for ModelWindows {
    fn default() -> Self {
        Self {
            rpm: VecDeque::new(),
            tpm: VecDeque::new(),
            rpd: VecDeque::new(),
            tpd: VecDeque::new(),
            provider_remaining_requests: None,
            provider_remaining_tokens: None,
        }
    }
}

struct ModelUsage {
    in_flight_requests: AtomicU32,
    in_flight_tokens: AtomicU64,
    windows: Mutex<ModelWindows>,
}

impl Default for ModelUsage {
    fn default() -> Self {
        Self {
            in_flight_requests: AtomicU32::new(0),
            in_flight_tokens: AtomicU64::new(0),
            windows: Mutex::new(ModelWindows::default()),
        }
    }
}

/// Proactive per-model quota tracker using sliding windows + atomic in-flight counters.
///
/// Works alongside `RateLimitState` (reactive 429 blocking). This layer prevents
/// over-booking models before any 429 fires by tracking estimated usage.
pub struct UsageTracker {
    map: DashMap<Uuid, Arc<ModelUsage>>,
}

impl Default for UsageTracker {
    fn default() -> Self {
        Self {
            map: DashMap::new(),
        }
    }
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    // Returns Arc so the DashMap shard lock is released before taking Mutex<ModelWindows>.
    fn get_or_default(&self, model_id: Uuid) -> Arc<ModelUsage> {
        if let Some(entry) = self.map.get(&model_id) {
            return entry.value().clone();
        }
        self.map
            .entry(model_id)
            .or_insert_with(|| Arc::new(ModelUsage::default()))
            .value()
            .clone()
    }

    /// Reserve a slot before returning a ModelCandidate. Lock-free (atomic only).
    pub fn reserve(&self, model_id: Uuid, estimated_tokens: u64) {
        let usage = self.get_or_default(model_id);
        usage.in_flight_requests.fetch_add(1, Ordering::Relaxed);
        usage
            .in_flight_tokens
            .fetch_add(estimated_tokens, Ordering::Relaxed);
    }

    /// Release a reservation and push actual usage into the sliding windows.
    /// Call from every report_* path. `estimated_tokens` must match what was passed to `reserve`.
    pub fn release(&self, model_id: Uuid, estimated_tokens: u64, actual_tokens: Option<u64>) {
        let usage = self.get_or_default(model_id);
        usage
            .in_flight_requests
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
        usage
            .in_flight_tokens
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(estimated_tokens))
            })
            .ok();

        let token_count = actual_tokens.unwrap_or(estimated_tokens);
        let now = Instant::now();
        let mut w = usage.windows.lock().unwrap();
        w.rpm.push_back(WindowEntry { at: now, value: 1 });
        w.tpm.push_back(WindowEntry {
            at: now,
            value: token_count,
        });
        w.rpd.push_back(WindowEntry { at: now, value: 1 });
        w.tpd.push_back(WindowEntry {
            at: now,
            value: token_count,
        });
    }

    /// Record provider-reported remaining capacity from response headers.
    ///
    /// Called alongside `release()` on every successful response. The stored values are used
    /// in `headroom()` as a floor: `effective_used = max(window_sum, limit - remaining)`.
    /// This prevents our window estimates from drifting below what the provider actually sees.
    pub fn anchor_remaining(
        &self,
        model_id: Uuid,
        remaining_requests: Option<u32>,
        remaining_tokens: Option<u64>,
    ) {
        if remaining_requests.is_none() && remaining_tokens.is_none() {
            return;
        }
        let usage = self.get_or_default(model_id);
        let mut w = usage.windows.lock().unwrap();
        if let Some(r) = remaining_requests {
            w.provider_remaining_requests = Some(r);
        }
        if let Some(t) = remaining_tokens {
            w.provider_remaining_tokens = Some(t);
        }
    }

    /// Earliest time (ms from now) at which any of the supplied models may regain positive headroom,
    /// based on the oldest sliding-window entry leaving the 60-second RPM/TPM window.
    ///
    /// Returns `Some(ms)` if any model has window entries that haven't expired yet.
    /// Returns `Some(2_000)` as a conservative hint when models are blocked only by in-flight
    /// reservations (no window entries) — those slots free up when current LLM calls complete.
    /// Returns `None` if `ids` is empty.
    pub fn earliest_drain_ms_for(&self, ids: &[Uuid]) -> Option<u64> {
        if ids.is_empty() {
            return None;
        }
        let now = Instant::now();
        let window_dur = Duration::from_secs(60);
        let mut min_ms: Option<u64> = None;
        let mut found_inflight_only = false;

        for id in ids {
            let arc = match self.map.get(id) {
                Some(entry) => entry.value().clone(),
                None => {
                    // No tracked usage at all — blocked only by a just-reserved in-flight slot.
                    found_inflight_only = true;
                    continue;
                }
            };

            let w = arc.windows.lock().unwrap();
            let oldest_rpm = w.rpm.front().map(|e| e.at);
            let oldest_tpm = w.tpm.front().map(|e| e.at);
            let oldest = match (oldest_rpm, oldest_tpm) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => {
                    // Windows are empty; blocked by in-flight only.
                    found_inflight_only = true;
                    continue;
                }
            };

            if let Some(oldest_at) = oldest {
                let expiry = oldest_at + window_dur;
                if let Some(remaining) = expiry.checked_duration_since(now) {
                    // +1 ms so callers that sleep exactly this long clear the boundary.
                    let ms = remaining.as_millis() as u64 + 1;
                    min_ms = Some(min_ms.map_or(ms, |prev| prev.min(ms)));
                }
                // else: already expired — this model's window is actually clear now.
            }
        }

        if min_ms.is_none() && found_inflight_only {
            // All blocked models have no window history; hint a short wait for in-flight to finish.
            Some(2_000)
        } else {
            min_ms
        }
    }

    /// Minimum headroom across all applicable limits for a candidate request.
    ///
    /// - Positive: room remaining; 1.0 = fully unconstrained.
    /// - 0.0: exactly last slot (eligible but deprioritised by scoring).
    /// - Negative: over capacity → caller must skip this model.
    ///
    /// Drains expired window entries as a side-effect (amortised cleanup).
    pub fn headroom(&self, model_id: Uuid, estimated_tokens: u64, model: &Model) -> f32 {
        let usage = self.get_or_default(model_id);
        let in_flight_req = usage.in_flight_requests.load(Ordering::Relaxed) as u64;
        let in_flight_tok = usage.in_flight_tokens.load(Ordering::Relaxed);

        let mut w = usage.windows.lock().unwrap();
        let now = Instant::now();

        drain_before(&mut w.rpm, now, 60);
        drain_before(&mut w.tpm, now, 60);
        drain_before(&mut w.rpd, now, 86_400);
        drain_before(&mut w.tpd, now, 86_400);

        let rpm_count = window_sum(&w.rpm);
        let tpm_sum = window_sum(&w.tpm);
        let rpd_count = window_sum(&w.rpd);
        let tpd_sum = window_sum(&w.tpd);

        // Use provider-reported remaining as a floor: if the provider says fewer requests/tokens
        // remain than our window suggests, trust the provider. In-flight (not yet acknowledged
        // by the provider) is always added on top.
        let effective_rpm =
            if let (Some(rem), Some(limit)) = (w.provider_remaining_requests, model.rpm_limit) {
                let provider_used = (limit as u64).saturating_sub(rem as u64);
                rpm_count.max(provider_used)
            } else {
                rpm_count
            };

        let effective_tpm =
            if let (Some(rem), Some(limit)) = (w.provider_remaining_tokens, model.tpm_limit) {
                let provider_used = (limit as u64).saturating_sub(rem as u64);
                tpm_sum.max(provider_used)
            } else {
                tpm_sum
            };

        let mut min_hr: f32 = 1.0;

        if let Some(limit) = model.rpm_limit {
            let projected = effective_rpm + in_flight_req + 1;
            min_hr = min_hr.min(headroom_ratio(projected, limit as u64));
        }
        if let Some(limit) = model.tpm_limit {
            let projected = effective_tpm + in_flight_tok + estimated_tokens;
            min_hr = min_hr.min(headroom_ratio(projected, limit as u64));
        }
        if let Some(limit) = model.rpd_limit {
            let projected = rpd_count + in_flight_req + 1;
            min_hr = min_hr.min(headroom_ratio(projected, limit as u64));
        }
        if let Some(limit) = model.tpd_limit {
            let projected = tpd_sum + in_flight_tok + estimated_tokens;
            min_hr = min_hr.min(headroom_ratio(projected, limit as u64));
        }

        min_hr
    }
}

fn drain_before(window: &mut VecDeque<WindowEntry>, now: Instant, window_secs: u64) {
    let cutoff = now - Duration::from_secs(window_secs);
    while window.front().map(|e| e.at < cutoff).unwrap_or(false) {
        window.pop_front();
    }
}

fn window_sum(window: &VecDeque<WindowEntry>) -> u64 {
    window.iter().map(|e| e.value).sum()
}

/// Returns an unclamped ratio. Zero = last slot (still eligible). Negative = over capacity.
/// Callers must filter on `< 0.0` and score with `.max(0.0)`.
fn headroom_ratio(projected: u64, limit: u64) -> f32 {
    if limit == 0 {
        return f32::NEG_INFINITY;
    }
    1.0_f32 - (projected as f32 / limit as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_model(rpm: Option<u32>, tpm: Option<u32>) -> Model {
        Model {
            id: Uuid::new_v4(),
            brand_id: Uuid::new_v4(),
            slug: "test".to_string(),
            display_name: "test".to_string(),
            max_context_tokens: 128_000,
            max_output_tokens: None,
            supports_function_calling: true,
            supports_json_mode: true,
            price_input_per_1m: None,
            price_output_per_1m: None,
            tpm_limit: tpm,
            rpm_limit: rpm,
            rpd_limit: None,
            tpd_limit: None,
            tpm_limit_month: None,
            rps_limit: None,
            quality_score: None,
            avg_latency_ms: None,
            is_enabled: true,
            notes: None,
            category: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn no_limits_full_headroom() {
        let tracker = UsageTracker::new();
        let model = make_model(None, None);
        assert_eq!(tracker.headroom(model.id, 1_000, &model), 1.0);
    }

    #[test]
    fn rpm_headroom_decreases_with_inflight() {
        let tracker = UsageTracker::new();
        let model = make_model(Some(10), None);
        for _ in 0..5 {
            tracker.reserve(model.id, 0);
        }
        // 5 in-flight + 1 projected = 6/10 → 0.4
        let h = tracker.headroom(model.id, 0, &model);
        assert!((h - 0.4).abs() < 1e-5, "expected ~0.4, got {h}");
    }

    #[test]
    fn headroom_negative_over_limit() {
        let tracker = UsageTracker::new();
        let model = make_model(Some(5), None);
        for _ in 0..5 {
            tracker.reserve(model.id, 0);
        }
        // 5 in-flight + 1 projected = 6/5 → -0.2 (over limit)
        let h = tracker.headroom(model.id, 0, &model);
        assert!(h < 0.0, "expected negative headroom, got {h}");
    }

    #[test]
    fn tpm_headroom_uses_tokens() {
        let tracker = UsageTracker::new();
        let model = make_model(None, Some(10_000));
        tracker.reserve(model.id, 3_000);
        // in_flight_tokens=3000, estimated=3000 → projected=6000/10000 → 0.4
        let h = tracker.headroom(model.id, 3_000, &model);
        assert!((h - 0.4).abs() < 1e-5, "expected ~0.4, got {h}");
    }

    #[test]
    fn release_decrements_inflight() {
        let tracker = UsageTracker::new();
        let model = make_model(None, None);
        tracker.reserve(model.id, 1_000);
        tracker.reserve(model.id, 1_000);
        tracker.release(model.id, 1_000, None);
        let usage = tracker.get_or_default(model.id);
        assert_eq!(usage.in_flight_requests.load(Ordering::Relaxed), 1);
        assert_eq!(usage.in_flight_tokens.load(Ordering::Relaxed), 1_000);
    }

    #[test]
    fn release_saturating_no_panic() {
        let tracker = UsageTracker::new();
        let model = make_model(None, None);
        // Release without prior reserve — must not underflow
        tracker.release(model.id, 500, None);
        let usage = tracker.get_or_default(model.id);
        assert_eq!(usage.in_flight_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn tightest_limit_wins() {
        let tracker = UsageTracker::new();
        let mut model = make_model(Some(100), Some(10_000));
        model.tpd_limit = Some(20_000);
        // 9000 in-flight tokens; requesting 2000 more → TPM projected=11000/10000 → -0.1
        tracker.reserve(model.id, 9_000);
        // RPM: 0+0+1=1/100 → 0.99
        // TPM: 9000+0+2000=11000/10000 → -0.1 (over)
        // TPD: 0+9000+2000=11000/20000 → 0.45
        let h = tracker.headroom(model.id, 2_000, &model);
        assert!(
            h < 0.0,
            "TPM over-capacity should make headroom negative, got {h}"
        );
    }
}

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
    rps: VecDeque<WindowEntry>,
    rpm: VecDeque<WindowEntry>,
    tpm: VecDeque<WindowEntry>,
    rpd: VecDeque<WindowEntry>,
    tpd: VecDeque<WindowEntry>,
    /// Last value of `x-ratelimit-remaining-requests` reported by the provider.
    /// Used as a floor for effective window usage so our estimates can't drift below reality.
    provider_remaining_requests: Option<u32>,
    /// Last value of `x-ratelimit-remaining-tokens` reported by the provider.
    provider_remaining_tokens: Option<u64>,
    /// Last RPM *ceiling* reported by the provider (`x-ratelimit-limit-req-minute`) for THIS key.
    /// Preferred over `model.rpm_limit` in headroom, so two accounts under one brand can be
    /// scored against different real limits without fighting over a single DB value.
    provider_limit_requests: Option<u32>,
    /// Last TPM *ceiling* reported by the provider (`x-ratelimit-limit-tokens-minute`) for THIS key.
    /// Preferred over `model.tpm_limit` in headroom.
    provider_limit_tokens: Option<u32>,
}

impl Default for ModelWindows {
    fn default() -> Self {
        Self {
            rps: VecDeque::new(),
            rpm: VecDeque::new(),
            tpm: VecDeque::new(),
            rpd: VecDeque::new(),
            tpd: VecDeque::new(),
            provider_remaining_requests: None,
            provider_remaining_tokens: None,
            provider_limit_requests: None,
            provider_limit_tokens: None,
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

/// Key into the usage map: `(model_id, brand_key_id)`.
///
/// `brand_key_id` isolates per-account quota. Multi-key brands track each API key
/// independently, so heavy usage (or a 429) on one account never shrinks another
/// account's headroom. Legacy single-key brands pass `None` and keep one bucket per model.
pub type UsageKey = (Uuid, Option<Uuid>);

/// Proactive quota tracker using sliding windows + atomic in-flight counters,
/// keyed per `(model, API key)`.
///
/// Works alongside `RateLimitState` (reactive 429 blocking). This layer prevents
/// over-booking before any 429 fires by tracking estimated usage. When a brand has
/// multiple API keys, each key gets its own independent quota bucket so a fresh
/// account is scored on its own (full) headroom rather than the brand's combined load.
pub struct UsageTracker {
    map: DashMap<UsageKey, Arc<ModelUsage>>,
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
    fn get_or_default(&self, key: UsageKey) -> Arc<ModelUsage> {
        if let Some(entry) = self.map.get(&key) {
            return entry.value().clone();
        }
        self.map
            .entry(key)
            .or_insert_with(|| Arc::new(ModelUsage::default()))
            .value()
            .clone()
    }

    /// Reserve a slot before returning a ModelCandidate. Lock-free (atomic only).
    /// `brand_key_id` selects the per-account bucket (`None` for single-key brands).
    pub fn reserve(&self, model_id: Uuid, brand_key_id: Option<Uuid>, estimated_tokens: u64) {
        let usage = self.get_or_default((model_id, brand_key_id));
        usage.in_flight_requests.fetch_add(1, Ordering::Relaxed);
        usage
            .in_flight_tokens
            .fetch_add(estimated_tokens, Ordering::Relaxed);
    }

    /// Release a reservation and push actual usage into the sliding windows.
    /// Call from every report_* path. `estimated_tokens` must match what was passed to `reserve`,
    /// and `brand_key_id` must match the value echoed by the caller from the `ModelCandidate`.
    pub fn release(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        estimated_tokens: u64,
        actual_tokens: Option<u64>,
    ) {
        let usage = self.get_or_default((model_id, brand_key_id));
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
        w.rps.push_back(WindowEntry { at: now, value: 1 });
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
        brand_key_id: Option<Uuid>,
        remaining_requests: Option<u32>,
        remaining_tokens: Option<u64>,
    ) {
        if remaining_requests.is_none() && remaining_tokens.is_none() {
            return;
        }
        let usage = self.get_or_default((model_id, brand_key_id));
        let mut w = usage.windows.lock().unwrap();
        if let Some(r) = remaining_requests {
            w.provider_remaining_requests = Some(r);
        }
        if let Some(t) = remaining_tokens {
            w.provider_remaining_tokens = Some(t);
        }
    }

    /// Record provider-reported rate-limit *ceilings* (`x-ratelimit-limit-*` headers) for this
    /// `(model, key)` bucket. Preferred over the model's DB `rpm_limit`/`tpm_limit` in `headroom*`,
    /// so each account is scored against the ceiling it actually reports — letting two keys under
    /// one brand carry different limits without clobbering a single shared model-level value.
    pub fn anchor_limits(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        limit_requests: Option<u32>,
        limit_tokens: Option<u32>,
    ) {
        if limit_requests.is_none() && limit_tokens.is_none() {
            return;
        }
        let usage = self.get_or_default((model_id, brand_key_id));
        let mut w = usage.windows.lock().unwrap();
        if let Some(r) = limit_requests {
            w.provider_limit_requests = Some(r);
        }
        if let Some(t) = limit_tokens {
            w.provider_limit_tokens = Some(t);
        }
    }

    /// Earliest time (ms from now) at which any of the supplied `(model, key)` buckets may regain
    /// positive headroom, based on the oldest sliding-window entry leaving the 60-second RPM/TPM window.
    ///
    /// Returns `Some(ms)` if any bucket has window entries that haven't expired yet.
    /// Returns `Some(2_000)` as a conservative hint when buckets are blocked only by in-flight
    /// reservations (no window entries) — those slots free up when current LLM calls complete.
    /// Returns `None` if `keys` is empty.
    pub fn earliest_drain_ms_for(&self, keys: &[UsageKey]) -> Option<u64> {
        if keys.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut min_ms: Option<u64> = None;
        let mut found_inflight_only = false;

        for key in keys {
            let arc = match self.map.get(key) {
                Some(entry) => entry.value().clone(),
                None => {
                    // No tracked usage at all — blocked only by a just-reserved in-flight slot.
                    found_inflight_only = true;
                    continue;
                }
            };

            let w = arc.windows.lock().unwrap();
            let mut model_has_window = false;

            // Check each window with its own expiry duration.
            for (oldest, dur_secs) in [
                (w.rps.front().map(|e| e.at), 1u64),
                (w.rpm.front().map(|e| e.at), 60),
                (w.tpm.front().map(|e| e.at), 60),
            ] {
                if let Some(oldest_at) = oldest {
                    model_has_window = true;
                    let expiry = oldest_at + Duration::from_secs(dur_secs);
                    if let Some(remaining) = expiry.checked_duration_since(now) {
                        // +1 ms so callers that sleep exactly this long clear the boundary.
                        let ms = remaining.as_millis() as u64 + 1;
                        min_ms = Some(min_ms.map_or(ms, |prev| prev.min(ms)));
                    }
                    // else: already expired — this window is actually clear now.
                }
            }

            if !model_has_window {
                // All windows empty; model is blocked by in-flight reservations only.
                found_inflight_only = true;
            }
        }

        if min_ms.is_none() && found_inflight_only {
            // All blocked models have no window history; hint a short wait for in-flight to finish.
            Some(2_000)
        } else {
            min_ms
        }
    }

    /// Headroom considering only fast-recovering windows (RPS 1s, RPM 60s, TPM 60s).
    ///
    /// Returns 1.0 when none of these limits are configured (unconstrained on fast windows).
    /// Negative means over the per-minute/per-second quota but this will recover within 60s.
    pub fn headroom_fast(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        estimated_tokens: u64,
        model: &Model,
    ) -> f32 {
        let usage = self.get_or_default((model_id, brand_key_id));
        let in_flight_req = usage.in_flight_requests.load(Ordering::Relaxed) as u64;
        let in_flight_tok = usage.in_flight_tokens.load(Ordering::Relaxed);

        let mut w = usage.windows.lock().unwrap();
        let now = Instant::now();

        drain_before(&mut w.rps, now, 1);
        drain_before(&mut w.rpm, now, 60);
        drain_before(&mut w.tpm, now, 60);

        let rps_count = window_sum(&w.rps);
        let rpm_count = window_sum(&w.rpm);
        let tpm_sum = window_sum(&w.tpm);

        // Prefer the provider-reported per-key ceiling over the model's DB limit.
        let rpm_limit = w.provider_limit_requests.or(model.rpm_limit);
        let tpm_limit = w.provider_limit_tokens.or(model.tpm_limit);

        let effective_rpm =
            if let (Some(rem), Some(limit)) = (w.provider_remaining_requests, rpm_limit) {
                let provider_used = (limit as u64).saturating_sub(rem as u64);
                rpm_count.max(provider_used)
            } else {
                rpm_count
            };

        let effective_tpm =
            if let (Some(rem), Some(limit)) = (w.provider_remaining_tokens, tpm_limit) {
                let provider_used = (limit as u64).saturating_sub(rem as u64);
                tpm_sum.max(provider_used)
            } else {
                tpm_sum
            };

        let mut min_hr: f32 = 1.0;

        if let Some(limit) = model.rps_limit {
            if limit > 0.0 {
                let projected = (rps_count + in_flight_req + 1) as f32;
                min_hr = min_hr.min(1.0_f32 - projected / limit as f32);
            }
        }
        if let Some(limit) = rpm_limit {
            let projected = effective_rpm + in_flight_req + 1;
            min_hr = min_hr.min(headroom_ratio(projected, limit as u64));
        }
        if let Some(limit) = tpm_limit {
            let projected = effective_tpm + in_flight_tok + estimated_tokens;
            min_hr = min_hr.min(headroom_ratio(projected, limit as u64));
        }

        min_hr
    }

    /// Headroom considering only slow-recovering windows (RPD 24h, TPD 24h, TPM-month).
    ///
    /// Returns 1.0 when none of these limits are configured (unconstrained on long windows).
    /// Negative means the daily/monthly budget is depleted — recovery takes hours or days.
    /// Scoring should weigh this heavily to avoid burning irreplaceable long-horizon credits.
    pub fn headroom_slow(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        estimated_tokens: u64,
        model: &Model,
    ) -> f32 {
        let usage = self.get_or_default((model_id, brand_key_id));
        let in_flight_req = usage.in_flight_requests.load(Ordering::Relaxed) as u64;
        let in_flight_tok = usage.in_flight_tokens.load(Ordering::Relaxed);

        let mut w = usage.windows.lock().unwrap();
        let now = Instant::now();

        drain_before(&mut w.rpd, now, 86_400);
        drain_before(&mut w.tpd, now, 86_400);

        let rpd_count = window_sum(&w.rpd);
        let tpd_sum = window_sum(&w.tpd);

        let mut min_hr: f32 = 1.0;

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

    /// Minimum headroom across ALL applicable limits for a candidate request.
    ///
    /// - Positive: room remaining; 1.0 = fully unconstrained.
    /// - 0.0: exactly last slot (eligible but deprioritised by scoring).
    /// - Negative: over capacity.
    ///
    /// Drains expired window entries as a side-effect (amortised cleanup).
    pub fn headroom(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        estimated_tokens: u64,
        model: &Model,
    ) -> f32 {
        let usage = self.get_or_default((model_id, brand_key_id));
        let in_flight_req = usage.in_flight_requests.load(Ordering::Relaxed) as u64;
        let in_flight_tok = usage.in_flight_tokens.load(Ordering::Relaxed);

        let mut w = usage.windows.lock().unwrap();
        let now = Instant::now();

        drain_before(&mut w.rps, now, 1);
        drain_before(&mut w.rpm, now, 60);
        drain_before(&mut w.tpm, now, 60);
        drain_before(&mut w.rpd, now, 86_400);
        drain_before(&mut w.tpd, now, 86_400);

        let rps_count = window_sum(&w.rps);
        let rpm_count = window_sum(&w.rpm);
        let tpm_sum = window_sum(&w.tpm);
        let rpd_count = window_sum(&w.rpd);
        let tpd_sum = window_sum(&w.tpd);

        // Use provider-reported remaining as a floor: if the provider says fewer requests/tokens
        // remain than our window suggests, trust the provider. In-flight (not yet acknowledged
        // by the provider) is always added on top. The ceiling itself prefers the provider-reported
        // per-key limit over the model's DB limit.
        let rpm_limit = w.provider_limit_requests.or(model.rpm_limit);
        let tpm_limit = w.provider_limit_tokens.or(model.tpm_limit);

        let effective_rpm =
            if let (Some(rem), Some(limit)) = (w.provider_remaining_requests, rpm_limit) {
                let provider_used = (limit as u64).saturating_sub(rem as u64);
                rpm_count.max(provider_used)
            } else {
                rpm_count
            };

        let effective_tpm =
            if let (Some(rem), Some(limit)) = (w.provider_remaining_tokens, tpm_limit) {
                let provider_used = (limit as u64).saturating_sub(rem as u64);
                tpm_sum.max(provider_used)
            } else {
                tpm_sum
            };

        let mut min_hr: f32 = 1.0;

        if let Some(limit) = model.rps_limit {
            if limit > 0.0 {
                let projected = (rps_count + in_flight_req + 1) as f32;
                min_hr = min_hr.min(1.0_f32 - projected / limit as f32);
            }
        }
        if let Some(limit) = rpm_limit {
            let projected = effective_rpm + in_flight_req + 1;
            min_hr = min_hr.min(headroom_ratio(projected, limit as u64));
        }
        if let Some(limit) = tpm_limit {
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
            batch_price_multiplier: None,
        }
    }

    #[test]
    fn no_limits_full_headroom() {
        let tracker = UsageTracker::new();
        let model = make_model(None, None);
        assert_eq!(tracker.headroom(model.id, None, 1_000, &model), 1.0);
    }

    #[test]
    fn rpm_headroom_decreases_with_inflight() {
        let tracker = UsageTracker::new();
        let model = make_model(Some(10), None);
        for _ in 0..5 {
            tracker.reserve(model.id, None, 0);
        }
        // 5 in-flight + 1 projected = 6/10 → 0.4
        let h = tracker.headroom(model.id, None, 0, &model);
        assert!((h - 0.4).abs() < 1e-5, "expected ~0.4, got {h}");
    }

    #[test]
    fn headroom_negative_over_limit() {
        let tracker = UsageTracker::new();
        let model = make_model(Some(5), None);
        for _ in 0..5 {
            tracker.reserve(model.id, None, 0);
        }
        // 5 in-flight + 1 projected = 6/5 → -0.2 (over limit)
        let h = tracker.headroom(model.id, None, 0, &model);
        assert!(h < 0.0, "expected negative headroom, got {h}");
    }

    #[test]
    fn tpm_headroom_uses_tokens() {
        let tracker = UsageTracker::new();
        let model = make_model(None, Some(10_000));
        tracker.reserve(model.id, None, 3_000);
        // in_flight_tokens=3000, estimated=3000 → projected=6000/10000 → 0.4
        let h = tracker.headroom(model.id, None, 3_000, &model);
        assert!((h - 0.4).abs() < 1e-5, "expected ~0.4, got {h}");
    }

    #[test]
    fn release_decrements_inflight() {
        let tracker = UsageTracker::new();
        let model = make_model(None, None);
        tracker.reserve(model.id, None, 1_000);
        tracker.reserve(model.id, None, 1_000);
        tracker.release(model.id, None, 1_000, None);
        let usage = tracker.get_or_default((model.id, None));
        assert_eq!(usage.in_flight_requests.load(Ordering::Relaxed), 1);
        assert_eq!(usage.in_flight_tokens.load(Ordering::Relaxed), 1_000);
    }

    #[test]
    fn release_saturating_no_panic() {
        let tracker = UsageTracker::new();
        let model = make_model(None, None);
        // Release without prior reserve — must not underflow
        tracker.release(model.id, None, 500, None);
        let usage = tracker.get_or_default((model.id, None));
        assert_eq!(usage.in_flight_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn tightest_limit_wins() {
        let tracker = UsageTracker::new();
        let mut model = make_model(Some(100), Some(10_000));
        model.tpd_limit = Some(20_000);
        // 9000 in-flight tokens; requesting 2000 more → TPM projected=11000/10000 → -0.1
        tracker.reserve(model.id, None, 9_000);
        // RPM: 0+0+1=1/100 → 0.99
        // TPM: 9000+0+2000=11000/10000 → -0.1 (over)
        // TPD: 0+9000+2000=11000/20000 → 0.45
        let h = tracker.headroom(model.id, None, 2_000, &model);
        assert!(
            h < 0.0,
            "TPM over-capacity should make headroom negative, got {h}"
        );
    }

    #[test]
    fn keys_have_independent_headroom() {
        // Two API keys (accounts) under one brand share a model_id but must NOT share quota.
        let tracker = UsageTracker::new();
        let model = make_model(Some(10), None);
        let key_a = Some(Uuid::new_v4());
        let key_b = Some(Uuid::new_v4());

        // Saturate key A's RPM bucket: 9 in-flight + 1 projected = 10/10 → 0.0.
        for _ in 0..9 {
            tracker.reserve(model.id, key_a, 0);
        }
        let hr_a = tracker.headroom_fast(model.id, key_a, 0, &model);
        assert!(
            hr_a.abs() < 1e-5,
            "key A should be saturated (~0.0), got {hr_a}"
        );

        // key B is untouched: 0 + 0 + 1 = 1/10 → 0.9.
        let hr_b = tracker.headroom_fast(model.id, key_b, 0, &model);
        assert!(
            (hr_b - 0.9).abs() < 1e-5,
            "key B should be fresh (~0.9), got {hr_b}"
        );

        // The legacy (None) bucket is independent of both keys.
        let hr_legacy = tracker.headroom_fast(model.id, None, 0, &model);
        assert!(
            (hr_legacy - 0.9).abs() < 1e-5,
            "legacy bucket independent (~0.9), got {hr_legacy}"
        );
    }

    #[test]
    fn anchor_remaining_is_per_key() {
        // A provider header floor reported for one key must not bleed into another key's bucket.
        let tracker = UsageTracker::new();
        let model = make_model(Some(100), None);
        let key_a = Some(Uuid::new_v4());
        let key_b = Some(Uuid::new_v4());

        // Provider says key A has only 2 of 100 requests left → heavy used floor.
        tracker.anchor_remaining(model.id, key_a, Some(2), None);

        // key A headroom reflects the floor: effective_used = max(0, 100-2)=98; +1 → 99/100 → 0.01.
        let hr_a = tracker.headroom_fast(model.id, key_a, 0, &model);
        assert!(
            (hr_a - 0.01).abs() < 1e-5,
            "key A should reflect anchor (~0.01), got {hr_a}"
        );

        // key B never got an anchor → full headroom (1/100 → 0.99).
        let hr_b = tracker.headroom_fast(model.id, key_b, 0, &model);
        assert!(
            (hr_b - 0.99).abs() < 1e-5,
            "key B unaffected by key A anchor (~0.99), got {hr_b}"
        );
    }

    #[test]
    fn provider_limit_overrides_db_per_key() {
        // The DB seeds RPM=10, but the provider reports key A's real ceiling is 100.
        let tracker = UsageTracker::new();
        let model = make_model(Some(10), None);
        let key_a = Some(Uuid::new_v4());
        let key_b = Some(Uuid::new_v4());

        tracker.anchor_limits(model.id, key_a, Some(100), None);

        // 50 in-flight on key A: against DB(10) headroom would be deeply negative, but against
        // the provider ceiling(100) it's 1 - 51/100 = 0.49.
        for _ in 0..50 {
            tracker.reserve(model.id, key_a, 0);
        }
        let hr_a = tracker.headroom_fast(model.id, key_a, 0, &model);
        assert!(
            (hr_a - 0.49).abs() < 1e-5,
            "key A should use provider ceiling 100 (~0.49), got {hr_a}"
        );

        // key B has no limit anchor → falls back to the DB limit 10 → 1/10 → 0.9.
        let hr_b = tracker.headroom_fast(model.id, key_b, 0, &model);
        assert!(
            (hr_b - 0.9).abs() < 1e-5,
            "key B falls back to DB limit (~0.9), got {hr_b}"
        );
    }
}

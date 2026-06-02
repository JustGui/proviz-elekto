use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use dashmap::DashMap;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    error::{ProvizError, Result},
    models::{
        Brand, BrandApiKey, Group, GroupMember, Model, ModelCandidate, RateLimitErrorType,
        SelectRequest, SelectionRule,
    },
    rate_state::RateLimitState,
    storage::CatalogStorage,
    usage_tracker::UsageTracker,
};

/// Rolling window (seconds) for per-brand traffic-share tracking.
const TRAFFIC_WINDOW_SECS: u64 = 300;

const CACHE_TTL_SECS: u64 = 300; // 5 minutes

struct CatalogCache {
    models: HashMap<Uuid, Model>,
    brands: HashMap<Uuid, Brand>,
    /// step → sorted rules (priority ASC)
    rules: HashMap<String, Vec<SelectionRule>>,
    groups: HashMap<Uuid, Group>,
    group_slugs: HashMap<String, Uuid>,
    /// group_id → members sorted by (brand.priority, member.priority)
    group_members: HashMap<Uuid, Vec<GroupMember>>,
    /// brand_id → API keys sorted by priority ASC
    brand_keys: HashMap<Uuid, Vec<BrandApiKey>>,
    loaded_at: Instant,
}

pub struct Selector {
    storage: Arc<dyn CatalogStorage>,
    cache: RwLock<Option<CatalogCache>>,
    rate_state: RateLimitState,
    /// Per-key rate limit state for brands with multiple API keys.
    /// Keyed by BrandApiKey.id. A blocked key means that account got a 429;
    /// other keys for the same brand remain available.
    key_rate_state: RateLimitState,
    usage_tracker: UsageTracker,
    /// Per-brand selection timestamps for traffic-share balancing.
    /// Entries older than TRAFFIC_WINDOW_SECS are drained lazily on each selection.
    brand_traffic: DashMap<Uuid, Arc<Mutex<VecDeque<Instant>>>>,
}

impl Selector {
    pub fn new(storage: Arc<dyn CatalogStorage>) -> Self {
        Self {
            storage,
            cache: RwLock::new(None),
            rate_state: RateLimitState::new(),
            key_rate_state: RateLimitState::new(),
            usage_tracker: UsageTracker::new(),
            brand_traffic: DashMap::new(),
        }
    }

    /// Force reload from storage. Called at startup and by POST /catalog/reload.
    pub fn reload(&self) -> Result<(usize, usize)> {
        let brands: HashMap<Uuid, Brand> = self
            .storage
            .load_brands()
            .map_err(ProvizError::Storage)?
            .into_iter()
            .map(|b| (b.id, b))
            .collect();

        let models: HashMap<Uuid, Model> = self
            .storage
            .load_models()
            .map_err(ProvizError::Storage)?
            .into_iter()
            .map(|m| (m.id, m))
            .collect();

        let all_rules = self.load_all_rules()?;

        let mut rules: HashMap<String, Vec<SelectionRule>> = HashMap::new();
        for rule in all_rules {
            rules.entry(rule.step.clone()).or_default().push(rule);
        }
        for list in rules.values_mut() {
            list.sort_by_key(|r| {
                let brand_prio = models
                    .get(&r.model_id)
                    .and_then(|m| brands.get(&m.brand_id))
                    .map(|b| b.priority)
                    .unwrap_or(0);
                (brand_prio, r.priority)
            });
        }

        let all_groups = self.storage.load_groups().map_err(ProvizError::Storage)?;
        let group_slugs: HashMap<String, Uuid> =
            all_groups.iter().map(|g| (g.slug.clone(), g.id)).collect();
        let groups: HashMap<Uuid, Group> = all_groups.into_iter().map(|g| (g.id, g)).collect();

        let all_members = self
            .storage
            .load_all_group_members()
            .map_err(ProvizError::Storage)?;
        let mut group_members: HashMap<Uuid, Vec<GroupMember>> = HashMap::new();
        for member in all_members {
            group_members
                .entry(member.group_id)
                .or_default()
                .push(member);
        }
        for (group_id, members) in &mut group_members {
            members.sort_by_key(|m| {
                let brand_prio = models
                    .get(&m.model_id)
                    .and_then(|model| brands.get(&model.brand_id))
                    .map(|b| b.priority)
                    .unwrap_or(0);
                (brand_prio, m.priority)
            });
            let _ = group_id;
        }

        let all_keys = self
            .storage
            .load_all_brand_api_keys()
            .map_err(ProvizError::Storage)?;
        let mut brand_keys: HashMap<Uuid, Vec<BrandApiKey>> = HashMap::new();
        for key in all_keys {
            brand_keys.entry(key.brand_id).or_default().push(key);
        }
        for keys in brand_keys.values_mut() {
            keys.sort_by_key(|k| k.priority);
        }

        let model_count = models.len();
        let rule_count: usize = rules.values().map(|v| v.len()).sum();

        info!(
            models = model_count,
            rules = rule_count,
            groups = groups.len(),
            "catalog reloaded"
        );

        let mut guard = self.cache.write().unwrap();
        *guard = Some(CatalogCache {
            models,
            brands,
            rules,
            groups,
            group_slugs,
            group_members,
            brand_keys,
            loaded_at: Instant::now(),
        });

        Ok((model_count, rule_count))
    }

    fn load_all_rules(&self) -> Result<Vec<SelectionRule>> {
        self.storage
            .load_selection_rules("*")
            .map_err(ProvizError::Storage)
    }

    fn ensure_cache(&self) -> Result<()> {
        let needs_reload = {
            let guard = self.cache.read().unwrap();
            match guard.as_ref() {
                None => true,
                Some(c) => c.loaded_at.elapsed() > Duration::from_secs(CACHE_TTL_SECS),
            }
        };
        if needs_reload {
            self.reload()?;
        }
        Ok(())
    }

    pub fn select(&self, req: &SelectRequest) -> Result<ModelCandidate> {
        self.ensure_cache()?;

        let guard = self.cache.read().unwrap();
        let cache = guard.as_ref().unwrap();

        let synthetic_rules: Vec<SelectionRule>;
        let use_priority_scoring: bool;
        let rules: &[SelectionRule] = if req.group_id.is_some() || req.group_name.is_some() {
            // Group-based selection: restrict candidates to group members.
            let group_id = if let Some(id) = req.group_id {
                if cache.groups.contains_key(&id) {
                    id
                } else {
                    return Err(ProvizError::GroupNotFound(id.to_string()));
                }
            } else {
                let slug = req.group_name.as_deref().unwrap();
                match cache.group_slugs.get(slug) {
                    Some(&id) => id,
                    None => return Err(ProvizError::GroupNotFound(slug.to_string())),
                }
            };

            let group = cache.groups.get(&group_id).unwrap();
            if !group.is_active {
                return Err(ProvizError::GroupNotFound(group.slug.clone()));
            }

            debug!(group = %group.slug, "group-based selection");
            let members = cache
                .group_members
                .get(&group_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            synthetic_rules = members
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let priority = if req.use_member_priority && m.priority != 0 {
                        m.priority
                    } else {
                        cache
                            .models
                            .get(&m.model_id)
                            .and_then(|model| cache.brands.get(&model.brand_id))
                            .map(|b| b.priority)
                            .unwrap_or(i as i16)
                    };
                    SelectionRule {
                        id: Uuid::nil(),
                        step: req.step.clone(),
                        model_id: m.model_id,
                        priority,
                        max_ctx_tokens: None,
                        requires_fn_call: false,
                        is_enabled: m.is_enabled,
                    }
                })
                .collect();
            use_priority_scoring = req.use_member_priority;
            &synthetic_rules
        } else {
            use_priority_scoring = false;
            match cache.rules.get(&req.step) {
                Some(r) if !r.is_empty() => r.as_slice(),
                _ => {
                    debug!(step = %req.step, "no rules for step, falling back to brand-priority order");
                    let mut entries: Vec<(i16, Uuid)> = cache
                        .models
                        .values()
                        .filter_map(|m| cache.brands.get(&m.brand_id).map(|b| (b.priority, m.id)))
                        .collect();
                    entries.sort_unstable();
                    synthetic_rules = entries
                        .into_iter()
                        .enumerate()
                        .map(|(i, (_, model_id))| SelectionRule {
                            id: Uuid::nil(),
                            step: req.step.clone(),
                            model_id,
                            priority: i as i16,
                            max_ctx_tokens: None,
                            requires_fn_call: false,
                            is_enabled: true,
                        })
                        .collect();
                    &synthetic_rules
                }
            }
        };

        let exclude_set: std::collections::HashSet<&Uuid> = req.exclude_ids.iter().collect();
        let estimated_tokens = req.estimated_tokens as u64;

        // ── Pass 1: hard filters — only truly unavailable models are excluded ─────
        //
        // Headroom is NOT a hard filter: a model that is over its per-minute quota is
        // still eligible (it may succeed, and if it 429s the reactive RateLimitState
        // will block it for the appropriate TTL). This guarantees we never return
        // AllModelsExhausted while any model still has capacity on any window.

        struct Candidate<'c> {
            model: &'c Model,
            brand: &'c Brand,
            rule_priority: i16,
            fast_headroom: f32,
            slow_headroom: f32,
            score: f32,
            member_priority: Option<i16>,
            /// API key chosen for this candidate at filter time (lowest-priority active,
            /// non-rate-limited key for the brand). `None` for single-key/legacy brands.
            /// Headroom and the in-flight reservation are tracked against this specific key.
            brand_key_id: Option<Uuid>,
            api_key_env: Option<String>,
        }

        let mut tried = 0;
        let mut candidates: Vec<Candidate<'_>> = Vec::new();
        let mut rate_limited_ids: Vec<Uuid> = Vec::new();
        let mut blocked_key_ids: Vec<Uuid> = Vec::new();

        for rule in rules {
            if !rule.is_enabled {
                continue;
            }

            let model = match cache.models.get(&rule.model_id) {
                Some(m) => m,
                None => {
                    warn!(model_id = %rule.model_id, "rule references unknown model_id");
                    continue;
                }
            };

            if !model.is_enabled {
                continue;
            }

            let brand = match cache.brands.get(&model.brand_id) {
                Some(b) => b,
                None => {
                    warn!(brand_id = %model.brand_id, "model references unknown brand_id");
                    continue;
                }
            };

            if !brand.is_active {
                continue;
            }

            if model.max_context_tokens < req.estimated_tokens {
                debug!(model = %model.slug, max = model.max_context_tokens,
                    needed = req.estimated_tokens, "skipped: context too large");
                continue;
            }

            if let Some(max_ctx) = rule.max_ctx_tokens {
                if req.estimated_tokens > max_ctx {
                    debug!(model = %model.slug, "skipped: input exceeds rule max_ctx_tokens");
                    continue;
                }
            }

            if !req.categories.is_empty() {
                let matches = model
                    .category
                    .as_deref()
                    .map(|c| req.categories.iter().any(|r| r == c))
                    .unwrap_or(false);
                if !matches {
                    debug!(model = %model.slug, "skipped: category not in request list");
                    continue;
                }
            } else if model
                .category
                .as_deref()
                .map(|c| !matches!(c, "text" | "code" | "vision"))
                .unwrap_or(false)
            {
                debug!(model = %model.slug, category = ?model.category,
                    "skipped: specialized category requires explicit opt-in");
                continue;
            }

            if req.requires_fn_call && !model.supports_function_calling {
                debug!(model = %model.slug, "skipped: function calling required");
                continue;
            }

            if req.requires_json_mode && !model.supports_json_mode {
                debug!(model = %model.slug, "skipped: json mode required");
                continue;
            }

            if req.quality_min > 0.0 {
                match model.quality_score {
                    None => {
                        debug!(model = %model.slug, "skipped: quality unknown, min required");
                        continue;
                    }
                    Some(q) if q < req.quality_min as f64 => {
                        debug!(model = %model.slug, quality = q, min = req.quality_min,
                            "skipped: quality below min");
                        continue;
                    }
                    _ => {}
                }
            }

            if exclude_set.contains(&model.id) {
                tried += 1;
                continue;
            }

            // Rate-limit check: multi-key brands are blocked only when ALL active keys are
            // rate-limited. Single-key (legacy) brands use the model-level rate_state.
            let brand_key_pool = cache.brand_keys.get(&brand.id);
            let is_rate_blocked = match brand_key_pool {
                Some(keys) => {
                    let active: Vec<&BrandApiKey> = keys.iter().filter(|k| k.is_active).collect();
                    if active.is_empty() {
                        // Keys table has rows but none active — fall back to model-level check.
                        self.rate_state.is_limited(&model.id)
                    } else {
                        active.iter().all(|k| self.key_rate_state.is_limited(&k.id))
                    }
                }
                None => self.rate_state.is_limited(&model.id),
            };

            if is_rate_blocked {
                debug!(model = %model.slug, "skipped: rate limited (reactive)");
                tried += 1;
                if let Some(keys) = brand_key_pool {
                    for k in keys.iter().filter(|k| k.is_active) {
                        blocked_key_ids.push(k.id);
                    }
                } else {
                    rate_limited_ids.push(model.id);
                }
                continue;
            }

            // Pick the key that WILL serve this candidate (lowest-priority active,
            // non-rate-limited key — same order as reload()). Done here, before scoring, so
            // headroom and the in-flight reservation are tracked against the specific account,
            // giving each key its own independent quota bucket. `None` for legacy brands (no rows
            // in pz_brand_api_keys) and for the keys-present-but-none-active fallback above.
            let (brand_key_id, api_key_env) = match brand_key_pool {
                Some(keys) => keys
                    .iter()
                    .find(|k| k.is_active && !self.key_rate_state.is_limited(&k.id))
                    .map(|k| (Some(k.id), Some(k.api_key_env.clone())))
                    .unwrap_or((None, None)),
                None => (None, None),
            };

            let fast_headroom =
                self.usage_tracker
                    .headroom_fast(model.id, brand_key_id, estimated_tokens, model);
            let slow_headroom =
                self.usage_tracker
                    .headroom_slow(model.id, brand_key_id, estimated_tokens, model);

            if fast_headroom < 0.0 || slow_headroom < 0.0 {
                debug!(
                    model = %model.slug,
                    fast_headroom,
                    slow_headroom,
                    "over quota (soft — still eligible)"
                );
            }

            candidates.push(Candidate {
                model,
                brand,
                rule_priority: rule.priority,
                fast_headroom,
                slow_headroom,
                score: 0.0,
                member_priority: if use_priority_scoring {
                    Some(rule.priority)
                } else {
                    None
                },
                brand_key_id,
                api_key_env,
            });
        }

        if candidates.is_empty() {
            let model_ms = self
                .rate_state
                .min_remaining_ms_for(&rate_limited_ids)
                .unwrap_or(u64::MAX);
            let key_ms = self
                .key_rate_state
                .min_remaining_ms_for(&blocked_key_ids)
                .unwrap_or(u64::MAX);
            let retry_after_ms = model_ms.min(key_ms);
            let retry_after_ms = if retry_after_ms == u64::MAX {
                0
            } else {
                retry_after_ms
            };
            warn!(
                step = %req.step,
                tried,
                retry_after_ms,
                rate_limited = rate_limited_ids.len() + blocked_key_ids.len(),
                "all models exhausted"
            );
            return Err(ProvizError::AllModelsExhausted {
                step: req.step.clone(),
                tried,
                retry_after_ms,
            });
        }

        // ── Pass 2: score across the pool with min-max normalization ─────────────
        //
        // Headroom is now split into two signals:
        //   fast_headroom (RPS/RPM/TPM) — recovers within 60s, penalise lightly
        //   slow_headroom (RPD/TPD)     — recovers over 24h, penalise heavily
        //
        // Both are mapped [-1,1] → [0,1] so over-quota models remain eligible but
        // rank below models that have capacity.  Traffic balance steers load toward
        // under-served brands according to their traffic_weight.

        let prices: Vec<f64> = candidates
            .iter()
            .filter_map(|c| c.model.price_input_per_1m)
            .collect();
        let min_price = prices.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_price = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

        let latencies: Vec<u32> = candidates
            .iter()
            .filter_map(|c| c.model.avg_latency_ms)
            .collect();
        let min_latency = latencies.iter().cloned().min().unwrap_or(0);
        let max_latency = latencies.iter().cloned().max().unwrap_or(0);

        let (min_prio, max_prio) = if use_priority_scoring {
            let priorities: Vec<i16> = candidates
                .iter()
                .filter_map(|c| c.member_priority)
                .collect();
            let mn = priorities.iter().cloned().min().unwrap_or(0);
            let mx = priorities.iter().cloned().max().unwrap_or(0);
            (mn, mx)
        } else {
            (0, 0)
        };

        // ── Traffic balance ───────────────────────────────────────────────────────
        // For each candidate brand, compute how much of the recent selection window
        // it consumed vs how much its traffic_weight entitles it to.
        // balance_ratio = target_share / (actual_share + ε) — high when under-served.
        let now = Instant::now();
        let cutoff = now - Duration::from_secs(TRAFFIC_WINDOW_SECS);

        // Collect recent selection counts per brand across the candidate pool only.
        let pool_brand_ids: Vec<Uuid> = {
            let mut ids: Vec<Uuid> = candidates.iter().map(|c| c.brand.id).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };

        let mut brand_recent: HashMap<Uuid, u64> = HashMap::new();
        let mut total_recent: u64 = 0;
        for &bid in &pool_brand_ids {
            let count = self
                .brand_traffic
                .get(&bid)
                .map(|arc| {
                    let w = arc.lock().unwrap();
                    w.iter().filter(|&&t| t > cutoff).count() as u64
                })
                .unwrap_or(0);
            brand_recent.insert(bid, count);
            total_recent += count;
        }

        let total_weight: f64 = pool_brand_ids
            .iter()
            .map(|id| {
                cache
                    .brands
                    .get(id)
                    .map(|b| b.traffic_weight.max(0.0))
                    .unwrap_or(1.0)
            })
            .sum::<f64>()
            .max(f64::EPSILON);

        let balance_ratios: Vec<f32> = pool_brand_ids
            .iter()
            .map(|bid| {
                let target_share = cache
                    .brands
                    .get(bid)
                    .map(|b| b.traffic_weight.max(0.0))
                    .unwrap_or(1.0)
                    / total_weight;
                let actual_share = brand_recent[bid] as f64 / (total_recent as f64 + f64::EPSILON);
                (target_share / (actual_share + f64::EPSILON)) as f32
            })
            .collect();

        let min_ratio = balance_ratios.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_ratio = balance_ratios
            .iter()
            .cloned()
            .fold(f32::NEG_INFINITY, f32::max);
        let brand_balance_score: HashMap<Uuid, f32> = pool_brand_ids
            .iter()
            .zip(balance_ratios.iter())
            .map(|(&bid, &r)| {
                let norm = if (max_ratio - min_ratio).abs() < f32::EPSILON {
                    0.5_f32
                } else {
                    (r - min_ratio) / (max_ratio - min_ratio)
                };
                (bid, norm)
            })
            .collect();

        // ── Per-candidate scoring ────────────────────────────────────────────────
        for c in &mut candidates {
            let quality = c.model.quality_score.unwrap_or(0.5) as f32;

            let cost_score = match c.model.price_input_per_1m {
                None => 0.5_f32,
                Some(_) if (max_price - min_price).abs() < f64::EPSILON => 0.5_f32,
                Some(p) => 1.0_f32 - ((p - min_price) / (max_price - min_price)) as f32,
            };

            let latency_score = match c.model.avg_latency_ms {
                None => 0.5_f32,
                Some(_) if max_latency == min_latency => 0.5_f32,
                Some(ms) => {
                    1.0_f32 - ((ms - min_latency) as f32 / (max_latency - min_latency) as f32)
                }
            };

            // Map headroom [-1, 1] → [0, 1]. Clamped so very negative values
            // don't dominate — they just score 0 on this component.
            let fast_hr_norm = (c.fast_headroom.clamp(-1.0, 1.0) + 1.0) / 2.0;
            let slow_hr_norm = (c.slow_headroom.clamp(-1.0, 1.0) + 1.0) / 2.0;

            let traffic_score = *brand_balance_score.get(&c.brand.id).unwrap_or(&0.5);

            c.score = if use_priority_scoring {
                let priority_score = match c.member_priority {
                    None => 0.5_f32,
                    Some(_) if max_prio == min_prio => 1.0_f32,
                    Some(p) => 1.0_f32 - ((p - min_prio) as f32 / (max_prio - min_prio) as f32),
                };
                // With group priority (sum = 1.0)
                0.20 * fast_hr_norm
                    + 0.15 * slow_hr_norm
                    + 0.20 * quality
                    + 0.15 * cost_score
                    + 0.10 * latency_score
                    + 0.10 * priority_score
                    + 0.10 * traffic_score
            } else {
                // Without group (sum = 1.0)
                0.25 * fast_hr_norm
                    + 0.20 * slow_hr_norm
                    + 0.20 * quality
                    + 0.15 * cost_score
                    + 0.10 * latency_score
                    + 0.10 * traffic_score
            };
        }

        // Sort by (score DESC, rule_priority ASC) so priority is the tiebreaker.
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.rule_priority.cmp(&b.rule_priority))
        });

        let winner = &candidates[0];

        // Atomic reservation — on the winner's specific (model, key) bucket.
        self.usage_tracker
            .reserve(winner.model.id, winner.brand_key_id, estimated_tokens);

        // Record this selection in the brand traffic window for future balance scoring.
        {
            let arc = self
                .brand_traffic
                .entry(winner.brand.id)
                .or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())))
                .value()
                .clone();
            let mut w = arc.lock().unwrap();
            // Drain expired entries lazily.
            while w.front().map(|&t| t <= cutoff).unwrap_or(false) {
                w.pop_front();
            }
            w.push_back(now);
        }

        let estimated_input_cost_usd = winner
            .model
            .price_input_per_1m
            .map(|p| p * estimated_tokens as f64 / 1_000_000.0);

        // The serving key was chosen at filter time (Pass 1) so headroom and the in-flight
        // reservation track the specific account; reuse it here rather than re-picking.
        let api_key_env = winner.api_key_env.clone();
        let brand_key_id = winner.brand_key_id;

        debug!(
            model = %winner.model.slug,
            brand = %winner.brand.slug,
            score = winner.score,
            fast_headroom = winner.fast_headroom,
            slow_headroom = winner.slow_headroom,
            brand_key_id = ?brand_key_id,
            "selected"
        );

        Ok(ModelCandidate {
            model_id: winner.model.id,
            brand_slug: winner.brand.slug.clone(),
            model_slug: winner.model.slug.clone(),
            api_key_env,
            brand_key_id,
            max_context_tokens: winner.model.max_context_tokens,
            supports_function_calling: winner.model.supports_function_calling,
            supports_json_mode: winner.model.supports_json_mode,
            estimated_input_cost_usd,
            estimated_tokens,
            price_input_per_1m: winner.model.price_input_per_1m,
            price_output_per_1m: winner.model.price_output_per_1m,
            batch_price_multiplier: winner.model.batch_price_multiplier,
        })
    }

    pub fn report_rate_limit(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        error_type: RateLimitErrorType,
        estimated_tokens: u64,
        actual_tokens: Option<u64>,
        remaining_requests: Option<u32>,
        remaining_tokens: Option<u64>,
    ) {
        match brand_key_id {
            Some(key_id) => {
                // Multi-key brand: block the specific key, not the model.
                // Other keys for the same brand remain available.
                self.key_rate_state.mark(key_id, &error_type);
            }
            None => {
                self.rate_state.mark(model_id, &error_type);
            }
        }
        self.usage_tracker
            .release(model_id, brand_key_id, estimated_tokens, actual_tokens);
        self.usage_tracker.anchor_remaining(
            model_id,
            brand_key_id,
            remaining_requests,
            remaining_tokens,
        );
        if let Err(e) = self.storage.log_rate_event(model_id, &error_type) {
            warn!(error = %e, "failed to persist rate limit event");
        }
    }

    pub fn report_success(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        estimated_tokens: u64,
        actual_tokens: Option<u64>,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        remaining_requests: Option<u32>,
        remaining_tokens: Option<u64>,
    ) -> Option<f64> {
        self.rate_state.clear(&model_id);
        if let Some(key_id) = brand_key_id {
            self.key_rate_state.clear(&key_id);
        }
        // Prefer explicit split over legacy total when both are provided.
        let effective_actual = match (prompt_tokens, completion_tokens) {
            (Some(p), Some(c)) => Some(p + c),
            _ => actual_tokens,
        };
        self.usage_tracker
            .release(model_id, brand_key_id, estimated_tokens, effective_actual);
        self.usage_tracker.anchor_remaining(
            model_id,
            brand_key_id,
            remaining_requests,
            remaining_tokens,
        );

        // Compute actual cost from model prices + token breakdown when available.
        let guard = self.cache.read().unwrap();
        guard
            .as_ref()
            .and_then(|c| c.models.get(&model_id))
            .and_then(|m| {
                let in_cost = m
                    .price_input_per_1m
                    .zip(prompt_tokens)
                    .map(|(p, t)| p * t as f64 / 1_000_000.0)
                    .unwrap_or(0.0);
                let out_cost = m
                    .price_output_per_1m
                    .zip(completion_tokens)
                    .map(|(p, t)| p * t as f64 / 1_000_000.0)
                    .unwrap_or(0.0);
                if m.price_input_per_1m.is_some() || m.price_output_per_1m.is_some() {
                    Some(in_cost + out_cost)
                } else {
                    None
                }
            })
    }

    pub fn report_error(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        error_type: RateLimitErrorType,
        estimated_tokens: u64,
        actual_tokens: Option<u64>,
        remaining_requests: Option<u32>,
        remaining_tokens: Option<u64>,
    ) {
        match brand_key_id {
            Some(key_id) => self.key_rate_state.mark(key_id, &error_type),
            None => self.rate_state.mark(model_id, &error_type),
        }
        self.usage_tracker
            .release(model_id, brand_key_id, estimated_tokens, actual_tokens);
        self.usage_tracker.anchor_remaining(
            model_id,
            brand_key_id,
            remaining_requests,
            remaining_tokens,
        );
        if let Err(e) = self.storage.log_rate_event(model_id, &error_type) {
            warn!(error = %e, "failed to persist error event");
        }
    }

    /// Apply provider-reported `rpm_limit` / `tpm_limit` (from response headers) for the key that
    /// served the request.
    ///
    /// Always anchors the ceiling **per key** in the `UsageTracker` bucket, so headroom scores
    /// against the limit each account actually reports. For single-key brands (`brand_key_id =
    /// None`) the value is *also* persisted to the model-level DB row (and in-memory cache) when it
    /// changed. For multi-key brands the model-level write is skipped — the limit is per-account
    /// and a shared DB write would clobber across keys; the per-key bucket anchor is authoritative.
    pub fn sync_provider_limits(
        &self,
        model_id: Uuid,
        brand_key_id: Option<Uuid>,
        rpm: Option<u32>,
        tpm: Option<u32>,
    ) {
        if rpm.is_none() && tpm.is_none() {
            return;
        }
        // Per-key ceiling for live scoring — applies to both single- and multi-key brands.
        self.usage_tracker
            .anchor_limits(model_id, brand_key_id, rpm, tpm);

        // Multi-key brands stop here: the limit is per-account, not per-model.
        if brand_key_id.is_some() {
            return;
        }
        let needs_update = {
            let guard = self.cache.read().unwrap();
            guard
                .as_ref()
                .and_then(|c| c.models.get(&model_id))
                .map(|m| {
                    (rpm.is_some() && rpm != m.rpm_limit) || (tpm.is_some() && tpm != m.tpm_limit)
                })
                .unwrap_or(false)
        };
        if !needs_update {
            return;
        }
        if let Err(e) = self.storage.sync_model_limits(model_id, rpm, tpm) {
            warn!(model_id = %model_id, error = %e, "failed to persist synced provider limits");
            return;
        }
        // Patch the in-memory cache without a full reload.
        let mut guard = self.cache.write().unwrap();
        if let Some(cache) = guard.as_mut() {
            if let Some(model) = cache.models.get_mut(&model_id) {
                if let Some(r) = rpm {
                    model.rpm_limit = Some(r);
                }
                if let Some(t) = tpm {
                    model.tpm_limit = Some(t);
                }
                info!(
                    model = %model.slug,
                    rpm = ?rpm,
                    tpm = ?tpm,
                    "synced provider limits from response headers"
                );
            }
        }
    }

    pub fn storage(&self) -> &Arc<dyn CatalogStorage> {
        &self.storage
    }
}

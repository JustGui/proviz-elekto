use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    error::{ProvizError, Result},
    models::{
        Brand, Group, GroupMember, Model, ModelCandidate, RateLimitErrorType, SelectRequest,
        SelectionRule,
    },
    rate_state::RateLimitState,
    storage::CatalogStorage,
    usage_tracker::UsageTracker,
};

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
    loaded_at: Instant,
}

pub struct Selector {
    storage: Arc<dyn CatalogStorage>,
    cache: RwLock<Option<CatalogCache>>,
    rate_state: RateLimitState,
    usage_tracker: UsageTracker,
}

impl Selector {
    pub fn new(storage: Arc<dyn CatalogStorage>) -> Self {
        Self {
            storage,
            cache: RwLock::new(None),
            rate_state: RateLimitState::new(),
            usage_tracker: UsageTracker::new(),
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
            &synthetic_rules
        } else {
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

        // ── Pass 1: collect all hard-filter-eligible candidates ──────────────────

        struct Candidate<'c> {
            model: &'c Model,
            brand: &'c Brand,
            rule_priority: i16,
            headroom: f32,
            score: f32,
        }

        let mut tried = 0;
        let mut candidates: Vec<Candidate<'_>> = Vec::new();

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

            if self.rate_state.is_limited(&model.id) {
                debug!(model = %model.slug, "skipped: rate limited");
                tried += 1;
                continue;
            }

            let headroom = self
                .usage_tracker
                .headroom(model.id, estimated_tokens, model);
            if headroom < 0.0 {
                // Negative = strictly over limit. Zero = last slot, still eligible.
                debug!(model = %model.slug, headroom, "skipped: usage tracker headroom exhausted");
                tried += 1;
                continue;
            }

            candidates.push(Candidate {
                model,
                brand,
                rule_priority: rule.priority,
                headroom,
                score: 0.0,
            });
        }

        if candidates.is_empty() {
            return Err(ProvizError::AllModelsExhausted {
                step: req.step.clone(),
                tried,
            });
        }

        // ── Pass 2: score across the pool with min-max normalization ─────────────

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

        for c in &mut candidates {
            let quality = c.model.quality_score.unwrap_or(0.5);

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

            // Clamp headroom to 0 for scoring (last slot = 0, not negative).
            c.score = 0.50 * c.headroom.max(0.0)
                + 0.25 * quality as f32
                + 0.15 * cost_score
                + 0.10 * latency_score;
        }

        // Sort by (score DESC, rule_priority ASC) so priority is the tiebreaker.
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.rule_priority.cmp(&b.rule_priority))
        });

        let winner = &candidates[0];

        // Atomic reservation — must happen before returning.
        self.usage_tracker
            .reserve(winner.model.id, estimated_tokens);

        let estimated_input_cost_usd = winner
            .model
            .price_input_per_1m
            .map(|p| p * estimated_tokens as f64 / 1_000_000.0);

        debug!(
            model = %winner.model.slug,
            brand = %winner.brand.slug,
            score = winner.score,
            headroom = winner.headroom,
            "selected"
        );

        Ok(ModelCandidate {
            model_id: winner.model.id,
            brand_slug: winner.brand.slug.clone(),
            model_slug: winner.model.slug.clone(),
            api_key_env: winner.brand.api_key_env.clone(),
            max_context_tokens: winner.model.max_context_tokens,
            supports_function_calling: winner.model.supports_function_calling,
            supports_json_mode: winner.model.supports_json_mode,
            estimated_input_cost_usd,
            estimated_tokens,
        })
    }

    pub fn report_rate_limit(
        &self,
        model_id: Uuid,
        error_type: RateLimitErrorType,
        estimated_tokens: u64,
        actual_tokens: Option<u64>,
    ) {
        self.rate_state.mark(model_id, &error_type);
        self.usage_tracker
            .release(model_id, estimated_tokens, actual_tokens);
        if let Err(e) = self.storage.log_rate_event(model_id, &error_type) {
            warn!(error = %e, "failed to persist rate limit event");
        }
    }

    pub fn report_success(
        &self,
        model_id: Uuid,
        estimated_tokens: u64,
        actual_tokens: Option<u64>,
    ) {
        self.rate_state.clear(&model_id);
        self.usage_tracker
            .release(model_id, estimated_tokens, actual_tokens);
    }

    pub fn report_error(
        &self,
        model_id: Uuid,
        error_type: RateLimitErrorType,
        estimated_tokens: u64,
        actual_tokens: Option<u64>,
    ) {
        self.rate_state.mark(model_id, &error_type);
        self.usage_tracker
            .release(model_id, estimated_tokens, actual_tokens);
        if let Err(e) = self.storage.log_rate_event(model_id, &error_type) {
            warn!(error = %e, "failed to persist error event");
        }
    }

    pub fn storage(&self) -> &Arc<dyn CatalogStorage> {
        &self.storage
    }
}

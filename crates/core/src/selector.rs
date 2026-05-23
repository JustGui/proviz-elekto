use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    error::{ProvizError, Result},
    models::{Brand, Model, ModelCandidate, RateLimitErrorType, SelectRequest, SelectionRule},
    rate_state::RateLimitState,
    storage::CatalogStorage,
};

const CACHE_TTL_SECS: u64 = 300; // 5 minutes

struct CatalogCache {
    models: HashMap<Uuid, Model>,
    brands: HashMap<Uuid, Brand>,
    /// step → sorted rules (priority ASC)
    rules: HashMap<String, Vec<SelectionRule>>,
    loaded_at: Instant,
}

pub struct Selector {
    storage: Arc<dyn CatalogStorage>,
    cache: RwLock<Option<CatalogCache>>,
    rate_state: RateLimitState,
}

impl Selector {
    pub fn new(storage: Arc<dyn CatalogStorage>) -> Self {
        Self {
            storage,
            cache: RwLock::new(None),
            rate_state: RateLimitState::new(),
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
            .filter(|m| {
                // Keep model only if its plan matches the brand's configured plan.
                // If either side has no plan set, always include the row.
                let brand_plan = brands.get(&m.brand_id).and_then(|b| b.plan.as_deref());
                match (brand_plan, m.plan.as_deref()) {
                    (Some(bp), Some(mp)) => bp == mp,
                    _ => true,
                }
            })
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

        let model_count = models.len();
        let rule_count: usize = rules.values().map(|v| v.len()).sum();

        info!(models = model_count, rules = rule_count, "catalog reloaded");

        let mut guard = self.cache.write().unwrap();
        *guard = Some(CatalogCache {
            models,
            brands,
            rules,
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

        let rules = match cache.rules.get(&req.step) {
            None => {
                return Err(ProvizError::AllModelsExhausted {
                    step: req.step.clone(),
                    tried: 0,
                })
            }
            Some(r) if r.is_empty() => {
                return Err(ProvizError::AllModelsExhausted {
                    step: req.step.clone(),
                    tried: 0,
                })
            }
            Some(r) => r,
        };

        let exclude_set: std::collections::HashSet<&Uuid> = req.exclude_ids.iter().collect();
        let mut tried = 0;

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

            // Context fit
            if model.max_context_tokens < req.estimated_tokens {
                debug!(
                    model = %model.slug,
                    max = model.max_context_tokens,
                    needed = req.estimated_tokens,
                    "skipped: context too large"
                );
                continue;
            }

            // Upper bound: don't waste a large-context model on tiny input
            if let Some(max_ctx) = rule.max_ctx_tokens {
                if req.estimated_tokens > max_ctx {
                    debug!(model = %model.slug, "skipped: input exceeds rule max_ctx_tokens");
                    continue;
                }
            }

            // Category filter: if caller specified categories, model must match one.
            // If caller specified no categories, skip models that have a specialized category.
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
                debug!(model = %model.slug, category = ?model.category, "skipped: specialized category requires explicit opt-in");
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
                    Some(q) if q < req.quality_min => {
                        debug!(model = %model.slug, quality = q, min = req.quality_min, "skipped: quality below min");
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

            let estimated_input_cost_usd = model
                .price_input_per_1m
                .map(|p| p * (req.estimated_tokens as f64) / 1_000_000.0);

            debug!(
                model = %model.slug,
                brand = %brand.slug,
                priority = rule.priority,
                "selected"
            );

            return Ok(ModelCandidate {
                model_id: model.id,
                brand_slug: brand.slug.clone(),
                model_slug: model.slug.clone(),
                api_key_env: brand.api_key_env.clone(),
                max_context_tokens: model.max_context_tokens,
                supports_function_calling: model.supports_function_calling,
                supports_json_mode: model.supports_json_mode,
                estimated_input_cost_usd,
            });
        }

        Err(ProvizError::AllModelsExhausted {
            step: req.step.clone(),
            tried,
        })
    }

    pub fn report_rate_limit(&self, model_id: Uuid, error_type: RateLimitErrorType) {
        self.rate_state.mark(model_id, &error_type);
        if let Err(e) = self.storage.log_rate_event(model_id, &error_type) {
            warn!(error = %e, "failed to persist rate limit event");
        }
    }

    pub fn report_success(&self, model_id: Uuid) {
        self.rate_state.clear(&model_id);
    }

    pub fn report_error(&self, model_id: Uuid, error_type: RateLimitErrorType) {
        self.rate_state.mark(model_id, &error_type);
        if let Err(e) = self.storage.log_rate_event(model_id, &error_type) {
            warn!(error = %e, "failed to persist error event");
        }
    }

    pub fn storage(&self) -> &Arc<dyn CatalogStorage> {
        &self.storage
    }
}

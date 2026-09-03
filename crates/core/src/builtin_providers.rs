use chrono::Utc;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use uuid::Uuid;

use crate::{
    models::{Brand, BrandApiKey, Model, ModelCatalogEntry},
    storage::{CatalogStorage, StorageResult},
};

#[derive(Deserialize)]
struct BrandDef {
    slug: String,
    name: String,
    api_key_env: Option<String>,
    base_url: Option<String>,
    endpoints: Option<JsonValue>,
    /// ISO currency the model prices in this provider's `models.json` are in. Defaults to
    /// `"USD"`. See `crate::fx`.
    #[serde(default)]
    price_currency: Option<String>,
}

#[derive(Deserialize)]
struct ModelDef {
    slug: String,
    display_name: Option<String>,
    max_context_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
    supports_function_calling: Option<bool>,
    supports_json_mode: Option<bool>,
    #[serde(default)]
    reasoning_effort_value: Option<String>,
    price_input_per_1m: Option<f64>,
    price_output_per_1m: Option<f64>,
    /// Per-million price for prompt-cache-hit input tokens. Auto-filled by the Nous Portal sync
    /// from `pricing.input_cache_read`, or set by hand for a curated provider (DeepSeek). `None`
    /// = no distinct cached rate; cost uses `price_input_per_1m` for all prompt tokens.
    #[serde(default)]
    price_cached_input_per_1m: Option<f64>,
    tpm_limit: Option<u32>,
    rpm_limit: Option<u32>,
    rpd_limit: Option<u32>,
    tpd_limit: Option<u64>,
    tpm_limit_month: Option<u64>,
    rps_limit: Option<f64>,
    quality_score: Option<f64>,
    avg_latency_ms: Option<u32>,
    notes: Option<String>,
    category: Option<String>,
    #[serde(default)]
    batch_price_multiplier: Option<f64>,
    diarization: Option<bool>,
    streaming: Option<bool>,
    http_batch: Option<bool>,
    word_timestamps: Option<bool>,
    base_url: Option<String>,
    supported_languages: Option<Vec<String>>,
    /// Key into the shared model catalog (`providers/model_catalog.json`) identifying this
    /// model's underlying family across brands — typically a HuggingFace `org/model` id. When
    /// set, `quality_score`/`category`/`max_context_tokens`/capability flags omitted here are
    /// filled in from the matching catalog entry instead of the hardcoded default.
    #[serde(default)]
    canonical_model: Option<String>,
    /// RFC3339 timestamp of when this entry's pricing was last fetched from a live provider
    /// catalog (e.g. OpenRouter). Only ever set by an automated sync — omitted by hand-curated
    /// providers.
    #[serde(default)]
    price_synced_at: Option<chrono::DateTime<Utc>>,
    /// Whether this model/provider combination may train on submitted data (e.g. Requesty's
    /// `data_used_for_training`). A fact about the provider, not the model family — always taken
    /// directly from this entry, never inherited from the shared catalog.
    #[serde(default)]
    trains_on_data: Option<bool>,
    /// Whether this model/provider combination retains submitted data (e.g. Requesty's
    /// `data_retention`).
    #[serde(default)]
    retains_data: Option<bool>,
}

pub struct LoadSummary {
    pub brands_added: usize,
    pub models_added: usize,
    pub models_updated: usize,
    pub models_skipped: usize,
    pub models_disabled: usize,
}

/// Seeds built-in provider catalog if the DB is empty. Idempotent: no-op if any brand exists.
/// Scans `providers_dir` for subdirectories each containing `brand.json` and `models.json`.
pub fn seed_if_empty(storage: &dyn CatalogStorage, providers_dir: &str) -> StorageResult<()> {
    if !storage.load_brands()?.is_empty() {
        return Ok(());
    }
    load_from_dir(storage, providers_dir, false, false)?;
    Ok(())
}

/// Reads `providers/model_catalog.json` (a flat array, sibling to the per-provider directories —
/// not inside any one provider's dir) and upserts it into `pz_model_catalog`. Returns a map keyed
/// by `canonical_key` for `load_from_dir` to resolve per-brand rows against. Missing file is not
/// an error — the shared catalog is optional.
fn load_model_catalog(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
) -> StorageResult<HashMap<String, ModelCatalogEntry>> {
    let path = std::path::Path::new(providers_dir).join("model_catalog.json");
    if !path.exists() {
        return Ok(HashMap::new());
    }

    #[derive(Deserialize)]
    struct CatalogDef {
        canonical_key: String,
        display_name: Option<String>,
        category: Option<String>,
        max_context_tokens: Option<u32>,
        supports_function_calling: Option<bool>,
        supports_json_mode: Option<bool>,
        quality_score: Option<f64>,
        knowledge_cutoff: Option<String>,
    }

    let defs: Vec<CatalogDef> = match std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("invalid model_catalog.json: {e}");
            return Ok(HashMap::new());
        }
    };

    let existing: HashMap<String, ModelCatalogEntry> = storage
        .load_model_catalog()?
        .into_iter()
        .map(|e| (e.canonical_key.clone(), e))
        .collect();

    let mut result = HashMap::new();
    for def in defs {
        let id = existing
            .get(&def.canonical_key)
            .map(|e| e.id)
            .unwrap_or_else(Uuid::new_v4);
        let entry = ModelCatalogEntry {
            id,
            canonical_key: def.canonical_key.clone(),
            display_name: def.display_name,
            category: def.category,
            max_context_tokens: def.max_context_tokens,
            supports_function_calling: def.supports_function_calling,
            supports_json_mode: def.supports_json_mode,
            quality_score: def.quality_score,
            knowledge_cutoff: def.knowledge_cutoff,
            created_at: existing
                .get(&def.canonical_key)
                .map(|e| e.created_at)
                .unwrap_or_else(Utc::now),
        };
        storage.insert_model_catalog_entry(&entry)?;
        result.insert(entry.canonical_key.clone(), entry);
    }
    Ok(result)
}

/// Upserts all providers found in `providers_dir` regardless of whether the DB is empty.
/// New brands/models are inserted; existing models always pick up pricing/context/capability
/// changes from the source JSON (that's the whole point of re-running this for a provider whose
/// catalog drifts, e.g. OpenRouter) — `update_limits` only additionally gates *rate limits*
/// (tpm/rpm/rpd/tpd), which are sometimes hand-tuned separately via `sync_provider_limits` and
/// shouldn't be silently clobbered by a stale JSON default unless requested.
/// When `disable_missing=true`, any model that exists in the DB for a brand but is no longer
/// present in that brand's `models.json` is disabled (not deleted) — opt-in, since for hand-curated
/// providers a model missing from the file is usually an accident, not an intentional removal.
/// Returns a summary of what changed.
pub fn load_from_dir(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
    update_limits: bool,
    disable_missing: bool,
) -> StorageResult<LoadSummary> {
    let mut summary = LoadSummary {
        brands_added: 0,
        models_added: 0,
        models_updated: 0,
        models_skipped: 0,
        models_disabled: 0,
    };

    let entries = match std::fs::read_dir(providers_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read providers dir '{providers_dir}': {e} — skipping");
            return Ok(summary);
        }
    };

    let catalog = load_model_catalog(storage, providers_dir)?;

    let existing_brands: std::collections::HashMap<String, Brand> = storage
        .load_brands()?
        .into_iter()
        .map(|b| (b.slug.clone(), b))
        .collect();

    let existing_models: std::collections::HashMap<(Uuid, String, bool, bool), Model> = storage
        .load_models()?
        .into_iter()
        .map(|m| {
            (
                (
                    m.brand_id,
                    m.slug.clone(),
                    m.streaming.unwrap_or(false),
                    m.http_batch.unwrap_or(false),
                ),
                m,
            )
        })
        .collect();

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let provider_name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();

        let brand_path = path.join("brand.json");
        let models_path = path.join("models.json");

        if !brand_path.exists() || !models_path.exists() {
            continue;
        }

        let brand_def: BrandDef = match std::fs::read_to_string(&brand_path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("[{provider_name}] invalid brand.json: {e}");
                continue;
            }
        };

        let model_defs: Vec<ModelDef> = match std::fs::read_to_string(&models_path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("[{provider_name}] invalid models.json: {e}");
                continue;
            }
        };

        // Reuse existing UUID so FK references on pz_models stay valid.
        let brand_id = if let Some(existing) = existing_brands.get(&brand_def.slug) {
            let updated = Brand {
                endpoints: brand_def.endpoints.clone(),
                base_url: brand_def.base_url.clone(),
                name: brand_def.name.clone(),
                price_currency: brand_def
                    .price_currency
                    .clone()
                    .unwrap_or_else(|| existing.price_currency.clone()),
                ..existing.clone()
            };
            storage.insert_brand(&updated)?;
            existing.id
        } else {
            let brand = Brand {
                id: Uuid::new_v4(),
                slug: brand_def.slug.clone(),
                name: brand_def.name.clone(),
                base_url: brand_def.base_url.clone(),
                is_active: true,
                priority: 0,
                created_at: Utc::now(),
                traffic_weight: 1.0,
                endpoints: brand_def.endpoints.clone(),
                price_currency: brand_def
                    .price_currency
                    .clone()
                    .unwrap_or_else(|| "USD".to_string()),
            };
            let id = brand.id;
            storage.insert_brand(&brand)?;
            if let Some(env) = &brand_def.api_key_env {
                storage.insert_brand_api_key(&BrandApiKey {
                    id: Uuid::new_v4(),
                    brand_id: id,
                    api_key_env: env.clone(),
                    priority: 0,
                    is_active: true,
                    created_at: Utc::now(),
                })?;
            }
            tracing::info!("[{provider_name}] brand created");
            summary.brands_added += 1;
            id
        };

        let mut seen_keys: std::collections::HashSet<(Uuid, String, bool, bool)> =
            std::collections::HashSet::new();

        for def in &model_defs {
            let variant_key = (
                brand_id,
                def.slug.clone(),
                def.streaming.unwrap_or(false),
                def.http_batch.unwrap_or(false),
            );
            seen_keys.insert(variant_key.clone());

            let canonical = def.canonical_model.as_ref().and_then(|k| catalog.get(k));
            let max_context_tokens = def
                .max_context_tokens
                .or_else(|| canonical.and_then(|c| c.max_context_tokens))
                .unwrap_or(0);
            let supports_function_calling = def
                .supports_function_calling
                .or_else(|| canonical.and_then(|c| c.supports_function_calling))
                .unwrap_or(false);
            let supports_json_mode = def
                .supports_json_mode
                .or_else(|| canonical.and_then(|c| c.supports_json_mode))
                .unwrap_or(false);
            let quality_score = def
                .quality_score
                .or_else(|| canonical.and_then(|c| c.quality_score));
            let category = def
                .category
                .clone()
                .or_else(|| canonical.and_then(|c| c.category.clone()));
            let display_name = def
                .display_name
                .clone()
                .or_else(|| canonical.and_then(|c| c.display_name.clone()))
                .unwrap_or_else(|| def.slug.clone());

            if let Some(existing) = existing_models.get(&variant_key) {
                // Pricing/context/capabilities/descriptive fields always reflect the current
                // source JSON — that's the point of re-running this for a drifting catalog
                // (e.g. OpenRouter). Only *rate limits* are gated by `update_limits`, since
                // those are sometimes hand-tuned live via sync_provider_limits and shouldn't be
                // silently clobbered by a stale JSON default unless requested.
                let model = Model {
                    display_name,
                    max_context_tokens,
                    max_output_tokens: def.max_output_tokens,
                    supports_function_calling,
                    supports_json_mode,
                    price_input_per_1m: def.price_input_per_1m,
                    price_output_per_1m: def.price_output_per_1m,
                    quality_score,
                    avg_latency_ms: def.avg_latency_ms,
                    notes: def.notes.clone(),
                    category,
                    batch_price_multiplier: def.batch_price_multiplier,
                    canonical_key: def.canonical_model.clone(),
                    price_synced_at: def.price_synced_at,
                    trains_on_data: def.trains_on_data,
                    retains_data: def.retains_data,
                    price_cached_input_per_1m: def.price_cached_input_per_1m,
                    diarization: def.diarization,
                    streaming: def.streaming,
                    http_batch: def.http_batch,
                    word_timestamps: def.word_timestamps,
                    base_url: def.base_url.clone(),
                    supported_languages: def.supported_languages.clone(),
                    reasoning_effort_value: def.reasoning_effort_value.clone(),
                    tpm_limit: if update_limits {
                        def.tpm_limit.or(existing.tpm_limit)
                    } else {
                        existing.tpm_limit
                    },
                    rpm_limit: if update_limits {
                        def.rpm_limit.or(existing.rpm_limit)
                    } else {
                        existing.rpm_limit
                    },
                    rpd_limit: if update_limits {
                        def.rpd_limit.or(existing.rpd_limit)
                    } else {
                        existing.rpd_limit
                    },
                    tpd_limit: if update_limits {
                        def.tpd_limit.or(existing.tpd_limit)
                    } else {
                        existing.tpd_limit
                    },
                    tpm_limit_month: if update_limits {
                        def.tpm_limit_month.or(existing.tpm_limit_month)
                    } else {
                        existing.tpm_limit_month
                    },
                    rps_limit: if update_limits {
                        def.rps_limit.or(existing.rps_limit)
                    } else {
                        existing.rps_limit
                    },
                    ..existing.clone()
                };
                storage.insert_model(&model)?;
                summary.models_updated += 1;
            } else {
                let model = Model {
                    id: Uuid::new_v4(),
                    brand_id,
                    slug: def.slug.clone(),
                    display_name,
                    max_context_tokens,
                    max_output_tokens: def.max_output_tokens,
                    supports_function_calling,
                    supports_json_mode,
                    reasoning_effort_value: def.reasoning_effort_value.clone(),
                    price_input_per_1m: def.price_input_per_1m,
                    price_output_per_1m: def.price_output_per_1m,
                    tpm_limit: def.tpm_limit,
                    rpm_limit: def.rpm_limit,
                    rpd_limit: def.rpd_limit,
                    tpd_limit: def.tpd_limit,
                    tpm_limit_month: def.tpm_limit_month,
                    rps_limit: def.rps_limit,
                    quality_score,
                    avg_latency_ms: def.avg_latency_ms,
                    is_enabled: true,
                    notes: def.notes.clone(),
                    category,
                    created_at: Utc::now(),
                    batch_price_multiplier: def.batch_price_multiplier,
                    canonical_key: def.canonical_model.clone(),
                    price_synced_at: def.price_synced_at,
                    trains_on_data: def.trains_on_data,
                    retains_data: def.retains_data,
                    price_cached_input_per_1m: def.price_cached_input_per_1m,
                    diarization: def.diarization,
                    streaming: def.streaming,
                    http_batch: def.http_batch,
                    word_timestamps: def.word_timestamps,
                    base_url: def.base_url.clone(),
                    supported_languages: def.supported_languages.clone(),
                };
                storage.insert_model(&model)?;
                summary.models_added += 1;
            }
        }

        if disable_missing {
            for (key, existing) in &existing_models {
                if key.0 == brand_id && existing.is_enabled && !seen_keys.contains(key) {
                    storage.set_model_enabled(existing.id, false)?;
                    summary.models_disabled += 1;
                    tracing::info!(
                        "[{provider_name}] disabled (missing from source): {}",
                        existing.slug
                    );
                }
            }
        }

        tracing::info!(
            "[{provider_name}] added={} updated={} skipped={} disabled={}",
            summary.models_added,
            summary.models_updated,
            summary.models_skipped,
            summary.models_disabled,
        );
    }

    Ok(summary)
}

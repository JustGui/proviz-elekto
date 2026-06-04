use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    models::{Brand, BrandApiKey, Model},
    storage::{CatalogStorage, StorageResult},
};

#[derive(Deserialize)]
struct BrandDef {
    slug: String,
    name: String,
    api_key_env: Option<String>,
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct ModelDef {
    slug: String,
    display_name: Option<String>,
    #[serde(default)]
    max_context_tokens: u32,
    max_output_tokens: Option<u32>,
    #[serde(default)]
    supports_function_calling: bool,
    #[serde(default)]
    supports_json_mode: bool,
    price_input_per_1m: Option<f64>,
    price_output_per_1m: Option<f64>,
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
}

pub struct LoadSummary {
    pub brands_added: usize,
    pub models_added: usize,
    pub models_updated: usize,
    pub models_skipped: usize,
}

/// Seeds built-in provider catalog if the DB is empty. Idempotent: no-op if any brand exists.
/// Scans `providers_dir` for subdirectories each containing `brand.json` and `models.json`.
pub fn seed_if_empty(storage: &dyn CatalogStorage, providers_dir: &str) -> StorageResult<()> {
    if !storage.load_brands()?.is_empty() {
        return Ok(());
    }
    load_from_dir(storage, providers_dir, false)?;
    Ok(())
}

/// Upserts all providers found in `providers_dir` regardless of whether the DB is empty.
/// New brands/models are inserted; existing models are updated only when `update_limits=true`.
/// Returns a summary of what changed.
pub fn load_from_dir(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
    update_limits: bool,
) -> StorageResult<LoadSummary> {
    let mut summary = LoadSummary {
        brands_added: 0,
        models_added: 0,
        models_updated: 0,
        models_skipped: 0,
    };

    let entries = match std::fs::read_dir(providers_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read providers dir '{providers_dir}': {e} — skipping");
            return Ok(summary);
        }
    };

    let existing_brands: std::collections::HashMap<String, Brand> = storage
        .load_brands()?
        .into_iter()
        .map(|b| (b.slug.clone(), b))
        .collect();

    let existing_models: std::collections::HashMap<(Uuid, String), Model> = storage
        .load_models()?
        .into_iter()
        .map(|m| ((m.brand_id, m.slug.clone()), m))
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

        for def in &model_defs {
            if let Some(existing) = existing_models.get(&(brand_id, def.slug.clone())) {
                if update_limits {
                    let model = Model {
                        tpm_limit: def.tpm_limit.or(existing.tpm_limit),
                        rpm_limit: def.rpm_limit.or(existing.rpm_limit),
                        rpd_limit: def.rpd_limit.or(existing.rpd_limit),
                        tpd_limit: def.tpd_limit.or(existing.tpd_limit),
                        tpm_limit_month: def.tpm_limit_month.or(existing.tpm_limit_month),
                        rps_limit: def.rps_limit.or(existing.rps_limit),
                        ..existing.clone()
                    };
                    storage.insert_model(&model)?;
                    summary.models_updated += 1;
                } else {
                    summary.models_skipped += 1;
                }
            } else {
                let display = def.display_name.clone().unwrap_or_else(|| def.slug.clone());
                let model = Model {
                    id: Uuid::new_v4(),
                    brand_id,
                    slug: def.slug.clone(),
                    display_name: display,
                    max_context_tokens: def.max_context_tokens,
                    max_output_tokens: def.max_output_tokens,
                    supports_function_calling: def.supports_function_calling,
                    supports_json_mode: def.supports_json_mode,
                    price_input_per_1m: def.price_input_per_1m,
                    price_output_per_1m: def.price_output_per_1m,
                    tpm_limit: def.tpm_limit,
                    rpm_limit: def.rpm_limit,
                    rpd_limit: def.rpd_limit,
                    tpd_limit: def.tpd_limit,
                    tpm_limit_month: def.tpm_limit_month,
                    rps_limit: def.rps_limit,
                    quality_score: def.quality_score,
                    avg_latency_ms: def.avg_latency_ms,
                    is_enabled: true,
                    notes: def.notes.clone(),
                    category: def.category.clone(),
                    created_at: Utc::now(),
                    batch_price_multiplier: def.batch_price_multiplier,
                };
                storage.insert_model(&model)?;
                summary.models_added += 1;
            }
        }

        tracing::info!(
            "[{provider_name}] added={} updated={} skipped={}",
            summary.models_added,
            summary.models_updated,
            summary.models_skipped
        );
    }

    Ok(summary)
}

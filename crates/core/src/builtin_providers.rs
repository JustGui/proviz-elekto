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

/// Seeds built-in provider catalog if the DB is empty. Idempotent: no-op if any brand exists.
/// Scans `providers_dir` for subdirectories each containing `brand.json` and `models.json`.
pub fn seed_if_empty(storage: &dyn CatalogStorage, providers_dir: &str) -> StorageResult<()> {
    if !storage.load_brands()?.is_empty() {
        return Ok(());
    }

    let entries = match std::fs::read_dir(providers_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read providers dir '{providers_dir}': {e} — skipping auto-seed");
            return Ok(());
        }
    };

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
        storage.insert_brand(&brand)?;

        if let Some(env) = &brand_def.api_key_env {
            storage.insert_brand_api_key(&BrandApiKey {
                id: Uuid::new_v4(),
                brand_id: brand.id,
                api_key_env: env.clone(),
                priority: 0,
                is_active: true,
                created_at: Utc::now(),
            })?;
        }

        for def in &model_defs {
            let display = def.display_name.clone().unwrap_or_else(|| def.slug.clone());
            let model = Model {
                id: Uuid::new_v4(),
                brand_id: brand.id,
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
        }

        tracing::info!("[{provider_name}] seeded {} models", model_defs.len());
    }

    Ok(())
}

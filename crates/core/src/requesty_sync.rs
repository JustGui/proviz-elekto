//! Fetches Requesty's public model catalog (`GET {base_url}/models`) and upserts it into the
//! local provider directory + DB via `builtin_providers::load_from_dir` — same shape as
//! `openrouter_sync`, since Requesty is the same kind of aggregator (600+ models, pricing that
//! changes often), just with different field names in its `/models` response.
//!
//! Unlike OpenRouter, Requesty's `/models` response has no HuggingFace-style cross-provider
//! identity field (`model_canonical_name` is Requesty's own short slug, not an HF `org/model`
//! id, so it can't be used to auto-fill `canonical_model` the way OpenRouter's `hugging_face_id`
//! is) — `canonical_model` is simply left unset for Requesty entries. It also has no per-model
//! `quality_score` (same as OpenRouter), but it does report per-model `data_used_for_training`/
//! `data_retention`, which OpenRouter doesn't report at all — see `Model.trains_on_data`/
//! `retains_data`.

use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::{builtin_providers::load_from_dir, storage::CatalogStorage};

pub const DEFAULT_BASE_URL: &str = "https://router.requesty.ai/v1";

/// Below this many models, a fetch is treated as suspect (truncated/empty response) and the
/// whole sync is skipped rather than upserted — protects against `disable_missing` mass-disabling
/// the catalog on a bad response. Requesty has consistently listed 600+ models; 100 is a
/// generous floor.
const MIN_SANE_MODEL_COUNT: usize = 100;

pub struct RequestySyncSummary {
    pub fetched: usize,
    pub brands_added: usize,
    pub models_added: usize,
    pub models_updated: usize,
    pub models_disabled: usize,
    pub skipped_suspicious: bool,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<RequestyModel>,
}

#[derive(Deserialize)]
struct RequestyModel {
    id: String,
    description: Option<String>,
    model_canonical_name: Option<String>,
    input_price: Option<f64>,
    output_price: Option<f64>,
    max_output_tokens: Option<u32>,
    context_window: Option<u32>,
    supports_tool_calling: Option<bool>,
    supports_output_json_object: Option<bool>,
    data_retention: Option<bool>,
    data_used_for_training: Option<bool>,
}

/// Maps one Requesty `/models` entry into the on-disk `models.json` shape consumed by
/// `builtin_providers::ModelDef`. Every entry Requesty lists has `"api": "chat"` (verified: all
/// 668 live entries), so — same as OpenRouter — category is uniformly "text", no filtering
/// needed to include every model.
fn map_entry(m: &RequestyModel, synced_at: chrono::DateTime<Utc>) -> serde_json::Value {
    // Requesty's input_price/output_price are already dollars-per-token (floats, not strings
    // like OpenRouter's) — just scale to per-million.
    let price_per_1m = |p: Option<f64>| p.map(|v| v * 1_000_000.0);
    json!({
        "slug": m.id,
        "display_name": m.model_canonical_name.clone().or_else(|| m.description.clone()),
        "max_context_tokens": m.context_window,
        "max_output_tokens": m.max_output_tokens,
        "supports_function_calling": m.supports_tool_calling,
        "supports_json_mode": m.supports_output_json_object,
        "price_input_per_1m": price_per_1m(m.input_price),
        "price_output_per_1m": price_per_1m(m.output_price),
        "category": "text",
        "trains_on_data": m.data_used_for_training,
        "retains_data": m.data_retention,
        "price_synced_at": synced_at.to_rfc3339(),
    })
}

/// Fetches the live Requesty catalog, writes `{providers_dir}/requesty/models.json`, and upserts
/// it via `load_from_dir` (reusing the same brand/model dedup, UUID-reuse, and
/// price/capability-refresh logic every other provider goes through). `disable_missing` is
/// always passed as `true` for the requesty brand's own entries — staleness is the norm for this
/// provider, unlike hand-curated ones.
pub fn sync(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
    base_url: &str,
) -> Result<RequestySyncSummary, String> {
    let url = format!("{base_url}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("requesty fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("requesty fetch returned error status: {e}"))?;
    let parsed: ModelsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse requesty response: {e}"))?;

    let fetched = parsed.data.len();
    if fetched < MIN_SANE_MODEL_COUNT {
        tracing::warn!(
            fetched,
            min = MIN_SANE_MODEL_COUNT,
            "requesty sync: suspiciously few models returned, skipping upsert"
        );
        return Ok(RequestySyncSummary {
            fetched,
            brands_added: 0,
            models_added: 0,
            models_updated: 0,
            models_disabled: 0,
            skipped_suspicious: true,
        });
    }

    let synced_at = Utc::now();
    let entries: Vec<serde_json::Value> = parsed
        .data
        .iter()
        .map(|m| map_entry(m, synced_at))
        .collect();

    let requesty_dir = std::path::Path::new(providers_dir).join("requesty");
    std::fs::create_dir_all(&requesty_dir)
        .map_err(|e| format!("failed to create {}: {e}", requesty_dir.display()))?;
    let models_path = requesty_dir.join("models.json");
    let body = serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("failed to serialize models.json: {e}"))?;
    std::fs::write(&models_path, body)
        .map_err(|e| format!("failed to write {}: {e}", models_path.display()))?;

    let summary = load_from_dir(storage, providers_dir, true, true).map_err(|e| e.to_string())?;

    Ok(RequestySyncSummary {
        fetched,
        brands_added: summary.brands_added,
        models_added: summary.models_added,
        models_updated: summary.models_updated,
        models_disabled: summary.models_disabled,
        skipped_suspicious: false,
    })
}

/// Fetches and maps the catalog without touching disk or the DB — used by `--dry-run` so the
/// mapping can be reviewed before anything is written.
pub fn fetch_preview(base_url: &str) -> Result<Vec<serde_json::Value>, String> {
    let url = format!("{base_url}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("requesty fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("requesty fetch returned error status: {e}"))?;
    let parsed: ModelsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse requesty response: {e}"))?;
    let synced_at = Utc::now();
    Ok(parsed
        .data
        .iter()
        .map(|m| map_entry(m, synced_at))
        .collect())
}

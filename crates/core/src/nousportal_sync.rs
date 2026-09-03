//! Fetches Nous Portal's public model catalog (`GET {base_url}/models`) and upserts it into the
//! local provider directory + DB via `builtin_providers::load_from_dir` — same shape as
//! `openrouter_sync`, because Nous Portal's inference API is an OpenRouter fork: byte-identical
//! `/models` response schema (`pricing.prompt`/`completion` as dollar-per-token strings,
//! `hugging_face_id`, `context_length`, `supported_parameters`, `top_provider.max_completion_tokens`).
//!
//! Two things Nous exposes that OpenRouter doesn't:
//!   - **`pricing.input_cache_read`** — a per-token price for prompt-cache hits, mapped straight
//!     into `Model.price_cached_input_per_1m` so the selector's cost accounting discounts cached
//!     input automatically (Nous also returns `usage.prompt_tokens_details.cached_tokens` per
//!     response, which `/complete` forwards — see `server/src/complete.rs`).
//!   - nothing per-model about data retention. Nous's privacy posture is **account-wide**
//!     ("Privacy Mode", set in the portal account settings), so this sync stamps every Nous row
//!     with a single `trains_on_data`/`retains_data` value driven by the `NOUS_PORTAL_PRIVACY_MODE`
//!     env var: set (`1`/`true`) → `false`/`false` (detector-eligible); unset → `true`/`true`, so
//!     the selector's `require_no_training` filter keeps Nous out of privacy-sensitive steps until
//!     the operator has actually enabled Privacy Mode on the account.

use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::{builtin_providers::load_from_dir, storage::CatalogStorage};

pub const DEFAULT_BASE_URL: &str = "https://inference-api.nousresearch.com/v1";

/// Env var gating the privacy stamp (see module docs). Must be set to a truthy value AND
/// "Privacy Mode" must be enabled on the Nous Portal account for Nous models to be eligible for
/// steps that pass `require_no_training`.
pub const PRIVACY_MODE_ENV: &str = "NOUS_PORTAL_PRIVACY_MODE";

/// Below this many models, a fetch is treated as suspect (truncated/empty response) and the whole
/// sync is skipped rather than upserted — protects against `disable_missing` mass-disabling the
/// catalog on a bad response. Nous Portal has consistently listed 350+ models; 100 is a generous
/// floor.
const MIN_SANE_MODEL_COUNT: usize = 100;

pub struct NousPortalSyncSummary {
    pub fetched: usize,
    pub brands_added: usize,
    pub models_added: usize,
    pub models_updated: usize,
    pub models_disabled: usize,
    pub skipped_suspicious: bool,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<NousModel>,
}

#[derive(Deserialize)]
struct NousModel {
    id: String,
    name: Option<String>,
    hugging_face_id: Option<String>,
    context_length: Option<u32>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    pricing: Option<NousPricing>,
    top_provider: Option<NousTopProvider>,
}

#[derive(Deserialize)]
struct NousPricing {
    prompt: Option<String>,
    completion: Option<String>,
    /// Per-token price for prompt-cache-hit input tokens. Nous quotes this for every cache-capable
    /// model (e.g. `deepseek/deepseek-v4-flash`); absent/`"0"` for models with no distinct rate.
    input_cache_read: Option<String>,
}

#[derive(Deserialize)]
struct NousTopProvider {
    max_completion_tokens: Option<u32>,
}

/// Dollar-per-token string (e.g. `"0.0000000660"`) → dollar-per-million-tokens. Negative /
/// unparseable / zero-or-below values map to `None` (same guard as `openrouter_sync`; a `0`
/// cache-read price means "not priced separately", not "free").
fn price_per_1m(raw: &Option<String>) -> Option<f64> {
    raw.as_ref()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|per_token| *per_token > 0.0)
        .map(|per_token| per_token * 1_000_000.0)
}

fn privacy_mode_enabled() -> bool {
    std::env::var(PRIVACY_MODE_ENV)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Maps one Nous Portal `/models` entry into the on-disk `models.json` shape consumed by
/// `builtin_providers::ModelDef`. Every entry is a chat-completion model (Nous's `/models` only
/// lists chat models — embeddings/other are a separate `type` filter in the portal UI, not the
/// API list), so `category` is uniformly "text".
fn map_entry(m: &NousModel, trains: bool, synced_at: chrono::DateTime<Utc>) -> serde_json::Value {
    let pricing = m.pricing.as_ref();
    json!({
        "slug": m.id,
        "display_name": m.name,
        "max_context_tokens": m.context_length,
        "max_output_tokens": m.top_provider.as_ref().and_then(|p| p.max_completion_tokens),
        "supports_function_calling": m.supported_parameters.iter().any(|p| p == "tools"),
        "supports_json_mode": m.supported_parameters.iter().any(|p| p == "response_format"),
        "price_input_per_1m": pricing.and_then(|p| price_per_1m(&p.prompt)),
        "price_output_per_1m": pricing.and_then(|p| price_per_1m(&p.completion)),
        "price_cached_input_per_1m": pricing.and_then(|p| price_per_1m(&p.input_cache_read)),
        "category": "text",
        "canonical_model": m.hugging_face_id.as_deref().filter(|s| !s.is_empty()),
        // Account-wide privacy posture (see module docs) — same value on every row.
        "trains_on_data": trains,
        "retains_data": trains,
        "price_synced_at": synced_at.to_rfc3339(),
    })
}

/// Fetches the live Nous Portal catalog, writes `{providers_dir}/nousportal/models.json`, and
/// upserts it via `load_from_dir`. `disable_missing` is always `true` for the nousportal brand —
/// staleness is the norm for an aggregator, same as openrouter/requesty.
pub fn sync(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
    base_url: &str,
) -> Result<NousPortalSyncSummary, String> {
    let url = format!("{base_url}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("nousportal fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("nousportal fetch returned error status: {e}"))?;
    let parsed: ModelsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse nousportal response: {e}"))?;

    let fetched = parsed.data.len();
    if fetched < MIN_SANE_MODEL_COUNT {
        tracing::warn!(
            fetched,
            min = MIN_SANE_MODEL_COUNT,
            "nousportal sync: suspiciously few models returned, skipping upsert"
        );
        return Ok(NousPortalSyncSummary {
            fetched,
            brands_added: 0,
            models_added: 0,
            models_updated: 0,
            models_disabled: 0,
            skipped_suspicious: true,
        });
    }

    let trains = !privacy_mode_enabled();
    let synced_at = Utc::now();
    let entries: Vec<serde_json::Value> = parsed
        .data
        .iter()
        .map(|m| map_entry(m, trains, synced_at))
        .collect();

    let dir = std::path::Path::new(providers_dir).join("nousportal");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let models_path = dir.join("models.json");
    let body = serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("failed to serialize models.json: {e}"))?;
    std::fs::write(&models_path, body)
        .map_err(|e| format!("failed to write {}: {e}", models_path.display()))?;

    let summary = load_from_dir(storage, providers_dir, true, true).map_err(|e| e.to_string())?;

    Ok(NousPortalSyncSummary {
        fetched,
        brands_added: summary.brands_added,
        models_added: summary.models_added,
        models_updated: summary.models_updated,
        models_disabled: summary.models_disabled,
        skipped_suspicious: false,
    })
}

/// Fetches and maps the catalog without touching disk or the DB — used by `--dry-run`.
pub fn fetch_preview(base_url: &str) -> Result<Vec<serde_json::Value>, String> {
    let url = format!("{base_url}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("nousportal fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("nousportal fetch returned error status: {e}"))?;
    let parsed: ModelsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse nousportal response: {e}"))?;
    let trains = !privacy_mode_enabled();
    let synced_at = Utc::now();
    Ok(parsed
        .data
        .iter()
        .map(|m| map_entry(m, trains, synced_at))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `deepseek/deepseek-v4-flash` entry from the live `/models` response — verifies the
    /// dollar-per-token → per-million scaling and, crucially, that `pricing.input_cache_read`
    /// lands in `price_cached_input_per_1m` (the field OpenRouter/Requesty don't carry).
    #[test]
    fn maps_cache_read_price_and_capabilities() {
        let raw = serde_json::json!({
            "id": "deepseek/deepseek-v4-flash",
            "name": "DeepSeek V4 Flash",
            "hugging_face_id": "deepseek-ai/DeepSeek-V4-Flash",
            "context_length": 1_048_576,
            "supported_parameters": ["tools", "response_format", "reasoning"],
            "pricing": {
                "prompt": "0.0000000660",
                "completion": "0.0000001320",
                "input_cache_read": "0.0000000132"
            },
            "top_provider": { "max_completion_tokens": 65_536 }
        });
        let m: NousModel = serde_json::from_value(raw).unwrap();
        let out = map_entry(&m, false, Utc::now());
        assert_eq!(out["slug"], "deepseek/deepseek-v4-flash");
        assert!((out["price_input_per_1m"].as_f64().unwrap() - 0.066).abs() < 1e-9);
        assert!((out["price_output_per_1m"].as_f64().unwrap() - 0.132).abs() < 1e-9);
        assert!((out["price_cached_input_per_1m"].as_f64().unwrap() - 0.0132).abs() < 1e-9);
        assert_eq!(out["supports_function_calling"], true);
        assert_eq!(out["supports_json_mode"], true);
        assert_eq!(out["canonical_model"], "deepseek-ai/DeepSeek-V4-Flash");
        assert_eq!(out["trains_on_data"], false);
        assert_eq!(out["retains_data"], false);
    }

    /// A `"0"` cache-read price means "not priced separately" — must map to `null`, not `0.0`
    /// (which would make the model look free on cache hits).
    #[test]
    fn zero_cache_read_price_is_null() {
        let m: NousModel = serde_json::from_value(serde_json::json!({
            "id": "x/y",
            "pricing": { "prompt": "0.000001", "completion": "0.000002", "input_cache_read": "0" }
        }))
        .unwrap();
        let out = map_entry(&m, true, Utc::now());
        assert!(out["price_cached_input_per_1m"].is_null());
        assert_eq!(out["trains_on_data"], true);
    }
}

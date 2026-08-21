//! Fetches OpenRouter's public model catalog (`GET {base_url}/models`) and upserts it into the
//! local provider directory + DB via the existing `builtin_providers::load_from_dir` path.
//!
//! OpenRouter aggregates ~400+ models and its catalog/pricing changes far more often than the
//! other (hand-curated) providers, so this is the one provider whose `models.json` is generated
//! rather than hand-written — see `providers/openrouter/models.json`. Called once at server
//! startup and then on an interval (`server/src/main.rs`), and on demand via
//! `proviz providers sync-openrouter` (`cli/src/main.rs`) for the initial "generate it once so I
//! can review it" step.

use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::{builtin_providers::load_from_dir, storage::CatalogStorage};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Below this many models, a fetch is treated as suspect (truncated/empty response) and the
/// whole sync is skipped rather than upserted — protects against `disable_missing` mass-disabling
/// the catalog on a bad response. OpenRouter has consistently listed 400+ models; 50 is a very
/// generous floor.
const MIN_SANE_MODEL_COUNT: usize = 50;

pub struct OpenRouterSyncSummary {
    pub fetched: usize,
    pub brands_added: usize,
    pub models_added: usize,
    pub models_updated: usize,
    pub models_disabled: usize,
    /// True if the fetch succeeded but was skipped for being suspiciously small
    /// (see `MIN_SANE_MODEL_COUNT`) — the DB was left untouched.
    pub skipped_suspicious: bool,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<OpenRouterModel>,
}

#[derive(Deserialize)]
struct OpenRouterModel {
    id: String,
    name: Option<String>,
    hugging_face_id: Option<String>,
    context_length: Option<u32>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    pricing: Option<OpenRouterPricing>,
    top_provider: Option<OpenRouterTopProvider>,
}

#[derive(Deserialize)]
struct OpenRouterPricing {
    prompt: Option<String>,
    completion: Option<String>,
}

#[derive(Deserialize)]
struct OpenRouterTopProvider {
    max_completion_tokens: Option<u32>,
}

/// Dollar-per-token string (e.g. `"0.0000008"`) → dollar-per-million-tokens. OpenRouter's
/// meta/auto-routing models (e.g. `openrouter/auto`, `openrouter/fusion`) publish `"-1"` as a
/// documented sentinel for "variable pricing, can't be quoted upfront" — treated as unknown
/// (`None`) rather than passed through, since a literal negative price would make the model
/// artificially win every selection on cost (lower is scored as cheaper).
fn price_per_1m(raw: &Option<String>) -> Option<f64> {
    raw.as_ref()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|per_token| *per_token >= 0.0)
        .map(|per_token| per_token * 1_000_000.0)
}

/// Maps one OpenRouter `/models` entry into the on-disk `models.json` shape consumed by
/// `builtin_providers::ModelDef`. Every entry OpenRouter lists is a chat-completion model
/// (verified: `architecture.modality` always ends in `->text`/`->text+image`/etc. — OpenRouter's
/// `/models` endpoint doesn't surface embeddings/image-gen/audio-only endpoints), so category is
/// uniformly "text" — no filtering needed to include every model.
fn map_entry(m: &OpenRouterModel, synced_at: chrono::DateTime<Utc>) -> serde_json::Value {
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
        "category": "text",
        // OpenRouter returns "" (not null) for closed-weight models with no HF repo (~139 of
        // them, e.g. OpenAI's own models) — treat that the same as no id.
        "canonical_model": m.hugging_face_id.as_deref().filter(|s| !s.is_empty()),
        "price_synced_at": synced_at.to_rfc3339(),
    })
}

/// Fetches the live OpenRouter catalog, writes `{providers_dir}/openrouter/models.json`, and
/// upserts it via `load_from_dir` (reusing the same brand/model dedup, UUID-reuse, and
/// price/capability-refresh logic every other provider goes through). `disable_missing` is
/// always passed as `true` to `load_from_dir` for the openrouter brand's own entries — staleness
/// is the norm for this provider, unlike hand-curated ones.
pub fn sync(
    storage: &dyn CatalogStorage,
    providers_dir: &str,
    base_url: &str,
) -> Result<OpenRouterSyncSummary, String> {
    let url = format!("{base_url}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("openrouter fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("openrouter fetch returned error status: {e}"))?;
    let parsed: ModelsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse openrouter response: {e}"))?;

    let fetched = parsed.data.len();
    if fetched < MIN_SANE_MODEL_COUNT {
        tracing::warn!(
            fetched,
            min = MIN_SANE_MODEL_COUNT,
            "openrouter sync: suspiciously few models returned, skipping upsert"
        );
        return Ok(OpenRouterSyncSummary {
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

    let openrouter_dir = std::path::Path::new(providers_dir).join("openrouter");
    std::fs::create_dir_all(&openrouter_dir)
        .map_err(|e| format!("failed to create {}: {e}", openrouter_dir.display()))?;
    let models_path = openrouter_dir.join("models.json");
    let body = serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("failed to serialize models.json: {e}"))?;
    std::fs::write(&models_path, body)
        .map_err(|e| format!("failed to write {}: {e}", models_path.display()))?;

    let summary = load_from_dir(storage, providers_dir, true, true).map_err(|e| e.to_string())?;

    Ok(OpenRouterSyncSummary {
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
        .map_err(|e| format!("openrouter fetch failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("openrouter fetch returned error status: {e}"))?;
    let parsed: ModelsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse openrouter response: {e}"))?;
    let synced_at = Utc::now();
    Ok(parsed
        .data
        .iter()
        .map(|m| map_entry(m, synced_at))
        .collect())
}

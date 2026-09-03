use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use uuid::Uuid;

/// One API account (key) for a brand. Multiple accounts can exist per brand
/// to distribute rate limits and share expenses across accounts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrandApiKey {
    pub id: Uuid,
    pub brand_id: Uuid,
    /// Name of the environment variable holding the actual API key secret.
    pub api_key_env: String,
    /// Lower = preferred (tried first). Default 0.
    pub priority: i16,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Brand {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub base_url: Option<String>,
    pub is_active: bool,
    /// Lower = tried first. Brands with same priority compete by rule.priority. Default 0.
    pub priority: i16,
    pub created_at: DateTime<Utc>,
    /// Relative traffic weight for load-balancing across brands within a candidate pool.
    /// Higher weight = more traffic directed here. Default 1.0 (equal share with peers).
    /// Used together with per-brand selection history to steer toward under-served brands.
    pub traffic_weight: f64,
    /// Provider-specific endpoint paths keyed by capability (e.g. "stt", "tts").
    /// Stored as JSON; None for brands that don't declare custom endpoints.
    pub endpoints: Option<JsonValue>,
    /// ISO currency code the model prices in this brand's `models.json` are denominated in.
    /// Defaults to `"USD"`. Non-USD prices are converted to USD for cost scoring and reporting
    /// via `crate::fx::FxRates`. Declared per-brand in `brand.json` (`"price_currency"`).
    #[serde(default = "default_currency")]
    pub price_currency: String,
}

pub(crate) fn default_currency() -> String {
    "USD".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: Uuid,
    pub brand_id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub max_context_tokens: u32,
    pub max_output_tokens: Option<u32>,
    pub supports_function_calling: bool,
    pub supports_json_mode: bool,
    /// Exact `reasoning_effort` literal to send to the provider for this model, or `None` to never
    /// send the param. A bool ("does it accept the param") isn't enough: acceptance ≠ effectiveness
    /// — some accept every literal without erroring while only one actually reduces reasoning (e.g.
    /// Scaleway's qwen3.5-397b-a17b only responds to `"none"`; `"low"` is accepted but does nothing).
    /// Others reject the off-switch entirely (OVHCloud/Scaleway Harmony-family gpt-oss/Qwen3.6 models
    /// 400 on `"none"`/`"minimal"`, only accept `"low"|"medium"|"high"`). So this stores the one
    /// literal confirmed to behave correctly for this specific model, not a capability flag.
    pub reasoning_effort_value: Option<String>,
    pub price_input_per_1m: Option<f64>,
    pub price_output_per_1m: Option<f64>,
    pub tpm_limit: Option<u32>,
    pub rpm_limit: Option<u32>,
    pub rpd_limit: Option<u32>,
    pub tpd_limit: Option<u64>,
    pub tpm_limit_month: Option<u64>,
    pub rps_limit: Option<f64>,
    pub quality_score: Option<f64>,
    pub avg_latency_ms: Option<u32>,
    pub is_enabled: bool,
    pub notes: Option<String>,
    /// Coarse capability tag: "text", "code", "embedding", "vision", "audio", "moderation"
    /// If set, callers must explicitly request this category to receive this model.
    pub category: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Multiplier applied to pricing when this model is used via batch API (e.g. 0.5 for 50% discount).
    /// None means no batch pricing is configured (standard prices apply).
    pub batch_price_multiplier: Option<f64>,
    /// STT capability: supports speaker diarization. None = unknown / not an STT model.
    pub diarization: Option<bool>,
    /// STT capability: supports real-time streaming transcription.
    pub streaming: Option<bool>,
    /// STT capability: supports HTTP batch transcription.
    pub http_batch: Option<bool>,
    /// STT capability: returns per-word timestamps.
    pub word_timestamps: Option<bool>,
    /// STT capability: returns new base url if different.
    pub base_url: Option<String>,
    /// Languages this model can be called for (ISO 639-1 codes, e.g. `["en","fr"]`).
    /// `None`/empty means unrestricted (model accepts any language — the common case for
    /// general-purpose LLMs). Set explicitly for models with real language limits (many
    /// STT/TTS models) so callers can filter with `SelectRequest.languages` and avoid
    /// calling a model in a language it doesn't support.
    pub supported_languages: Option<Vec<String>>,
    /// Key into `pz_model_catalog` identifying the underlying model family shared across brands
    /// (e.g. a HuggingFace `org/model` id). When set and this row omits `quality_score`/`category`/
    /// `max_context_tokens`/capability flags, `builtin_providers::load_from_dir` fills them in from
    /// the matching catalog entry at load time — this row's own values, when present, always win.
    /// `None` for models with no known cross-provider identity.
    pub canonical_key: Option<String>,
    /// When this row's pricing was last synced from a live provider catalog (e.g. OpenRouter's
    /// `/models` endpoint). `None` for hand-curated providers whose prices are only ever edited
    /// by hand — there's no "sync" to timestamp for those.
    pub price_synced_at: Option<DateTime<Utc>>,
    /// Whether this specific model/provider combination may train on submitted prompts, as
    /// reported by the source (e.g. Requesty's `data_used_for_training`). This is a fact about
    /// the provider, not the model family, so it's read directly from this row's own JSON —
    /// never inherited from `pz_model_catalog`. `None` when the source doesn't report it (most
    /// providers, including OpenRouter, don't — see `SelectRequest.require_no_training`).
    pub trains_on_data: Option<bool>,
    /// Whether this specific model/provider combination retains (stores) submitted prompts
    /// beyond serving the request, as reported by the source (e.g. Requesty's `data_retention`).
    /// Informational only today — not enforced by any selector filter (see
    /// `SelectRequest.require_no_training`, which filters on `trains_on_data`).
    pub retains_data: Option<bool>,
    /// Per-million-token price for *cached* input tokens (prompt-cache hits). Providers that keep
    /// a warm KV-cache of a repeated prompt prefix bill those tokens at a large discount
    /// (typically ~5–20% of the normal input price) and report the hit count in the response —
    /// DeepSeek's `prompt_cache_hit_tokens` / OpenAI-style `usage.prompt_tokens_details.cached_tokens`.
    /// `None` = provider doesn't cache or doesn't quote a distinct cached rate; cost then uses
    /// `price_input_per_1m` for all prompt tokens exactly as before. Denominated in the brand's
    /// `price_currency`, same as `price_input_per_1m`. Auto-filled from a live catalog's cache
    /// price (e.g. Nous Portal's `pricing.input_cache_read`) or hand-curated per row.
    pub price_cached_input_per_1m: Option<f64>,
}

/// A measured, task-specific quality score for one model.
/// Distinct from `Model.quality_score`, which is a single
/// hand-curated column shared across every step: a model good at claim extraction isn't
/// necessarily good at verdict synthesis, so this is keyed by `(model_id, step)` instead.
/// `Selector::effective_quality` prefers a row here over the global `Model.quality_score`,
/// falling back to it when no step-specific measurement exists yet (e.g. every worker step,
/// until a worker benchmark feeds this table too). Populated exclusively by automated sync
/// (`POST /catalog/step-quality`) — never hand-edited, so there's no "preserve the curated
/// value" tension: each push is a plain upsert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStepQuality {
    pub model_id: Uuid,
    pub step: String,
    /// Measured score, same `[0.0, 1.0]` scale as `Model.quality_score` (e.g. a benchmark
    /// pass-rate).
    pub quality_score: f64,
    /// How many benchmark cases this score was computed from — informational (surfaced via the
    /// catalog, not read by the selector), lets an operator judge how much to trust a score
    /// computed from very few cases.
    pub sample_size: i32,
    pub updated_at: DateTime<Utc>,
}

/// Shared intrinsic properties for a model family, keyed by a manually-curated `canonical_key`
/// (typically a HuggingFace `org/model` id). Lets brands that host the same underlying model
/// (e.g. "deepseek-v4-flash" under groq, ovh, and openrouter) share one `quality_score`/`category`/
/// context/capability definition instead of re-curating it per brand. Purely additive: a model row
/// with no `canonical_key`, or one that sets its own values, is unaffected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: Uuid,
    pub canonical_key: String,
    pub display_name: Option<String>,
    pub category: Option<String>,
    pub max_context_tokens: Option<u32>,
    pub supports_function_calling: Option<bool>,
    pub supports_json_mode: Option<bool>,
    pub quality_score: Option<f64>,
    pub knowledge_cutoff: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionRule {
    pub id: Uuid,
    pub step: String,
    pub model_id: Uuid,
    pub priority: i16,
    pub max_ctx_tokens: Option<u32>,
    pub requires_fn_call: bool,
    pub is_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitErrorType {
    Tpm,
    Rpm,
    Tpd,
    Auth,
    Timeout,
    Parse,
    Other,
}

impl RateLimitErrorType {
    /// True for error types that indicate a problem with the account/key itself
    /// (quota exhaustion, bad credentials) rather than the specific model that
    /// happened to be called. These should block the shared key so every model
    /// behind it backs off together. Model-scoped types (a slow/flaky model
    /// timing out, a malformed response, a one-off error) must NOT block sibling
    /// models that happen to share the same key — see report_error/report_rate_limit,
    /// which route by this instead of unconditionally keying off brand_key_id.
    /// Without this split, a single-key brand with multiple models (e.g. one
    /// OVHCloud key serving four Qwen variants) has one flaky model repeatedly
    /// locking out its perfectly healthy siblings for cooldown_secs() at a time.
    pub fn is_account_scoped(&self) -> bool {
        matches!(self, Self::Auth | Self::Rpm | Self::Tpm | Self::Tpd)
    }

    /// TTL in seconds before a model blocked by this error type is retried.
    pub fn cooldown_secs(&self) -> u64 {
        match self {
            Self::Tpm => 60,
            Self::Rpm => 60,
            Self::Tpd => 3600,
            Self::Auth => 300,
            Self::Timeout => 30,
            Self::Parse => 0, // parse failures don't rate-limit; still logged
            Self::Other => 60,
        }
    }
}

impl std::str::FromStr for RateLimitErrorType {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "tpm" => Ok(Self::Tpm),
            "rpm" => Ok(Self::Rpm),
            "tpd" => Ok(Self::Tpd),
            "auth" => Ok(Self::Auth),
            "timeout" => Ok(Self::Timeout),
            "parse" => Ok(Self::Parse),
            "other" => Ok(Self::Other),
            other => Err(format!("unknown error type: {other}")),
        }
    }
}

impl std::fmt::Display for RateLimitErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Tpm => "tpm",
            Self::Rpm => "rpm",
            Self::Tpd => "tpd",
            Self::Auth => "auth",
            Self::Timeout => "timeout",
            Self::Parse => "parse",
            Self::Other => "other",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    /// Group-level default for the Pass-2 cost/latency/quality scoring weights (see
    /// `SelectRequest.cost_weight`/`latency_weight`/`quality_weight`). A group represents a
    /// stable task (e.g. "detector", "worker") whose inherent quality/speed/price tradeoff
    /// doesn't change per call, so it's configured once here rather than re-sent by every
    /// caller. Precedence at selection time: request override > group override > built-in
    /// default. `None` on all three reproduces today's behavior exactly.
    #[serde(default)]
    pub cost_weight_override: Option<f32>,
    #[serde(default)]
    pub latency_weight_override: Option<f32>,
    #[serde(default)]
    pub quality_weight_override: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub id: Uuid,
    pub group_id: Uuid,
    pub model_id: Uuid,
    /// Lower = tried first within the group (tiebreaker alongside brand priority).
    pub priority: i16,
    pub is_enabled: bool,
}

/// Input to /select
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectRequest {
    pub step: String,
    pub estimated_tokens: u32,
    #[serde(default)]
    pub requires_fn_call: bool,
    #[serde(default)]
    pub requires_json_mode: bool,
    /// STT-only: pick the streaming-mode row (true) or the HTTP-batch-mode row (false) when a
    /// model has both (see `Model.streaming`/`Model.http_batch`). `None` (default) doesn't
    /// filter on call mode — irrelevant for non-STT models, which never have more than one row
    /// per slug.
    #[serde(default)]
    pub requires_streaming: Option<bool>,
    #[serde(default)]
    pub quality_min: f32,
    #[serde(default)]
    pub exclude_ids: Vec<Uuid>,
    /// If non-empty, only models whose category is in this list are eligible.
    /// Use to explicitly request specialized models (e.g. ["audio"], ["embedding"]).
    #[serde(default)]
    pub categories: Vec<String>,
    /// If non-empty, only models whose `supported_languages` overlaps this list are eligible
    /// (ISO 639-1 codes, e.g. `["en","fr"]`). Models with `supported_languages = None` (no
    /// restriction declared) always pass this filter — the language column is opt-in and
    /// mainly relevant for STT/TTS models with real language limits.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Restrict candidates to models belonging to this group (by UUID). Takes priority over rules.
    #[serde(default)]
    pub group_id: Option<Uuid>,
    /// Restrict candidates to models belonging to this group (by slug). Takes priority over rules.
    #[serde(default)]
    pub group_name: Option<String>,
    /// When true (default), member.priority is used as a tiebreaker within the same brand.
    /// When false, only brand.priority and the selection score determine order.
    #[serde(default = "default_true")]
    pub use_member_priority: bool,
    /// Maximum time (ms) to wait server-side if all models are exhausted.
    /// When set and `retry_after_ms <= max_wait_ms`, the server sleeps and retries the
    /// selection once before returning 409. Saves a client round-trip on short waits.
    #[serde(default)]
    pub max_wait_ms: Option<u64>,
    /// When true, only models the source explicitly reports as NOT training on submitted data
    /// (`Model.trains_on_data == Some(false)`) are eligible — a model with unknown status
    /// (`None`, the common case: most providers, including OpenRouter, don't report this at all)
    /// is excluded too, not treated as safe by default. This is the opposite convention from
    /// `languages`/`categories` (there, no declared restriction means "unrestricted"; here, no
    /// declared info means "unverified," which is the conservative choice for a privacy filter).
    /// `/complete` additionally sends OpenRouter's `provider: {data_collection: "deny", zdr:
    /// true}` request-level opt-out when this is set, since OpenRouter doesn't publish per-model
    /// training info at all — the only way to protect those calls is the request flag itself.
    #[serde(default)]
    pub require_no_training: bool,
    /// Override the Pass-2 scoring weight given to the cost component (built-in default 0.15,
    /// 0.20 without group priority — see `Selector::select`). `None` uses the group's own
    /// `cost_weight_override` when group-based, else the built-in default. When set, the whole
    /// weight set (including untouched components) is renormalized to sum to 1.0, so raising
    /// this shrinks the others proportionally rather than zeroing them out — a low-quality or
    /// rate-limited model still can't casually win just because it's cheap. Range `[0.0, 1.0]`;
    /// out-of-range values are clamped.
    #[serde(default)]
    pub cost_weight: Option<f32>,
    /// Same contract as `cost_weight`, for the latency component (built-in default 0.10).
    #[serde(default)]
    pub latency_weight: Option<f32>,
    /// Same contract as `cost_weight`, for the quality component (built-in default 0.20).
    #[serde(default)]
    pub quality_weight: Option<f32>,
    /// Benchmark / A-B hook: pin selection to ONE specific model, bypassing group and step
    /// rules entirely — every enabled catalog model is considered and Pass 1 keeps only the
    /// one whose `brand_slug/model_slug`, bare `model_slug`, or `canonical_key` matches this
    /// string (tolerant match: callers may pass a litellm-style `brand/slug`). Heatroom,
    /// rate-limit and exhaustion handling are unchanged — a pinned model that is rate-limited
    /// still waits/retries then 409s. `None` (default) = normal selection.
    #[serde(default)]
    pub pin_model: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Output of /select
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCandidate {
    pub model_id: Uuid,
    pub brand_slug: String,
    pub model_slug: String,
    pub api_key_env: Option<String>,
    /// Brand's OpenAI-compatible API base URL (e.g. `https://api.scaleway.ai/v1`). `None` when the
    /// brand relies on a well-known default endpoint (e.g. groq/mistral). Exposed so the server-side
    /// `/complete` path can build the `{base_url}/chat/completions` request without a catalog lookup.
    #[serde(default)]
    pub base_url: Option<String>,
    /// ID of the specific BrandApiKey selected for this call. Present when the brand has rows in
    /// pz_brand_api_keys; None for legacy single-key brands. Echo back in ReportRequest so the
    /// server knows which key to mark rate-limited on a 429.
    #[serde(default)]
    pub brand_key_id: Option<Uuid>,
    pub max_context_tokens: u32,
    pub supports_function_calling: bool,
    pub supports_json_mode: bool,
    /// Exact `reasoning_effort` literal `/complete` should send for this model, or `None` to omit
    /// the param entirely. See `Model.reasoning_effort_value`. `/complete` uses this — not any
    /// caller-supplied value — since only the server knows which model was picked.
    #[serde(default)]
    pub reasoning_effort_value: Option<String>,
    pub estimated_input_cost_usd: Option<f64>,
    /// Echoed from SelectRequest so callers can include it in /report for accurate window tracking.
    pub estimated_tokens: u64,
    /// Provider's per-million-token input price. Exposed so callers can compute actual_cost_usd
    /// client-side (prompt_tokens / 1M × price_input + completion_tokens / 1M × price_output).
    pub price_input_per_1m: Option<f64>,
    /// Provider's per-million-token output price.
    pub price_output_per_1m: Option<f64>,
    /// Provider's per-million-token price for *cached* input tokens (prompt-cache hits), when the
    /// provider prices them separately (DeepSeek, Nous Portal, OpenAI, Anthropic, …). `None` when
    /// the provider doesn't cache or doesn't quote a distinct cached rate — callers should then
    /// fall back to `price_input_per_1m` for every prompt token. See `Model.price_cached_input_per_1m`.
    #[serde(default)]
    pub price_cached_input_per_1m: Option<f64>,
    /// Multiplier applied to pricing when this model is used via batch API (e.g. 0.5 for 50% discount).
    #[serde(default)]
    pub batch_price_multiplier: Option<f64>,
    /// Brand's chat-completions path (e.g. `/openai/v1/chat/completions`), read from
    /// `brand.endpoints.chat`. `None` when the brand relies on the default `/chat/completions`
    /// suffix. Lets `/complete` build the request URL without a catalog lookup.
    #[serde(default)]
    pub chat_path: Option<String>,
    /// Echo of `Model.canonical_key` — the shared model-family key this row was resolved against,
    /// if any. See `ModelCatalogEntry`.
    #[serde(default)]
    pub canonical_key: Option<String>,
    /// Echo of `Model.price_synced_at` — when this row's pricing was last synced from a live
    /// provider catalog. `None` for hand-curated (non-auto-synced) providers.
    #[serde(default)]
    pub price_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Echo of `Model.trains_on_data`. `None` when the source doesn't report it.
    #[serde(default)]
    pub trains_on_data: Option<bool>,
    /// Echo of `Model.retains_data`. `None` when the source doesn't report it.
    #[serde(default)]
    pub retains_data: Option<bool>,
    /// Echo of `Brand.price_currency` — the ISO currency `price_input_per_1m` /
    /// `price_output_per_1m` on this candidate are denominated in. `"USD"` for the vast
    /// majority of brands. `estimated_input_cost_usd` is always already in USD.
    #[serde(default = "default_currency")]
    pub price_currency: String,
}

/// Input to /report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportRequest {
    pub model_id: Uuid,
    pub outcome: ReportOutcome,
    #[serde(default)]
    pub error_type: Option<RateLimitErrorType>,
    /// Echo of ModelCandidate.estimated_tokens — used to release the in-flight reservation.
    /// Omitting this (legacy clients) leaves the in-flight counter inflated until expiry,
    /// which is safe (pessimistic direction).
    #[serde(default)]
    pub estimated_tokens: Option<u64>,
    /// Actual tokens consumed as reported by the provider. Improves TPM window accuracy.
    /// When both prompt_tokens and completion_tokens are set, their sum is preferred over this field.
    #[serde(default)]
    pub actual_tokens: Option<u64>,
    /// Input (prompt) tokens from the provider response (e.g. response.usage.prompt_tokens).
    /// Used together with completion_tokens for accurate cost computation.
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    /// Output (completion) tokens from the provider response (e.g. response.usage.completion_tokens).
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    /// Of `prompt_tokens`, how many were served from the provider's prompt cache (a warm KV-cache
    /// of a repeated prefix), billed at the discounted `Model.price_cached_input_per_1m` rather
    /// than the full input rate. From `usage.prompt_tokens_details.cached_tokens` (OpenAI/Nous
    /// shape) or `usage.prompt_cache_hit_tokens` (DeepSeek). `None`/`0` = no cache hit, cost is
    /// computed exactly as before. Only affects the fallback catalog-price computation — a
    /// provider-supplied `actual_cost_usd` still wins outright.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    /// Remaining requests in the current window as reported by the provider response headers
    /// (e.g. `x-ratelimit-remaining-requests`). Used to anchor the UsageTracker windows
    /// against provider reality rather than relying solely on internal estimation.
    #[serde(default)]
    pub remaining_requests: Option<u32>,
    /// Remaining tokens in the current window as reported by the provider response headers
    /// (e.g. `x-ratelimit-remaining-tokens`).
    #[serde(default)]
    pub remaining_tokens: Option<u64>,
    /// Actual RPM limit reported by the provider (e.g. `x-ratelimit-limit-req-minute`).
    /// When `sync_limits=true`, overwrites the model's `rpm_limit` in storage if it changed.
    #[serde(default)]
    pub limit_requests: Option<u32>,
    /// Actual TPM limit reported by the provider (e.g. `x-ratelimit-limit-tokens-minute`).
    /// When `sync_limits=true`, overwrites the model's `tpm_limit` in storage if it changed.
    #[serde(default)]
    pub limit_tokens: Option<u32>,
    /// When true, sync `limit_requests`/`limit_tokens` back to the DB if they differ from the
    /// stored values. Keeps configured limits aligned with actual provider plan without manual edits.
    #[serde(default)]
    pub sync_limits: bool,
    /// Echo of ModelCandidate.brand_key_id. When set, the server marks this specific key as
    /// rate-limited rather than the model, allowing other keys for the same brand to still serve.
    #[serde(default)]
    pub brand_key_id: Option<Uuid>,
    /// Actual cost in USD as reported by the provider itself (e.g. OpenRouter's `usage.cost`,
    /// computed server-side from whichever upstream sub-provider actually served the request —
    /// which can price differently than the catalog's static `price_input_per_1m`/`price_output_per_1m`).
    /// When set, `Selector::report_success` returns this verbatim instead of computing an estimate
    /// from the catalog price. Most providers have a single fixed price and don't need this —
    /// only set it when the provider actually returns a real, request-specific cost figure.
    #[serde(default)]
    pub actual_cost_usd: Option<f64>,
    /// Observed end-to-end response time (ms) of the actual provider call — measured by the
    /// caller around the request that was sent (e.g. wall-clock elapsed for the `/chat/completions`
    /// HTTP round-trip). Feeds a live per-`(model, key)` EWMA in `UsageTracker` that scoring's
    /// `latency_score` prefers over the static catalog `Model.avg_latency_ms` once populated —
    /// lets slow-routing aggregators (e.g. OpenRouter, Requesty) get penalised based on their
    /// actual observed latency instead of a hand-curated guess. Omit when no provider call was
    /// actually made (e.g. a pre-flight validation failure) — that's not a latency sample.
    #[serde(default)]
    pub response_time_ms: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReportOutcome {
    Success,
    RateLimit,
    Error,
}

/// Response body returned by POST /report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportResponse {
    pub status: &'static str,
    /// Computed actual cost in USD when the model's prices and token counts are both known.
    /// Only populated for outcome=success with prompt_tokens + completion_tokens set.
    pub actual_cost_usd: Option<f64>,
}

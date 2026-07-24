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
    #[serde(default)]
    pub quality_min: f32,
    #[serde(default)]
    pub exclude_ids: Vec<Uuid>,
    /// If non-empty, only models whose category is in this list are eligible.
    /// Use to explicitly request specialized models (e.g. ["audio"], ["embedding"]).
    #[serde(default)]
    pub categories: Vec<String>,
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
    pub estimated_input_cost_usd: Option<f64>,
    /// Echoed from SelectRequest so callers can include it in /report for accurate window tracking.
    pub estimated_tokens: u64,
    /// Provider's per-million-token input price. Exposed so callers can compute actual_cost_usd
    /// client-side (prompt_tokens / 1M × price_input + completion_tokens / 1M × price_output).
    pub price_input_per_1m: Option<f64>,
    /// Provider's per-million-token output price.
    pub price_output_per_1m: Option<f64>,
    /// Multiplier applied to pricing when this model is used via batch API (e.g. 0.5 for 50% discount).
    #[serde(default)]
    pub batch_price_multiplier: Option<f64>,
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

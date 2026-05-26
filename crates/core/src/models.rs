use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Brand {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub is_active: bool,
    /// Lower = tried first. Brands with same priority compete by rule.priority. Default 0.
    pub priority: i16,
    pub created_at: DateTime<Utc>,
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
    pub max_context_tokens: u32,
    pub supports_function_calling: bool,
    pub supports_json_mode: bool,
    pub estimated_input_cost_usd: Option<f64>,
    /// Echoed from SelectRequest so callers can include it in /report for accurate window tracking.
    pub estimated_tokens: u64,
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
    #[serde(default)]
    pub actual_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReportOutcome {
    Success,
    RateLimit,
    Error,
}

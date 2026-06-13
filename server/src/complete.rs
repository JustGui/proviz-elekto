//! Synchronous `/complete` endpoint: select → call provider → report, in one round-trip.
//!
//! This is the server-side equivalent of the Python client's `call_litellm` loop. The caller no
//! longer needs to embed litellm (or any provider SDK): it POSTs messages, the server picks a
//! model, calls the provider's OpenAI-compatible `/chat/completions` endpoint, reports usage back
//! to the selector, and returns the parsed text + token usage + cost. All our providers (groq,
//! mistral, ovh, scaleway) are OpenAI-compatible, so a single payload shape works for every brand.

use std::sync::Arc;
use std::time::Duration;

use axum::{http::StatusCode, response::IntoResponse, Json};
use proviz_elekto_core::models::{
    ModelCandidate, RateLimitErrorType, ReportOutcome, ReportRequest, SelectRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{apply_report, select_with_wait, AppState};

/// Default per-call provider HTTP timeout (seconds). Mirrors the Python client default (~120s).
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Maximum number of distinct models to try before giving up with 502.
const MAX_PROVIDER_ATTEMPTS: usize = 4;

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct CompleteRequest {
    // ── selection fields (mirror SelectRequest) ─────────────────────────────
    pub step: String,
    #[serde(default = "default_estimated_tokens")]
    pub estimated_tokens: u32,
    #[serde(default)]
    pub requires_fn_call: bool,
    #[serde(default)]
    pub requires_json_mode: bool,
    #[serde(default)]
    pub quality_min: f32,
    #[serde(default)]
    pub exclude_ids: Vec<Uuid>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub group_id: Option<Uuid>,
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(default)]
    pub max_wait_ms: Option<u64>,

    // ── completion fields ───────────────────────────────────────────────────
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Pass-through to the provider, e.g. `{"type":"json_object"}`.
    #[serde(default)]
    pub response_format: Option<Value>,
    /// Tool-use: forwarded to the provider verbatim. Returned `tool_calls` are NOT executed —
    /// the caller drives the loop (mirrors `call_litellm_tool_loop` semantics).
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// Per-call provider HTTP timeout (seconds). Defaults to `DEFAULT_TIMEOUT_SECS`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

fn default_estimated_tokens() -> u32 {
    1000
}

#[derive(Debug, Serialize)]
pub struct CompleteResponse {
    pub text: String,
    /// Un-executed tool calls from the provider response (`choices[0].message.tool_calls`), or null.
    pub tool_calls: Option<Value>,
    pub model: String,
    pub brand: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_usd: Option<f64>,
}

impl CompleteRequest {
    fn to_select_request(&self, exclude_ids: Vec<Uuid>) -> SelectRequest {
        SelectRequest {
            step: self.step.clone(),
            estimated_tokens: self.estimated_tokens,
            requires_fn_call: self.requires_fn_call || self.tools.is_some(),
            requires_json_mode: self.requires_json_mode,
            quality_min: self.quality_min,
            exclude_ids,
            categories: self.categories.clone(),
            group_id: self.group_id,
            group_name: self.group_name.clone(),
            use_member_priority: true,
            max_wait_ms: self.max_wait_ms,
        }
    }

    fn payload(&self, model_slug: &str) -> Value {
        let messages: Vec<Value> = self
            .messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect();
        let mut body = json!({
            "model": model_slug,
            "messages": messages,
        });
        let obj = body.as_object_mut().expect("payload is object");
        if let Some(t) = self.temperature {
            obj.insert("temperature".into(), json!(t));
        }
        if let Some(mt) = self.max_tokens {
            obj.insert("max_tokens".into(), json!(mt));
        }
        if let Some(ref rf) = self.response_format {
            obj.insert("response_format".into(), rf.clone());
        }
        if let Some(ref tools) = self.tools {
            obj.insert("tools".into(), json!(tools));
        }
        if let Some(ref tc) = self.tool_choice {
            obj.insert("tool_choice".into(), tc.clone());
        }
        body
    }
}

/// Drive selection → provider call → report. On provider failure, the failed model is excluded
/// and the next-best candidate is selected, up to `MAX_PROVIDER_ATTEMPTS`.
pub async fn run_complete(state: Arc<AppState>, req: CompleteRequest) -> axum::response::Response {
    let timeout = Duration::from_secs(req.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let mut exclude_ids = req.exclude_ids.clone();
    let mut last_error = String::from("no provider attempted");

    for attempt in 0..MAX_PROVIDER_ATTEMPTS {
        let select_req = req.to_select_request(exclude_ids.clone());
        let candidate = match select_with_wait(&state, select_req).await {
            Ok(c) => c,
            Err(e) => return select_error_to_response(e),
        };

        let base_url = match resolve_base_url(&candidate.brand_slug, &candidate.base_url) {
            Some(u) => u,
            None => {
                warn!(brand = %candidate.brand_slug, "no base_url and no default endpoint known");
                // Unusable model: release the reservation and skip it on the next attempt.
                report(
                    &state,
                    &candidate,
                    ReportOutcome::Error,
                    RateLimitErrorType::Other,
                    None,
                    None,
                    None,
                    None,
                )
                .await;
                exclude_ids.push(candidate.model_id);
                last_error = format!("no endpoint for brand '{}'", candidate.brand_slug);
                continue;
            }
        };

        let api_key_env = candidate.api_key_env.clone().unwrap_or_default();
        let api_key = match std::env::var(&api_key_env) {
            Ok(k) if !k.is_empty() => k,
            _ => {
                warn!(env = %api_key_env, "API key env var not set or empty");
                report(
                    &state,
                    &candidate,
                    ReportOutcome::Error,
                    RateLimitErrorType::Auth,
                    None,
                    None,
                    None,
                    None,
                )
                .await;
                exclude_ids.push(candidate.model_id);
                last_error = format!("API key env var '{api_key_env}' not set");
                continue;
            }
        };

        let url = format!("{base_url}/chat/completions");
        let payload = req.payload(&candidate.model_slug);
        debug!(
            attempt,
            model = %candidate.model_slug,
            brand = %candidate.brand_slug,
            "complete: calling provider"
        );

        match call_provider(&state.http, &url, &api_key, &payload, timeout).await {
            Ok(parsed) => {
                let cost = report(
                    &state,
                    &candidate,
                    ReportOutcome::Success,
                    RateLimitErrorType::Other,
                    Some(parsed.prompt_tokens),
                    Some(parsed.completion_tokens),
                    parsed.remaining_requests,
                    parsed.remaining_tokens,
                )
                .await;
                let cost_usd = cost.or_else(|| {
                    compute_cost(
                        candidate.price_input_per_1m,
                        candidate.price_output_per_1m,
                        parsed.prompt_tokens,
                        parsed.completion_tokens,
                    )
                });
                return (
                    StatusCode::OK,
                    Json(CompleteResponse {
                        text: parsed.text,
                        tool_calls: parsed.tool_calls,
                        model: candidate.model_slug.clone(),
                        brand: candidate.brand_slug.clone(),
                        prompt_tokens: parsed.prompt_tokens,
                        completion_tokens: parsed.completion_tokens,
                        cost_usd,
                    }),
                )
                    .into_response();
            }
            Err(ProviderError {
                message,
                is_rate_limit,
            }) => {
                warn!(
                    model = %candidate.model_slug,
                    brand = %candidate.brand_slug,
                    error = %message,
                    is_rate_limit,
                    "complete: provider call failed — trying next candidate"
                );
                let (outcome, et) = if is_rate_limit {
                    (ReportOutcome::RateLimit, RateLimitErrorType::Rpm)
                } else {
                    (ReportOutcome::Error, RateLimitErrorType::Other)
                };
                report(&state, &candidate, outcome, et, None, None, None, None).await;
                exclude_ids.push(candidate.model_id);
                last_error = message;
            }
        }
    }

    (
        StatusCode::BAD_GATEWAY,
        Json(json!({
            "error": "all_providers_failed",
            "detail": last_error,
        })),
    )
        .into_response()
}

fn select_error_to_response(e: proviz_elekto_core::error::ProvizError) -> axum::response::Response {
    use proviz_elekto_core::error::ProvizError;
    match e {
        ProvizError::AllModelsExhausted {
            step,
            tried,
            retry_after_ms,
        } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "all_models_exhausted",
                "step": step,
                "tried": tried,
                "retry_after_ms": retry_after_ms,
            })),
        )
            .into_response(),
        ProvizError::GroupNotFound(name) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group_not_found", "group": name })),
        )
            .into_response(),
        other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": other.to_string() })),
        )
            .into_response(),
    }
}

/// Resolve the OpenAI-compatible base URL for a brand. Brands that store a `base_url` (ovh,
/// scaleway) use it verbatim; brands that rely on a well-known endpoint (groq, mistral) fall back
/// to the canonical default. The brand slug may carry a multi-account suffix (e.g. `groq-free`),
/// so we match on the leading segment.
fn resolve_base_url(brand_slug: &str, base_url: &Option<String>) -> Option<String> {
    if let Some(u) = base_url {
        if !u.is_empty() {
            return Some(u.trim_end_matches('/').to_string());
        }
    }
    let prefix = brand_slug.split('-').next().unwrap_or(brand_slug);
    let default = match prefix {
        "groq" => "https://api.groq.com/openai/v1",
        "mistral" => "https://api.mistral.ai/v1",
        _ => return None,
    };
    Some(default.to_string())
}

struct ParsedCompletion {
    text: String,
    tool_calls: Option<Value>,
    prompt_tokens: u64,
    completion_tokens: u64,
    remaining_requests: Option<u32>,
    remaining_tokens: Option<u64>,
}

struct ProviderError {
    message: String,
    is_rate_limit: bool,
}

async fn call_provider(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<ParsedCompletion, ProviderError> {
    let resp = http
        .post(url)
        .bearer_auth(api_key)
        .timeout(timeout)
        .json(payload)
        .send()
        .await
        .map_err(|e| ProviderError {
            is_rate_limit: false,
            message: e.to_string(),
        })?;

    let status = resp.status();
    // Capture rate-limit headers before consuming the body.
    let (remaining_requests, remaining_tokens) = extract_remaining(resp.headers());

    if !status.is_success() {
        let is_rate_limit = status.as_u16() == 429;
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError {
            is_rate_limit,
            message: format!("HTTP {status}: {body}"),
        });
    }

    let body: Value = resp.json().await.map_err(|e| ProviderError {
        is_rate_limit: false,
        message: format!("invalid JSON response: {e}"),
    })?;

    let message = &body["choices"][0]["message"];
    let text = message["content"].as_str().unwrap_or_default().to_string();
    let tool_calls = match &message["tool_calls"] {
        Value::Null => None,
        v => Some(v.clone()),
    };
    let usage = &body["usage"];
    let prompt_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
    let completion_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);

    Ok(ParsedCompletion {
        text,
        tool_calls,
        prompt_tokens,
        completion_tokens,
        remaining_requests,
        remaining_tokens,
    })
}

/// Read provider rate-limit headers. Mirrors the key list in the Python client's
/// `_extract_provider_limits` so groq/mistral/openai/anthropic variants are all handled.
fn extract_remaining(headers: &reqwest::header::HeaderMap) -> (Option<u32>, Option<u64>) {
    fn parse(headers: &reqwest::header::HeaderMap, keys: &[&str]) -> Option<u64> {
        for key in keys {
            if let Some(val) = headers.get(*key) {
                if let Ok(s) = val.to_str() {
                    if let Ok(n) = s.trim().parse::<u64>() {
                        return Some(n);
                    }
                }
            }
        }
        None
    }
    let remaining_requests = parse(
        headers,
        &[
            "x-ratelimit-remaining-requests",
            "ratelimit-remaining-requests",
            "x-ratelimit-remaining-req-minute",
            "anthropic-ratelimit-requests-remaining",
        ],
    )
    .map(|v| v as u32);
    let remaining_tokens = parse(
        headers,
        &[
            "x-ratelimit-remaining-tokens",
            "ratelimit-remaining-tokens",
            "x-ratelimit-remaining-tokens-minute",
            "anthropic-ratelimit-tokens-remaining",
        ],
    );
    (remaining_requests, remaining_tokens)
}

#[allow(clippy::too_many_arguments)]
async fn report(
    state: &Arc<AppState>,
    candidate: &ModelCandidate,
    outcome: ReportOutcome,
    error_type: RateLimitErrorType,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    remaining_requests: Option<u32>,
    remaining_tokens: Option<u64>,
) -> Option<f64> {
    let report_req = ReportRequest {
        model_id: candidate.model_id,
        outcome,
        error_type: Some(error_type),
        estimated_tokens: Some(candidate.estimated_tokens),
        actual_tokens: None,
        prompt_tokens,
        completion_tokens,
        remaining_requests,
        remaining_tokens,
        limit_requests: None,
        limit_tokens: None,
        sync_limits: false,
        brand_key_id: candidate.brand_key_id,
    };
    let sel = state.selector.clone();
    tokio::task::spawn_blocking(move || apply_report(&sel, report_req))
        .await
        .expect("report task panicked")
}

fn compute_cost(
    price_in: Option<f64>,
    price_out: Option<f64>,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Option<f64> {
    if price_in.is_none() && price_out.is_none() {
        return None;
    }
    Some(
        (price_in.unwrap_or(0.0) * prompt_tokens as f64
            + price_out.unwrap_or(0.0) * completion_tokens as f64)
            / 1_000_000.0,
    )
}

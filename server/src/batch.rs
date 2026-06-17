use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use proviz_elekto_core::{
    models::{RateLimitErrorType, SelectRequest},
    selector::Selector,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A single request waiting in the batch queue.
struct QueuedEntry {
    request_id: Uuid,
    model_id: Uuid,
    model_slug: String,
    api_key_env: String,
    brand_key_id: Option<Uuid>,
    estimated_tokens: u64,
    price_input_per_1m: Option<f64>,
    price_output_per_1m: Option<f64>,
    batch_price_multiplier: Option<f64>,
    messages: Value,
    extra_body: Value,
}

pub enum BatchResultState {
    Success {
        body: Value,
        prompt_tokens: u64,
        completion_tokens: u64,
        actual_cost_usd: Option<f64>,
    },
    Error {
        message: String,
    },
}

/// Request body for POST /batch/submit
#[derive(Debug, Deserialize)]
pub struct BatchSubmitRequest {
    pub step: String,
    #[serde(default = "default_estimated_tokens")]
    pub estimated_tokens: u32,
    pub messages: Value,
    #[serde(default)]
    pub extra_body: Value,
    #[serde(default)]
    pub requires_fn_call: bool,
    #[serde(default)]
    pub requires_json_mode: bool,
    #[serde(default)]
    pub quality_min: f32,
    #[serde(default)]
    pub exclude_ids: Vec<Uuid>,
    /// If empty, defaults to ["text", "code"] to ensure only batch-compatible models are selected.
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub group_id: Option<Uuid>,
    #[serde(default)]
    pub group_name: Option<String>,
}

fn default_estimated_tokens() -> u32 {
    1000
}

/// Response body for POST /batch/submit
#[derive(Debug, Serialize)]
pub struct BatchSubmitResponse {
    pub request_id: Uuid,
    /// Hint to the client: how long before a result is likely ready (ms).
    /// Clients should sleep at least this long before polling /batch/result/{id}.
    pub retry_after_ms: u64,
}

/// Response body for GET /batch/result/{id}
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BatchResultResponse {
    Pending {
        retry_after_ms: u64,
    },
    Success {
        body: Value,
        prompt_tokens: u64,
        completion_tokens: u64,
        actual_cost_usd: Option<f64>,
    },
    Error {
        message: String,
    },
}

// ── BatchQueue ────────────────────────────────────────────────────────────────

/// Server-side batch queue. Accumulates requests from all workers, flushes them
/// to Mistral's Batch API after `window_secs`, then stores results for workers to poll.
pub struct BatchQueue {
    /// Pending entries grouped by model_id (one Mistral batch job per model).
    queue: Mutex<HashMap<Uuid, Vec<QueuedEntry>>>,
    /// Results indexed by request_id (set by the flush task after Mistral completes).
    pub results: Arc<DashMap<Uuid, BatchResultState>>,
    /// Signal to trigger an early flush before the window expires.
    pub flush_notify: Arc<Notify>,
    /// Time to accumulate before flushing.
    pub window_secs: u64,
    /// Flush early if total queue size reaches this.
    pub max_batch_size: usize,
    /// Mistral API base URL.
    pub mistral_base_url: String,
    /// Wall-clock time the current window started (used to estimate retry_after_ms for clients).
    window_start: Mutex<Instant>,
}

impl BatchQueue {
    pub fn new(window_secs: u64, max_batch_size: usize, mistral_base_url: String) -> Self {
        Self {
            queue: Mutex::new(HashMap::new()),
            results: Arc::new(DashMap::new()),
            flush_notify: Arc::new(Notify::new()),
            window_secs,
            max_batch_size,
            mistral_base_url,
            window_start: Mutex::new(Instant::now()),
        }
    }

    async fn enqueue(&self, entry: QueuedEntry) -> u64 {
        let mut q = self.queue.lock().await;
        q.entry(entry.model_id).or_default().push(entry);
        let total: usize = q.values().map(|v| v.len()).sum();
        if total >= self.max_batch_size {
            self.flush_notify.notify_one();
        }
        // Estimate time remaining in the current window (+ 30 s for Mistral processing).
        let elapsed = self.window_start.lock().await.elapsed().as_millis() as u64;
        let window_ms = self.window_secs * 1000;
        let remaining_window_ms = window_ms.saturating_sub(elapsed);
        remaining_window_ms + 30_000
    }
}

// ── Background flush task ─────────────────────────────────────────────────────

pub fn spawn_flush_task(queue: Arc<BatchQueue>, selector: Arc<Selector>, http: reqwest::Client) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(queue.window_secs)) => {}
                _ = queue.flush_notify.notified() => {}
            }

            // Reset window start time.
            *queue.window_start.lock().await = Instant::now();

            // Drain all pending entries grouped by model.
            let groups: HashMap<Uuid, Vec<QueuedEntry>> = {
                let mut q = queue.queue.lock().await;
                std::mem::take(&mut *q)
            };

            if groups.is_empty() {
                continue;
            }

            info!(groups = groups.len(), "batch flush triggered");

            // Spawn one task per model group so they run concurrently.
            for (model_id, entries) in groups {
                let q = queue.clone();
                let http = http.clone();
                let sel = selector.clone();
                tokio::spawn(async move {
                    flush_model_group(model_id, entries, &q, &http, sel).await;
                });
            }
        }
    });
}

async fn flush_model_group(
    model_id: Uuid,
    entries: Vec<QueuedEntry>,
    queue: &BatchQueue,
    http: &reqwest::Client,
    selector: Arc<Selector>,
) {
    if entries.is_empty() {
        return;
    }

    let first = &entries[0];
    let model_slug = first.model_slug.clone();
    let api_key_env = first.api_key_env.clone();

    // Resolve API key from environment.
    let api_key = match std::env::var(&api_key_env) {
        Ok(k) if !k.is_empty() => k,
        _ => {
            error!(env = %api_key_env, "batch flush: API key env var not set or empty");
            reject_all(
                &entries,
                &queue.results,
                format!("API key env var '{api_key_env}' not set"),
            );
            return;
        }
    };

    // Build the Mistral batch payload.
    let requests: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut body = e.extra_body.clone();
            if let Value::Object(ref mut map) = body {
                map.insert("messages".to_string(), e.messages.clone());
            } else {
                body = serde_json::json!({ "messages": e.messages });
            }
            serde_json::json!({
                "custom_id": e.request_id.to_string(),
                "body": body,
            })
        })
        .collect();

    let payload = serde_json::json!({
        "model": model_slug,
        "endpoint": "/v1/chat/completions",
        "requests": requests,
    });

    // Submit the batch job to Mistral.
    let submit_url = format!("{}/v1/batch/jobs", queue.mistral_base_url);
    debug!(model = %model_slug, count = entries.len(), "submitting batch job to Mistral");

    let batch_job_id = match submit_batch(http, &submit_url, &api_key, &payload).await {
        Ok(id) => id,
        Err(e) => {
            error!(error = %e, "batch submit failed");
            reject_all(
                &entries,
                &queue.results,
                format!("Mistral batch submit failed: {e}"),
            );
            return;
        }
    };

    info!(job_id = %batch_job_id, model = %model_slug, count = entries.len(), "batch job submitted");

    // Poll until terminal status.
    let poll_url = format!("{}/v1/batch/jobs/{}", queue.mistral_base_url, batch_job_id);
    let output_file_id = match poll_until_complete(http, &poll_url, &api_key, 3600).await {
        Ok(file_id) => file_id,
        Err(e) => {
            error!(job_id = %batch_job_id, error = %e, "batch poll failed");
            reject_all(
                &entries,
                &queue.results,
                format!("Mistral batch job failed: {e}"),
            );
            return;
        }
    };

    info!(job_id = %batch_job_id, "batch job complete, downloading results");

    // Download and parse JSONL results.
    let file_url = format!(
        "{}/v1/files/{}/content",
        queue.mistral_base_url, output_file_id
    );
    let jsonl = match download_file(http, &file_url, &api_key).await {
        Ok(s) => s,
        Err(e) => {
            error!(job_id = %batch_job_id, error = %e, "batch result download failed");
            reject_all(
                &entries,
                &queue.results,
                format!("Mistral result download failed: {e}"),
            );
            return;
        }
    };

    // Index results by custom_id (= request_id string).
    let result_map: HashMap<String, Value> = jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|v| {
            let id = v["custom_id"].as_str()?.to_string();
            Some((id, v))
        })
        .collect();

    // Compute total successful tokens for aggregate reporting.
    let mut total_prompt: u64 = 0;
    let mut total_completion: u64 = 0;
    let mut total_estimated: u64 = 0;

    for entry in &entries {
        total_estimated += entry.estimated_tokens;
        let id_str = entry.request_id.to_string();
        match result_map.get(&id_str) {
            Some(result) if result["error"].is_null() => {
                let body = result["response"]["body"].clone();
                let usage = &body["usage"];
                let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
                let completion = usage["completion_tokens"].as_u64().unwrap_or(0);
                total_prompt += prompt;
                total_completion += completion;

                let actual_cost_usd = compute_batch_cost(
                    entry.price_input_per_1m,
                    entry.price_output_per_1m,
                    entry.batch_price_multiplier,
                    prompt,
                    completion,
                );

                queue.results.insert(
                    entry.request_id,
                    BatchResultState::Success {
                        body,
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        actual_cost_usd,
                    },
                );
            }
            Some(result) => {
                let msg = result["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_string();
                warn!(request_id = %entry.request_id, error = %msg, "batch item failed");
                queue
                    .results
                    .insert(entry.request_id, BatchResultState::Error { message: msg });
            }
            None => {
                warn!(request_id = %entry.request_id, "batch result missing for request");
                queue.results.insert(
                    entry.request_id,
                    BatchResultState::Error {
                        message: "result missing from batch output".to_string(),
                    },
                );
            }
        }
    }

    // Report aggregate token usage back to the selector for window tracking.
    if total_prompt > 0 || total_completion > 0 {
        let brand_key_id = first.brand_key_id;
        tokio::task::spawn_blocking(move || {
            selector.report_success(
                model_id,
                brand_key_id,
                total_estimated,
                None,
                Some(total_prompt),
                Some(total_completion),
                None,
                None,
            );
        });
    }
}

// ── Mistral HTTP helpers ──────────────────────────────────────────────────────

async fn submit_batch(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    payload: &Value,
) -> Result<String, String> {
    let resp = http
        .post(url)
        .bearer_auth(api_key)
        .json(payload)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;

    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}"));
    }

    body["id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing 'id' in batch submit response: {body}"))
}

async fn poll_until_complete(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut interval_secs = 5u64;

    loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        interval_secs = (interval_secs * 2).min(60);

        if Instant::now() > deadline {
            return Err(format!("polling timed out after {timeout_secs}s"));
        }

        let resp = match http.get(url).bearer_auth(api_key).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "network error polling batch job — retrying");
                continue;
            }
        };

        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "failed to parse poll response — retrying");
                continue;
            }
        };

        if !status.is_success() {
            return Err(format!("HTTP {status} polling job: {body}"));
        }

        let job_status = body["status"].as_str().unwrap_or("UNKNOWN");
        debug!(status = %job_status, "batch job poll");

        match job_status {
            "SUCCESS" => {
                let file_id = body["output_file"]
                    .as_str()
                    .ok_or_else(|| "missing output_file in SUCCESS response".to_string())?
                    .to_string();
                return Ok(file_id);
            }
            "FAILED" | "TIMEOUT_EXCEEDED" | "CANCELLED" => {
                return Err(format!("batch job ended with status: {job_status}"));
            }
            // QUEUED, RUNNING, CANCELLATION_REQUESTED — keep polling
            _ => {}
        }
    }
}

async fn download_file(http: &reqwest::Client, url: &str, api_key: &str) -> Result<String, String> {
    let resp = http
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} downloading results file"));
    }

    resp.text().await.map_err(|e| e.to_string())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn reject_all(entries: &[QueuedEntry], results: &DashMap<Uuid, BatchResultState>, message: String) {
    for entry in entries {
        results.insert(
            entry.request_id,
            BatchResultState::Error {
                message: message.clone(),
            },
        );
    }
}

fn compute_batch_cost(
    price_in: Option<f64>,
    price_out: Option<f64>,
    multiplier: Option<f64>,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Option<f64> {
    let in_cost = price_in.map(|p| p * prompt_tokens as f64 / 1_000_000.0)?;
    let out_cost = price_out
        .map(|p| p * completion_tokens as f64 / 1_000_000.0)
        .unwrap_or(0.0);
    let raw = in_cost + out_cost;
    Some(raw * multiplier.unwrap_or(1.0))
}

// ── Handler helpers (called from main.rs) ─────────────────────────────────────

pub async fn handle_batch_submit(
    queue: Arc<BatchQueue>,
    selector: Arc<Selector>,
    req: BatchSubmitRequest,
) -> Result<BatchSubmitResponse, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;

    // Default to text/code categories if the caller didn't restrict.
    let categories = if req.categories.is_empty() {
        vec!["text".to_string(), "code".to_string()]
    } else {
        req.categories
    };

    let select_req = SelectRequest {
        step: req.step,
        estimated_tokens: req.estimated_tokens,
        requires_fn_call: req.requires_fn_call,
        requires_json_mode: req.requires_json_mode,
        quality_min: req.quality_min,
        exclude_ids: req.exclude_ids,
        categories,
        group_id: req.group_id,
        group_name: req.group_name,
        use_member_priority: true,
        max_wait_ms: None,
    };

    let sel = selector.clone();
    let candidate = tokio::task::spawn_blocking(move || sel.select(&select_req))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;

    // Validate brand — batch is Mistral-only.
    if !candidate.brand_slug.starts_with("mistral") {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "batch only supported for Mistral brands, got: {}",
                candidate.brand_slug
            ),
        ));
    }

    // Release the in-flight reservation — batch is async and does not hold an in-flight slot.
    // Parse error has TTL=0: model is not blocked, just the slot is freed.
    {
        let sel2 = selector.clone();
        let model_id = candidate.model_id;
        let brand_key_id = candidate.brand_key_id;
        let estimated = req.estimated_tokens as u64;
        tokio::task::spawn_blocking(move || {
            sel2.report_error(
                model_id,
                brand_key_id,
                RateLimitErrorType::Parse,
                estimated,
                None,
                None,
                None,
            );
        });
    }

    let request_id = Uuid::new_v4();
    let entry = QueuedEntry {
        request_id,
        model_id: candidate.model_id,
        model_slug: candidate.model_slug.clone(),
        api_key_env: candidate.api_key_env.clone().unwrap_or_default(),
        brand_key_id: candidate.brand_key_id,
        estimated_tokens: candidate.estimated_tokens,
        price_input_per_1m: candidate.price_input_per_1m,
        price_output_per_1m: candidate.price_output_per_1m,
        batch_price_multiplier: candidate.batch_price_multiplier,
        messages: req.messages,
        extra_body: req.extra_body,
    };

    let retry_after_ms = queue.enqueue(entry).await;

    Ok(BatchSubmitResponse {
        request_id,
        retry_after_ms,
    })
}

pub fn handle_batch_result(queue: &BatchQueue, request_id: Uuid) -> BatchResultResponse {
    match queue.results.get(&request_id) {
        None => BatchResultResponse::Pending {
            retry_after_ms: 15_000,
        },
        Some(state) => match state.value() {
            BatchResultState::Success {
                body,
                prompt_tokens,
                completion_tokens,
                actual_cost_usd,
            } => BatchResultResponse::Success {
                body: body.clone(),
                prompt_tokens: *prompt_tokens,
                completion_tokens: *completion_tokens,
                actual_cost_usd: *actual_cost_usd,
            },
            BatchResultState::Error { message } => BatchResultResponse::Error {
                message: message.clone(),
            },
        },
    }
}

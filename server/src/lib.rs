use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use proviz_elekto_core::{
    error::ProvizError,
    models::{
        ModelCandidate, RateLimitErrorType, ReportOutcome, ReportRequest, ReportResponse,
        SelectRequest,
    },
    selector::Selector,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::debug;
use uuid::Uuid;

pub mod batch;
pub mod complete;

pub struct AppState {
    pub selector: Arc<Selector>,
    pub batch_queue: Arc<batch::BatchQueue>,
    pub started_at: Instant,
    pub providers_dir: String,
    /// Shared HTTP client reused for both the batch flush task and the synchronous `/complete`
    /// path. Per-request timeouts are applied via `RequestBuilder::timeout`, so one client is fine.
    pub http: reqwest::Client,
}

/// Build the axum router for the server. Exposed so integration tests can mount the same routes
/// against an in-memory selector without spawning the binary.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/select", post(handle_select))
        .route("/report", post(handle_report))
        .route("/complete", post(handle_complete))
        .route("/health", get(handle_health))
        .route("/catalog/reload", post(handle_reload))
        .route("/catalog/seed", post(handle_catalog_seed))
        .route("/catalog/refresh", post(handle_catalog_refresh))
        .route("/catalog/models", get(handle_catalog_models))
        .route("/stt/model-info", get(handle_stt_model_info))
        .route("/batch/submit", post(handle_batch_submit))
        .route("/batch/result/{request_id}", get(handle_batch_result))
        .with_state(state)
        .layer(tower_http::cors::CorsLayer::permissive())
}

/// Run a selection with the same `max_wait_ms` sleep-and-retry-once behaviour the `/select`
/// handler exposes. Shared by `handle_select` and the `/complete` path so both honour the
/// transient-exhaustion wait budget identically.
pub(crate) async fn select_with_wait(
    state: &Arc<AppState>,
    req: SelectRequest,
) -> Result<ModelCandidate, ProvizError> {
    let max_wait_ms = req.max_wait_ms;

    let result = {
        let sel = state.selector.clone();
        let req2 = req.clone();
        tokio::task::spawn_blocking(move || sel.select(&req2))
            .await
            .expect("select task panicked")
    };

    match result {
        Err(ProvizError::AllModelsExhausted {
            ref step,
            tried,
            retry_after_ms,
        }) if max_wait_ms.is_some_and(|max| retry_after_ms > 0 && retry_after_ms <= max) => {
            debug!(
                step = %step,
                tried,
                retry_after_ms,
                "all models exhausted — sleeping before retry"
            );
            tokio::time::sleep(Duration::from_millis(retry_after_ms)).await;
            let sel = state.selector.clone();
            tokio::task::spawn_blocking(move || sel.select(&req))
                .await
                .expect("select retry panicked")
        }
        other => other,
    }
}

async fn handle_select(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<SelectRequest>,
) -> impl IntoResponse {
    debug!(
        peer = %peer,
        step = %req.step,
        estimated_tokens = req.estimated_tokens,
        group_name = ?req.group_name,
        group_id = ?req.group_id,
        requires_fn_call = req.requires_fn_call,
        requires_json_mode = req.requires_json_mode,
        quality_min = req.quality_min,
        "select request"
    );

    match select_with_wait(&state, req).await {
        Ok(candidate) => {
            debug!(
                peer = %peer,
                model = %candidate.model_slug,
                brand = %candidate.brand_slug,
                estimated_tokens = candidate.estimated_tokens,
                cost_usd = ?candidate.estimated_input_cost_usd,
                "select response"
            );
            (StatusCode::OK, Json(json!(candidate))).into_response()
        }
        Err(ProvizError::AllModelsExhausted {
            step,
            tried,
            retry_after_ms,
        }) => {
            debug!(peer = %peer, step = %step, tried, retry_after_ms, "select exhausted");
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "all_models_exhausted",
                    "step": step,
                    "tried": tried,
                    "retry_after_ms": retry_after_ms
                })),
            )
                .into_response()
        }
        Err(ProvizError::GroupNotFound(name)) => {
            debug!(peer = %peer, group = %name, "select group not found");
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "group_not_found", "group": name })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_report(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReportRequest>,
) -> impl IntoResponse {
    debug!(
        peer = %peer,
        model_id = %req.model_id,
        outcome = ?req.outcome,
        error_type = ?req.error_type,
        actual_tokens = ?req.actual_tokens,
        remaining_requests = ?req.remaining_requests,
        remaining_tokens = ?req.remaining_tokens,
        "report"
    );
    let sel = state.selector.clone();
    let actual_cost_usd = tokio::task::spawn_blocking(move || apply_report(&sel, req))
        .await
        .expect("report task panicked");
    (
        StatusCode::OK,
        Json(ReportResponse {
            status: "ok",
            actual_cost_usd,
        }),
    )
        .into_response()
}

/// Apply a `ReportRequest` to the selector (blocking). Shared by `/report` and the `/complete`
/// path so accounting/rate-limit state is updated identically regardless of entry point.
pub(crate) fn apply_report(sel: &Selector, req: ReportRequest) -> Option<f64> {
    let estimated = req.estimated_tokens.unwrap_or(0);
    let actual = req.actual_tokens;
    let prompt = req.prompt_tokens;
    let completion = req.completion_tokens;
    let rem_req = req.remaining_requests;
    let rem_tok = req.remaining_tokens;
    let brand_key_id = req.brand_key_id;
    let cost = match req.outcome {
        ReportOutcome::Success => sel.report_success(
            req.model_id,
            brand_key_id,
            estimated,
            actual,
            prompt,
            completion,
            rem_req,
            rem_tok,
        ),
        ReportOutcome::RateLimit => {
            let et = req.error_type.unwrap_or(RateLimitErrorType::Other);
            sel.report_rate_limit(
                req.model_id,
                brand_key_id,
                et,
                estimated,
                actual,
                rem_req,
                rem_tok,
            );
            None
        }
        ReportOutcome::Error => {
            let et = req.error_type.unwrap_or(RateLimitErrorType::Other);
            sel.report_error(
                req.model_id,
                brand_key_id,
                et,
                estimated,
                actual,
                rem_req,
                rem_tok,
            );
            None
        }
    };
    if req.sync_limits {
        sel.sync_provider_limits(
            req.model_id,
            brand_key_id,
            req.limit_requests,
            req.limit_tokens,
        );
    }
    cost
}

async fn handle_complete(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<complete::CompleteRequest>,
) -> impl IntoResponse {
    debug!(
        peer = %peer,
        step = %req.step,
        messages = req.messages.len(),
        "complete request"
    );
    complete::run_complete(state, req).await
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
}

async fn handle_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        uptime_secs: state.started_at.elapsed().as_secs(),
    })
}

async fn handle_reload(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let sel = state.selector.clone();
    let result = tokio::task::spawn_blocking(move || sel.reload())
        .await
        .expect("reload task panicked");
    match result {
        Ok((models, rules)) => (
            StatusCode::OK,
            Json(json!({ "status": "ok", "models_loaded": models, "rules_loaded": rules })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_catalog_seed(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let sel = state.selector.clone();
    let dir = state.providers_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let storage = sel.storage();
        let summary =
            proviz_elekto_core::builtin_providers::load_from_dir(storage.as_ref(), &dir, false)
                .map_err(|e| e.to_string())?;
        sel.reload().map_err(|e| e.to_string())?;
        Ok::<_, String>(summary)
    })
    .await
    .expect("seed task panicked");
    match result {
        Ok(s) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "brands_added": s.brands_added,
                "models_added": s.models_added,
                "models_updated": s.models_updated,
                "models_skipped": s.models_skipped,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// Upserts capability fields (incl. STT flags) for all providers from JSON files, then reloads
/// the in-memory cache. Unlike /catalog/seed this always runs even if brands already exist,
/// and unlike /catalog/reload it writes to the DB first so new JSON fields reach existing rows.
async fn handle_catalog_refresh(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let sel = state.selector.clone();
    let dir = state.providers_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let storage = sel.storage();
        let summary =
            proviz_elekto_core::builtin_providers::load_from_dir(storage.as_ref(), &dir, false)
                .map_err(|e| e.to_string())?;
        sel.reload().map_err(|e| e.to_string())?;
        Ok::<_, String>(summary)
    })
    .await
    .expect("refresh task panicked");
    match result {
        Ok(s) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "brands_added": s.brands_added,
                "models_added": s.models_added,
                "models_updated": s.models_updated,
                "models_skipped": s.models_skipped,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct CatalogModelsQuery {
    category: Option<String>,
}

#[derive(Serialize)]
struct CatalogModelEntry {
    brand_slug: String,
    brand_name: String,
    model_slug: String,
    display_name: String,
    category: Option<String>,
    price_input_per_1m: Option<f64>,
    price_output_per_1m: Option<f64>,
    is_enabled: bool,
}

async fn handle_catalog_models(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CatalogModelsQuery>,
) -> impl IntoResponse {
    let sel = state.selector.clone();
    let result = tokio::task::spawn_blocking(move || {
        let storage = sel.storage();
        let brands = storage.load_brands().map_err(|e| e.to_string())?;
        let models = storage.load_models().map_err(|e| e.to_string())?;
        let brand_map: std::collections::HashMap<_, _> =
            brands.into_iter().map(|b| (b.id, b)).collect();
        let entries: Vec<CatalogModelEntry> = models
            .into_iter()
            .filter(|m| {
                if let Some(ref cat) = params.category {
                    m.category.as_deref() == Some(cat.as_str())
                } else {
                    true
                }
            })
            .filter_map(|m| {
                let brand = brand_map.get(&m.brand_id)?;
                Some(CatalogModelEntry {
                    brand_slug: brand.slug.clone(),
                    brand_name: brand.name.clone(),
                    model_slug: m.slug.clone(),
                    display_name: m.display_name.clone(),
                    category: m.category.clone(),
                    price_input_per_1m: m.price_input_per_1m,
                    price_output_per_1m: m.price_output_per_1m,
                    is_enabled: m.is_enabled,
                })
            })
            .collect();
        Ok::<_, String>(entries)
    })
    .await
    .expect("catalog_models task panicked");

    match result {
        Ok(entries) => (StatusCode::OK, Json(entries)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct SttModelInfoQuery {
    provider: String,
}

#[derive(Serialize)]
struct SttModelInfoResponse {
    brand_slug: String,
    model_slug: String,
    base_url: String,
    stt_path: String,
    api_key_env: Option<String>,
    /// False for ElevenLabs (own request format + auth header); true for all OAI-compatible brands.
    openai_compatible: bool,
    diarization: bool,
    streaming: bool,
    http_batch: bool,
    word_timestamps: bool,
}

async fn handle_stt_model_info(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SttModelInfoQuery>,
) -> impl IntoResponse {
    let provider = params.provider;
    let Some((brand_slug, model_slug)) = provider.split_once('/') else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "provider must be brand/model" })),
        )
            .into_response();
    };
    let brand_slug = brand_slug.to_string();
    let model_slug = model_slug.to_string();

    let sel = state.selector.clone();
    let result = tokio::task::spawn_blocking(move || {
        let storage = sel.storage();
        let brands = storage.load_brands().map_err(|e| e.to_string())?;
        let models = storage.load_models().map_err(|e| e.to_string())?;
        let keys = storage
            .load_all_brand_api_keys()
            .map_err(|e| e.to_string())?;

        let brand = brands
            .iter()
            .find(|b| b.slug == brand_slug)
            .ok_or_else(|| format!("brand '{}' not found", brand_slug))?;
        let model = models
            .iter()
            .find(|m| m.brand_id == brand.id && m.slug == model_slug)
            .ok_or_else(|| format!("model '{}/{}' not found", brand_slug, model_slug))?;

        let stt_path = brand
            .endpoints
            .as_ref()
            .and_then(|e| e.get("stt"))
            .and_then(|s| s.as_str())
            .unwrap_or("/audio/transcriptions")
            .to_string();

        let openai_compatible = stt_path != "/v1/speech-to-text";

        let base_url = brand
            .base_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .map(|u| u.trim_end_matches('/').to_string())
            .or_else(|| {
                let prefix = brand_slug.split('-').next().unwrap_or(&brand_slug);
                match prefix {
                    "scaleway" => Some("https://api.scaleway.ai/v1".to_string()),
                    "mistral" => Some("https://api.mistral.ai/v1".to_string()),
                    "elevenlabs" => Some("https://api.elevenlabs.io".to_string()),
                    _ => None,
                }
            })
            .ok_or_else(|| format!("no base_url configured for brand '{}'", brand_slug))?;

        let api_key_env = keys
            .into_iter()
            .find(|k| k.brand_id == brand.id)
            .map(|k| k.api_key_env);

        Ok::<_, String>(SttModelInfoResponse {
            brand_slug: brand.slug.clone(),
            model_slug: model.slug.clone(),
            base_url,
            stt_path,
            api_key_env,
            openai_compatible,
            diarization: model.diarization.unwrap_or(false),
            streaming: model.streaming.unwrap_or(false),
            http_batch: model.http_batch.unwrap_or(false),
            word_timestamps: model.word_timestamps.unwrap_or(false),
        })
    })
    .await
    .expect("stt_model_info task panicked");

    match result {
        Ok(info) => (StatusCode::OK, Json(info)).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response(),
    }
}

async fn handle_batch_submit(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<batch::BatchSubmitRequest>,
) -> impl IntoResponse {
    debug!(peer = %peer, step = %req.step, "batch/submit");
    match batch::handle_batch_submit(state.batch_queue.clone(), state.selector.clone(), req).await {
        Ok(resp) => (StatusCode::OK, Json(json!(resp))).into_response(),
        Err((code, msg)) => (code, Json(json!({ "error": msg }))).into_response(),
    }
}

async fn handle_batch_result(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Path(request_id): Path<Uuid>,
) -> impl IntoResponse {
    debug!(peer = %peer, %request_id, "batch/result");
    let resp = batch::handle_batch_result(&state.batch_queue, request_id);
    (StatusCode::OK, Json(json!(resp))).into_response()
}

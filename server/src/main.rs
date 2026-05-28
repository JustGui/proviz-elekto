use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use proviz_elekto_core::{
    models::{RateLimitErrorType, ReportOutcome, ReportRequest, ReportResponse, SelectRequest},
    selector::Selector,
    storage::CatalogStorage,
};
use proviz_elekto_storage_pg::PostgresStorage;
use proviz_elekto_storage_sqlite::SqliteStorage;
use serde::Serialize;
use serde_json::json;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

#[derive(Parser)]
#[command(name = "proviz-server", about = "ProvizElekto LLM model router")]
struct Args {
    #[arg(long, env = "PROVIZ_STORAGE", default_value = "sqlite")]
    storage: String,

    #[arg(long, env = "PROVIZ_DATABASE_URL")]
    database_url: Option<String>,

    #[arg(long, env = "PROVIZ_DB_PATH", default_value = "./proviz.db")]
    db_path: String,

    #[arg(long, env = "PROVIZ_PORT", default_value = "0")]
    port: u16,
}

struct AppState {
    selector: Selector,
    started_at: Instant,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let log_filter = match std::env::var("LOG_LEVEL")
        .unwrap_or_default()
        .to_uppercase()
        .as_str()
    {
        "DEBUG" | "TRACE" => "proviz_server=debug,proviz_elekto_core=debug".to_string(),
        _ => tracing_subscriber::EnvFilter::try_from_default_env()
            .map(|f| f.to_string())
            .unwrap_or_else(|_| "proviz_server=info,proviz_elekto_core=debug".to_string()),
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(log_filter)
        .init();

    let args = Args::parse();

    let storage: Arc<dyn CatalogStorage> = match args.storage.as_str() {
        "postgres" | "postgresql" => {
            let url = args
                .database_url
                .expect("PROVIZ_DATABASE_URL required for postgres storage");
            info!("using PostgreSQL storage");
            // postgres::Client uses block_on internally; must connect outside the async context.
            let pg = tokio::task::spawn_blocking(move || {
                PostgresStorage::connect(&url).expect("failed to connect to PostgreSQL")
            })
            .await
            .expect("postgres connect task panicked");
            Arc::new(pg) as Arc<dyn CatalogStorage>
        }
        _ => {
            info!(path = %args.db_path, "using SQLite storage");
            Arc::new(SqliteStorage::open(&args.db_path).expect("failed to open SQLite"))
                as Arc<dyn CatalogStorage>
        }
    };

    // Initial catalog load — run in a blocking thread so postgres storage can call block_on.
    let selector = tokio::task::spawn_blocking(move || {
        let sel = Selector::new(storage);
        match sel.reload() {
            Ok((models, rules)) => info!(models, rules, "catalog loaded"),
            Err(e) => error!("catalog load failed: {e}"),
        }
        sel
    })
    .await
    .expect("catalog load task panicked");

    let state = Arc::new(AppState {
        selector,
        started_at: Instant::now(),
    });

    let app = Router::new()
        .route("/select", post(handle_select))
        .route("/report", post(handle_report))
        .route("/health", get(handle_health))
        .route("/catalog/reload", post(handle_reload))
        .with_state(state)
        .layer(tower_http::cors::CorsLayer::permissive());

    let addr = format!("0.0.0.0:{}", args.port);
    let listener = TcpListener::bind(&addr).await.unwrap();
    let actual_port = listener.local_addr().unwrap().port();
    println!("PROVIZ_PORT={actual_port}");
    std::io::stdout().flush().ok();
    info!(port = actual_port, "listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
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

    let max_wait_ms = req.max_wait_ms;

    // First attempt.
    let result = {
        let state2 = state.clone();
        let req2 = req.clone();
        tokio::task::spawn_blocking(move || state2.selector.select(&req2))
            .await
            .expect("select task panicked")
    };

    // If all models are exhausted and the caller supplied a wait budget that covers the hint,
    // sleep until the soonest model's window drains, then retry once.
    let result = match result {
        Err(proviz_elekto_core::error::ProvizError::AllModelsExhausted {
            ref step,
            tried,
            retry_after_ms,
        }) if max_wait_ms.map_or(false, |max| retry_after_ms > 0 && retry_after_ms <= max) => {
            debug!(
                peer = %peer,
                step = %step,
                tried,
                retry_after_ms,
                "all models exhausted — sleeping before retry"
            );
            tokio::time::sleep(Duration::from_millis(retry_after_ms)).await;
            tokio::task::spawn_blocking(move || state.selector.select(&req))
                .await
                .expect("select retry panicked")
        }
        other => other,
    };

    match result {
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
        Err(proviz_elekto_core::error::ProvizError::AllModelsExhausted {
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
        Err(proviz_elekto_core::error::ProvizError::GroupNotFound(name)) => {
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
    let actual_cost_usd = tokio::task::spawn_blocking(move || {
        let estimated = req.estimated_tokens.unwrap_or(0);
        let actual = req.actual_tokens;
        let prompt = req.prompt_tokens;
        let completion = req.completion_tokens;
        let rem_req = req.remaining_requests;
        let rem_tok = req.remaining_tokens;
        let brand_key_id = req.brand_key_id;
        let cost = match req.outcome {
            ReportOutcome::Success => state.selector.report_success(
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
                state.selector.report_rate_limit(
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
                state.selector.report_error(
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
            state
                .selector
                .sync_provider_limits(req.model_id, req.limit_requests, req.limit_tokens);
        }
        cost
    })
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
    let result = tokio::task::spawn_blocking(move || state.selector.reload())
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

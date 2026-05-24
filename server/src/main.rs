use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use proviz_elekto_core::{
    models::{RateLimitErrorType, ReportOutcome, ReportRequest, SelectRequest},
    selector::Selector,
    storage::CatalogStorage,
};
use proviz_elekto_storage_pg::PostgresStorage;
use proviz_elekto_storage_sqlite::SqliteStorage;
use serde::Serialize;
use serde_json::json;
use tokio::net::TcpListener;
use tracing::{error, info};

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
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proviz_server=info,proviz_elekto_core=debug".into()),
        )
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
    axum::serve(listener, app).await.unwrap();
}

async fn handle_select(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SelectRequest>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || state.selector.select(&req))
        .await
        .expect("select task panicked");
    match result {
        Ok(candidate) => (StatusCode::OK, Json(json!(candidate))).into_response(),
        Err(proviz_elekto_core::error::ProvizError::AllModelsExhausted { step, tried }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "all_models_exhausted",
                "step": step,
                "tried": tried
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_report(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReportRequest>,
) -> impl IntoResponse {
    tokio::task::spawn_blocking(move || {
        let estimated = req.estimated_tokens.unwrap_or(0);
        let actual = req.actual_tokens;
        match req.outcome {
            ReportOutcome::Success => {
                state
                    .selector
                    .report_success(req.model_id, estimated, actual);
            }
            ReportOutcome::RateLimit => {
                let et = req.error_type.unwrap_or(RateLimitErrorType::Other);
                state
                    .selector
                    .report_rate_limit(req.model_id, et, estimated, actual);
            }
            ReportOutcome::Error => {
                let et = req.error_type.unwrap_or(RateLimitErrorType::Other);
                state
                    .selector
                    .report_error(req.model_id, et, estimated, actual);
            }
        }
    })
    .await
    .expect("report task panicked");
    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
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

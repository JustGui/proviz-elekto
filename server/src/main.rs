use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use proviz_elekto_core::{selector::Selector, storage::CatalogStorage};
use proviz_elekto_storage_pg::PostgresStorage;
use proviz_elekto_storage_sqlite::SqliteStorage;
use tokio::net::TcpListener;
use tracing::{error, info};

use proviz_server::{batch, build_router, AppState};

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

    /// Seconds to accumulate batch requests before flushing to Mistral's Batch API.
    #[arg(long, env = "PROVIZ_BATCH_WINDOW_SECS", default_value = "60")]
    batch_window_secs: u64,

    /// Maximum batch size before an early flush is triggered.
    #[arg(long, env = "PROVIZ_BATCH_MAX_SIZE", default_value = "100")]
    batch_max_size: usize,

    /// Mistral API base URL for batch operations.
    #[arg(
        long,
        env = "PROVIZ_BATCH_MISTRAL_BASE_URL",
        default_value = "https://api.mistral.ai"
    )]
    batch_mistral_base_url: String,

    /// Directory containing provider subdirectories (brand.json + models.json).
    /// Used for auto-seeding on first start and for POST /catalog/seed.
    #[arg(long, env = "PROVIZ_PROVIDERS_DIR", default_value = "./providers")]
    providers_dir: String,
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

    let providers_dir = args.providers_dir.clone();
    let storage: Arc<dyn CatalogStorage> = match args.storage.as_str() {
        "postgres" | "postgresql" => {
            let url = args
                .database_url
                .expect("PROVIZ_DATABASE_URL required for postgres storage");
            info!("using PostgreSQL storage");
            let pdir = providers_dir.clone();
            let pg = tokio::task::spawn_blocking(move || {
                PostgresStorage::connect_with_providers(&url, &pdir)
                    .expect("failed to connect to PostgreSQL")
            })
            .await
            .expect("postgres connect task panicked");
            Arc::new(pg) as Arc<dyn CatalogStorage>
        }
        _ => {
            info!(path = %args.db_path, "using SQLite storage");
            Arc::new(
                SqliteStorage::open_with_providers(&args.db_path, &providers_dir)
                    .expect("failed to open SQLite"),
            ) as Arc<dyn CatalogStorage>
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

    let selector = Arc::new(selector);

    let http = reqwest::Client::new();

    let batch_queue = Arc::new(batch::BatchQueue::new(
        args.batch_window_secs,
        args.batch_max_size,
        args.batch_mistral_base_url.clone(),
    ));

    batch::spawn_flush_task(batch_queue.clone(), selector.clone(), http.clone());

    info!(
        window_secs = args.batch_window_secs,
        max_size = args.batch_max_size,
        "batch queue started"
    );

    let state = Arc::new(AppState {
        selector,
        batch_queue,
        started_at: Instant::now(),
        providers_dir,
        http,
    });

    let app = build_router(state);

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

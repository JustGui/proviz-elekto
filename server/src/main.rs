use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use proviz_elekto_core::{selector::Selector, storage::CatalogStorage};
use proviz_elekto_storage_pg::PostgresStorage;
use proviz_elekto_storage_sqlite::SqliteStorage;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

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

    /// Seconds between automatic OpenRouter catalog syncs (fetch + upsert + reload). Runs once
    /// immediately at startup, then on this interval. No-op if providers/openrouter/brand.json
    /// isn't present.
    #[arg(long, env = "PROVIZ_OPENROUTER_SYNC_SECS", default_value = "3600")]
    openrouter_sync_secs: u64,

    /// Seconds between automatic Requesty catalog syncs. Same behavior as
    /// PROVIZ_OPENROUTER_SYNC_SECS, for the requesty provider.
    #[arg(long, env = "PROVIZ_REQUESTY_SYNC_SECS", default_value = "3600")]
    requesty_sync_secs: u64,

    /// Seconds between automatic Nous Portal catalog syncs. Same behavior as
    /// PROVIZ_OPENROUTER_SYNC_SECS, for the nousportal provider. No-op if
    /// providers/nousportal/brand.json is missing.
    #[arg(long, env = "PROVIZ_NOUSPORTAL_SYNC_SECS", default_value = "3600")]
    nousportal_sync_secs: u64,

    /// Seconds between automatic OrcaRouter catalog syncs. Same behavior as
    /// PROVIZ_OPENROUTER_SYNC_SECS, for the orcarouter provider. No-op if
    /// providers/orcarouter/brand.json is missing.
    #[arg(long, env = "PROVIZ_ORCAROUTER_SYNC_SECS", default_value = "3600")]
    orcarouter_sync_secs: u64,

    /// Seconds between automatic TokenHub catalog syncs. Same behavior as
    /// PROVIZ_OPENROUTER_SYNC_SECS, for the tokenhub provider. No-op if
    /// providers/tokenhub/brand.json is missing.
    #[arg(long, env = "PROVIZ_TOKENHUB_SYNC_SECS", default_value = "3600")]
    tokenhub_sync_secs: u64,

    /// Endpoint for FX rate lookups (currency -> USD, for per-brand `price_currency`
    /// normalisation). Frankfurter `base=USD` shape. Fetched once eagerly at startup, then
    /// lazily (at most hourly) whenever a selection finds the rates stale.
    #[arg(
        long,
        env = "PROVIZ_FX_BASE_URL",
        default_value = proviz_elekto_core::fx::DEFAULT_FX_URL
    )]
    fx_base_url: String,
}

/// Periodically fetches OpenRouter's live catalog and upserts it (see
/// `proviz_elekto_core::openrouter_sync`) — OpenRouter's ~400+ model catalog and pricing change
/// far more often than the other, hand-curated providers, so this is the one provider whose
/// catalog can't just be edited by hand. No-op if the provider hasn't been onboarded
/// (`providers/openrouter/brand.json` missing) — e.g. deployments that don't use OpenRouter.
fn spawn_openrouter_sync_task(selector: Arc<Selector>, providers_dir: String, interval_secs: u64) {
    let brand_file = std::path::Path::new(&providers_dir)
        .join("openrouter")
        .join("brand.json");
    if !brand_file.exists() {
        info!("openrouter not onboarded (no providers/openrouter/brand.json) — skipping auto-sync");
        return;
    }

    tokio::spawn(async move {
        loop {
            let sel = selector.clone();
            let dir = providers_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                let storage = sel.storage();
                let outcome = proviz_elekto_core::openrouter_sync::sync(
                    storage.as_ref(),
                    &dir,
                    proviz_elekto_core::openrouter_sync::DEFAULT_BASE_URL,
                );
                if let Ok(ref summary) = outcome {
                    if !summary.skipped_suspicious {
                        if let Err(e) = sel.reload() {
                            error!("openrouter sync: catalog reload failed: {e}");
                        }
                    }
                }
                outcome
            })
            .await
            .expect("openrouter sync task panicked");

            match result {
                Ok(summary) if summary.skipped_suspicious => {
                    warn!(
                        fetched = summary.fetched,
                        "openrouter sync skipped: suspiciously few models returned"
                    );
                }
                Ok(summary) => {
                    info!(
                        fetched = summary.fetched,
                        brands_added = summary.brands_added,
                        models_added = summary.models_added,
                        models_updated = summary.models_updated,
                        models_disabled = summary.models_disabled,
                        "openrouter catalog synced"
                    );
                }
                Err(e) => error!("openrouter sync failed: {e}"),
            }

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    });
}

/// Periodically fetches Requesty's live catalog and upserts it (see
/// `proviz_elekto_core::requesty_sync`) — same rationale as `spawn_openrouter_sync_task`.
/// No-op if the provider hasn't been onboarded (`providers/requesty/brand.json` missing).
fn spawn_requesty_sync_task(selector: Arc<Selector>, providers_dir: String, interval_secs: u64) {
    let brand_file = std::path::Path::new(&providers_dir)
        .join("requesty")
        .join("brand.json");
    if !brand_file.exists() {
        info!("requesty not onboarded (no providers/requesty/brand.json) — skipping auto-sync");
        return;
    }

    tokio::spawn(async move {
        loop {
            let sel = selector.clone();
            let dir = providers_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                let storage = sel.storage();
                let outcome = proviz_elekto_core::requesty_sync::sync(
                    storage.as_ref(),
                    &dir,
                    proviz_elekto_core::requesty_sync::DEFAULT_BASE_URL,
                );
                if let Ok(ref summary) = outcome {
                    if !summary.skipped_suspicious {
                        if let Err(e) = sel.reload() {
                            error!("requesty sync: catalog reload failed: {e}");
                        }
                    }
                }
                outcome
            })
            .await
            .expect("requesty sync task panicked");

            match result {
                Ok(summary) if summary.skipped_suspicious => {
                    warn!(
                        fetched = summary.fetched,
                        "requesty sync skipped: suspiciously few models returned"
                    );
                }
                Ok(summary) => {
                    info!(
                        fetched = summary.fetched,
                        brands_added = summary.brands_added,
                        models_added = summary.models_added,
                        models_updated = summary.models_updated,
                        models_disabled = summary.models_disabled,
                        "requesty catalog synced"
                    );
                }
                Err(e) => error!("requesty sync failed: {e}"),
            }

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    });
}

/// Spawns the background Nous Portal catalog sync (fetch + upsert + reload via
/// `proviz_elekto_core::nousportal_sync`) — Nous Portal's API is an OpenRouter fork (same
/// `/models` schema) aggregating 350+ models with pricing that drifts, plus a `pricing.input_cache_read`
/// per-model cache-hit rate the sync maps into `Model.price_cached_input_per_1m`. Same rationale
/// and shape as `spawn_openrouter_sync_task`. No-op if the provider hasn't been onboarded
/// (`providers/nousportal/brand.json` missing).
fn spawn_nousportal_sync_task(selector: Arc<Selector>, providers_dir: String, interval_secs: u64) {
    let brand_file = std::path::Path::new(&providers_dir)
        .join("nousportal")
        .join("brand.json");
    if !brand_file.exists() {
        info!("nousportal not onboarded (no providers/nousportal/brand.json) — skipping auto-sync");
        return;
    }

    tokio::spawn(async move {
        loop {
            let sel = selector.clone();
            let dir = providers_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                let storage = sel.storage();
                let outcome = proviz_elekto_core::nousportal_sync::sync(
                    storage.as_ref(),
                    &dir,
                    proviz_elekto_core::nousportal_sync::DEFAULT_BASE_URL,
                );
                if let Ok(ref summary) = outcome {
                    if !summary.skipped_suspicious {
                        if let Err(e) = sel.reload() {
                            error!("nousportal sync: catalog reload failed: {e}");
                        }
                    }
                }
                outcome
            })
            .await
            .expect("nousportal sync task panicked");

            match result {
                Ok(summary) if summary.skipped_suspicious => {
                    warn!(
                        fetched = summary.fetched,
                        "nousportal sync skipped: suspiciously few models returned"
                    );
                }
                Ok(summary) => {
                    info!(
                        fetched = summary.fetched,
                        brands_added = summary.brands_added,
                        models_added = summary.models_added,
                        models_updated = summary.models_updated,
                        models_disabled = summary.models_disabled,
                        "nousportal catalog synced"
                    );
                }
                Err(e) => error!("nousportal sync failed: {e}"),
            }

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    });
}

/// Spawns the background OrcaRouter catalog sync (fetch + upsert + reload via
/// `proviz_elekto_core::orcarouter_sync`). OrcaRouter's `/v1/models` is OpenRouter-shaped for the
/// basics but omits capability flags and cache pricing — those are scraped once per newly-seen
/// model from the SEO detail page (capped at `PROVIZ_ORCAROUTER_ENRICH_MAX` per run). Same
/// spawn/interval shape as `spawn_openrouter_sync_task`. No-op if the provider hasn't been
/// onboarded (`providers/orcarouter/brand.json` missing).
fn spawn_orcarouter_sync_task(selector: Arc<Selector>, providers_dir: String, interval_secs: u64) {
    let brand_file = std::path::Path::new(&providers_dir)
        .join("orcarouter")
        .join("brand.json");
    if !brand_file.exists() {
        info!("orcarouter not onboarded (no providers/orcarouter/brand.json) — skipping auto-sync");
        return;
    }

    tokio::spawn(async move {
        loop {
            let sel = selector.clone();
            let dir = providers_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                let storage = sel.storage();
                let outcome = proviz_elekto_core::orcarouter_sync::sync(
                    storage.as_ref(),
                    &dir,
                    proviz_elekto_core::orcarouter_sync::DEFAULT_BASE_URL,
                    proviz_elekto_core::orcarouter_sync::enrich_max_from_env(),
                );
                if let Ok(ref summary) = outcome {
                    if !summary.skipped_suspicious {
                        if let Err(e) = sel.reload() {
                            error!("orcarouter sync: catalog reload failed: {e}");
                        }
                    }
                }
                outcome
            })
            .await
            .expect("orcarouter sync task panicked");

            match result {
                Ok(summary) if summary.skipped_suspicious => {
                    warn!(
                        fetched = summary.fetched,
                        "orcarouter sync skipped: suspiciously few models returned"
                    );
                }
                Ok(summary) => {
                    info!(
                        fetched = summary.fetched,
                        brands_added = summary.brands_added,
                        models_added = summary.models_added,
                        models_updated = summary.models_updated,
                        models_disabled = summary.models_disabled,
                        enriched = summary.enriched,
                        "orcarouter catalog synced"
                    );
                }
                Err(e) => error!("orcarouter sync failed: {e}"),
            }

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    });
}

/// Spawns the background TokenHub catalog sync (fetch roster + scrape new models + upsert +
/// reload via `proviz_elekto_core::tokenhub_sync`). TokenHub's `GET /v1/models` (auth'd with
/// `TOKENHUB_API_KEY`) returns only model ids — pricing/context/capabilities are scraped once per
/// newly-seen model from its server-rendered detail page (capped at `PROVIZ_TOKENHUB_ENRICH_MAX`
/// per run). Known models are carried forward verbatim; a price refresh is a manual
/// `proviz providers sync-tokenhub --refresh`. Same spawn/interval shape as
/// `spawn_openrouter_sync_task`. No-op if the provider hasn't been onboarded
/// (`providers/tokenhub/brand.json` missing).
fn spawn_tokenhub_sync_task(selector: Arc<Selector>, providers_dir: String, interval_secs: u64) {
    let brand_file = std::path::Path::new(&providers_dir)
        .join("tokenhub")
        .join("brand.json");
    if !brand_file.exists() {
        info!("tokenhub not onboarded (no providers/tokenhub/brand.json) — skipping auto-sync");
        return;
    }

    tokio::spawn(async move {
        loop {
            let sel = selector.clone();
            let dir = providers_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                let storage = sel.storage();
                let outcome = proviz_elekto_core::tokenhub_sync::sync(
                    storage.as_ref(),
                    &dir,
                    proviz_elekto_core::tokenhub_sync::DEFAULT_BASE_URL,
                    proviz_elekto_core::tokenhub_sync::enrich_max_from_env(),
                    false,
                );
                if let Ok(ref summary) = outcome {
                    if !summary.skipped_suspicious {
                        if let Err(e) = sel.reload() {
                            error!("tokenhub sync: catalog reload failed: {e}");
                        }
                    }
                }
                outcome
            })
            .await
            .expect("tokenhub sync task panicked");

            match result {
                Ok(summary) if summary.skipped_suspicious => {
                    warn!(
                        fetched = summary.fetched,
                        "tokenhub sync skipped: suspiciously few models returned"
                    );
                }
                Ok(summary) => {
                    info!(
                        fetched = summary.fetched,
                        brands_added = summary.brands_added,
                        models_added = summary.models_added,
                        models_updated = summary.models_updated,
                        models_disabled = summary.models_disabled,
                        enriched = summary.enriched,
                        "tokenhub catalog synced"
                    );
                }
                Err(e) => error!("tokenhub sync failed: {e}"),
            }

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    });
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
    let fx_base_url = args.fx_base_url.clone();
    let selector = tokio::task::spawn_blocking(move || {
        let sel = Selector::new(storage);
        sel.set_fx_url(fx_base_url);
        match sel.reload() {
            Ok((models, rules)) => info!(models, rules, "catalog loaded"),
            Err(e) => error!("catalog load failed: {e}"),
        }
        sel
    })
    .await
    .expect("catalog load task panicked");

    let selector = Arc::new(selector);

    // Eager FX refresh so a fresh server is immediately current; thereafter the lazy
    // per-selection trigger keeps rates fresh (at most one fetch per hour).
    {
        let sel = selector.clone();
        tokio::task::spawn_blocking(move || sel.refresh_fx());
    }

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

    spawn_openrouter_sync_task(
        selector.clone(),
        providers_dir.clone(),
        args.openrouter_sync_secs,
    );

    spawn_requesty_sync_task(
        selector.clone(),
        providers_dir.clone(),
        args.requesty_sync_secs,
    );

    spawn_nousportal_sync_task(
        selector.clone(),
        providers_dir.clone(),
        args.nousportal_sync_secs,
    );

    spawn_orcarouter_sync_task(
        selector.clone(),
        providers_dir.clone(),
        args.orcarouter_sync_secs,
    );

    spawn_tokenhub_sync_task(
        selector.clone(),
        providers_dir.clone(),
        args.tokenhub_sync_secs,
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

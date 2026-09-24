pub mod builtin_providers;
pub mod env_expand;
pub mod error;
pub mod fx;
pub mod models;
pub mod nousportal_sync;
pub mod openrouter_sync;
pub mod orcarouter_sync;
pub mod rate_state;
pub mod requesty_sync;
pub mod selector;
pub mod storage;
pub mod tokenhub_sync;
pub mod usage_tracker;

/// User-Agent sent by every outbound HTTP client (catalog syncs AND provider calls). reqwest sends NONE by default, and
/// Nous Portal's edge answers 403 to a request without one (measured 2026-09-24: its
/// `/v1/models` returns 200 to any User-Agent, 403 to none) - the hourly Nous sync had been
/// failing silently, so no new Nous model ever entered the catalog.
pub const SYNC_USER_AGENT: &str = concat!("proviz-elekto/", env!("CARGO_PKG_VERSION"));

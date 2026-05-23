use uuid::Uuid;

use crate::{
    error::StorageError,
    models::{Brand, Model, RateLimitErrorType, SelectionRule},
};

pub type StorageResult<T> = std::result::Result<T, StorageError>;

pub trait CatalogStorage: Send + Sync {
    fn load_brands(&self) -> StorageResult<Vec<Brand>>;
    fn load_models(&self) -> StorageResult<Vec<Model>>;
    fn load_selection_rules(&self, step: &str) -> StorageResult<Vec<SelectionRule>>;
    fn load_model(&self, model_id: Uuid) -> StorageResult<Option<Model>>;
    fn load_brand(&self, brand_id: Uuid) -> StorageResult<Option<Brand>>;

    // Catalog mutations (used by CLI)
    fn insert_brand(&self, brand: &Brand) -> StorageResult<()>;
    fn insert_model(&self, model: &Model) -> StorageResult<()>;
    fn insert_rule(&self, rule: &SelectionRule) -> StorageResult<()>;
    fn delete_rule(&self, rule_id: Uuid) -> StorageResult<()>;
    fn set_model_enabled(&self, model_id: Uuid, enabled: bool) -> StorageResult<()>;
    fn set_brand_active(&self, brand_id: Uuid, active: bool) -> StorageResult<()>;

    // Rate events
    fn log_rate_event(&self, model_id: Uuid, error_type: &RateLimitErrorType) -> StorageResult<()>;
    fn recent_rate_events(
        &self,
        model_id: Uuid,
        window_secs: u64,
    ) -> StorageResult<Vec<(chrono::DateTime<chrono::Utc>, RateLimitErrorType)>>;

    // Schema init - called at startup
    fn init_schema(&self) -> StorageResult<()>;
}

use uuid::Uuid;

use crate::{
    error::StorageError,
    fx::FxRate,
    models::{
        Brand, BrandApiKey, Group, GroupMember, Model, ModelCatalogEntry, ModelStepQuality,
        RateLimitErrorType, SelectionRule,
    },
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
    /// Overwrite `rpm_limit` and/or `tpm_limit` for a model when the provider reports
    /// different values via response headers. Only non-None fields are updated.
    fn sync_model_limits(
        &self,
        model_id: Uuid,
        rpm: Option<u32>,
        tpm: Option<u32>,
    ) -> StorageResult<()>;

    // Shared model catalog (cross-provider intrinsic properties, keyed by canonical_key)
    fn load_model_catalog(&self) -> StorageResult<Vec<ModelCatalogEntry>>;
    fn insert_model_catalog_entry(&self, entry: &ModelCatalogEntry) -> StorageResult<()>;

    // Groups
    fn load_groups(&self) -> StorageResult<Vec<Group>>;
    fn load_all_group_members(&self) -> StorageResult<Vec<GroupMember>>;
    fn insert_group(&self, group: &Group) -> StorageResult<()>;
    fn delete_group(&self, group_id: Uuid) -> StorageResult<()>;
    fn set_group_active(&self, group_id: Uuid, active: bool) -> StorageResult<()>;
    fn insert_group_member(&self, member: &GroupMember) -> StorageResult<()>;
    fn remove_group_member(&self, group_id: Uuid, model_id: Uuid) -> StorageResult<()>;
    /// Overwrite a group's cost/latency/quality weight overrides outright (each `None` clears
    /// that column to NULL — unlike `sync_model_limits`, there's no "leave unchanged" sentinel
    /// here, since the only caller (the CLI) always reads the current row first and merges in
    /// just the flags the operator passed).
    fn set_group_weights(
        &self,
        group_id: Uuid,
        cost_weight: Option<f32>,
        latency_weight: Option<f32>,
        quality_weight: Option<f32>,
    ) -> StorageResult<()>;
    /// Toggle a group's prompt-cache stickiness (see `Group.sticky_model`).
    fn set_group_sticky(&self, group_id: Uuid, sticky: bool) -> StorageResult<()>;

    // Per-step measured model quality
    fn load_all_step_quality(&self) -> StorageResult<Vec<ModelStepQuality>>;
    /// Upsert one `(model_id, step)` row. Called once per model per sync run — always a plain
    /// overwrite, since this table is populated exclusively by automated sync (see
    /// `ModelStepQuality` doc comment).
    fn upsert_step_quality(
        &self,
        model_id: Uuid,
        step: &str,
        quality_score: f64,
        sample_size: i32,
    ) -> StorageResult<()>;

    // Brand API keys (multi-account rotation)
    fn insert_brand_api_key(&self, key: &BrandApiKey) -> StorageResult<()>;
    fn load_all_brand_api_keys(&self) -> StorageResult<Vec<BrandApiKey>>;
    fn delete_brand_api_key(&self, key_id: Uuid) -> StorageResult<()>;

    // FX rates (currency -> USD conversion factors, persisted so last-good values survive
    // restarts and are shared with the CLI). See `crate::fx`.
    fn load_fx_rates(&self) -> StorageResult<Vec<FxRate>>;
    /// Upsert one row per currency (`currency` is the primary key).
    fn save_fx_rates(&self, rates: &[FxRate]) -> StorageResult<()>;

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

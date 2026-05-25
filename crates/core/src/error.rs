use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProvizError {
    #[error("all eligible models exhausted for step '{step}' (tried {tried}, retry_after={retry_after_ms}ms)")]
    AllModelsExhausted {
        step: String,
        tried: usize,
        /// Hint: milliseconds to wait before the next select() call may succeed.
        /// 0 means unknown. Derived from the earliest rate-limit cooldown expiry or
        /// the oldest sliding-window entry across all skipped models.
        retry_after_ms: u64,
    },

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("brand not found: {0}")]
    BrandNotFound(String),

    #[error("group not found: {0}")]
    GroupNotFound(String),
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("serialization error: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, ProvizError>;

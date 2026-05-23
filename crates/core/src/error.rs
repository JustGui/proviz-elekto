use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProvizError {
    #[error("all eligible models exhausted for step '{step}' (tried {tried})")]
    AllModelsExhausted { step: String, tried: usize },

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("brand not found: {0}")]
    BrandNotFound(String),
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

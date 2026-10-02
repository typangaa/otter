//! Crate-wide error type.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum WeirError {
    #[error("config error: {0}")]
    Config(String),

    #[error("validation failed: {0}")]
    Validation(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, WeirError>;

//! Shared error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrowserControlError {
    #[error("browser not found: {0}")]
    NotFound(String),
    #[error("no running browser matches the selector")]
    NoRunningMatch,
    #[error("invalid BROWSER_CONTROL value: {0}")]
    InvalidSelector(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

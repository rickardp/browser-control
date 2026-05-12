//! WebDriver BiDi wire protocol types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct Command<'a> {
    pub id: u64,
    pub method: &'a str,
    pub params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum IncomingMessage {
    Success {
        id: u64,
        #[serde(default)]
        result: Value,
    },
    Error {
        id: Option<u64>,
        error: String,
        message: String,
    },
    Event {
        method: String,
        #[serde(default)]
        params: Value,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("BiDi error {code}: {message}")]
pub struct BidiError {
    pub code: String,
    pub message: String,
}

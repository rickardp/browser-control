//! CDP JSON-RPC framing types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct Request<'a> {
    pub id: u64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub params: Value,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<CdpError>,
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, thiserror::Error)]
#[error("CDP error {code}: {message}")]
pub struct CdpError {
    pub code: i64,
    pub message: String,
}

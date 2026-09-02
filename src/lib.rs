//! Library entry point for `browser-control`.

pub mod a11y;
pub mod bidi;
pub mod cdp;
pub mod cli;
pub mod config;
pub mod detect;
pub mod dom;
pub mod errors;
pub mod launch;
pub mod mcp;
pub mod paths;
pub mod registry;
pub mod session;
pub mod sidecar;
pub mod transport;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;
    /// Global lock for tests that mutate process-wide env vars
    /// (`BROWSER_CONTROL_DATA_DIR`, `BROWSER_CONTROL_CONFIG_DIR`).
    pub static ENV_LOCK: Mutex<()> = Mutex::new(());
}

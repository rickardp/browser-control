//! Library entry point for `browser-control`.

#[allow(clippy::all)]
#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[allow(unused_imports)]
#[allow(unused_qualifications)]
#[allow(unused_parens)]
pub mod errors_capnp {
    include!("generated/errors_capnp.rs");
}

#[allow(clippy::all)]
#[allow(non_camel_case_types)]
#[allow(dead_code)]
#[allow(unused_imports)]
#[allow(unused_qualifications)]
#[allow(unused_parens)]
pub mod daemon_capnp {
    include!("generated/daemon_capnp.rs");
}

pub mod bidi;
pub mod cdp;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod detect;
pub mod dom;
pub mod errors;
pub mod launch;
pub mod mcp;
pub mod paths;
pub mod registry;
pub mod session;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;
    /// Global lock for tests that mutate process-wide env vars
    /// (`BROWSER_CONTROL_DATA_DIR`, `BROWSER_CONTROL_CONFIG_DIR`).
    pub static ENV_LOCK: Mutex<()> = Mutex::new(());
}

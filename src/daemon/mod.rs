//! browser-control daemon.
//!
//! See plan for full design. This module currently scaffolds the foundation:
//! generated schemas, transport, and bringup. RPC server and client logic
//! land in subsequent phases.

pub mod bringup;
pub mod client;
pub mod probe;
pub mod rpc;
pub mod schema;
pub mod tabs;
pub mod transport;

pub use client::{connect_browser, DaemonClient, TabSummary};
pub use probe::{probe_target, ProbeResult, DEFAULT_PROBE_TIMEOUT};
pub use rpc::{connect_client, serve, DaemonImpl, DaemonState};
pub use tabs::{OpenError, TabConfig, TabHealth, TabRegistry, TabRow};
pub use transport::{connect, listen, Endpoint, Listener, Stream};

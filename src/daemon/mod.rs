//! browser-control daemon.
//!
//! See plan for full design. This module currently scaffolds the foundation:
//! generated schemas, transport, and bringup. RPC server and client logic
//! land in subsequent phases.

pub mod bringup;
pub mod schema;
pub mod transport;

pub use transport::{connect, listen, Endpoint, Listener, Stream};

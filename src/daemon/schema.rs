//! Re-exports of generated Cap'n Proto bindings.
//!
//! The generated code uses absolute `crate::*_capnp` paths, so the actual
//! `include!` lives at the crate root. This module aliases them for clarity
//! when consumed from `crate::daemon::schema::*`.

pub use crate::daemon_capnp;
pub use crate::errors_capnp;

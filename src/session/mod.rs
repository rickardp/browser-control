//! Engine-agnostic page operations shared by CLI and MCP.
//!
//! The CLI surface (`cookies`, `fetch`, `storage`, `eval`, …) and the MCP
//! tools (`navigate`, `get_dom`, `screenshot`, `fetch`, `select_element`)
//! both need the same primitives: attach to a page target, evaluate a script,
//! navigate, capture a screenshot. This module provides those once instead
//! of duplicating them per consumer.

pub mod attach;
pub mod targets;

pub use attach::PageSession;
pub use targets::{list as list_targets, TargetInfo};

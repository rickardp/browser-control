//! Engine-agnostic page operations shared by CLI and MCP.
//!
//! The CLI surface (`cookies`, `fetch`, `storage`, `eval`, …) and the MCP
//! tools (`navigate`, `get_dom`, `screenshot`, `fetch`, `select_element`)
//! both need the same primitives: attach to a page target, evaluate a script,
//! navigate, capture a screenshot. This module provides those once instead
//! of duplicating them per consumer.

pub mod attach;
pub mod backend;
pub mod cdp_session;
pub mod crash;
pub mod freshness;
pub mod input;
pub mod scratch;
pub mod tabs;
pub mod targets;

pub use attach::{evaluate_for_origin_with_recover_once, PageSession};
pub use backend::{open_backend, TabBackend};
pub use crash::evaluate_with_crash_detection;
pub use scratch::with_scratch_recovery;
pub use tabs::{resolve_tab, tab_list, tab_open, with_named_tab_recovery};
pub use targets::{list as list_targets, TargetInfo};

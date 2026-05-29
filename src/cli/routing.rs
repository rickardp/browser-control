//! Shared routing glue for the `<browser>[/<tab>]` positional commands
//! (`eval`, `fetch`, `storage`, …).
//!
//! These commands all parse the same positional shape and then peel the
//! `/<tab>` suffix back off to recover the bare `<browser>` selector for
//! [`crate::cli::mcp::resolve_browser`]. Keeping that one operation here
//! avoids the three identical copies the routing handlers used to carry.

/// Strip a `/<tab>` suffix from a raw `<browser>[/<tab>]` positional.
///
/// `tab` is the tab name already parsed out of `raw` (via
/// [`crate::cli::env_resolver::parse_target`]). When `Some`, the matching
/// `/<name>` suffix is removed; if `raw` doesn't actually end in that
/// suffix the original is returned unchanged (defensive — the caller
/// derives `tab` from the same `raw`, so a mismatch shouldn't happen).
pub fn strip_tab(raw: &str, tab: Option<&str>) -> String {
    match tab {
        Some(name) => raw
            .strip_suffix(&format!("/{name}"))
            .unwrap_or(raw)
            .to_string(),
        None => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_tab_removes_suffix_when_present() {
        assert_eq!(strip_tab("brave/cart", Some("cart")), "brave");
        assert_eq!(strip_tab("brave", None), "brave");
        // If `tab` is `Some` but does not in fact match the suffix, the
        // original raw is returned unchanged (defensive — shouldn't happen
        // in practice because the caller derives `tab` from the same raw).
        assert_eq!(strip_tab("brave/cart", Some("other")), "brave/cart");
    }
}

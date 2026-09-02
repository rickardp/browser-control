//! Per-tab element reference table.
//!
//! A ref (`e1`, `e2`, …) is a short, agent-facing handle for a DOM node
//! discovered through an accessibility snapshot or `browser_find`. Each ref
//! maps to a CDP `backendDOMNodeId`, which is what `DOM.*` and `Input.*`
//! accept directly, so a ref-based click never has to re-resolve a CSS
//! selector.
//!
//! Refs are stable for the lifetime of a *document*: taking a second
//! snapshot of the same page reuses the existing ref for a node that was
//! already interned, and only allocates new numbers for nodes that appear
//! for the first time. A navigation replaces the whole table (the document
//! token — the Document node's `backendDOMNodeId` — changes), so refs from
//! the previous page are reported as stale instead of silently hitting a
//! recycled node id.

use std::collections::HashMap;

/// One interned element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// The agent-facing handle, e.g. `e12`.
    pub r#ref: String,
    /// CDP `backendDOMNodeId` of the element.
    pub backend_node_id: u64,
    /// Accessibility role at the time of interning (for messages only).
    pub role: String,
    /// Accessible name at the time of interning (for messages only).
    pub name: String,
}

/// Ref table for one tab and one document.
#[derive(Debug, Clone)]
pub struct RefTable {
    /// Identity of the document the refs were taken from (Document node's
    /// `backendDOMNodeId`). Compared before every ref-based action.
    pub doc_token: u64,
    by_ref: HashMap<String, RefEntry>,
    by_backend: HashMap<u64, String>,
    next: u32,
}

impl RefTable {
    pub fn new(doc_token: u64) -> Self {
        Self {
            doc_token,
            by_ref: HashMap::new(),
            by_backend: HashMap::new(),
            next: 1,
        }
    }

    /// Return the ref for `backend_node_id`, allocating a fresh `eN` when
    /// the node has not been seen in this document yet. Role and name are
    /// refreshed on every call so error messages describe the latest
    /// snapshot.
    pub fn intern(&mut self, backend_node_id: u64, role: &str, name: &str) -> String {
        if let Some(r) = self.by_backend.get(&backend_node_id) {
            if let Some(entry) = self.by_ref.get_mut(r) {
                entry.role = role.to_string();
                entry.name = name.to_string();
            }
            return r.clone();
        }
        let r = format!("e{}", self.next);
        self.next += 1;
        self.by_backend.insert(backend_node_id, r.clone());
        self.by_ref.insert(
            r.clone(),
            RefEntry {
                r#ref: r.clone(),
                backend_node_id,
                role: role.to_string(),
                name: name.to_string(),
            },
        );
        r
    }

    /// Look up an interned ref.
    pub fn lookup(&self, r: &str) -> Option<&RefEntry> {
        self.by_ref.get(r)
    }

    /// Number of interned refs (tests / diagnostics).
    pub fn len(&self) -> usize {
        self.by_ref.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ref.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_is_stable_per_backend_node() {
        let mut t = RefTable::new(1);
        let a = t.intern(10, "button", "Save");
        let b = t.intern(11, "link", "Docs");
        let a2 = t.intern(10, "button", "Save changes");
        assert_eq!(a, "e1");
        assert_eq!(b, "e2");
        assert_eq!(a2, "e1");
        assert_eq!(t.lookup("e1").unwrap().name, "Save changes");
        assert_eq!(t.lookup("e1").unwrap().backend_node_id, 10);
        assert!(t.lookup("e3").is_none());
        assert_eq!(t.len(), 2);
    }
}

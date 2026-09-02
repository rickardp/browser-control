//! Accessibility-tree snapshots with element refs.
//!
//! This module is pure: it turns the JSON returned by CDP
//! `Accessibility.getFullAXTree` into a compact, Playwright-style YAML
//! listing that agents can read cheaply, and it hands out stable element
//! refs (`[ref=eN]`) that the native input path (`crate::session::input`)
//! resolves back to `backendDOMNodeId`s. Nothing here talks to a browser,
//! so every rule is unit-tested against canned tree fixtures.
//!
//! Why the AX tree rather than an injected JS walker: Chromium's
//! accessibility tree is already composed across shadow roots, carries the
//! computed accessible name/role the same way Playwright reports them, and
//! attaches a `backendDOMNodeId` per node — exactly the handle `DOM.*` and
//! `Input.*` accept. A DOM walker would have to reimplement accessible-name
//! computation and could not cross closed shadow roots.

pub mod refs;

pub use refs::{RefEntry, RefTable};

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{anyhow, Result};
use serde_json::Value;

/// One node of the accessibility tree, normalised from an `AXNode`.
#[derive(Debug, Clone)]
pub struct AxNode {
    pub id: String,
    pub backend_node_id: Option<u64>,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub description: Option<String>,
    pub ignored: bool,
    /// `properties[].name -> properties[].value.value` (booleans, numbers,
    /// strings such as `checked: "true" | "mixed"`, `level: 2`, `url`).
    pub props: BTreeMap<String, Value>,
    pub children: Vec<String>,
}

/// The whole tree, indexed by `nodeId`.
#[derive(Debug, Clone)]
pub struct AxTree {
    pub nodes: HashMap<String, AxNode>,
    pub root: String,
    parents: HashMap<String, String>,
}

/// Parse a `Accessibility.getFullAXTree` result (`{ nodes: [...] }`).
pub fn parse_full_ax_tree(v: &Value) -> Result<AxTree> {
    let arr = v
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Accessibility.getFullAXTree returned no `nodes` array"))?;
    let mut nodes = HashMap::with_capacity(arr.len());
    let mut root: Option<String> = None;
    let mut first: Option<String> = None;
    let mut parents = HashMap::new();
    for raw in arr {
        let id = raw
            .get("nodeId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("AXNode without nodeId"))?
            .to_string();
        if first.is_none() {
            first = Some(id.clone());
        }
        if raw.get("parentId").and_then(Value::as_str).is_none() && root.is_none() {
            root = Some(id.clone());
        }
        let children: Vec<String> = raw
            .get("childIds")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        for c in &children {
            parents.insert(c.clone(), id.clone());
        }
        let mut props = BTreeMap::new();
        if let Some(list) = raw.get("properties").and_then(Value::as_array) {
            for p in list {
                if let Some(name) = p.get("name").and_then(Value::as_str) {
                    let val = p.pointer("/value/value").cloned().unwrap_or(Value::Null);
                    props.insert(name.to_string(), val);
                }
            }
        }
        nodes.insert(
            id.clone(),
            AxNode {
                id,
                backend_node_id: raw.get("backendDOMNodeId").and_then(Value::as_u64),
                role: ax_string(raw.get("role")).unwrap_or_default(),
                name: ax_string(raw.get("name")).unwrap_or_default(),
                value: ax_string(raw.get("value")).filter(|s| !s.is_empty()),
                description: ax_string(raw.get("description")).filter(|s| !s.is_empty()),
                ignored: raw.get("ignored").and_then(Value::as_bool).unwrap_or(false),
                props,
                children,
            },
        );
    }
    let root = root
        .or(first)
        .ok_or_else(|| anyhow!("Accessibility.getFullAXTree returned an empty tree"))?;
    Ok(AxTree {
        nodes,
        root,
        parents,
    })
}

/// `AXValue { type, value }` → string (numbers rendered with `to_string`).
fn ax_string(v: Option<&Value>) -> Option<String> {
    let inner = v?.get("value")?;
    match inner {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Identity of the document the tree was taken from: the root
/// (`RootWebArea`) node's `backendDOMNodeId`, which is the Document node.
/// It changes on every navigation, so it doubles as a staleness token
/// for refs. `None` when the tree carries no backend ids (mock/partial).
pub fn document_token(tree: &AxTree) -> Option<u64> {
    tree.nodes.get(&tree.root).and_then(|n| n.backend_node_id)
}

/// Roles that mean "the agent can act on this".
pub fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "checkbox"
            | "radio"
            | "combobox"
            | "listbox"
            | "option"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "slider"
            | "switch"
            | "tab"
            | "spinbutton"
            | "treeitem"
    )
}

fn is_text_role(role: &str) -> bool {
    matches!(role, "StaticText" | "text" | "InlineTextBox")
}

/// Nodes that add no information of their own: their children are hoisted
/// to the parent's level. `RootWebArea` is included because the tool
/// prints its own header line for the page.
fn is_structural(node: &AxNode) -> bool {
    node.ignored
        || matches!(
            node.role.as_str(),
            "generic" | "none" | "presentation" | "LineBreak" | "RootWebArea"
        )
}

fn prop_true(node: &AxNode, key: &str) -> bool {
    matches!(node.props.get(key), Some(Value::Bool(true)))
}

fn is_hidden(node: &AxNode) -> bool {
    prop_true(node, "hidden") || prop_true(node, "hiddenRoot")
}

fn is_focusable(node: &AxNode) -> bool {
    prop_true(node, "focusable")
}

fn is_interactive(node: &AxNode) -> bool {
    is_interactive_role(&node.role) || is_focusable(node)
}

/// Does this node get a `[ref=eN]`? Interactive/focusable nodes always;
/// otherwise anything with a name (headings, images, labelled regions) so
/// `browser_take_screenshot { ref }` and subtree snapshots have something
/// to point at. Unnamed structural containers stay ref-less to keep the
/// listing short.
fn wants_ref(node: &AxNode) -> bool {
    node.backend_node_id.is_some()
        && !is_text_role(&node.role)
        && (is_interactive(node) || !node.name.is_empty())
}

/// Rendering options for [`render_snapshot`].
#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    /// Keep only interactive elements and their ancestors.
    pub interactive_only: bool,
    /// Cut the output at the last line boundary before this many bytes.
    pub max_chars: usize,
    /// Render only the subtree rooted at the node with this
    /// `backendDOMNodeId` (from a previous ref).
    pub root_backend_id: Option<u64>,
    /// Number of emitted levels below the root to include; deeper
    /// content is collapsed into a `… (N more)` marker.
    pub depth: Option<usize>,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            interactive_only: false,
            max_chars: DEFAULT_MAX_CHARS,
            root_backend_id: None,
            depth: None,
        }
    }
}

pub const DEFAULT_MAX_CHARS: usize = 50_000;

/// Rendered snapshot.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub text: String,
    pub truncated: bool,
    /// Size before truncation, in bytes.
    pub total_chars: usize,
}

/// Render the tree as indented YAML-ish lines, interning refs as it goes.
pub fn render_snapshot(
    tree: &AxTree,
    refs: &mut RefTable,
    opts: &SnapshotOptions,
) -> Result<Snapshot> {
    let start = match opts.root_backend_id {
        Some(bid) => tree
            .nodes
            .values()
            .find(|n| n.backend_node_id == Some(bid))
            .map(|n| n.id.clone())
            .ok_or_else(|| anyhow!("ref not found in the current accessibility tree"))?,
        None => tree.root.clone(),
    };
    let keep = if opts.interactive_only {
        Some(interactive_closure(tree))
    } else {
        None
    };
    let mut r = Renderer {
        tree,
        refs,
        keep: keep.as_ref(),
        interactive_only: opts.interactive_only,
        out: String::new(),
    };
    r.emit(&start, 0, opts.depth);
    let mut text = r.out;
    let total_chars = text.len();
    let mut truncated = false;
    if text.len() > opts.max_chars {
        let mut cut = opts.max_chars;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let cut = text[..cut].rfind('\n').unwrap_or(cut);
        text.truncate(cut);
        text.push_str(&format!(
            "\n… [truncated at {} chars; use interactive_only, ref, or depth to narrow]",
            opts.max_chars
        ));
        truncated = true;
    }
    Ok(Snapshot {
        text,
        truncated,
        total_chars,
    })
}

/// Set of node ids that are interactive themselves or contain an
/// interactive descendant (so landmarks survive `interactive_only` as
/// context).
fn interactive_closure(tree: &AxTree) -> HashSet<String> {
    let mut keep = HashSet::new();
    fn walk(tree: &AxTree, id: &str, keep: &mut HashSet<String>) -> bool {
        let Some(node) = tree.nodes.get(id) else {
            return false;
        };
        if is_hidden(node) {
            return false;
        }
        let mut any = is_interactive(node) && !is_text_role(&node.role);
        for c in &node.children {
            if walk(tree, c, keep) {
                any = true;
            }
        }
        if any {
            keep.insert(id.to_string());
        }
        any
    }
    walk(tree, &tree.root, &mut keep);
    keep
}

struct Renderer<'a> {
    tree: &'a AxTree,
    refs: &'a mut RefTable,
    keep: Option<&'a HashSet<String>>,
    interactive_only: bool,
    out: String,
}

impl Renderer<'_> {
    fn visible(&self, node: &AxNode) -> bool {
        if is_hidden(node) {
            return false;
        }
        if let Some(keep) = self.keep {
            if !keep.contains(&node.id) {
                return false;
            }
        }
        true
    }

    fn emit(&mut self, id: &str, level: usize, depth: Option<usize>) {
        let Some(node) = self.tree.nodes.get(id) else {
            return;
        };
        if !self.visible(node) {
            return;
        }
        if is_structural(node) {
            for c in node.children.clone() {
                self.emit(&c, level, depth);
            }
            return;
        }
        if node.role == "InlineTextBox" {
            return;
        }
        if is_text_role(&node.role) {
            if !self.interactive_only && !node.name.trim().is_empty() {
                self.out.push_str(&"  ".repeat(level));
                self.out.push_str("- text: ");
                self.out.push_str(node.name.trim());
                self.out.push('\n');
            }
            return;
        }
        let line = self.line_for(node, level);
        self.out.push_str(&line);
        self.out.push('\n');
        match depth {
            Some(0) => {
                let n = self.count_lines_below(node);
                if n > 0 {
                    self.out.push_str(&"  ".repeat(level + 1));
                    self.out.push_str(&format!("… ({n} more)\n"));
                }
            }
            _ => {
                for c in node.children.clone() {
                    self.emit(&c, level + 1, depth.map(|d| d.saturating_sub(1)));
                }
            }
        }
    }

    /// Number of lines `emit` would print for the descendants of `node`.
    fn count_lines_below(&self, node: &AxNode) -> usize {
        let mut n = 0;
        for c in &node.children {
            n += self.count_lines(c);
        }
        n
    }

    fn count_lines(&self, id: &str) -> usize {
        let Some(node) = self.tree.nodes.get(id) else {
            return 0;
        };
        if !self.visible(node) {
            return 0;
        }
        if is_structural(node) {
            return node.children.iter().map(|c| self.count_lines(c)).sum();
        }
        if node.role == "InlineTextBox" {
            return 0;
        }
        if is_text_role(&node.role) {
            return usize::from(!self.interactive_only && !node.name.trim().is_empty());
        }
        1 + self.count_lines_below(node)
    }

    fn line_for(&mut self, node: &AxNode, level: usize) -> String {
        let mut line = String::new();
        line.push_str(&"  ".repeat(level));
        line.push_str("- ");
        line.push_str(&node.role);
        if !node.name.is_empty() {
            line.push(' ');
            line.push_str(&quote(&node.name));
        }
        if wants_ref(node) {
            let r = self
                .refs
                .intern(node.backend_node_id.unwrap_or(0), &node.role, &node.name);
            line.push_str(&format!(" [ref={r}]"));
        }
        for (key, label) in [
            ("disabled", "disabled"),
            ("expanded", "expanded"),
            ("selected", "selected"),
            ("focused", "focused"),
            ("required", "required"),
            ("readonly", "readonly"),
        ] {
            if prop_true(node, key) {
                line.push_str(&format!(" [{label}]"));
            }
        }
        for key in ["checked", "pressed"] {
            match node.props.get(key) {
                Some(Value::String(s)) if s == "true" => line.push_str(&format!(" [{key}]")),
                Some(Value::String(s)) if s == "mixed" => line.push_str(&format!(" [{key}=mixed]")),
                Some(Value::Bool(true)) => line.push_str(&format!(" [{key}]")),
                _ => {}
            }
        }
        if let Some(Value::Number(n)) = node.props.get("level") {
            line.push_str(&format!(" [level={n}]"));
        }
        if let Some(Value::String(url)) = node.props.get("url") {
            if node.role == "link" && !url.is_empty() {
                line.push_str(&format!(" [url={}]", quote(&truncate(url, 100))));
            }
        }
        if let Some(v) = &node.value {
            if matches!(
                node.role.as_str(),
                "textbox" | "searchbox" | "combobox" | "slider" | "spinbutton" | "listbox"
            ) {
                line.push_str(&format!(" [value={}]", quote(&truncate(v, 80))));
            }
        }
        line
    }
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

// ---------------------------------------------------------------------------
// find
// ---------------------------------------------------------------------------

/// Options for [`find`].
#[derive(Debug, Clone)]
pub struct FindOptions {
    pub interactive_only: bool,
    pub limit: usize,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            interactive_only: true,
            limit: DEFAULT_FIND_LIMIT,
        }
    }
}

pub const DEFAULT_FIND_LIMIT: usize = 20;

/// One `find` hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub r#ref: String,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    /// Nearest named landmark/form/dialog/list ancestor, for orientation.
    pub context: Option<String>,
    pub score: i32,
}

/// Map query words that name a role onto the AX roles they cover.
fn role_aliases(token: &str) -> Option<&'static [&'static str]> {
    Some(match token {
        "button" | "btn" => &["button"],
        "link" | "anchor" => &["link"],
        "textbox" | "input" | "field" | "textarea" => {
            &["textbox", "searchbox", "combobox", "spinbutton"]
        }
        "checkbox" => &["checkbox"],
        "radio" => &["radio"],
        "combobox" | "select" | "dropdown" => &["combobox", "listbox"],
        "heading" | "header" | "title" => &["heading"],
        "tab" => &["tab"],
        "menuitem" | "menu" => &["menuitem", "menuitemcheckbox", "menuitemradio"],
        "switch" | "toggle" => &["switch"],
        "option" => &["option"],
        "slider" => &["slider"],
        "image" | "img" | "icon" => &["image", "img"],
        _ => return None,
    })
}

fn tokenize(q: &str) -> Vec<String> {
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Score every candidate node against a free-text query. Pure text
/// matching — no model call. Returns at most `opts.limit` matches, best
/// first, ties broken by document order.
pub fn find(tree: &AxTree, refs: &mut RefTable, query: &str, opts: &FindOptions) -> Vec<Match> {
    let tokens = tokenize(query);
    let mut role_tokens: Vec<&'static [&'static str]> = Vec::new();
    let mut text_tokens: Vec<String> = Vec::new();
    for t in &tokens {
        match role_aliases(t) {
            Some(roles) => role_tokens.push(roles),
            None => text_tokens.push(t.clone()),
        }
    }
    let text_query = text_tokens.join(" ");
    let mut candidates: Vec<(usize, &AxNode)> = Vec::new();
    collect_candidates(tree, &tree.root, opts.interactive_only, &mut candidates);

    let mut scored: Vec<(i32, usize, &AxNode)> = Vec::new();
    for (order, node) in candidates {
        let name = node.name.to_lowercase();
        let value = node.value.as_deref().unwrap_or("").to_lowercase();
        let desc = node.description.as_deref().unwrap_or("").to_lowercase();
        let mut score = 0;
        let mut text_hit = false;
        if !text_query.is_empty() {
            if name == text_query {
                score += 100;
                text_hit = true;
            } else if name.contains(&text_query) {
                score += 60;
                text_hit = true;
            }
            for t in &text_tokens {
                if name.contains(t.as_str()) {
                    score += 10;
                    text_hit = true;
                }
                if value.contains(t.as_str()) || desc.contains(t.as_str()) {
                    score += 5;
                    text_hit = true;
                }
            }
        }
        let mut role_hit = false;
        if !role_tokens.is_empty() {
            if role_tokens
                .iter()
                .any(|roles| roles.contains(&node.role.as_str()))
            {
                score += 8;
                role_hit = true;
            } else {
                score -= 20;
            }
        }
        if !text_hit && !(text_tokens.is_empty() && role_hit) {
            continue;
        }
        if is_interactive_role(&node.role) {
            score += 15;
        }
        if is_focusable(node) {
            score += 3;
        }
        scored.push((score, order, node));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(opts.limit)
        .map(|(score, _, node)| Match {
            r#ref: refs.intern(node.backend_node_id.unwrap_or(0), &node.role, &node.name),
            role: node.role.clone(),
            name: node.name.clone(),
            value: node.value.clone(),
            context: context_for(tree, node),
            score,
        })
        .collect()
}

fn collect_candidates<'a>(
    tree: &'a AxTree,
    id: &str,
    interactive_only: bool,
    out: &mut Vec<(usize, &'a AxNode)>,
) {
    let Some(node) = tree.nodes.get(id) else {
        return;
    };
    if is_hidden(node) {
        return;
    }
    let eligible = node.backend_node_id.is_some()
        && !is_structural(node)
        && !is_text_role(&node.role)
        && if interactive_only {
            is_interactive(node)
        } else {
            wants_ref(node)
        };
    if eligible {
        out.push((out.len(), node));
    }
    for c in &node.children {
        collect_candidates(tree, c, interactive_only, out);
    }
}

fn context_for(tree: &AxTree, node: &AxNode) -> Option<String> {
    let mut cur = tree.parents.get(&node.id);
    while let Some(pid) = cur {
        let p = tree.nodes.get(pid)?;
        if !p.name.is_empty()
            && matches!(
                p.role.as_str(),
                "form"
                    | "dialog"
                    | "alertdialog"
                    | "navigation"
                    | "main"
                    | "region"
                    | "banner"
                    | "contentinfo"
                    | "complementary"
                    | "list"
                    | "table"
                    | "group"
                    | "article"
                    | "section"
                    | "menu"
                    | "tablist"
            )
        {
            return Some(format!("{} {}", p.role, quote(&p.name)));
        }
        cur = tree.parents.get(pid);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, role: &str, name: &str, backend: u64, children: &[&str]) -> Value {
        json!({
            "nodeId": id,
            "backendDOMNodeId": backend,
            "role": {"type": "role", "value": role},
            "name": {"type": "computedString", "value": name},
            "childIds": children,
        })
    }

    fn with_parent(mut v: Value, parent: &str) -> Value {
        v["parentId"] = json!(parent);
        v
    }

    fn with_props(mut v: Value, props: Value) -> Value {
        let list: Vec<Value> = props
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, val)| json!({"name": k, "value": {"type": "x", "value": val}}))
            .collect();
        v["properties"] = json!(list);
        v
    }

    /// RootWebArea → generic(ignored) → main → [heading, textbox(desc Search), button, StaticText, link(focusable), hidden button]
    fn fixture() -> Value {
        let mut textbox = with_parent(node("5", "textbox", "Query", 105, &[]), "3");
        textbox["description"] = json!({"type": "computedString", "value": "Search the docs"});
        textbox["value"] = json!({"type": "string", "value": "hooks"});
        let textbox = with_props(textbox, json!({"focusable": true, "focused": true}));
        let link = with_props(
            with_parent(node("8", "link", "Docs", 108, &["9"]), "3"),
            json!({"focusable": true, "url": "https://example.com/docs"}),
        );
        let hidden = with_props(
            with_parent(node("10", "button", "Ghost", 110, &[]), "3"),
            json!({"hidden": true}),
        );
        let mut ignored = with_parent(node("2", "generic", "", 102, &["3"]), "1");
        ignored["ignored"] = json!(true);
        json!({"nodes": [
            node("1", "RootWebArea", "Example", 101, &["2"]),
            ignored,
            with_parent(node("3", "main", "Content", 103, &["4", "5", "6", "7", "8", "10"]), "2"),
            with_props(with_parent(node("4", "heading", "Welcome", 104, &[]), "3"), json!({"level": 1})),
            textbox,
            with_props(with_parent(node("6", "button", "Submit", 106, &[]), "3"), json!({"focusable": true, "disabled": true})),
            with_parent(node("7", "StaticText", "Some text", 107, &[]), "3"),
            link,
            with_parent(node("9", "StaticText", "Docs", 109, &[]), "8"),
            hidden,
        ]})
    }

    #[test]
    fn renders_playwright_style_lines_with_refs() {
        let tree = parse_full_ax_tree(&fixture()).unwrap();
        assert_eq!(document_token(&tree), Some(101));
        let mut refs = RefTable::new(101);
        let snap = render_snapshot(&tree, &mut refs, &SnapshotOptions::default()).unwrap();
        let expected = "\
- main \"Content\" [ref=e1]
  - heading \"Welcome\" [ref=e2] [level=1]
  - textbox \"Query\" [ref=e3] [focused] [value=\"hooks\"]
  - button \"Submit\" [ref=e4] [disabled]
  - text: Some text
  - link \"Docs\" [ref=e5] [url=\"https://example.com/docs\"]
    - text: Docs
";
        assert_eq!(snap.text, expected);
        assert!(!snap.truncated);
        assert_eq!(refs.lookup("e4").unwrap().backend_node_id, 106);
        // Hidden node never gets a ref.
        assert!(refs.lookup("e6").is_none());
    }

    #[test]
    fn interactive_only_keeps_ancestors_and_drops_text_and_headings() {
        let tree = parse_full_ax_tree(&fixture()).unwrap();
        let mut refs = RefTable::new(101);
        let snap = render_snapshot(
            &tree,
            &mut refs,
            &SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        let expected = "\
- main \"Content\" [ref=e1]
  - textbox \"Query\" [ref=e2] [focused] [value=\"hooks\"]
  - button \"Submit\" [ref=e3] [disabled]
  - link \"Docs\" [ref=e4] [url=\"https://example.com/docs\"]
";
        assert_eq!(snap.text, expected);
    }

    #[test]
    fn depth_collapses_deeper_levels() {
        let tree = parse_full_ax_tree(&fixture()).unwrap();
        let mut refs = RefTable::new(101);
        let snap = render_snapshot(
            &tree,
            &mut refs,
            &SnapshotOptions {
                depth: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(snap.text, "- main \"Content\" [ref=e1]\n  … (6 more)\n");
        let snap = render_snapshot(
            &tree,
            &mut refs,
            &SnapshotOptions {
                depth: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(snap.text.contains("- link \"Docs\" [ref=e5]"));
        assert!(snap.text.contains("    … (1 more)"));
        assert!(!snap.text.contains("    - text: Docs"));
    }

    #[test]
    fn max_chars_truncates_on_line_boundary() {
        let tree = parse_full_ax_tree(&fixture()).unwrap();
        let mut refs = RefTable::new(101);
        let snap = render_snapshot(
            &tree,
            &mut refs,
            &SnapshotOptions {
                max_chars: 70,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(snap.truncated);
        assert!(snap.total_chars > 70);
        // 70 bytes lands mid-way through the textbox line; the cut backs up
        // to the end of the heading line.
        let body = snap.text.split("\n…").next().unwrap();
        assert!(body.ends_with("[level=1]"), "{body:?}");
        assert!(snap.text.contains("[truncated at 70 chars"));
    }

    #[test]
    fn ref_subtree_and_stability_across_renders() {
        let tree = parse_full_ax_tree(&fixture()).unwrap();
        let mut refs = RefTable::new(101);
        render_snapshot(&tree, &mut refs, &SnapshotOptions::default()).unwrap();
        // Subtree rooted at the link keeps the link's existing ref.
        let snap = render_snapshot(
            &tree,
            &mut refs,
            &SnapshotOptions {
                root_backend_id: Some(108),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            snap.text,
            "- link \"Docs\" [ref=e5] [url=\"https://example.com/docs\"]\n  - text: Docs\n"
        );
        assert_eq!(refs.len(), 5);
        let err = render_snapshot(
            &tree,
            &mut refs,
            &SnapshotOptions {
                root_backend_id: Some(999),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("ref not found"));
    }

    #[test]
    fn find_ranks_exact_then_substring_then_tokens_and_filters_by_role() {
        let tree = parse_full_ax_tree(&fixture()).unwrap();
        let mut refs = RefTable::new(101);
        let hits = find(&tree, &mut refs, "submit", &FindOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role, "button");
        assert_eq!(hits[0].context.as_deref(), Some("main \"Content\""));

        // Description (placeholder-like) matches too, at lower weight.
        let hits = find(&tree, &mut refs, "search", &FindOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Query");

        // Role-only query returns every node of that role.
        let hits = find(&tree, &mut refs, "link", &FindOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Docs");

        // Role + text: a mismatched role token is penalised relative to a
        // matching one, but the text hit still surfaces the element.
        let wrong = find(&tree, &mut refs, "docs button", &FindOptions::default());
        let right = find(&tree, &mut refs, "docs link", &FindOptions::default());
        assert_eq!(wrong[0].name, "Docs");
        assert_eq!(right[0].name, "Docs");
        assert!(wrong[0].score < right[0].score);

        // interactive_only=false surfaces headings.
        let hits = find(
            &tree,
            &mut refs,
            "welcome",
            &FindOptions {
                interactive_only: false,
                limit: 20,
            },
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role, "heading");
        assert!(find(&tree, &mut refs, "welcome", &FindOptions::default()).is_empty());

        // Refs handed out by find are the same ones a snapshot would give.
        let r = refs.lookup(&hits[0].r#ref).unwrap();
        assert_eq!(r.backend_node_id, 104);
    }

    #[test]
    fn find_respects_limit_and_document_order_ties() {
        let mut nodes = vec![node("1", "RootWebArea", "", 1, &["2", "3", "4"])];
        for (i, id) in ["2", "3", "4"].iter().enumerate() {
            nodes.push(with_props(
                with_parent(node(id, "button", "Add to cart", 10 + i as u64, &[]), "1"),
                json!({"focusable": true}),
            ));
        }
        let tree = parse_full_ax_tree(&json!({"nodes": nodes})).unwrap();
        let mut refs = RefTable::new(1);
        let hits = find(
            &tree,
            &mut refs,
            "add to cart",
            &FindOptions {
                interactive_only: true,
                limit: 2,
            },
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].r#ref, "e1");
        assert_eq!(hits[1].r#ref, "e2");
        assert_eq!(refs.lookup("e1").unwrap().backend_node_id, 10);
    }

    #[test]
    fn parse_rejects_missing_nodes() {
        assert!(parse_full_ax_tree(&json!({})).is_err());
        assert!(parse_full_ax_tree(&json!({"nodes": []})).is_err());
    }
}

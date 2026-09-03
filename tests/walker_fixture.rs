//! Pins the contract of the Firefox accessibility walker
//! (`src/dom/js/snapshot_tree.js`) through a checked-in sample.
//!
//! `tests/fixtures/walker_sample.ax.json` is the walker's raw output for
//! `tests/fixtures/walker_sample.html` in headless Firefox, with the random
//! document token normalised to `4294967296`; `walker_sample.snapshot.txt`
//! is what `browser_snapshot` renders from it. There is no JS runtime in
//! CI, so this test proves the Rust side (parser, renderer, `find`, refs)
//! agrees with what the walker actually produces.
//!
//! Regenerate after changing the walker:
//!
//! ```sh
//! browser-control start firefox --headless
//! # open the sample in a named tab, run the walker through the MCP server
//! # (see the BiDi smoke driver), write the JSON to walker_sample.ax.json with
//! # the root backendDOMNodeId replaced by 4294967296, then:
//! UPDATE_FIXTURES=1 cargo test --test walker_fixture
//! ```

use browser_control::a11y::{
    self, document_token, find, parse_full_ax_tree, render_snapshot, FindOptions, RefTable,
    SnapshotOptions,
};

const AX_JSON: &str = include_str!("fixtures/walker_sample.ax.json");
const SNAPSHOT_TXT: &str = include_str!("fixtures/walker_sample.snapshot.txt");
const TOKEN: u64 = 4_294_967_296;

fn tree() -> a11y::AxTree {
    let v: serde_json::Value = serde_json::from_str(AX_JSON).expect("fixture is JSON");
    parse_full_ax_tree(&v).expect("fixture parses as an AX tree")
}

#[test]
fn walker_output_renders_expected_snapshot() {
    let tree = tree();
    assert_eq!(document_token(&tree), Some(TOKEN));
    let mut refs = RefTable::new(TOKEN);
    let snap = render_snapshot(&tree, &mut refs, &SnapshotOptions::default()).unwrap();
    if std::env::var("UPDATE_FIXTURES").is_ok() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/walker_sample.snapshot.txt"
        );
        std::fs::write(path, &snap.text).unwrap();
        eprintln!("updated {path}");
        return;
    }
    // A Windows checkout with autocrlf turns the fixture's newlines into
    // CRLF; the renderer always emits LF.
    assert_eq!(snap.text, SNAPSHOT_TXT.replace("\r\n", "\n"));
    assert!(!snap.truncated);
}

#[test]
fn walker_text_nodes_never_carry_refs() {
    let tree = tree();
    for n in tree.nodes.values() {
        if n.role == "StaticText" {
            assert!(
                n.backend_node_id.is_none(),
                "text node {} has a backend id",
                n.id
            );
        } else if n.id != tree.root {
            assert!(
                n.backend_node_id.is_some(),
                "element node {} lacks a backend id",
                n.id
            );
        }
    }
}

#[test]
fn walker_interactive_only_keeps_controls_and_landmarks() {
    let tree = tree();
    let mut refs = RefTable::new(TOKEN);
    let snap = render_snapshot(
        &tree,
        &mut refs,
        &SnapshotOptions {
            interactive_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(snap.text.contains("- form \"Sign up\""));
    assert!(snap.text.contains("- textbox \"Email address\""));
    assert!(snap.text.contains("- checkbox \"Subscribe\""));
    assert!(!snap.text.contains("- heading"));
    assert!(!snap.text.contains("- text:"));
}

#[test]
fn walker_find_matches_labels_placeholders_and_roles() {
    let tree = tree();
    let mut refs = RefTable::new(TOKEN);
    let hits = find(&tree, &mut refs, "email", &FindOptions::default());
    assert_eq!(hits[0].role, "textbox");
    assert_eq!(hits[0].name, "Email address");
    assert_eq!(hits[0].context.as_deref(), Some("form \"Sign up\""));

    let hits = find(&tree, &mut refs, "search", &FindOptions::default());
    assert_eq!(hits[0].role, "searchbox");

    let hits = find(
        &tree,
        &mut refs,
        "create account button",
        &FindOptions::default(),
    );
    assert_eq!(hits[0].name, "Create account");

    let hits = find(&tree, &mut refs, "shadow button", &FindOptions::default());
    assert_eq!(
        hits[0].name, "Shadow button",
        "open shadow roots are walked"
    );

    let hits = find(
        &tree,
        &mut refs,
        "prices",
        &FindOptions {
            interactive_only: false,
            limit: 5,
        },
    );
    assert_eq!(hits[0].role, "table");
}

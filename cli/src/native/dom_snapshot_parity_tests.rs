//! The DOM-walk snapshot that WebDriver backends use, checked inside real
//! headless Chrome: parity with the CDP accessibility snapshot, page-side ref
//! verification, and linear cost.
//!
//!   cargo test dom_snapshot -- --ignored --test-threads=1

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

use super::actions::{execute_command, DaemonState};
use super::element::RefMap;
use super::snapshot::{self, SnapshotOptions};

fn data(resp: &Value) -> &Value {
    assert_eq!(resp["success"], true, "command failed: {}", resp);
    &resp["data"]
}

/// Ref numbers depend on traversal order, which differs between the AX tree
/// and the DOM walk; compare everything else.
fn normalize_refs(tree: &str) -> String {
    let mut out = String::with_capacity(tree.len());
    let mut rest = tree;
    while let Some(pos) = rest.find("ref=e") {
        out.push_str(&rest[..pos + 4]);
        rest = rest[pos + 5..].trim_start_matches(|c: char| c.is_ascii_digit());
        out.push('*');
    }
    out.push_str(rest);
    out
}

async fn open_html(state: &mut DaemonState, html: &str) {
    data(
        &execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            state,
        )
        .await,
    );
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    data(
        &execute_command(
            &json!({ "id": "2", "action": "navigate", "url": url }),
            state,
        )
        .await,
    );
}

async fn eval(state: &DaemonState, expression: &str) -> Value {
    state
        .browser
        .as_ref()
        .unwrap()
        .evaluate(expression, None)
        .await
        .unwrap_or_else(|e| panic!("evaluation failed: {}", e))
}

/// Runs a WebDriver-style function body in Chrome.
fn as_expression(body: &str) -> String {
    format!("(function() {{\n{}\n}})()", body)
}

async fn dom_snapshot(state: &DaemonState, options: &SnapshotOptions, refs: &mut RefMap) -> String {
    let result = eval(
        state,
        &as_expression(&snapshot::dom_snapshot_script(options)),
    )
    .await;
    snapshot::render_dom_snapshot(snapshot::parse_dom_snapshot(result).unwrap(), options, refs)
}

#[tokio::test]
#[ignore]
async fn e2e_dom_snapshot_parity_fixture_interactive() {
    let mut state = DaemonState::new();
    open_html(
        &mut state,
        include_str!("test_fixtures/dom_snapshot_parity.html"),
    )
    .await;

    let cdp = data(
        &execute_command(
            &json!({ "id": "3", "action": "snapshot", "interactive": true }),
            &mut state,
        )
        .await,
    )["snapshot"]
        .as_str()
        .unwrap()
        .to_string();
    let options = SnapshotOptions {
        interactive: true,
        ..Default::default()
    };
    let dom = dom_snapshot(&state, &options, &mut RefMap::new()).await;
    let (cdp, dom) = (normalize_refs(&cdp), normalize_refs(&dom));

    let diff = TextDiff::from_lines(&cdp, &dom);
    let changed: Vec<String> = diff
        .iter_all_changes()
        .filter(|c| c.tag() != ChangeTag::Equal)
        .map(|c| {
            format!(
                "{}{}",
                if c.tag() == ChangeTag::Delete {
                    "-"
                } else {
                    "+"
                },
                c.value()
            )
        })
        .collect();
    assert!(
        changed.is_empty(),
        "DOM-walk -i snapshot differs from CDP (- CDP, + DOM walk):\n{}",
        changed.join("")
    );
    data(&execute_command(&json!({ "id": "9", "action": "close" }), &mut state).await);
}

/// What the page-side resolver returns for a ref: "element:<TAG>", "gone",
/// or the mismatch reason.
async fn resolve(state: &DaemonState, refs: &RefMap, ref_id: &str) -> String {
    let entry = refs.get(ref_id).unwrap();
    let body =
        snapshot::dom_resolve_script(entry.backend_node_id.unwrap(), &entry.role, &entry.name);
    let probe = format!(
        "(function() {{ var r = {}; return r === null ? 'gone' : typeof r === 'string' ? r : 'element:' + r.tagName; }})()",
        as_expression(&body)
    );
    eval(state, &probe).await.as_str().unwrap().to_string()
}

fn ref_named(refs: &RefMap, role: &str, name: &str) -> String {
    refs.entries_sorted()
        .into_iter()
        .find(|(_, e)| e.role == role && e.name == name)
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("no ref for {} {:?}", role, name))
}

#[tokio::test]
#[ignore]
async fn e2e_dom_snapshot_refs_are_verified_in_the_page() {
    let mut state = DaemonState::new();
    open_html(
        &mut state,
        r#"<button id="save">Save</button><button id="other">Other</button>"#,
    )
    .await;
    let mut refs = RefMap::new();
    dom_snapshot(&state, &SnapshotOptions::default(), &mut refs).await;
    let save = ref_named(&refs, "button", "Save");

    assert_eq!(resolve(&state, &refs, &save).await, "element:BUTTON");

    // A page that patches built-ins to redirect lookups is caught by the
    // role/name re-check rather than clicking the wrong element.
    eval(
        &state,
        "(() => { const other = document.getElementById('other'); \
         Map.prototype.get = function () { return { deref: () => other }; }; \
         WeakMap.prototype.get = function () { return undefined; }; return true; })()",
    )
    .await;
    let redirected = resolve(&state, &refs, &save).await;
    assert!(redirected.contains("named \"Other\""), "{}", redirected);

    // Reload restores built-ins; a renamed element no longer matches.
    data(&execute_command(&json!({ "id": "r", "action": "reload" }), &mut state).await);
    let mut refs = RefMap::new();
    dom_snapshot(&state, &SnapshotOptions::default(), &mut refs).await;
    let save = ref_named(&refs, "button", "Save");
    eval(
        &state,
        "document.getElementById('save').textContent = 'Delete'",
    )
    .await;
    let renamed = resolve(&state, &refs, &save).await;
    assert!(renamed.contains("expected \"Save\""), "{}", renamed);

    eval(&state, "document.getElementById('save').remove()").await;
    assert_eq!(resolve(&state, &refs, &save).await, "gone");

    // The registry is not enumerable on window.
    let leaked = eval(
        &state,
        "Object.keys(window).filter(k => k.startsWith('__ab')).length",
    )
    .await;
    assert_eq!(leaked, json!(0));
    data(&execute_command(&json!({ "id": "9", "action": "close" }), &mut state).await);
}

/// Name-from-content over nested tables must not recompute styles for every
/// ancestor: each element's style is read a bounded number of times.
#[tokio::test]
#[ignore]
async fn e2e_dom_snapshot_style_lookups_stay_linear() {
    let mut html = String::new();
    for _ in 0..25 {
        html.push_str("<table><tr><th>h</th><td>cell text <a href='#'>link</a>");
    }
    for _ in 0..25 {
        html.push_str("</td></tr></table>");
    }
    let mut state = DaemonState::new();
    open_html(&mut state, &html).await;
    eval(
        &state,
        "(() => { window.__calls = 0; const orig = window.getComputedStyle; \
         window.getComputedStyle = function (...a) { window.__calls++; return orig.apply(this, a); }; \
         return true; })()",
    )
    .await;
    dom_snapshot(&state, &SnapshotOptions::default(), &mut RefMap::new()).await;
    let calls = eval(&state, "window.__calls").await.as_u64().unwrap();
    let elements = eval(&state, "document.querySelectorAll('*').length")
        .await
        .as_u64()
        .unwrap();
    assert!(
        calls <= elements * 6,
        "{} getComputedStyle calls for {} elements",
        calls,
        elements
    );
    data(&execute_command(&json!({ "id": "9", "action": "close" }), &mut state).await);
}

/// `-i` prunes non-ref nodes in the page to keep the payload small; the
/// rendered result must equal rendering the full walk in interactive mode.
#[tokio::test]
#[ignore]
async fn e2e_dom_snapshot_interactive_pruning_changes_payload_not_output() {
    let mut state = DaemonState::new();
    open_html(
        &mut state,
        include_str!("test_fixtures/dom_snapshot_parity.html"),
    )
    .await;
    let interactive = SnapshotOptions {
        interactive: true,
        ..Default::default()
    };

    let pruned_raw = eval(
        &state,
        &as_expression(&snapshot::dom_snapshot_script(&interactive)),
    )
    .await;
    let full_raw = eval(
        &state,
        &as_expression(&snapshot::dom_snapshot_script(&SnapshotOptions::default())),
    )
    .await;
    let pruned_nodes = pruned_raw["nodes"].as_array().unwrap().len();
    let full_nodes = full_raw["nodes"].as_array().unwrap().len();
    assert!(
        pruned_nodes * 2 < full_nodes,
        "pruning should drop most nodes: {} of {}",
        pruned_nodes,
        full_nodes
    );

    let pruned = snapshot::render_dom_snapshot(
        snapshot::parse_dom_snapshot(pruned_raw).unwrap(),
        &interactive,
        &mut RefMap::new(),
    );
    let full = snapshot::render_dom_snapshot(
        snapshot::parse_dom_snapshot(full_raw).unwrap(),
        &interactive,
        &mut RefMap::new(),
    );
    assert_eq!(normalize_refs(&pruned), normalize_refs(&full));
    data(&execute_command(&json!({ "id": "9", "action": "close" }), &mut state).await);
}

#[test]
fn normalize_refs_masks_ref_numbers_only() {
    assert_eq!(
        normalize_refs("- link \"a\" [ref=e12]\n- button [level=2, ref=e3]"),
        "- link \"a\" [ref=*]\n- button [level=2, ref=*]"
    );
}

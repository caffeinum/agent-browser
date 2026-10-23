//! Parity between the CDP accessibility snapshot and the DOM-walk snapshot
//! that WebDriver backends use, both taken from the same headless Chrome page.
//!
//!   cargo test dom_snapshot_parity -- --ignored --test-threads=1
//!
//! `e2e_dom_snapshot_parity_report` compares arbitrary pages:
//!   AB_PARITY_URLS=https://a,https://b AB_PARITY_OUT=/dir cargo test \
//!     e2e_dom_snapshot_parity_report -- --ignored --nocapture

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

async fn launch(state: &mut DaemonState) {
    data(
        &execute_command(
            &json!({ "id": "1", "action": "launch", "headless": true }),
            state,
        )
        .await,
    );
}

async fn navigate(state: &mut DaemonState, url: &str) {
    data(
        &execute_command(
            &json!({ "id": "2", "action": "navigate", "url": url }),
            state,
        )
        .await,
    );
}

async fn snapshots(state: &mut DaemonState, interactive: bool) -> (String, String) {
    let cdp = data(
        &execute_command(
            &json!({ "id": "3", "action": "snapshot", "interactive": interactive }),
            state,
        )
        .await,
    )["snapshot"]
        .as_str()
        .unwrap()
        .to_string();

    let options = SnapshotOptions {
        interactive,
        ..Default::default()
    };
    let script = format!(
        "(function() {{\n{}\n}})()",
        snapshot::dom_snapshot_script(&options)
    );
    let result = state
        .browser
        .as_ref()
        .unwrap()
        .evaluate(&script, None)
        .await
        .expect("DOM-walk snapshot script failed in Chrome");
    let dom = snapshot::render_dom_snapshot(
        snapshot::parse_dom_snapshot(result).unwrap(),
        &options,
        &mut RefMap::new(),
    );
    (normalize_refs(&cdp), normalize_refs(&dom))
}

struct DiffStats {
    only_cdp: Vec<String>,
    only_dom: Vec<String>,
}

fn diff_lines(cdp: &str, dom: &str) -> DiffStats {
    let diff = TextDiff::from_lines(cdp, dom);
    let mut stats = DiffStats {
        only_cdp: Vec::new(),
        only_dom: Vec::new(),
    };
    for change in diff.iter_all_changes() {
        let line = change.value().trim_end().to_string();
        match change.tag() {
            ChangeTag::Delete => stats.only_cdp.push(line),
            ChangeTag::Insert => stats.only_dom.push(line),
            ChangeTag::Equal => {}
        }
    }
    stats
}

#[tokio::test]
#[ignore]
async fn e2e_dom_snapshot_parity_fixture_interactive() {
    let html = include_str!("test_fixtures/dom_snapshot_parity.html");
    let url = format!("data:text/html;base64,{}", STANDARD.encode(html));
    let mut state = DaemonState::new();
    launch(&mut state).await;
    navigate(&mut state, &url).await;

    let (cdp, dom) = snapshots(&mut state, true).await;
    let stats = diff_lines(&cdp, &dom);
    assert!(
        stats.only_cdp.is_empty() && stats.only_dom.is_empty(),
        "DOM-walk -i snapshot differs from CDP.\n--- CDP\n{}\n--- DOM walk\n{}\n--- only CDP\n{}\n--- only DOM\n{}",
        cdp,
        dom,
        stats.only_cdp.join("\n"),
        stats.only_dom.join("\n")
    );
    data(&execute_command(&json!({ "id": "9", "action": "close" }), &mut state).await);
}

#[tokio::test]
#[ignore]
async fn e2e_dom_snapshot_parity_report() {
    let Ok(urls) = std::env::var("AB_PARITY_URLS") else {
        return;
    };
    let out_dir = std::env::var("AB_PARITY_OUT").expect("AB_PARITY_OUT is required");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut state = DaemonState::new();
    launch(&mut state).await;
    for (i, url) in urls.split(',').enumerate() {
        navigate(&mut state, url).await;
        for interactive in [true, false] {
            let (cdp, dom) = snapshots(&mut state, interactive).await;
            let mode = if interactive { "i" } else { "full" };
            std::fs::write(format!("{}/{}-{}-cdp.txt", out_dir, i, mode), &cdp).unwrap();
            std::fs::write(format!("{}/{}-{}-dom.txt", out_dir, i, mode), &dom).unwrap();
            let stats = diff_lines(&cdp, &dom);
            println!(
                "{} [{}] cdp: {} lines {} bytes | dom: {} lines {} bytes | only-cdp {} lines, only-dom {} lines",
                url,
                mode,
                cdp.lines().count(),
                cdp.len(),
                dom.lines().count(),
                dom.len(),
                stats.only_cdp.len(),
                stats.only_dom.len()
            );
        }
    }
    data(&execute_command(&json!({ "id": "9", "action": "close" }), &mut state).await);
}

#[test]
fn normalize_refs_masks_ref_numbers_only() {
    assert_eq!(
        normalize_refs("- link \"a\" [ref=e12]\n- button [level=2, ref=e3]"),
        "- link \"a\" [ref=*]\n- button [level=2, ref=*]"
    );
}

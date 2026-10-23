//! Snapshot refs on WebDriver sessions. A DOM-walk snapshot records each
//! element under a per-document id in `window.__abById`; refs resolve back
//! through that map to a WebDriver element reference.

use serde_json::{json, Value};

use super::backend::WebDriverBackend;
use crate::native::element::{parse_ref, select_unique_node_id, RefMap};
use crate::native::snapshot::{self, SnapshotOptions};

const W3C_ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";

/// Returns the element for an id, or null when it is gone from the document.
const RESOLVE_ID_JS: &str = "var map = window.__abById; var ref = map && map.get(arguments[0]); \
var el = ref && ref.deref(); return el && el.isConnected ? el : null;";

pub async fn take_snapshot(
    wb: &WebDriverBackend,
    options: &SnapshotOptions,
    ref_map: &mut RefMap,
) -> Result<String, String> {
    let result = wb
        .execute_function_body(&snapshot::dom_snapshot_script(options), vec![])
        .await?;
    Ok(snapshot::render_dom_snapshot(
        snapshot::parse_dom_snapshot(result)?,
        options,
        ref_map,
    ))
}

/// Resolve `@eN` or a CSS selector to a WebDriver element id.
///
/// A ref whose element is gone recovers only when the snapshot had a single
/// element with its role and name and exactly one matches now, mirroring the
/// CDP resolver; otherwise it fails and asks for a new snapshot.
pub async fn resolve_element(
    wb: &WebDriverBackend,
    ref_map: &RefMap,
    selector_or_ref: &str,
) -> Result<String, String> {
    let Some(ref_id) = parse_ref(selector_or_ref) else {
        return wb.find_css(selector_or_ref).await;
    };
    let entry = ref_map
        .get(&ref_id)
        .ok_or_else(|| format!("Unknown ref: {}", ref_id))?;
    let id = entry
        .backend_node_id
        .ok_or_else(|| format!("Element ref {} has no element id", ref_id))?;
    if let Some(element) = element_by_id(wb, id).await? {
        return Ok(element);
    }
    if entry.nth.is_some() {
        return Err(format!(
            "Stale ref for role={} name={} cannot be recovered: the snapshot had several matching elements. Run snapshot again and use a fresh ref.",
            entry.role, entry.name
        ));
    }
    let fresh = snapshot::parse_dom_snapshot(
        wb.execute_function_body(
            &snapshot::dom_snapshot_script(&SnapshotOptions::default()),
            vec![],
        )
        .await?,
    )?;
    let fresh_id = select_unique_node_id(&fresh.nodes, &entry.role, &entry.name)?;
    element_by_id(wb, fresh_id)
        .await?
        .ok_or_else(|| format!("Element ref {} is stale. Run snapshot again.", ref_id))
}

async fn element_by_id(wb: &WebDriverBackend, id: i64) -> Result<Option<String>, String> {
    wb.element_from_script(RESOLVE_ID_JS, vec![json!(id)]).await
}

pub fn element_arg(element_id: &str) -> Value {
    json!({ W3C_ELEMENT_KEY: element_id })
}

pub async fn element_text(wb: &WebDriverBackend, element_id: &str) -> Result<String, String> {
    let value = wb
        .execute_function_body(
            "var el = arguments[0]; return el.innerText || el.textContent || '';",
            vec![element_arg(element_id)],
        )
        .await?;
    Ok(value.as_str().unwrap_or_default().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::webdriver::client::WebDriverClient;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers each request with the next `value`, returning the request lines.
    async fn fake_driver(values: Vec<Value>) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for value in values {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                seen.push(request.lines().next().unwrap_or("").to_string());
                let body = json!({ "value": value }).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            seen
        });
        (port, handle)
    }

    fn backend(port: u16) -> WebDriverBackend {
        WebDriverBackend::new(WebDriverClient::new_with_session(port, "s".to_string()))
    }

    #[tokio::test]
    async fn live_ref_resolves_to_its_element() {
        let (port, server) = fake_driver(vec![json!({ W3C_ELEMENT_KEY: "el-7" })]).await;
        let mut refs = RefMap::new();
        refs.add("e1".to_string(), Some(42), "button", "Save", None);
        let element = resolve_element(&backend(port), &refs, "@e1").await.unwrap();
        assert_eq!(element, "el-7");
        assert_eq!(
            server.await.unwrap(),
            ["POST /session/s/execute/sync HTTP/1.1"]
        );
    }

    #[tokio::test]
    async fn stale_duplicate_ref_refuses_recovery() {
        let (port, server) = fake_driver(vec![Value::Null]).await;
        let mut refs = RefMap::new();
        refs.add("e1".to_string(), Some(42), "button", "Save", Some(1));
        let err = resolve_element(&backend(port), &refs, "@e1")
            .await
            .unwrap_err();
        assert!(err.contains("several matching elements"), "got: {}", err);
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_ref_fails_without_touching_the_page() {
        let refs = RefMap::new();
        let err = resolve_element(&backend(1), &refs, "@e9")
            .await
            .unwrap_err();
        assert_eq!(err, "Unknown ref: e9");
    }
}

//! Snapshot refs on WebDriver sessions. A DOM-walk snapshot records each
//! element in a page-side registry under a per-document id; a ref resolves
//! back through that registry to a WebDriver element reference, after the
//! page re-confirms the element's role and name.

use serde_json::{json, Value};

use super::backend::WebDriverBackend;
use crate::native::element::{parse_ref, select_unique_node_id, RefMap};
use crate::native::snapshot::{self, SnapshotOptions};

const W3C_ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";

pub async fn take_snapshot(
    wb: &WebDriverBackend,
    options: &SnapshotOptions,
    ref_map: &mut RefMap,
) -> Result<String, String> {
    let result = run_dom_walk(wb, options).await?;
    Ok(snapshot::render_dom_snapshot(
        snapshot::parse_dom_snapshot(result)?,
        options,
        ref_map,
    ))
}

async fn run_dom_walk(wb: &WebDriverBackend, options: &SnapshotOptions) -> Result<Value, String> {
    wb.execute_function_body(&snapshot::dom_snapshot_script(options), vec![])
        .await
        .map_err(|e| {
            if e.contains("script timeout") {
                format!(
                    "Snapshot timed out in the page ({}). Narrow it with `snapshot -s <selector>`.",
                    e
                )
            } else {
                e
            }
        })
}

/// Resolve `@eN` or a CSS selector to a WebDriver element id.
///
/// A ref whose element is gone recovers only when the snapshot had a single
/// element with its role and name and exactly one matches now, mirroring the
/// CDP resolver; otherwise it fails and asks for a new snapshot. A ref whose
/// element no longer has its snapshot role and name is refused.
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
    if let Some(element) = element_by_id(wb, &ref_id, id, &entry.role, &entry.name).await? {
        return Ok(element);
    }
    if entry.nth.is_some() {
        return Err(format!(
            "Stale ref for role={} name={} cannot be recovered: the snapshot had several matching elements. Run snapshot again and use a fresh ref.",
            entry.role, entry.name
        ));
    }
    let fresh = snapshot::parse_dom_snapshot(run_dom_walk(wb, &SnapshotOptions::default()).await?)?;
    let fresh_id = select_unique_node_id(&fresh.nodes, &entry.role, &entry.name)?;
    element_by_id(wb, &ref_id, fresh_id, &entry.role, &entry.name)
        .await?
        .ok_or_else(|| format!("Element ref {} is stale. Run snapshot again.", ref_id))
}

/// `None` when the element is gone; an error when it no longer matches.
async fn element_by_id(
    wb: &WebDriverBackend,
    ref_id: &str,
    id: i64,
    role: &str,
    name: &str,
) -> Result<Option<String>, String> {
    let value = wb
        .execute_function_body(&snapshot::dom_resolve_script(id, role, name), vec![])
        .await?;
    match value {
        Value::Null => Ok(None),
        Value::String(reason) => Err(format!(
            "Element ref {} no longer matches its snapshot: {}. Run snapshot again.",
            ref_id, reason
        )),
        Value::Object(ref map) => map
            .get(W3C_ELEMENT_KEY)
            .and_then(Value::as_str)
            .map(|element| Some(element.to_string()))
            .ok_or_else(|| format!("Resolving ref {} returned a non-element: {}", ref_id, value)),
        other => Err(format!(
            "Resolving ref {} returned a non-element: {}",
            ref_id, other
        )),
    }
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
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("get text returned a non-string: {}", value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::webdriver::client::WebDriverClient;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Reads one whole request (headers plus Content-Length body), so large
    /// scripts are consumed before the reply and the client never sees a reset.
    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = stream.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let length = text[..head_end]
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if data.len() >= head_end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&data).to_string()
    }

    async fn fake_driver(values: Vec<Value>) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        fake_driver_with_status(200, values).await
    }

    /// Answers each request with the next `value`, returning the request lines.
    async fn fake_driver_with_status(
        status: u16,
        values: Vec<Value>,
    ) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for value in values {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                seen.push(request);
                let body = json!({ "value": value }).to_string();
                let response = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
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
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /session/s/execute/sync HTTP/1.1"));
        assert!(
            requests[0]
                .contains(r#"\"resolve\":{\"id\":42,\"name\":\"Save\",\"role\":\"button\"}"#),
            "resolve must carry the snapshot role and name"
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

    fn button_snapshot(id: i64) -> Value {
        json!({
            "document": "doc-2",
            "nodes": [
                { "nodeId": "0", "role": { "type": "role", "value": "RootWebArea" }, "childIds": ["1"] },
                {
                    "nodeId": "1",
                    "role": { "type": "role", "value": "button" },
                    "name": { "type": "computedString", "value": "Save" },
                    "backendDOMNodeId": id,
                    "childIds": []
                }
            ],
            "cursor": [],
            "hrefs": {}
        })
    }

    #[tokio::test]
    async fn stale_unique_ref_recovers_to_the_single_match() {
        let (port, server) = fake_driver(vec![
            Value::Null,
            button_snapshot(99),
            json!({ W3C_ELEMENT_KEY: "el-99" }),
        ])
        .await;
        let mut refs = RefMap::new();
        refs.add("e1".to_string(), Some(42), "button", "Save", None);
        let element = resolve_element(&backend(port), &refs, "@e1").await.unwrap();
        assert_eq!(element, "el-99");
        assert_eq!(server.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn ref_whose_element_changed_role_or_name_is_refused() {
        let (port, server) = fake_driver(vec![json!(
            "its element is now named \"Delete\", expected \"Save\""
        )])
        .await;
        let mut refs = RefMap::new();
        refs.add("e1".to_string(), Some(42), "button", "Save", None);
        let err = resolve_element(&backend(port), &refs, "@e1")
            .await
            .unwrap_err();
        assert!(err.contains("no longer matches its snapshot"), "{}", err);
        assert!(err.contains("Delete"), "{}", err);
        assert_eq!(
            server.await.unwrap().len(),
            1,
            "a mismatch must not fall back"
        );
    }

    #[tokio::test]
    async fn snapshot_script_timeout_names_the_remedy() {
        let (port, _server) = fake_driver_with_status(
            500,
            vec![json!({ "error": "script timeout", "message": "30000ms" })],
        )
        .await;
        let mut refs = RefMap::new();
        let err = take_snapshot(&backend(port), &SnapshotOptions::default(), &mut refs)
            .await
            .unwrap_err();
        assert!(err.contains("Snapshot timed out"), "{}", err);
        assert!(err.contains("snapshot -s"), "{}", err);
    }

    #[tokio::test]
    async fn get_text_rejects_non_string_results() {
        let (port, _server) = fake_driver(vec![json!({ "not": "text" })]).await;
        let err = element_text(&backend(port), "el-1").await.unwrap_err();
        assert!(err.contains("non-string"), "{}", err);
    }

    /// `eval` on WebDriver has CDP semantics: the expression's value, not a
    /// function body. This is a breaking change for safari/ios users who wrote
    /// `eval "return ..."`.
    #[tokio::test]
    async fn eval_evaluates_an_expression_like_cdp() {
        let (port, server) = fake_driver(vec![json!("Example Domain")]).await;
        let value = backend(port)
            .evaluate_expression("document.title")
            .await
            .unwrap();
        assert_eq!(value, json!("Example Domain"));
        let request = &server.await.unwrap()[0];
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["script"], "return (0, eval)(arguments[0]);");
        assert_eq!(body["args"], json!(["document.title"]));
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

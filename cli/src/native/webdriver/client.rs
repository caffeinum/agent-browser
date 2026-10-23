use serde_json::{json, Value};
use std::time::Duration;

pub struct WebDriverClient {
    base_url: String,
    session_id: Option<String>,
}

impl WebDriverClient {
    pub fn new(port: u16) -> Self {
        Self {
            base_url: format!("http://127.0.0.1:{}", port),
            session_id: None,
        }
    }

    pub async fn create_session(&mut self, capabilities: Value) -> Result<Value, String> {
        let body = json!({
            "capabilities": {
                "alwaysMatch": capabilities,
            }
        });

        let response = self.post("/session", &body).await?;

        let session_id = response
            .get("value")
            .and_then(|v| v.get("sessionId"))
            .and_then(|v| v.as_str())
            .ok_or("No sessionId in response")?
            .to_string();

        self.session_id = Some(session_id);
        Ok(response)
    }

    pub async fn delete_session(&mut self) -> Result<(), String> {
        if let Some(ref sid) = self.session_id.clone() {
            let _ = self.delete(&format!("/session/{}", sid)).await;
            self.session_id = None;
        }
        Ok(())
    }

    pub async fn navigate(&self, url: &str) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(&format!("/session/{}/url", sid), &json!({ "url": url }))
            .await?;
        Ok(())
    }

    pub async fn get_url(&self) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/url", sid)).await?;
        Ok(response
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn get_title(&self) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/title", sid)).await?;
        Ok(response
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn find_element(&self, using: &str, value: &str) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = http_request(
            "POST",
            &format!("{}/session/{}/element", self.base_url, sid),
            Some(&json!({ "using": using, "value": value })),
        )
        .await?;
        if !response.is_success() && response.w3c_error() == Some("no such element") {
            return Err(format!("No element found by {} '{}'", using, value));
        }
        let body = response.into_value()?;
        let element_value = body.get("value").ok_or("No element in response")?;
        element_id_from_value(element_value, using, value)
    }

    pub async fn click_element(&self, element_id: &str) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(
            &format!("/session/{}/element/{}/click", sid, element_id),
            &json!({}),
        )
        .await?;
        Ok(())
    }

    pub async fn send_keys(&self, element_id: &str, text: &str) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(
            &format!("/session/{}/element/{}/value", sid, element_id),
            &json!({ "text": text }),
        )
        .await?;
        Ok(())
    }

    pub async fn clear_element(&self, element_id: &str) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(
            &format!("/session/{}/element/{}/clear", sid, element_id),
            &json!({}),
        )
        .await?;
        Ok(())
    }

    pub async fn execute_script(&self, script: &str, args: Vec<Value>) -> Result<Value, String> {
        let sid = self.session_id()?.to_string();
        let response = self
            .post(
                &format!("/session/{}/execute/sync", sid),
                &json!({ "script": script, "args": args }),
            )
            .await?;
        Ok(response.get("value").cloned().unwrap_or(Value::Null))
    }

    pub async fn screenshot(&self) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/screenshot", sid)).await?;
        Ok(response
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn get_cookies(&self) -> Result<Value, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/cookie", sid)).await?;
        Ok(response.get("value").cloned().unwrap_or(Value::Null))
    }

    pub async fn get_page_source(&self) -> Result<String, String> {
        let sid = self.session_id()?.to_string();
        let response = self.get(&format!("/session/{}/source", sid)).await?;
        Ok(response
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn back(&self) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(&format!("/session/{}/back", sid), &json!({}))
            .await?;
        Ok(())
    }

    pub async fn forward(&self) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(&format!("/session/{}/forward", sid), &json!({}))
            .await?;
        Ok(())
    }

    pub async fn refresh(&self) -> Result<(), String> {
        let sid = self.session_id()?.to_string();
        self.post(&format!("/session/{}/refresh", sid), &json!({}))
            .await?;
        Ok(())
    }

    pub fn session_id_pub(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn new_with_session(port: u16, session_id: String) -> Self {
        Self {
            base_url: format!("http://127.0.0.1:{}", port),
            session_id: Some(session_id),
        }
    }

    pub async fn execute_actions(&self, session_id: &str, actions: &Value) -> Result<(), String> {
        self.post(&format!("/session/{}/actions", session_id), actions)
            .await?;
        Ok(())
    }

    fn session_id(&self) -> Result<&str, String> {
        self.session_id
            .as_deref()
            .ok_or("No active WebDriver session".to_string())
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        http_request("GET", &format!("{}{}", self.base_url, path), None)
            .await?
            .into_value()
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        http_request("POST", &format!("{}{}", self.base_url, path), Some(body))
            .await?
            .into_value()
    }

    async fn delete(&self, path: &str) -> Result<Value, String> {
        http_request("DELETE", &format!("{}{}", self.base_url, path), None)
            .await?
            .into_value()
    }
}

/// Extract the element id from a WebDriver find-element `value` payload.
///
/// A genuine locator miss arrives as a WebDriver error payload
/// ("no such element"), not as a malformed response. It is translated to the
/// anchored locator-miss shape the rest of the CLI produces, so it keeps the
/// selector detail and receives the AI-friendly guidance that
/// `to_ai_friendly_error` reserves for locator misses. A payload with
/// neither an error nor an element id is genuinely malformed and keeps the
/// protocol-shaped message.
pub(crate) fn element_id_from_value(
    element_value: &Value,
    using: &str,
    value: &str,
) -> Result<String, String> {
    if element_value
        .get("error")
        .and_then(|e| e.as_str())
        .is_some_and(|e| e == "no such element")
    {
        return Err(format!("No element found by {} '{}'", using, value));
    }

    element_value
        .get("element-6066-11e4-a52e-4f735466cecf")
        .or_else(|| element_value.get("ELEMENT"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or("No element ID in response".to_string())
}

/// How long one WebDriver round trip may take before it fails loudly.
#[derive(Clone, Copy, Debug)]
enum Wait {
    /// Ordinary commands: a server that accepts and never answers is broken.
    Command,
    /// Navigation blocks on page load; W3C's default page-load timeout is
    /// 300s, so allow a little more and let the driver report its own timeout.
    PageLoad,
    /// POST /session can boot a simulator or real device (Appium) and take
    /// minutes; an abandoned request would orphan the session it creates.
    Unbounded,
}

impl Wait {
    fn for_request(method: &str, path: &str) -> Self {
        if method != "POST" {
            return Wait::Command;
        }
        if path == "/session" {
            return Wait::Unbounded;
        }
        let page_load = ["/url", "/back", "/forward", "/refresh"];
        if path.starts_with("/session/") && page_load.iter().any(|suffix| path.ends_with(suffix)) {
            return Wait::PageLoad;
        }
        Wait::Command
    }

    fn limit(self) -> Option<Duration> {
        match self {
            Wait::Command => Some(Duration::from_secs(60)),
            Wait::PageLoad => Some(Duration::from_secs(330)),
            Wait::Unbounded => None,
        }
    }
}

struct HttpResponse {
    status: u16,
    body: Value,
}

impl HttpResponse {
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn w3c_error(&self) -> Option<&str> {
        self.body
            .get("value")
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str)
    }

    /// The HTTP status decides success, never the payload shape: a script
    /// may legitimately return `{"error": ...}` as page data.
    fn into_value(self) -> Result<Value, String> {
        if self.is_success() {
            return Ok(self.body);
        }
        let message = self
            .body
            .get("value")
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        Err(match self.w3c_error() {
            Some(error) => format!("WebDriver {} (HTTP {}): {}", error, self.status, message),
            None => format!("WebDriver HTTP {}: {}", self.status, self.body),
        })
    }
}

fn decode_chunked(mut rest: &str) -> Result<String, String> {
    let mut out = String::new();
    loop {
        let (size_line, after) = rest.split_once("\r\n").unwrap_or((rest, ""));
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| format!("Malformed chunk size {:?}", size_line))?;
        if size == 0 {
            return Ok(out);
        }
        let chunk = after
            .get(..size)
            .ok_or("Truncated chunked WebDriver response")?;
        out.push_str(chunk);
        rest = after[size..].trim_start_matches("\r\n");
    }
}

fn parse_status_line(head: &str) -> Result<u16, String> {
    let line = head.lines().next().unwrap_or("");
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .filter(|_| line.starts_with("HTTP/"))
        .ok_or_else(|| format!("Malformed WebDriver HTTP status line: {:?}", line))
}

async fn http_request(
    method: &str,
    url: &str,
    body: Option<&Value>,
) -> Result<HttpResponse, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {}", e))?;
    let host = parsed.host_str().unwrap_or("127.0.0.1");
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path();

    let addr = format!("{}:{}", host, port);
    let stream = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .map_err(|_| format!("Connection timeout: {}", addr))?
    .map_err(|e| format!("Connection failed: {}", e))?;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body_str = body
        .map(|b| serde_json::to_string(b).unwrap_or_default())
        .unwrap_or_default();

    let request = if body.is_some() {
        format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            method, path, addr, body_str.len(), body_str
        )
    } else {
        format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            method, path, addr
        )
    };

    let mut stream = stream;
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("Write failed: {}", e))?;

    let mut response = Vec::new();
    let read = stream.read_to_end(&mut response);
    match Wait::for_request(method, path).limit() {
        Some(limit) => tokio::time::timeout(limit, read).await.map_err(|_| {
            format!(
                "WebDriver server at {} did not answer {} {} within {}s",
                addr,
                method,
                path,
                limit.as_secs()
            )
        })?,
        None => read.await,
    }
    .map_err(|e| format!("Read failed: {}", e))?;

    let response_str = String::from_utf8_lossy(&response);
    let (head, body_part) = response_str
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("Malformed WebDriver HTTP response from {} {}", method, path))?;
    let status = parse_status_line(head)?;
    let body_part = body_part.trim();

    let json_body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decode_chunked(body_part)?
    } else {
        body_part.to_string()
    };

    let body = if json_body.is_empty() {
        json!({})
    } else {
        serde_json::from_str(&json_body).map_err(|e| {
            format!(
                "Invalid JSON response: {} (body: {})",
                e,
                json_body.chars().take(100).collect::<String>()
            )
        })?
    };
    Ok(HttpResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_new() {
        let client = WebDriverClient::new(4444);
        assert_eq!(client.base_url, "http://127.0.0.1:4444");
        assert!(client.session_id.is_none());
    }

    #[test]
    fn test_session_id_none() {
        let client = WebDriverClient::new(4444);
        let result = client.session_id();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No active WebDriver session"));
    }

    #[test]
    fn test_client_custom_port() {
        let client = WebDriverClient::new(9515);
        assert_eq!(client.base_url, "http://127.0.0.1:9515");
    }

    /// In the WebDriver engine a genuine locator miss surfaces as a
    /// "no such element" error payload; it must translate to the anchored
    /// locator-miss shape (selector included) so `to_ai_friendly_error`
    /// appends its guidance, exactly as the CDP engine's misses do.
    #[test]
    fn test_find_element_miss_translates_to_locator_miss() {
        let payload = json!({
            "error": "no such element",
            "message": "An element could not be located on the page using the given search parameters.",
        });
        let err = element_id_from_value(&payload, "css selector", ".missing")
            .expect_err("an error payload is a miss, not an element");
        assert_eq!(err, "No element found by css selector '.missing'");
    }

    async fn serve_once(status_line: &'static str, body: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status_line,
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        port
    }

    /// A page script may return data shaped like an error; only the HTTP
    /// status says whether WebDriver failed.
    #[tokio::test]
    async fn test_script_result_shaped_like_an_error_is_data() {
        let port = serve_once(
            "HTTP/1.1 200 OK",
            r#"{"value":{"error":"not_found","message":"page data"}}"#,
        )
        .await;
        let client = WebDriverClient::new_with_session(port, "s".to_string());
        let value = client.execute_script("return x", vec![]).await.unwrap();
        assert_eq!(
            value,
            json!({ "error": "not_found", "message": "page data" })
        );
    }

    #[tokio::test]
    async fn test_http_error_status_fails_with_w3c_message() {
        let port = serve_once(
            "HTTP/1.1 500 Internal Server Error",
            r#"{"value":{"error":"javascript error","message":"boom"}}"#,
        )
        .await;
        let client = WebDriverClient::new_with_session(port, "s".to_string());
        let err = client.execute_script("throw 1", vec![]).await.unwrap_err();
        assert_eq!(err, "WebDriver javascript error (HTTP 500): boom");
    }

    #[tokio::test]
    async fn test_no_such_element_status_is_a_locator_miss() {
        let port = serve_once(
            "HTTP/1.1 404 Not Found",
            r#"{"value":{"error":"no such element","message":"gone"}}"#,
        )
        .await;
        let client = WebDriverClient::new_with_session(port, "s".to_string());
        let err = client.find_element("css selector", "#x").await.unwrap_err();
        assert_eq!(err, "No element found by css selector '#x'");
    }

    #[test]
    fn test_session_creation_and_navigation_are_not_capped_at_command_wait() {
        fn wait(m: &str, p: &str) -> Option<Duration> {
            Wait::for_request(m, p).limit()
        }
        assert_eq!(wait("POST", "/session"), None);
        for nav in ["url", "back", "forward", "refresh"] {
            let path = format!("/session/abc/{}", nav);
            assert!(
                wait("POST", &path).unwrap() > Duration::from_secs(300),
                "{}",
                path
            );
        }
        assert_eq!(
            wait("GET", "/session/abc/url"),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            wait("POST", "/session/abc/execute/sync"),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            wait("DELETE", "/session/abc"),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn test_chunked_body_is_decoded() {
        assert_eq!(
            decode_chunked("5\r\n{\"val\r\n6\r\nue\":1}\r\n0").unwrap(),
            "{\"value\":1}"
        );
        assert!(decode_chunked("zz\r\nabc").is_err());
        assert!(decode_chunked("10\r\nabc").is_err());
    }

    #[test]
    fn test_malformed_status_line_is_rejected() {
        assert_eq!(
            parse_status_line("HTTP/1.1 404 Not Found\r\nX: y").unwrap(),
            404
        );
        assert!(parse_status_line("garbage").is_err());
    }

    #[test]
    fn test_find_element_id_extracted_from_w3c_payload() {
        let payload = json!({ "element-6066-11e4-a52e-4f735466cecf": "abc123" });
        assert_eq!(
            element_id_from_value(&payload, "css selector", "#x").unwrap(),
            "abc123"
        );
    }

    /// A payload with neither an error nor an element id is genuinely
    /// malformed; it keeps the protocol-shaped message, which
    /// `to_ai_friendly_error` deliberately passes through unchanged.
    #[test]
    fn test_find_element_malformed_payload_keeps_protocol_message() {
        let payload = json!({ "unexpected": true });
        let err = element_id_from_value(&payload, "css selector", "#x")
            .expect_err("no id and no error is malformed");
        assert_eq!(err, "No element ID in response");
    }
}

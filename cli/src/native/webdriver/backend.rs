use async_trait::async_trait;
use serde_json::Value;

/// Abstract backend for browser automation. CDP (Chromium) and WebDriver
/// (Safari/iOS) share this interface so actions.rs can remain backend-agnostic
/// in the future.
// async_trait adds #[must_use] to methods returning an already must_use future.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    async fn navigate(&self, url: &str) -> Result<(), String>;
    async fn get_url(&self) -> Result<String, String>;
    async fn get_title(&self) -> Result<String, String>;
    async fn get_content(&self) -> Result<String, String>;
    async fn evaluate(&self, script: &str) -> Result<Value, String>;
    async fn screenshot(&self) -> Result<String, String>;
    async fn click(&self, selector: &str) -> Result<(), String>;
    async fn fill(&self, selector: &str, value: &str) -> Result<(), String>;
    async fn close(&mut self) -> Result<(), String>;
    async fn back(&self) -> Result<(), String>;
    async fn forward(&self) -> Result<(), String>;
    async fn reload(&self) -> Result<(), String>;
    async fn get_cookies(&self) -> Result<Value, String>;
    fn backend_type(&self) -> &str;

    fn supports(&self, feature: &str) -> bool {
        match feature {
            "navigate" | "evaluate" | "screenshot" | "click" | "fill" => true,
            "screencast" | "tracing" | "network_intercept" | "cdp" => self.backend_type() == "cdp",
            _ => false,
        }
    }

    fn unsupported_error(&self, action: &str) -> String {
        format!(
            "Action '{}' is not supported on the {} backend",
            action,
            self.backend_type()
        )
    }
}

/// WebDriver implementation of BrowserBackend
pub struct WebDriverBackend {
    client: super::client::WebDriverClient,
}

impl WebDriverBackend {
    pub fn new(client: super::client::WebDriverClient) -> Self {
        Self { client }
    }

    /// Evaluate `expression` like CDP Runtime.evaluate: the value of the last
    /// expression statement, with promises awaited. WebDriver's execute/sync
    /// runs a function body, so without this a bare expression returns null.
    /// A function-body script such as `return document.title` is therefore a
    /// SyntaxError here, exactly as it is on the CDP path.
    ///
    /// A single expression is compiled directly into the WebDriver script,
    /// which page CSP does not govern. Only scripts that are not one
    /// expression (several statements) go through `eval`, which a page CSP
    /// without 'unsafe-eval' blocks; that case fails with an explicit error.
    pub async fn evaluate_expression(&self, expression: &str) -> Result<Value, String> {
        let single = expression.trim().trim_end_matches(';');
        match self
            .client
            .execute_script(&format!("return (\n{}\n);", single), vec![])
            .await
        {
            Err(e) if e.contains("SyntaxError") => self
                .client
                .execute_script(
                    "return (0, eval)(arguments[0]);",
                    vec![Value::String(expression.to_string())],
                )
                .await
                .map_err(|e| {
                    if e.contains("unsafe-eval") {
                        format!(
                            "{}\nThis page's Content Security Policy blocks eval, so on WebDriver only a single expression can be evaluated here.",
                            e
                        )
                    } else {
                        e
                    }
                }),
            other => other,
        }
    }

    /// Run a function body (`arguments` available, `return` for the result).
    pub async fn execute_function_body(
        &self,
        body: &str,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        self.client.execute_script(body, args).await
    }

    /// Resolve a WebDriver element reference returned by a function body;
    /// `None` when the body returned null.
    pub async fn element_from_script(
        &self,
        body: &str,
        args: Vec<Value>,
    ) -> Result<Option<String>, String> {
        let value = self.client.execute_script(body, args).await?;
        if value.is_null() {
            return Ok(None);
        }
        super::client::element_id_from_value(&value, "script", body).map(Some)
    }

    pub async fn find_css(&self, selector: &str) -> Result<String, String> {
        self.client.find_element("css selector", selector).await
    }

    pub async fn click_element(&self, element_id: &str) -> Result<(), String> {
        self.client.click_element(element_id).await
    }

    pub async fn fill_element(&self, element_id: &str, value: &str) -> Result<(), String> {
        self.client.clear_element(element_id).await?;
        self.client.send_keys(element_id, value).await
    }
}

#[async_trait]
impl BrowserBackend for WebDriverBackend {
    async fn navigate(&self, url: &str) -> Result<(), String> {
        self.client.navigate(url).await
    }

    async fn get_url(&self) -> Result<String, String> {
        self.client.get_url().await
    }

    async fn get_title(&self) -> Result<String, String> {
        self.client.get_title().await
    }

    async fn get_content(&self) -> Result<String, String> {
        self.client.get_page_source().await
    }

    async fn evaluate(&self, script: &str) -> Result<Value, String> {
        self.client.execute_script(script, vec![]).await
    }

    async fn screenshot(&self) -> Result<String, String> {
        self.client.screenshot().await
    }

    async fn click(&self, selector: &str) -> Result<(), String> {
        let element_id = self.client.find_element("css selector", selector).await?;
        self.client.click_element(&element_id).await
    }

    async fn fill(&self, selector: &str, value: &str) -> Result<(), String> {
        let element_id = self.client.find_element("css selector", selector).await?;
        self.client.clear_element(&element_id).await?;
        self.client.send_keys(&element_id, value).await
    }

    async fn close(&mut self) -> Result<(), String> {
        self.client.delete_session().await
    }

    async fn back(&self) -> Result<(), String> {
        self.client.back().await
    }

    async fn forward(&self) -> Result<(), String> {
        self.client.forward().await
    }

    async fn reload(&self) -> Result<(), String> {
        self.client.refresh().await
    }

    async fn get_cookies(&self) -> Result<Value, String> {
        self.client.get_cookies().await
    }

    fn backend_type(&self) -> &str {
        "webdriver"
    }
}

/// CDP-backed backend constants for unsupported actions on WebDriver
pub const WEBDRIVER_UNSUPPORTED_ACTIONS: &[&str] = &[
    // axe-core frame injection currently relies on CDP execution contexts.
    "a11y",
    "screencast_start",
    "screencast_stop",
    "trace_start",
    "trace_stop",
    "profiler_start",
    "profiler_stop",
    "route",
    "unroute",
    "expose",
    "addscript",
    "addinitscript",
    "network",
    "har_start",
    "har_stop",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unsupported_actions() {
        assert!(WEBDRIVER_UNSUPPORTED_ACTIONS.contains(&"a11y"));
        assert!(WEBDRIVER_UNSUPPORTED_ACTIONS.contains(&"screencast_start"));
        assert!(WEBDRIVER_UNSUPPORTED_ACTIONS.contains(&"trace_start"));
        assert!(!WEBDRIVER_UNSUPPORTED_ACTIONS.contains(&"navigate"));
    }
}

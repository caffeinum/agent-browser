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
    /// Driver name used in messages ("webkit-cli", "safaridriver").
    label: String,
    /// What the driver is still loading after a navigation outlived its
    /// limit. Drivers such as webkit-cli answer nothing else until it ends.
    busy_loading: std::sync::Mutex<Option<String>>,
}

/// How long a command waits to learn whether a busy driver is free again.
const BUSY_PROBE: std::time::Duration = std::time::Duration::from_secs(1);

impl WebDriverBackend {
    pub fn new(client: super::client::WebDriverClient) -> Self {
        Self {
            client,
            label: "WebDriver server".to_string(),
            busy_loading: std::sync::Mutex::new(None),
        }
    }

    pub fn with_label(mut self, label: &str) -> Self {
        self.label = label.to_string();
        self
    }

    /// Fail fast while the driver is still loading a page that outlived its
    /// limit, instead of queueing behind it until the CLI gives up.
    pub async fn ensure_idle(&self) -> Result<(), String> {
        let Some(target) = self.busy_loading.lock().map(|b| b.clone()).unwrap_or(None) else {
            return Ok(());
        };
        match self.client.probe_url(BUSY_PROBE).await {
            Ok(_) => {
                if let Ok(mut busy) = self.busy_loading.lock() {
                    *busy = None;
                }
                Ok(())
            }
            Err(_) => Err(format!(
                "{} is still loading {}. Retry shortly, or run `agent-browser close` to abandon it",
                self.label, target
            )),
        }
    }

    /// Run a page-loading call; when it outlives its limit, remember what is
    /// loading and say so.
    async fn loading<F>(&self, target: String, call: F) -> Result<(), String>
    where
        F: std::future::Future<Output = Result<(), String>>,
    {
        self.ensure_idle().await?;
        match call.await {
            Err(e) if e.contains(super::client::NO_ANSWER) => {
                if let Ok(mut busy) = self.busy_loading.lock() {
                    *busy = Some(target.clone());
                }
                Err(format!(
                    "{} did not finish loading within {}s; {} is still loading it. Later commands fail until it finishes; `agent-browser close` abandons it",
                    target,
                    self.client.limits().page_load.as_secs_f32(),
                    self.label
                ))
            }
            other => other,
        }
    }

    /// Navigate with an explicit page-load limit (a command's --timeout).
    pub async fn navigate_within(
        &self,
        url: &str,
        limit: std::time::Duration,
    ) -> Result<(), String> {
        self.loading(url.to_string(), self.client.navigate_within(url, limit))
            .await
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

    /// A click may start a navigation the driver waits for, so it is bounded
    /// and tracked like one.
    pub async fn click_element(&self, element_id: &str) -> Result<(), String> {
        self.ensure_idle().await?;
        let target = match self.client.get_url().await {
            Ok(url) => format!("the page opened by a click on {}", url),
            Err(_) => "the page opened by a click".to_string(),
        };
        self.loading(target, self.client.click_element(element_id))
            .await
    }

    pub async fn fill_element(&self, element_id: &str, value: &str) -> Result<(), String> {
        self.client.clear_element(element_id).await?;
        self.client.send_keys(element_id, value).await
    }
}

#[async_trait]
impl BrowserBackend for WebDriverBackend {
    async fn navigate(&self, url: &str) -> Result<(), String> {
        self.loading(url.to_string(), self.client.navigate(url))
            .await
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
        self.click_element(&element_id).await
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
        self.loading("the previous page".to_string(), self.client.back())
            .await
    }

    async fn forward(&self) -> Result<(), String> {
        self.loading("the next page".to_string(), self.client.forward())
            .await
    }

    async fn reload(&self) -> Result<(), String> {
        self.loading("the reloaded page".to_string(), self.client.refresh())
            .await
    }

    async fn get_cookies(&self) -> Result<Value, String> {
        self.client.get_cookies().await
    }

    fn backend_type(&self) -> &str {
        "webdriver"
    }
}

/// Daemon actions implemented on WebDriver sessions, with the CLI command
/// each comes from. Everything else is refused up front by
/// [`unsupported_on_webdriver`] instead of failing deep in a CDP handler.
pub const WEBDRIVER_SUPPORTED_ACTIONS: &[(&str, &str)] = &[
    ("navigate", "open"),
    ("back", "back"),
    ("forward", "forward"),
    ("reload", "reload"),
    ("snapshot", "snapshot"),
    ("click", "click"),
    ("fill", "fill"),
    ("gettext", "get text"),
    ("url", "get url"),
    ("title", "get title"),
    ("content", "get html"),
    ("evaluate", "eval"),
    ("screenshot", "screenshot"),
    ("cookies_get", "cookies"),
];

/// iOS adds touch gestures through Appium.
const IOS_ONLY_ACTIONS: &[&str] = &["tap", "swipe", "device"];

/// `None` when `action` works on a WebDriver session for `provider`
/// ("webkit", "safari", "ios"); otherwise the error to return. Actions that
/// never touch the page (`skip_launch` ones) are not passed here.
pub fn unsupported_on_webdriver(action: &str, provider: &str) -> Option<String> {
    let supported = WEBDRIVER_SUPPORTED_ACTIONS
        .iter()
        .any(|(a, _)| *a == action)
        || (provider == "ios" && IOS_ONLY_ACTIONS.contains(&action));
    if supported {
        return None;
    }
    let commands: Vec<&str> = WEBDRIVER_SUPPORTED_ACTIONS
        .iter()
        .map(|(_, c)| *c)
        .collect();
    Some(format!(
        "{} is not supported on -p {} (WebDriver); supported: {}; workaround: eval",
        cli_command_name(action),
        provider,
        commands.join(", ")
    ))
}

/// The CLI spelling of a daemon action, for messages.
fn cli_command_name(action: &str) -> String {
    let known = match action {
        "network" | "requests" | "network_requests" => "network",
        "har_start" | "har_stop" => "network har",
        "route" | "unroute" => "network route",
        "trace_start" | "trace_stop" => "trace",
        "profiler_start" | "profiler_stop" => "profiler",
        "screencast_start" | "screencast_stop" => "screencast",
        "addscript" | "addinitscript" => "addscript",
        "tab_new" | "tab_list" | "tab_switch" | "tab_close" => "tab",
        "setviewport" | "viewport" => "set viewport",
        "keyboard" | "press" | "keydown" | "keyup" => "press",
        _ => "",
    };
    if known.is_empty() {
        action.replace('_', " ")
    } else {
        known.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supported_actions_pass_and_others_name_the_alternatives() {
        assert_eq!(unsupported_on_webdriver("navigate", "webkit"), None);
        assert_eq!(unsupported_on_webdriver("gettext", "safari"), None);
        let err = unsupported_on_webdriver("trace_start", "webkit").unwrap();
        assert!(
            err.starts_with(
                "trace is not supported on -p webkit (WebDriver); supported: open, back"
            ),
            "{}",
            err
        );
        assert!(err.ends_with("; workaround: eval"), "{}", err);
        assert!(unsupported_on_webdriver("tap", "webkit").is_some());
        assert_eq!(unsupported_on_webdriver("tap", "ios"), None);
    }
}

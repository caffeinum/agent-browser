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
    /// The session's W3C page-load timeout when the driver enforces one
    /// (webkit-cli). It always sits below the client's backstop.
    page_load_timeout: std::sync::Mutex<Option<std::time::Duration>>,
    /// The session uses pageLoadStrategy "none" and agent-browser waits for
    /// page readiness itself (see readiness.rs).
    own_page_load: bool,
}

/// A driver-enforced page-load timeout strictly below a client backstop, so
/// the driver reports the timeout and stays usable before the client gives up.
pub fn page_load_timeout_below(backstop: std::time::Duration) -> std::time::Duration {
    backstop * 4 / 5
}

/// One page-loading call, for messages.
pub struct Load {
    /// "navigation to https://x", "navigation started by a click on https://x".
    pub phrase: String,
    /// What a busy driver is still loading: "https://x", "the previous page".
    pub what: String,
}

/// How long a command waits to learn whether a busy driver is free again.
const BUSY_PROBE: std::time::Duration = std::time::Duration::from_secs(1);

impl WebDriverBackend {
    pub fn new(client: super::client::WebDriverClient) -> Self {
        Self {
            client,
            label: "WebDriver server".to_string(),
            busy_loading: std::sync::Mutex::new(None),
            page_load_timeout: std::sync::Mutex::new(None),
            own_page_load: false,
        }
    }

    /// The driver returns before pages load; agent-browser waits itself.
    pub fn with_own_page_load(mut self) -> Self {
        self.own_page_load = true;
        self
    }

    pub fn manages_page_load(&self) -> bool {
        self.own_page_load
    }

    /// How long agent-browser itself waits for a page, below `backstop`.
    fn own_wait(&self, backstop: std::time::Duration) -> std::time::Duration {
        self.current_page_load_timeout()
            .unwrap_or_else(|| page_load_timeout_below(backstop))
    }

    /// Record the page-load timeout the session was created with.
    pub fn with_page_load_timeout(self, timeout: std::time::Duration) -> Self {
        if let Ok(mut current) = self.page_load_timeout.lock() {
            *current = Some(timeout);
        }
        self
    }

    fn current_page_load_timeout(&self) -> Option<std::time::Duration> {
        self.page_load_timeout.lock().ok().and_then(|t| *t)
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
    async fn loading<F>(
        &self,
        load: Load,
        backstop: std::time::Duration,
        call: F,
    ) -> Result<(), String>
    where
        F: std::future::Future<Output = Result<(), String>>,
    {
        self.ensure_idle().await?;
        match call.await {
            // The driver enforced its page-load timeout and stays usable.
            Err(e) if e.starts_with("WebDriver timeout ") => {
                let limit = self.current_page_load_timeout().unwrap_or(backstop);
                Err(format!(
                    "{} did not finish within {}s; the page may be partly loaded",
                    capitalize(&load.phrase),
                    limit.as_secs_f32()
                ))
            }
            // The driver never answered: it is still loading and will answer
            // nothing else until that ends.
            Err(e) if e.contains(super::client::NO_ANSWER) => {
                if let Ok(mut busy) = self.busy_loading.lock() {
                    *busy = Some(load.what.clone());
                }
                Err(format!(
                    "{} did not finish within {}s; {} is still loading {}. Later commands fail until it finishes; `agent-browser close` abandons it",
                    capitalize(&load.phrase),
                    backstop.as_secs_f32(),
                    self.label,
                    load.what
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
        // Keep a driver-enforced page-load timeout below this call's backstop.
        if let Some(current) = self.current_page_load_timeout() {
            let wanted = page_load_timeout_below(limit);
            if wanted != current {
                self.ensure_idle().await?;
                self.client.set_timeouts(wanted).await?;
                if let Ok(mut t) = self.page_load_timeout.lock() {
                    *t = Some(wanted);
                }
            }
        }
        let load = Load {
            phrase: format!("navigation to {}", url),
            what: url.to_string(),
        };
        if self.own_page_load {
            let phrase = load.phrase.clone();
            return self
                .loading(
                    load,
                    limit,
                    self.navigate_and_wait(
                        &phrase,
                        super::readiness::Arrival::Url(url),
                        self.own_wait(limit),
                        self.client.navigate_within(url, limit),
                    ),
                )
                .await;
        }
        self.loading(load, limit, self.client.navigate_within(url, limit))
            .await
    }

    async fn history_step<F>(
        &self,
        load: Load,
        arrival: super::readiness::Arrival<'_>,
        call: F,
    ) -> Result<(), String>
    where
        F: std::future::Future<Output = Result<(), String>>,
    {
        let backstop = self.page_load_backstop();
        if self.own_page_load {
            let phrase = load.phrase.clone();
            return self
                .loading(
                    load,
                    backstop,
                    self.navigate_and_wait(&phrase, arrival, self.own_wait(backstop), call),
                )
                .await;
        }
        self.loading(load, backstop, call).await
    }

    fn page_load_backstop(&self) -> std::time::Duration {
        self.client.limits().page_load
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

    pub async fn send_keys(&self, element_id: &str, text: &str) -> Result<(), String> {
        self.client.send_keys(element_id, text).await
    }

    pub async fn clear_element(&self, element_id: &str) -> Result<(), String> {
        self.client.clear_element(element_id).await
    }

    /// W3C input actions (pointer and key sequences), released afterwards.
    pub async fn perform_actions(&self, actions: Value) -> Result<(), String> {
        let sid = self
            .client
            .session_id_pub()
            .ok_or("No active WebDriver session")?
            .to_string();
        self.client.execute_actions(&sid, &actions).await
    }

    /// The current URL (for results and polling).
    pub async fn get_url_now(&self) -> Result<String, String> {
        self.client.get_url().await
    }

    pub async fn find_css(&self, selector: &str) -> Result<String, String> {
        self.client.find_element("css selector", selector).await
    }

    /// A click may start a navigation the driver waits for, so it is bounded
    /// and tracked like one.
    pub async fn click_element(&self, element_id: &str) -> Result<(), String> {
        self.ensure_idle().await?;
        let from = self
            .client
            .get_url()
            .await
            .unwrap_or_else(|_| "the current page".to_string());
        let load = Load {
            phrase: format!("navigation started by a click on {}", from),
            what: format!("the page opened by a click on {}", from),
        };
        self.loading(
            load,
            self.client.limits().command,
            self.client.click_element(element_id),
        )
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
        self.navigate_within(url, self.page_load_backstop()).await
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
        let load = Load {
            phrase: "navigation back".to_string(),
            what: "the previous page".to_string(),
        };
        self.history_step(load, super::readiness::Arrival::History, self.client.back())
            .await
    }

    async fn forward(&self) -> Result<(), String> {
        let load = Load {
            phrase: "navigation forward".to_string(),
            what: "the next page".to_string(),
        };
        self.history_step(
            load,
            super::readiness::Arrival::History,
            self.client.forward(),
        )
        .await
    }

    async fn reload(&self) -> Result<(), String> {
        let load = Load {
            phrase: "reload".to_string(),
            what: "the reloaded page".to_string(),
        };
        self.history_step(
            load,
            super::readiness::Arrival::Reload,
            self.client.refresh(),
        )
        .await
    }

    async fn get_cookies(&self) -> Result<Value, String> {
        self.client.get_cookies().await
    }

    fn backend_type(&self) -> &str {
        "webdriver"
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
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
    // Internal (read without a URL); no CLI command of its own.
    ("content", ""),
    ("evaluate", "eval"),
    ("screenshot", "screenshot"),
    ("cookies_get", "cookies"),
];

/// Every action implemented on WebDriver sessions, with its CLI name.
pub fn all_supported() -> impl Iterator<Item = (&'static str, &'static str)> {
    WEBDRIVER_SUPPORTED_ACTIONS
        .iter()
        .chain(super::commands::ACTIONS.iter())
        .copied()
}

/// iOS adds touch gestures through Appium.
const IOS_ONLY_ACTIONS: &[&str] = &["tap", "swipe", "device"];

/// `None` when `action` works on a WebDriver session for `provider`
/// ("webkit", "safari", "ios"); otherwise the error to return. Actions that
/// never touch the page (`skip_launch` ones) are not passed here.
pub fn unsupported_on_webdriver(action: &str, provider: &str) -> Option<String> {
    if provider == "webkit" {
        if let Some(reason) = WEBKIT_CLI_INPUT_GAPS
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, r)| *r)
        {
            return Some(format!(
                "{} is not supported on -p webkit: {}; workaround: eval",
                command_name(action),
                reason
            ));
        }
    }
    let supported = all_supported().any(|(a, _)| a == action)
        || (provider == "ios" && IOS_ONLY_ACTIONS.contains(&action));
    if supported {
        return None;
    }
    let mut commands: Vec<&str> = all_supported()
        .filter(|(a, _)| provider != "webkit" || !WEBKIT_CLI_INPUT_GAPS.iter().any(|(g, _)| g == a))
        .map(|(_, c)| c)
        .filter(|c| !c.is_empty())
        .collect();
    commands.push("read");
    commands.push("close");
    let workaround = if NO_WORKAROUND.contains(&action) {
        "no workaround on WebDriver"
    } else {
        "workaround: eval"
    };
    Some(format!(
        "{} is not supported on -p {} (WebDriver); supported: {}; {}",
        command_name(action),
        provider,
        commands.join(", "),
        workaround
    ))
}

/// webkit-cli 0.5.0 synthesizes input in the page (isTrusted=false): a
/// pointer move sends no mouseover, a double press sends no dblclick, and
/// held modifiers are not applied. Commands that depend on those are refused
/// on webkit rather than reporting success the page never saw.
pub const WEBKIT_CLI_INPUT_GAPS: &[(&str, &str)] = &[
    (
        "hover",
        "webkit-cli does not send mouseover/mouseenter for pointer moves yet",
    ),
    (
        "dblclick",
        "webkit-cli does not send dblclick events for double presses yet",
    ),
];

/// Browser-level features WebDriver has no equivalent for.
const NO_WORKAROUND: &[&str] = &[
    "tab_list",
    "tab_new",
    "tab_switch",
    "tab_close",
    "window_new",
    "viewport",
    "set_media",
    "device",
    "user_agent",
    "timezone",
    "locale",
    "geolocation",
    "permissions",
    "offline",
    "headers",
    "upload",
    "pdf",
    "download",
    "waitfordownload",
    "console",
    "errors",
    "requests",
    "request_detail",
    "responsebody",
    "route",
    "unroute",
    "har_start",
    "har_stop",
    "trace_start",
    "trace_stop",
    "profiler_start",
    "profiler_stop",
    "screencast_start",
    "screencast_stop",
    "recording_start",
    "recording_stop",
    "recording_restart",
    "video_start",
    "video_stop",
    "dialog",
    "addinitscript",
    "expose",
    "state_save",
    "state_load",
    "clipboard",
    "bringtofront",
    "frame",
    "mainframe",
    "diff_screenshot",
    "pause",
];

/// The command an agent typed for a daemon action, for messages.
pub fn command_name(action: &str) -> String {
    let name = match action {
        "inputvalue" => "get value",
        "getattribute" => "get attr",
        "innerhtml" => "get html",
        "innertext" | "gettext" => "get text",
        "boundingbox" => "get box",
        "styles" => "get styles",
        "count" => "get count",
        "isvisible" => "is visible",
        "isenabled" => "is enabled",
        "ischecked" => "is checked",
        "waitforloadstate" => "wait --load",
        "waitforurl" => "wait --url",
        "waitforfunction" => "wait --fn",
        "waitfordownload" => "wait --download",
        "keyboard" => "keyboard type",
        "inserttext" => "keyboard inserttext",
        "keydown" => "keydown",
        "keyup" => "keyup",
        "getbyrole" | "getbytext" | "getbylabel" | "getbyplaceholder" | "getbyalttext"
        | "getbytitle" | "getbytestid" | "nth" => "find",
        "mousemove" | "mousedown" | "mouseup" | "mouse" => "mouse",
        "wheel" => "mouse wheel",
        "input_mouse" | "input_keyboard" | "input_touch" => "input",
        "tab_list" | "tab_new" | "tab_switch" | "tab_close" => "tab",
        "window_new" => "window new",
        "viewport" => "set viewport",
        "set_media" => "set media",
        "device" => "set device",
        "user_agent" => "set user-agent",
        "timezone" => "set timezone",
        "locale" => "set locale",
        "geolocation" => "set geo",
        "permissions" => "set permissions",
        "offline" => "set offline",
        "headers" => "set headers",
        "setcontent" => "set content",
        "setvalue" => "set value",
        "cookies_set" => "cookies set",
        "cookies_clear" => "cookies clear",
        "storage_get" | "storage_set" | "storage_clear" => "storage",
        "state_save" => "state save",
        "state_load" => "state load",
        "requests" | "request_detail" => "network requests",
        "responsebody" => "network response",
        "route" | "unroute" => "network route",
        "har_start" | "har_stop" => "network har",
        "trace_start" | "trace_stop" => "trace",
        "profiler_start" | "profiler_stop" => "profiler",
        "screencast_start" | "screencast_stop" => "screencast",
        "recording_start" | "recording_stop" | "recording_restart" | "video_start"
        | "video_stop" => "record",
        "diff_snapshot" => "diff snapshot",
        "diff_url" => "diff url",
        "diff_screenshot" => "diff screenshot",
        "evaluate" | "evalhandle" => "eval",
        "multiselect" => "select",
        "selectall" => "selectall",
        "mainframe" => "frame main",
        "navigate" => "open",
        other => return other.replace('_', " "),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supported_actions_pass_and_others_name_the_alternatives() {
        assert_eq!(unsupported_on_webdriver("navigate", "webkit"), None);
        assert_eq!(unsupported_on_webdriver("gettext", "safari"), None);
        assert_eq!(unsupported_on_webdriver("inputvalue", "webkit"), None);
        let err = unsupported_on_webdriver("trace_start", "webkit").unwrap();
        assert!(
            err.starts_with(
                "trace is not supported on -p webkit (WebDriver); supported: open, back"
            ),
            "{}",
            err
        );
        assert!(err.contains(", read, close;"), "{}", err);
        assert!(err.ends_with("; no workaround on WebDriver"), "{}", err);
        let err = unsupported_on_webdriver("getbyrole", "webkit").unwrap();
        assert!(err.starts_with("find is not supported"), "{}", err);
        assert!(err.ends_with("; workaround: eval"), "{}", err);
        let err = unsupported_on_webdriver("keyboard", "webkit").unwrap();
        assert!(err.starts_with("keyboard type is not supported"), "{}", err);
        assert!(unsupported_on_webdriver("tap", "webkit").is_some());
        assert_eq!(unsupported_on_webdriver("tap", "ios"), None);
    }

    #[test]
    fn webkit_refuses_input_webkit_cli_cannot_deliver_but_safari_keeps_it() {
        let err = unsupported_on_webdriver("hover", "webkit").unwrap();
        assert!(
            err.contains("webkit-cli does not send mouseover"),
            "{}",
            err
        );
        assert!(unsupported_on_webdriver("dblclick", "webkit")
            .unwrap()
            .contains("dblclick"));
        assert_eq!(unsupported_on_webdriver("hover", "safari"), None);
        assert_eq!(unsupported_on_webdriver("dblclick", "safari"), None);
        assert!(!unsupported_on_webdriver("tab_new", "webkit")
            .unwrap()
            .contains("hover"));
    }
}

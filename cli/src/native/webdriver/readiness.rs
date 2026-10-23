//! Page readiness for drivers run with pageLoadStrategy "none" (webkit-cli).
//!
//! WebDriver calls then return as soon as a navigation or click is
//! dispatched, so agent-browser waits for readiness itself:
//! - open/back/forward/reload wait until the new document exists and is past
//!   `readyState == "loading"`;
//! - every other page command first waits for a navigation the previous
//!   command started (seen through the Navigation API `navigate` event) to
//!   commit and reach DOMContentLoaded, so it never reads the old page.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::backend::WebDriverBackend;

const POLL: Duration = Duration::from_millis(50);

/// Installs (once per document) a watcher that records cross-document
/// navigations the page starts, and reports the page's state.
const WATCH_JS: &str = "var k = arguments[0], w = window[k], fresh = false;
if (!w) {
  fresh = true;
  w = { dest: null };
  Object.defineProperty(window, k, { value: w, enumerable: false, configurable: false });
  if (window.navigation) {
    navigation.addEventListener('navigate', function (e) {
      if (!e.destination.sameDocument && !e.downloadRequest) w.dest = e.destination.url;
    });
  }
  // A page restored from the back/forward cache is current again.
  addEventListener('pageshow', function () { w.dest = null; });
}
return { pending: w.dest, ready: document.readyState !== 'loading', fresh: fresh, url: location.href, origin: performance.timeOrigin };";

#[derive(Debug, Clone, PartialEq)]
struct PageState {
    pending: Option<String>,
    ready: bool,
    /// The watcher was installed by this call: a document not seen before.
    fresh: bool,
    url: String,
    origin: f64,
}

/// What a navigation call targets, to tell when it has arrived.
pub enum Arrival<'a> {
    /// `open <url>`: a new document, or the same document at a #fragment.
    Url(&'a str),
    /// back/forward: a new document, or a same-document history entry.
    History,
    /// reload: a new document.
    Reload,
}

impl WebDriverBackend {
    fn watch_key() -> String {
        format!("{}_nav", crate::native::snapshot::dom_registry_key())
    }

    async fn page_state(&self) -> Result<PageState, String> {
        let value = self
            .execute_function_body(WATCH_JS, vec![json!(Self::watch_key())])
            .await?;
        Ok(PageState {
            pending: value
                .get("pending")
                .and_then(Value::as_str)
                .map(String::from),
            ready: value.get("ready").and_then(Value::as_bool).unwrap_or(false),
            fresh: value.get("fresh").and_then(Value::as_bool).unwrap_or(false),
            url: value
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            origin: value.get("origin").and_then(Value::as_f64).unwrap_or(0.0),
        })
    }

    /// Before a page command: wait out a navigation the previous command
    /// started, and arm the watcher on the current document.
    pub async fn settle(&self, limit: Duration) -> Result<(), String> {
        if !self.manages_page_load() {
            return Ok(());
        }
        let deadline = Instant::now() + limit;
        let mut last: Option<PageState> = None;
        // Only a document this command is the first to see (one a previous
        // command's navigation just produced) must reach DOMContentLoaded; a
        // page already reported as slow is read as it is.
        let mut new_document = false;
        loop {
            // A document mid-swap can fail a script; keep polling.
            if let Ok(state) = self.page_state().await {
                new_document |= state.fresh;
                if state.pending.is_none() && (state.ready || !new_document) {
                    return Ok(());
                }
                last = Some(state);
            }
            if Instant::now() >= deadline {
                return Err(match last {
                    Some(PageState {
                        pending: Some(dest),
                        url,
                        ..
                    }) => format!(
                        "The navigation to {} started by the previous command did not commit within {}s; still on {}",
                        dest,
                        limit.as_secs_f32(),
                        url
                    ),
                    Some(state) => format!(
                        "{} did not finish loading within {}s",
                        state.url,
                        limit.as_secs_f32()
                    ),
                    None => format!(
                        "The page did not answer within {}s",
                        limit.as_secs_f32()
                    ),
                });
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Run a navigation call and wait until it has arrived and the new page
    /// is past DOMContentLoaded.
    pub(super) async fn navigate_and_wait<F>(
        &self,
        phrase: &str,
        arrival: Arrival<'_>,
        limit: Duration,
        call: F,
    ) -> Result<(), String>
    where
        F: std::future::Future<Output = Result<(), String>>,
    {
        let before = self.page_state().await?;
        call.await?;
        let deadline = Instant::now() + limit;
        let mut last = before.clone();
        loop {
            if let Ok(state) = self.page_state().await {
                if arrived(&before, &state, &arrival) && state.ready {
                    return Ok(());
                }
                last = state;
            }
            if Instant::now() >= deadline {
                let started = arrived(&before, &last, &arrival);
                return Err(if started {
                    format!(
                        "{} did not finish loading within {}s; the page may be partly loaded",
                        capitalize(phrase),
                        limit.as_secs_f32()
                    )
                } else {
                    format!(
                        "{} did not start within {}s; still on {}",
                        capitalize(phrase),
                        limit.as_secs_f32(),
                        last.url
                    )
                });
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

fn arrived(before: &PageState, now: &PageState, arrival: &Arrival<'_>) -> bool {
    let new_document = now.origin != before.origin;
    match arrival {
        Arrival::Reload => new_document,
        Arrival::History => new_document || now.url != before.url,
        Arrival::Url(target) => {
            new_document || (same_document_target(&before.url, target) && now.url == *target)
        }
    }
}

/// `open` of the current page with only a different #fragment stays in the
/// same document.
fn same_document_target(current: &str, target: &str) -> bool {
    let strip = |u: &str| u.split('#').next().unwrap_or(u).to_string();
    target.contains('#') && strip(current) == strip(target)
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(url: &str, origin: f64) -> PageState {
        PageState {
            pending: None,
            ready: true,
            fresh: false,
            url: url.to_string(),
            origin,
        }
    }

    #[test]
    fn arrival_needs_a_new_document_or_the_expected_same_document_change() {
        let before = state("https://a.test/x", 1.0);
        assert!(!arrived(
            &before,
            &state("https://a.test/x", 1.0),
            &Arrival::Url("https://b.test/")
        ));
        assert!(arrived(
            &before,
            &state("https://b.test/", 2.0),
            &Arrival::Url("https://b.test/")
        ));
        assert!(arrived(
            &before,
            &state("https://a.test/x#y", 1.0),
            &Arrival::Url("https://a.test/x#y")
        ));
        assert!(!arrived(
            &before,
            &state("https://a.test/x", 1.0),
            &Arrival::Reload
        ));
        assert!(arrived(
            &before,
            &state("https://a.test/x", 3.0),
            &Arrival::Reload
        ));
        assert!(arrived(
            &before,
            &state("https://a.test/prev", 1.0),
            &Arrival::History
        ));
    }
}

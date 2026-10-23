//! Page readiness for drivers run with pageLoadStrategy "none" (webkit-cli).
//!
//! WebDriver calls then return as soon as a navigation or click is
//! dispatched, so agent-browser waits for readiness itself:
//! - open/back/forward/reload wait until the new document exists and is past
//!   `readyState == "loading"`;
//! - every other page command first waits for a navigation the previous
//!   command started (seen through the Navigation API `navigate` event) to
//!   commit and reach DOMContentLoaded, so it never reads the old page.
//!
//! WebKit does not always cancel a navigation that a later one replaced, so an
//! abandoned navigation can still commit while a newer command waits. A
//! navigation command therefore accepts only a document whose navigation
//! began after the command dispatched it (`performance.timeOrigin` is the
//! navigation's start, not its commit), or a back/forward cache restore shown
//! after that.

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
  addEventListener('pageshow', function (e) {
    w.dest = null;
    if (e.persisted) w.shown = Date.now();
  });
}
var nav = performance.getEntriesByType ? performance.getEntriesByType('navigation')[0] : null;
return { pending: w.dest, ready: document.readyState !== 'loading', fresh: fresh, url: location.href,
  origin: performance.timeOrigin, since: w.shown || performance.timeOrigin, now: Date.now(),
  redirected: !!(nav && nav.redirectCount > 0) };";

#[derive(Debug, Clone, PartialEq)]
struct PageState {
    pending: Option<String>,
    ready: bool,
    /// The watcher was installed by this call: a document not seen before.
    fresh: bool,
    url: String,
    /// `performance.timeOrigin`: when this document's navigation started.
    origin: f64,
    /// When this document became current: its navigation start, or when it
    /// was restored from the back/forward cache.
    since: f64,
    /// The page's clock (`Date.now()`) when the state was read.
    now: f64,
    /// The navigation that produced this document followed a redirect.
    redirected: bool,
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
            since: value
                .get("since")
                .or_else(|| value.get("origin"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            now: value.get("now").and_then(Value::as_f64).unwrap_or(0.0),
            redirected: value
                .get("redirected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
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

/// Whether `now` is the page the navigation command (dispatched right after
/// `before` was read) produced.
fn arrived(before: &PageState, now: &PageState, arrival: &Arrival<'_>) -> bool {
    let same_document = now.origin == before.origin;
    // A different document counts only if it became current after this
    // command dispatched its navigation; one from an earlier, abandoned
    // navigation that commits late does not. 1ms absorbs clock rounding.
    let new_document = !same_document && now.since >= before.now - 1.0;
    match arrival {
        Arrival::Reload => new_document,
        Arrival::History => new_document || (same_document && now.url != before.url),
        Arrival::Url(target) => {
            if same_document {
                same_document_target(&before.url, target) && now.url == *target
            } else {
                // A redirect may land elsewhere, but a new document still at
                // the old URL without one is not the page that was asked for.
                new_document
                    && (same_url(&now.url, target) || now.redirected || now.url != before.url)
            }
        }
    }
}

fn same_url(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
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

    /// A document whose navigation started at `origin`, read at time 100.
    fn state(url: &str, origin: f64) -> PageState {
        PageState {
            pending: None,
            ready: true,
            fresh: false,
            url: url.to_string(),
            origin,
            since: origin,
            now: 100.0,
            redirected: false,
        }
    }

    /// The state read just before a navigation command dispatches, at `now`.
    fn before(url: &str, origin: f64, now: f64) -> PageState {
        PageState {
            now,
            ..state(url, origin)
        }
    }

    #[test]
    fn arrival_needs_a_new_document_or_the_expected_same_document_change() {
        let before = before("https://a.test/x", 1.0, 1.5);
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

    /// R4-1: a navigation abandoned before this command (started at 5; the
    /// command dispatched at 20) that commits during the wait is not the page
    /// open, back or reload asked for.
    #[test]
    fn a_late_commit_from_an_earlier_navigation_is_not_arrival() {
        let b = before("http://s.test/fast", 15.0, 20.0);
        let target = Arrival::Url("http://s.test/slow?s=30");
        let stale = state("http://s.test/slow?s=25", 5.0);
        assert!(!arrived(&b, &stale, &target));
        assert!(!arrived(
            &b,
            &stale,
            &Arrival::Url("http://s.test/slow?s=25")
        ));
        assert!(!arrived(&b, &stale, &Arrival::Reload));
        assert!(!arrived(&b, &stale, &Arrival::History));

        let ours = state("http://s.test/slow?s=30", 21.0);
        assert!(arrived(&b, &ours, &target));
        assert!(arrived(&b, &ours, &Arrival::Reload));

        // A new document still at the old URL, without a redirect, is not
        // the page asked for; with one it is.
        let same = state("http://s.test/fast", 21.0);
        assert!(!arrived(&b, &same, &target));
        let redirected = PageState {
            redirected: true,
            ..same.clone()
        };
        assert!(arrived(&b, &redirected, &target));
        // A redirect elsewhere arrives.
        assert!(arrived(&b, &state("http://s.test/login", 21.0), &target));

        // A back/forward cache restore shown after the command arrives.
        let restored = PageState {
            since: 22.0,
            ..state("http://s.test/prev", 2.0)
        };
        assert!(arrived(&b, &restored, &Arrival::History));

        // URLs compare after parsing.
        assert!(arrived(
            &before("http://s.test/", 15.0, 20.0),
            &state("http://s.test/", 21.0),
            &Arrival::Url("http://s.test")
        ));
    }
}

pub mod appium;
pub mod backend;
pub mod client;
pub mod commands;
pub mod dom_refs;
pub mod driver_process;
pub mod ios;
pub mod readiness;
pub mod safari;
pub mod types;
pub mod webkit;

/// How long typing `chars` characters may take on a WebDriver session: 20s
/// plus 20ms a character (webkit-cli 0.7.1 measured about 2ms a key). It is
/// the session script timeout for the call, at which webkit-cli stops input
/// and drops the keys it has not sent.
pub fn typing_budget_ms(chars: usize) -> u64 {
    20_000u64.saturating_add((chars as u64).saturating_mul(20))
}

/// How long agent-browser waits for a typing call to answer: the budget plus
/// a margin, so the driver reports its own timeout first. The CLI's read
/// timeout for type/fill/keyboard uses this too.
pub fn typing_request_ms(chars: usize) -> u64 {
    typing_budget_ms(chars).saturating_add(TYPING_MARGIN_MS)
}

/// How much longer than a typing budget agent-browser waits for an answer.
pub const TYPING_MARGIN_MS: u64 = 2_000;

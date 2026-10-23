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

/// How long typing `chars` characters key by key may take on a WebDriver
/// session: 20s plus 20ms a character (webkit-cli 0.6.0 measured 3-5ms).
/// The CLI's read timeout for type/fill/keyboard uses the same budget.
pub fn typing_budget_ms(chars: usize) -> u64 {
    20_000u64.saturating_add((chars as u64).saturating_mul(20))
}

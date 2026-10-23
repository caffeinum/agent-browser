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

/// How long one type/fill/keyboard call may wait on a WebDriver session: 65s
/// plus 20ms a character. The 65s base sits above webkit-cli 0.7.0's fixed
/// 60s input deadline, so webkit-cli stops typing, drops the keys it has not
/// sent and answers `timeout` before agent-browser gives up; a request
/// abandoned earlier would keep typing (measured). The CLI's read timeout for
/// these commands uses the same budget.
pub fn typing_budget_ms(chars: usize) -> u64 {
    65_000u64.saturating_add((chars as u64).saturating_mul(20))
}

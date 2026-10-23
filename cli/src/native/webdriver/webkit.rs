//! System WebKit through `webkit-cli webdriver`, a W3C WebDriver server that
//! drives headless WKWebView with persistent webkit-cli profiles ("accounts").
//!
//! This is a built-in provider rather than a `browser.provider` plugin
//! because plugins must hand back a CDP endpoint, and WebKit speaks WebDriver.

use std::env;
use std::path::{Path, PathBuf};

use super::driver_process::{DriverError, DriverProcess, Readiness, DEFAULT_READY_TIMEOUT};

pub const WEBKIT_CLI_ENV: &str = "AGENT_BROWSER_WEBKIT_CLI";
pub const WEBKIT_ACCOUNT_ENV: &str = "AGENT_BROWSER_WEBKIT_ACCOUNT";
const BINARY_NAME: &str = "webkit-cli";
const INSTALL_HINT: &str =
    "Install webkit-cli 0.6.0 or later (https://github.com/caffeinum/webkit-cli)";

/// Resolve the webkit-cli binary: `--executable-path`, then
/// `AGENT_BROWSER_WEBKIT_CLI`, then `webkit-cli` on PATH.
pub fn resolve_webkit_cli(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(path) = explicit.filter(|p| !p.is_empty()) {
        return existing(path, "--executable-path");
    }
    if let Some(path) = env::var(WEBKIT_CLI_ENV).ok().filter(|p| !p.is_empty()) {
        return existing(&path, WEBKIT_CLI_ENV);
    }
    find_on_path(BINARY_NAME, env::var_os("PATH").as_deref()).ok_or_else(|| {
        format!(
            "{} not found on PATH. {}, or point --executable-path or {} at it.",
            BINARY_NAME, INSTALL_HINT, WEBKIT_CLI_ENV
        )
    })
}

/// The webkit-cli account to drive, from `--webkit-account` (sent as
/// `webkitAccount`) or `AGENT_BROWSER_WEBKIT_ACCOUNT`. There is no default, so
/// a person's signed-in profile is never used by accident, and a Chrome
/// `--profile` never becomes the account.
pub fn resolve_account(explicit: Option<&str>) -> Result<String, String> {
    explicit
        .map(String::from)
        .or_else(|| env::var(WEBKIT_ACCOUNT_ENV).ok())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            format!(
                "The webkit provider needs a webkit-cli account: pass --webkit-account <name> or set {} (create one with `webkit-cli create <name>`).",
                WEBKIT_ACCOUNT_ENV
            )
        })
}

fn existing(path: &str, source: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(path);
    if p.is_file() {
        Ok(p)
    } else {
        Err(format!(
            "webkit-cli not found at {} (from {}). {}.",
            path, source, INSTALL_HINT
        ))
    }
}

fn find_on_path(name: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    env::split_paths(path_var?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

pub fn webdriver_args(account: &str, port: u16) -> Vec<String> {
    vec![
        "webdriver".to_string(),
        "--account".to_string(),
        account.to_string(),
        "--port".to_string(),
        port.to_string(),
    ]
}

/// Oldest webkit-cli with what the provider relies on: honored
/// pageLoadStrategy, session timeouts, and commands that keep working after
/// a navigation times out (0.5.0), and trusted input from the OS event path
/// with element handles kept out of the DOM (0.6.0).
pub const MIN_VERSION: (u64, u64, u64) = (0, 6, 0);

/// Parse `webkit-cli 0.5.0` (or a bare `0.5.0`).
pub fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    let token = output
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = token.split(['.', '-', '+']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Fail before launching when webkit-cli is older than [`MIN_VERSION`].
pub async fn require_supported_version(binary: &Path) -> Result<(), String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| format!("`{} --version` did not finish within 10s", binary.display()))?
    .map_err(|e| format!("Failed to run `{} --version`: {}", binary.display(), e))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (major, minor, patch) = MIN_VERSION;
    match parse_version(&text) {
        Some(found) if found >= MIN_VERSION => Ok(()),
        Some((a, b, c)) => Err(format!(
            "webkit-cli {}.{}.{} at {} is too old: -p webkit needs {}.{}.{} or later. {}.",
            a,
            b,
            c,
            binary.display(),
            major,
            minor,
            patch,
            INSTALL_HINT
        )),
        None => Err(format!(
            "Could not read the webkit-cli version from `{} --version` (got {:?}). -p webkit needs {}.{}.{} or later.",
            binary.display(),
            text.trim(),
            major,
            minor,
            patch
        )),
    }
}

/// `webkit-cli stop --account NAME`: ends the account's background session
/// (it outlives `webkit-cli webdriver` by design) so `close` leaves nothing
/// running.
pub fn stop_args(account: &str) -> Vec<String> {
    vec![
        "stop".to_string(),
        "--account".to_string(),
        account.to_string(),
    ]
}

/// Start `webkit-cli webdriver`. Port 0 lets webkit-cli choose; readiness is
/// its `{"account","port","url"}` stdout line either way.
pub async fn launch_webkit_cli(
    binary: &Path,
    account: &str,
    port: u16,
) -> Result<DriverProcess, String> {
    DriverProcess::spawn(
        "webkit",
        binary,
        &webdriver_args(account, port),
        Readiness::AnnouncedPort,
        DEFAULT_READY_TIMEOUT,
    )
    .await
    .map_err(|e| describe_launch_failure(e, binary))
}

/// webkit-cli's own message (e.g. "no account 'x' — create it with ...") is
/// the most precise explanation, so it is shown alone. The version hint is
/// added only when webkit-cli did not recognize the `webdriver` command.
fn describe_launch_failure(error: DriverError, binary: &Path) -> String {
    if !error.exited || error.output.is_empty() {
        return error.message;
    }
    let lower = error.output.to_lowercase();
    if lower.contains("unknown flag") || lower.contains("unknown command") {
        return format!(
            "{}\n{} does not support `webkit-cli webdriver`. {}.",
            error.output,
            binary.display(),
            INSTALL_HINT
        );
    }
    error.output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_webkit_cli_versions() {
        assert_eq!(parse_version("webkit-cli 0.5.0\n"), Some((0, 5, 0)));
        assert_eq!(parse_version("0.4.2"), Some((0, 4, 2)));
        assert_eq!(parse_version("webkit-cli 1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("webkit-cli"), None);
        assert!(parse_version("webkit-cli 0.4.2").unwrap() < MIN_VERSION);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn old_webkit_cli_is_refused_with_its_version() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(BINARY_NAME);
        std::fs::write(&bin, "#!/bin/sh\necho 'webkit-cli 0.5.0'\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = require_supported_version(&bin).await.unwrap_err();
        assert!(err.contains("webkit-cli 0.5.0"), "{}", err);
        assert!(err.contains("needs 0.6.0 or later"), "{}", err);

        std::fs::write(&bin, "#!/bin/sh\necho 'webkit-cli 0.6.0'\n").unwrap();
        require_supported_version(&bin).await.unwrap();
    }

    #[test]
    fn args_pass_account_and_port() {
        assert_eq!(
            webdriver_args("agent", 0),
            ["webdriver", "--account", "agent", "--port", "0"]
        );
    }

    #[test]
    fn missing_account_is_an_error() {
        let guard = crate::test_utils::EnvGuard::new(&[WEBKIT_ACCOUNT_ENV]);
        guard.remove(WEBKIT_ACCOUNT_ENV);
        let err = resolve_account(None).unwrap_err();
        assert!(err.contains("--webkit-account"), "got: {}", err);
        assert!(resolve_account(Some("")).is_err());
        assert_eq!(resolve_account(Some("agent")).unwrap(), "agent");
        guard.set(WEBKIT_ACCOUNT_ENV, "from-env");
        assert_eq!(resolve_account(None).unwrap(), "from-env");
        assert_eq!(resolve_account(Some("flag")).unwrap(), "flag");
    }

    #[test]
    fn explicit_path_must_exist() {
        let err = resolve_webkit_cli(Some("/nonexistent/webkit-cli")).unwrap_err();
        assert!(err.contains("/nonexistent/webkit-cli"), "got: {}", err);
        assert!(err.contains("--executable-path"), "got: {}", err);
        assert!(err.contains("Install webkit-cli"), "got: {}", err);
    }

    #[test]
    fn finds_binary_on_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(BINARY_NAME);
        std::fs::write(&bin, "").unwrap();
        let path_var = env::join_paths([Path::new("/nonexistent"), dir.path()]).unwrap();
        assert_eq!(find_on_path(BINARY_NAME, Some(&path_var)), Some(bin));
        assert_eq!(find_on_path(BINARY_NAME, None), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn webkit_cli_without_webdriver_fails_loudly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(BINARY_NAME);
        std::fs::write(
            &bin,
            "#!/bin/sh\necho 'webkit-cli: unknown flag --account (see --help)' >&2\nexit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = launch_webkit_cli(&bin, "test", 0)
            .await
            .err()
            .expect("old webkit-cli must fail");
        assert!(err.contains("unknown flag --account"), "got: {}", err);
        assert!(
            err.contains("does not support `webkit-cli webdriver`"),
            "got: {}",
            err
        );
        assert!(err.contains("0.6.0"), "got: {}", err);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_account_shows_only_webkit_clis_message() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(BINARY_NAME);
        let line =
            "webkit-cli: no account 'ghost' — create it with: webkit-cli create ghost (empty)";
        std::fs::write(&bin, format!("#!/bin/sh\necho \"{}\" >&2\nexit 1\n", line)).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = launch_webkit_cli(&bin, "ghost", 0).await.err().unwrap();
        assert_eq!(err, line);
    }
}

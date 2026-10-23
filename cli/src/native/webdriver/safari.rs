use std::path::PathBuf;
use std::process::Command;

use super::driver_process::{DriverProcess, DEFAULT_READY_TIMEOUT};

pub fn find_safaridriver() -> Option<PathBuf> {
    let candidates = ["/usr/bin/safaridriver"];

    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }

    // Try PATH
    if let Ok(output) = Command::new("which").arg("safaridriver").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }

    None
}

pub fn launch_safaridriver(port: u16) -> Result<DriverProcess, String> {
    let driver_path = find_safaridriver()
        .ok_or("safaridriver not found. Safari WebDriver requires macOS with Safari.")?;

    DriverProcess::spawn(
        "safaridriver",
        &driver_path,
        &["--port".to_string(), port.to_string()],
        port,
        DEFAULT_READY_TIMEOUT,
    )
    .map_err(|e| {
        format!(
            "{}\nIf safaridriver reports that remote automation is off, run `safaridriver --enable` once (or enable Develop > Allow Remote Automation in Safari).",
            e
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_safaridriver() {
        // Only check on macOS
        if cfg!(target_os = "macos") {
            let result = find_safaridriver();
            // Don't assert Some since it may not be enabled
            if let Some(path) = result {
                assert!(path.exists());
            }
        }
    }
}

//! A locally spawned WebDriver server (safaridriver, `webkit-cli webdriver`).
//!
//! The driver's stdout and stderr are drained into a bounded buffer so that a
//! driver that exits or never opens its port produces an error carrying its
//! own diagnostics instead of a bare "Connection refused".
//!
//! Waiting never blocks a tokio worker: readiness and shutdown poll with
//! `tokio::time::sleep`, and `Drop` hands any remaining wait to a detached
//! thread.

use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 8 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(15);

pub struct DriverProcess {
    /// `None` only after shutdown or drop has taken ownership of the child.
    child: Option<Child>,
    pub port: u16,
    name: String,
    output: Arc<Mutex<String>>,
}

impl DriverProcess {
    /// Spawn `program args...` and wait until it accepts TCP connections on
    /// `port`, it exits, or `ready_timeout` elapses.
    pub async fn spawn(
        name: &str,
        program: &Path,
        args: &[String],
        port: u16,
        ready_timeout: Duration,
    ) -> Result<Self, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to launch {} ({}): {}", name, program.display(), e))?;

        let output = Arc::new(Mutex::new(String::new()));
        if let Some(stdout) = child.stdout.take() {
            drain_into(stdout, Arc::clone(&output));
        }
        if let Some(stderr) = child.stderr.take() {
            drain_into(stderr, Arc::clone(&output));
        }

        let mut process = Self {
            child: Some(child),
            port,
            name: name.to_string(),
            output,
        };
        process.wait_until_ready(ready_timeout).await?;
        Ok(process)
    }

    async fn wait_until_ready(&mut self, timeout: Duration) -> Result<(), String> {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_exit_status()? {
                // Give the drain threads a moment to collect the final lines.
                tokio::time::sleep(POLL_INTERVAL).await;
                return Err(format!(
                    "{} exited ({}) before accepting connections on port {}{}",
                    self.name,
                    status,
                    self.port,
                    self.output_suffix()
                ));
            }
            let probe = tokio::time::timeout(POLL_INTERVAL, tokio::net::TcpStream::connect(addr));
            if matches!(probe.await, Ok(Ok(_))) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.shutdown().await;
                return Err(format!(
                    "{} did not accept connections on port {} within {}s{}",
                    self.name,
                    self.port,
                    timeout.as_secs_f32(),
                    self.output_suffix()
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    fn try_exit_status(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
        match self.child.as_mut() {
            Some(child) => child.try_wait().map_err(|e| e.to_string()),
            None => Err(format!("{} was already shut down", self.name)),
        }
    }

    /// `Some(message)` once the driver has exited on its own, so commands can
    /// say why the session is gone instead of failing with a connection error.
    pub fn exit_report(&mut self) -> Option<String> {
        let status = self.child.as_mut()?.try_wait().ok()??;
        Some(format!(
            "{} WebDriver driver exited ({}); run launch again{}",
            self.name,
            status,
            self.output_suffix()
        ))
    }

    /// Everything the driver has written to stdout/stderr so far (tail-bounded).
    pub fn output(&self) -> String {
        self.output.lock().map(|o| o.clone()).unwrap_or_default()
    }

    fn output_suffix(&self) -> String {
        let output = self.output();
        let trimmed = output.trim();
        if trimmed.is_empty() {
            " (no output)".to_string()
        } else {
            format!(". Driver output:\n{}", trimmed)
        }
    }

    /// SIGTERM first so drivers can persist state (webkit-cli saves cookies on
    /// exit), then SIGKILL if the driver does not exit in time.
    pub async fn shutdown(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        send_sigterm(&child);
        let deadline = Instant::now() + GRACEFUL_EXIT_TIMEOUT;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        let _ = child.kill();
        // SIGKILL has been sent, so this reap returns promptly.
        let _ = tokio::task::spawn_blocking(move || child.wait()).await;
    }
}

impl Drop for DriverProcess {
    /// Drop cannot await, so the graceful-exit wait moves to a detached thread.
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        send_sigterm(&child);
        std::thread::spawn(move || {
            let deadline = Instant::now() + GRACEFUL_EXIT_TIMEOUT;
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

fn send_sigterm(child: &Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = child;
}

fn drain_into<R: Read + Send + 'static>(mut reader: R, sink: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut out) = sink.lock() {
                        out.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if out.len() > OUTPUT_LIMIT {
                            let mut cut = out.len() - OUTPUT_LIMIT;
                            while !out.is_char_boundary(cut) {
                                cut += 1;
                            }
                            out.drain(..cut);
                        }
                    }
                }
            }
        }
    });
}

pub fn pick_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Failed to find free port: {}", e))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("Failed to get local address: {}", e))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sh(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn early_exit_surfaces_stderr() {
        let port = pick_free_port().unwrap();
        let err = DriverProcess::spawn(
            "fake-driver",
            Path::new("/bin/sh"),
            &sh("echo 'unknown flag --port' >&2; exit 2"),
            port,
            Duration::from_secs(5),
        )
        .await
        .err()
        .expect("driver that exits must fail");
        assert!(err.contains("fake-driver exited"), "got: {}", err);
        assert!(err.contains("unknown flag --port"), "got: {}", err);
    }

    #[tokio::test]
    async fn timeout_surfaces_output_and_kills_driver() {
        let port = pick_free_port().unwrap();
        let started = Instant::now();
        let err = DriverProcess::spawn(
            "silent-driver",
            Path::new("/bin/sh"),
            &sh("echo 'still booting' >&2; exec sleep 30"),
            port,
            Duration::from_millis(400),
        )
        .await
        .err()
        .expect("driver that never listens must time out");
        assert!(err.contains("did not accept connections"), "got: {}", err);
        assert!(err.contains("still booting"), "got: {}", err);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn ready_once_port_accepts_and_sigterm_stops_it() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let port = pick_free_port().unwrap();
        let script = format!(
            "sleep 0.3; exec python3 -c 'import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind((\"127.0.0.1\",{})); s.listen(); time.sleep(60)'",
            port
        );
        let mut driver = DriverProcess::spawn(
            "listening-driver",
            Path::new("/bin/sh"),
            &sh(&script),
            port,
            Duration::from_secs(10),
        )
        .await
        .expect("driver should become ready");
        let pid = driver.child.as_ref().unwrap().id() as i32;
        let started = Instant::now();
        driver.shutdown().await;
        assert!(!pid_alive(pid), "driver must be reaped after shutdown");
        assert!(
            started.elapsed() < GRACEFUL_EXIT_TIMEOUT,
            "SIGTERM should stop the driver without waiting for SIGKILL"
        );
    }

    /// On a single-threaded runtime, a blocking wait would starve every other
    /// task for the whole readiness window.
    #[tokio::test(flavor = "current_thread")]
    async fn waiting_for_readiness_does_not_block_the_runtime() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        let port = pick_free_port().unwrap();
        let _ = DriverProcess::spawn(
            "slow-driver",
            Path::new("/bin/sh"),
            &sh("exec sleep 30"),
            port,
            Duration::from_millis(500),
        )
        .await;
        ticker.abort();
        assert!(
            ticks.load(Ordering::SeqCst) >= 5,
            "other tasks should keep running while the driver boots"
        );
    }

    #[tokio::test]
    async fn drop_returns_immediately_and_still_kills_a_stubborn_driver() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let port = pick_free_port().unwrap();
        // Ignores SIGTERM, so only the SIGKILL fallback can stop it.
        let script = format!(
            "trap '' TERM; exec python3 -c 'import signal,socket,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind((\"127.0.0.1\",{})); s.listen(); time.sleep(60)'",
            port
        );
        let driver = DriverProcess::spawn(
            "stubborn-driver",
            Path::new("/bin/sh"),
            &sh(&script),
            port,
            Duration::from_secs(10),
        )
        .await
        .expect("driver should become ready");
        let pid = driver.child.as_ref().unwrap().id() as i32;
        let started = Instant::now();
        drop(driver);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "drop must not wait for the driver to exit"
        );
        let deadline = Instant::now() + GRACEFUL_EXIT_TIMEOUT + Duration::from_secs(3);
        while pid_alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        assert!(!pid_alive(pid), "SIGKILL fallback must stop the driver");
    }
}

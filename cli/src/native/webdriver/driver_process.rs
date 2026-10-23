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
/// Longest stdout line scanned for the readiness announcement.
const ANNOUNCE_LINE_LIMIT: usize = 4 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// How the driver signals that it is accepting connections.
pub enum Readiness {
    /// The driver was told to listen on this port; poll it.
    Port(u16),
    /// The driver prints one JSON line containing `"port"` on stdout once it
    /// is listening (`webkit-cli webdriver`).
    AnnouncedPort,
}

type PortSlot = Arc<Mutex<Option<u16>>>;

/// Why a driver never became ready. `output` is the driver's own
/// stdout/stderr, so callers can show it verbatim when it already explains
/// the failure (e.g. webkit-cli's "no account ... create it with").
#[derive(Debug)]
pub struct DriverError {
    pub message: String,
    pub output: String,
    /// The driver exited by itself (as opposed to failing to spawn or
    /// timing out).
    pub exited: bool,
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<DriverError> for String {
    fn from(error: DriverError) -> String {
        error.message
    }
}

pub struct DriverProcess {
    /// `None` only after shutdown or drop has taken ownership of the child.
    child: Option<Child>,
    pub port: u16,
    name: String,
    output: Arc<Mutex<String>>,
}

impl DriverProcess {
    /// Spawn `program args...` and wait until it is ready, it exits, or
    /// `ready_timeout` elapses.
    pub async fn spawn(
        name: &str,
        program: &Path,
        args: &[String],
        readiness: Readiness,
        ready_timeout: Duration,
    ) -> Result<Self, DriverError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| DriverError {
                message: format!("Failed to launch {} ({}): {}", name, program.display(), e),
                output: String::new(),
                exited: false,
            })?;

        let output = Arc::new(Mutex::new(String::new()));
        let announced: PortSlot = Arc::new(Mutex::new(None));
        let announce_sink =
            matches!(readiness, Readiness::AnnouncedPort).then(|| Arc::clone(&announced));
        if let Some(stdout) = child.stdout.take() {
            drain_into(stdout, Arc::clone(&output), announce_sink);
        }
        if let Some(stderr) = child.stderr.take() {
            drain_into(stderr, Arc::clone(&output), None);
        }

        let mut process = Self {
            child: Some(child),
            port: 0,
            name: name.to_string(),
            output,
        };
        match readiness {
            Readiness::Port(port) => {
                process.port = port;
                process.wait_until_ready(ready_timeout).await?;
            }
            Readiness::AnnouncedPort => {
                process
                    .wait_for_announcement(&announced, ready_timeout)
                    .await?
            }
        }
        Ok(process)
    }

    async fn wait_for_announcement(
        &mut self,
        announced: &PortSlot,
        timeout: Duration,
    ) -> Result<(), DriverError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(port) = announced.lock().ok().and_then(|slot| *slot) {
                self.port = port;
                return Ok(());
            }
            if let Some(status) = self.try_exit_status()? {
                tokio::time::sleep(POLL_INTERVAL).await;
                return Err(self.error(
                    true,
                    format!(
                        "{} exited ({}) before announcing its port",
                        self.name, status
                    ),
                ));
            }
            if Instant::now() >= deadline {
                self.shutdown().await;
                return Err(self.error(
                    false,
                    format!(
                        "{} did not announce a listening port within {}s",
                        self.name,
                        timeout.as_secs_f32()
                    ),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait_until_ready(&mut self, timeout: Duration) -> Result<(), DriverError> {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_exit_status()? {
                // Give the drain threads a moment to collect the final lines.
                tokio::time::sleep(POLL_INTERVAL).await;
                return Err(self.error(
                    true,
                    format!(
                        "{} exited ({}) before accepting connections on port {}",
                        self.name, status, self.port
                    ),
                ));
            }
            let probe = tokio::time::timeout(POLL_INTERVAL, tokio::net::TcpStream::connect(addr));
            if matches!(probe.await, Ok(Ok(_))) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.shutdown().await;
                return Err(self.error(
                    false,
                    format!(
                        "{} did not accept connections on port {} within {}s",
                        self.name,
                        self.port,
                        timeout.as_secs_f32()
                    ),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    fn try_exit_status(&mut self) -> Result<Option<std::process::ExitStatus>, DriverError> {
        let result = match self.child.as_mut() {
            Some(child) => child.try_wait().map_err(|e| e.to_string()),
            None => Err(format!("{} was already shut down", self.name)),
        };
        result.map_err(|message| self.error(false, message))
    }

    fn error(&self, exited: bool, message: String) -> DriverError {
        DriverError {
            message: format!("{}{}", message, self.output_suffix()),
            output: self.output().trim().to_string(),
            exited,
        }
    }

    /// `Some(message)` once the driver has exited on its own, so commands can
    /// say why the session is gone instead of failing with a connection error.
    pub fn exit_report(&mut self) -> Option<String> {
        let status = self.child.as_mut()?.try_wait().ok()??;
        Some(format!(
            "{} WebDriver driver exited ({}){}",
            self.name,
            status,
            self.output_suffix()
        ))
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
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

/// Port from a readiness line such as `{"account":"a","port":4444,"url":"..."}`.
fn announced_port(line: &str) -> Option<u16> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    u16::try_from(value.get("port")?.as_u64()?)
        .ok()
        .filter(|port| *port > 0)
}

/// Scans stdout lines for the readiness announcement. The partial line is
/// capped, and scanning stops once a port is found, so a chatty driver or one
/// that never prints a newline cannot grow memory or be parsed forever.
struct AnnounceScanner {
    slot: PortSlot,
    pending: String,
}

impl AnnounceScanner {
    /// Returns false once the port is known and scanning should stop.
    fn feed(&mut self, chunk: &str) -> bool {
        self.pending.push_str(chunk);
        while let Some(end) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=end).collect();
            if let Some(port) = announced_port(&line) {
                if let Ok(mut slot) = self.slot.lock() {
                    *slot = Some(port);
                }
                self.pending.clear();
                return false;
            }
        }
        if self.pending.len() > ANNOUNCE_LINE_LIMIT {
            self.pending.clear();
        }
        true
    }
}

fn drain_into<R: Read + Send + 'static>(
    mut reader: R,
    sink: Arc<Mutex<String>>,
    announce: Option<PortSlot>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut scanner = announce.map(|slot| AnnounceScanner {
            slot,
            pending: String::new(),
        });
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    if scanner.as_mut().is_some_and(|s| !s.feed(&chunk)) {
                        scanner = None;
                    }
                    if let Ok(mut out) = sink.lock() {
                        out.push_str(&chunk);
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

    #[test]
    fn parses_announced_port_line() {
        assert_eq!(
            announced_port(r#"{"account":"a","port":4444,"url":"http://127.0.0.1:4444"}"#),
            Some(4444)
        );
        assert_eq!(announced_port("listening"), None);
        assert_eq!(announced_port(r#"{"port":0}"#), None);
    }

    #[test]
    fn announce_scanner_caps_unterminated_output_and_stops_after_the_port() {
        let slot: PortSlot = Arc::new(Mutex::new(None));
        let mut scanner = AnnounceScanner {
            slot: Arc::clone(&slot),
            pending: String::new(),
        };
        let blob = "x".repeat(1024);
        for _ in 0..64 {
            assert!(scanner.feed(&blob));
            assert!(scanner.pending.len() <= ANNOUNCE_LINE_LIMIT + blob.len());
        }
        scanner.pending.clear();
        assert!(!scanner.feed("{\"port\":4567}\n{\"port\":1}\n"));
        assert_eq!(*slot.lock().unwrap(), Some(4567));
    }

    #[tokio::test]
    async fn announced_port_becomes_driver_port() {
        let mut driver = DriverProcess::spawn(
            "announcing-driver",
            Path::new("/bin/sh"),
            &sh("echo booting; echo '{\"port\":4567}'; exec sleep 30"),
            Readiness::AnnouncedPort,
            Duration::from_secs(5),
        )
        .await
        .expect("announced port should mark the driver ready");
        assert_eq!(driver.port, 4567);
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn missing_announcement_times_out_with_output() {
        let err = DriverProcess::spawn(
            "quiet-driver",
            Path::new("/bin/sh"),
            &sh("echo 'no json here'; exec sleep 30"),
            Readiness::AnnouncedPort,
            Duration::from_millis(400),
        )
        .await
        .err()
        .expect("driver without a readiness line must time out");
        assert!(err.message.contains("did not announce"), "got: {}", err);
        assert!(err.message.contains("no json here"), "got: {}", err);
    }

    #[tokio::test]
    async fn early_exit_surfaces_stderr() {
        let port = pick_free_port().unwrap();
        let err = DriverProcess::spawn(
            "fake-driver",
            Path::new("/bin/sh"),
            &sh("echo 'unknown flag --port' >&2; exit 2"),
            Readiness::Port(port),
            Duration::from_secs(5),
        )
        .await
        .err()
        .expect("driver that exits must fail");
        assert!(err.message.contains("fake-driver exited"), "got: {}", err);
        assert!(err.message.contains("unknown flag --port"), "got: {}", err);
    }

    #[tokio::test]
    async fn timeout_surfaces_output_and_kills_driver() {
        let port = pick_free_port().unwrap();
        let started = Instant::now();
        let err = DriverProcess::spawn(
            "silent-driver",
            Path::new("/bin/sh"),
            &sh("echo 'still booting' >&2; exec sleep 30"),
            Readiness::Port(port),
            Duration::from_millis(400),
        )
        .await
        .err()
        .expect("driver that never listens must time out");
        assert!(
            err.message.contains("did not accept connections"),
            "got: {}",
            err
        );
        assert!(err.message.contains("still booting"), "got: {}", err);
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
            Readiness::Port(port),
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
            Readiness::Port(port),
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
            Readiness::Port(port),
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

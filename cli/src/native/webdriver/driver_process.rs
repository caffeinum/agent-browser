//! A locally spawned WebDriver server (safaridriver, `webkit-cli webdriver`).
//!
//! The driver's stdout and stderr are drained into a bounded buffer so that a
//! driver that exits or never opens its port produces an error carrying its
//! own diagnostics instead of a bare "Connection refused".

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 8 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(15);

pub struct DriverProcess {
    child: Child,
    pub port: u16,
    name: String,
    output: Arc<Mutex<String>>,
}

impl DriverProcess {
    /// Spawn `program args...` and block until it accepts TCP connections on
    /// `port`, it exits, or `ready_timeout` elapses.
    pub fn spawn(
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
            child,
            port,
            name: name.to_string(),
            output,
        };
        process.wait_until_ready(ready_timeout)?;
        Ok(process)
    }

    fn wait_until_ready(&mut self, timeout: Duration) -> Result<(), String> {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
                // Give the drain threads a moment to collect the final lines.
                std::thread::sleep(POLL_INTERVAL);
                return Err(format!(
                    "{} exited ({}) before accepting connections on port {}{}",
                    self.name,
                    status,
                    self.port,
                    self.output_suffix()
                ));
            }
            if TcpStream::connect_timeout(&addr, POLL_INTERVAL).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.kill();
                return Err(format!(
                    "{} did not accept connections on port {} within {}s{}",
                    self.name,
                    self.port,
                    timeout.as_secs(),
                    self.output_suffix()
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
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
    pub fn kill(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        {
            if let Ok(pid) = i32::try_from(self.child.id()) {
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
            let deadline = Instant::now() + GRACEFUL_EXIT_TIMEOUT;
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DriverProcess {
    fn drop(&mut self) {
        self.kill();
    }
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

    fn sh(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    #[test]
    fn early_exit_surfaces_stderr() {
        let port = pick_free_port().unwrap();
        let err = DriverProcess::spawn(
            "fake-driver",
            Path::new("/bin/sh"),
            &sh("echo 'unknown flag --port' >&2; exit 2"),
            port,
            Duration::from_secs(5),
        )
        .err()
        .expect("driver that exits must fail");
        assert!(err.contains("fake-driver exited"), "got: {}", err);
        assert!(err.contains("unknown flag --port"), "got: {}", err);
    }

    #[test]
    fn timeout_surfaces_output_and_kills_driver() {
        let port = pick_free_port().unwrap();
        let started = Instant::now();
        let err = DriverProcess::spawn(
            "silent-driver",
            Path::new("/bin/sh"),
            &sh("echo 'still booting' >&2; exec sleep 30"),
            port,
            Duration::from_millis(400),
        )
        .err()
        .expect("driver that never listens must time out");
        assert!(err.contains("did not accept connections"), "got: {}", err);
        assert!(err.contains("still booting"), "got: {}", err);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn ready_once_port_accepts_and_sigterm_stops_it() {
        let port = pick_free_port().unwrap();
        let script = format!(
            "sleep 0.3; exec python3 -c 'import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind((\"127.0.0.1\",{})); s.listen(); time.sleep(60)'",
            port
        );
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let mut driver = DriverProcess::spawn(
            "listening-driver",
            Path::new("/bin/sh"),
            &sh(&script),
            port,
            Duration::from_secs(10),
        )
        .expect("driver should become ready");
        let started = Instant::now();
        driver.kill();
        assert!(matches!(driver.child.try_wait(), Ok(Some(_))));
        assert!(
            started.elapsed() < GRACEFUL_EXIT_TIMEOUT,
            "SIGTERM should stop the driver without waiting for SIGKILL"
        );
    }
}

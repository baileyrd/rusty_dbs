//! Integration tests for `dbs serve` (issues #75 flag wiring, #79 web
//! app skeleton, #81 auth gate).
//!
//! Every accepted bind — loopback, or non-loopback with `--token` now
//! that the auth gate (#81) actually enforces it — spawns the real
//! binary, polls the port until it answers, checks a real HTTP
//! response, then kills the child. The off-localhost-without-token
//! refusal and the `--allow-setup --no-setup` usage error still
//! quick-exit, since neither ever starts a server.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

/// `dbs serve` (issue #170) now loads a real `Config` at startup, same
/// as every other command — so every test spawning it for real needs a
/// valid `dbs.toml` to point `--config` at, in its own temp dir (unique
/// per test, since these run concurrently and each needs its own
/// `:memory:`-adjacent SQLite file path).
fn write_test_config(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("dbs-cli-serve-test-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("dbs.toml");
    std::fs::write(
        &config_path,
        "[dbs]\ndatabase = \"dbs.sqlite3\"\nexport_dir = \"exports\"\ndownload_root = \"downloads\"\n",
    )
    .unwrap();
    config_path
}

/// How long to wait for a spawned `dbs serve` to start accepting
/// connections or print its startup line. Every wait returns as soon as
/// the server is ready, so a generous limit costs nothing when it starts
/// quickly. (These tests failed intermittently in CI with "never
/// accepted a connection"; raising this from 5s did not stop that, so
/// [`Server`] now reports the server's exit status and stderr instead.)
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Kills the wrapped child on drop, so a failing assertion never leaks
/// a `dbs serve` process past the test that started it.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A `dbs serve` started for one test: killed on drop, with its stderr
/// captured to a file so a failure can say *why* the server isn't
/// answering instead of just "connection refused".
struct Server {
    child: Child,
    log: PathBuf,
    port: u16,
}

impl Server {
    /// Spawns `dbs serve --config <fresh config> <args...>` and remembers
    /// `port` (where it should listen) for [`Server::get`]. `args` carries
    /// every other flag, `--port` included, so the default-port test can
    /// leave it out.
    fn start(label: &str, port: u16, args: &[&str]) -> Self {
        let config = write_test_config(label);
        let log = config.with_file_name("serve-stderr.log");
        let stderr = std::fs::File::create(&log).unwrap();
        let child = Command::new(dbs_bin())
            .arg("serve")
            .arg("--config")
            .arg(&config)
            .args(args)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        Self { child, log, port }
    }

    /// The server's stderr so far, for failure messages.
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// A quick GET over a raw `TcpStream` (no HTTP client dependency
    /// needed just to check "is this the app skeleton answering"),
    /// retried until the server accepts. Fails at once if the server
    /// process has exited, reporting its status and stderr.
    fn get(&mut self, path: &str) -> String {
        let port = self.port;
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "dbs serve exited ({status}) before accepting on :{port}; stderr:
{}",
                    self.stderr()
                );
            }
            match get_once(port, path) {
                Ok(response) => return response,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!(
                    "dbs serve never accepted a connection on :{port}: {e}; stderr:
{}",
                    self.stderr()
                ),
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One GET attempt; `Err` while nothing is listening yet.
fn get_once(port: u16, path: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.write_all(
        format!(
            "GET {path} HTTP/1.1
Host: 127.0.0.1
Connection: close

"
        )
        .as_bytes(),
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(String::from_utf8_lossy(&response).into_owned())
}

#[test]
fn default_host_and_port_serve_the_app_skeleton() {
    let mut server = Server::start("default", 8000, &[]);

    let response = server.get("/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("<!DOCTYPE html"), "{response}");
}

#[test]
fn a_custom_port_is_actually_bound() {
    let mut server = Server::start("custom-port", 18123, &["--port", "18123"]);

    let response = server.get("/static/app.js");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("text/javascript"), "{response}");
}

#[test]
fn localhost_by_name_is_actually_bound() {
    let mut server = Server::start(
        "localhost-by-name",
        18124,
        &["--host", "localhost", "--port", "18124"],
    );

    let response = server.get("/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

/// Reads lines from `child`'s stderr (must be `Stdio::piped()`) on a
/// background thread until one contains `pattern` or `timeout` elapses.
fn wait_for_stderr_line(child: &mut Child, pattern: &str, timeout: Duration) -> bool {
    let mut stderr = child.stderr.take().expect("stderr not piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(&mut stderr);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains(pattern) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

#[test]
fn schedule_flag_is_reflected_in_the_report() {
    let mut child = Command::new(dbs_bin())
        .arg("serve")
        .arg("--port")
        .arg("18125")
        .arg("--schedule")
        .arg("--config")
        .arg(write_test_config("schedule"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(wait_for_stderr_line(
        &mut child,
        "scheduler",
        STARTUP_TIMEOUT
    ));
    let _guard = KillOnDrop(child);
}

#[test]
fn no_setup_flag_is_reflected_in_the_report() {
    let mut child = Command::new(dbs_bin())
        .arg("serve")
        .arg("--port")
        .arg("18126")
        .arg("--no-setup")
        .arg("--config")
        .arg(write_test_config("no-setup"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(wait_for_stderr_line(
        &mut child,
        "--no-setup noted",
        STARTUP_TIMEOUT
    ));
    let _guard = KillOnDrop(child);
}

#[test]
fn binding_off_localhost_without_a_token_is_refused() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--host")
        .arg("0.0.0.0")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Refusing to bind"), "{stderr}");
    assert!(stderr.contains("--token"), "{stderr}");
}

#[test]
fn binding_off_localhost_with_a_token_is_actually_served_and_the_token_is_enforced() {
    let mut server = Server::start(
        "token-enforced",
        18127,
        &["--host", "0.0.0.0", "--port", "18127", "--token", "secret"],
    );

    // The static frontend stays reachable without the token...
    let response = server.get("/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    // ...but an /api request without it is rejected.
    let response = server.get("/api/whatever");
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
}

#[test]
fn allow_setup_and_no_setup_together_is_a_usage_error() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--allow-setup")
        .arg("--no-setup")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
}

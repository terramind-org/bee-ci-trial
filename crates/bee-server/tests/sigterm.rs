//! SIGTERM and SIGINT stop the server cleanly.
//!
//! As PID 1 (every container) a process without a signal handler ignores SIGTERM, so an
//! orchestrator waited out its grace period and then killed it. A handled signal ends the process
//! with exit code 0 and a "stopping" line; an unhandled one ends it by signal, with no code.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The server process; whatever a failed assertion leaves running is killed and reaped.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start() -> (Server, BufReader<std::process::ChildStdout>) {
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_bee-server"))
            .args(["--dev", "--temp", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bee-server"),
    );
    let mut out = BufReader::new(server.0.stdout.take().expect("piped stdout"));
    let mut line = String::new();
    loop {
        line.clear();
        let n = out.read_line(&mut line).expect("read stdout");
        assert!(n > 0, "the server exited before it listened");
        if line.starts_with("bee-server listening on") {
            return (server, out);
        }
    }
}

fn stops_cleanly_on(signal: &str) {
    let (mut server, _stdout) = start();
    let sent = Command::new("kill")
        .args([&format!("-{signal}"), &server.0.id().to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success(), "kill -{signal} failed");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = server.0.try_wait().expect("try_wait") {
            break s;
        }
        assert!(
            Instant::now() <= deadline,
            "the server was still running 30 s after SIG{signal}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        status.code(),
        Some(0),
        "SIG{signal} should end the process through its handler (exit 0), got {status}"
    );
    let mut stderr = String::new();
    std::io::Read::read_to_string(
        &mut server.0.stderr.take().expect("piped stderr"),
        &mut stderr,
    )
    .expect("read stderr");
    assert!(
        stderr.contains("bee-server: stopping"),
        "stderr after SIG{signal}: {stderr}"
    );
}

#[test]
fn sigterm_stops_the_server_with_exit_zero() {
    stops_cleanly_on("TERM");
}

#[test]
fn sigint_stops_the_server_with_exit_zero() {
    stops_cleanly_on("INT");
}

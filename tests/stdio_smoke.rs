//! Live stdio smoke test (WP-10; T-CLI-01): spawn the real binary with
//! piped stdin/stdout under an isolated HOME, complete MCP `initialize`,
//! assert our server identity, then close stdin and expect a clean exit.
//! Proves the default (no-argument) CLI surface serves MCP over stdio backed
//! by the spawned daemon process — not an `Unimplemented` stub.

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Read one JSON-RPC line with a deadline (the child prints nothing else on
/// stdout; diagnostics go to stderr).
fn read_line(reader: &mut BufReader<std::process::ChildStdout>, what: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("stdout closed before {what}: {e}"));
        if !line.trim().is_empty() {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} from the child"
        );
    }
}

#[test]
fn stdio_initialize_serves_ltmrs_identity() {
    let home = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_ltmrs");
    let mut child = std::process::Command::new(bin)
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ltmrs binary");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));

    // MCP initialize (JSON-RPC, Content-Length framing is NOT used on stdio:
    // rmcp speaks newline-delimited JSON).
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0.0"}
        }
    });
    writeln!(stdin, "{}", init).unwrap();
    stdin.flush().unwrap();
    let resp = read_line(&mut reader, "initialize response");
    let v: serde_json::Value = serde_json::from_str(&resp).expect("valid JSON response");
    assert_eq!(v["id"], 1);
    assert_eq!(
        v["result"]["serverInfo"]["name"], "ltmrs",
        "server identity must be ltmrs, got: {v}"
    );

    // Complete the handshake, then close stdin: the server must exit cleanly.
    let notified = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(stdin, "{}", notified).unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let out = child.wait_timeout_or_kill(
        Duration::from_secs(60),
        "ltmrs did not exit after stdin EOF",
    );
    assert!(out.success(), "clean exit after stdin EOF, got: {}", out);
    // The managed home was created (spawned daemon opened its store).
    assert!(
        home.path().join(".ltmrs").join("store").exists(),
        "managed store must exist under isolated HOME"
    );
}

/// Stdout helper: wait with a deadline, then kill.
trait WaitTimeout {
    fn wait_timeout_or_kill(&mut self, dur: Duration, msg: &str) -> std::process::ExitStatus;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout_or_kill(&mut self, dur: Duration, msg: &str) -> std::process::ExitStatus {
        let deadline = Instant::now() + dur;
        loop {
            match self.try_wait().expect("try_wait") {
                Some(status) => return status,
                None if Instant::now() >= deadline => {
                    let _ = self.kill();
                    panic!("{msg}");
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

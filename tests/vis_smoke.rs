//! Live visualizer smoke test (WP-10 tasks 6-7): spawn the real binary with
//! `-vis -p PORT` under an isolated HOME, read the `serving at ... (pid ...)`
//! line from the parent's stdout, fetch `/` over TCP, then stop the reported
//! pid. Proves background detach (the parent exits while the child serves)
//! and loopback-only HTTP.
//!
//! NOTE: the parent's stdout is read explicitly — never `wait_with_output`:
//! the detached grandchild inherits stderr, which would hold the capture
//! pipes open forever.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Find a free loopback port (bind 0, read back, release; TOCTOU-acceptable
/// for a local smoke test — the child fails loudly if it loses the race).
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn get(port: u16, path: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                write!(stream, "GET {path} HTTP/1.1\r\nhost: smoke\r\n\r\n").unwrap();
                let mut out = String::new();
                stream.read_to_string(&mut out).unwrap();
                return out;
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("cannot connect to visualizer: {e}"),
        }
    }
}

/// Wait for the child to exit (it must: background parents are short-lived).
fn wait_exit(child: &mut std::process::Child, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "{what} must exit 0, got: {status}");
                return;
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("{what} did not exit in time");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
fn vis_background_detaches_and_serves() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ltmrs").join("store")).unwrap();
    let bin = env!("CARGO_BIN_EXE_ltmrs");
    let port = free_port();
    let mut child = std::process::Command::new(bin)
        .env("HOME", home.path())
        .arg("-vis")
        .arg("-p")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ltmrs -vis");

    // The parent prints exactly one line, then exits. Its stdout closes on
    // exit (the grandchild's stdout is /dev/null), so this cannot hang.
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .expect("read serving line");
    // Never wait on stderr: the detached grandchild inherits it.
    drop(child.stderr.take());
    wait_exit(&mut child, "background parent");
    assert!(
        first.contains(&format!("http://127.0.0.1:{port}/")),
        "parent reports the URL, got: {first}"
    );
    // The URL carries the per-boot access token (?token=...); the smoke
    // must present it (untokened requests are denied by design).
    let token_path = first
        .split_whitespace()
        .find(|w| w.contains("127.0.0.1") && w.contains("token="))
        .and_then(|url| url.split("127.0.0.1:").nth(1))
        .and_then(|rest| rest.split('/').nth(1))
        .map(|path| format!("/{path}"))
        .unwrap_or_else(|| panic!("parent reports a token URL, got: {first}"));
    let pid: u32 = first
        .split("(pid ")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("parent reports the child pid, got: {first}"));

    let index = get(port, &token_path);
    assert!(index.starts_with("HTTP/1.1 200"), "got: {index}");
    assert!(index.contains("ltmrs library"), "got: {index}");

    // Stop the detached child via the reported pid; the port must go quiet.
    let killed = unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0;
    assert!(killed, "child pid {pid} must be killable");
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "port must go quiet after killing the child"
    );
}

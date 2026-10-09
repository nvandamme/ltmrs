//! Independent-daemon lifecycle (re-review P1-4): the daemon runs as its
//! own process, so killing one frontend never stops service for others.
//!
//! - cold start: a bare frontend spawns the daemon and serves through it.
//! - owner SIGKILL: with an explicitly started daemon, killing the first
//!   frontend leaves the second fully usable (stateful write proves the
//!   daemon, not just the frontend, survived).
//! - foreground idle: `--daemon --daemon-idle-ms N` exits cleanly with no
//!   clients, bounding stray processes.
//! - SIGTERM (unix) / Ctrl-Break (Windows): the daemon shuts down
//!   gracefully (exit 0, bindings persisted).
//!
//! Every test uses an isolated HOME; no two tests share a store.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ltmrs")
}

/// The home-directory env var per platform (`HOME` unix, `USERPROFILE`
/// Windows) — same source of truth as the frontend. Spawn sites set literal
/// `HOME` alongside it: Windows honors HOME-first, so an ambient HOME must
/// never leak in and break test isolation.
const HOME_ENV: &str = ltmrs_frontend::frontend::serve::home_env_var();

/// A spawned stdio frontend with piped streams under an isolated HOME.
struct Frontend {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Frontend {
    fn spawn(home: &std::path::Path, extra_env: &[(&str, &str)]) -> Self {
        let mut cmd = std::process::Command::new(bin());
        cmd.env(HOME_ENV, home)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn ltmrs frontend");
        let stdin = child.stdin.take().expect("piped stdin");
        let reader = BufReader::new(child.stdout.take().expect("piped stdout"));
        Self {
            child,
            stdin: Some(stdin),
            reader,
            next_id: 1,
        }
    }

    fn rpc(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        // Skip server push-notifications (id-less, e.g. tools/list_changed
        // after mutating calls): the matching response carries our id.
        loop {
            let line = read_line(&mut self.reader, method);
            let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON response");
            if v.get("id").is_some() {
                assert_eq!(v["id"], id, "response id must match request");
                return v;
            }
        }
    }

    fn initialize(&mut self) {
        let resp = self.rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "lifecycle", "version": "0.0"},
            }),
        );
        assert_eq!(
            resp["result"]["serverInfo"]["name"], "ltmrs",
            "server identity must be ltmrs"
        );
        // A notification carries no id (JSON-RPC): the server sends no
        // reply, so the next response read belongs to the next request.
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        stdin.flush().unwrap();
    }

    /// A stateful write through the daemon: proves the daemon (not just
    /// the frontend process) is serving.
    fn memory_add(&mut self, fragment: &str) -> serde_json::Value {
        let resp = self.rpc(
            "tools/call",
            serde_json::json!({
                "name": "memory_add",
                "arguments": {"fragment": fragment},
            }),
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("Added fragment"),
            "stateful write must succeed, got: {resp}"
        );
        resp
    }

    fn close_stdin(&mut self) {
        // Closing the pipe ends the frontend's stdio serving.
        drop(self.stdin.take());
    }

    /// Close stdio and reap the frontend; it must exit cleanly.
    fn shutdown(mut self) {
        self.close_stdin();
        let status = wait_timeout_or_kill(&mut self.child, Duration::from_secs(60));
        assert!(status.success(), "frontend must exit 0, got: {status}");
    }
}

/// Read one JSON-RPC line with a deadline.
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

/// The managed daemon IPC endpoint for an isolated HOME (mirrors the
/// daemon layout: `RuntimePaths::resolve(home/.ltmrs, "daemon")`). On unix
/// a socket file; on Windows a named-pipe name (the pipe namespace answers
/// `exists()` while instances are live).
fn managed_socket(home: &std::path::Path) -> PathBuf {
    ltmrs_daemon::runtime::RuntimePaths::resolve(&home.join(".ltmrs"), "daemon").endpoint
}

/// Wait until a path exists (bounded), then wait until it is gone.
fn wait_for_path(path: &std::path::Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Cold start: with an empty HOME, a bare frontend spawns the daemon
/// itself and serves statefully through it (no manual daemon step).
#[test]
fn cold_start_frontend_spawns_daemon_and_serves() {
    let home = tempfile::tempdir().unwrap();
    let mut front = Frontend::spawn(home.path(), &[("LTMRS_DAEMON_IDLE_MS", "1500")]);
    front.initialize();
    front.memory_add("## Cold start\n\n### Context\nSpawned daemon serves.");
    // The daemon owns its store under the isolated HOME.
    assert!(
        home.path().join(".ltmrs").join("store").exists(),
        "managed store must exist under isolated HOME"
    );
    // Detached child diagnostics land in the runtime log, never /dev/null.
    assert!(
        home.path()
            .join(".ltmrs")
            .join("ltmrs")
            .join("daemon")
            .join("daemon.log")
            .exists(),
        "daemon log must exist under isolated HOME"
    );
    // Clean shutdown: closing our stdio ends the frontend; the idle daemon
    // (1.5s budget) must then exit on its own — no strays.
    front.shutdown();
    // Idle daemon (1.5s budget) exits on its own — no strays.
    wait_for_gone(&managed_socket(home.path()), "daemon socket after idle");
}

/// Owner SIGKILL: the daemon never belonged to the first frontend, so
/// killing it cannot stop the second frontend's stateful work.
#[test]
fn peer_survives_owner_sigkill() {
    let home = tempfile::tempdir().unwrap();
    // Explicit short-idle daemon: deterministic teardown, zero strays.
    let mut daemon = std::process::Command::new(bin())
        .env(HOME_ENV, home.path())
        .env("HOME", home.path())
        .arg("daemon")
        .arg("--daemon-idle-ms")
        .arg("60000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ltmrs daemon");
    wait_for_path(&managed_socket(home.path()), "daemon socket");

    let mut first = Frontend::spawn(home.path(), &[]);
    first.initialize();
    let mut second = Frontend::spawn(home.path(), &[]);
    second.initialize();

    // SIGKILL the first frontend (host kills are not graceful disconnects).
    first.child.kill().expect("SIGKILL first frontend");
    let status = first.child.wait().expect("reap first frontend");
    assert!(!status.success(), "killed frontend must not exit 0");

    // The second frontend performs stateful work through the same daemon.
    second.memory_add("## Survivor write\n\n### Context\nOwner is dead.");
    second.shutdown();

    // Teardown: the daemon outlives both frontends by design; stop it.
    daemon.kill().expect("stop daemon");
    let _ = daemon.wait();
}

/// Simultaneous cold start: two frontends racing on an empty HOME both
/// end up serving (one spawns the daemon, the other attaches to the
/// winner); nobody fails with AlreadyRunning.
#[test]
fn simultaneous_cold_start_serves_both_frontends() {
    let home = tempfile::tempdir().unwrap();
    let env = [("LTMRS_DAEMON_IDLE_MS", "1500")];
    let mut first = Frontend::spawn(home.path(), &env);
    let mut second = Frontend::spawn(home.path(), &env);
    // Initialize concurrently so both frontends race through daemon
    // startup (probe-miss, spawn, attach) at the same time.
    std::thread::scope(|s| {
        s.spawn(|| first.initialize());
        s.spawn(|| second.initialize());
    });
    first.memory_add("## Racer one\n\n### Context\nFirst writer wins nothing.");
    second.memory_add("## Racer two\n\n### Context\nSecond writer served.");
    first.shutdown();
    second.shutdown();
    // Idle daemon (1.5s budget) exits on its own — no strays.
    wait_for_gone(&managed_socket(home.path()), "daemon socket after idle");
}

/// Bare `daemon` is idempotent: running it twice serves and exits 0 both
/// times (second call attaches to the live daemon, spawns nothing).
#[test]
fn bare_daemon_command_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let out = std::process::Command::new(bin())
            .env(HOME_ENV, home.path())
            .env("HOME", home.path())
            .env("LTMRS_DAEMON_IDLE_MS", "1500")
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run ltmrs daemon");
        assert!(
            out.status.success(),
            "bare daemon must exit 0, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            stdout.contains("daemon serving at"),
            "must report the socket, got: {stdout}"
        );
    }
    wait_for_gone(&managed_socket(home.path()), "daemon socket after idle");
}

/// Foreground daemon with no clients exits on its idle budget (clean,
///
/// exit 0): bounded stray processes without any explicit stop command.
#[test]
fn foreground_daemon_idle_exits_cleanly() {
    let home = tempfile::tempdir().unwrap();
    let mut daemon = std::process::Command::new(bin())
        .env(HOME_ENV, home.path())
        .env("HOME", home.path())
        .arg("daemon")
        .arg("--foreground")
        .arg("--daemon-idle-ms")
        .arg("500")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn foreground daemon");
    let status = wait_timeout_or_kill(&mut daemon, Duration::from_secs(30));
    assert!(status.success(), "idle daemon must exit 0, got: {status}");
}

/// Serve-forever daemon survives quiet windows: with `--daemon-idle-ms 0`
/// a full connect-data gate with zero clients must not error the accept
/// loop out (Windows `WouldBlock` regression test) — and the daemon must
/// still be serving afterwards.
#[test]
fn foreground_daemon_without_idle_budget_serves_across_quiet_windows() {
    /// A serve-forever daemon never exits alone: guarantee no stray on
    /// any failure path below (disarmed before the explicit stop).
    struct KillGuard<'a> {
        child: Option<&'a mut std::process::Child>,
    }
    impl Drop for KillGuard<'_> {
        fn drop(&mut self) {
            if let Some(child) = self.child.take() {
                let _ = child.kill();
            }
        }
    }
    let home = tempfile::tempdir().unwrap();
    let mut daemon = std::process::Command::new(bin())
        .env(HOME_ENV, home.path())
        .env("HOME", home.path())
        .arg("daemon")
        .arg("--foreground")
        .arg("--daemon-idle-ms")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foreground daemon");
    let mut guard = KillGuard {
        child: Some(&mut daemon),
    };
    wait_for_path(&managed_socket(home.path()), "daemon endpoint");
    // A full 1000ms data-gate window with zero clients attached.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        guard
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .expect("try_wait")
            .is_none(),
        "serve-forever daemon must survive quiet windows"
    );
    // Still serving: a frontend attaches and initializes through it.
    let mut front = Frontend::spawn(home.path(), &[]);
    front.initialize();
    front.shutdown();
    // Serve-forever never exits on its own: stop it explicitly (disarmed
    // first so the guard does not double-kill).
    let child = guard.child.take().unwrap();
    child.kill().expect("stop daemon");
    let _ = child.wait();
}

/// SIGTERM shuts the daemon down gracefully: exit 0 and bindings persisted
/// (a kill -9 would leave no file behind to check).
#[cfg(unix)]
#[test]
fn foreground_daemon_sigterm_shuts_down_gracefully() {
    let home = tempfile::tempdir().unwrap();
    let mut daemon = std::process::Command::new(bin())
        .env(HOME_ENV, home.path())
        .env("HOME", home.path())
        .arg("daemon")
        .arg("--foreground")
        .arg("--daemon-idle-ms")
        .arg("60000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foreground daemon");
    wait_for_path(&managed_socket(home.path()), "daemon socket");
    let pid = daemon.id();
    unsafe {
        assert_eq!(libc::kill(pid as i32, libc::SIGTERM), 0);
    }
    let status = wait_timeout_or_kill(&mut daemon, Duration::from_secs(30));
    assert!(status.success(), "SIGTERM must exit 0, got: {status}");
    assert!(
        home.path().join(".ltmrs").join("sessions.json").exists(),
        "graceful shutdown must persist bindings"
    );
    wait_for_gone(
        &managed_socket(home.path()),
        "daemon socket after graceful shutdown",
    );
    let _ = daemon.wait();
}

/// Ctrl-Break shuts the daemon down gracefully on Windows: exit 0 and
/// bindings persisted. NOT RUN under `cargo test` on this conhost:
/// group-0 events kill cargo (cargo registers no handler), and targeted
/// delivery to the daemon's own process group is delayed tens of seconds
/// and hangs `try_wait` after the daemon exits — both verified by probe
/// (2026-10-08, deviation ledger). Handler registration is pinned
/// executably in `serve::tests::shutdown_handlers_register`; the
/// foreground Ctrl-C path is proven clean by probe in one console group.
#[cfg(windows)]
#[ignore = "conhost blocks programmatic delivery under cargo test (see doc comment)"]
#[test]
fn foreground_daemon_ctrlbreak_shuts_down_gracefully() {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0020;
    const CTRL_BREAK_EVENT: u32 = 1;
    unsafe extern "system" fn pass_through(_ty: u32) -> i32 {
        0
    }
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(pass_through), 1);
    }
    let home = tempfile::tempdir().unwrap();
    let mut daemon = std::process::Command::new(bin())
        .env(HOME_ENV, home.path())
        .env("HOME", home.path())
        .arg("daemon")
        .arg("--foreground")
        .arg("--daemon-idle-ms")
        .arg("60000")
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foreground daemon");
    wait_for_path(&managed_socket(home.path()), "daemon endpoint");
    unsafe {
        windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0);
    }
    let status = wait_timeout_or_kill(&mut daemon, Duration::from_secs(30));
    assert!(status.success(), "Ctrl-Break must exit 0, got: {status}");
    assert!(
        home.path().join(".ltmrs").join("sessions.json").exists(),
        "graceful shutdown must persist bindings"
    );
    wait_for_gone(
        &managed_socket(home.path()),
        "daemon endpoint after graceful shutdown",
    );
    let _ = daemon.wait();
}

/// Wait until a path disappears (bounded).
fn wait_for_gone(path: &std::path::Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} to go"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait with a deadline, then kill.
fn wait_timeout_or_kill(
    child: &mut std::process::Child,
    dur: Duration,
) -> std::process::ExitStatus {
    let deadline = Instant::now() + dur;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("timed out waiting for child exit");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

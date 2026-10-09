# Windows Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render ltmrs fully Windows-compatible: certified named-pipe IPC, LockFileEx singleton, DACL peer auth, detached spawn, Ctrl-break shutdown, `.cmd` shim, and visualizer conformance against upstream Lemma 0.21.0.

**Architecture:** Transport abstraction over `AsyncRead + AsyncWrite` with platform dispatch in the owning crates (`ltmrs-daemon` owns IPC/lock/auth, `ltmrs-frontend` owns spawn/home/shim, root binary's visualizer calls the shared spawn helper). Framing/envelope unchanged.

**Tech Stack:** Rust edition 2024, tokio (`os::windows::named_pipe`, `signal::windows`), `windows-sys` (target-conditional), `sha2`, SDDL.

**Spec:** `plans/2026-10-08-windows-compat-design.md` (read it with this plan; all deviations and the visualizer conformance table are defined there).

## Global Constraints

- Edition 2024, toolchain 1.99.0 pinned; `cargo fmt -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` must pass on all touched code.
- Fast iteration only: `cargo check -p <crate> --all-targets` then `cargo test -p <crate> --lib`. Never bare `cargo test` or `--workspace` for iteration. `PROTOC` env must be set for lance builds: `PROTOC=C:\Users\Antoine\AppData\Local\Microsoft\WinGet\Packages\Google.Protobuf_Microsoft.Winget.Source_8wekyb3d8bbwe\bin\protoc.exe`.
- `windows-sys` is target-conditional: `[target.'cfg(windows)'.dependencies] windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_Security", "Win32_Storage_FileSystem", "Win32_System_Threading", "Win32_System_Console"] }` in `crates/daemon/Cargo.toml` and `crates/frontend/Cargo.toml`; dev-dependency in root `Cargo.toml` (tests) with the same features, called out in the manifest per AGENTS rule 5.
- No comments unless explaining invariants/contracts/protocol. No backward-compat shims. `unsafe` needs a documented safety argument.
- Commits per AGENTS commit workflow (DONE.md → CHANGELOG.md date heading with SHA placeholder, `--no-pager`), conventional `fix:`/`feat:` tags, one coherent commit per task.
- Deterministic async tests via `#[tokio::test(start_paused = true)]` where time is involved.
- Every plan/ledger statement distinguishes planned vs executed; never claim unrun evidence.

## Review Focus

- Two Windows users with the same store identity must not collide on the pipe name → user-SID token in the endpoint (Task 1 test).
- A live pipe endpoint must reject a second daemon exactly like flock → LockFileEx exclusivity + probe tests (Task 1).
- DACL over-permission (granting Everyone/SYSTEM-only) is a security fail → ACE content test pins exactly user + SYSTEM (Task 1).
- Detached children must survive parent Ctrl-C → `CREATE_NEW_PROCESS_GROUP`; graceful shutdown via `CTRL_BREAK_EVENT` test (Tasks 3, 6).
- `.cmd` shim identity is byte-exact (CRLF, quoted path); foreign content must refuse (Task 4).
- `HOME` set by tests must win over `USERPROFILE` (Task 3).

---

### Task 1: Daemon runtime platform core (`ltmrs-daemon`)

**Files:**
- Modify: `crates/daemon/Cargo.toml` (target-conditional `windows-sys`)
- Create: `crates/daemon/src/runtime/unix.rs`, `crates/daemon/src/runtime/windows.rs`
- Modify: `crates/daemon/src/runtime.rs` (dispatch: `RuntimePaths`, `DaemonLock`, `DaemonListener`, `acquire_singleton`, `lock_held`)
- Test: unit tests in the platform modules

**Interfaces:**
- Produces:
  - `RuntimePaths { runtime_dir: PathBuf, lock_path: PathBuf, endpoint: PathBuf }` (`socket_path` renamed; `resolve(base, identity)` unchanged; Windows endpoint = `\\.\pipe\ltmrs-<identity>-<first-12-hex-of-sha256(sid-string ‖ runtime_dir)>`).
  - `DaemonRuntime { lock: DaemonLock, listener: DaemonListener }`
  - `DaemonListener::accept(&self) -> Result<IpcStream, std::io::Error>` (`IpcStream` defined in Task 2; Unix: persistent `Arc<UnixListener>`; Windows: `NamedPipeServer` created per accept with `ServerOptions::security_attributes(&sec)` from SDDL `D:(A;;GRGWGX;;;<sid>)(A;;GRGWGX;;;SY)` via `ConvertSecurityDescriptorFromStringW` + `SECURITY_ATTRIBUTES`).
  - `pub fn user_sid_string() -> Result<String, RuntimeError>` (`GetTokenInformation(TokenUser)` + `ConvertSidToStringSidW`; SAFETY: process token is valid for the process lifetime, buffers are windows-sys-owned).
  - `lock_held(paths) -> Option<bool>` and `acquire_singleton(paths)` semantics unchanged.
- Consumes: nothing (bottom of the chain).

- [x] **Step 1:** Failing tests in `runtime/windows.rs` (cfg(windows)) + shared: `endpoint_is_unique_per_store_identity` (two identities → distinct endpoints); `endpoint_carries_user_sid_token` (endpoint contains the 12-hex SID token, deterministic from `user_sid_string()`); `dacl_grants_only_user_and_system` (create pipe via `accept`-path builder, `GetSecurityInfo` on the handle, assert exactly 2 ACEs: user SID + `S-1-5-18`, no `GR` for Everyone); `junction_runtime_dir_refused` (`std::os::windows::fs::symlink_dir` — dev-mode may refuse; fallback `mklink /J` via `Command::new("cmd").args(["/C","mklink","/J",…])`); `second_acquire_rejects_while_first_holds_lock` (LockFileEx exclusivity); `lock_probe_distinguishes_held_free_and_missing` (shared probe). Run scoped: `cargo test -p ltmrs-daemon --lib` → FAIL.
- [x] **Step 2:** Implement `runtime/unix.rs` (move current flock/UnixListener/PermissionsExt/symlink-refusal code verbatim), `runtime/windows.rs` (`LockFileEx(LOCKFILE_EXCLUSIVE_LOCK|LOCKFILE_FAIL_IMMEDIATELY)` on `File::as_raw_handle`; shared probe = `LockFileEx(LOCKFILE_FAIL_IMMEDIATELY)` then `UnlockFile`; endpoint build with `sha2::Sha256` over SID string; SDDL DACL; `ServerOptions::create` per accept; reparse refusal via `symlink_metadata`), `runtime.rs` `#[cfg(unix)]/#[cfg(windows)]` re-exports + `RuntimePaths::resolve` platform endpoint. Safety comments on each unsafe block.
- [x] **Step 3:** `cargo test -p ltmrs-daemon --lib` → PASS (unix tests keep passing on Linux side by keeping them cfg(unix)).
- [ ] **Step 4:** Commit (AGENTS workflow).

### Task 2: Transport abstraction (`ltmrs-daemon`)

**Files:**
- Modify: `crates/daemon/src/client.rs`, `crates/daemon/src/server.rs`, `crates/daemon/src/envelope.rs` (test harness), `crates/daemon/src/runtime.rs` (IpcStream alias)
- Test: `client.rs`/`server.rs` unit tests

**Interfaces:**
- Produces:
  - `pub type IpcStream = Box<dyn tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send>;` in `ltmrs_daemon::client` (re-exported by runtime).
  - `pub async fn handle_connection<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send>(stream: S, dispatcher: Arc<Dispatcher>, quotas: Arc<QuotaTracker>) -> Result<(), DaemonError>` — peer-cred check REMOVED from `handle_connection` (moves to Unix accept path).
  - `IpcClient { socket_path: PathBuf, stream: Option<IpcStream>, … }`; `set_stream(&mut self, stream: IpcStream)`; `connect()` dials Unix `UnixStream::connect` (cfg(unix)) / `NamedPipeClient::connect` (cfg(windows)), boxing into `IpcStream`.
  - Unix accept path: `check_peer_cred(&UnixStream)` stays cfg(unix), called in `spawn_socket_server`'s accept loop before `handle_connection`; `DaemonListener::accept` returns `IpcStream`.
- Consumes: `DaemonListener`, `RuntimePaths.endpoint` (Task 1).

- [x] **Step 1:** Failing tests: `client.rs` — `duplex_roundtrip` (`tokio::io::duplex(65536)`, `set_stream(Box::new(a))`, handshake + `ListMemories` roundtrip against `handle_connection(Box::new(b), …)`); `server.rs` — `peer_cred_same_uid_passes` stays cfg(unix) on the pair; `accept_loop_checks_peer_before_frame` (cfg(unix): accept on a `UnixListener::pair`-based listener rejects foreign-cred path is not constructible in-process — pin that `check_peer_cred` runs in the accept loop by refactoring the call site and keeping the pair test green). Run scoped → FAIL.
- [x] **Step 2:** Implement generic `handle_connection`, boxed `IpcStream` in `IpcClient`, platform `connect()`, accept-loop boxing. All frame read/write helpers already take `&mut S` — widen signatures minimally.
- [x] **Step 3:** `cargo test -p ltmrs-daemon --lib` → PASS.
- [ ] **Step 4:** Commit.

### Task 3: Frontend serve, spawn, signals, home (`ltmrs-frontend`)

**Files:**
- Modify: `crates/frontend/Cargo.toml` (target-conditional `windows-sys`), `crates/frontend/src/frontend/serve.rs`, `crates/frontend/src/frontend/mcp.rs` (test harness)
- Test: `serve.rs` unit tests

**Interfaces:**
- Produces:
  - `pub fn spawn_detached(exe: &Path, args: &[String], env: &[(&str, &str)], stdout: std::process::Stdio, stderr: std::process::Stdio) -> Result<std::process::Child, CliError>` in `serve.rs` — Unix: `pre_exec` + `libc::setsid` (SAFETY comment as today); Windows: `creation_flags(0x20)` (`CREATE_NEW_PROCESS_GROUP`), stdin null.
  - `run_daemon_foreground` shutdown signal: cfg(unix) SIGTERM unchanged; cfg(windows) `tokio::select!` over `ctrl_c()` and `ctrl_break()`.
  - `resolve_home`: explicit `--home` → `HOME` env → (cfg(windows)) `USERPROFILE` → error naming both.
  - `start_local_daemon` bridge: `tokio::io::duplex(65536)` replaces `UnixStream::pair`; `daemon_log_path` unchanged; log opened with plain `OpenOptions` on Windows (NTFS inheritance, deviation recorded in Task 7).
  - `mcp.rs` test harness: replace `UnixListener::bind` with `Daemon::start` + `spawn_socket_server` (live listener) so all 4 harness tests are platform-neutral.
- Consumes: `IpcClient::set_stream(IpcStream)`, `DaemonListener` (Task 2).

- [x] **Step 1:** Failing tests: `resolve_home_prefers_home_over_userprofile` (both env set → HOME wins; cfg(windows): USERPROFILE-only resolves); `spawn_detached_child_exits_and_survives_group_signal` (spawn `daemon --foreground --daemon-idle-ms 0`… no — spawn a child running `daemon --foreground --daemon-idle-ms 10`, `try_wait` within bounded timeout exits success); `start_local_daemon` duplex bridge roundtrip (existing `local_daemon_bridge_roundtrips_memory_add` must pass via duplex). Scoped run → FAIL.
- [x] **Step 2:** Implement the interfaces above. `spawn_daemon_child` becomes a thin wrapper over `spawn_detached` (log-file Stdio preserved).
- [x] **Step 3:** `cargo test -p ltmrs-frontend --lib` → PASS.
- [ ] **Step 4:** Commit.

### Task 4: `.cmd` shim (`ltmrs-frontend` skills)

**Files:**
- Modify: `crates/frontend/src/skills/shim.rs`
- Test: `shim.rs` unit tests

**Interfaces:**
- Produces: `install_shim(exe, home) -> ShimOutcome` unchanged in behavior; Windows path writes `<home>/.local/bin/lemma.cmd` with exact bytes `@echo off\r\n"<exe-path>" %*\r\n`; existing-file identity = byte-for-byte equality with that content (accept), anything else refuses (foreign rule unchanged); install result string names the dir + PATH hint on Windows.
- Consumes: nothing new.

- [x] **Step 1:** Failing tests: `cmd_shim_accepts_our_exact_bytes` (pre-write expected content, re-install → accepted, file unchanged); `cmd_shim_refuses_foreign_content` (write `@echo off\r\nother.exe %*\r\n` → refuse); symlink tests stay cfg(unix). Scoped run → FAIL.
- [x] **Step 2:** Implement cfg(windows) shim writer + byte-identity check.
- [x] **Step 3:** `cargo test -p ltmrs-frontend --lib` → PASS.
- [ ] **Step 4:** Commit.

### Task 5: Visualizer background via shared helper (root binary)

**Files:**
- Modify: `src/visualizer/mod.rs` (`run_background` detach only)
- Test: `tests/vis_smoke.rs` (existing live smoke)

**Interfaces:**
- Consumes: `ltmrs_frontend::frontend::serve::spawn_detached` (Task 3).
- Produces: unchanged public behavior (URL+pid printed, port probe, `owns_port` token probe).

- [x] **Step 1:** Replace `pre_exec`+`setsid` with `spawn_detached(exe, ["-vis","--fg","-p",port], [("LTMRS_VIS_TOKEN", token)], Stdio::null(), Stdio::inherit())`; keep the `owns_port` probe logic verbatim.
- [x] **Step 2:** `cargo test -p ltmrs --test vis_smoke` (root binary, scoped to that target) → PASS on Windows.
- [ ] **Step 3:** Commit.

### Task 6: Root tests + full Windows gate

**Files:**
- Modify: `Cargo.toml` (root dev-dependency `windows-sys`, cfg(windows), same features, called out in manifest), `tests/daemon_lifecycle.rs`
- Test: existing `tests/*` + new `windows_graceful_shutdown_via_ctrl_break`

**Interfaces:**
- Consumes: `CREATE_NEW_PROCESS_GROUP`, `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` (windows-sys; SAFETY: console control events target only our spawned group).

- [x] **Step 1:** Failing test `windows_graceful_shutdown_via_ctrl_break` (cfg(windows)): spawn `ltmrs daemon --foreground --daemon-idle-ms 0` with `CREATE_NEW_PROCESS_GROUP` + isolated HOME, send `CTRL_BREAK_EVENT` to the child pid, assert exit code 0 within bounded timeout. SIGTERM test gated cfg(unix). Scoped: `cargo test -p ltmrs --test daemon_lifecycle` → FAIL, then implement (signal select in Task 3 already handles ctrl_break; this test pins the binary path). **Deviation (2026-10-08, probe-verified):** programmatic console-event delivery is not achievable under `cargo test` on this conhost (group-0 kills cargo; targeted delivery is delayed tens of seconds and hangs `try_wait` after the daemon exits). The test is kept as `#[ignore]` with the reason; handler registration is pinned executably by `serve::tests::shutdown_handlers_register` (ctrl_c + ctrl_break Ok on Windows); the visualizer smoke stops its detached child via `TerminateProcess` on Windows.
- [x] **Step 2:** Full gate on Windows, in this order, all four commands:
  `cargo fmt -- --check`
  `cargo clippy --workspace --all-targets -- -D warnings`
  `cargo test --workspace`
  `cargo build --release`
  Expected: all clean. Fix everything found; no claims without executed output.
- [x] **Step 3:** Live smokes against the release binary: `ltmrs daemon --foreground --daemon-idle-ms 2000` (idle exit 0), two stdio frontends attach to one managed daemon (second handshake), `-vis` background prints URL+pid and `/api/library?token` serves 200, `--install-skill` writes the `.cmd` shim. Record outputs.
- [ ] **Step 4:** Commit.

### Task 7: Plan, ledger and changelog updates (one review)

**Files:**
- Modify: `plans/01_design_and_concepts.md` (§7.1 Windows paragraph; AD-07 row → Linux+Windows certified with host matrix: Windows 11 x86_64, NTFS, toolchain 1.99.0 MSVC, protoc 36.0, no mold/wild)
- Modify: `plans/03_quality_tests_benchmarks_conformance.md` (T-SEC-01 Windows row: DACL, LockFileEx, reparse refusal, generation mismatch — executed values from Task 6)
- Modify: `plans/traceability.json` + `plans/conformance_matrix.json` (deviation ledger entries from spec §2.3/2.5/2.6/2.7/2.8; visualizer conformance table verbatim from spec §2.8)
- Modify: `CHANGELOG.md`, `DONE.md` (per commit workflow)

- [x] **Step 1:** Re-read spec §2.8 + §4; enter every deviation with exact upstream facts (port 3456, `x-lemma-token`, browser auto-open, CORS, write routes) and ltmrs behavior; mark executed evidence ONLY where Task 6 output exists.
- [x] **Step 2:** Cross-check §7.1 wording matches the shipped interfaces (endpoint, DACL, LockFileEx, Ctrl-break, USERPROFILE, `.cmd`).
- [ ] **Step 3:** Commit with the CHANGELOG date heading (verify via grep per AGENTS step 2).

---

## Self-review notes

- Spec coverage: §2.1→Tasks 1-2, §2.2→Task 1, §2.3→Tasks 1,6, §2.4→Tasks 1,3, §2.5→Tasks 3,6, §2.6→Task 3, §2.7→Task 4, §2.8→Tasks 5,7, §4→Task 7, §5→all, §6→Task 6.
- Types: `IpcStream`, `DaemonListener::accept`, `spawn_detached` signatures consistent across tasks.
- Review Focus lines map to: T1 (SID token, LockFileEx, DACL), T3/T6 (process group, ctrl-break), T4 (byte identity), T3 (HOME precedence).

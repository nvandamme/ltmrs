# Windows Compatibility Design — ltmrs MCP (2026-10-08)

Status: design approved in chat (full certification, named pipes, `%USERPROFILE%` home, `.cmd` shim, visualizer checked against upstream). Companion implementation plan: `plans/2026-10-08-windows-compat-plan.md` (written after spec approval via writing-plans).

## 1. Mission and success criteria

Render ltmrs fully Windows-compatible: the Linux-first platform protocol of §7.1 gains a
certified Windows counterpart. Success bar (agreed): **full certification** —
`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, `cargo build --release` all pass on this Windows host; the IPC,
storage, restore and packaging suites pass; AD-07 closes with a recorded host/OS matrix;
§7.1, T-SEC-01, T-CLI-01 and the deviation ledger are updated in one review (plan authority).

Claimed Windows target: Windows x86_64 (MSVC toolchain 1.99.0), NTFS, per-user accounts.
macOS stays uncertified. CUDA remains optional and out of scope.

### Verified blockers (inspected, not guessed)

- `crates/daemon/src/runtime.rs`: `libc::flock`, `tokio::net::UnixListener`, `PermissionsExt`
  0700/0600, symlink refusal, `std::os::unix` tests.
- `crates/daemon/src/client.rs` + `server.rs`: `UnixStream` transport, `peer_cred()` +
  `libc::geteuid` same-user check, `handle_connection` signature.
- `crates/frontend/src/frontend/serve.rs`: `UnixStream::pair` bridge, `pre_exec`+`libc::setsid`
  detach, `tokio::signal::unix` SIGTERM, `OpenOptionsExt` 0600 log, `$HOME` resolution.
- `crates/frontend/src/skills/shim.rs`: `std::os::unix::fs::symlink` lemma shim.
- `crates/frontend/src/frontend/mcp.rs` tests: `UnixListener::bind` harness.
- `src/visualizer/mod.rs` `run_background`: `pre_exec`+`libc::setsid` detach (TCP, loopback
  binding, `ctrl_c`, token, routes are already cross-platform).
- Environment: `protoc` required by lance build scripts (installed 2026-10-08 via winget
  `Google.Protobuf` 36.0); mold/wild absent → system linker (probe falls back, no change).
- Clean-compile already verified on MSVC: `domain`, `service`, `compat`, `embeddings`,
  `search` check green with `PROTOC` set.

## 2. Platform protocol design

### 2.1 IPC transport (daemon crate owns it)

- Framing/envelope unchanged (length-prefixed UTF-8 JSON over a byte stream).
- `handle_connection` becomes generic over `AsyncRead + AsyncWrite + Unpin + Send`
  (UnixStream, NamedPipeServer and `tokio::io::DuplexStream` all satisfy it).
- `IpcClient` stores `Box<dyn AsyncRead + AsyncWrite + Unpin + Send>`; connect is
  platform-dispatched (`UnixStream::connect` / `NamedPipeClient::connect`).
- Accept loop: Unix = `UnixListener` as today. Windows = loop { `NamedPipeServer::new(name)`;
  `connect()`; spawn per connection } — one pipe instance serves one client, the loop
  recreates instances (standard named-pipe fan-out).
- Windows accept data gate (2026-10-08, MSVC-verified): byte-mode `ConnectNamedPipe`
  reports `ERROR_NO_DATA` with no client attached, which tokio/mio mistake for a
  successful connect — the instance is listening but never connected and reads EOF at
  once. `accept()` therefore gates on DATA, not connectedness: after `connect()`, the
  SAME instance is polled (so a racing client always finds a listener) for
  `PeekNamedPipe` bytes-available `> 0` — the protocol is client-writes-first
  (handshake), so waiting bytes prove a genuine peer. `PeekNamedPipe` success alone does
  NOT discriminate (verified: it succeeds on unconnected instances). Bounded at 1000ms
  → `WouldBlock`, which the serve idle loop treats exactly like a timeout tick (exit
  when quiet past the budget), the serve-forever loop (`--daemon-idle-ms 0`) skips
  with `continue`, and `spawn_socket_server` skips with `continue`. The
  connect stays inside tokio/mio: their `OVERLAPPED` lives in boxed, never-moved memory
  — a future-stack `OVERLAPPED` handed to the kernel dangles across awaits and
  access-violates (verified exit `0xc0000005`). `HasOverlappedIoCompleted` is a WinBase
  macro (`Internal != STATUS_PENDING`), not an importable function.
- In-process bridge (`start_local_daemon`) uses `tokio::io::duplex` (cross-platform),
  replacing `UnixStream::pair`.
- `RuntimePaths.socket_path` becomes `RuntimePaths.endpoint`:
  - Unix: `<runtime_dir>/daemon.sock` (unchanged).
  - Windows: `\\.\pipe\ltmrs-<store_identity>-<user-token>` where `user-token` is the first
    12 hex chars of SHA-256 over the user SID string concatenated with the runtime-dir path
    (pipe namespace is system-global, so the token keeps two users' stores from colliding
    AND two stores under different bases from sharing a name — tests and portable stores
    isolate naturally; computed via `windows-sys` `GetTokenInformation(TokenUser)`).
- `--socket <path>` on Windows takes a pipe name; CLI help documents the platform form.

### 2.2 Singleton lock

- Windows: `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)` on
  `daemon.lock` (windows-sys). Same ownership semantics as flock: the lock dies with the
  handle/process. `lock_held` probe = `LockFileEx(LOCKFILE_FAIL_IMMEDIATELY)` shared
  (succeeds only when no exclusive owner), then `UnlockFile`.
- `DaemonLock` holds the `File` handle; drop releases. No behavior change on Unix.

### 2.3 Peer authentication (T-SEC-01 Windows row)

- Unix: `peer_cred` + `geteuid` same-UID check unchanged.
- Windows: named pipes have no peer-cred. The equivalent is a **per-user DACL** on the pipe:
  the `NamedPipeServer` is created with a security descriptor granting
  `FILE_CREATE_PIPE_INSTANCE`/`FILE_WRITE`/`FILE_READ` only to the current user's SID
  (from `GetTokenInformation(TokenUser)`) and SYSTEM; nobody else can connect.
  Implemented with `windows-sys` (`AllocateAndInitializeSecurityDescriptor`,
  `InitializeSecurityDescriptor`, `SetSecurityDescriptorDacl` passed to
  `CreateNamedPipeW` via `SECURITY_ATTRIBUTES`).
- The check moves into the platform accept path; `check_peer_cred` stays cfg(unix).
  A Windows test verifies the created descriptor contains exactly the user+SYSTEM ACEs.

### 2.4 Runtime directory protection

- Windows: the runtime dir lives under `%USERPROFILE%\.ltmrs` (see 2.6); NTFS inherits the
  per-user ACL — this is the 0700/0600 mapping, recorded as a deviation.
- Symlink refusal keeps `symlink_metadata` (Windows reparse points — symlinks and junctions —
  are detected by `is_symlink()`); the test is cfg-gated: Unix symlinks, Windows junctions
  (`mklink /J` works unprivileged).

### 2.5 Detached spawn + signals (shared helper, no duplication)

- One shared detach helper in `ltmrs-frontend` (owner of daemon spawn; `src/visualizer` calls
  it): `detach_child(&mut Command)` — setsid via `pre_exec` on unix,
  `CREATE_NEW_PROCESS_GROUP` on Windows (shipped shape; the plan's
  `spawn_detached` constructor spelling was superseded).
  - Unix: `pre_exec` + `libc::setsid` (current code).
  - Windows: `CREATE_NEW_PROCESS_GROUP` creation flag (Ctrl-C to the parent group never
    reaches the child; no console signal otherwise).
- Foreground shutdown signal: Unix SIGTERM unchanged; Windows `tokio::select!` over
  `ctrl_c()` and `ctrl_break()` (console Ctrl+C is the supervisor-equivalent; Ctrl-Break
  reaches a detached child in its own process group). Deviation recorded 2026-10-08:
  programmatic delivery of console control events is not testable under `cargo test` on
  this conhost — group-0 events kill cargo (cargo registers no handler), targeted
  delivery to the child's own group is delayed tens of seconds and hangs `try_wait`
  after the child exits (probe-verified). Handler registration is pinned executably
  (`serve::tests::shutdown_handlers_register`); the foreground Ctrl-C path is proven
  clean by probe in one console group (daemon fires, exits 0); the visualizer smoke
  stops its detached child via `TerminateProcess` on Windows.

### 2.6 Home resolution

- `resolve_home` (frontend): explicit `--home` wins; otherwise `HOME` env, then on Windows
  `USERPROFILE` fallback. Layout unchanged (`<home>/.ltmrs`). Tests that set `HOME`
  explicitly keep working.

### 2.7 Lemma shim

- Unix: symlink at `<home>/.local/bin/lemma` unchanged.
- Windows: write `<home>\.local\bin\lemma.cmd` with content
  `@echo off` + quoted exe path + `%*` + `exit /b %ERRORLEVEL%` (CRLF throughout).
  Identity check: existing file must equal the exact expected shim content
  (byte-for-byte); foreign content refuses, same rule as foreign
  symlinks. Install result reports the dir and a PATH hint (Windows PATH is not user-owned
  by convention; upstream shim dir convention is recorded as a deviation).

### 2.8 Visualizer conformance against upstream Lemma 0.21.0 (pinned d30a8166)

Inspected upstream facts (`src/server/visualize.ts`, `src/index.ts`, `assets/visualizer.html`,
CHANGELOG 0.11.0/0.19.0, README): default port **3456**; token accepted as `x-lemma-token`
header **or** `?token=` query; CORS localhost allow-list; REST routes `/api/data`,
`/api/stats`, `/api/health`, `PATCH/DELETE /api/fragments/:id`, `POST/DELETE /api/relations`,
`/api/export`; **browser opens automatically** (cross-platform claim); background mode spawns
a detached child and prints URL+PID ("Stop: kill PID").

ltmrs native visualizer (`src/visualizer/mod.rs`) conformance record — deviations to enter in
the ledger (T-CLI-01: "unsupported visualizer behavior is not advertised"):

| Upstream fact | ltmrs | Status |
|---|---|---|
| `-vis`, `-vis -p PORT`, foreground | implemented, aliases matched | conform |
| loopback-only bind, token gate | 127.0.0.1-only, token on every route | conform (hardened) |
| default port 3456 | native 18721, pinned by test | deviation (recorded) |
| token header `x-lemma-token` | `?token=` query only | deviation (recorded) |
| browser auto-open | URL printed, user opens | deviation (not advertised) |
| D3 graph + write routes (PATCH/DELETE/POST) | read-only native routes `/`, `/api/library`, `/api/export` | deviation (WP-10: routes are ltmrs-native; graph parity not required) |
| CORS allow-list | no CORS (single static page, no browser fetch) | deviation (recorded) |
| background prints URL+PID | prints URL+pid | conform |

Windows work in the visualizer: replace `setsid` detach with the shared helper (2.5);
everything else (TCP loopback, token, routes, `ctrl_c`) is already cross-platform.

## 3. Module organization (AGENTS rules 1-5)

- `ltmrs-daemon` owns the IPC domain: `runtime.rs` gains cfg dispatch to
  `runtime_unix.rs`/`runtime_windows.rs` (lock, endpoint, accept-loop construction);
  `server.rs` accept path gains the platform peer-auth; `client.rs` stores the boxed stream.
- `ltmrs-frontend` owns spawn/home/shim: `serve.rs` gains the shared `detach_child` helper
  (cfg dispatch), the `HOME`-first/`USERPROFILE`-fallback home resolution, and the
  `tokio::io::duplex` bridge; `skills/shim.rs` gains the `.cmd` path.
- `src/visualizer/mod.rs` calls the frontend shared helper (no duplicated detach).
- `windows-sys` is a target-conditional dependency (`cfg(windows)`) in `ltmrs-daemon`
  (lock+DACL+SID) and `ltmrs-frontend` (process-group flag). Heavy-dep ownership unchanged.

## 4. Plan updates (one review, plan authority)

- §7.1: add the Windows platform-protocol paragraph (named pipes + DACL, LockFileEx,
  per-user token in the pipe name, Ctrl+C, USERPROFILE, `.cmd` shim).
- AD-07: state → Linux + Windows certified; evidence = host/OS matrix recorded in the
  conformance/deviation ledger (this host: Windows 11 x86_64, NTFS, toolchain 1.99.0
  MSVC, protoc 36.0, no mold/wild).
- T-SEC-01: add the Windows row (DACL-only user connect, LockFileEx one-owner,
  reparse-point refusal, generation mismatch) with executed evidence after the suite runs.
- T-CLI-01: record visualizer alias behavior + deviations table (2.8) as executed on Windows.
- WP-10 acceptance: extend with the Windows stdio/shim/visualizer smoke evidence.
- `plans/traceability.json` + `conformance_matrix.json`: ledger entries for every deviation
  in 2.3, 2.5, 2.6, 2.7, 2.8 (never conflate planned/executed).

## 5. Tests (platform-gated, deterministic)

- runtime: lock held/free/missing probe, second-acquire rejection, stale-endpoint recovery
  (unix), reparse-point refusal, endpoint uniqueness per store identity + per user token.
- Windows-only: DACL descriptor content test; named-pipe connect/roundtrip; duplex bridge.
- server: peer-auth path tests per platform (unix pair stays; windows: connect succeeds,
  descriptor verified).
- client/envelope: boxed-stream roundtrips (duplex pair replaces `UnixStream::pair`).
- serve: `spawn_detached` smoke (child idle-exits; lock race loser behavior), `ctrl_c`
  foreground shutdown (deterministic via signal send is not testable portably — keep the
  visualizer's existing foreground/`ctrl_c` live smoke over TCP).
- shim: `.cmd` byte-identity accept/refuse tests (windows), symlink tests stay cfg(unix).
- Root `tests/` (daemon_lifecycle, stdio_smoke, vis_smoke, release_evidence): run on
  Windows with `HOME` set to the temp dir (tests already set env explicitly).

## 6. Validation gate (executed on this host)

```bash
cargo fmt -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

Plus live-binary smokes: `ltmrs daemon --foreground`, stdio MCP attach, second-frontend
socket attach, `-vis` background/foreground, `--install-skill` shim.

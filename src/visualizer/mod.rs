//! Local visualizer (WP-10 tasks 6-7; T-VIS-01 equivalent under T-CLI-01).
//!
//! Serves the canonical library snapshot over HTTP on loopback only:
//! `GET /` renders an HTML index (all dynamic text HTML-escaped),
//! `GET /api/library` returns the canonical export as JSON,
//! `GET /api/export` downloads memories as JSONL (one object per line).
//! Anything else is 404 (unknown route) or 405 (non-GET). `-vis`
//! backgrounds by spawning a detached child (`-vis --fg`); `-vis --fg`
//! serves in the foreground until Ctrl-C. No new dependencies: minimal
//! HTTP/1.1 over tokio.

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use ltmrs_domain::export::CanonicalExport;
use ltmrs_frontend::cli::CliError;

/// Default visualizer port. ltmrs-native (no upstream default is recorded
/// in the baseline), pinned by test so changes are deliberate.
pub const DEFAULT_VIS_PORT: u16 = 18721;

/// Loopback interface: the visualizer never binds anything else.
/// (`Ipv4Addr` so the `(addr, port)` pair satisfies `ToSocketAddrs`.)
pub const LOOPBACK: std::net::Ipv4Addr = std::net::Ipv4Addr::new(127, 0, 0, 1);

/// Resolve the serving port: explicit `-p/--port` wins, else the native
/// default. A bind failure later names the port explicitly (never a silent
/// fallback to another port).
pub fn resolve_port(port: Option<u16>) -> u16 {
    port.unwrap_or(DEFAULT_VIS_PORT)
}

/// Escape the five HTML-significant characters (safe output encoding).
pub fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render the index page from a canonical export (titles, fragments and
/// counts escaped; links the JSON route).
pub fn render_index(export: &CanonicalExport, token: &str) -> String {
    let mut page = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>ltmrs library</title></head><body>\n<h1>ltmrs library</h1>\n",
    );
    page.push_str(&format!(
        "<p>Memories: {} &middot; Relations: {} &middot; Guides: {}</p>\n<ul>\n",
        export.memories.len(),
        export.relations.len(),
        export.guides.len()
    ));
    for m in &export.memories {
        page.push_str(&format!(
            "<li><b>{}</b> &mdash; {}</li>\n",
            html_escape(&m.title),
            html_escape(&m.fragment)
        ));
    }
    page.push_str(&format!(
        "</ul>\n<p><a href=\"/api/library?token={token}\">JSON snapshot</a> | <a href=\"/api/export?token={token}\">JSONL export</a></p>\n</body></html>\n"
    ));
    page
}

/// Per-boot access token: memory content is same-user data (RQ-20), and any
/// local UID can reach loopback. 128 bits from an OS-seeded CSPRNG (uuid v4,
/// same unpredictability contract as the former getrandom fill): a fixed-key
/// hash over (pid, wall-time, counter) would be brute-forceable from /proc
/// plus a loopback port scan, since the token is the sole cross-UID control.
pub fn access_token() -> String {
    uuid::Uuid::new_v4().as_simple().to_string()
}

/// Serve until `shutdown` resolves. Opens the store first so a missing
/// store fails fast with `no store at ...` instead of serving 500s.
/// Test-only: every production path passes an explicit token via
/// `serve_with_token`; only the missing-store test uses this wrapper.
#[cfg(test)]
pub async fn serve(
    listener: TcpListener,
    store_path: String,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), CliError> {
    serve_with_token(listener, store_path, shutdown, access_token()).await
}

/// Serve with an explicit access token (tests pin a fixed token; the
/// background child inherits the parent's via environment).
pub async fn serve_with_token(
    listener: TcpListener,
    store_path: String,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    token: String,
) -> Result<(), CliError> {
    if !Path::new(&store_path).exists() {
        return Err(CliError::Usage(format!("no store at {store_path}")));
    }
    let mut shutdown = shutdown;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // A failed accept is not fatal: keep serving (the error is
                // dropped with the connection; the loop stays alive).
                let Ok((stream, _peer)) = accepted else {
                    continue;
                };
                let store = store_path.to_string();
                let token = token.clone();
                tokio::spawn(async move {
                    let _ = handle_request(stream, store, token).await;
                });
            }
            _ = &mut shutdown => break,
        }
    }
    Ok(())
}

/// Constant-time byte comparison for the access token: the token is the
/// sole cross-UID control on loopback, so short-circuit comparison would
/// leak it byte-at-a-time to local timing probes. (Std-only: no extra dep
/// for one comparison.)
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Read one request (bounded), route it, and write the response. Memory
/// content requires the per-boot access token on every route: loopback is
/// reachable by any local UID, so binding alone is not a user boundary.
async fn handle_request(
    mut stream: tokio::net::TcpStream,
    store_path: String,
    token: String,
) -> Result<(), CliError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| CliError::Runtime(format!("visualizer read failed: {e}")))?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 65536 {
            let resp = respond(400, "Bad Request", "text/plain", b"request too large");
            stream
                .write_all(&resp)
                .await
                .map_err(|e| CliError::Runtime(format!("visualizer write failed: {e}")))?;
            return Ok(());
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let authorized = query.split('&').any(|pair| {
        let mut kv = pair.splitn(2, '=');
        kv.next() == Some("token")
            && kv
                .next()
                .is_some_and(|candidate| constant_time_eq(candidate.as_bytes(), token.as_bytes()))
    });
    let resp = if method != "GET" {
        respond(
            405,
            "Method Not Allowed",
            "text/plain",
            b"only GET is supported",
        )
    } else if !authorized {
        respond(403, "Forbidden", "text/plain", b"access token required")
    } else if path == "/" {
        match library_export(&store_path) {
            Ok(export) => respond(
                200,
                "OK",
                "text/html; charset=utf-8",
                render_index(&export, &token).as_bytes(),
            ),
            Err(message) => respond(
                500,
                "Internal Server Error",
                "text/plain",
                message.as_bytes(),
            ),
        }
    } else if path == "/api/library" {
        match library_export(&store_path) {
            Ok(export) => respond(200, "OK", "application/json", export.to_json().as_bytes()),
            Err(message) => respond(
                500,
                "Internal Server Error",
                "text/plain",
                message.as_bytes(),
            ),
        }
    } else if path == "/api/export" {
        // Memory export as a JSONL download (upstream parity: one JSON
        // object per fragment, attachment disposition). Token-gated
        // like every route (?token= form, since a download link cannot
        // set custom headers).
        match library_export(&store_path) {
            Ok(export) => respond_extra(
                200,
                "OK",
                "application/x-jsonlines",
                "content-disposition: attachment; filename=memory-export.jsonl\r\n",
                export.to_jsonlines().as_bytes(),
            ),
            Err(message) => respond(
                500,
                "Internal Server Error",
                "text/plain",
                message.as_bytes(),
            ),
        }
    } else {
        respond(404, "Not Found", "text/plain", b"unknown route")
    };
    stream
        .write_all(&resp)
        .await
        .map_err(|e| CliError::Runtime(format!("visualizer write failed: {e}")))?;
    Ok(())
}

/// Open the store and export a canonical snapshot for one request (open per
/// request: the visualizer never holds a second long-lived store handle
/// next to a running daemon).
fn library_export(store_path: &str) -> Result<CanonicalExport, String> {
    if !Path::new(store_path).exists() {
        return Err(format!("no store at {store_path}"));
    }
    let repo =
        ltmrs_service::repository::CanonicalRepository::open(store_path).map_err(|e| e.message)?;
    repo.export_snapshot().map_err(|e| e.message)
}

/// Build a minimal HTTP/1.1 response (caller closes the connection).
fn respond(code: u16, reason: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    respond_extra(code, reason, content_type, "", body)
}

/// Response with additional headers (e.g. download disposition);
/// `extra_headers` must already end with `\r\n` when non-empty.
fn respond_extra(
    code: u16,
    reason: &str,
    content_type: &str,
    extra_headers: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Serve in the foreground until Ctrl-C; returns a farewell message.
pub async fn run_foreground(port: Option<u16>, store: &str) -> Result<String, CliError> {
    let port = resolve_port(port);
    let listener = TcpListener::bind((LOOPBACK, port))
        .await
        .map_err(|e| CliError::Runtime(format!("cannot bind 127.0.0.1:{port}: {e}")))?;
    // The access token is per-boot: a background parent passes its own via
    // LTMRS_VIS_TOKEN so the printed URL matches the serving child;
    // foreground use generates (and prints) a fresh one.
    let token = std::env::var("LTMRS_VIS_TOKEN").unwrap_or_else(|_| access_token());
    println!("serving at http://127.0.0.1:{port}/?token={token}");
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(());
    });
    serve_with_token(listener, store.to_string(), rx, token.clone()).await?;
    Ok(format!(
        "visualizer stopped (was http://127.0.0.1:{port}/?token={token})"
    ))
}

/// Spawn a detached child serving in the foreground, wait until it listens,
/// and return the `serving at ... (pid ...)` message. A port that is already
/// bound fails fast without spawning.
/// The store is NOT passed down: the child re-derives the managed-home store
/// from its (inherited) environment, so parent and child always agree. A
/// store deleted between the parent check and the child start surfaces as
/// "child exited before listening".
pub async fn run_background(port: Option<u16>) -> Result<String, CliError> {
    let port = resolve_port(port);
    // Fail fast when the port is taken (avoids spawning a child that can
    // never bind, and avoids silently serving on another port).
    let probe = TcpListener::bind((LOOPBACK, port)).await;
    match probe {
        Ok(bound) => drop(bound),
        Err(e) => {
            return Err(CliError::Runtime(format!(
                "port {port} is already in use: {e}"
            )));
        }
    }
    let exe = std::env::current_exe()
        .map_err(|e| CliError::Runtime(format!("cannot locate ltmrs binary: {e}")))?;
    // The parent owns the per-boot token and hands it to the child, so the
    // printed URL matches the serving process (and the ownership probe
    // below can authenticate).
    let token = access_token();
    let mut child = std::process::Command::new(exe);
    child
        .arg("-vis")
        .arg("--fg")
        .arg("-p")
        .arg(port.to_string())
        .env("LTMRS_VIS_TOKEN", &token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    // Detach: `pre_exec` runs between fork and exec, where only
    // async-signal-safe calls are allowed — `setsid` qualifies. The child
    // then execs a fresh single-purpose process, so no thread state survives.
    use std::os::unix::process::CommandExt as _;
    unsafe {
        child.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = child
        .spawn()
        .map_err(|e| CliError::Runtime(format!("cannot spawn visualizer child: {e}")))?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match tokio::net::TcpStream::connect((LOOPBACK, port)).await {
            Ok(stream) => {
                // The port may have been grabbed by another process between
                // our probe and the child bind: only claim success when the
                // listener answers with our own index page (authenticated
                // with our token, so a squatter without it cannot fake us).
                if owns_port(stream, &token).await {
                    return Ok(format!(
                        "serving at http://127.0.0.1:{port}/?token={token} (pid {})",
                        child.id()
                    ));
                }
                if tokio::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(CliError::Runtime(format!(
                        "port {port} answers but is not our visualizer"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                // The child may have died (e.g. missing store): surface it
                // instead of waiting out the deadline.
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(CliError::Runtime(format!(
                        "visualizer child exited before listening: {status}"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(_) => {
                let _ = child.kill();
                return Err(CliError::Runtime(format!(
                    "visualizer did not listen on 127.0.0.1:{port} within 10s"
                )));
            }
        }
    }
}

/// Check that the listener on our port is the child we just spawned: fetch
/// `/` with our token and look for our index marker. Reads until the
/// marker, EOF, the cap, or the deadline — headers and body may arrive in
/// separate segments (so stopping at end-of-headers would miss the marker),
/// but a squatter holding the connection open with no data must not stall
/// past the deadline either.
async fn owns_port(mut stream: tokio::net::TcpStream, token: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let probe = format!("GET /?token={token} HTTP/1.1\r\nhost: probe\r\n\r\n");
    let mut buf = vec![0u8; 512];
    let mut seen = Vec::new();
    // The whole probe races a deadline: a squatter stalling the write
    // (zero window) or holding the connection open with no data must fail
    // closed instead of stalling past the caller's deadline.
    let probed = async {
        stream.write_all(probe.as_bytes()).await.ok()?;
        loop {
            if seen.len() >= 8192 {
                break;
            }
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&seen).contains("ltmrs library") {
                        break;
                    }
                }
            }
        }
        Some(())
    };
    if tokio::time::timeout(std::time::Duration::from_secs(5), probed)
        .await
        .is_err()
    {
        return false;
    }
    let head = String::from_utf8_lossy(&seen);
    head.starts_with("HTTP/1.1 200") && head.contains("ltmrs library")
}

/// Run the visualizer: foreground blocks, background prints the URL.
/// The store is the managed home store; a missing store is a usage error
/// (a read must not create stores, mirroring `-lib`).
pub async fn run_visualize(
    foreground: bool,
    port: Option<u16>,
    home: Option<String>,
) -> Result<String, CliError> {
    let base = ltmrs_frontend::frontend::serve::resolve_home(home)?;
    let store = ltmrs_frontend::frontend::serve::stdio_layout(&base).store_path;
    if !Path::new(&store).exists() {
        return Err(CliError::Usage(format!("no store at {store}")));
    }
    if foreground {
        run_foreground(port, &store).await
    } else {
        run_background(port).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    /// The native default port is pinned (change deliberately, not by drift).
    #[test]
    fn default_port_is_pinned() {
        assert_eq!(DEFAULT_VIS_PORT, 18721);
        assert_eq!(resolve_port(None), 18721);
        assert_eq!(resolve_port(Some(9999)), 9999);
    }

    /// All five markup-significant characters are escaped; plain text passes
    /// through untouched.
    #[test]
    fn html_escape_covers_markup_chars() {
        assert_eq!(html_escape("plain 123"), "plain 123");
        assert_eq!(
            html_escape("<script>&\"'"),
            "&lt;script&gt;&amp;&quot;&#x27;"
        );
    }

    fn hostile_memory() -> ltmrs_domain::memory::Memory {
        use ltmrs_domain::id::{DocumentRevision, EligibilityRevision, EntityId, EntityRevision};
        use ltmrs_domain::memory::Instant;
        use ltmrs_domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
        Memory {
            id: EntityId::new(uuid::Uuid::from_u128(9)),
            external_alias: None,
            title: "<b>title</b>".into(),
            fragment: "x <script>alert(1)</script> & \"y\"".into(),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: None,
            source: MemorySource::Ai,
            confidence: 0.5,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: EntityRevision::new(1),
            document_revision: DocumentRevision::new(1),
            eligibility_revision: EligibilityRevision::new(1),
            created_at: Instant::new(1),
            updated_at: Instant::new(1),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    /// Hostile content is escaped in the rendered index (no raw markup).
    #[test]
    fn render_index_escapes_hostile_content() {
        let export = CanonicalExport {
            memories: vec![hostile_memory()],
            ..Default::default()
        };
        let page = render_index(&export, "tok");
        assert!(page.contains("&lt;b&gt;title&lt;/b&gt;"), "got: {page}");
        assert!(
            page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "got: {page}"
        );
        assert!(!page.contains("<script>"), "raw markup leaked: {page}");
        assert!(page.contains("/api/library"), "must link the JSON route");
    }

    /// Raw request helper over TCP (the server only speaks HTTP/1.1 + close).
    async fn raw_request(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect((LOOPBACK, port)).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Listener, routes, methods and loopback binding over a real socket.
    #[tokio::test]
    async fn serves_index_api_404_405_over_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store").to_str().unwrap().to_string();
        drop(ltmrs_service::repository::CanonicalRepository::open(&store));
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            listener.local_addr().unwrap().ip().is_loopback(),
            "visualizer must bind loopback only"
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(serve_with_token(listener, store, rx, "t".to_string()));

        let index = raw_request(port, "GET /?token=t HTTP/1.1\r\nhost: x\r\n\r\n").await;
        assert!(index.starts_with("HTTP/1.1 200"), "got: {index}");
        assert!(index.contains("content-type: text/html"), "got: {index}");
        assert!(index.contains("Memories: 0"), "got: {index}");

        let api = raw_request(port, "GET /api/library?token=t HTTP/1.1\r\n\r\n").await;
        assert!(api.starts_with("HTTP/1.1 200"), "got: {api}");
        assert!(api.contains("content-type: application/json"), "got: {api}");
        let body = api.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["memories"], serde_json::json!([]));

        let missing = raw_request(port, "GET /nope?token=t HTTP/1.1\r\n\r\n").await;
        assert!(missing.starts_with("HTTP/1.1 404"), "got: {missing}");

        let post = raw_request(port, "POST /?token=t HTTP/1.1\r\ncontent-length: 0\r\n\r\n").await;
        assert!(post.starts_with("HTTP/1.1 405"), "got: {post}");

        let _ = tx.send(());
        handle.await.unwrap().unwrap();
    }

    /// Token strength: 128 bits of OS entropy per boot, unique across
    /// calls. A fixed-key hash over (pid, time, counter) would be
    /// reproducible from /proc + a port scan; the OS CSPRNG is not.
    #[test]
    fn access_token_is_128_bits_unique() {
        let tokens: Vec<String> = (0..100).map(|_| access_token()).collect();
        for t in &tokens {
            assert_eq!(t.len(), 32, "128-bit token as 32 hex chars, got {t:?}");
            assert!(
                t.bytes().all(|b| b.is_ascii_hexdigit()),
                "hex only, got {t:?}"
            );
        }
        let unique: std::collections::HashSet<&str> = tokens.iter().map(String::as_str).collect();
        assert_eq!(unique.len(), tokens.len(), "tokens must not repeat");
    }

    /// Ownership probe tolerates split delivery: headers and body in
    /// separate segments must still match (stopping at end-of-headers
    /// would report a live child as foreign).
    #[tokio::test]
    async fn owns_port_tolerates_split_delivery() {
        use tokio::io::AsyncWriteExt;
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            use tokio::io::AsyncReadExt;
            let _ = sock.read(&mut buf).await;
            // Headers now, body after a beat: separate segments.
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 30\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            sock.write_all(b"<html>ltmrs library index</html>")
                .await
                .unwrap();
        });
        let stream = TcpStream::connect((LOOPBACK, port)).await.unwrap();
        assert!(
            owns_port(stream, "tok").await,
            "split headers/body must still match"
        );
    }

    /// A squatter holding the connection open with no data must fail the
    /// probe at the deadline, never stall past it.
    #[tokio::test]
    async fn owns_port_times_out_on_silent_server() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let stream = TcpStream::connect((LOOPBACK, port)).await.unwrap();
        let probed =
            tokio::time::timeout(std::time::Duration::from_secs(8), owns_port(stream, "tok"))
                .await
                .expect("probe must return within its deadline");
        assert!(!probed, "silent server must fail the probe");
    }

    /// Token comparison is correctness-pinned (equal/unequal/length);
    /// timing hardness follows from the fixed full-length XOR walk.
    #[test]
    fn access_token_comparison_is_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"a", b""));
    }

    /// Same-user boundary: memory content requires the per-boot access
    /// token. Requests without (or with a wrong) token are denied, even on
    /// loopback (any local UID can reach loopback).
    #[tokio::test]
    async fn library_denied_without_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store").to_str().unwrap().to_string();
        drop(ltmrs_service::repository::CanonicalRepository::open(&store));
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(serve_with_token(
            listener,
            store,
            rx,
            "test-token-abc".to_string(),
        ));

        let denied = raw_request(port, "GET /api/library HTTP/1.1\r\nhost: x\r\n\r\n").await;
        assert!(
            denied.starts_with("HTTP/1.1 403"),
            "library without token must be denied, got: {denied}"
        );
        let wrong = raw_request(
            port,
            "GET /api/library?token=nope HTTP/1.1\r\nhost: x\r\n\r\n",
        )
        .await;
        assert!(
            wrong.starts_with("HTTP/1.1 403"),
            "wrong token must be denied, got: {wrong}"
        );
        let index_denied = raw_request(port, "GET / HTTP/1.1\r\nhost: x\r\n\r\n").await;
        assert!(
            index_denied.starts_with("HTTP/1.1 403"),
            "index without token must be denied, got: {index_denied}"
        );
        let ok = raw_request(
            port,
            "GET /api/library?token=test-token-abc HTTP/1.1\r\nhost: x\r\n\r\n",
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200"), "got: {ok}");

        let _ = tx.send(());
        handle.await.unwrap().unwrap();
    }

    /// A missing store fails fast with `no store at ...` (never serves 500s,
    /// never creates the store).
    #[tokio::test]
    async fn missing_store_fails_fast() {
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let err = serve(listener, "/nonexistent-dir-xyz/store".to_string(), rx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no store"), "got: {err}");
        assert!(!std::path::Path::new("/nonexistent-dir-xyz/store").exists());
    }

    /// `/api/export` serves one JSON object per memory as a download
    /// attachment (upstream parity): token-gated like every route,
    /// JSONL content type, filename pinned.
    #[tokio::test]
    async fn export_serves_jsonl_attachment() {
        use ltmrs_domain::id::{DocumentRevision, EligibilityRevision, EntityId, EntityRevision};
        use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};

        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store").to_str().unwrap().to_string();
        let repo = ltmrs_service::repository::CanonicalRepository::open(&store).unwrap();
        repo.put_memory_direct(&Memory {
            id: EntityId::new(uuid::Uuid::from_u128(1)),
            external_alias: None,
            title: "Export Me".to_string(),
            fragment: "exportable content".to_string(),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: None,
            source: MemorySource::Ai,
            confidence: 0.5,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: EntityRevision::new(1),
            document_revision: DocumentRevision::new(1),
            eligibility_revision: EligibilityRevision::new(1),
            created_at: Instant::new(100),
            updated_at: Instant::new(100),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        })
        .unwrap();
        drop(repo);
        let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(serve_with_token(
            listener,
            store,
            rx,
            "test-token-abc".to_string(),
        ));

        let denied = raw_request(port, "GET /api/export HTTP/1.1\r\nhost: x\r\n\r\n").await;
        assert!(
            denied.starts_with("HTTP/1.1 403"),
            "export without token must be denied, got: {denied}"
        );
        let wrong_method = raw_request(
            port,
            "POST /api/export?token=test-token-abc HTTP/1.1\r\nhost: x\r\n\r\n",
        )
        .await;
        assert!(
            wrong_method.starts_with("HTTP/1.1 405"),
            "export POST must be rejected, got: {wrong_method}"
        );
        let ok = raw_request(
            port,
            "GET /api/export?token=test-token-abc HTTP/1.1\r\nhost: x\r\n\r\n",
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200"), "got: {ok}");
        assert!(
            ok.contains("content-type: application/x-jsonlines"),
            "got: {ok}"
        );
        assert!(
            ok.contains("attachment; filename=memory-export.jsonl"),
            "got: {ok}"
        );
        let body = ok.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(body.lines().count(), 1);
        let row: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(row["title"], "Export Me");

        let _ = tx.send(());
        handle.await.unwrap().unwrap();
    }
}

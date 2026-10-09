//! Frontend-side IPC client (design §7.1).
//!
//! The frontend terminates stdio (via rmcp) and sends typed domain requests
//! over IPC to the daemon. This client connects to the daemon socket, writes
//! length-prefixed request frames, and reads streamed responses. It supports
//! reconnection for resilience.

use tokio::io::AsyncWriteExt;

use crate::envelope::{
    HandshakeRequest, HandshakeResponse, IpcError, IpcResponse, WireMessage, WireReply,
    read_response_payload,
};
use crate::runtime::IpcStream;

/// Parse a generation-mismatch wire error back into its typed form.
/// Message shape (from `IpcError::GenerationMismatch`'s Display):
/// "store generation mismatch: daemon {d}, client {c}".
/// Parse a typed handshake rejection: generation mismatches (adoptable)
/// and transient daemon-busy (retryable) survive the wire; anything else
/// stays a generic rejection.
fn parse_handshake_error(kind: &str, message: &str) -> Option<IpcError> {
    if kind == "daemon_busy" {
        let inner = message.strip_prefix("daemon busy: ").unwrap_or(message);
        return Some(IpcError::Busy(inner.to_string()));
    }
    if kind == "stale_namespace" {
        let inner = message
            .strip_prefix("stale retry namespace: ")
            .unwrap_or(message);
        return Some(IpcError::StaleNamespace(inner.to_string()));
    }
    parse_generation_mismatch(kind, message)
}

fn parse_generation_mismatch(kind: &str, message: &str) -> Option<IpcError> {
    if kind != "generation_mismatch" {
        return None;
    }
    let (_, rest) = message.split_once("daemon ")?;
    let (daemon, rest) = rest.split_once(", client ")?;
    let daemon: u64 = daemon.trim().parse().ok()?;
    let client: u64 = rest.trim().parse().ok()?;
    Some(IpcError::GenerationMismatch { daemon, client })
}

/// A typed IPC client used by a frontend to talk to the daemon.
pub struct IpcClient {
    /// The daemon IPC endpoint to connect to (Unix socket path, Windows
    /// named-pipe name).
    endpoint: std::path::PathBuf,
    /// The current connection (None when disconnected).
    stream: Option<IpcStream>,
    /// The retry epoch issued by the daemon handshake (None until handshaked).
    retry_epoch: Option<u64>,
    /// In-process bridge (stdio mode): the stream was handed over directly
    /// and no socket listener exists to redial. Dialing the daemon's
    /// bound-but-unaccepted socket path would park the caller forever.
    bridged: bool,
}

impl IpcClient {
    pub fn new(endpoint: std::path::PathBuf) -> Self {
        Self {
            endpoint,
            stream: None,
            retry_epoch: None,
            bridged: false,
        }
    }

    /// Connect to the daemon (or reconnect if already connected). On a
    /// bridged client this never dials: a live bridge stream is kept
    /// as-is, and a closed one fails loudly — there is no listener to
    /// redial in-process.
    pub async fn connect(&mut self) -> Result<(), IpcError> {
        if self.bridged {
            if self.stream.is_some() {
                return Ok(());
            }
            return Err(IpcError::NotConnected);
        }
        #[cfg(unix)]
        let stream = tokio::net::UnixStream::connect(&self.endpoint).await?;
        #[cfg(windows)]
        let stream = {
            use tokio::net::windows::named_pipe::ClientOptions;
            // The daemon creates one pipe instance per accept: opening a
            // name with no live instance fails with NotFound (2) / pipe-not-
            // available (231), so retry briefly until the accept side
            // publishes one. Bounded: a daemon that never serves must fail
            // the connect, not hang it.
            let start = std::time::Instant::now();
            loop {
                match ClientOptions::new().open(&self.endpoint) {
                    Ok(client) => {
                        // `open` succeeded against a live server instance,
                        // so the connection is established; `readable()`
                        // would gate on incoming data and deadlock the
                        // request-first protocol.
                        break client;
                    }
                    Err(e)
                        if matches!(e.raw_os_error(), Some(2) | Some(231))
                            && start.elapsed() < std::time::Duration::from_secs(10) =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };
        self.stream = Some(Box::new(stream));
        self.retry_epoch = None;
        Ok(())
    }

    /// Perform the connect-time handshake: validate protocol/generation and
    /// receive the retry namespace epoch. Must succeed before sending requests.
    pub async fn handshake(
        &mut self,
        req: &HandshakeRequest,
    ) -> Result<HandshakeResponse, IpcError> {
        let stream = self.stream.as_mut().ok_or(IpcError::NotConnected)?;
        let msg = WireMessage::Handshake(req.clone());
        let payload = serde_json::to_vec(&msg)?;
        if payload.len() > crate::envelope::MAX_FRAME_BYTES {
            return Err(IpcError::FrameTooLarge(payload.len()));
        }
        stream
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;

        let bytes = read_response_payload(stream).await?;
        let reply: WireReply =
            serde_json::from_slice(&bytes).map_err(|e| IpcError::InvalidJson(e.to_string()))?;
        match reply {
            WireReply::Handshake(hs) => {
                self.retry_epoch = Some(hs.retry_epoch);
                Ok(hs)
            }
            WireReply::Response(_) => Err(IpcError::InvalidJson(
                "expected handshake reply, got response".into(),
            )),
            WireReply::Error(err) => Err(parse_handshake_error(&err.kind, &err.message)
                .unwrap_or_else(|| {
                    IpcError::InvalidJson(format!(
                        "handshake rejected: {} — {}",
                        err.kind, err.message
                    ))
                })),
        }
    }

    /// The retry epoch from the last handshake (None until handshaked).
    pub fn retry_epoch(&self) -> Option<u64> {
        self.retry_epoch
    }

    /// Set an existing stream (for tests and the in-process bridge).
    pub fn set_stream(&mut self, stream: IpcStream) {
        self.stream = Some(stream);
        self.bridged = true;
    }

    /// Whether the client is connected.
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// The IPC endpoint this client targets.
    pub fn endpoint(&self) -> &std::path::Path {
        &self.endpoint
    }

    /// Send a tagged request frame over the current connection.
    pub async fn write_request(
        &mut self,
        envelope: &crate::envelope::IpcEnvelope,
    ) -> Result<(), IpcError> {
        let stream = self.stream.as_mut().ok_or(IpcError::NotConnected)?;
        let msg = WireMessage::Request(Box::new(envelope.clone()));
        let payload = serde_json::to_vec(&msg)?;
        // A request is a single length-prefixed frame (bounded).
        if payload.len() > crate::envelope::MAX_FRAME_BYTES {
            return Err(IpcError::FrameTooLarge(payload.len()));
        }
        stream
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;
        Ok(())
    }

    /// Read a streamed response payload from the current connection.
    pub async fn read_response(&mut self) -> Result<Vec<u8>, IpcError> {
        let stream = self.stream.as_mut().ok_or(IpcError::NotConnected)?;
        read_response_payload(stream).await
    }

    /// Read a tagged reply and unwrap it as an IpcResponse.
    pub async fn read_ipc_response(&mut self) -> Result<IpcResponse, IpcError> {
        let bytes = self.read_response().await?;
        let reply: WireReply =
            serde_json::from_slice(&bytes).map_err(|e| IpcError::InvalidJson(e.to_string()))?;
        match reply {
            WireReply::Response(resp) => Ok(resp),
            WireReply::Handshake(_) => Err(IpcError::InvalidJson(
                "expected response, got handshake reply".into(),
            )),
            WireReply::Error(err) => Err(IpcError::InvalidJson(format!(
                "wire error: {} — {}",
                err.kind, err.message
            ))),
        }
    }

    /// Close the current connection.
    pub fn close(&mut self) {
        self.stream = None;
    }

    /// Forget handshake state so the next call re-handshakes from scratch.
    /// A stale store generation invalidates the epoch: without this, the
    /// frontend would keep sending the dead epoch forever.
    pub fn forget_handshake(&mut self) {
        self.stream = None;
        self.retry_epoch = None;
    }

    /// Drop only the retry epoch, keeping a live stream: a StaleGeneration
    /// reply arrives over a healthy connection, so redialing would abandon
    /// it pointlessly — and on a bridged client fatally (no listener).
    pub fn forget_epoch(&mut self) {
        self.retry_epoch = None;
    }
}

/// An error for when an operation requires a connection.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("not connected")]
    NotConnected,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ipc: {0}")]
    Ipc(#[from] IpcError),
}

impl IpcClient {
    /// Connect, write a request, read a response — one round-trip.
    pub async fn roundtrip(
        &mut self,
        envelope: &crate::envelope::IpcEnvelope,
    ) -> Result<IpcResponse, ClientError> {
        if !self.is_connected() {
            self.connect().await?;
        }
        self.write_request(envelope)
            .await
            .map_err(ClientError::Ipc)?;
        self.read_ipc_response().await.map_err(ClientError::Ipc)
    }

    /// Round-trip, forgetting handshake state on transport failure so the
    /// next call reconnects from scratch instead of failing on a dead
    /// stream forever (broken pipe, daemon restart, rejected socket).
    pub async fn roundtrip_or_forget(
        &mut self,
        envelope: &crate::envelope::IpcEnvelope,
    ) -> Result<IpcResponse, ClientError> {
        match self.roundtrip(envelope).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                self.forget_handshake();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::DomainRequest;
    use crate::envelope::{IpcEnvelope, PROTOCOL_VERSION};
    use crate::runtime::RuntimePaths;
    use crate::server::{Daemon, DaemonConfig, handle_connection};
    use ltmrs_domain::command::Scope;
    use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
    use uuid::Uuid;

    fn envelope(op: u64) -> IpcEnvelope {
        IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            operation_id: OperationId::new(Uuid::from_u128(op as u128)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::GetMemories { ids: vec![] },
        }
    }

    fn handshake_req() -> HandshakeRequest {
        HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            resume_retry_epoch: None,
        }
    }

    /// The mismatch recovery parses the live `Display` string: if anyone
    /// rewords it, recovery must fail loudly here, not silently brick
    /// frontends post-restore.
    #[test]
    fn generation_mismatch_parse_tracks_display() {
        let err = crate::envelope::IpcError::GenerationMismatch {
            daemon: 7,
            client: 3,
        };
        let parsed = parse_generation_mismatch("generation_mismatch", &err.to_string()).unwrap();
        assert!(
            matches!(
                parsed,
                crate::envelope::IpcError::GenerationMismatch {
                    daemon: 7,
                    client: 3
                }
            ),
            "round-trip failed for: {err}"
        );
        assert!(parse_generation_mismatch("handshake_rejected", &err.to_string()).is_none());
        assert!(parse_generation_mismatch("generation_mismatch", "garbage").is_none());
    }

    /// The transient-busy kind must survive the wire (round-trip through
    /// Display) so frontends can tell "retry" from "refused".
    #[test]
    fn daemon_busy_parse_tracks_display() {
        let err = crate::envelope::IpcError::Busy("namespace contention".to_string());
        let parsed = parse_handshake_error("daemon_busy", &err.to_string()).unwrap();
        assert!(
            matches!(
                parsed,
                crate::envelope::IpcError::Busy(ref m) if m == "namespace contention"
            ),
            "round-trip failed for: {err}"
        );
        assert!(parse_handshake_error("handshake_rejected", &err.to_string()).is_none());
    }

    #[tokio::test]
    async fn client_roundtrips_a_request_over_ipc() {
        // Start a daemon (owns the store + dispatcher).
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        let dispatcher = daemon.dispatcher_arc();
        let quotas = daemon.quotas();

        // Simulate a connected frontend/daemon pair with a duplex stream.
        let (client_stream, server_stream) = tokio::io::duplex(65536);
        let server_handle = tokio::spawn(async move {
            let _ = handle_connection(Box::new(server_stream), dispatcher, quotas).await;
        });

        // Client uses the paired stream: handshake, then round-trip.
        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(Box::new(client_stream));
        assert!(client.is_connected());

        // The handshake validates protocol/generation and issues the epoch.
        let hs = client.handshake(&handshake_req()).await.unwrap();
        assert!(hs.retry_epoch >= 1, "handshake must issue a retry epoch");
        assert_eq!(client.retry_epoch(), Some(hs.retry_epoch));

        // Now a request round-trips.
        let env = envelope(1);
        let resp = client.roundtrip(&env).await.unwrap();
        assert_eq!(resp.operation_id.as_uuid(), Uuid::from_u128(1));

        client.close();
        drop(daemon);
        let _ = server_handle.await;
    }

    /// Transport failure forgets handshake state: a dead stream (peer
    /// gone) errors the round-trip and resets epoch + connection, so the
    /// next call reconnects instead of failing forever.
    #[tokio::test]
    async fn roundtrip_or_forget_resets_on_dead_stream() {
        let (dead, peer) = tokio::io::duplex(64);
        drop(peer);
        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(Box::new(dead));
        client.retry_epoch = Some(7);
        assert!(client.roundtrip_or_forget(&envelope(1)).await.is_err());
        assert_eq!(
            client.retry_epoch(),
            None,
            "transport failure must forget the epoch"
        );
        assert!(
            !client.is_connected(),
            "transport failure must drop the dead stream"
        );
    }

    /// Bridged clients never dial the socket path: connect() keeps a live
    /// bridge stream (no-op) and fails loudly without one — dialing the
    /// daemon's bound-but-unaccepted listener would park the caller
    /// forever with nobody to answer.
    #[tokio::test]
    async fn bridged_connect_never_dials_socket() {
        let (a, _peer) = tokio::io::duplex(64);
        let mut client =
            IpcClient::new(std::path::PathBuf::from("/nonexistent-dir-xyz/daemon.sock"));
        client.set_stream(Box::new(a));
        client.retry_epoch = Some(7);
        assert!(client.connect().await.is_ok());
        assert!(client.is_connected(), "live bridge must survive connect");
        assert_eq!(client.retry_epoch(), Some(7), "epoch must survive connect");
        client.close();
        assert!(
            client.connect().await.is_err(),
            "closed bridge must fail loudly, never dial"
        );
    }

    #[tokio::test]
    async fn handshake_rejects_wrong_generation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        let dispatcher = daemon.dispatcher_arc();
        let quotas = daemon.quotas();

        let (client_stream, server_stream) = tokio::io::duplex(65536);
        let server_handle = tokio::spawn(async move {
            let _ = handle_connection(Box::new(server_stream), dispatcher, quotas).await;
        });

        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(Box::new(client_stream));

        // A wrong store generation must be rejected.
        let mut bad = handshake_req();
        bad.store_generation = StoreGeneration::new(99);
        let result = client.handshake(&bad).await;
        assert!(
            result.is_err(),
            "handshake with wrong generation must be rejected"
        );

        client.close();
        drop(daemon);
        let _ = server_handle.await;
    }

    /// A generation rejection must stay typed across the wire (the frontend
    /// adopts the daemon generation and retries; stringly errors brick it).
    #[tokio::test]
    async fn handshake_generation_mismatch_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        let dispatcher = daemon.dispatcher_arc();
        let quotas = daemon.quotas();

        let (client_stream, server_stream) = tokio::io::duplex(65536);
        let server_handle = tokio::spawn(async move {
            let _ = handle_connection(Box::new(server_stream), dispatcher, quotas).await;
        });

        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(Box::new(client_stream));

        let mut bad = handshake_req();
        bad.store_generation = StoreGeneration::new(99);
        let err = client.handshake(&bad).await.unwrap_err();
        assert!(
            matches!(
                err,
                crate::envelope::IpcError::GenerationMismatch {
                    daemon: 1,
                    client: 99
                }
            ),
            "mismatch must stay typed, got: {err:?}"
        );

        client.close();
        drop(daemon);
        let _ = server_handle.await;
    }

    #[test]
    fn client_reports_not_connected() {
        let client = IpcClient::new(std::path::PathBuf::from("/nonexistent"));
        assert!(!client.is_connected());
    }

    /// P1-B: a stale-namespace refusal must stay typed across the wire (the
    /// frontend matches it to surface an unknown outcome instead of minting
    /// a silent fresh epoch; stringly errors would brick that decision).
    #[test]
    fn stale_namespace_refusal_is_typed() {
        let err = parse_handshake_error("stale_namespace", "stale retry namespace: gone")
            .expect("stale_namespace must parse");
        assert!(
            matches!(err, crate::envelope::IpcError::StaleNamespace(_)),
            "refusal must stay typed, got: {err:?}"
        );
        assert!(
            parse_handshake_error("handshake_rejected", "nope").is_none(),
            "other rejections stay untyped"
        );
    }
}

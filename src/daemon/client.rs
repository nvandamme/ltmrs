//! Frontend-side IPC client (design §7.1).
//!
//! The frontend terminates stdio (via rmcp) and sends typed domain requests
//! over IPC to the daemon. This client connects to the daemon socket, writes
//! length-prefixed request frames, and reads streamed responses. It supports
//! reconnection for resilience.

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::daemon::envelope::{
    HandshakeRequest, HandshakeResponse, IpcError, IpcResponse, WireMessage, WireReply,
    read_response_payload,
};

/// A typed IPC client used by a frontend to talk to the daemon.
pub struct IpcClient {
    /// The daemon socket path to connect to.
    socket_path: std::path::PathBuf,
    /// The current connection (None when disconnected).
    stream: Option<UnixStream>,
    /// The retry epoch issued by the daemon handshake (None until handshaked).
    retry_epoch: Option<u64>,
}

impl IpcClient {
    pub fn new(socket_path: std::path::PathBuf) -> Self {
        Self {
            socket_path,
            stream: None,
            retry_epoch: None,
        }
    }

    /// Connect to the daemon (or reconnect if already connected).
    pub async fn connect(&mut self) -> Result<(), IpcError> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        self.stream = Some(stream);
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
        if payload.len() > crate::daemon::envelope::MAX_FRAME_BYTES {
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
            WireReply::Error(err) => Err(IpcError::InvalidJson(format!(
                "handshake rejected: {} — {}",
                err.kind, err.message
            ))),
        }
    }

    /// The retry epoch from the last handshake (None until handshaked).
    pub fn retry_epoch(&self) -> Option<u64> {
        self.retry_epoch
    }

    /// Set an existing stream (for tests using a stream pair).
    pub fn set_stream(&mut self, stream: UnixStream) {
        self.stream = Some(stream);
    }

    /// Whether the client is connected.
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// The socket path this client targets.
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Send a tagged request frame over the current connection.
    pub async fn write_request(
        &mut self,
        envelope: &crate::daemon::envelope::IpcEnvelope,
    ) -> Result<(), IpcError> {
        let stream = self.stream.as_mut().ok_or(IpcError::NotConnected)?;
        let msg = WireMessage::Request(Box::new(envelope.clone()));
        let payload = serde_json::to_vec(&msg)?;
        // A request is a single length-prefixed frame (bounded).
        if payload.len() > crate::daemon::envelope::MAX_FRAME_BYTES {
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
        envelope: &crate::daemon::envelope::IpcEnvelope,
    ) -> Result<IpcResponse, ClientError> {
        if !self.is_connected() {
            self.connect().await?;
        }
        self.write_request(envelope)
            .await
            .map_err(ClientError::Ipc)?;
        self.read_ipc_response().await.map_err(ClientError::Ipc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::envelope::DomainRequest;
    use crate::daemon::envelope::{IpcEnvelope, PROTOCOL_VERSION};
    use crate::daemon::runtime::RuntimePaths;
    use crate::daemon::server::{Daemon, DaemonConfig, handle_connection};
    use crate::domain::command::Scope;
    use crate::domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
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
        }
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

        // Simulate a connected frontend/daemon pair with a stream pair.
        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = handle_connection(server_stream, dispatcher, quotas).await;
        });

        // Client uses the paired stream: handshake, then round-trip.
        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_stream);
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

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = handle_connection(server_stream, dispatcher, quotas).await;
        });

        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_stream);

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

    #[test]
    fn client_reports_not_connected() {
        let client = IpcClient::new(std::path::PathBuf::from("/nonexistent"));
        assert!(!client.is_connected());
    }
}

//! Private IPC envelope and framing for the ltmrs daemon (design §7.1).
//!
//! Every request carries the frontend/channel identity, store generation,
//! operation ID, session binding, deadline and a typed body. Frames are
//! length-prefixed UTF-8 JSON with a hard size cap so a malicious or buggy
//! client cannot exhaust memory. The frame limit is a parse bound, never a
//! silent result truncation: large responses stream in bounded frames.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::compatibility::lemma::tool_args::ToolArgs;
use crate::domain::command::{
    CommandContext, DomainCommand, DomainError, DomainErrorCode, DomainResult, MemoryPatch,
    ReceiptOutcome, Scope,
};
use crate::domain::id::{
    ChannelId, EntityId, EntityRevision, FrontendId, OperationId, SessionHandle, StoreGeneration,
};
use crate::domain::memory::Memory;
use crate::domain::relation::{Relation, RelationType};
use crate::domain::session::{AttemptOutcome, TaskOutcome};
use serde_json::Value;

/// The IPC protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Hard cap on a single IPC frame (bytes). Larger frames are rejected, not
/// truncated — large results stream across multiple frames.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Wire error for the IPC layer (distinct from domain errors).
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("frame too large: {0} bytes > {MAX_FRAME_BYTES}")]
    FrameTooLarge(usize),
    #[error("incomplete frame: need {need} bytes, have {have}")]
    IncompleteFrame { need: usize, have: usize },
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("unsupported protocol version: {0}")]
    UnsupportedProtocol(u32),
    #[error("store generation mismatch: daemon {daemon}, client {client}")]
    GenerationMismatch { daemon: u64, client: u64 },
    #[error("unauthorized frontend")]
    Unauthorized,
    #[error("daemon busy: {0}")]
    Busy(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("not connected to daemon")]
    NotConnected,
}

/// A typed domain request body carried in an IPC envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DomainRequest {
    AddMemory {
        memory: Memory,
    },
    UpdateMemory {
        id: EntityId,
        expected_revision: Option<EntityRevision>,
        patch: MemoryPatch,
    },
    Feedback {
        memory_id: EntityId,
        useful: bool,
    },
    Relate {
        relation: Relation,
    },
    Unrelate {
        source: EntityId,
        target: EntityId,
        relation_type: RelationType,
    },
    Merge {
        source_ids: Vec<EntityId>,
        result: Memory,
    },
    Forget {
        id: EntityId,
        mode: crate::domain::command::ForgetMode,
    },
    SessionAttempt {
        approach: String,
        outcome: AttemptOutcome,
        critique: Option<String>,
        rationale: Option<String>,
        related_memory_id: Option<EntityId>,
    },
    SessionEnd {
        outcome: TaskOutcome,
        final_approach: Option<String>,
        lessons: Vec<String>,
    },
    GetMemories {
        ids: Vec<EntityId>,
    },
    /// List all canonical memories (read-only). Used by the frontend to build
    /// the dynamic instructions index (WP-08).
    ListMemories,
    Neighbors {
        id: EntityId,
    },
    /// A legacy MCP tool call (WP-08). The daemon executes the tool and returns
    /// a shaped legacy wire result (text + structured + error flag).
    ToolCall {
        tool: ToolArgs,
    },
}

impl DomainRequest {
    /// Convert to the canonical domain command (mutations only).
    pub fn to_domain_command(&self, session: Option<SessionHandle>) -> Option<DomainCommand> {
        match self {
            DomainRequest::AddMemory { memory } => Some(DomainCommand::AddMemory {
                memory: memory.clone(),
                session,
            }),
            DomainRequest::UpdateMemory {
                id,
                expected_revision,
                patch,
            } => Some(DomainCommand::UpdateMemory {
                id: *id,
                expected_revision: *expected_revision,
                patch: patch.clone(),
            }),
            DomainRequest::Feedback { memory_id, useful } => Some(DomainCommand::Feedback {
                memory_id: *memory_id,
                useful: *useful,
            }),
            DomainRequest::Relate { relation } => Some(DomainCommand::Relate {
                relation: relation.clone(),
            }),
            DomainRequest::Unrelate {
                source,
                target,
                relation_type,
            } => Some(DomainCommand::Unrelate {
                source: *source,
                target: *target,
                relation_type: *relation_type,
            }),
            DomainRequest::Merge { source_ids, result } => Some(DomainCommand::Merge {
                source_ids: source_ids.clone(),
                result: result.clone(),
            }),
            DomainRequest::Forget { id, mode } => Some(DomainCommand::Forget {
                id: *id,
                mode: *mode,
            }),
            DomainRequest::SessionEnd {
                outcome,
                final_approach,
                lessons,
                ..
            } => Some(DomainCommand::EndSession {
                session: session?,
                outcome: *outcome,
                final_approach: final_approach.clone(),
                lessons: lessons.clone(),
            }),
            // Reads and session bookkeeping are handled by the dispatcher, not
            // the canonical command gateway.
            DomainRequest::SessionAttempt { .. }
            | DomainRequest::GetMemories { .. }
            | DomainRequest::ListMemories
            | DomainRequest::Neighbors { .. }
            | DomainRequest::ToolCall { .. } => None,
        }
    }
}

/// A typed result payload returned with a successful command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DomainPayload {
    Memories(Vec<Memory>),
    Relations(Vec<Relation>),
    /// A shaped legacy MCP tool result (WP-08): the human-readable text, the
    /// structured payload (when the tool has an output schema) and the tool
    /// error flag. The frontend wraps this verbatim into the wire response.
    ToolResult {
        text: String,
        structured: Option<Value>,
        is_error: bool,
    },
    None,
}

/// A typed IPC response from the daemon to a frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub protocol_version: u32,
    pub operation_id: OperationId,
    pub result: IpcResult,
}

/// The result of an IPC request: a domain success (with receipt + payload) or
/// a domain error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResult {
    Success {
        outcome: ReceiptOutcome,
        payload: DomainPayload,
    },
    Error {
        code: DomainErrorCode,
        message: String,
    },
}

impl IpcResponse {
    pub fn success(
        operation_id: OperationId,
        outcome: ReceiptOutcome,
        payload: DomainPayload,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            operation_id,
            result: IpcResult::Success { outcome, payload },
        }
    }

    pub fn error(operation_id: OperationId, err: &DomainError) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            operation_id,
            result: IpcResult::Error {
                code: err.code,
                message: err.message.clone(),
            },
        }
    }
}

/// The IPC envelope: the full header + typed body of one request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEnvelope {
    pub protocol_version: u32,
    pub store_generation: StoreGeneration,
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
    pub operation_id: OperationId,
    pub session: Option<SessionHandle>,
    pub retry_epoch: u64,
    pub deadline_millis: Option<u64>,
    pub scope: Scope,
    pub body: DomainRequest,
}

impl IpcEnvelope {
    /// Build the canonical command context from this envelope. The request
    /// digest is computed by the caller from the canonical command bytes.
    pub fn to_command_context(&self, request_digest: String) -> CommandContext {
        CommandContext {
            store_generation: self.store_generation,
            frontend_id: self.frontend_id,
            channel_id: self.channel_id,
            session: self.session,
            operation_id: self.operation_id,
            request_digest,
            deadline_millis: self.deadline_millis,
            scope: self.scope.clone(),
            retry_epoch: self.retry_epoch,
        }
    }

    /// Validate the protocol version.
    pub fn check_protocol(&self) -> Result<(), IpcError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(IpcError::UnsupportedProtocol(self.protocol_version));
        }
        Ok(())
    }

    /// Canonical request digest: a stable hash of the typed body so two
    /// identical logical requests share a digest and a reused key with
    /// different input does not.
    pub fn request_digest(&self) -> DomainResult<String> {
        let bytes = serde_json::to_vec(&self.body)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(sha256_hex(&bytes))
    }
}

fn sha256_hex(input: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input);
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Encode a payload into a length-prefixed frame.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, IpcError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Decode one frame from the front of a buffer. Returns the payload and
/// advances the buffer. Errors on oversized or incomplete frames.
pub fn decode_frame(buf: &mut &[u8]) -> Result<Vec<u8>, IpcError> {
    if buf.len() < 4 {
        return Err(IpcError::IncompleteFrame {
            need: 4,
            have: buf.len(),
        });
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&buf[..4]);
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge(len));
    }
    if buf.len() < 4 + len {
        return Err(IpcError::IncompleteFrame {
            need: 4 + len,
            have: buf.len(),
        });
    }
    let payload = buf[4..4 + len].to_vec();
    *buf = &buf[4 + len..];
    Ok(payload)
}

/// Decode a JSON envelope from a frame payload.
pub fn parse_envelope(payload: &[u8]) -> Result<IpcEnvelope, IpcError> {
    serde_json::from_slice(payload).map_err(|e| IpcError::InvalidJson(e.to_string()))
}

/// The first frame on every connection: the handshake that validates identity,
/// protocol, and store generation, and receives the retry namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRequest {
    pub protocol_version: u32,
    pub store_generation: StoreGeneration,
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponse {
    pub protocol_version: u32,
    pub store_generation: StoreGeneration,
    pub retry_epoch: u64,
}

/// Validate a handshake request against the daemon's actual state.
/// Returns the retry epoch to issue on success, or a wire error.
pub fn validate_handshake(
    req: &HandshakeRequest,
    daemon_generation: StoreGeneration,
) -> Result<(), IpcError> {
    if req.protocol_version != PROTOCOL_VERSION {
        return Err(IpcError::UnsupportedProtocol(req.protocol_version));
    }
    if req.store_generation != daemon_generation {
        return Err(IpcError::GenerationMismatch {
            daemon: daemon_generation.as_u64(),
            client: req.store_generation.as_u64(),
        });
    }
    Ok(())
}

/// A tagged wire message: either the connect-time handshake or a typed
/// domain request. The `kind` tag is how the daemon tells the two apart on
/// the same socket — the first frame must be a handshake. The request is
/// boxed to keep the enum small (the envelope carries a full domain body).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireMessage {
    Handshake(HandshakeRequest),
    Request(Box<IpcEnvelope>),
}

/// A tagged wire reply: the handshake ack, a typed domain response, or a
/// wire-level rejection (bad handshake, protocol violation). Adjacently
/// tagged: the inner `WireError.kind` string would collide with an
/// internally-tagged envelope (`duplicate field 'kind'`), making every
/// rejection unparseable by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum WireReply {
    Handshake(HandshakeResponse),
    Response(IpcResponse),
    Error(WireError),
}

/// A wire error response sent back when a handshake (or any frame) is rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub kind: String,
    pub message: String,
}

/// Split a payload into bounded chunks, each at most `MAX_FRAME_BYTES`.
/// A payload within the cap yields a single chunk; larger payloads split.
/// Reassembling the chunks in order yields the exact original (no truncation).
pub fn split_payload(payload: &[u8]) -> Result<Vec<Vec<u8>>, IpcError> {
    if payload.is_empty() {
        return Ok(Vec::new());
    }
    if payload.len() <= MAX_FRAME_BYTES {
        return Ok(vec![payload.to_vec()]);
    }
    let mut chunks = Vec::with_capacity(payload.len() / MAX_FRAME_BYTES + 1);
    for chunk in payload.chunks(MAX_FRAME_BYTES) {
        chunks.push(chunk.to_vec());
    }
    Ok(chunks)
}

/// Write a response payload as a stream: an 8-byte total-length header
/// (u64 BE) followed by length-prefixed chunk frames, each bounded by
/// `MAX_FRAME_BYTES`. Uniform for small and large payloads.
pub async fn write_response_payload(
    stream: &mut (impl AsyncWriteExt + Unpin),
    payload: &[u8],
) -> Result<(), IpcError> {
    let total = payload.len() as u64;
    stream.write_all(&total.to_be_bytes()).await?;
    for chunk in split_payload(payload)? {
        stream
            .write_all(&(chunk.len() as u32).to_be_bytes())
            .await?;
        stream.write_all(&chunk).await?;
    }
    stream.flush().await?;
    Ok(())
}

/// Read a streamed response payload: read the 8-byte total length, then read
/// length-prefixed chunk frames until the total is reached. Returns the
/// reassembled payload (byte-exact, never truncated).
pub async fn read_response_payload(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> Result<Vec<u8>, IpcError> {
    let mut total_buf = [0u8; 8];
    stream.read_exact(&mut total_buf).await?;
    let total = u64::from_be_bytes(total_buf) as usize;

    let mut out = Vec::with_capacity(total.min(MAX_FRAME_BYTES * 4));
    while out.len() < total {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(IpcError::FrameTooLarge(len));
        }
        let mut chunk = vec![0u8; len];
        stream.read_exact(&mut chunk).await?;
        if out.len() + len > total {
            return Err(IpcError::IncompleteFrame {
                need: total,
                have: out.len() + len,
            });
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::EntityId;
    use uuid::Uuid;

    fn sample_envelope() -> IpcEnvelope {
        IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            operation_id: OperationId::new(Uuid::from_u128(3)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::GetMemories {
                ids: vec![EntityId::new(Uuid::from_u128(4))],
            },
        }
    }

    #[test]
    fn frame_roundtrip() {
        let payload = b"hello world";
        let frame = encode_frame(payload).unwrap();
        let mut buf = frame.as_slice();
        let decoded = decode_frame(&mut buf).unwrap();
        assert_eq!(decoded, payload);
        assert!(buf.is_empty());
    }

    /// Every reply shape — including rejections — must survive a
    /// serialize/parse round trip (the inner `WireError.kind` once collided
    /// with internal tagging, making all rejections unparseable).
    #[test]
    fn wire_reply_error_roundtrips() {
        for reply in [
            WireReply::Error(WireError {
                kind: "handshake_rejected".into(),
                message: "bad generation".into(),
            }),
            WireReply::Error(WireError {
                kind: "unauthorized".into(),
                message: "wrong uid".into(),
            }),
        ] {
            let bytes = serde_json::to_vec(&reply).unwrap();
            match serde_json::from_slice::<WireReply>(&bytes).unwrap() {
                WireReply::Error(err) => assert!(!err.kind.is_empty()),
                other => panic!("expected error reply, got: {other:?}"),
            }
        }
    }

    #[test]
    fn oversized_frame_rejected() {
        let big = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(
            encode_frame(&big),
            Err(IpcError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn oversized_length_prefix_rejected() {
        // A malicious length prefix claiming more than the cap.
        let mut frame = Vec::new();
        frame.extend_from_slice(&((MAX_FRAME_BYTES as u32 + 1).to_be_bytes()));
        frame.extend_from_slice(&[0u8; 8]);
        let mut buf = frame.as_slice();
        assert!(matches!(
            decode_frame(&mut buf),
            Err(IpcError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn incomplete_frame_reported() {
        let frame = encode_frame(b"abcdef").unwrap();
        // Only give part of the frame.
        let mut buf: &[u8] = &frame[..5];
        assert!(matches!(
            decode_frame(&mut buf),
            Err(IpcError::IncompleteFrame { .. })
        ));
    }

    #[test]
    fn envelope_json_roundtrip() {
        let env = sample_envelope();
        let json = serde_json::to_vec(&env).unwrap();
        let parsed = parse_envelope(&json).unwrap();
        assert_eq!(parsed.operation_id, env.operation_id);
        assert_eq!(parsed.frontend_id, env.frontend_id);
    }

    #[test]
    fn envelope_rejects_bad_json() {
        assert!(matches!(
            parse_envelope(b"not json"),
            Err(IpcError::InvalidJson(_))
        ));
    }

    #[test]
    fn request_digest_is_stable_and_input_sensitive() {
        let a = sample_envelope();
        let mut b = sample_envelope();
        b.body = DomainRequest::GetMemories { ids: vec![] };
        assert_eq!(a.request_digest().unwrap(), a.request_digest().unwrap());
        assert_ne!(a.request_digest().unwrap(), b.request_digest().unwrap());
    }

    #[test]
    fn protocol_version_check() {
        let env = sample_envelope();
        assert!(env.check_protocol().is_ok());
        let mut bad = sample_envelope();
        bad.protocol_version = 99;
        assert!(matches!(
            bad.check_protocol(),
            Err(IpcError::UnsupportedProtocol(99))
        ));
    }

    #[test]
    fn oversized_payload_splits_into_bounded_frames() {
        // A payload larger than the frame cap must split into multiple frames,
        // each within the cap.
        let payload = "x".repeat(MAX_FRAME_BYTES * 3 + 4096).into_bytes();
        assert!(payload.len() > MAX_FRAME_BYTES);
        let frames = split_payload(&payload).unwrap();
        assert!(frames.len() >= 4, "must split into multiple frames");
        for f in &frames {
            assert!(f.len() <= MAX_FRAME_BYTES, "every frame within the cap");
        }
        // Reassembling the frames yields the exact original (no truncation).
        let mut buf = Vec::new();
        for f in &frames {
            buf.extend_from_slice(f);
        }
        assert_eq!(buf, payload);
    }

    #[test]
    fn small_payload_is_a_single_frame() {
        let payload = b"tiny".to_vec();
        let frames = split_payload(&payload).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], payload);
    }

    #[tokio::test]
    async fn oversized_response_roundtrips_over_a_stream() {
        let (mut client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let payload = "y".repeat(MAX_FRAME_BYTES + 1024).into_bytes();
        let expected = payload.clone();
        tokio::spawn(async move {
            write_response_payload(&mut server, &payload).await.unwrap();
        });
        let reassembled = read_response_payload(&mut client).await.unwrap();
        assert_eq!(reassembled, expected, "reassembled response is byte-exact");
    }

    #[tokio::test]
    async fn small_response_roundtrips_as_single_frame() {
        let (mut client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let payload = b"{\"hello\":\"world\"}".to_vec();
        let expected = payload.clone();
        tokio::spawn(async move {
            write_response_payload(&mut server, &payload).await.unwrap();
        });
        let reassembled = read_response_payload(&mut client).await.unwrap();
        assert_eq!(reassembled, expected);
    }
}

//! Single-connection handling and wire I/O (moved verbatim from `server.rs`).

use std::sync::Arc;

use tokio::io::AsyncReadExt;

use super::DaemonError;
use super::guards::{ClientGuard, LiveConnGuard};
use crate::dispatcher::Dispatcher;
use crate::envelope::{
    IpcError, IpcResponse, MAX_FRAME_BYTES, WireError, WireMessage, WireReply,
    write_response_payload,
};
use crate::limits::QuotaTracker;
use ltmrs_domain::command::{DomainError, DomainErrorCode};

/// Handle a single client connection: read frames, dispatch, write responses.
/// The first frame MUST be a handshake; it authenticates the frontend and
/// issues its retry namespace. Subsequent frames are typed domain requests.
/// The same-user boundary runs in the platform accept path (Unix peer-cred,
/// Windows per-user DACL), so no frame is ever read from a foreign user.
pub async fn handle_connection(
    mut stream: crate::runtime::IpcStream,
    dispatcher: Arc<Dispatcher>,
    quotas: Arc<QuotaTracker>,
) -> Result<(), DaemonError> {
    let mut buf = Vec::with_capacity(4096);

    // Live-connection tracking for restore readiness: counted while this
    // task lives (RAII decrement on every exit path, including panic
    // unwind). Persisted channel bindings outlive their runs and must
    // never stand in for liveness.
    let _live_guard = LiveConnGuard::new(&dispatcher);
    // Released at connection end on every path below: assigned after a
    // successful handshake, dropped when this function returns.
    let mut _client_guard: Option<ClientGuard> = None;
    // Handshake-authenticated frontend; every later frame must carry it.
    // None until the first accepted handshake (a generation-mismatched
    // first attempt keeps the connection open for a same-stream retry).
    let mut authed: Option<ltmrs_domain::id::FrontendId> = None;
    // Handshake-authenticated channel; every later frame must carry it.
    // Frontend-only binding would let one channel's frames reach another
    // channel's session (RQ-05).
    let mut authed_channel: Option<ltmrs_domain::id::ChannelId> = None;

    // ---- Frames: the first must be a handshake. A generation-mismatched
    // handshake keeps the connection open for a same-stream retry (restore
    // bumps the generation mid-session); every other rejection closes it.
    // ---- Request loop ----
    loop {
        let msg = match read_wire_frame(&mut stream, &mut buf).await {
            Ok(m) => m,
            Err(DaemonError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match msg {
            WireMessage::Handshake(req) => {
                // RQ-05: on an authenticated connection only a same-identity
                // epoch refresh is accepted; a different identity stays a
                // protocol violation, never routed anywhere.
                if let (Some(af), Some(ac)) = (authed, authed_channel)
                    && (req.frontend_id != af || req.channel_id != ac)
                {
                    let err = WireError {
                        kind: "unexpected_handshake".into(),
                        message: "handshake already completed".into(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    continue;
                }
                let frontend = req.frontend_id;
                let channel = req.channel_id;
                let dispatcher = Arc::clone(&dispatcher);
                let result = tokio::task::spawn_blocking(move || dispatcher.handle_handshake(&req))
                    .await
                    .unwrap_or_else(|e| Err(IpcError::from(std::io::Error::other(e.to_string()))));
                match result {
                    Ok(hs) => {
                        // Admit the client to the quota table on first success
                        // only (a refresh reuses the held slot); a full table
                        // rejects instead of over-admitting (RQ-22).
                        if _client_guard.is_none() {
                            if let Err(qe) = quotas.register_client(frontend, channel) {
                                let err = WireError {
                                    kind: "client_limit_reached".into(),
                                    message: qe.to_string(),
                                };
                                write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                                return Ok(());
                            }
                            // Free the connection's quota slot at connection end
                            // on every path below. Bound to the
                            // handshake-authenticated (frontend, channel), not
                            // request-claimed ones: each connection holds
                            // exactly one slot.
                            _client_guard =
                                Some(ClientGuard::new(Arc::clone(&quotas), frontend, channel));
                        }
                        authed = Some(frontend);
                        authed_channel = Some(channel);
                        let reply = WireReply::Handshake(hs);
                        write_reply(&mut stream, &reply, &quotas).await?;
                    }
                    Err(e) => {
                        // Rejected handshake: send a wire error. A generation
                        // mismatch keeps the connection open for a same-stream
                        // retry (the frontend adopts the live generation);
                        // every other rejection closes it, as before.
                        let keep_open = matches!(&e, IpcError::GenerationMismatch { .. });
                        let kind = match &e {
                            IpcError::GenerationMismatch { .. } => "generation_mismatch",
                            IpcError::Busy(_) => "daemon_busy",
                            IpcError::StaleNamespace(_) => "stale_namespace",
                            _ => "handshake_rejected",
                        };
                        let err = WireError {
                            kind: kind.into(),
                            message: e.to_string(),
                        };
                        write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                        if !keep_open {
                            return Ok(());
                        }
                        // De-authenticate so a failed epoch refresh retries
                        // from scratch (slot freed, re-registered on success).
                        _client_guard = None;
                        authed = None;
                        authed_channel = None;
                    }
                }
                continue;
            }
            WireMessage::Request(env) => {
                if authed.is_none() {
                    // A request before a handshake is a protocol violation.
                    let err = WireError {
                        kind: "handshake_required".into(),
                        message: "first frame must be a handshake".into(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    return Ok(());
                }
                let envelope = env;
                // Bind every frame to the handshake identity (RQ-05): a frame
                // claiming another frontend OR another channel is a protocol
                // violation, never routed into its session namespace or quota
                // bucket. Channel is bound too: same-frontend frames must not
                // reach a sibling channel's session.
                if Some(envelope.frontend_id) != authed {
                    let err = WireError {
                        kind: "frontend_mismatch".into(),
                        message: "frame frontend differs from handshake identity".into(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    continue;
                }
                if Some(envelope.channel_id) != authed_channel {
                    let err = WireError {
                        kind: "channel_mismatch".into(),
                        message: "frame channel differs from handshake identity".into(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    continue;
                }
                // (Client slot was bound to the handshake ID by the guard above.)

                // Enforce the per-client quota: visible backpressure, not unbounded work.
                if let Err(qe) = quotas.try_enqueue(envelope.frontend_id) {
                    let busy = DomainError::new(
                        DomainErrorCode::Validation,
                        format!("backpressure: {qe}"),
                    );
                    let resp = IpcResponse::error(envelope.operation_id, &busy);
                    write_reply(&mut stream, &WireReply::Response(resp), &quotas).await?;
                    continue;
                }

                // In-flight storage bound: refuse visibly instead of piling up
                // blocking work beyond the configured concurrency.
                if let Err(qe) = quotas.try_start_storage() {
                    quotas.dequeue(envelope.frontend_id);
                    let busy = DomainError::new(
                        DomainErrorCode::Validation,
                        format!("backpressure: {qe}"),
                    );
                    let resp = IpcResponse::error(envelope.operation_id, &busy);
                    write_reply(&mut stream, &WireReply::Response(resp), &quotas).await?;
                    continue;
                }

                // Dispatch on a blocking thread: Fjall write transactions + fsync
                // must not block a Tokio core I/O worker (§7.3).
                let op_id = envelope.operation_id;
                let fe_id = envelope.frontend_id;
                let dispatcher = Arc::clone(&dispatcher);
                let dispatch_result =
                    tokio::task::spawn_blocking(move || dispatcher.handle(&envelope))
                        .await
                        .unwrap_or_else(|e| {
                            Err(DomainError::new(
                                DomainErrorCode::Validation,
                                format!("dispatch task failed: {e}"),
                            ))
                        });
                let response = match dispatch_result {
                    Ok(r) => r,
                    Err(e) => IpcResponse::error(op_id, &e),
                };
                quotas.finish_storage();
                quotas.dequeue(fe_id);

                // Write the response.
                write_reply(&mut stream, &WireReply::Response(response), &quotas).await?;
            }
        }
    }

    /// Read one tagged wire frame (a `WireMessage`) from the stream.
    async fn read_wire_frame(
        stream: &mut crate::runtime::IpcStream,
        buf: &mut Vec<u8>,
    ) -> Result<WireMessage, DaemonError> {
        buf.clear();
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(DaemonError::Ipc(IpcError::FrameTooLarge(len)));
        }
        buf.resize(len, 0);
        stream.read_exact(buf).await?;
        let msg: WireMessage = serde_json::from_slice(buf).map_err(|e| DaemonError::Domain {
            code: DomainErrorCode::Validation,
            message: format!("invalid wire frame: {e}"),
        })?;
        Ok(msg)
    }

    /// Write a tagged wire reply as a stream: an 8-byte total-length header
    /// (u64 BE) followed by length-prefixed chunk frames, each bounded by
    /// `MAX_FRAME_BYTES`. Uniform for small and large replies — large payloads
    /// stream across multiple frames instead of being rejected or truncated.
    async fn write_reply(
        stream: &mut crate::runtime::IpcStream,
        reply: &WireReply,
        quotas: &QuotaTracker,
    ) -> Result<(), DaemonError> {
        let payload = serde_json::to_vec(reply)?;
        // Response byte budget (RQ-22): oversized data responses are refused
        // explicitly, never silently truncated. Control replies (handshake,
        // errors) always pass — they are small by construction and required
        // for the protocol to report failures at all.
        if matches!(reply, WireReply::Response(_)) && !quotas.allows_response(payload.len()) {
            let too_big = DomainError::new(
                DomainErrorCode::Validation,
                format!("response exceeds budget ({} bytes)", payload.len()),
            );
            let op_id = match reply {
                WireReply::Response(resp) => resp.operation_id,
                _ => unreachable!("checked above"),
            };
            let fallback = WireReply::Response(IpcResponse::error(op_id, &too_big));
            let payload = serde_json::to_vec(&fallback)?;
            write_response_payload(stream, &payload).await?;
            return Ok(());
        }
        write_response_payload(stream, &payload).await?;
        Ok(())
    }
}

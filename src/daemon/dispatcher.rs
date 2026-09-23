//! Daemon-side domain dispatcher (design §7.1).
//!
//! Routes typed IPC requests to the canonical repository (mutations + reads)
//! and the frontend registry (session operations). Every request is scoped by
//! its frontend/channel identity; session operations never cross channels.

use std::sync::{Arc, Mutex};

use crate::daemon::envelope::{
    DomainPayload, DomainRequest, HandshakeRequest, HandshakeResponse, IpcEnvelope, IpcError,
    IpcResponse, PROTOCOL_VERSION, validate_handshake,
};
use crate::daemon::registry::FrontendRegistry;
use crate::domain::clock::Clock;
use crate::domain::command::{DomainError, DomainErrorCode, DomainResult, ReceiptOutcome};
use crate::domain::id::EntityId;
use crate::domain::memory::Instant;
use crate::domain::session::Attempt;
use crate::search::backend::SearchBackend;
use crate::service::repository::CanonicalRepository;
use uuid::Uuid;

/// The daemon dispatcher: shares the repository and registry across
/// connections. The repository is `&self`-safe (Fjall transactions); the
/// registry is guarded by a mutex for session operations.
pub struct Dispatcher {
    repo: Arc<CanonicalRepository>,
    registry: Mutex<FrontendRegistry>,
    clock: Arc<dyn Clock + Send + Sync>,
    /// The search backend for semantic retrieval (WP-08). None disables dense
    /// search (lexical/FTS still works if the table is present).
    search: Option<Arc<SearchBackend>>,
}

impl Dispatcher {
    pub fn new(
        repo: Arc<CanonicalRepository>,
        registry: FrontendRegistry,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Self {
        Self {
            repo,
            registry: Mutex::new(registry),
            clock,
            search: None,
        }
    }

    /// Attach a search backend (WP-08 semantic retrieval).
    pub fn with_search(mut self, search: Arc<SearchBackend>) -> Self {
        self.search = Some(search);
        self
    }

    /// Handle the connect-time handshake: validate protocol + generation,
    /// issue/refresh the frontend's retry namespace, and return the epoch.
    /// This is where the daemon authenticates a connection's claims.
    pub fn handle_handshake(&self, req: &HandshakeRequest) -> Result<HandshakeResponse, IpcError> {
        let daemon_gen = self
            .repo
            .store_generation()
            .map_err(|e| IpcError::from(std::io::Error::other(e.message)))?;

        // Validate protocol version and store generation.
        validate_handshake(req, daemon_gen)?;

        // Issue (or refresh) the retry namespace for this frontend. This is
        // what makes subsequent mutations valid — without it, apply() rejects
        // every command as StaleReplay.
        let ns = self
            .repo
            .issue_namespace(req.frontend_id, self.clock.now_millis())
            .map_err(|e| IpcError::from(std::io::Error::other(e.message)))?;

        Ok(HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            store_generation: daemon_gen,
            retry_epoch: ns.retry_epoch,
        })
    }

    /// The store generation this daemon serves (for health/handshake).
    pub fn store_generation(&self) -> crate::domain::id::StoreGeneration {
        self.repo
            .store_generation()
            .unwrap_or(crate::domain::id::StoreGeneration::FIRST)
    }

    /// Handle one IPC envelope, returning the typed response.
    pub fn handle(&self, envelope: &IpcEnvelope) -> DomainResult<IpcResponse> {
        // Validate the protocol version.
        envelope
            .check_protocol()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        // Resolve the channel's active session (never a daemon-global session).
        let session = {
            let reg = self.registry.lock().unwrap();
            reg.resolve_session(envelope.frontend_id, envelope.channel_id)
        };

        match &envelope.body {
            DomainRequest::ToolCall { tool } => {
                let result = crate::daemon::tools::execute_tool(self, envelope, tool)?;
                Ok(IpcResponse::success(
                    envelope.operation_id,
                    ReceiptOutcome::Success { affected: vec![] },
                    result,
                ))
            }
            DomainRequest::GetMemories { ids } => {
                let memories = self.repo.get_memories(ids)?;
                Ok(IpcResponse::success(
                    envelope.operation_id,
                    ReceiptOutcome::Success { affected: vec![] },
                    DomainPayload::Memories(memories),
                ))
            }
            DomainRequest::ListMemories => {
                let export = self.repo.export_snapshot()?;
                Ok(IpcResponse::success(
                    envelope.operation_id,
                    ReceiptOutcome::Success { affected: vec![] },
                    DomainPayload::Memories(export.memories),
                ))
            }
            DomainRequest::Neighbors { id } => {
                let rels = self.repo.neighbors(*id)?;
                Ok(IpcResponse::success(
                    envelope.operation_id,
                    ReceiptOutcome::Success { affected: vec![] },
                    DomainPayload::Relations(rels),
                ))
            }
            DomainRequest::SessionAttempt {
                approach,
                outcome,
                critique,
                rationale,
                related_memory_id,
            } => {
                let session = session.ok_or_else(|| {
                    DomainError::new(DomainErrorCode::Validation, "no active session for channel")
                })?;
                let attempt = Attempt {
                    id: EntityId::new(Uuid::new_v5(
                        &Uuid::NAMESPACE_URL,
                        format!("ltmrs:attempt:{}", envelope.operation_id.as_uuid()).as_bytes(),
                    )),
                    session_id: session,
                    approach: approach.clone(),
                    outcome: *outcome,
                    critique: critique.clone(),
                    rationale: rationale.clone(),
                    related_memory_id: *related_memory_id,
                    created_at: Instant::new(self.clock.now_millis()),
                };
                self.registry.lock().unwrap().record_attempt(
                    envelope.frontend_id,
                    envelope.channel_id,
                    attempt,
                );
                Ok(IpcResponse::success(
                    envelope.operation_id,
                    ReceiptOutcome::Success { affected: vec![] },
                    DomainPayload::None,
                ))
            }
            DomainRequest::SessionEnd {
                outcome,
                final_approach,
                lessons,
            } => {
                // End only THIS channel's session.
                self.registry.lock().unwrap().end_session(
                    envelope.frontend_id,
                    envelope.channel_id,
                    *outcome,
                    final_approach.clone(),
                    lessons.clone(),
                    self.clock.now_millis(),
                );
                Ok(IpcResponse::success(
                    envelope.operation_id,
                    ReceiptOutcome::Success { affected: vec![] },
                    DomainPayload::None,
                ))
            }
            // Mutations: route through the canonical command gateway.
            _ => {
                let cmd = envelope.body.to_domain_command(session).ok_or_else(|| {
                    DomainError::new(
                        DomainErrorCode::Validation,
                        "request requires a session binding",
                    )
                })?;
                let ctx = envelope.to_command_context(envelope.request_digest()?);
                let receipt = self.repo.apply(&ctx, &cmd)?;
                Ok(IpcResponse::success(
                    envelope.operation_id,
                    receipt.outcome,
                    DomainPayload::None,
                ))
            }
        }
    }

    /// Access the repository (for namespace issuance, GC, health).
    pub fn repo(&self) -> &CanonicalRepository {
        self.repo.as_ref()
    }

    /// Access the search backend (WP-08 semantic retrieval), if attached.
    pub fn search(&self) -> Option<&SearchBackend> {
        self.search.as_deref()
    }

    /// Access the clock (for tool execution timestamps).
    pub fn clock(&self) -> &Arc<dyn Clock + Send + Sync> {
        &self.clock
    }

    /// Lock the registry (for lease management, health, session ops).
    pub fn registry(&self) -> std::sync::MutexGuard<'_, FrontendRegistry> {
        self.registry.lock().unwrap()
    }

    /// The protocol version this dispatcher speaks.
    pub fn protocol_version() -> u32 {
        PROTOCOL_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::envelope::{DomainRequest, IpcEnvelope};
    use crate::domain::clock::FrozenClock;
    use crate::domain::command::Scope;
    use crate::domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
    use crate::domain::session::TaskOutcome;
    use crate::service::repository::CanonicalRepository;

    fn fe(n: u64) -> FrontendId {
        FrontendId::new(Uuid::from_u128(n as u128))
    }
    fn ch(n: u64) -> ChannelId {
        ChannelId::new(Uuid::from_u128(n as u128))
    }

    fn test_dispatcher() -> (Dispatcher, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        // Issue a namespace so apply() validates.
        repo.issue_namespace(fe(1), 1000).unwrap();
        let registry = FrontendRegistry::new();
        (Dispatcher::new(repo, registry, clock), dir)
    }

    fn envelope(fe: FrontendId, ch: ChannelId, op: u64, body: DomainRequest) -> IpcEnvelope {
        IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe,
            channel_id: ch,
            operation_id: OperationId::new(Uuid::from_u128(op as u128)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body,
        }
    }

    #[test]
    fn session_end_is_channel_scoped() {
        let disp = test_dispatcher().0;
        // Two channels start sessions.
        let h_a = disp
            .registry()
            .start_session(fe(1), ch(1), None, None, 1000);
        let h_b = disp
            .registry()
            .start_session(fe(1), ch(2), None, None, 1000);
        assert_ne!(h_a, h_b);

        // End channel 1's session via the dispatcher.
        let env = envelope(
            fe(1),
            ch(1),
            1,
            DomainRequest::SessionEnd {
                outcome: TaskOutcome::Success,
                final_approach: None,
                lessons: vec![],
            },
        );
        disp.handle(&env).unwrap();

        // Channel 1's session ended; channel 2's is still active.
        assert_eq!(disp.registry().resolve_session(fe(1), ch(1)), None);
        assert_eq!(disp.registry().resolve_session(fe(1), ch(2)), Some(h_b));
    }

    #[test]
    fn read_request_returns_memories() {
        let disp = test_dispatcher().0;
        let env = envelope(fe(1), ch(1), 1, DomainRequest::GetMemories { ids: vec![] });
        let resp = disp.handle(&env).unwrap();
        match resp.result {
            crate::daemon::envelope::IpcResult::Success { payload, .. } => {
                assert!(matches!(payload, DomainPayload::Memories(_)));
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn wrong_protocol_version_rejected() {
        let disp = test_dispatcher().0;
        let mut env = envelope(fe(1), ch(1), 1, DomainRequest::GetMemories { ids: vec![] });
        env.protocol_version = 99;
        let result = disp.handle(&env);
        assert!(result.is_err());
    }
}

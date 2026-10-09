//! Daemon-side domain dispatcher (design §7.1).
//!
//! Routes typed IPC requests to the canonical repository (mutations + reads)
//! and the frontend registry (session operations). Every request is scoped by
//! its frontend/channel identity; session operations never cross channels.

use std::sync::{Arc, Mutex};

use crate::envelope::{
    DomainPayload, DomainRequest, HandshakeRequest, HandshakeResponse, IpcEnvelope, IpcError,
    IpcResponse, PROTOCOL_VERSION, validate_handshake,
};
use crate::registry::FrontendRegistry;
use ltmrs_domain::clock::Clock;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult, ReceiptOutcome};
use ltmrs_domain::id::{ChannelId, FrontendId, SessionHandle};
use ltmrs_search::search::backend::SearchBackend;
use ltmrs_service::repository::CanonicalRepository;

/// Map a namespace-issue failure to its handshake presentation: transient
/// contention is a typed Busy (retryable, never auth-flavored); fatal
/// errors (corrupt counter, storage) stay generic rejections.
fn map_handshake_issue_error(e: DomainError) -> IpcError {
    if e.code == DomainErrorCode::Contention {
        IpcError::Busy(e.message)
    } else {
        IpcError::from(std::io::Error::other(e.message))
    }
}

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
    /// Restore preview registry (WP-11): single-use TTL tokens bound to
    /// backup digest + live store generation. Daemon-lifetime state.
    restore: Mutex<ltmrs_interchange::restore::RestoreCoordinator>,
    /// Sessions snapshot path for eager durability (P1): every acknowledged
    /// session mutation persists before success returns, so kill-after-ack
    /// loses nothing. None disables persistence (tests without a path).
    sessions_path: Mutex<Option<std::path::PathBuf>>,
    /// Mutation-time similarity gate: serializes duplicate-check + commit
    /// across concurrent memory_add/update/merge so two racing neardup
    /// writes cannot both miss each other. Held for milliseconds (one
    /// Lance query + one commit); different scopes still share it because
    /// unprojected writes are globally visible to every scope's scan.
    similarity_gate: std::sync::Mutex<()>,
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
            restore: Mutex::new(ltmrs_interchange::restore::RestoreCoordinator::default()),
            sessions_path: Mutex::new(None),
            similarity_gate: std::sync::Mutex::new(()),
        }
    }

    /// Attach a search backend (WP-08 semantic retrieval).
    pub fn with_search(mut self, search: Arc<SearchBackend>) -> Self {
        self.search = Some(search);
        self
    }

    /// Set the sessions snapshot path for eager durability (P1). Called by
    /// `Daemon::start` when sessions are enabled.
    pub fn set_sessions_path(&self, path: Option<std::path::PathBuf>) {
        *self.sessions_path.lock().unwrap() = path;
    }

    /// Persist registry sessions now, before an acknowledgement returns (P1
    /// durable sessions). Returns Err on failure: callers must fail the
    /// acknowledgement, never report success for unpersisted state
    /// (re-review R1). Loud either way (previous file intact via
    /// tmp+rename); the loss on error is bounded to a failed disk write.
    pub fn persist_sessions(&self) -> Result<(), String> {
        let path = self.sessions_path.lock().unwrap().clone();
        if let Some(p) = path
            && let Err(e) = self.registry.lock().unwrap().persist(&p)
        {
            let msg = format!("failed to persist session history: {e:?}");
            eprintln!("ltmrs: {msg}");
            return Err(msg);
        }
        Ok(())
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

        // Unknown-outcome recovery (P1-B): a reconnected frontend may resume
        // its retry namespace instead of minting a new epoch, so its
        // pre-failure operation IDs keep resolving to their receipts.
        // Refusals (expired/unknown, e.g. drained by a restore) are typed
        // so the frontend surfaces an unknown outcome rather than silently
        // minting a fresh epoch for an uncertain mutation.
        if let Some(epoch) = req.resume_retry_epoch {
            match self.repo.resume_namespace(
                req.frontend_id,
                req.channel_id,
                epoch,
                self.clock.now_millis(),
            ) {
                Ok(ns) => {
                    return Ok(HandshakeResponse {
                        protocol_version: PROTOCOL_VERSION,
                        store_generation: daemon_gen,
                        retry_epoch: ns.retry_epoch,
                    });
                }
                Err(e) if e.code == DomainErrorCode::StaleReplay => {
                    return Err(IpcError::StaleNamespace(e.message));
                }
                Err(e) => {
                    return Err(map_handshake_issue_error(e));
                }
            }
        }

        // Issue (or refresh) the retry namespace for this frontend. This is
        // what makes subsequent mutations valid — without it, apply() rejects
        // every command as StaleReplay. Transient write contention is
        // retried here (issue_namespace already retries internally; this
        // covers a conflict landing between its budget and our read): a
        // retryable conflict must never present as a handshake refusal.
        // Fatal errors (corrupt counter, storage) return immediately.
        let mut attempt = 0;
        let ns = loop {
            match self.repo.issue_namespace(
                req.frontend_id,
                req.channel_id,
                self.clock.now_millis(),
            ) {
                Ok(ns) => break ns,
                Err(e) if e.code == DomainErrorCode::Contention && attempt < 2 => {
                    attempt += 1;
                    continue;
                }
                Err(e) => {
                    return Err(map_handshake_issue_error(e));
                }
            }
        };

        Ok(HandshakeResponse {
            protocol_version: PROTOCOL_VERSION,
            store_generation: daemon_gen,
            retry_epoch: ns.retry_epoch,
        })
    }

    /// The store generation this daemon serves (for health/handshake).
    /// Fail-closed: an unreadable generation is a server fault, never a
    /// default — callers refuse rather than admit under generation 1.
    pub fn store_generation(&self) -> DomainResult<ltmrs_domain::id::StoreGeneration> {
        self.repo.store_generation()
    }

    /// Handle one IPC envelope, returning the typed response.
    pub fn handle(&self, envelope: &IpcEnvelope) -> DomainResult<IpcResponse> {
        // Validate the protocol version.
        envelope
            .check_protocol()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        // Bounded wait: a request arriving past its deadline is refused
        // instead of executed late (design §7.2 cancellation semantics).
        if let Some(deadline) = envelope.deadline_millis
            && self.clock.now_millis() >= deadline
        {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "deadline exceeded before dispatch",
            ));
        }

        // Generation gate: a restore replaces the store under a new
        // generation. Envelopes naming a retired generation are refused so
        // stale writers re-handshake instead of polluting the new store
        // (or minting receipts under a dead generation). An unreadable live
        // generation fails closed as a server fault, not a default.
        let live = self.store_generation().map_err(|e| {
            DomainError::new(
                DomainErrorCode::Validation,
                format!("cannot read live store generation: {}", e.message),
            )
        })?;
        if envelope.store_generation != live {
            return Err(DomainError::new(
                DomainErrorCode::StaleGeneration,
                format!(
                    "stale store generation {} (live {}): re-handshake and retry",
                    envelope.store_generation.as_u64(),
                    live.as_u64()
                ),
            ));
        }

        // Resolve the channel's active session (registry binding +
        // canonical liveness; never a daemon-global session).
        let session = self.resolve_session(envelope.frontend_id, envelope.channel_id);

        // Retry-namespace gate (RQ-06) for the direct session bodies:
        // a mutating request executes only under a live namespace for
        // its own channel. ToolCall bodies admit inside execute_tool
        // (single admission point); read-only requests are unaffected.
        if matches!(
            &envelope.body,
            DomainRequest::SessionAttempt { .. } | DomainRequest::SessionEnd { .. }
        ) {
            let scope = envelope.operation_scope(envelope.request_digest()?);
            self.repo.validate_scope(&scope)?;
        }

        match &envelope.body {
            DomainRequest::ToolCall { tool } => {
                let result = crate::tools::execute_tool(self, envelope, tool)?;
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
                let digest = envelope.request_digest()?;
                let scope = envelope.operation_scope(digest);
                let session = session.ok_or_else(|| {
                    DomainError::new(DomainErrorCode::Validation, "no active session for channel")
                })?;
                // ONE canonical operation (re-review P1-3): record, counters
                // and receipt commit together in the store; replay resolves,
                // digest mismatch rejects, durability failures fail loudly
                // via the barrier.
                match self.repo.session_attempt_tx(
                    &scope,
                    session,
                    approach.clone(),
                    *outcome,
                    critique.clone(),
                    rationale.clone(),
                    *related_memory_id,
                    self.clock.now_millis(),
                ) {
                    Ok(ltmrs_domain::session::SessionOp::Conflict) => Err(DomainError::new(
                        DomainErrorCode::KeyReuseDifferentInput,
                        "operation key reused with different input",
                    )),
                    Ok(_) => Ok(IpcResponse::success(
                        envelope.operation_id,
                        ReceiptOutcome::Success { affected: vec![] },
                        DomainPayload::None,
                    )),
                    Err(e) => Err(e),
                }
            }
            DomainRequest::SessionEnd {
                outcome,
                final_approach,
                lessons,
            } => {
                let digest = envelope.request_digest()?;
                let scope = envelope.operation_scope(digest);
                // End only THIS channel's session, atomically with its
                // guide effects and receipt (re-review P1-3). A replay
                // resolves; a digest mismatch rejects. Ending an
                // already-terminal session is a recorded no-op, never an
                // error, so retried ends always resolve.
                let handle = match session {
                    Some(h) => h,
                    None => {
                        // No live binding: resolve the bound handle (even
                        // terminal) so the operation still records against
                        // the channel's session instead of executing
                        // nowhere. Without any binding there is nothing
                        // to end.
                        match self
                            .registry
                            .lock()
                            .unwrap()
                            .channel_session(envelope.frontend_id, envelope.channel_id)
                        {
                            Some(h) => h,
                            None => {
                                return Err(DomainError::new(
                                    DomainErrorCode::Validation,
                                    "no active session for channel",
                                ));
                            }
                        }
                    }
                };
                match self.repo.session_end_tx(
                    &scope,
                    handle,
                    *outcome,
                    final_approach.clone(),
                    lessons.clone(),
                    self.clock.now_millis(),
                ) {
                    Ok(ltmrs_domain::session::SessionOp::Conflict) => Err(DomainError::new(
                        DomainErrorCode::KeyReuseDifferentInput,
                        "operation key reused with different input",
                    )),
                    // Fresh end of an already-terminal session: nothing to
                    // do (matches the tools path's "no active session").
                    Ok(ltmrs_domain::session::SessionOp::Applied((_, _, false))) => {
                        Err(DomainError::new(
                            DomainErrorCode::Validation,
                            "no active session for channel",
                        ))
                    }
                    Ok(_) => Ok(IpcResponse::success(
                        envelope.operation_id,
                        ReceiptOutcome::Success { affected: vec![] },
                        DomainPayload::None,
                    )),
                    Err(e) => Err(e),
                }
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

    /// Shared ownership of the repository (for maintenance wiring).
    pub fn repo_arc(&self) -> Arc<CanonicalRepository> {
        Arc::clone(&self.repo)
    }

    /// Access the search backend (WP-08 semantic retrieval), if attached.
    pub fn search(&self) -> Option<&SearchBackend> {
        self.search.as_deref()
    }

    /// Hold across a mutation preflight (similarity check) plus its commit:
    /// concurrent memory_add/update/merge serialize here so racing
    /// near-duplicates cannot both miss. Leaf lock, milliseconds held.
    pub fn similarity_gate(&self) -> &std::sync::Mutex<()> {
        &self.similarity_gate
    }

    /// Access the restore preview registry (lock briefly; never hold across IO).
    pub fn restore_coordinator(
        &self,
    ) -> std::sync::MutexGuard<'_, ltmrs_interchange::restore::RestoreCoordinator> {
        self.restore.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Access the clock (for tool execution timestamps).
    pub fn clock(&self) -> &Arc<dyn Clock + Send + Sync> {
        &self.clock
    }

    /// Lock the registry (for lease management, health, session ops).
    pub fn registry(&self) -> std::sync::MutexGuard<'_, FrontendRegistry> {
        self.registry.lock().unwrap()
    }

    /// Resolve the channel's ACTIVE traced session: the registry binding
    /// plus canonical liveness (None if unbound or terminal). Routing
    /// comes from the registry; truth comes from the store.
    pub fn resolve_session(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
    ) -> Option<SessionHandle> {
        let handle = self
            .registry
            .lock()
            .unwrap()
            .channel_session(frontend_id, channel_id)?;
        match self.repo.get_session(handle) {
            Ok(Some(s)) if !s.status.is_terminal() => Some(handle),
            _ => None,
        }
    }

    /// The protocol version this dispatcher speaks.
    pub fn protocol_version() -> u32 {
        PROTOCOL_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{DomainRequest, IpcEnvelope};
    use ltmrs_domain::clock::FrozenClock;
    use ltmrs_domain::command::{OperationScope, Scope};
    use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, SessionHandle, StoreGeneration};
    use ltmrs_domain::session::{SessionOp, TaskOutcome};
    use ltmrs_service::repository::CanonicalRepository;
    use uuid::Uuid;

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
        repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
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
    /// Handshake error presentation: transient contention maps to typed
    /// Busy (retryable); fatal errors stay generic rejections.
    #[test]
    fn handshake_issue_error_mapping_is_typed() {
        let busy = map_handshake_issue_error(DomainError::new(
            DomainErrorCode::Contention,
            "namespace issue conflicted",
        ));
        assert!(
            matches!(busy, IpcError::Busy(_)),
            "contention must present as Busy, got {busy:?}"
        );
        let fatal = map_handshake_issue_error(DomainError::new(
            DomainErrorCode::Validation,
            "corrupt namespace epoch counter",
        ));
        assert!(
            !matches!(fatal, IpcError::Busy(_)),
            "fatal errors must not present as Busy, got {fatal:?}"
        );
    }

    /// Start a traced session in the canonical store and bind the channel
    /// to it (the two halves of session creation after the migration).
    fn start_bound(
        disp: &Dispatcher,
        fe: FrontendId,
        ch: ChannelId,
        seed: u128,
        op: u64,
    ) -> SessionHandle {
        let handle = SessionHandle::new(Uuid::from_u128(seed));
        // Each bound channel gets its own namespace (RQ-06 scoped):
        // issuing per setup keeps the helper channel-agnostic.
        let ns = disp.repo().issue_namespace(fe, ch, 1000).unwrap();
        let scope = OperationScope {
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe,
            channel_id: ch,
            retry_epoch: ns.retry_epoch,
            operation_id: OperationId::new(Uuid::from_u128(op as u128)),
            request_digest: "digest".to_string(),
        };
        match disp
            .repo()
            .session_start_tx(&scope, handle, None, None, vec![], None, None, 1000)
            .unwrap()
        {
            SessionOp::Applied(h) | SessionOp::Replayed(h) => {
                disp.registry().bind_session(fe, ch, h, false);
                h
            }
            SessionOp::Conflict => panic!("test setup conflict"),
        }
    }

    #[test]
    fn session_end_is_channel_scoped() {
        let disp = test_dispatcher().0;
        // Two channels start sessions.
        let h_a = start_bound(&disp, fe(1), ch(1), 101, 1);
        let h_b = start_bound(&disp, fe(1), ch(2), 102, 2);
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
        assert_eq!(disp.resolve_session(fe(1), ch(1)), None);
        assert_eq!(disp.resolve_session(fe(1), ch(2)), Some(h_b));
    }

    #[test]
    fn read_request_returns_memories() {
        let disp = test_dispatcher().0;
        let env = envelope(fe(1), ch(1), 1, DomainRequest::GetMemories { ids: vec![] });
        let resp = disp.handle(&env).unwrap();
        match resp.result {
            crate::envelope::IpcResult::Success { payload, .. } => {
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

    /// A request arriving past its deadline is refused instead of executed
    /// late (frozen clock is at 1000 here).
    #[test]
    fn past_deadline_refused_before_dispatch() {
        let disp = test_dispatcher().0;
        let mut env = envelope(fe(1), ch(1), 1, DomainRequest::ListMemories);
        env.deadline_millis = Some(0);
        let err = disp.handle(&env).unwrap_err();
        assert!(
            err.message.contains("deadline"),
            "expired deadline must refuse, got: {}",
            err.message
        );
    }

    /// A future deadline proceeds normally.
    #[test]
    fn future_deadline_proceeds() {
        let disp = test_dispatcher().0;
        let mut env = envelope(fe(1), ch(1), 1, DomainRequest::ListMemories);
        env.deadline_millis = Some(2000);
        assert!(disp.handle(&env).is_ok());
    }

    /// Post-restore, envelopes naming the retired generation are refused
    /// (stale writers must re-handshake, not pollute the new store).
    #[test]
    fn stale_generation_envelopes_are_refused() {
        use ltmrs_domain::id::StoreGeneration;
        let (disp, _dir) = test_dispatcher();
        disp.repo()
            .set_store_generation(StoreGeneration::new(2))
            .unwrap();
        let stale = envelope(fe(1), ch(1), 1, DomainRequest::ListMemories);
        let err = disp.handle(&stale).unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::StaleGeneration
        );
        assert!(
            err.message.contains("re-handshake"),
            "must direct recovery, got: {}",
            err.message
        );
        let mut fresh = envelope(fe(1), ch(1), 2, DomainRequest::ListMemories);
        fresh.store_generation = StoreGeneration::new(2);
        assert!(disp.handle(&fresh).is_ok());
    }

    /// P1 replay safety: repeating the same SessionAttempt operation records
    /// exactly one attempt (deterministic UUIDv5 ID dedups).
    #[test]
    fn session_attempt_replay_records_once() {
        use ltmrs_domain::session::AttemptOutcome;

        let (disp, _dir) = test_dispatcher();
        let handle = start_bound(&disp, fe(1), ch(1), 142, 1);
        let env = envelope(
            fe(1),
            ch(1),
            42,
            DomainRequest::SessionAttempt {
                approach: "try X".into(),
                outcome: AttemptOutcome::Rejected,
                critique: None,
                rationale: None,
                related_memory_id: None,
            },
        );
        disp.handle(&env).unwrap();
        disp.handle(&env).unwrap();
        let session = disp.repo().get_session(handle).unwrap().unwrap();
        assert_eq!(
            session.attempts.len(),
            1,
            "replayed operation must not duplicate the attempt"
        );
    }

    /// P1 durability: an acknowledged session_end survives a store
    /// reopen (the process-death equivalent for storage: Fjall recovery
    /// runs on open). The outcome, lessons and approach come back intact.
    #[test]
    fn session_end_ack_survives_store_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store");
        let store_str = store_path.to_str().unwrap().to_string();
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
        let repo =
            Arc::new(CanonicalRepository::open_with_clock(&store_str, Arc::clone(&clock)).unwrap());
        repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
        let disp = Dispatcher::new(repo, FrontendRegistry::new(), clock);
        let handle = start_bound(&disp, fe(1), ch(1), 107, 1);
        let env = envelope(
            fe(1),
            ch(1),
            7,
            DomainRequest::SessionEnd {
                outcome: TaskOutcome::Success,
                final_approach: Some("fixed".into()),
                lessons: vec!["check logs".into()],
            },
        );
        disp.handle(&env).unwrap();
        // Kill: drop everything without shutdown, then reopen the store
        // from the same path (recovery runs here).
        let live_handle = handle;
        drop(disp);
        let clock2: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(2000));
        let repo2 = CanonicalRepository::open_with_clock(&store_str, clock2).unwrap();
        let session = repo2
            .get_session(live_handle)
            .unwrap()
            .expect("session must survive kill");
        assert_eq!(session.outcome, Some(TaskOutcome::Success));
        assert_eq!(session.lessons, vec!["check logs".to_string()]);
        assert_eq!(
            session.final_approach.as_deref(),
            Some("fixed"),
            "final approach must survive kill"
        );
    }

    /// Re-review R1: a durability-barrier failure fails the acknowledgement
    /// instead of reporting success for unflushed state — and the identical
    /// retry fails too (no fabricated durability from a visible receipt).
    #[test]
    fn session_ack_fails_when_barrier_fails() {
        use ltmrs_domain::session::AttemptOutcome;

        let (disp, _dir) = test_dispatcher();
        start_bound(&disp, fe(1), ch(1), 111, 1);
        let attempt = || {
            envelope(
                fe(1),
                ch(1),
                11,
                DomainRequest::SessionAttempt {
                    approach: "try X".into(),
                    outcome: AttemptOutcome::Rejected,
                    critique: None,
                    rationale: None,
                    related_memory_id: None,
                },
            )
        };
        disp.repo().fault_injector().set_persist_failures(1);
        let err = disp.handle(&attempt()).unwrap_err();
        assert!(
            err.message.contains("persist"),
            "barrier failure must fail loudly, got: {}",
            err.message
        );
        // Retry with the barrier STILL failing: must fail again.
        disp.repo().fault_injector().set_persist_failures(1);
        let err = disp.handle(&attempt()).unwrap_err();
        assert!(
            err.message.contains("persist"),
            "replay without durability must fail loudly, got: {}",
            err.message
        );
        // Barrier healthy: the identical retry now succeeds, recorded once.
        disp.handle(&attempt()).unwrap();
        let sessions = disp.repo().all_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].attempts.len(), 1);
    }

    /// Re-review R5: replaying a session_end with changed arguments rejects
    /// as key reuse instead of re-executing.
    #[test]
    fn session_end_replay_with_changed_args_rejects() {
        let (disp, _dir) = test_dispatcher();
        start_bound(&disp, fe(1), ch(1), 121, 1);
        let end = |op: u64, lessons: Vec<String>| {
            envelope(
                fe(1),
                ch(1),
                op,
                DomainRequest::SessionEnd {
                    outcome: TaskOutcome::Success,
                    final_approach: None,
                    lessons,
                },
            )
        };
        disp.handle(&end(21, vec!["a".into()])).unwrap();
        // Same operation, different arguments: conflict, never re-executed.
        let err = disp.handle(&end(21, vec!["b".into()])).unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput
        );
    }
}

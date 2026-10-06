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
    restore: Mutex<crate::interchange::restore::RestoreCoordinator>,
    /// Sessions snapshot path for eager durability (P1): every acknowledged
    /// session mutation persists before success returns, so kill-after-ack
    /// loses nothing. None disables persistence (tests without a path).
    sessions_path: Mutex<Option<std::path::PathBuf>>,
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
            restore: Mutex::new(crate::interchange::restore::RestoreCoordinator::default()),
            sessions_path: Mutex::new(None),
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

    fn persist_error(e: String) -> DomainError {
        DomainError::new(DomainErrorCode::Validation, e)
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
        // every command as StaleReplay. Transient write contention is
        // retried here (issue_namespace already retries internally; this
        // covers a conflict landing between its budget and our read): a
        // retryable conflict must never present as a handshake refusal.
        // Fatal errors (corrupt counter, storage) return immediately.
        let mut attempt = 0;
        let ns = loop {
            match self
                .repo
                .issue_namespace(req.frontend_id, self.clock.now_millis())
            {
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
        // (or minting receipts under a dead generation).
        let live = self.store_generation();
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
                let op_id = envelope.operation_id.as_uuid().to_string();
                let digest = envelope.request_digest()?;
                // Operation receipt first (re-review R5): a replay returns
                // success without re-recording; a digest mismatch rejects.
                // The gate precedes session resolution so replays after a
                // terminal session still resolve to the recorded outcome.
                match self.registry.lock().unwrap().check_op(&op_id, &digest) {
                    crate::daemon::registry::OpCheck::Replay(_) => {
                        return Ok(IpcResponse::success(
                            envelope.operation_id,
                            ReceiptOutcome::Success { affected: vec![] },
                            DomainPayload::None,
                        ));
                    }
                    crate::daemon::registry::OpCheck::Conflict => {
                        return Err(DomainError::new(
                            DomainErrorCode::KeyReuseDifferentInput,
                            "operation key reused with different input",
                        ));
                    }
                    crate::daemon::registry::OpCheck::Fresh => {}
                }
                let session = session.ok_or_else(|| {
                    DomainError::new(DomainErrorCode::Validation, "no active session for channel")
                })?;
                let attempt = Attempt {
                    id: EntityId::new(Uuid::new_v5(
                        &Uuid::NAMESPACE_URL,
                        format!("ltmrs:attempt:{}", envelope.operation_id.as_uuid()).as_bytes(),
                    )),
                    session_id: session,
                    seq: 0,
                    approach: approach.clone(),
                    outcome: *outcome,
                    critique: critique.clone(),
                    rationale: rationale.clone(),
                    related_memory_id: *related_memory_id,
                    confidence: 1.0,
                    access_count: 0,
                    last_accessed_at: None,
                    created_at: Instant::new(self.clock.now_millis()),
                };
                {
                    let mut reg = self.registry.lock().unwrap();
                    reg.record_attempt(envelope.frontend_id, envelope.channel_id, attempt);
                    reg.record_op(&op_id, &digest, session, None, None);
                }
                // Durable before ack, failing loudly (re-review R1): a kill
                // immediately after success must not lose the attempt, and
                // a failed save must not report success.
                self.persist_sessions().map_err(Self::persist_error)?;
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
                let op_id = envelope.operation_id.as_uuid().to_string();
                let digest = envelope.request_digest()?;
                // Receipt before resolution (re-review R5): replays after a
                // terminal session still resolve to the recorded outcome.
                match self.registry.lock().unwrap().check_op(&op_id, &digest) {
                    crate::daemon::registry::OpCheck::Replay(_) => {
                        return Ok(IpcResponse::success(
                            envelope.operation_id,
                            ReceiptOutcome::Success { affected: vec![] },
                            DomainPayload::None,
                        ));
                    }
                    crate::daemon::registry::OpCheck::Conflict => {
                        return Err(DomainError::new(
                            DomainErrorCode::KeyReuseDifferentInput,
                            "operation key reused with different input",
                        ));
                    }
                    crate::daemon::registry::OpCheck::Fresh => {}
                }
                // End only THIS channel's session.
                let handle = self.registry.lock().unwrap().end_session(
                    envelope.frontend_id,
                    envelope.channel_id,
                    *outcome,
                    final_approach.clone(),
                    lessons.clone(),
                    self.clock.now_millis(),
                );
                if let Some(handle) = handle {
                    self.registry
                        .lock()
                        .unwrap()
                        .record_op(&op_id, &digest, handle, None, None);
                }
                // Durable before ack, failing loudly (re-review R1).
                self.persist_sessions().map_err(Self::persist_error)?;
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

    /// Shared ownership of the repository (for maintenance wiring).
    pub fn repo_arc(&self) -> Arc<CanonicalRepository> {
        Arc::clone(&self.repo)
    }

    /// Access the search backend (WP-08 semantic retrieval), if attached.
    pub fn search(&self) -> Option<&SearchBackend> {
        self.search.as_deref()
    }

    /// Access the restore preview registry (lock briefly; never hold across IO).
    pub fn restore_coordinator(
        &self,
    ) -> std::sync::MutexGuard<'_, crate::interchange::restore::RestoreCoordinator> {
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
        use crate::domain::id::StoreGeneration;
        let (disp, _dir) = test_dispatcher();
        disp.repo()
            .set_store_generation(StoreGeneration::new(2))
            .unwrap();
        let stale = envelope(fe(1), ch(1), 1, DomainRequest::ListMemories);
        let err = disp.handle(&stale).unwrap_err();
        assert_eq!(
            err.code,
            crate::domain::command::DomainErrorCode::StaleGeneration
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
        use crate::domain::session::AttemptOutcome;

        let (disp, _dir) = test_dispatcher();
        let handle = disp
            .registry()
            .start_session(fe(1), ch(1), None, None, 1000);
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
        let session = disp.registry().session(handle).unwrap().clone();
        assert_eq!(
            session.attempts.len(),
            1,
            "replayed operation must not duplicate the attempt"
        );
    }

    /// P1 durability: an acknowledged session_end persists before success
    /// returns, so a kill (drop without shutdown) loses nothing — the
    /// reloaded registry holds the outcome and lessons.
    #[test]
    fn session_end_ack_survives_kill_without_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_file = dir.path().join("sessions.json");
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(
                dir.path().join("store").to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let disp = Dispatcher::new(repo, FrontendRegistry::new(), clock);
        disp.set_sessions_path(Some(sessions_file.clone()));
        let handle = disp
            .registry()
            .start_session(fe(1), ch(1), None, None, 1000);
        disp.persist_sessions().unwrap();
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
        // Kill: drop without shutdown. Eager persist already wrote the file.
        let live_handle = handle;
        drop(disp);
        let restored = FrontendRegistry::load(&sessions_file).unwrap();
        let session = restored
            .session(live_handle)
            .expect("session must survive kill");
        assert_eq!(session.outcome, Some(TaskOutcome::Success));
        assert_eq!(session.lessons, vec!["check logs".to_string()]);
        assert_eq!(
            session.final_approach.as_deref(),
            Some("fixed"),
            "final approach must survive kill"
        );
    }

    /// Re-review R1: a session-file save failure fails the acknowledgement
    /// instead of reporting success for unpersisted state. The sessions path
    /// points inside a nonexistent directory, so every persist fails.
    #[test]
    fn session_ack_fails_when_persist_fails() {
        use crate::domain::session::AttemptOutcome;

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir").join("sessions.json");
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(
                dir.path().join("store").to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let disp = Dispatcher::new(repo, FrontendRegistry::new(), clock);
        disp.set_sessions_path(Some(missing));
        disp.registry()
            .start_session(fe(1), ch(1), None, None, 1000);
        // session_attempt must not report success when the save fails.
        let attempt = envelope(
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
        );
        let err = disp.handle(&attempt).unwrap_err();
        assert!(
            err.message.contains("persist"),
            "save failure must fail loudly, got: {}",
            err.message
        );
        // session_end likewise.
        let end = envelope(
            fe(1),
            ch(1),
            12,
            DomainRequest::SessionEnd {
                outcome: TaskOutcome::Success,
                final_approach: None,
                lessons: vec![],
            },
        );
        let err = disp.handle(&end).unwrap_err();
        assert!(
            err.message.contains("persist"),
            "save failure must fail loudly, got: {}",
            err.message
        );
    }

    /// Re-review R5: replaying a session_end with changed arguments rejects
    /// as key reuse instead of re-executing.
    #[test]
    fn session_end_replay_with_changed_args_rejects() {
        let (disp, _dir) = test_dispatcher();
        disp.registry()
            .start_session(fe(1), ch(1), None, None, 1000);
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
            crate::domain::command::DomainErrorCode::KeyReuseDifferentInput
        );
    }
}

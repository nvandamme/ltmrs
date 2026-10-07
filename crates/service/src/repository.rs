//! Hardened canonical repository over Fjall (AD-01 Option B).
//!
//! A single `apply` entry point centralizes command application, precondition
//! validation and atomic receipt storage. The receipt is committed in the same
//! Fjall transaction as the command it records — never in a later best-effort
//! write. Storage conflicts retry from a fresh snapshot; stale revisions are
//! surfaced, not blindly rebased; unknown commit outcomes are resolved via the
//! receipt and the same operation key.

use fjall::{
    KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace, OptimisticWriteTx,
    PersistMode, Readable,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::migrations::{MigrationOutcome, MigrationPlan, MigrationRunner, MigrationSafetyRules};
use crate::repository_internal::{CommandState, TxAction, apply_command};
use ltmrs_domain::command::{
    CommandContext, CommandReceipt, DomainCommand, DomainError, DomainErrorCode, DomainResult,
    ReceiptOutcome, RetryNamespace,
};
use ltmrs_domain::export::CanonicalExport;
use ltmrs_domain::id::{
    ChannelId, EntityId, FrontendId, ModelFingerprint, OperationId, SessionHandle, StoreGeneration,
};
use ltmrs_domain::memory::Memory;
use ltmrs_domain::projection::{GenerationRecord, GenerationStatus};
use ltmrs_domain::relation::Relation;
use ltmrs_domain::session::{Session, SessionOp, SessionReceipt};

const MAX_RETRIES: u32 = 8;
/// Default retry namespace TTL: 24 hours (initial policy per design §5.2).
pub const DEFAULT_NAMESPACE_TTL_MILLIS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ReceiptRecord {
    store_generation: u64,
    retry_epoch: u64,
    operation_id: String,
    frontend_id: String,
    channel_id: String,
    request_digest: String,
    outcome: OutcomeRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum OutcomeRecord {
    Success { affected: Vec<String> },
    Rejected { code: String },
}

/// Fault injection for crash/durability testing (design RV-18).
///
/// Injects failures at specific points to verify crash recovery,
/// unknown-outcome resolution, and migration safety. Process-crash,
/// storage-fault and power-loss qualifications remain distinct.
#[derive(Debug, Default)]
pub struct FaultInjector {
    /// Number of times to force the unknown-outcome path on commit.
    /// When > 0, the next N commits are treated as having an unknown
    /// outcome (the write may or may not have succeeded).
    commit_unknown_outcomes: std::sync::atomic::AtomicU32,
    /// Number of times to fail a migration step before succeeding.
    migration_failures: std::sync::atomic::AtomicU32,
    /// Number of times to fail the durability barrier before succeeding.
    /// When > 0, the next N barrier calls return an error instead of
    /// persisting: callers must fail the ACK, never report success on
    /// buffered-only data.
    persist_failures: std::sync::atomic::AtomicU32,
}

impl FaultInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure N upcoming commits to take the unknown-outcome path.
    pub fn set_commit_unknown_outcomes(&self, n: u32) {
        self.commit_unknown_outcomes
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Configure N upcoming migration steps to fail.
    pub fn set_migration_failures(&self, n: u32) {
        self.migration_failures
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Configure N upcoming durability barriers to fail.
    pub fn set_persist_failures(&self, n: u32) {
        self.persist_failures
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Consume one unknown-outcome fault. Returns true if a fault was injected.
    pub fn inject_unknown_outcome(&self) -> bool {
        Self::consume(&self.commit_unknown_outcomes)
    }

    /// Consume one migration fault. Returns true if a fault was injected.
    pub fn inject_migration_fault(&self) -> bool {
        Self::consume(&self.migration_failures)
    }

    /// Consume one durability-barrier fault. Returns true if injected.
    pub fn inject_persist_failure(&self) -> bool {
        Self::consume(&self.persist_failures)
    }

    /// Atomically consume one fault from a counter without underflow.
    fn consume(counter: &std::sync::atomic::AtomicU32) -> bool {
        use std::sync::atomic::Ordering;
        let mut current = counter.load(Ordering::SeqCst);
        while current > 0 {
            match counter.compare_exchange_weak(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(prev) => current = prev,
            }
        }
        false
    }
}

pub struct CanonicalRepository {
    db: OptimisticTxDatabase,
    memories: OptimisticTxKeyspace,
    relations: OptimisticTxKeyspace,
    receipts: OptimisticTxKeyspace,
    aliases: OptimisticTxKeyspace,
    namespaces: OptimisticTxKeyspace,
    projections: OptimisticTxKeyspace,
    generations: OptimisticTxKeyspace,
    feedback_events: OptimisticTxKeyspace,
    guides: OptimisticTxKeyspace,
    suggestions: OptimisticTxKeyspace,
    /// Idempotency log for guide practice (P1 replay safety): operation ID
    /// → guide name. Written atomically with the guide mutation so a retried
    /// operation replays without double-counting usage/success counters.
    guide_ops: OptimisticTxKeyspace,
    /// Canonical session state (P1-3 re-review): traced sessions keyed by
    /// handle. The daemon registry keeps only channel bindings, leases and
    /// ephemeral virtual sessions; every durable session mutation commits
    /// here atomically with its receipt.
    sessions: OptimisticTxKeyspace,
    /// Canonical session operation receipts (op ID → SessionReceipt),
    /// committed in the same transaction as the mutation they record.
    session_ops: OptimisticTxKeyspace,
    /// Suggestion response receipts (P1-2, op ID → SuggestionOpLog),
    /// committed atomically with the status transition + attempt
    /// adjustments so a retried respond replays instead of adjusting twice.
    suggestion_ops: OptimisticTxKeyspace,
    /// Post-commit wake-up hook (P2-1 projection latency): fired once per
    /// committed mutation so the projection worker drives the new job
    /// immediately instead of waiting out its maintenance interval. Replays
    /// fire nothing (no new work). Never fails the commit when unset.
    commit_hook: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    fault_injector: std::sync::Arc<FaultInjector>,
    clock: std::sync::Arc<dyn ltmrs_domain::clock::Clock + Send + Sync>,
    /// Restore barrier: shared by every mutating entry point, exclusive to
    /// the restore path. A restore drains and rewrites keyspaces no
    /// concurrent writer may interleave with — optimistic SSI alone cannot
    /// see brand-new keys, so mutual exclusion (not conflict detection)
    /// closes the drain-then-write race. RULE: fence outermost public
    /// entries only; internals and the restore path itself never re-fence
    /// (a write guard is not re-entrant with a waiting writer).
    restore_lock: std::sync::RwLock<()>,
}

/// Digest-bound practice receipt stored in the `guide_ops` log: the recorded
/// guide snapshot is the replay outcome (never current contents, never a
/// silent re-application). Legacy bare-name entries predate digests and keep
/// their old semantics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PracticeLog {
    name: String,
    digest: String,
    recorded: ltmrs_domain::guide::Guide,
}

/// Operation kinds sharing the `guide_ops` idempotency log (P1-2): every
/// mutating guide tool records its outcome atomically with the mutation, so
/// a transport retry (same operation ID + digest) replays the recorded
/// outcome instead of re-executing, and the same ID with a different digest
/// rejects as key reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GuideOpKind {
    Create,
    /// `guide_create` applied as an update to an existing/similar guide
    /// (distinct response shape from a plain update).
    CreateUpdate,
    Update,
    Forget,
    Merge,
}

/// Digest-bound receipt for one guide tool operation (P1-2). The recorded
/// outcome rebuilds the exact tool response on replay: responses are pure
/// functions of the recorded snapshot (plus merge sources), so nothing is
/// ever re-applied and nothing is recomputed from live state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GuideOpLog {
    digest: String,
    kind: GuideOpKind,
    /// Result snapshot for Create/Update/Merge; the deleted snapshot for
    /// Forget (the forget response only needs the name, kept for audit).
    recorded: Option<ltmrs_domain::guide::Guide>,
    /// Merge sources, for rebuilding the merge response on replay.
    #[serde(default)]
    merged_sources: Vec<String>,
}

/// One guide tool mutation, fully planned by the caller (P1-2): the
/// transform stays in the exec layer, the repository applies it fresh
/// in-transaction with revision enforcement.
#[derive(Debug, Clone)]
pub enum GuideMutation {
    Create {
        guide: ltmrs_domain::guide::Guide,
    },
    Update {
        expected: Option<ltmrs_domain::id::EntityRevision>,
        guide: ltmrs_domain::guide::Guide,
        /// Set when the update renames (old name differs): rename path.
        old_name: Option<String>,
    },
    Forget {
        name: String,
    },
    Merge {
        sources: Vec<String>,
        expected: Vec<(String, ltmrs_domain::id::EntityRevision)>,
        result: ltmrs_domain::guide::Guide,
    },
    /// `guide_create` updating an existing/similar guide: applies exactly
    /// like Update (no rename) but records the create-update response shape.
    CreateUpdate {
        expected: Option<ltmrs_domain::id::EntityRevision>,
        guide: ltmrs_domain::guide::Guide,
    },
}

/// The recorded outcome of one guide tool operation (P1-2): the exec layer
/// rebuilds the exact tool response from this, identically on first
/// execution and on replay.
#[derive(Debug, Clone)]
pub struct RecordedGuideOp {
    pub kind: GuideOpKind,
    pub guide: Option<ltmrs_domain::guide::Guide>,
    pub merged_sources: Vec<String>,
}

/// Digest-bound receipt for one `suggestion_respond` operation (P1-2): the
/// suggestion status transition and all attempt confidence adjustments
/// commit atomically with the receipt, so a transport retry replays instead
/// of adjusting twice.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SuggestionOpLog {
    digest: String,
    suggestion_id: u64,
    status: ltmrs_domain::session::SuggestionStatus,
    adjusted: u32,
}

/// The recorded outcome of one `suggestion_respond` operation (P1-2).
#[derive(Debug, Clone)]
pub struct RecordedSuggestionOp {
    pub suggestion_id: u64,
    pub status: ltmrs_domain::session::SuggestionStatus,
    pub adjusted: u32,
}

/// Which session link field [`CanonicalRepository::track_session_link`]
/// appends to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLinkField {
    GuideUsed,
    MemoryRead,
    MemoryCreated,
}

impl CanonicalRepository {
    /// Open a repository with the production wall-clock.
    pub fn open(base_path: &str) -> DomainResult<Self> {
        Self::open_with_clock(
            base_path,
            std::sync::Arc::new(ltmrs_domain::clock::SystemClock),
        )
    }

    /// Open a repository with an explicit clock (for tests / deterministic time).
    pub fn open_with_clock(
        base_path: &str,
        clock: std::sync::Arc<dyn ltmrs_domain::clock::Clock + Send + Sync>,
    ) -> DomainResult<Self> {
        let db = OptimisticTxDatabase::builder(base_path)
            .open()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;

        // Fault injector shared with the migration runner and the command path.
        let fault_injector = std::sync::Arc::new(FaultInjector::new());

        // Run migrations with safety checks.
        let runner = MigrationRunner::new(
            MigrationPlan::default_plan(),
            MigrationSafetyRules::default(),
        )
        .with_fault_injector(std::sync::Arc::clone(&fault_injector));
        match runner.assess(&db)? {
            MigrationOutcome::AlreadyCurrent { .. } | MigrationOutcome::Migrated { .. } => {}
            MigrationOutcome::RefusedNewerVersion {
                store_version,
                supported_version,
            } => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    format!(
                        "refusing to open: store schema {store_version} is newer than supported {supported_version}"
                    ),
                ));
            }
            MigrationOutcome::RefusedUnknownSchema { reason } => {
                return Err(DomainError::new(DomainErrorCode::Validation, reason));
            }
        }

        let memories = Self::keyspace(&db, "memories")?;
        let relations = Self::keyspace(&db, "relations")?;
        let receipts = Self::keyspace(&db, "receipts")?;
        let aliases = Self::keyspace(&db, "aliases")?;
        let namespaces = Self::keyspace(&db, "namespaces")?;
        let projections = Self::keyspace(&db, "projections")?;
        let generations = Self::keyspace(&db, "generations")?;
        let feedback_events = Self::keyspace(&db, "feedback_events")?;
        let guides = Self::keyspace(&db, "guides")?;
        let suggestions = Self::keyspace(&db, "suggestions")?;
        let guide_ops = Self::keyspace(&db, "guide_ops")?;
        let sessions = Self::keyspace(&db, "sessions")?;
        let session_ops = Self::keyspace(&db, "session_ops")?;
        let suggestion_ops = Self::keyspace(&db, "suggestion_ops")?;

        Ok(Self {
            db,
            memories,
            relations,
            receipts,
            aliases,
            namespaces,
            projections,
            generations,
            feedback_events,
            guides,
            suggestions,
            guide_ops,
            sessions,
            session_ops,
            suggestion_ops,
            commit_hook: std::sync::Mutex::new(None),
            fault_injector,
            clock,
            restore_lock: std::sync::RwLock::new(()),
        })
    }

    fn keyspace(db: &OptimisticTxDatabase, name: &str) -> DomainResult<OptimisticTxKeyspace> {
        db.keyspace(name, KeyspaceCreateOptions::default)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
    }

    /// Access the fault injector for crash/durability testing (RV-18).
    pub fn fault_injector(&self) -> &std::sync::Arc<FaultInjector> {
        &self.fault_injector
    }

    /// Set the post-commit wake-up hook (P2-1): fired once per committed
    /// mutation, never on replay. The projection worker uses it to drive new
    /// jobs immediately; the interval remains as the maintenance fallback.
    pub fn set_commit_hook(&self, hook: std::sync::Arc<dyn Fn() + Send + Sync>) {
        *self.commit_hook.lock().unwrap() = Some(hook);
    }

    /// Fire the commit hook after a durable commit (best-effort: a panicking
    /// hook must never fail the already-committed write).
    fn fire_commit_hook(&self) {
        let hook = self.commit_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook()));
        }
    }

    /// Durability barrier (RQ-18): no ACK without it. Flushes through
    /// fdatasync+metadata so a returned success means the write survives a
    /// process crash and OS-level loss, not just process survival. Called by
    /// every entry point that reports a mutation as successful; progress
    /// bookkeeping with fail-closed loss semantics (projection acks, build
    /// progress notes) documents its buffered mode instead.
    fn persist_barrier(&self) -> DomainResult<()> {
        if self.fault_injector.inject_persist_failure() {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "injected persist failure: durability barrier refused",
            ));
        }
        self.db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
    }

    /// Resume an existing retry namespace for unknown-outcome recovery
    /// (P1-B): returns the SAME epoch without minting a new one, so a
    /// reconnected frontend keeps resolving its pre-failure receipts. The
    /// namespace must exist, belong to this (frontend, channel) pair
    /// (keyed + stored), and be within TTL — otherwise refused as stale
    /// (the caller must surface an unknown outcome, never silently mint a
    /// fresh epoch for an uncertain mutation). A sibling channel resuming
    /// the same epoch is refused: retry namespaces are channel-scoped.
    /// Read-only: no counter bump, no barrier.
    pub fn resume_namespace(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        retry_epoch: u64,
        now_millis: u64,
    ) -> DomainResult<RetryNamespace> {
        match self.lookup_namespace(frontend_id, retry_epoch)? {
            Some(ns) if ns.channel_id != channel_id => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace belongs to another channel",
            )),
            Some(ns) if ns.is_valid_at(now_millis) => Ok(ns),
            Some(_) => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace expired",
            )),
            None => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "unknown retry namespace",
            )),
        }
    }

    /// Issue a new retry namespace for one channel with the default TTL.
    /// Called by the daemon when a frontend authenticates. Epoch allocation
    /// reads inside the write transaction (creating a read dependency), so
    /// concurrent issuers conflict and retry instead of double-issuing the
    /// same epoch. A corrupt epoch counter fails closed (epoch reuse would
    /// confuse replays across channels).
    pub fn issue_namespace(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        now_millis: u64,
    ) -> DomainResult<RetryNamespace> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let key = namespace_key(frontend_id);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let current_epoch = match tx
                .get(&self.namespaces, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                Some(raw) => {
                    let bytes = raw.as_ref();
                    if bytes.len() == 8 {
                        let mut arr = [0u8; 8];
                        arr.copy_from_slice(bytes);
                        u64::from_le_bytes(arr).checked_add(1).ok_or_else(|| {
                            DomainError::new(
                                DomainErrorCode::Validation,
                                "namespace epoch counter exhausted",
                            )
                        })?
                    } else {
                        return Err(DomainError::new(
                            DomainErrorCode::Validation,
                            "corrupt namespace epoch counter",
                        ));
                    }
                }
                None => 1,
            };

            let ns = RetryNamespace::new(
                frontend_id,
                channel_id,
                current_epoch,
                now_millis,
                DEFAULT_NAMESPACE_TTL_MILLIS,
            );

            // Persist the namespace and its fixed expiry.
            tx.insert(&self.namespaces, &key, current_epoch.to_le_bytes());
            let ns_raw = serde_json::to_vec(&ns)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.namespaces, ns_key(&ns), &ns_raw);
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(ns);
                }
                Ok(Err(_)) => continue,
                Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
            }
        }
        Err(DomainError::new(
            DomainErrorCode::Contention,
            "namespace issue conflicted (transient write contention): retry the handshake",
        ))
    }

    /// SSI-exhaustion signal (transient, safe to retry): every write
    /// path that runs out of conflict budget reports Contention, never
    /// Validation — callers and hosts must be able to tell "retry" from
    /// "refused" (see `DomainErrorCode::Contention`). Each site keeps its
    /// specific message; only the code is unified.
    fn exhausted_contention(message: &str) -> DomainError {
        DomainError::new(DomainErrorCode::Contention, message)
    }

    /// Advance the mutation watermark inside the caller's write
    /// transaction (atomic with the mutation itself). Keys are per-writer
    /// (`op_seq:{frontend}` for commands, `op_seq:direct` for the direct
    /// primitives) so concurrent writers never collide on one global key —
    /// a single shared counter would serialize every write under SSI and
    /// collapse concurrent throughput. Shared by the command path and the
    /// direct-write primitives (guides, suggestions, distill side-effects)
    /// so every canonical write counts — the restore delta would otherwise
    /// miss non-command writes. Operational bookkeeping (epochs, projection
    /// jobs, generation lifecycle) does not bump it: only knowledge
    /// records count.
    fn bump_op_seq_tx(&self, tx: &mut OptimisticWriteTx, key: &str) -> DomainResult<()> {
        let seq = match tx
            .get(&self.namespaces, key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
        {
            Some(raw) => {
                let bytes = raw.as_ref();
                if bytes.len() != 8 {
                    return Err(DomainError::new(
                        DomainErrorCode::Validation,
                        "corrupt op sequence counter",
                    ));
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(bytes);
                u64::from_le_bytes(arr)
            }
            None => 0,
        };
        let next = seq.checked_add(1).ok_or_else(|| {
            DomainError::new(DomainErrorCode::Validation, "op sequence exhausted")
        })?;
        tx.insert(&self.namespaces, key, next.to_le_bytes());
        Ok(())
    }

    /// Look up a retry namespace by frontend and epoch.
    pub fn lookup_namespace(
        &self,
        frontend_id: FrontendId,
        retry_epoch: u64,
    ) -> DomainResult<Option<RetryNamespace>> {
        let snapshot = self.db.read_tx();
        let key = format!("ns:{}:{}", frontend_id.as_uuid(), retry_epoch);
        let raw = snapshot
            .get(&self.namespaces, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode::<RetryNamespace>(v.as_ref()))
            .transpose()
    }

    /// Monotonic mutation watermark: sum over per-writer counters
    /// (`op_seq:{frontend}` + `op_seq:direct`). Restores bind
    /// preview/confirm to it so writes landing between the two are
    /// acknowledged, never drained unseen. Absent on old stores: reads as
    /// 0. A malformed counter fails closed (storage trouble is never
    /// silently skipped).
    pub fn op_seq(&self) -> DomainResult<u64> {
        let snapshot = self.db.read_tx();
        let mut total = 0u64;
        for kv in snapshot.iter(&self.namespaces) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if !String::from_utf8_lossy(k.as_ref()).starts_with("op_seq:") {
                continue;
            }
            let bytes = v.as_ref();
            if bytes.len() != 8 {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "corrupt op sequence counter",
                ));
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            total = total.checked_add(u64::from_le_bytes(arr)).ok_or_else(|| {
                DomainError::new(DomainErrorCode::Validation, "op sequence exhausted")
            })?;
        }
        Ok(total)
    }

    /// Garbage-collect expired namespaces and their receipts.
    /// Called periodically by the daemon. Returns the number of receipts removed.
    pub fn gc_expired(&self, now_millis: u64) -> DomainResult<usize> {
        let _restore_guard = self.restore_lock.read().unwrap();
        // Find expired namespaces on a read snapshot. Undecodable entries
        // are corrupt (every reader fails closed on them): collect them
        // for removal below so one bad record cannot leak forever while
        // GC keeps skipping it. A corrupt namespace also orphans its
        // receipts (matching needs the decoded value), so receipts for
        // its frontend are collected too; other frontends are untouched.
        // Malformed watermark keys heal the same way (otherwise op_seq
        // fails closed forever and bricks preview/confirm).
        let snapshot = self.db.read_tx();
        let mut expired: Vec<RetryNamespace> = Vec::new();
        let mut corrupt_keys: Vec<String> = Vec::new();
        // (frontend, epoch) scopes for orphaned receipts. The epoch comes
        // from the corrupt key itself (`ns:{fe}:{epoch}`): scoping to it
        // keeps live epochs' receipts intact (RQ-06 replay). Unparseable
        // keys heal key-only; their receipts strand until a valid same-
        // scope record expires normally (documented residual).
        let mut corrupt_scopes: Vec<(String, Option<String>)> = Vec::new();
        for kv in snapshot.iter(&self.namespaces) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let key_str = String::from_utf8_lossy(k.as_ref());
            if key_str.starts_with("ns:") {
                match decode::<RetryNamespace>(v.as_ref()) {
                    Ok(ns) if !ns.is_valid_at(now_millis) => expired.push(ns),
                    Ok(_) => {}
                    Err(_) => {
                        corrupt_keys.push(key_str.to_string());
                        // ns:{frontend}:{epoch}: scope the orphaned
                        // receipts to this exact epoch so live epochs of
                        // the same frontend keep their replay state.
                        let scope = match key_str.split(':').collect::<Vec<_>>()[..] {
                            [_, fe, epoch] => (fe.to_string(), Some(epoch.to_string())),
                            _ => (String::new(), None),
                        };
                        corrupt_scopes.push(scope);
                    }
                }
            } else if key_str.starts_with("op_seq:") && v.as_ref().len() != 8 {
                // Malformed watermark: op_seq() fails closed on it, so GC
                // heals it (accounting restarts; preview/confirm unblock).
                corrupt_keys.push(key_str.to_string());
            }
        }

        if expired.is_empty() && corrupt_keys.is_empty() {
            return Ok(0);
        }

        // Remove expired namespaces and their receipts in one transaction.
        // Keys are collected before removing (restore_replace precedent):
        // removing while iterating the same keyspace risks skipping
        // entries on iterators without snapshot isolation, orphaning
        // receipts no future GC re-triggers for.
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut removed = 0;
        for ns in &expired {
            // Remove all receipts issued under this retry_epoch.
            let mut doomed = Vec::new();
            for kv in tx.iter(&self.receipts) {
                let (k, _) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let key_str = String::from_utf8_lossy(k.as_ref()).to_string();
                if receipt_matches_namespace(&key_str, ns) {
                    doomed.push(key_str);
                }
            }
            for key in doomed {
                tx.remove(&self.receipts, &key);
                removed += 1;
            }
            let ns_key = ns_key(ns);
            tx.remove(&self.namespaces, &ns_key);
        }
        // Corrupt records are undecodable everywhere (all readers fail
        // closed on them): removing heals the leak without changing any
        // observable outcome. A corrupt namespace also strands its
        // receipts (no trigger can ever match them again), so receipts
        // for its exact (frontend, epoch) scope go too — live epochs keep
        // their RQ-06 replay state; other frontends are untouched.
        // Loud: silent healing would mask storage trouble.
        if !corrupt_keys.is_empty() {
            eprintln!(
                "ltmrs: GC healing {} corrupt namespace/watermark record(s)",
                corrupt_keys.len()
            );
        }
        for key in &corrupt_keys {
            tx.remove(&self.namespaces, key);
        }
        if !corrupt_scopes.is_empty() {
            let mut doomed = Vec::new();
            for kv in tx.iter(&self.receipts) {
                let (k, _) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let key_str = String::from_utf8_lossy(k.as_ref()).to_string();
                let parts: Vec<&str> = key_str.split(':').collect();
                if parts.len() == 4
                    && corrupt_scopes.iter().any(|(fe, epoch)| {
                        parts[1] == fe && epoch.as_deref().is_none_or(|e| parts[2] == e)
                    })
                {
                    doomed.push(key_str);
                }
            }
            for key in doomed {
                tx.remove(&self.receipts, &key);
                removed += 1;
            }
        }

        match tx.commit() {
            Ok(Ok(())) => {
                self.persist_barrier()?;
                Ok(removed)
            }
            Ok(Err(_)) => Err(Self::exhausted_contention("gc conflicted")),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Validate that the command's retry namespace is still valid and
    /// belongs to the calling channel. Expired, unknown, or cross-channel
    /// namespaces are refused as stale.
    fn validate_namespace(&self, ctx: &CommandContext) -> DomainResult<()> {
        let now = self.clock.now_millis();
        match self.lookup_namespace(ctx.frontend_id, ctx.retry_epoch)? {
            Some(ns) if ns.channel_id != ctx.channel_id => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace belongs to another channel",
            )),
            Some(ns) if ns.is_valid_at(now) => Ok(()),
            Some(_) => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "retry namespace expired",
            )),
            None => Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "unknown retry namespace",
            )),
        }
    }

    /// Centralized command application: idempotent, atomic, precondition-checked.
    pub fn apply(&self, ctx: &CommandContext, cmd: &DomainCommand) -> DomainResult<CommandReceipt> {
        let _restore_guard = self.restore_lock.read().unwrap();
        // Validate the retry namespace: expired or unknown namespaces are
        // refused as stale, not silently converted into new work.
        self.validate_namespace(ctx)?;

        // Fast-path idempotency on a read-only snapshot.
        if let Some(r) = self.lookup_receipt(
            ctx.store_generation,
            ctx.frontend_id,
            ctx.retry_epoch,
            ctx.operation_id,
        )? {
            let receipt = self.replay_or_conflict(r, ctx)?;
            self.persist_barrier()?;
            return Ok(receipt);
        }

        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            match self.apply_in_tx(&mut tx, ctx, cmd)? {
                TxAction::Commit(receipt) => {
                    // Fault injection: simulate an unknown commit outcome
                    // (write may or may not have succeeded, ACK lost).
                    if self.fault_injector.inject_unknown_outcome() {
                        // Drop the transaction without committing — simulates a
                        // crash before the durability barrier. The store must
                        // remain consistent; the receipt is NOT recorded.
                        drop(tx);
                        return self.resolve_unknown_outcome(
                            ctx,
                            fjall::Error::Io(std::io::Error::other("injected unknown outcome")),
                        );
                    }
                    match tx.commit() {
                        Ok(Ok(())) => {
                            self.persist_barrier()?;
                            self.fire_commit_hook();
                            return Ok(receipt);
                        }
                        Ok(Err(_conflict)) => {
                            // Storage conflict: retry from a fresh snapshot.
                            continue;
                        }
                        Err(io_err) => {
                            // Unknown commit outcome: resolve via the receipt.
                            return self.resolve_unknown_outcome(ctx, io_err);
                        }
                    }
                }
                TxAction::Replay(receipt) => {
                    tx.rollback();
                    self.persist_barrier()?;
                    return Ok(receipt);
                }
            }
        }
        Err(Self::exhausted_contention(
            "max transaction retries exceeded",
        ))
    }

    fn apply_in_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        ctx: &CommandContext,
        cmd: &DomainCommand,
    ) -> DomainResult<TxAction<CommandReceipt>> {
        // Re-check the receipt inside the transaction to handle the race where
        // another transaction committed it after our fast-path read.
        let key = receipt_key(
            ctx.store_generation,
            ctx.frontend_id,
            ctx.retry_epoch,
            ctx.operation_id,
        );
        if let Some(raw) = tx
            .get(&self.receipts, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
        {
            let existing = decode_receipt(raw.as_ref())?;
            Self::check_replay_owner(&existing, ctx)?;
            if existing.request_digest == ctx.request_digest {
                return Ok(TxAction::Replay(existing));
            }
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }

        // Validate preconditions and apply the command inside the transaction.
        let mut state = CommandState::new(
            tx,
            &self.memories,
            &self.relations,
            &self.aliases,
            &self.projections,
            &self.generations,
            &self.feedback_events,
            self.clock.now_millis(),
        );
        let outcome = apply_command(&mut state, ctx, cmd)?;

        // Store the receipt atomically with the command.
        let receipt = CommandReceipt {
            operation_id: ctx.operation_id,
            store_generation: ctx.store_generation,
            frontend_id: ctx.frontend_id,
            channel_id: ctx.channel_id,
            request_digest: ctx.request_digest.clone(),
            outcome: outcome.clone(),
            retry_epoch: ctx.retry_epoch,
        };
        let raw = encode_receipt(&receipt)?;
        tx.insert(&self.receipts, &key, &raw);

        // Mutation watermark for restore preview/confirm binding: every
        // executed command advances it atomically with its receipt, so a
        // restore can tell whether the live store moved since the preview.
        // Replays return before this point and advance nothing. Per-frontend
        // key: concurrent frontends never collide on one global counter.
        self.bump_op_seq_tx(tx, &op_seq_key(Some(ctx.frontend_id)))?;

        Ok(TxAction::Commit(receipt))
    }

    fn resolve_unknown_outcome(
        &self,
        ctx: &CommandContext,
        io_err: fjall::Error,
    ) -> DomainResult<CommandReceipt> {
        // The commit result is ambiguous. Re-read the receipt with the same
        // operation key to determine whether the command actually committed.
        match self.lookup_receipt(
            ctx.store_generation,
            ctx.frontend_id,
            ctx.retry_epoch,
            ctx.operation_id,
        )? {
            Some(r) if r.request_digest == ctx.request_digest => {
                Self::check_replay_owner(&r, ctx)?;
                // The write did commit: barrier before acknowledging it.
                self.persist_barrier()?;
                Ok(r)
            }
            Some(_) => Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            )),
            None => Err(DomainError::new(
                DomainErrorCode::Validation,
                format!("commit outcome unknown and no receipt recorded: {io_err}"),
            )),
        }
    }

    fn replay_or_conflict(
        &self,
        existing: CommandReceipt,
        ctx: &CommandContext,
    ) -> DomainResult<CommandReceipt> {
        Self::check_replay_owner(&existing, ctx)?;
        if existing.request_digest == ctx.request_digest {
            Ok(existing)
        } else {
            Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ))
        }
    }

    /// Defense-in-depth receipt ownership: a receipt replays only for the
    /// channel that recorded it. The namespace gates (resume/validate)
    /// already enforce this — receipts additionally carry their channel so
    /// a cross-channel replay can never resolve, even if a namespace
    /// check is ever bypassed.
    fn check_replay_owner(existing: &CommandReceipt, ctx: &CommandContext) -> DomainResult<()> {
        if existing.frontend_id != ctx.frontend_id || existing.channel_id != ctx.channel_id {
            return Err(DomainError::new(
                DomainErrorCode::StaleReplay,
                "receipt belongs to another channel",
            ));
        }
        Ok(())
    }

    // ---- Snapshot-consistent reads ----

    /// The store's current generation (1 for a fresh store; bumped on
    /// destructive restores). Used in the IPC handshake to reject clients
    /// targeting a different generation.
    ///
    /// Prefers the cutover record: exactly one generation is Active at a
    /// time, and `activate_generation` flips it atomically with the pointer.
    /// Pre-cutover stores have no records and read the meta pointer.
    /// (Defensive max: multiple Actives are unreachable via this API —
    /// both writers retire the predecessor in the same commit — and
    /// `set_store_generation` heals them; max keeps reads available.)
    pub fn store_generation(&self) -> DomainResult<StoreGeneration> {
        if let Some(active) = self
            .list_generations()?
            .into_iter()
            .filter(|r| r.status == GenerationStatus::Active)
            .map(|r| r.generation)
            .max_by_key(|g| g.as_u64())
        {
            return Ok(active);
        }
        let meta = Self::keyspace(&self.db, "meta")?;
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&meta, "store_generation")
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        match raw {
            Some(v) => {
                let bytes = v.as_ref();
                if bytes.len() >= 8 {
                    let value = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
                    Ok(StoreGeneration::new(value))
                } else {
                    Ok(StoreGeneration::FIRST)
                }
            }
            None => Ok(StoreGeneration::FIRST),
        }
    }

    /// Set the store's active generation. Called by WP-11 restore when a
    /// verified snapshot is activated; projection publication for any other
    /// generation is refused until readers drain (design §8).
    ///
    /// A restore creates a new generation, so past pipeline records must not
    /// survive it: every non-retired record is retired in the same commit.
    /// That keeps the "Active record == pointer" invariant (a stale Active
    /// can never shadow the restored pointer) and kills pre-restore staged
    /// workers' publish rights (design §12.3 step 9: old projection work is
    /// invalidated). A fresh pipeline can be staged immediately after.
    pub fn set_store_generation(&self, generation: StoreGeneration) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let meta = Self::keyspace(&self.db, "meta")?;
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&meta, "store_generation", generation.as_u64().to_le_bytes());
        let now = self.clock.now_millis();
        let retired = self.read_generation_records(&tx)?;
        for mut rec in retired
            .into_iter()
            .filter(|r| !matches!(r.status, GenerationStatus::Retired))
        {
            rec.status = GenerationStatus::Retired;
            rec.updated_at_millis = now;
            tx.insert(
                &self.generations,
                generation_key(rec.generation),
                serde_json::to_vec(&rec)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
            );
        }
        match tx.commit() {
            Ok(Ok(())) => {
                self.persist_barrier()?;
                Ok(())
            }
            Ok(Err(_)) => Err(Self::exhausted_contention("generation switch conflicted")),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Stage a blue-green generation build (design §8.2): allocates the next
    /// generation number, snapshots the watermark denominator (recallable
    /// canonical memories now), and records the build fingerprint. Exactly
    /// one pipeline (Staged/Building/Ready) may exist at a time. The active
    /// pointer is untouched — staging is never observable to readers.
    pub fn stage_generation(&self, fingerprint: ModelFingerprint) -> DomainResult<StoreGeneration> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let records = self.read_generation_records(&tx)?;
        if records.iter().any(|r| {
            matches!(
                r.status,
                GenerationStatus::Staged | GenerationStatus::Building | GenerationStatus::Ready
            )
        }) {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "a generation build is already in progress",
            ));
        }
        let max_record = records.iter().map(|r| r.generation.as_u64()).max();
        let mut next = StoreGeneration::FIRST
            .as_u64()
            .max(self.meta_generation(&tx)?.as_u64());
        if let Some(m) = max_record {
            next = next.max(m);
        }
        let next = StoreGeneration::new(next.checked_add(1).ok_or_else(|| {
            DomainError::new(DomainErrorCode::Validation, "generation counter exhausted")
        })?);
        let desired = self.recallable_count(&tx)? as u64;
        let now = self.clock.now_millis();
        let rec = GenerationRecord {
            generation: next,
            model_fingerprint: Some(fingerprint),
            status: if desired == 0 {
                GenerationStatus::Ready
            } else {
                GenerationStatus::Staged
            },
            desired_memories: desired,
            projected_memories: 0,
            updated_at_millis: now,
            build_dirty: false,
        };
        tx.insert(
            &self.generations,
            generation_key(next),
            serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        );
        match tx.commit() {
            Ok(Ok(())) => {
                self.persist_barrier()?;
                Ok(next)
            }
            Ok(Err(_)) => Err(Self::exhausted_contention("generation staging conflicted")),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Report build progress for a staged generation. Promotes Staged to
    /// Building and to Ready once the watermark is met (projected >=
    /// desired). Ready is sticky upward only through this path — a lower
    /// recount moves it back to Building rather than silently holding Ready.
    ///
    /// Trust boundary: the report attests a fresh build and clears the dirty
    /// flag unconditionally — the repository cannot distinguish a real
    /// rebuild from a bare recount. The operator protocol (final rebuild
    /// before note) is assumed, not enforced; only the refusal paths
    /// (dirty, watermark) are verified.
    ///
    /// Deliberately buffered (no durability barrier): losing a progress
    /// note only delays activation (fail-closed on the watermark), while
    /// knowledge and protocol state always persist (see `persist_barrier`).
    pub fn note_generation_progress(
        &self,
        generation: StoreGeneration,
        projected: u64,
    ) -> DomainResult<GenerationRecord> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut rec = self
            .read_generation_record(&tx, generation)?
            .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "unknown generation"))?;
        match rec.status {
            GenerationStatus::Staged | GenerationStatus::Building | GenerationStatus::Ready => {}
            GenerationStatus::Active | GenerationStatus::Retired => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "only a staged generation accepts build progress",
                ));
            }
        }
        rec.projected_memories = projected;
        rec.updated_at_millis = self.clock.now_millis();
        // The report attests a fresh build of current canonical state, so it
        // clears the dirty flag set by any mid-build write.
        rec.build_dirty = false;
        rec.status = if projected >= rec.desired_memories {
            GenerationStatus::Ready
        } else {
            GenerationStatus::Building
        };
        tx.insert(
            &self.generations,
            generation_key(generation),
            serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        );
        match tx.commit() {
            Ok(Ok(())) => Ok(rec),
            Ok(Err(_)) => Err(Self::exhausted_contention("generation progress conflicted")),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Fetch one generation record, if present.
    pub fn generation_record(
        &self,
        generation: StoreGeneration,
    ) -> DomainResult<Option<GenerationRecord>> {
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.generations, generation_key(generation))
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode_generation(v.as_ref())).transpose()
    }

    /// Whether a generation currently accepts projection publication for a
    /// model fingerprint: staged (or building/ready) with a matching build
    /// fingerprint. Retired, active-through-pointer and unknown generations
    /// are refused — a misconfigured projector advancing the wrong vector
    /// space, or a delayed worker writing a dead generation, is rejected
    /// rather than silently mixed.
    pub fn generation_under_construction(
        &self,
        generation: StoreGeneration,
        fingerprint: ModelFingerprint,
    ) -> DomainResult<bool> {
        Ok(matches!(
            self.generation_record(generation)?,
            Some(rec)
                if matches!(
                    rec.status,
                    GenerationStatus::Staged
                        | GenerationStatus::Building
                        | GenerationStatus::Ready
                ) && rec.model_fingerprint == Some(fingerprint)
        ))
    }

    /// Abandon a staged pipeline that will never activate (interrupted build,
    /// misconfigured fingerprint): marks it Retired so the reaper cleans any
    /// partial rows after retention and a fresh pipeline can be staged.
    /// Active generations cannot be abandoned — restore or cut over instead.
    pub fn abandon_generation(&self, generation: StoreGeneration) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut rec = self
            .read_generation_record(&tx, generation)?
            .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "unknown generation"))?;
        match rec.status {
            GenerationStatus::Staged | GenerationStatus::Building | GenerationStatus::Ready => {}
            GenerationStatus::Active | GenerationStatus::Retired => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "only a staged pipeline can be abandoned",
                ));
            }
        }
        rec.status = GenerationStatus::Retired;
        rec.updated_at_millis = self.clock.now_millis();
        tx.insert(
            &self.generations,
            generation_key(generation),
            serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        );
        match tx.commit() {
            Ok(Ok(())) => {
                self.persist_barrier()?;
                Ok(())
            }
            Ok(Err(_)) => Err(Self::exhausted_contention("generation abandon conflicted")),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// All generation records from a single snapshot (cutover/reaper views).
    pub fn list_generations(&self) -> DomainResult<Vec<GenerationRecord>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.generations) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode_generation(v.as_ref())?);
        }
        out.sort_by_key(|r| r.generation.as_u64());
        Ok(out)
    }

    /// Atomically publish a ready generation (design §12.3 steps 5-8): one
    /// commit flips the active pointer, marks the generation Active, and
    /// retires its predecessor (creating a Retired record when the previous
    /// generation predates cutover records). Ready is re-validated against
    /// the CURRENT recallable count inside the same transaction, so memories
    /// added mid-build cannot slip into a silently partial generation.
    /// Retired generations may be re-activated (rollback needs no rebuild).
    /// Re-activating the already-active generation is a no-op success, so
    /// operator retries after a timeout do not look like failures.
    ///
    /// Generation-granularity safety is structural: any canonical memory
    /// write while a pipeline is open durties it atomically, and activation
    /// refuses dirty pipelines until a fresh build is reported via
    /// `note_generation_progress`. What remains trusted (not verified) is
    /// the report itself — `note()` attests a fresh build and the projected
    /// count comes from the projector's `rebuild()` return. Independently
    /// verify with `Projector::verify_generation_converged` (table-measured)
    /// before activating; per-memory pending verification is a follow-up.
    pub fn activate_generation(&self, generation: StoreGeneration) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        if self.resolve_generation(&tx)? == generation {
            return Ok(());
        }
        let mut rec = self
            .read_generation_record(&tx, generation)?
            .ok_or_else(|| DomainError::new(DomainErrorCode::Validation, "unknown generation"))?;
        match rec.status {
            GenerationStatus::Ready | GenerationStatus::Retired => {}
            GenerationStatus::Staged | GenerationStatus::Building | GenerationStatus::Active => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "only a ready (or retained retired) generation can be activated",
                ));
            }
        }
        let current = self.resolve_generation(&tx)?;
        let live = self.recallable_count(&tx)? as u64;
        // A dirty pipeline must be rebuilt and re-reported before activation
        // — except rollback: a Retired generation reuses retained rows, so
        // dirt is moot there exactly as the watermark is (abandon/restore
        // preserve the flag, and Retired has no clearing path by design).
        if rec.status == GenerationStatus::Ready && rec.build_dirty {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "build is dirty: canonical state changed since the last build report; rebuild and re-note first",
            ));
        }
        if rec.status == GenerationStatus::Ready && rec.projected_memories < live {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                format!(
                    "stale watermark: {} projected but {} recallable; rebuild first",
                    rec.projected_memories, live
                ),
            ));
        }
        // Note: re-activating a Retired generation (rollback) skips the
        // watermark — it reuses retained rows, no build needed. Rolling back
        // after the reaper deleted those rows yields an empty generation;
        // the retention window is the guardrail, not this check.
        let now = self.clock.now_millis();
        tx.insert(
            &Self::keyspace(&self.db, "meta")?,
            "store_generation",
            generation.as_u64().to_le_bytes(),
        );
        rec.status = GenerationStatus::Active;
        rec.updated_at_millis = now;
        tx.insert(
            &self.generations,
            generation_key(generation),
            serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        );
        match self.read_generation_record(&tx, current)? {
            Some(mut prev) => {
                prev.status = GenerationStatus::Retired;
                prev.updated_at_millis = now;
                tx.insert(
                    &self.generations,
                    generation_key(current),
                    serde_json::to_vec(&prev).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?,
                );
            }
            None => {
                // Pre-record predecessor: create its Retired record so the
                // reaper sees a uniform retention view (fingerprint unknown).
                let prev = GenerationRecord {
                    generation: current,
                    model_fingerprint: None,
                    status: GenerationStatus::Retired,
                    desired_memories: 0,
                    projected_memories: 0,
                    updated_at_millis: now,
                    build_dirty: false,
                };
                tx.insert(
                    &self.generations,
                    generation_key(current),
                    serde_json::to_vec(&prev).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?,
                );
            }
        }
        match tx.commit() {
            Ok(Ok(())) => {
                self.persist_barrier()?;
                Ok(())
            }
            Ok(Err(_)) => Err(Self::exhausted_contention(
                "generation activation conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Read one generation record inside a write transaction.
    fn read_generation_record(
        &self,
        tx: &OptimisticWriteTx,
        generation: StoreGeneration,
    ) -> DomainResult<Option<GenerationRecord>> {
        let raw = tx
            .get(&self.generations, generation_key(generation))
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode_generation(v.as_ref())).transpose()
    }

    /// Read all generation records inside a write transaction.
    fn read_generation_records(
        &self,
        tx: &OptimisticWriteTx,
    ) -> DomainResult<Vec<GenerationRecord>> {
        let mut out = Vec::new();
        for kv in tx.iter(&self.generations) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode_generation(v.as_ref())?);
        }
        Ok(out)
    }

    /// Resolve the active generation inside a write transaction: the Active
    /// record when present, else the meta pointer (pre-cutover stores).
    fn resolve_generation(&self, tx: &OptimisticWriteTx) -> DomainResult<StoreGeneration> {
        if let Some(active) = self
            .read_generation_records(tx)?
            .into_iter()
            .filter(|r| r.status == GenerationStatus::Active)
            .map(|r| r.generation)
            .max_by_key(|g| g.as_u64())
        {
            return Ok(active);
        }
        self.meta_generation(tx)
    }

    /// Read the raw meta pointer inside a write transaction.
    fn meta_generation(&self, tx: &OptimisticWriteTx) -> DomainResult<StoreGeneration> {
        let meta = Self::keyspace(&self.db, "meta")?;
        let raw = tx
            .get(&meta, "store_generation")
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        match raw {
            Some(v) => {
                let bytes = v.as_ref();
                if bytes.len() >= 8 {
                    let value = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
                    Ok(StoreGeneration::new(value))
                } else {
                    Ok(StoreGeneration::FIRST)
                }
            }
            None => Ok(StoreGeneration::FIRST),
        }
    }

    /// Count recallable canonical memories inside a write transaction (the
    /// watermark denominator/numerator guard for staging and activation).
    fn recallable_count(&self, tx: &OptimisticWriteTx) -> DomainResult<usize> {
        let mut count = 0usize;
        for kv in tx.iter(&self.memories) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let memory: Memory = serde_json::from_slice(v.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if memory.lifecycle.is_recallable() {
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn lookup_receipt(
        &self,
        generation: StoreGeneration,
        frontend_id: FrontendId,
        retry_epoch: u64,
        op: OperationId,
    ) -> DomainResult<Option<CommandReceipt>> {
        let key = receipt_key(generation, frontend_id, retry_epoch, op);
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.receipts, key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode_receipt(v.as_ref())).transpose()
    }

    /// Snapshot-consistent multi-get: all reads share one canonical view.
    pub fn get_memories(&self, ids: &[EntityId]) -> DomainResult<Vec<Memory>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for id in ids {
            let key = id.as_uuid().to_string();
            if let Some(raw) = snapshot
                .get(&self.memories, key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                out.push(decode::<Memory>(raw.as_ref())?);
            }
        }
        Ok(out)
    }

    /// All canonical relations from a single snapshot (graph consumers).
    pub fn all_relations(&self) -> DomainResult<Vec<Relation>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.relations) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<Relation>(v.as_ref())?);
        }
        Ok(out)
    }

    /// Resolve a legacy string ID (external alias or UUID string) to an
    /// EntityId. The legacy wire uses string IDs; ltmrs canonical IDs are
    /// UUIDs, so an ID that is not a registered alias is parsed as a UUID.
    pub fn resolve_id(&self, id: &str) -> DomainResult<EntityId> {
        let snapshot = self.db.read_tx();
        // First try the alias keyspace.
        if let Some(raw) = snapshot
            .get(&self.aliases, id)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
        {
            let s = String::from_utf8_lossy(raw.as_ref()).to_string();
            if let Ok(u) = uuid::Uuid::parse_str(&s) {
                return Ok(EntityId::new(u));
            }
        }
        // Fall back to parsing as a UUID.
        uuid::Uuid::parse_str(id)
            .map(EntityId::new)
            .map_err(|_| DomainError::new(DomainErrorCode::NotFound, format!("unknown ID: {id}")))
    }

    /// The legacy string ID for a memory: its external alias if set, else the
    /// UUID string.
    pub fn legacy_id(&self, memory: &Memory) -> String {
        memory
            .external_alias
            .as_ref()
            .map(|a| a.as_str().to_string())
            .unwrap_or_else(|| memory.id.as_uuid().to_string())
    }

    /// Graph-neighbor traversal from a single snapshot.
    pub fn neighbors(&self, id: EntityId) -> DomainResult<Vec<Relation>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.relations) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let rel: Relation = decode(v.as_ref())?;
            if rel.source == id || rel.target == id {
                out.push(rel);
            }
        }
        Ok(out)
    }

    /// Whether a memory still has a pending projection (embedding not yet
    /// computed). Used by the projection worker to know what remains to index.
    pub fn has_pending_projection(&self, id: EntityId) -> DomainResult<bool> {
        Ok(self.projection_job(id)?.is_some())
    }

    /// Read the durable desired-state job for a memory, if any is pending.
    pub fn projection_job(
        &self,
        id: EntityId,
    ) -> DomainResult<Option<ltmrs_domain::projection::ProjectionJob>> {
        let snapshot = self.db.read_tx();
        let key = id.as_uuid().to_string();
        let raw = snapshot
            .get(&self.projections, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode::<ltmrs_domain::projection::ProjectionJob>(v.as_ref()))
            .transpose()
    }

    /// List every pending projection job (the worker's durable work queue).
    pub fn projection_jobs(&self) -> DomainResult<Vec<ltmrs_domain::projection::ProjectionJob>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.projections) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<ltmrs_domain::projection::ProjectionJob>(
                v.as_ref(),
            )?);
        }
        Ok(out)
    }

    /// Compare-and-clear a projection job. Returns true only if the stored job
    /// still carries exactly this seq — a stale worker (whose desired revision
    /// was superseded, or whose memory was forgotten) gets false and leaves no
    /// trace. Retries on storage conflict from a fresh snapshot.
    /// Acknowledge a projection job as published. Deliberately buffered
    /// (no durability barrier): losing an ack only republishes idempotent
    /// work, while every ack costs a barrier. Progress notes share this
    /// treatment; knowledge and protocol state always persist (see
    /// `persist_barrier`).
    pub fn acknowledge_projection(&self, id: EntityId, seq: u64) -> DomainResult<bool> {
        let _restore_guard = self.restore_lock.read().unwrap();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let key = id.as_uuid().to_string();
            match tx
                .get(&self.projections, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                Some(raw) => {
                    let job: ltmrs_domain::projection::ProjectionJob = decode(raw.as_ref())?;
                    if job.memory_id != id || job.seq != seq {
                        tx.rollback();
                        return Ok(false);
                    }
                    tx.remove(&self.projections, &key);
                }
                None => {
                    tx.rollback();
                    return Ok(false);
                }
            }
            match tx.commit() {
                Ok(Ok(())) => return Ok(true),
                Ok(Err(_conflict)) => continue,
                Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
            }
        }
        Err(Self::exhausted_contention(
            "max transaction retries exceeded",
        ))
    }

    /// The count of pending projection jobs (projection lag). A stalled worker or
    /// embedder shows up here as a non-zero, growing number — separate from both
    /// canonical durability and vector availability.
    pub fn projection_lag(&self) -> DomainResult<usize> {
        Ok(self.projection_jobs()?.len())
    }

    /// The age of the oldest pending job at `now_millis`, or None when nothing is
    /// pending. This exposes how long a write has waited to be projected (RQ-08).
    pub fn oldest_pending_age_millis(&self, now_millis: u64) -> DomainResult<Option<u64>> {
        let jobs = self.projection_jobs()?;
        Ok(jobs
            .iter()
            .map(|j| now_millis.saturating_sub(j.enqueued_at_millis))
            .max())
    }

    /// Enqueue (or advance) a projection job using the repository's clock for
    /// the enqueue timestamp. Used by the projector to record semantic retries
    /// when an embedding pass fails; `seq` must be chosen by the caller so that
    /// stale acknowledgements cannot clear newer work.
    pub fn enqueue_projection_job(
        &self,
        memory_id: EntityId,
        desired_document_revision: ltmrs_domain::id::DocumentRevision,
        seq: u64,
        is_tombstone: bool,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let job = ltmrs_domain::projection::ProjectionJob {
            memory_id,
            desired_document_revision,
            seq,
            enqueued_at_millis: self.clock.now_millis(),
            is_tombstone,
        };

        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(
                &self.projections,
                memory_id.as_uuid().to_string(),
                serde_json::to_vec(&job)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    .as_slice(),
            );
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_conflict)) => continue,
                Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
            }
        }
        Err(Self::exhausted_contention(
            "max transaction retries exceeded",
        ))
    }

    pub fn feedback_events(&self) -> DomainResult<Vec<ltmrs_domain::session::FeedbackEvent>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.feedback_events) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<ltmrs_domain::session::FeedbackEvent>(v.as_ref())?);
        }
        Ok(out)
    }

    // ---- Guide and suggestion storage (WP-09) ----

    /// All guides from a single snapshot.
    pub fn get_guides(&self) -> DomainResult<Vec<ltmrs_domain::guide::Guide>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.guides) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<ltmrs_domain::guide::Guide>(v.as_ref())?);
        }
        Ok(out)
    }

    /// A single guide by name (case-insensitive, matching upstream COLLATE NOCASE).
    pub fn get_guide(&self, name: &str) -> DomainResult<Option<ltmrs_domain::guide::Guide>> {
        let target = name.to_lowercase();
        Ok(self
            .get_guides()?
            .into_iter()
            .find(|g| g.name.eq_ignore_ascii_case(&target)))
    }

    /// Store a guide with revision enforcement (re-review P1-2): every
    /// mutation of an existing guide must invalidate concurrent plans.
    /// `expected=None` creates if absent and fails when the key already
    /// exists; `expected=Some(rev)` requires the live record at `rev`
    /// (else RevisionConflict) and advances it. The blind `put_guide`
    /// remains for seed paths that own their key outright.
    pub fn put_guide_checked(
        &self,
        expected: Option<ltmrs_domain::id::EntityRevision>,
        guide: &ltmrs_domain::guide::Guide,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            self.put_guide_apply_tx(&mut tx, expected, guide)?;
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide write conflicted"))
    }

    /// Revision-checked guide put inside the caller's transaction (tx core
    /// shared by the standalone write and the idempotent tool wrapper).
    /// Returns the guide as written (revision advanced on updates).
    fn put_guide_apply_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        expected: Option<ltmrs_domain::id::EntityRevision>,
        guide: &ltmrs_domain::guide::Guide,
    ) -> DomainResult<ltmrs_domain::guide::Guide> {
        let key = guide.name.to_lowercase();
        let fresh: Option<ltmrs_domain::guide::Guide> = tx
            .get(&self.guides, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            .map(|raw| {
                serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
            })
            .transpose()?;
        let mut guide = guide.clone();
        match (fresh, expected) {
            (None, None) => {}
            (Some(_), None) => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "guide already exists",
                ));
            }
            (None, Some(_)) => {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "guide not found",
                ));
            }
            (Some(live), Some(rev)) => {
                if live.entity_revision != rev {
                    return Err(DomainError::new(
                        DomainErrorCode::RevisionConflict,
                        "guide changed since read: re-read and re-plan",
                    ));
                }
                guide.entity_revision = live.entity_revision.next();
            }
        }
        let raw = serde_json::to_vec(&guide)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.guides, &key, raw.as_slice());
        Ok(guide)
    }

    /// Store a guide (keyed by lowercased name).
    pub fn put_guide(&self, guide: &ltmrs_domain::guide::Guide) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let key = guide.name.to_lowercase();
        let raw = serde_json::to_vec(guide)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, &key, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide write conflicted"))
    }

    /// Practice a guide idempotently (P1 replay safety, hardened re-review
    /// R5): the operation ID is logged atomically with the guide mutation.
    /// A retried operation with the same ID + digest returns the RECORDED
    /// guide snapshot (not current contents); the same ID with a different
    /// digest is rejected as key reuse. Fresh-read inside each retry
    /// preserves concurrent updates.
    #[allow(clippy::too_many_arguments)]
    pub fn practice_guide_idempotent(
        &self,
        operation_id: &str,
        digest: &str,
        guide_name: &str,
        category: &str,
        description: Option<&str>,
        contexts: &[String],
        learnings: &[String],
        validated_by: &[String],
        outcome: Option<bool>,
        now_millis: u64,
    ) -> DomainResult<ltmrs_domain::guide::Guide> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            // Replay check first: same operation already applied.
            if let Some(raw) = tx
                .get(&self.guide_ops, operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                // New format: digest-bound recorded outcome. The barrier runs
                // again before acknowledging (re-review P1-1): a visible
                // receipt is not proof its flush succeeded — a prior
                // barrier failure must fail this replay too.
                if let Ok(log) = serde_json::from_slice::<PracticeLog>(raw.as_ref()) {
                    if log.digest != digest {
                        return Err(DomainError::new(
                            DomainErrorCode::KeyReuseDifferentInput,
                            "operation key reused with different input",
                        ));
                    }
                    self.persist_barrier()?;
                    return Ok(log.recorded);
                }
                // Legacy bare-name entries (pre-digest): preserve exact old
                // semantics (current contents, or NotFound when forgotten) —
                // no re-application, no new counting.
                if let Ok(name) = serde_json::from_slice::<String>(raw.as_ref()) {
                    let key = name.to_lowercase();
                    if let Some(raw) = tx
                        .get(&self.guides, &key)
                        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    {
                        let guide: ltmrs_domain::guide::Guide =
                            serde_json::from_slice(raw.as_ref()).map_err(|e| {
                                DomainError::new(DomainErrorCode::Validation, e.to_string())
                            })?;
                        return Ok(guide);
                    }
                    return Err(DomainError::new(
                        DomainErrorCode::NotFound,
                        "guide not found (practiced guide was forgotten)",
                    ));
                }
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "unrecognized practice log entry",
                ));
            }
            // Fresh guide state inside the tx.
            let key = guide_name.to_lowercase();
            let existing: Option<ltmrs_domain::guide::Guide> = tx
                .get(&self.guides, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                .map(|raw| {
                    serde_json::from_slice(raw.as_ref())
                        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
                })
                .transpose()?;
            let mut updated = match existing {
                None => {
                    let mut g = ltmrs_domain::guide::Guide {
                        name: guide_name.to_lowercase().trim().to_string(),
                        category: category.to_lowercase().trim().to_string(),
                        description: description.unwrap_or("").trim().to_string(),
                        contexts: contexts
                            .iter()
                            .map(|c| c.to_lowercase().trim().to_string())
                            .filter(|c| !c.is_empty())
                            .collect(),
                        learnings: learnings
                            .iter()
                            .map(|l| l.trim().to_string())
                            .filter(|l| !l.is_empty())
                            .collect(),
                        usage_count: 1,
                        last_used: Some(ltmrs_domain::memory::Instant::new(now_millis)),
                        success_count: 0,
                        failure_count: 0,
                        anti_patterns: vec![],
                        pitfalls: vec![],
                        depends_on: vec![],
                        enables: vec![],
                        source_memories: vec![],
                        validated_by: vec![],
                        superseded_by: None,
                        deprecated: false,
                        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
                        created_at: ltmrs_domain::memory::Instant::new(now_millis),
                        updated_at: ltmrs_domain::memory::Instant::new(now_millis),
                    };
                    if outcome == Some(true) {
                        g.success_count = 1;
                    } else if outcome == Some(false) {
                        g.failure_count = 1;
                    }
                    g
                }
                Some(mut g) => {
                    g.usage_count += 1;
                    g.last_used = Some(ltmrs_domain::memory::Instant::new(now_millis));
                    if g.description.is_empty()
                        && let Some(desc) = description
                    {
                        g.description = desc.trim().to_string();
                    }
                    for ctx in contexts {
                        let normalized = ctx.to_lowercase().trim().to_string();
                        if !normalized.is_empty()
                            && !g
                                .contexts
                                .iter()
                                .any(|c| c.eq_ignore_ascii_case(&normalized))
                        {
                            g.contexts.push(normalized);
                        }
                    }
                    for learning in learnings {
                        let trimmed = learning.trim().to_string();
                        if !trimmed.is_empty() && !g.learnings.contains(&trimmed) {
                            g.learnings.push(trimmed);
                        }
                    }
                    if outcome == Some(true) {
                        g.success_count += 1;
                    } else if outcome == Some(false) {
                        g.failure_count += 1;
                    }
                    g.entity_revision = g.entity_revision.next();
                    g.updated_at = ltmrs_domain::memory::Instant::new(now_millis);
                    g
                }
            };
            for mem_id in validated_by {
                if !updated.validated_by.contains(mem_id) {
                    updated.validated_by.push(mem_id.clone());
                }
            }
            let raw = serde_json::to_vec(&updated)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, &key, raw.as_slice());
            let log = PracticeLog {
                name: updated.name.clone(),
                digest: digest.to_string(),
                recorded: updated.clone(),
            };
            let log_raw = serde_json::to_vec(&log)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guide_ops, operation_id, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(updated);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide practice conflicted"))
    }

    /// Apply one session_end guide outcome atomically with its idempotency
    /// marker (re-review R5, hardened re-review P1-3): the success/failure
    /// count bump and the `{op}:guide:{name}` marker commit in ONE
    /// transaction. Returns true when newly applied, false when the marker
    /// was already present for the same arguments (retry resumes without
    /// double-counting) or the guide is gone (forget wins — no marker
    /// written, so a later retry re-checks). The marker binds the request
    /// digest AND outcome: a retry with changed arguments after partial
    /// effects rejects as key reuse instead of completing a mixed outcome.
    /// Entity revision advances so concurrent merges observe the change.
    pub fn apply_session_guide_effect(
        &self,
        operation_id: &str,
        digest: &str,
        guide_name: &str,
        success: bool,
        now_millis: u64,
    ) -> DomainResult<bool> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let marker = format!("{}:guide:{}", operation_id, guide_name.to_lowercase());
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.guide_ops, &marker)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                // A completed marker for this effect binds its digest and
                // outcome: same arguments resume, changed arguments reject
                // — a mixed-outcome completion can never assemble.
                let marked: serde_json::Value = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let same = marked.get("digest").and_then(|d| d.as_str()) == Some(digest)
                    && marked.get("success").and_then(|s| s.as_bool()) == Some(success);
                if same {
                    return Ok(false);
                }
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input after partial effects",
                ));
            }
            let guide_key = guide_name.to_lowercase();
            let raw = tx
                .get(&self.guides, &guide_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Ok(false);
            };
            let mut guide: ltmrs_domain::guide::Guide = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if success {
                guide.success_count += 1;
            } else {
                guide.failure_count += 1;
            }
            guide.entity_revision = guide.entity_revision.next();
            guide.updated_at = ltmrs_domain::memory::Instant::new(now_millis);
            let raw = serde_json::to_vec(&guide)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, &guide_key, raw.as_slice());
            let marker_raw = serde_json::to_vec(&serde_json::json!({
                "applied": true,
                "digest": digest,
                "success": success,
            }))
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guide_ops, &marker, marker_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(true);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "session guide effect conflicted",
        ))
    }

    /// Distill a memory fragment into a guide as ONE canonical operation
    /// (re-review R2): the memory and the guide are both read fresh inside
    /// the transaction, the learning/context merge, usage bump, source link
    /// and memory link patch commit together with an operation receipt. A
    /// concurrent content update can never be overwritten by a stale clone,
    /// because no pre-transaction snapshot exists. Replay returns the
    /// recorded guide; digest mismatch rejects.
    #[allow(clippy::too_many_arguments)]
    pub fn distill_memory_link(
        &self,
        operation_id: &str,
        digest: &str,
        memory_id: ltmrs_domain::id::EntityId,
        guide_name: &str,
        category_default: &str,
        now_millis: u64,
    ) -> DomainResult<ltmrs_domain::guide::Guide> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.guide_ops, operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                // Recorded outcome — but durability is not inherited from a
                // visible receipt (re-review P1-1): flush again first.
                if let Ok(log) = serde_json::from_slice::<PracticeLog>(raw.as_ref()) {
                    if log.digest != digest {
                        return Err(DomainError::new(
                            DomainErrorCode::KeyReuseDifferentInput,
                            "operation key reused with different input",
                        ));
                    }
                    self.persist_barrier()?;
                    return Ok(log.recorded);
                }
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "unrecognized distill log entry",
                ));
            }
            // Fresh memory read inside the transaction.
            let mem_key = memory_id.as_uuid().to_string();
            let raw = tx
                .get(&self.memories, &mem_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "memory fragment not found",
                ));
            };
            let mut memory: Memory = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            // Fresh guide read inside the transaction.
            let guide_key = guide_name.to_lowercase();
            let existing: Option<ltmrs_domain::guide::Guide> = tx
                .get(&self.guides, &guide_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                .map(|raw| {
                    serde_json::from_slice(raw.as_ref())
                        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
                })
                .transpose()?;
            let context = memory
                .project
                .clone()
                .unwrap_or_else(|| "global".to_string())
                .to_lowercase()
                .trim()
                .to_string();
            let mut updated = match existing {
                Some(mut g) => {
                    if !g.learnings.contains(&memory.fragment) {
                        g.learnings.push(memory.fragment.clone());
                    }
                    if !context.is_empty() && !g.contexts.contains(&context) {
                        g.contexts.push(context);
                    }
                    g.usage_count += 1;
                    g.last_used = Some(ltmrs_domain::memory::Instant::new(now_millis));
                    g.entity_revision = g.entity_revision.next();
                    g
                }
                None => {
                    let project_ctx = memory
                        .project
                        .clone()
                        .unwrap_or_else(|| "global".to_string())
                        .to_lowercase()
                        .trim()
                        .to_string();
                    ltmrs_domain::guide::Guide {
                        name: guide_name.to_lowercase().trim().to_string(),
                        category: category_default.to_lowercase().trim().to_string(),
                        description: "Created via distillation from memory.".to_string(),
                        contexts: if project_ctx.is_empty() {
                            vec![]
                        } else {
                            vec![project_ctx]
                        },
                        learnings: vec![memory.fragment.clone()],
                        usage_count: 1,
                        last_used: Some(ltmrs_domain::memory::Instant::new(now_millis)),
                        success_count: 0,
                        failure_count: 0,
                        anti_patterns: vec![],
                        pitfalls: vec![],
                        depends_on: vec![],
                        enables: vec![],
                        source_memories: vec![],
                        validated_by: vec![],
                        superseded_by: None,
                        deprecated: false,
                        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
                        created_at: ltmrs_domain::memory::Instant::new(now_millis),
                        updated_at: ltmrs_domain::memory::Instant::new(now_millis),
                    }
                }
            };
            if !updated.source_memories.contains(&memory_id) {
                updated.source_memories.push(memory_id);
            }
            updated.updated_at = ltmrs_domain::memory::Instant::new(now_millis);
            // Memory link patch on the FRESH record: related_guides plus
            // distill_candidate clear, entity-only revision advance.
            let normalized_name = guide_name.to_lowercase().trim().to_string();
            if !memory.related_guides.iter().any(|g| g == &normalized_name) {
                memory.related_guides.push(normalized_name);
            }
            memory.distill_candidate = false;
            memory.entity_revision = memory.entity_revision.next();
            let guide_raw = serde_json::to_vec(&updated)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, &guide_key, guide_raw.as_slice());
            let mem_raw = serde_json::to_vec(&memory)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.memories, &mem_key, mem_raw.as_slice());
            let log = PracticeLog {
                name: updated.name.clone(),
                digest: digest.to_string(),
                recorded: updated.clone(),
            };
            let log_raw = serde_json::to_vec(&log)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guide_ops, operation_id, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(updated);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide distill conflicted"))
    }

    /// Write a memory record directly (compatibility adapter derived writes
    /// that bypass the command gateway; kept for non-user-addressable paths.
    /// Guide tool paths that touch user-visible memories must use the
    /// fresh-read patch helpers or single-transaction operations instead —
    /// never read-modify-write a stale clone through this method.
    pub fn put_memory_direct(&self, memory: &Memory) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let key = memory.id.as_uuid().to_string();
        let raw = serde_json::to_vec(memory)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.memories, &key, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("memory write conflicted"))
    }

    /// Delete a guide by name (case-insensitive). Returns true if removed.
    pub fn delete_guide(&self, name: &str) -> DomainResult<bool> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let key = name.to_lowercase();
        let snapshot = self.db.read_tx();
        let exists = snapshot
            .get(&self.guides, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            .is_some();
        if !exists {
            return Ok(false);
        }
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.remove(&self.guides, &key);
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(true);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide delete conflicted"))
    }

    /// Rename a guide reference in every memory's `related_guides` (P1 atomic
    /// fix): patches freshly read records inside the transaction, so a
    /// concurrent content update is preserved (no stale-clone overwrite).
    /// `related_guides` is unindexed, so the document revision stays put
    /// while the entity revision advances for conflict detection. Errors
    /// propagate — callers must not swallow them with `let _`.
    pub fn rename_guide_references(&self, old_name: &str, new_name: &str) -> DomainResult<usize> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let old_norm = old_name.to_lowercase().trim().to_string();
        let new_norm = new_name.to_lowercase().trim().to_string();
        if old_norm.is_empty() || old_norm == new_norm {
            return Ok(0);
        }
        // Collect affected memory IDs from a snapshot (keys only; values are
        // re-read fresh inside each write attempt).
        let ids: Vec<String> = {
            let snapshot = self.db.read_tx();
            let mut out = Vec::new();
            for kv in snapshot.iter(&self.memories) {
                let (k, v) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let key: String = String::from_utf8_lossy(k.as_ref()).into_owned();
                let mem: Memory = serde_json::from_slice(v.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if mem
                    .related_guides
                    .iter()
                    .any(|g| g.eq_ignore_ascii_case(&old_norm))
                {
                    out.push(key);
                }
            }
            out
        };
        if ids.is_empty() {
            return Ok(0);
        }
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut patched = 0usize;
            for key in &ids {
                let raw = tx
                    .get(&self.memories, key)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let Some(raw) = raw else { continue };
                let mut mem: Memory = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if !mem
                    .related_guides
                    .iter()
                    .any(|g| g.eq_ignore_ascii_case(&old_norm))
                {
                    continue;
                }
                // Patch fresh record: swap the reference, dedup, preserve all
                // other content (concurrent updates survive).
                let mut refs: Vec<String> = mem
                    .related_guides
                    .iter()
                    .filter(|g| !g.eq_ignore_ascii_case(&old_norm))
                    .cloned()
                    .collect();
                if !refs.iter().any(|g| g.eq_ignore_ascii_case(&new_norm)) {
                    refs.push(new_norm.clone());
                }
                mem.related_guides = refs;
                mem.entity_revision = mem.entity_revision.next();
                let raw = serde_json::to_vec(&mem)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                tx.insert(&self.memories, key, raw.as_slice());
                patched += 1;
            }
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(patched);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "guide rename references conflicted",
        ))
    }

    /// Remove a guide reference from every memory's `related_guides` (P1
    /// atomic fix): same fresh-read patch contract as renames.
    pub fn remove_guide_references(&self, guide_name: &str) -> DomainResult<usize> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let normalized = guide_name.to_lowercase().trim().to_string();
        if normalized.is_empty() {
            return Ok(0);
        }
        let ids: Vec<String> = {
            let snapshot = self.db.read_tx();
            let mut out = Vec::new();
            for kv in snapshot.iter(&self.memories) {
                let (k, v) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let key: String = String::from_utf8_lossy(k.as_ref()).into_owned();
                let mem: Memory = serde_json::from_slice(v.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if mem
                    .related_guides
                    .iter()
                    .any(|g| g.eq_ignore_ascii_case(&normalized))
                {
                    out.push(key);
                }
            }
            out
        };
        if ids.is_empty() {
            return Ok(0);
        }
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut patched = 0usize;
            for key in &ids {
                let raw = tx
                    .get(&self.memories, key)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let Some(raw) = raw else { continue };
                let mut mem: Memory = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if !mem
                    .related_guides
                    .iter()
                    .any(|g| g.eq_ignore_ascii_case(&normalized))
                {
                    continue;
                }
                mem.related_guides = mem
                    .related_guides
                    .iter()
                    .filter(|g| !g.eq_ignore_ascii_case(&normalized))
                    .cloned()
                    .collect();
                mem.entity_revision = mem.entity_revision.next();
                let raw = serde_json::to_vec(&mem)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                tx.insert(&self.memories, key, raw.as_slice());
                patched += 1;
            }
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(patched);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "guide remove references conflicted",
        ))
    }

    /// Merge guides atomically (P1): reference updates, source deletions and
    /// the merged put commit in one Fjall transaction — no visible half-merge.
    /// Failures (missing source, existing result, storage conflict exhaustion)
    /// return errors; nothing is swallowed.
    pub fn merge_guides_atomically(
        &self,
        source_names: &[String],
        expected_revisions: &[(String, ltmrs_domain::id::EntityRevision)],
        result: &ltmrs_domain::guide::Guide,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        if source_names.len() < 2 {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "'guides' must name at least 2 guides",
            ));
        }
        let result_key = result.name.to_lowercase();
        let source_keys: Vec<String> = source_names.iter().map(|n| n.to_lowercase()).collect();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            self.merge_guides_apply_tx(
                &mut tx,
                &source_keys,
                expected_revisions,
                &result_key,
                result,
            )?;
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide merge conflicted"))
    }

    /// Merge inside the caller's transaction (tx core shared by the
    /// standalone merge and the idempotent tool wrapper).
    #[allow(clippy::too_many_arguments)]
    fn merge_guides_apply_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        source_keys: &[String],
        expected_revisions: &[(String, ltmrs_domain::id::EntityRevision)],
        result_key: &str,
        result: &ltmrs_domain::guide::Guide,
    ) -> DomainResult<()> {
        // Validate inside the transaction (fresh snapshot): sources must
        // exist at the revisions the merge was planned against. A
        // concurrent update (practice, end-effects, rename) changes the
        // entity revision, so a stale plan is rejected explicitly
        // instead of silently discarding the update (re-review R3).
        for key in source_keys {
            let raw = tx
                .get(&self.guides, key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    format!("guide not found: {key}"),
                ));
            };
            if let Some((_, expected)) = expected_revisions
                .iter()
                .find(|(n, _)| n.to_lowercase() == *key)
            {
                let current: ltmrs_domain::guide::Guide = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if current.entity_revision != *expected {
                    return Err(DomainError::new(
                        DomainErrorCode::RevisionConflict,
                        format!(
                            "guide \"{key}\" changed since merge planning: re-read and re-plan"
                        ),
                    ));
                }
            }
        }
        let result_exists = tx
            .get(&self.guides, result_key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            .is_some();
        if result_exists {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "guide already exists",
            ));
        }
        // Patch affected memories fresh inside the same tx.
        let mut mem_keys: Vec<String> = Vec::new();
        for kv in tx.iter(&self.memories) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let key: String = String::from_utf8_lossy(k.as_ref()).into_owned();
            let mem: Memory = serde_json::from_slice(v.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if source_keys
                .iter()
                .any(|s| mem.related_guides.iter().any(|g| g.eq_ignore_ascii_case(s)))
            {
                mem_keys.push(key);
            }
        }
        for key in &mem_keys {
            let raw = tx
                .get(&self.memories, key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else { continue };
            let mut mem: Memory = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut refs: Vec<String> = mem
                .related_guides
                .iter()
                .filter(|g| !source_keys.iter().any(|s| g.eq_ignore_ascii_case(s)))
                .cloned()
                .collect();
            if !refs.iter().any(|g| g.eq_ignore_ascii_case(&result.name)) {
                refs.push(result.name.to_lowercase());
            }
            mem.related_guides = refs;
            mem.entity_revision = mem.entity_revision.next();
            let raw = serde_json::to_vec(&mem)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.memories, key, raw.as_slice());
        }
        // Delete sources, put result — same commit.
        for key in source_keys {
            tx.remove(&self.guides, key);
        }
        let raw = serde_json::to_vec(result)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.guides, result_key, raw.as_slice());
        Ok(())
    }

    /// Rename a guide atomically (re-review R3): the renamed put, memory
    /// reference moves and old-key delete commit in ONE transaction. A
    /// failure anywhere leaves no half-rename (no dangling references to a
    /// deleted guide, no duplicate guides). The source revision read during
    /// planning is enforced: a concurrent update (practice, end-effects)
    /// rejects the stale rename explicitly instead of discarding it
    /// (re-review P1-2). Reference patching follows the unindexed-field
    /// contract (document revision untouched, entity revision advances).
    pub fn rename_guide_atomically(
        &self,
        old_name: &str,
        expected: ltmrs_domain::id::EntityRevision,
        updated: &ltmrs_domain::guide::Guide,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            self.rename_guide_apply_tx(&mut tx, old_name, expected, updated)?;
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide rename conflicted"))
    }

    /// Rename inside the caller's transaction (tx core shared by the
    /// standalone rename and the idempotent tool wrapper). Returns the guide
    /// as written (revision advanced from the live record).
    fn rename_guide_apply_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        old_name: &str,
        expected: ltmrs_domain::id::EntityRevision,
        updated: &ltmrs_domain::guide::Guide,
    ) -> DomainResult<ltmrs_domain::guide::Guide> {
        let old_key = old_name.to_lowercase();
        let new_key = updated.name.to_lowercase();
        let old_exists = tx
            .get(&self.guides, &old_key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(old_raw) = old_exists else {
            return Err(DomainError::new(
                DomainErrorCode::NotFound,
                format!("guide not found: {old_key}"),
            ));
        };
        // Stale-source guard (re-review P1-2): the rename must apply to
        // the revision it was planned against, not silently overwrite a
        // newer concurrent update.
        let old_guide: ltmrs_domain::guide::Guide = serde_json::from_slice(old_raw.as_ref())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        if old_guide.entity_revision != expected {
            return Err(DomainError::new(
                DomainErrorCode::RevisionConflict,
                "guide changed since rename planning: re-read and re-plan",
            ));
        }
        if new_key != old_key {
            let clash = tx
                .get(&self.guides, &new_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                .is_some();
            if clash {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "guide already exists",
                ));
            }
        }
        // Move memory references fresh inside the same tx.
        let mut mem_keys: Vec<String> = Vec::new();
        for kv in tx.iter(&self.memories) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let key: String = String::from_utf8_lossy(k.as_ref()).into_owned();
            let mem: Memory = serde_json::from_slice(v.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if mem
                .related_guides
                .iter()
                .any(|g| g.eq_ignore_ascii_case(&old_key))
            {
                mem_keys.push(key);
            }
        }
        for key in &mem_keys {
            let raw = tx
                .get(&self.memories, key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else { continue };
            let mut mem: Memory = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut refs: Vec<String> = mem
                .related_guides
                .iter()
                .filter(|g| !g.eq_ignore_ascii_case(&old_key))
                .cloned()
                .collect();
            if !refs.iter().any(|g| g.eq_ignore_ascii_case(&updated.name)) {
                refs.push(updated.name.to_lowercase());
            }
            mem.related_guides = refs;
            mem.entity_revision = mem.entity_revision.next();
            let raw = serde_json::to_vec(&mem)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.memories, key, raw.as_slice());
        }
        let mut renamed = updated.clone();
        // The rename itself is a mutation: advance from the live
        // revision so later plans observe it.
        renamed.entity_revision = old_guide.entity_revision.next();
        let raw = serde_json::to_vec(&renamed)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.guides, &new_key, raw.as_slice());
        if new_key != old_key {
            tx.remove(&self.guides, &old_key);
        }
        Ok(renamed)
    }

    /// Forget a guide atomically (re-review R3): reference removal and the
    /// guide delete commit in ONE transaction. Returns true when removed.
    pub fn forget_guide_atomically(&self, name: &str) -> DomainResult<bool> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let removed = self.forget_guide_apply_tx(&mut tx, name)?.is_some();
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(removed);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide forget conflicted"))
    }

    /// Forget inside the caller's transaction (tx core shared by the
    /// standalone forget and the idempotent tool wrapper). Returns the
    /// deleted snapshot, or None when absent.
    fn forget_guide_apply_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        name: &str,
    ) -> DomainResult<Option<ltmrs_domain::guide::Guide>> {
        let key = name.to_lowercase();
        let raw = tx
            .get(&self.guides, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let deleted: ltmrs_domain::guide::Guide = serde_json::from_slice(raw.as_ref())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut mem_keys: Vec<String> = Vec::new();
        for kv in tx.iter(&self.memories) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mkey: String = String::from_utf8_lossy(k.as_ref()).into_owned();
            let mem: Memory = serde_json::from_slice(v.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if mem
                .related_guides
                .iter()
                .any(|g| g.eq_ignore_ascii_case(&key))
            {
                mem_keys.push(mkey);
            }
        }
        for mkey in &mem_keys {
            let raw = tx
                .get(&self.memories, mkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else { continue };
            let mut mem: Memory = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            mem.related_guides = mem
                .related_guides
                .iter()
                .filter(|g| !g.eq_ignore_ascii_case(&key))
                .cloned()
                .collect();
            mem.entity_revision = mem.entity_revision.next();
            let raw = serde_json::to_vec(&mem)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.memories, mkey, raw.as_slice());
        }
        tx.remove(&self.guides, &key);
        Ok(Some(deleted))
    }

    /// Apply one guide tool mutation idempotently (P1-2 replay safety): the
    /// operation ID is logged atomically with the mutation in ONE
    /// transaction. A retried operation with the same ID + digest returns the
    /// RECORDED outcome (never re-applied, never recomputed from live
    /// state); the same ID with a different digest rejects as key reuse.
    /// Business errors (NotFound, RevisionConflict, Validation) propagate
    /// unlogged, so a retry re-evaluates against current state.
    pub fn guide_mutation_idempotent(
        &self,
        operation_id: &str,
        digest: &str,
        mutation: GuideMutation,
    ) -> DomainResult<RecordedGuideOp> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(recorded) = self.check_guide_op_tx(&tx, operation_id, digest)? {
                // Durability is not inherited from a visible receipt
                // (re-review P1-1): flush again before acknowledging.
                self.persist_barrier()?;
                return Ok(recorded);
            }
            let (kind, guide, merged_sources) = match mutation.clone() {
                GuideMutation::Create { guide } => {
                    let written = self.put_guide_apply_tx(&mut tx, None, &guide)?;
                    (GuideOpKind::Create, Some(written), Vec::new())
                }
                GuideMutation::Update {
                    expected,
                    guide,
                    old_name,
                } => {
                    let written = match old_name {
                        Some(old) => {
                            let rev = expected.ok_or_else(|| {
                                DomainError::new(
                                    DomainErrorCode::Validation,
                                    "rename requires the planned source revision",
                                )
                            })?;
                            self.rename_guide_apply_tx(&mut tx, &old, rev, &guide)?
                        }
                        None => self.put_guide_apply_tx(&mut tx, expected, &guide)?,
                    };
                    (GuideOpKind::Update, Some(written), Vec::new())
                }
                GuideMutation::Forget { name } => {
                    match self.forget_guide_apply_tx(&mut tx, &name)? {
                        Some(deleted) => (GuideOpKind::Forget, Some(deleted), Vec::new()),
                        None => {
                            return Err(DomainError::new(
                                DomainErrorCode::NotFound,
                                format!("guide not found: {}", name.to_lowercase()),
                            ));
                        }
                    }
                }
                GuideMutation::Merge {
                    sources,
                    expected,
                    result,
                } => {
                    let source_keys: Vec<String> =
                        sources.iter().map(|n| n.to_lowercase()).collect();
                    let result_key = result.name.to_lowercase();
                    self.merge_guides_apply_tx(
                        &mut tx,
                        &source_keys,
                        &expected,
                        &result_key,
                        &result,
                    )?;
                    (GuideOpKind::Merge, Some(result), sources)
                }
                GuideMutation::CreateUpdate { expected, guide } => {
                    let written = self.put_guide_apply_tx(&mut tx, expected, &guide)?;
                    (GuideOpKind::CreateUpdate, Some(written), Vec::new())
                }
            };
            let log = GuideOpLog {
                digest: digest.to_string(),
                kind,
                recorded: guide.clone(),
                merged_sources: merged_sources.clone(),
            };
            let raw = serde_json::to_vec(&log)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guide_ops, operation_id, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(RecordedGuideOp {
                        kind,
                        guide,
                        merged_sources,
                    });
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide mutation conflicted"))
    }

    /// Read back one recorded guide tool operation (P1-2): lets the exec
    /// layer replay before planning reads, so a retry never mistakes a
    /// concurrently changed store (renamed/consumed guides) for a failure.
    /// A digest mismatch rejects as key reuse. The durability barrier runs
    /// before acknowledging a replay, matching the in-transaction path.
    pub fn read_recorded_guide_op(
        &self,
        operation_id: &str,
        digest: &str,
    ) -> DomainResult<Option<RecordedGuideOp>> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.guide_ops, operation_id)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        if let Ok(log) = serde_json::from_slice::<GuideOpLog>(raw.as_ref()) {
            if log.digest != digest {
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input",
                ));
            }
            self.persist_barrier()?;
            return Ok(Some(RecordedGuideOp {
                kind: log.kind,
                guide: log.recorded,
                merged_sources: log.merged_sources,
            }));
        }
        if serde_json::from_slice::<PracticeLog>(raw.as_ref()).is_ok() {
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }
        Err(DomainError::new(
            DomainErrorCode::Validation,
            "corrupt guide operation log entry",
        ))
    }

    /// Look up one guide tool operation in the `guide_ops` log inside the
    /// caller's transaction: a digest match returns the recorded outcome, a
    /// digest mismatch rejects as key reuse. Entries written by practice
    /// (different receipt shape, same log) prove the ID is owned by another
    /// operation and reject the same way; corrupt entries fail loudly rather
    /// than risk a double-apply.
    fn check_guide_op_tx(
        &self,
        tx: &OptimisticWriteTx,
        operation_id: &str,
        digest: &str,
    ) -> DomainResult<Option<RecordedGuideOp>> {
        let Some(raw) = tx
            .get(&self.guide_ops, operation_id)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
        else {
            return Ok(None);
        };
        if let Ok(log) = serde_json::from_slice::<GuideOpLog>(raw.as_ref()) {
            if log.digest != digest {
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input",
                ));
            }
            return Ok(Some(RecordedGuideOp {
                kind: log.kind,
                guide: log.recorded,
                merged_sources: log.merged_sources,
            }));
        }
        if serde_json::from_slice::<PracticeLog>(raw.as_ref()).is_ok() {
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }
        Err(DomainError::new(
            DomainErrorCode::Validation,
            "corrupt guide operation log entry",
        ))
    }

    /// Start a traced session as ONE canonical operation (re-review P1-3):
    /// abandon-previous, attempt decay, session insert and operation
    /// receipt commit in a single transaction. A replay returns the
    /// recorded handle instead of abandoning and recreating; a digest
    /// mismatch rejects. The channel binding itself stays in the daemon
    /// registry (routing, not durability).
    #[allow(clippy::too_many_arguments)]
    pub fn session_start_tx(
        &self,
        operation_id: &str,
        digest: &str,
        handle: SessionHandle,
        channel_id: ChannelId,
        project: Option<String>,
        task_type: Option<String>,
        technologies: Vec<String>,
        initial_approach: Option<String>,
        abandon: Option<SessionHandle>,
        now_millis: u64,
    ) -> DomainResult<SessionOp<SessionHandle>> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.session_ops, operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if rec.digest != digest {
                    return Ok(SessionOp::Conflict);
                }
                // Durability is not inherited from a visible receipt
                // (re-review P1-1): flush again before acknowledging.
                self.persist_barrier()?;
                return Ok(SessionOp::Replayed(rec.session));
            }
            // Abandon the channel's previous session, if it can still end.
            if let Some(prev) = abandon {
                let pkey = prev.as_uuid().to_string();
                if let Some(raw) = tx
                    .get(&self.sessions, &pkey)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                {
                    let mut prev_session: Session =
                        serde_json::from_slice(raw.as_ref()).map_err(|e| {
                            DomainError::new(DomainErrorCode::Validation, e.to_string())
                        })?;
                    if prev_session.can_end() {
                        prev_session.status = ltmrs_domain::session::SessionStatus::Abandoned;
                        prev_session.outcome = Some(ltmrs_domain::session::TaskOutcome::Abandoned);
                        prev_session.ended_at =
                            Some(ltmrs_domain::memory::Instant::new(now_millis));
                        let raw = serde_json::to_vec(&prev_session).map_err(|e| {
                            DomainError::new(DomainErrorCode::Validation, e.to_string())
                        })?;
                        tx.insert(&self.sessions, &pkey, raw.as_slice());
                    }
                }
            }
            // Decay stale dead-ends across sessions (same 0.002 policy the
            // registry applied at start).
            let mut all: Vec<(String, Session)> = Vec::new();
            for kv in tx.iter(&self.sessions) {
                let (k, v) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let key: String = String::from_utf8_lossy(k.as_ref()).into_owned();
                let mut s: Session = serde_json::from_slice(v.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                for a in &mut s.attempts {
                    a.confidence = (a.confidence - 0.002).max(0.0);
                }
                all.push((key, s));
            }
            for (key, s) in &all {
                let raw = serde_json::to_vec(s)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                tx.insert(&self.sessions, key, raw.as_slice());
            }
            let session = Session {
                handle,
                channel_id,
                project: project.clone(),
                task_type: task_type.clone(),
                technologies: technologies.clone(),
                status: ltmrs_domain::session::SessionStatus::Active,
                attempts: Vec::new(),
                outcome: None,
                final_approach: None,
                lessons: Vec::new(),
                initial_approach: initial_approach.clone(),
                guides_used: Vec::new(),
                memories_read: Vec::new(),
                memories_created: Vec::new(),
                refinement_attempts: 0,
                self_critique_count: 0,
                started_at: ltmrs_domain::memory::Instant::new(now_millis),
                ended_at: None,
                is_virtual: false,
            };
            let hkey = handle.as_uuid().to_string();
            let raw = serde_json::to_vec(&session)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, &hkey, raw.as_slice());
            let receipt = SessionReceipt {
                digest: digest.to_string(),
                session: handle,
                seq: None,
                response: None,
                continuity_boosted: false,
            };
            let log_raw = serde_json::to_vec(&receipt)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, operation_id, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    self.fire_commit_hook();
                    return Ok(SessionOp::Applied(handle));
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("session start conflicted"))
    }

    /// Record a session attempt as ONE canonical operation (re-review P1-3):
    /// the attempt ID derives deterministically from the operation ID, so
    /// retries dedup; counters increment exactly once per operation; the
    /// receipt commits in the same transaction. Digest mismatch rejects.
    #[allow(clippy::too_many_arguments)]
    pub fn session_attempt_tx(
        &self,
        operation_id: &str,
        digest: &str,
        handle: SessionHandle,
        approach: String,
        outcome: ltmrs_domain::session::AttemptOutcome,
        critique: Option<String>,
        rationale: Option<String>,
        related_memory_id: Option<EntityId>,
        now_millis: u64,
    ) -> DomainResult<SessionOp<(SessionHandle, u32)>> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let attempt_id = EntityId::new(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("ltmrs:attempt:{operation_id}").as_bytes(),
        ));
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.session_ops, operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if rec.digest != digest {
                    return Ok(SessionOp::Conflict);
                }
                self.persist_barrier()?;
                let seq = rec.seq.unwrap_or(0);
                return Ok(SessionOp::Replayed((rec.session, seq)));
            }
            let hkey = handle.as_uuid().to_string();
            let raw = tx
                .get(&self.sessions, &hkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "session not found",
                ));
            };
            let mut session: Session = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if !session.can_end() {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "session is already terminal",
                ));
            }
            let seq = match session.attempts.iter().find(|a| a.id == attempt_id) {
                Some(existing) => existing.seq,
                None => {
                    let next = session.attempts.len() as u32 + 1;
                    session.attempts.push(ltmrs_domain::session::Attempt {
                        id: attempt_id,
                        session_id: handle,
                        seq: next,
                        approach: approach.clone(),
                        outcome,
                        critique: critique.clone(),
                        rationale: rationale.clone(),
                        related_memory_id,
                        confidence: 1.0,
                        access_count: 0,
                        last_accessed_at: None,
                        created_at: ltmrs_domain::memory::Instant::new(now_millis),
                    });
                    session.refinement_attempts += 1;
                    if matches!(
                        outcome,
                        ltmrs_domain::session::AttemptOutcome::Rejected
                            | ltmrs_domain::session::AttemptOutcome::Partial
                    ) && critique.is_some()
                    {
                        session.self_critique_count += 1;
                    }
                    next
                }
            };
            let raw = serde_json::to_vec(&session)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, &hkey, raw.as_slice());
            let receipt = SessionReceipt {
                digest: digest.to_string(),
                session: handle,
                seq: Some(seq),
                response: None,
                continuity_boosted: false,
            };
            let log_raw = serde_json::to_vec(&receipt)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, operation_id, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    self.fire_commit_hook();
                    return Ok(SessionOp::Applied((handle, seq)));
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("session attempt conflicted"))
    }

    /// Success-rate warning line for a guide after an outcome bump, or
    /// empty when the guide is healthy. Shared by the fresh end path and
    /// the replay path so both render identically.
    fn improvement_line(guide: &ltmrs_domain::guide::Guide) -> String {
        let total = guide.success_count + guide.failure_count;
        if total >= 3 {
            let rate = guide.success_count as f64 / total as f64;
            if rate < 0.4 {
                return format!(
                    "  [!] Guide \"{}\" success rate is {:.2} ({}/{total}). Consider refining with guide_update.",
                    guide.name, rate, guide.success_count
                );
            }
        }
        String::new()
    }

    /// Recompute improvement lines for a session's used guides from current
    /// store state (replay rendering): same rule as the fresh path.
    fn improvement_lines_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        handle: SessionHandle,
    ) -> DomainResult<Vec<String>> {
        let hkey = handle.as_uuid().to_string();
        let raw = tx
            .get(&self.sessions, &hkey)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };
        let session: Session = serde_json::from_slice(raw.as_ref())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut lines = Vec::new();
        for guide_name in &session.guides_used {
            let gkey = guide_name.to_lowercase();
            if let Some(graw) = tx
                .get(&self.guides, &gkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let guide: ltmrs_domain::guide::Guide = serde_json::from_slice(graw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let line = Self::improvement_line(&guide);
                if !line.is_empty() {
                    lines.push(line);
                }
            }
        }
        Ok(lines)
    }

    /// End a session as ONE canonical operation (re-review P1-3): required
    /// guide outcomes, the terminal transition and the operation receipt
    /// commit in a single transaction — no observable partial completion,
    /// no mixed outcomes. Replay returns the recorded handle; digest
    /// mismatch rejects. Improvement lines are derived from the committed
    /// counts and returned for the response (suggestion filing itself stays
    /// best-effort, content-deduplicated).
    #[allow(clippy::too_many_arguments)]
    pub fn session_end_tx(
        &self,
        operation_id: &str,
        digest: &str,
        handle: SessionHandle,
        outcome: ltmrs_domain::session::TaskOutcome,
        final_approach: Option<String>,
        lessons: Vec<String>,
        now_millis: u64,
    ) -> DomainResult<SessionOp<(SessionHandle, Vec<String>, bool)>> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.session_ops, operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if rec.digest != digest {
                    return Ok(SessionOp::Conflict);
                }
                // Rebuild the improvement lines from current guide state so
                // the replayed response matches a fresh rendering: same
                // inputs, same text (rates only change via later ops, in
                // which case current truth is the right rendering).
                let lines = self.improvement_lines_tx(&mut tx, rec.session)?;
                tx.rollback();
                // Flush again before ack (re-review P1-1): the receipt may
                // predate an unflushed barrier.
                self.persist_barrier()?;
                return Ok(SessionOp::Replayed((rec.session, lines, true)));
            }
            let hkey = handle.as_uuid().to_string();
            let raw = tx
                .get(&self.sessions, &hkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "session not found",
                ));
            };
            let mut session: Session = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if !session.can_end() {
                // Already terminal via another operation: no mutation, no
                // receipt — the caller reports "no active session". A
                // retry deterministically reports the same (nothing was
                // done, so there is nothing to make idempotent).
                tx.rollback();
                return Ok(SessionOp::Applied((handle, Vec::new(), false)));
            }
            // Required guide outcomes inside the SAME transaction.
            let mut improvement_lines: Vec<String> = Vec::new();
            if outcome == ltmrs_domain::session::TaskOutcome::Success
                || outcome == ltmrs_domain::session::TaskOutcome::Failure
            {
                for guide_name in session.guides_used.clone() {
                    let gkey = guide_name.to_lowercase();
                    let graw = tx.get(&self.guides, &gkey).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    let Some(graw) = graw else { continue };
                    let mut guide: ltmrs_domain::guide::Guide =
                        serde_json::from_slice(graw.as_ref()).map_err(|e| {
                            DomainError::new(DomainErrorCode::Validation, e.to_string())
                        })?;
                    if outcome == ltmrs_domain::session::TaskOutcome::Success {
                        guide.success_count += 1;
                    } else {
                        guide.failure_count += 1;
                    }
                    guide.entity_revision = guide.entity_revision.next();
                    guide.updated_at = ltmrs_domain::memory::Instant::new(now_millis);
                    if outcome == ltmrs_domain::session::TaskOutcome::Failure {
                        improvement_lines.push(Self::improvement_line(&guide));
                    }
                    let graw = serde_json::to_vec(&guide).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    tx.insert(&self.guides, &gkey, graw.as_slice());
                }
                improvement_lines.retain(|l| !l.is_empty());
            }
            session.status = ltmrs_domain::session::SessionStatus::Ended;
            session.outcome = Some(outcome);
            session.final_approach = final_approach.clone();
            session.lessons = lessons.clone();
            session.ended_at = Some(ltmrs_domain::memory::Instant::new(now_millis));
            let raw = serde_json::to_vec(&session)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, &hkey, raw.as_slice());
            // Improvement suggestions file in the SAME transaction: the
            // terminal transition, guide outcomes, suggestions and receipt
            // are one consistency boundary, so duplicate deliveries of
            // this operation can never file twice.
            self.file_suggestions_tx(&mut tx, handle, &improvement_lines, now_millis)?;
            let receipt = SessionReceipt {
                digest: digest.to_string(),
                session: handle,
                seq: None,
                response: None,
                continuity_boosted: false,
            };
            let log_raw = serde_json::to_vec(&receipt)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, operation_id, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    self.fire_commit_hook();
                    return Ok(SessionOp::Applied((handle, improvement_lines, true)));
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("session end conflicted"))
    }

    /// Track session links (guides used, memories read/created) with
    /// order-preserving deduplication, committed atomically.
    pub fn track_session_link(
        &self,
        handle: SessionHandle,
        field: SessionLinkField,
        ids: &[String],
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let hkey = handle.as_uuid().to_string();
            let raw = tx
                .get(&self.sessions, &hkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Ok(());
            };
            let mut session: Session = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let target = match field {
                SessionLinkField::GuideUsed => &mut session.guides_used,
                SessionLinkField::MemoryRead => &mut session.memories_read,
                SessionLinkField::MemoryCreated => &mut session.memories_created,
            };
            if field == SessionLinkField::GuideUsed {
                for id in ids {
                    let lower = id.to_lowercase();
                    if !target.contains(&lower) {
                        target.push(lower);
                    }
                }
            } else {
                for id in ids {
                    if !target.contains(id) {
                        target.push(id.clone());
                    }
                }
            }
            let raw = serde_json::to_vec(&session)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, &hkey, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("session link conflicted"))
    }

    /// Adjust one attempt's confidence (suggestion feedback): boost capped
    /// at 1.0, penalty floored at 0.0; access counters increment. No-op
    /// when the session or attempt is absent.
    pub fn adjust_attempt(
        &self,
        handle: SessionHandle,
        seq: u32,
        delta: f64,
        now_millis: u64,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            self.adjust_attempt_tx(&mut tx, handle, seq, delta, now_millis)?;
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("attempt adjust conflicted"))
    }

    /// Attempt confidence adjustment inside the caller's transaction (tx
    /// core shared by the standalone adjust and the receipt-claimed
    /// continuity boost). Missing sessions/attempts adjust nothing and
    /// still succeed, matching the old leniency. Returns whether an
    /// attempt was adjusted.
    fn adjust_attempt_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        handle: SessionHandle,
        seq: u32,
        delta: f64,
        now_millis: u64,
    ) -> DomainResult<bool> {
        let hkey = handle.as_uuid().to_string();
        let raw = tx
            .get(&self.sessions, &hkey)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let mut session: Session = serde_json::from_slice(raw.as_ref())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut adjusted = false;
        if let Some(a) = session.attempts.iter_mut().find(|a| a.seq == seq) {
            a.confidence = (a.confidence + delta).clamp(0.0, 1.0);
            a.access_count += 1;
            a.last_accessed_at = Some(ltmrs_domain::memory::Instant::new(now_millis));
            adjusted = true;
        }
        let raw = serde_json::to_vec(&session)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.sessions, &hkey, raw.as_slice());
        Ok(adjusted)
    }

    /// Apply continuity-recall attempt boosts exactly once per session-start
    /// operation (P2-B): the boost targets plus the claimed flag commit in
    /// ONE transaction. Returns true when this call applied the boosts,
    /// false when a previous call already claimed them (crash-window
    /// continuation must not double-boost). Digest mismatch rejects.
    pub fn claim_continuity_boost(
        &self,
        operation_id: &str,
        digest: &str,
        targets: &[(SessionHandle, u32)],
        delta: f64,
        now_millis: u64,
    ) -> DomainResult<bool> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let raw = tx
                .get(&self.session_ops, operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "session operation receipt not found",
                ));
            };
            let mut rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if rec.digest != digest {
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input",
                ));
            }
            if rec.continuity_boosted {
                self.persist_barrier()?;
                return Ok(false);
            }
            for (handle, seq) in targets {
                self.adjust_attempt_tx(&mut tx, *handle, *seq, delta, now_millis)?;
            }
            rec.continuity_boosted = true;
            let raw = serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, operation_id, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(true);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "continuity boost claim conflicted",
        ))
    }

    /// Read one session by handle.
    pub fn get_session(&self, handle: SessionHandle) -> DomainResult<Option<Session>> {
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.sessions, handle.as_uuid().to_string())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|raw| {
            serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
        })
        .transpose()
    }

    /// All traced sessions (analytics, stats, continuity recall, backup).
    pub fn all_sessions(&self) -> DomainResult<Vec<Session>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.sessions) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(
                serde_json::from_slice(v.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
            );
        }
        Ok(out)
    }

    /// Read one session operation receipt by ID (P2-1 frozen replay).
    pub fn session_receipt(&self, operation_id: &str) -> DomainResult<Option<SessionReceipt>> {
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.session_ops, operation_id)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|raw| {
            serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
        })
        .transpose()
    }

    /// Freeze the tool response into a session operation receipt (P2-1):
    /// stored after first execution so a lost-response retry returns the
    /// original verbatim. The digest must still match (else key reuse); a
    /// receipt lost to a concurrent restore errors loudly instead of
    /// resurrecting a stale identity.
    pub fn store_session_response(
        &self,
        operation_id: &str,
        digest: &str,
        response: &ltmrs_domain::session::FrozenToolResponse,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let raw = tx
                .get(&self.session_ops, operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "session operation receipt not found",
                ));
            };
            let mut rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if rec.digest != digest {
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input",
                ));
            }
            // First freeze wins: duplicate in-flight executions that both
            // passed the unfrozen check must converge on one response, or
            // two callers of the same operation id receive divergent
            // "verbatim" replays.
            if rec.response.is_some() {
                self.persist_barrier()?;
                return Ok(());
            }
            rec.response = Some(response.clone());
            let raw = serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, operation_id, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "session response freeze conflicted",
        ))
    }

    /// One-time import of pre-migration registry state (traced sessions +
    /// operation receipts from sessions.json): inserts only absent records,
    /// so repeated starts never duplicate. Virtual sessions and bindings
    /// stay in the registry file.
    pub fn import_legacy_sessions(
        &self,
        sessions: Vec<Session>,
        receipts: Vec<(String, SessionReceipt)>,
    ) -> DomainResult<usize> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        let mut imported = 0usize;
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut dirty = false;
            for s in &sessions {
                let key = s.handle.as_uuid().to_string();
                let exists = tx
                    .get(&self.sessions, &key)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    .is_some();
                if !exists {
                    let raw = serde_json::to_vec(s).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    tx.insert(&self.sessions, &key, raw.as_slice());
                    imported += 1;
                    dirty = true;
                }
            }
            for (op_id, rec) in &receipts {
                let exists = tx
                    .get(&self.session_ops, op_id)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    .is_some();
                if !exists {
                    let raw = serde_json::to_vec(rec).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    tx.insert(&self.session_ops, op_id, raw.as_slice());
                    dirty = true;
                }
            }
            if !dirty {
                return Ok(imported);
            }
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(imported);
                }
                Ok(Err(_)) => {
                    imported = 0;
                    continue;
                }
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "legacy session import conflicted",
        ))
    }

    /// All suggestions from a single snapshot.
    pub fn get_suggestions(&self) -> DomainResult<Vec<ltmrs_domain::session::Suggestion>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.suggestions) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<ltmrs_domain::session::Suggestion>(v.as_ref())?);
        }
        Ok(out)
    }

    /// A single suggestion by ID.
    pub fn get_suggestion(
        &self,
        id: u64,
    ) -> DomainResult<Option<ltmrs_domain::session::Suggestion>> {
        Ok(self.get_suggestions()?.into_iter().find(|s| s.id == id))
    }

    /// Store a suggestion (keyed by ID).
    pub fn put_suggestion(
        &self,
        suggestion: &ltmrs_domain::session::Suggestion,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let key = suggestion.id.to_string();
        let raw = serde_json::to_vec(suggestion)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, &key, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("suggestion write conflicted"))
    }

    /// Respond to a suggestion idempotently (P1-2 replay safety): the status
    /// transition and all attempt confidence adjustments commit atomically
    /// with the operation receipt in ONE transaction. A retried respond with
    /// the same ID + digest returns the RECORDED outcome (no second
    /// adjustment); the same ID with a different digest rejects as key
    /// reuse. A missing suggestion errors unlogged, so a retry re-evaluates.
    pub fn respond_suggestion_idempotent(
        &self,
        operation_id: &str,
        digest: &str,
        suggestion_id: u64,
        status: ltmrs_domain::session::SuggestionStatus,
        now_millis: u64,
    ) -> DomainResult<RecordedSuggestionOp> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.suggestion_ops, operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let log: SuggestionOpLog = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if log.digest != digest {
                    return Err(DomainError::new(
                        DomainErrorCode::KeyReuseDifferentInput,
                        "operation key reused with different input",
                    ));
                }
                // Flush again before ack (re-review P1-1): a visible receipt
                // is not proof its flush succeeded.
                self.persist_barrier()?;
                return Ok(RecordedSuggestionOp {
                    suggestion_id: log.suggestion_id,
                    status: log.status,
                    adjusted: log.adjusted,
                });
            }
            let skey = suggestion_id.to_string();
            let raw = tx
                .get(&self.suggestions, &skey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "Could not update this suggestion in the store.",
                ));
            };
            let mut suggestion: ltmrs_domain::session::Suggestion =
                serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            suggestion.status = status;
            suggestion.resolved_at = Some(ltmrs_domain::memory::Instant::new(now_millis));
            let raw = serde_json::to_vec(&suggestion)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, &skey, raw.as_slice());
            // Attempt confidence adjustments in the same tx (previously one
            // tx per attempt, best-effort). Missing sessions/attempts adjust
            // nothing and still succeed, matching the old leniency.
            let mut adjusted = 0u32;
            if let Some(handle) = suggestion
                .session_id
                .as_deref()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .map(ltmrs_domain::id::SessionHandle::new)
            {
                let hkey = handle.as_uuid().to_string();
                if let Some(sraw) = tx
                    .get(&self.sessions, &hkey)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                {
                    let mut session: Session =
                        serde_json::from_slice(sraw.as_ref()).map_err(|e| {
                            DomainError::new(DomainErrorCode::Validation, e.to_string())
                        })?;
                    for a in &mut session.attempts {
                        let applies = match status {
                            ltmrs_domain::session::SuggestionStatus::Dismissed => matches!(
                                a.outcome,
                                ltmrs_domain::session::AttemptOutcome::Rejected
                                    | ltmrs_domain::session::AttemptOutcome::Partial
                            ),
                            ltmrs_domain::session::SuggestionStatus::Accepted => {
                                a.outcome == ltmrs_domain::session::AttemptOutcome::Promising
                            }
                            ltmrs_domain::session::SuggestionStatus::Pending => false,
                        };
                        if applies {
                            let delta =
                                if status == ltmrs_domain::session::SuggestionStatus::Dismissed {
                                    -0.02
                                } else {
                                    0.02
                                };
                            a.confidence = (a.confidence + delta).clamp(0.0, 1.0);
                            a.access_count += 1;
                            a.last_accessed_at =
                                Some(ltmrs_domain::memory::Instant::new(now_millis));
                            adjusted += 1;
                        }
                    }
                    let sraw = serde_json::to_vec(&session).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    tx.insert(&self.sessions, &hkey, sraw.as_slice());
                }
            }
            let log = SuggestionOpLog {
                digest: digest.to_string(),
                suggestion_id,
                status,
                adjusted,
            };
            let raw = serde_json::to_vec(&log)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestion_ops, operation_id, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(RecordedSuggestionOp {
                        suggestion_id,
                        status,
                        adjusted,
                    });
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("suggestion respond conflicted"))
    }

    /// File a suggestion with an atomically allocated ID (lost-write fix):
    /// the max-ID scan and the insert commit in ONE transaction, so two
    /// concurrent filers cannot claim the same ID and silently overwrite
    /// each other (optimistic conflict retries recompute the max).
    /// File improvement suggestions inside the caller's transaction:
    /// content-deduplicated per session+text, IDs allocated max+1 in-tx.
    /// Used by `session_end_tx` so the terminal transition, guide outcomes,
    /// suggestions and receipt commit as one boundary — duplicate
    /// deliveries of the same end operation can never file twice.
    fn file_suggestions_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        session: SessionHandle,
        lines: &[String],
        now_millis: u64,
    ) -> DomainResult<()> {
        let session_key = session.as_uuid().to_string();
        let mut max: u64 = 0;
        let mut present = std::collections::HashSet::new();
        for kv in tx.iter(&self.suggestions) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Ok(id) = String::from_utf8_lossy(k.as_ref()).parse::<u64>() {
                max = max.max(id);
            }
            if let Ok(s) = serde_json::from_slice::<ltmrs_domain::session::Suggestion>(v.as_ref()) {
                present.insert((s.session_id, s.suggestion));
            }
        }
        for line in lines {
            let text = line.trim().to_string();
            if text.is_empty() || present.contains(&(Some(session_key.clone()), text.clone())) {
                continue;
            }
            max += 1;
            let suggestion = ltmrs_domain::session::Suggestion {
                id: max,
                session_id: Some(session_key.clone()),
                suggestion: text.clone(),
                status: ltmrs_domain::session::SuggestionStatus::Pending,
                created_at: ltmrs_domain::memory::Instant::new(now_millis),
                resolved_at: None,
            };
            let raw = serde_json::to_vec(&suggestion)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, suggestion.id.to_string(), raw.as_slice());
            present.insert((Some(session_key.clone()), text));
        }
        Ok(())
    }

    pub fn file_suggestion(
        &self,
        session_id: Option<String>,
        text: String,
        now_millis: u64,
    ) -> DomainResult<ltmrs_domain::session::Suggestion> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key(None);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut max: u64 = 0;
            for kv in tx.iter(&self.suggestions) {
                let (k, _) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if let Ok(id) = String::from_utf8_lossy(k.as_ref()).parse::<u64>() {
                    max = max.max(id);
                }
            }
            let suggestion = ltmrs_domain::session::Suggestion {
                id: max + 1,
                session_id: session_id.clone(),
                suggestion: text.clone(),
                status: ltmrs_domain::session::SuggestionStatus::Pending,
                created_at: ltmrs_domain::memory::Instant::new(now_millis),
                resolved_at: None,
            };
            let raw = serde_json::to_vec(&suggestion)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, suggestion.id.to_string(), raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(suggestion);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("suggestion filing conflicted"))
    }

    /// Export traversal from a single snapshot.
    pub fn export_snapshot(&self) -> DomainResult<CanonicalExport> {
        let snapshot = self.db.read_tx();

        let mut memories = Vec::new();
        for kv in snapshot.iter(&self.memories) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            memories.push(decode::<Memory>(v.as_ref())?);
        }

        let mut relations = Vec::new();
        for kv in snapshot.iter(&self.relations) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            relations.push(decode::<Relation>(v.as_ref())?);
        }

        Ok(CanonicalExport {
            memories,
            relations,
            ..Default::default()
        })
    }

    /// Full domain export for native backup (WP-11): every stored collection
    /// in one read transaction (memories, relations, guides, sessions,
    /// feedback, suggestions). Projects/archives/history have no storage yet
    /// and stay empty by design (documented, counted as zero — never
    /// silently dropped).
    pub fn export_full(&self) -> DomainResult<CanonicalExport> {
        Ok(self.export_full_with_generation()?.0)
    }

    /// Coherent cut: the domain export plus the live generation from ONE
    /// read transaction. A concurrent generation flip between two snapshots
    /// would otherwise mislabel data (backup manifest torn from content).
    /// Sessions ride the same snapshot (P1 follow-up): since b8bcb94 they
    /// are canonical Fjall records, and a caller-side `all_sessions()` from
    /// a second snapshot could tear across a concurrent `session_end`
    /// (an Active session paired with already-bumped guide counts that
    /// never coexisted).
    pub fn export_full_with_generation(&self) -> DomainResult<(CanonicalExport, StoreGeneration)> {
        let snapshot = self.db.read_tx();
        let read_all = |ks: &OptimisticTxKeyspace| -> DomainResult<Vec<Vec<u8>>> {
            let mut out = Vec::new();
            for kv in snapshot.iter(ks) {
                let (_k, v) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                out.push(v.as_ref().to_vec());
            }
            Ok(out)
        };
        let memories: Vec<Memory> = read_all(&self.memories)?
            .iter()
            .map(|v| decode::<Memory>(v))
            .collect::<DomainResult<_>>()?;
        let relations: Vec<Relation> = read_all(&self.relations)?
            .iter()
            .map(|v| decode::<Relation>(v))
            .collect::<DomainResult<_>>()?;
        let guides: Vec<ltmrs_domain::guide::Guide> = read_all(&self.guides)?
            .iter()
            .map(|v| decode::<ltmrs_domain::guide::Guide>(v))
            .collect::<DomainResult<_>>()?;
        let feedback: Vec<ltmrs_domain::session::FeedbackEvent> = read_all(&self.feedback_events)?
            .iter()
            .map(|v| decode::<ltmrs_domain::session::FeedbackEvent>(v))
            .collect::<DomainResult<_>>()?;
        let suggestions: Vec<ltmrs_domain::session::Suggestion> = read_all(&self.suggestions)?
            .iter()
            .map(|v| decode::<ltmrs_domain::session::Suggestion>(v))
            .collect::<DomainResult<_>>()?;
        let sessions: Vec<Session> = read_all(&self.sessions)?
            .iter()
            .map(|v| decode::<Session>(v))
            .collect::<DomainResult<_>>()?;
        let generation = Self::generation_from_snapshot(&snapshot, &self.generations, &self.db)?;
        Ok((
            CanonicalExport {
                memories,
                relations,
                guides,
                sessions,
                feedback,
                suggestions,
                ..Default::default()
            },
            generation,
        ))
    }

    /// Generation preference (Active record, else meta pointer, else FIRST)
    /// resolved inside the caller's snapshot so export and generation share
    /// one coherent cut.
    fn generation_from_snapshot(
        snapshot: &fjall::Snapshot,
        generations: &OptimisticTxKeyspace,
        db: &OptimisticTxDatabase,
    ) -> DomainResult<StoreGeneration> {
        let mut active: Option<StoreGeneration> = None;
        for kv in snapshot.iter(generations) {
            let (_, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let rec = decode_generation(v.as_ref())?;
            if matches!(rec.status, GenerationStatus::Active) {
                active = Some(match active {
                    Some(a) => a.max(rec.generation),
                    None => rec.generation,
                });
            }
        }
        if let Some(generation) = active {
            return Ok(generation);
        }
        let meta = Self::keyspace(db, "meta")?;
        let raw = snapshot
            .get(&meta, "store_generation")
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        match raw {
            Some(v) => {
                let bytes = v.as_ref();
                if bytes.len() >= 8 {
                    let value = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
                    Ok(StoreGeneration::new(value))
                } else {
                    Ok(StoreGeneration::FIRST)
                }
            }
            None => Ok(StoreGeneration::FIRST),
        }
    }

    /// Atomically replace the store with a verified snapshot (restore): drain
    /// every durable keyspace and insert the snapshot in ONE write
    /// transaction with a durable commit, including the generation flip.
    ///
    /// Coverage: the five exported collections are replaced from the
    /// snapshot; aliases are rebuilt from restored memories (a stale alias
    /// must not block reuse or resolve to a deleted id); receipts and
    /// namespaces are drained (single-generation operational state — the
    /// caller abandons live sessions, so no live operation may replay);
    /// projection jobs are re-enqueued for every restored memory (or search
    /// never converges on the restored state); every generation record
    /// retires and exactly one Active for the new generation is inserted
    /// alongside the meta pointer (the Active==pointer invariant holds).
    ///
    /// Concurrency: the exclusive restore barrier is held across the whole
    /// call and drain enumeration runs inside the same transaction, so no
    /// writer can interleave between drain and commit. A concurrent mutation
    /// blocks, then either precedes (drained) or follows (post-restore
    /// write) — never tears. A commit conflict retries from a fresh preview
    /// at the exec layer. Sessions live in the daemon registry, not here.
    ///
    /// Feedback keys reuse the canonical `feedback:{op}` scheme (the op id
    /// is recovered as event.id XOR 0xF0), so replays cannot double-record
    /// under a divergent key.
    ///
    /// Returns the number of restored sessions that were non-terminal and
    /// marked Abandoned (P2-A): a backup is persistent knowledge, not a
    /// live lease, so Active sessions must not resurrect as unowned live
    /// execution contexts. Terminal sessions restore verbatim.
    #[allow(clippy::too_many_arguments)]
    pub fn restore_replace(
        &self,
        memories: &[Memory],
        relations: &[Relation],
        guides: &[ltmrs_domain::guide::Guide],
        feedback: &[ltmrs_domain::session::FeedbackEvent],
        suggestions: &[ltmrs_domain::session::Suggestion],
        sessions: &[ltmrs_domain::session::Session],
        new_generation: StoreGeneration,
    ) -> DomainResult<u64> {
        let _restore_guard = self.restore_lock.write().unwrap();
        fn drain(tx: &OptimisticWriteTx, ks: &OptimisticTxKeyspace) -> DomainResult<Vec<String>> {
            let mut keys = Vec::new();
            for kv in tx.iter(ks) {
                let (k, _) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                keys.push(String::from_utf8_lossy(k.as_ref()).into_owned());
            }
            Ok(keys)
        }
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            .durability(Some(PersistMode::SyncAll));
        for ks in [
            &self.memories,
            &self.relations,
            &self.guides,
            &self.feedback_events,
            &self.suggestions,
            &self.aliases,
            &self.receipts,
            &self.namespaces,
            &self.projections,
            // Canonical sessions + all three op-receipt logs (P1-1/P1-2):
            // traced sessions are Fjall data since b8bcb94, and session_ops /
            // guide_ops / suggestion_ops are keyed by bare operation ID.
            // Leaving any behind would preserve pre-restore state and let
            // stale receipts replay across the generation cut.
            &self.sessions,
            &self.session_ops,
            &self.guide_ops,
            &self.suggestion_ops,
        ] {
            for key in drain(&tx, ks)? {
                tx.remove(ks, key);
            }
        }
        let now = self.clock.now_millis();
        for m in memories {
            let raw = serde_json::to_vec(m)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.memories, m.id.as_uuid().to_string(), raw.as_slice());
            if let Some(alias) = &m.external_alias {
                tx.insert(
                    &self.aliases,
                    alias.as_str(),
                    m.id.as_uuid().to_string().as_bytes(),
                );
            }
            let job = ltmrs_domain::projection::ProjectionJob {
                memory_id: m.id,
                desired_document_revision: m.document_revision,
                seq: 1,
                enqueued_at_millis: now,
                is_tombstone: false,
            };
            let raw = serde_json::to_vec(&job)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(
                &self.projections,
                m.id.as_uuid().to_string(),
                raw.as_slice(),
            );
        }
        for r in relations {
            let raw = serde_json::to_vec(r)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.relations, r.id.as_uuid().to_string(), raw.as_slice());
        }
        for g in guides {
            let raw = serde_json::to_vec(g)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, g.name.to_lowercase(), raw.as_slice());
        }
        for f in feedback {
            let raw = serde_json::to_vec(f)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let event_id = f.id.as_uuid();
            let event_bytes = event_id.as_bytes();
            let mut op_bytes = [0u8; 16];
            for (i, b) in event_bytes.iter().enumerate() {
                op_bytes[i] = b ^ 0xF0;
            }
            let op_id = uuid::Uuid::from_bytes(op_bytes).to_string();
            tx.insert(
                &self.feedback_events,
                format!("feedback:{op_id}"),
                raw.as_slice(),
            );
        }
        for s in suggestions {
            let raw = serde_json::to_vec(s)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, s.id.to_string(), raw.as_slice());
        }
        // True backup restore (P1-1): session history is knowledge used by
        // continuity recall and analytics, so the backup's sessions become
        // the live set. Non-terminal sessions restore as Abandoned (P2-A):
        // a backup is persistent knowledge, not a live lease. Channel
        // bindings/leases and virtual live sessions stay registry-side and
        // are never restored here.
        let mut sessions_marked_abandoned = 0u64;
        for s in sessions {
            let mut s = s.clone();
            if !s.status.is_terminal() {
                s.status = ltmrs_domain::session::SessionStatus::Abandoned;
                s.outcome = Some(ltmrs_domain::session::TaskOutcome::Abandoned);
                s.ended_at = Some(ltmrs_domain::memory::Instant::new(now));
                sessions_marked_abandoned += 1;
            }
            let raw = serde_json::to_vec(&s)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(
                &self.sessions,
                s.handle.as_uuid().to_string(),
                raw.as_slice(),
            );
        }
        for mut rec in self.read_generation_records(&tx)? {
            if !matches!(rec.status, GenerationStatus::Retired) {
                rec.status = GenerationStatus::Retired;
                rec.updated_at_millis = now;
                let raw = serde_json::to_vec(&rec)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                tx.insert(
                    &self.generations,
                    generation_key(rec.generation),
                    raw.as_slice(),
                );
            }
        }
        let active = GenerationRecord {
            generation: new_generation,
            model_fingerprint: None,
            status: GenerationStatus::Active,
            desired_memories: memories.len() as u64,
            projected_memories: 0,
            updated_at_millis: now,
            build_dirty: true,
        };
        let raw = serde_json::to_vec(&active)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(
            &self.generations,
            generation_key(new_generation),
            raw.as_slice(),
        );
        let meta = Self::keyspace(&self.db, "meta")?;
        tx.insert(
            &meta,
            "store_generation",
            new_generation.as_u64().to_le_bytes(),
        );
        match tx.commit() {
            Ok(Ok(())) => {
                // Same barrier discipline as every other mutation-ACK path:
                // the SyncAll commit above is durable, but without this the
                // barrier fault hook cannot fire here and failures stay
                // untestable. One redundant fsync on a rare op.
                self.persist_barrier()?;
                Ok(sessions_marked_abandoned)
            }
            Ok(Err(_)) => Err(Self::exhausted_contention(
                "restore replace conflicted with a concurrent write",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }
}

fn generation_key(generation: StoreGeneration) -> String {
    generation.as_u64().to_string()
}

fn decode_generation(bytes: &[u8]) -> DomainResult<GenerationRecord> {
    serde_json::from_slice(bytes)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

fn receipt_key(
    generation: StoreGeneration,
    frontend_id: FrontendId,
    retry_epoch: u64,
    op: OperationId,
) -> String {
    format!(
        "{}:{}:{}:{}",
        generation.as_u64(),
        frontend_id.as_uuid(),
        retry_epoch,
        op.as_uuid()
    )
}

fn namespace_key(frontend_id: FrontendId) -> String {
    format!("epoch:{}", frontend_id.as_uuid())
}

/// Mutation-watermark key for a writer: per-frontend for commands (no
/// cross-frontend contention), shared for the direct-write primitives.
fn op_seq_key(frontend: Option<FrontendId>) -> String {
    match frontend {
        Some(fe) => format!("op_seq:{}", fe.as_uuid()),
        None => "op_seq:direct".to_string(),
    }
}

fn ns_key(ns: &RetryNamespace) -> String {
    format!("ns:{}:{}", ns.frontend_id.as_uuid(), ns.retry_epoch)
}

/// Whether a receipt key belongs to the given namespace (frontend + epoch).
/// Receipt keys are `generation:frontend_id:retry_epoch:operation_id`.
/// Scoping to the frontend prevents GC of one frontend's expired namespace
/// from deleting another frontend's receipts that share the same epoch.
fn receipt_matches_namespace(key: &str, ns: &RetryNamespace) -> bool {
    let parts: Vec<&str> = key.split(':').collect();
    parts.len() == 4
        && parts[1] == ns.frontend_id.as_uuid().to_string()
        && parts[2]
            .parse::<u64>()
            .map(|e| e == ns.retry_epoch)
            .unwrap_or(false)
}

fn encode_receipt(receipt: &CommandReceipt) -> DomainResult<Vec<u8>> {
    let record = ReceiptRecord {
        store_generation: receipt.store_generation.as_u64(),
        retry_epoch: receipt.retry_epoch,
        operation_id: receipt.operation_id.as_uuid().to_string(),
        frontend_id: receipt.frontend_id.as_uuid().to_string(),
        channel_id: receipt.channel_id.as_uuid().to_string(),
        request_digest: receipt.request_digest.clone(),
        outcome: match &receipt.outcome {
            ReceiptOutcome::Success { affected } => OutcomeRecord::Success {
                affected: affected.iter().map(|id| id.as_uuid().to_string()).collect(),
            },
            ReceiptOutcome::Rejected { code } => OutcomeRecord::Rejected {
                code: code.as_str().to_string(),
            },
        },
    };
    serde_json::to_vec(&record)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

fn decode_receipt(bytes: &[u8]) -> DomainResult<CommandReceipt> {
    let record: ReceiptRecord = serde_json::from_slice(bytes)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
    let outcome = match record.outcome {
        OutcomeRecord::Success { affected } => {
            let ids: Vec<EntityId> = affected
                .iter()
                .filter_map(|s| uuid::Uuid::parse_str(s).ok().map(EntityId::new))
                .collect();
            ReceiptOutcome::Success { affected: ids }
        }
        OutcomeRecord::Rejected { code } => ReceiptOutcome::Rejected {
            code: DomainErrorCode::parse(&code),
        },
    };
    Ok(CommandReceipt {
        operation_id: OperationId::new(
            uuid::Uuid::parse_str(&record.operation_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        ),
        store_generation: StoreGeneration::new(record.store_generation),
        frontend_id: ltmrs_domain::id::FrontendId::new(
            uuid::Uuid::parse_str(&record.frontend_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        ),
        channel_id: ltmrs_domain::id::ChannelId::new(
            uuid::Uuid::parse_str(&record.channel_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        ),
        request_digest: record.request_digest,
        outcome,
        retry_epoch: record.retry_epoch,
    })
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> DomainResult<T> {
    serde_json::from_slice(bytes)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltmrs_domain::command::{DomainCommand, ForgetMode};
    use ltmrs_domain::id::{EntityId, ModelFingerprint, StoreGeneration};
    use ltmrs_domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
    use ltmrs_domain::relation::{Relation, RelationType};
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn memory(id: EntityId, title: &str) -> Memory {
        Memory {
            id,
            external_alias: None,
            title: title.to_string(),
            fragment: format!("frag-{title}"),
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
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
            created_at: ltmrs_domain::memory::Instant::new(1),
            updated_at: ltmrs_domain::memory::Instant::new(1),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn ctx(op_num: u64, digest: &str) -> CommandContext {
        CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1)),
            channel_id: ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: OperationId::new(Uuid::from_u128(op_num as u128)),
            request_digest: digest.to_string(),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch: 1,
        }
    }

    fn ch(n: u64) -> ChannelId {
        ChannelId::new(Uuid::from_u128(n as u128))
    }

    /// Open a repo and issue a namespace for frontend 1 at epoch 1.
    fn repo_with_ns() -> (CanonicalRepository, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        // Frozen clock at 1000 so namespace validity checks are deterministic
        // and consistent with the issue_namespace time below.
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        let ns = repo
            .issue_namespace(
                ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1)),
                ch(2),
                1000,
            )
            .unwrap();
        assert_eq!(ns.retry_epoch, 1, "first namespace is epoch 1");
        (repo, dir)
    }

    #[test]
    fn apply_add_memory_stores_receipt_atomically() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "hello");
        let r = repo
            .apply(
                &ctx(1, "d1"),
                &DomainCommand::AddMemory {
                    memory: m,
                    session: None,
                },
            )
            .unwrap();
        assert!(matches!(r.outcome, ReceiptOutcome::Success { .. }));
        // Receipt is durably recorded with the same operation key.
        let stored = repo
            .lookup_receipt(
                StoreGeneration::FIRST,
                r.frontend_id,
                r.retry_epoch,
                r.operation_id,
            )
            .unwrap()
            .unwrap();
        assert_eq!(stored.operation_id, r.operation_id);
        assert_eq!(stored.request_digest, "d1");
    }

    #[test]
    fn idempotent_replay_returns_recorded_receipt() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "hello");
        let c = ctx(1, "d1");
        let r1 = repo
            .apply(
                &c,
                &DomainCommand::AddMemory {
                    memory: m.clone(),
                    session: None,
                },
            )
            .unwrap();
        // Same key + same digest: replay returns the recorded result, no error.
        let r2 = repo
            .apply(
                &c,
                &DomainCommand::AddMemory {
                    memory: m,
                    session: None,
                },
            )
            .unwrap();
        assert_eq!(r1.operation_id, r2.operation_id);
    }

    #[test]
    fn key_reuse_with_different_input_is_error() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "hello");
        let c1 = ctx(1, "d1");
        repo.apply(
            &c1,
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
            },
        )
        .unwrap();
        // Same operation key, different digest: must be rejected.
        let c2 = ctx(1, "DIFFERENT");
        let err = repo
            .apply(
                &c2,
                &DomainCommand::AddMemory {
                    memory: memory(eid(1), "x"),
                    session: None,
                },
            )
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::KeyReuseDifferentInput);
    }

    #[test]
    fn stale_revision_conflict_is_not_blindly_rebased() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "hello");
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
            },
        )
        .unwrap();
        // Update with a stale expected_revision must be rejected, not rebased.
        let patch = ltmrs_domain::command::MemoryPatch {
            title: Some("new".into()),
            ..Default::default()
        };
        let err = repo
            .apply(
                &ctx(2, "d2"),
                &DomainCommand::UpdateMemory {
                    id: eid(1),
                    expected_revision: Some(ltmrs_domain::id::EntityRevision::new(99)),
                    patch,
                },
            )
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::RevisionConflict);
    }

    #[test]
    fn supersession_cycle_rejected_concurrency_safely() {
        let (repo, _dir) = repo_with_ns();
        for n in [1u64, 2, 3] {
            repo.apply(
                &ctx(n, &format!("m{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), &format!("m{n}")),
                    session: None,
                },
            )
            .unwrap();
        }
        // 1 -> 2 -> 3 supersession chain.
        repo.apply(
            &ctx(10, "e1"),
            &DomainCommand::Relate {
                relation: rel(eid(100), eid(1), eid(2), RelationType::Supersedes),
            },
        )
        .unwrap();
        repo.apply(
            &ctx(11, "e2"),
            &DomainCommand::Relate {
                relation: rel(eid(101), eid(2), eid(3), RelationType::Supersedes),
            },
        )
        .unwrap();
        // 3 -> 1 would form a cycle: must be rejected.
        let err = repo
            .apply(
                &ctx(12, "e3"),
                &DomainCommand::Relate {
                    relation: rel(eid(102), eid(3), eid(1), RelationType::Supersedes),
                },
            )
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::SupersessionCycle);
    }

    #[test]
    fn concurrent_opposite_supersession_edges_keep_graph_acyclic() {
        // T-GRAPH-01 race: A→B ∥ B→A supersedes with a barrier so both
        // validate against the same snapshot. Exactly one must win; the
        // loser must observe SupersessionCycle (via SSI conflict + retry
        // or by seeing the winner's edge directly). Final graph is acyclic.
        let (repo, _dir) = repo_with_ns();
        for n in [1u64, 2] {
            repo.apply(
                &ctx(n, &format!("m{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), &format!("m{n}")),
                    session: None,
                },
            )
            .unwrap();
        }
        let repo = std::sync::Arc::new(repo);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for (i, (s, t, rid)) in [(eid(1), eid(2), eid(100)), (eid(2), eid(1), eid(101))]
            .into_iter()
            .enumerate()
        {
            let repo = std::sync::Arc::clone(&repo);
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let c = ctx(i as u64 + 200, &format!("race{i}"));
                repo.apply(
                    &c,
                    &DomainCommand::Relate {
                        relation: rel(rid, s, t, RelationType::Supersedes),
                    },
                )
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.join().unwrap());
        }
        let winners = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            winners, 1,
            "exactly one opposite edge must win, got {results:?}"
        );
        for r in results.iter().filter_map(|r| r.as_ref().err()) {
            assert_eq!(r.code, DomainErrorCode::SupersessionCycle);
        }
        // Final graph holds a single supersession edge: still acyclic.
        // Note neighbors(id) returns edges where id is source OR target,
        // so one edge is visible from both endpoints: dedupe by relation id.
        let mut ids: Vec<EntityId> = repo
            .neighbors(eid(1))
            .unwrap()
            .into_iter()
            .chain(repo.neighbors(eid(2)).unwrap())
            .map(|r| r.id)
            .collect();
        ids.sort_by_key(|id| id.as_uuid());
        ids.dedup_by_key(|id| id.as_uuid());
        assert_eq!(ids.len(), 1, "one surviving supersession edge expected");
    }

    use ltmrs_domain::projection::GenerationStatus;

    /// Blue-green cutover is atomic: staging and partial builds never move the
    /// active pointer; one activation publishes the new generation in a single
    /// step while the old rows stay retained for rollback.
    #[test]
    fn cutover_is_atomic_and_requires_readiness() {
        let (repo, _dir) = repo_with_ns();
        for n in [1u64, 2] {
            repo.apply(
                &ctx(n, &format!("m{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), &format!("m{n}")),
                    session: None,
                },
            )
            .unwrap();
        }
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        assert_eq!(next, StoreGeneration::new(2));
        // Staged only: pointer unchanged, activation refused.
        assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
        let rec = repo.generation_record(next).unwrap().unwrap();
        assert_eq!(rec.status, GenerationStatus::Staged);
        assert_eq!(rec.desired_memories, 2);
        // Partial build: still not ready.
        repo.note_generation_progress(next, 1).unwrap();
        assert_eq!(
            repo.generation_record(next).unwrap().unwrap().status,
            GenerationStatus::Building
        );
        assert!(repo.activate_generation(next).is_err());
        assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
        // Complete build: ready, then exactly-one-step activation.
        repo.note_generation_progress(next, 2).unwrap();
        assert_eq!(
            repo.generation_record(next).unwrap().unwrap().status,
            GenerationStatus::Ready
        );
        repo.activate_generation(next).unwrap();
        assert_eq!(repo.store_generation().unwrap(), next);
        assert_eq!(
            repo.generation_record(next).unwrap().unwrap().status,
            GenerationStatus::Active
        );
        // Previous generation retired with a timestamp for the reaper.
        let prev = repo
            .generation_record(StoreGeneration::FIRST)
            .unwrap()
            .unwrap();
        assert_eq!(prev.status, GenerationStatus::Retired);
    }

    /// The watermark is conservative: memories added mid-build make the staged
    /// denominator stale, and activation must refuse until a fresh build
    /// covers them (no silent partial generation).
    #[test]
    fn activate_rejects_stale_watermark() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        // A concurrent write lands mid-build.
        repo.apply(
            &ctx(2, "m2"),
            &DomainCommand::AddMemory {
                memory: memory(eid(2), "m2"),
                session: None,
            },
        )
        .unwrap();
        // The build covered only the staged denominator: Ready by count, but
        // activation sees the newer recallable memory and refuses.
        repo.note_generation_progress(next, 1).unwrap();
        assert_eq!(
            repo.generation_record(next).unwrap().unwrap().status,
            GenerationStatus::Ready
        );
        assert!(repo.activate_generation(next).is_err());
        assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
    }

    /// Rollback needs no rebuild: a retired generation's rows are retained,
    /// so re-activating it flips the pointer back in one step.
    #[test]
    fn rollback_restores_previous_generation() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.note_generation_progress(next, 1).unwrap();
        repo.activate_generation(next).unwrap();
        assert_eq!(repo.store_generation().unwrap(), next);
        // Roll back: no build, just re-activation.
        repo.activate_generation(StoreGeneration::FIRST).unwrap();
        assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
        assert_eq!(
            repo.generation_record(StoreGeneration::FIRST)
                .unwrap()
                .unwrap()
                .status,
            GenerationStatus::Active
        );
        assert_eq!(
            repo.generation_record(next).unwrap().unwrap().status,
            GenerationStatus::Retired
        );
    }

    /// A canonical write during an open build dirties the pipeline: activation
    /// is refused until a fresh build is reported, so a mid-build write can
    /// never slip into a silently partial generation.
    #[test]
    fn mid_build_write_blocks_activation_until_renote() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.note_generation_progress(next, 1).unwrap();
        // A concurrent write lands mid-build.
        repo.apply(
            &ctx(2, "m2"),
            &DomainCommand::AddMemory {
                memory: memory(eid(2), "m2"),
                session: None,
            },
        )
        .unwrap();
        let err = repo.activate_generation(next).unwrap_err();
        assert!(
            err.message.contains("dirty"),
            "dirty pipeline must refuse activation, got: {}",
            err.message
        );
        assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
        // Fresh build covering both memories converges the cutover.
        repo.note_generation_progress(next, 2).unwrap();
        repo.activate_generation(next).unwrap();
        assert_eq!(repo.store_generation().unwrap(), next);
    }

    /// Deletions dirty the pipeline too: a forget removes projected content,
    /// so the build must be refreshed before activation.
    #[test]
    fn forget_mid_build_dirties_pipeline() {
        let (repo, _dir) = repo_with_ns();
        for n in [1u64, 2] {
            repo.apply(
                &ctx(n, &format!("m{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), &format!("m{n}")),
                    session: None,
                },
            )
            .unwrap();
        }
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.note_generation_progress(next, 2).unwrap();
        repo.apply(
            &ctx(3, "f1"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ltmrs_domain::command::ForgetMode::Delete,
            },
        )
        .unwrap();
        assert!(repo.activate_generation(next).is_err());
    }

    /// Writes with no open pipeline touch no records: staging starts clean.
    #[test]
    fn writes_without_pipeline_leave_no_dirty_state() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        let rec = repo.generation_record(next).unwrap().unwrap();
        assert!(!rec.build_dirty);
    }

    /// Merge changes the recallable set (archived sources, new live result),
    /// so it must enqueue projection work for both and dirty open builds —
    /// otherwise a cutover could activate missing the result entirely.
    #[test]
    fn merge_enqueues_jobs_and_dirties_pipeline() {
        let (repo, _dir) = repo_with_ns();
        for n in [1u64, 2] {
            repo.apply(
                &ctx(n, &format!("m{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), &format!("m{n}")),
                    session: None,
                },
            )
            .unwrap();
        }
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.apply(
            &ctx(3, "merge"),
            &DomainCommand::Merge {
                source_ids: vec![eid(1), eid(2)],
                result: memory(eid(3), "m3"),
            },
        )
        .unwrap();
        // Result gets a pending job; archived sources get tombstone jobs.
        let result_job = repo.projection_job(eid(3)).unwrap().unwrap();
        assert!(!result_job.is_tombstone);
        for s in [eid(1), eid(2)] {
            let tomb = repo.projection_job(s).unwrap().unwrap();
            assert!(tomb.is_tombstone, "archived source needs a tombstone job");
        }
        // And the open build is dirty.
        assert!(repo.generation_record(next).unwrap().unwrap().build_dirty);
        assert!(repo.activate_generation(next).is_err());
    }

    /// Project-only updates change indexed rows (project column), so they
    /// enqueue work and dirty builds exactly like content changes.
    #[test]
    fn project_only_update_enqueues_and_dirties() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.note_generation_progress(next, 1).unwrap();
        let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
        repo.apply(
            &ctx(2, "proj"),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch: ltmrs_domain::command::MemoryPatch {
                    project: Some(Some("elsewhere".to_string())),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let job = repo.projection_job(eid(1)).unwrap().unwrap();
        assert!(
            job.seq > seq_before,
            "project change must enqueue a newer job"
        );
        assert!(repo.generation_record(next).unwrap().unwrap().build_dirty);
    }

    /// Confidence-only updates refresh the projection: confidence is a
    /// filter-relevant projection column (source pre-filter), so a changed
    /// confidence must re-publish — otherwise post-convergence drift
    /// silently breaks eligibility. No content changed, so no build-dirty.
    #[test]
    fn confidence_only_update_enqueues_refresh() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
        repo.apply(
            &ctx(2, "conf"),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch: ltmrs_domain::command::MemoryPatch {
                    confidence: Some(0.9),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let job = repo.projection_job(eid(1)).unwrap().unwrap();
        assert_eq!(
            job.seq,
            seq_before + 1,
            "confidence change must refresh the projection"
        );
        assert!(!repo.generation_record(next).unwrap().unwrap().build_dirty);
        // Identical absolute value: no drift, no job.
        let seq_after = repo.projection_job(eid(1)).unwrap().unwrap().seq;
        repo.apply(
            &ctx(3, "conf2"),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch: ltmrs_domain::command::MemoryPatch {
                    confidence: Some(0.9),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            repo.projection_job(eid(1)).unwrap().unwrap().seq,
            seq_after,
            "identical confidence must enqueue nothing"
        );
    }

    /// Feedback changes confidence: the projection must refresh so the
    /// source pre-filter reads the adjusted value, not the converged one.
    #[test]
    fn feedback_enqueues_projection_refresh() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
        let mut c = ctx(2, "fb");
        c.retry_epoch = 1;
        repo.apply(
            &c,
            &DomainCommand::Feedback {
                memory_id: eid(1),
                useful: false,
            },
        )
        .unwrap();
        let job = repo.projection_job(eid(1)).unwrap().unwrap();
        assert_eq!(
            job.seq,
            seq_before + 1,
            "feedback confidence change must refresh the projection"
        );
    }

    /// Read-side access bumps confidence (+0.015): same refresh rule —
    /// micro-drift still flips threshold eligibility over time.
    #[test]
    fn access_enqueues_projection_refresh() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
        let mut c = ctx(2, "acc");
        c.retry_epoch = 1;
        repo.apply(
            &c,
            &DomainCommand::Access {
                memory_ids: vec![eid(1)],
                context: None,
            },
        )
        .unwrap();
        let job = repo.projection_job(eid(1)).unwrap().unwrap();
        assert_eq!(
            job.seq,
            seq_before + 1,
            "access confidence change must refresh the projection"
        );
    }

    /// Saturated clamps enqueue nothing on any hot path: at confidence
    /// 1.0 a positive feedback/access/boost changes nothing, so no
    /// refresh job is recorded.
    #[test]
    fn saturated_clamp_enqueues_nothing() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let mut top = repo.get_memories(&[eid(1)]).unwrap().remove(0);
        top.confidence = 1.0;
        repo.put_memory_direct(&top).unwrap();
        let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
        let mut c = ctx(2, "fb");
        c.retry_epoch = 1;
        repo.apply(
            &c,
            &DomainCommand::Feedback {
                memory_id: eid(1),
                useful: true,
            },
        )
        .unwrap();
        let mut c = ctx(3, "acc");
        c.retry_epoch = 1;
        repo.apply(
            &c,
            &DomainCommand::Access {
                memory_ids: vec![eid(1)],
                context: None,
            },
        )
        .unwrap();
        let mut c = ctx(4, "boost");
        c.retry_epoch = 1;
        repo.apply(
            &c,
            &DomainCommand::BoostConfidence {
                memory_ids: vec![eid(1)],
            },
        )
        .unwrap();
        assert_eq!(
            repo.projection_job(eid(1)).unwrap().unwrap().seq,
            seq_before,
            "saturated clamps must enqueue nothing on any path"
        );
    }

    /// Floor clamp likewise: at confidence 0.0 a negative feedback
    /// changes nothing, so no refresh job is recorded.
    #[test]
    fn floor_clamp_enqueues_nothing() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let mut low = repo.get_memories(&[eid(1)]).unwrap().remove(0);
        low.confidence = 0.0;
        repo.put_memory_direct(&low).unwrap();
        let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
        let mut c = ctx(2, "fb");
        c.retry_epoch = 1;
        repo.apply(
            &c,
            &DomainCommand::Feedback {
                memory_id: eid(1),
                useful: false,
            },
        )
        .unwrap();
        assert_eq!(
            repo.projection_job(eid(1)).unwrap().unwrap().seq,
            seq_before,
            "floor clamp must enqueue nothing"
        );
    }

    /// Pre-upgrade records without the flag decode as dirty (fail-closed):
    /// an open pipeline of unknown build state must be re-reported, never
    /// trusted clean.
    #[test]
    fn old_record_without_flag_decodes_dirty() {
        let raw = r#"{"generation":2,"model_fingerprint":7,"status":"Staged","desired_memories":1,"projected_memories":0,"updated_at_millis":1000}"#;
        let rec: ltmrs_domain::projection::GenerationRecord = serde_json::from_str(raw).unwrap();
        assert!(rec.build_dirty);
    }

    /// A dirty retired pipeline still rolls back: retained rows need no
    /// build, so the dirty gate (like the watermark) exempts rollback.
    #[test]
    fn dirty_retired_generation_still_rolls_back() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.note_generation_progress(next, 1).unwrap();
        // Mid-build write, then abandon instead of rebuilding.
        repo.apply(
            &ctx(2, "m2"),
            &DomainCommand::AddMemory {
                memory: memory(eid(2), "m2"),
                session: None,
            },
        )
        .unwrap();
        repo.abandon_generation(next).unwrap();
        // Rollback to the abandoned generation succeeds despite the dirt:
        // it reuses retained rows.
        repo.activate_generation(next).unwrap();
        assert_eq!(repo.store_generation().unwrap(), next);
    }

    /// Abandoning a staged pipeline retires it (partial rows become reaper
    /// food) and unblocks fresh staging. Active generations cannot be
    /// abandoned — restore or cut over instead.
    #[test]
    fn abandon_releases_staged_pipeline() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let staged = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.abandon_generation(staged).unwrap();
        assert_eq!(
            repo.generation_record(staged).unwrap().unwrap().status,
            GenerationStatus::Retired
        );
        // Fresh pipeline stages immediately after (numbers keep advancing).
        let next = repo.stage_generation(ModelFingerprint::new(8)).unwrap();
        assert_eq!(next, StoreGeneration::new(3));
        // Unknown and active generations cannot be abandoned.
        assert!(repo.abandon_generation(StoreGeneration::new(99)).is_err());
        assert!(repo.abandon_generation(StoreGeneration::FIRST).is_err());
    }

    /// A restore retires every live pipeline record: pre-restore staged
    /// workers lose publish rights and fresh staging works immediately.
    #[test]
    fn restore_retires_staged_pipeline() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let staged = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.set_store_generation(StoreGeneration::new(9)).unwrap();
        assert_eq!(repo.store_generation().unwrap(), StoreGeneration::new(9));
        assert_eq!(
            repo.generation_record(staged).unwrap().unwrap().status,
            GenerationStatus::Retired
        );
        let next = repo.stage_generation(ModelFingerprint::new(8)).unwrap();
        assert_eq!(next, StoreGeneration::new(10));
    }

    /// Re-activating the active generation succeeds, so operator retries
    /// after a timeout do not look like failures.
    #[test]
    fn activate_is_idempotent() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        repo.note_generation_progress(next, 1).unwrap();
        repo.activate_generation(next).unwrap();
        repo.activate_generation(next).unwrap();
        assert_eq!(repo.store_generation().unwrap(), next);
    }

    /// An interrupted build (staged record, no activation) still resolves to
    /// the last verified generation after reopen — never a partial one.
    #[test]
    fn interrupted_build_resolves_to_last_active() {
        let (repo, dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "m1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m1"),
                session: None,
            },
        )
        .unwrap();
        let staged = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        assert_eq!(staged, StoreGeneration::new(2));
        // Simulate a crash between stage and activate: drop the handle and
        // reopen over the same directory (dir stays alive, like the Fjall
        // kill/reopen durability test).
        let path = dir.path().to_str().unwrap().to_string();
        drop(repo);
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo = CanonicalRepository::open_with_clock(&path, clock).unwrap();
        assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
        let staged = repo
            .generation_record(StoreGeneration::new(2))
            .unwrap()
            .unwrap();
        assert_eq!(staged.status, GenerationStatus::Staged);
        // A stale activation attempt against the partial build still fails.
        assert!(repo.activate_generation(StoreGeneration::new(2)).is_err());
    }

    fn rel(id: EntityId, s: EntityId, t: EntityId, ty: RelationType) -> Relation {
        Relation::new(id, s, t, ty, None, ltmrs_domain::memory::Instant::new(1))
    }

    #[test]
    fn forget_transitions_lifecycle() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "x"),
                session: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Invalidate,
            },
        )
        .unwrap();
        let m = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
        assert!(!m.lifecycle.is_recallable());
    }

    #[test]
    fn hard_delete_severs_adjacency() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "a"),
                session: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::AddMemory {
                memory: memory(eid(2), "b"),
                session: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(3, "e1"),
            &DomainCommand::Relate {
                relation: rel(eid(100), eid(1), eid(2), RelationType::Supports),
            },
        )
        .unwrap();
        assert_eq!(repo.neighbors(eid(1)).unwrap().len(), 1);

        // Hard delete memory 1: its edge must be removed.
        repo.apply(
            &ctx(4, "d3"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
        .unwrap();
        assert_eq!(repo.neighbors(eid(1)).unwrap().len(), 0);
    }

    #[test]
    fn one_winner_for_concurrent_absent_key() {
        let (repo, _dir) = repo_with_ns();
        let repo = std::sync::Arc::new(repo);

        const N: usize = 8;
        let mut handles = Vec::new();
        for i in 0..N {
            let repo = std::sync::Arc::clone(&repo);
            handles.push(std::thread::spawn(move || {
                // Distinct operation ids, same target memory: contested uniqueness.
                let c = ctx(i as u64 + 100, &format!("d{i}"));
                repo.apply(
                    &c,
                    &DomainCommand::AddMemory {
                        memory: memory(eid(1), "T"),
                        session: None,
                    },
                )
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.join().unwrap());
        }
        let winners = results.iter().filter(|r| r.is_ok()).count();
        let rows = repo.get_memories(&[eid(1)]).unwrap().len();
        eprintln!(
            "T-CONC-02: winners={winners} rows={rows} (one-winner requires winners==1 && rows==1)"
        );
        assert_eq!(winners, 1, "exactly one winner expected");
        assert_eq!(rows, 1, "exactly one row expected");
    }

    #[test]
    fn namespace_epochs_increment() {
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
        let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
        let ns1 = repo.issue_namespace(fe, ch(2), 1000).unwrap();
        let ns2 = repo.issue_namespace(fe, ch(2), 2000).unwrap();
        assert_eq!(ns1.retry_epoch, 1);
        assert_eq!(ns2.retry_epoch, 2, "each issue increments the epoch");
        assert!(ns1.is_valid_at(1000));
        assert!(ns2.expires_at > ns2.issued_at);
    }

    #[test]
    fn unknown_namespace_is_refused_as_stale() {
        let (repo, _dir) = repo_with_ns();
        // A ctx with a retry_epoch that was never issued must be refused.
        let mut c = ctx(1, "d1");
        c.retry_epoch = 999;
        let err = repo
            .apply(
                &c,
                &DomainCommand::AddMemory {
                    memory: memory(eid(1), "x"),
                    session: None,
                },
            )
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::StaleReplay);
    }

    #[test]
    fn expired_namespace_receipts_are_gc_d() {
        let dir = tempfile::tempdir().unwrap();
        // Frozen clock at 1000 (within the namespace's validity window) so the
        // apply() below passes namespace validation deterministically.
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
        let ns = repo.issue_namespace(fe, ch(2), 1000).unwrap();

        // Apply a command under this namespace.
        let mut c = ctx(1, "d1");
        c.retry_epoch = ns.retry_epoch;
        repo.apply(
            &c,
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "x"),
                session: None,
            },
        )
        .unwrap();
        assert!(
            repo.lookup_receipt(StoreGeneration::FIRST, fe, ns.retry_epoch, c.operation_id)
                .unwrap()
                .is_some()
        );

        // GC at a time beyond the namespace expiry removes the receipt.
        let removed = repo.gc_expired(ns.expires_at + 1).unwrap();
        assert!(removed >= 1, "expired receipt should be removed");
        assert!(
            repo.lookup_receipt(StoreGeneration::FIRST, fe, ns.retry_epoch, c.operation_id)
                .unwrap()
                .is_none()
        );
    }

    /// GC must remove EVERY receipt under an expired namespace: removing
    /// while iterating the keyspace risks skipping entries (iterator
    /// invalidation) and orphaning receipts no future GC re-triggers for.
    #[test]
    fn gc_expired_removes_all_receipts_under_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
        let ns = repo.issue_namespace(fe, ch(2), 1000).unwrap();

        let mut ops = Vec::new();
        for (i, op) in [(1u64, 10u64), (2, 11), (3, 12)] {
            let mut c = ctx(op, &format!("d{op}"));
            c.retry_epoch = ns.retry_epoch;
            repo.apply(
                &c,
                &DomainCommand::AddMemory {
                    memory: memory(eid(i), "x"),
                    session: None,
                },
            )
            .unwrap();
            ops.push(c.operation_id);
        }

        let removed = repo.gc_expired(ns.expires_at + 1).unwrap();
        assert_eq!(
            removed, 3,
            "all three receipts must be removed, got {removed}"
        );
        for op in ops {
            assert!(
                repo.lookup_receipt(StoreGeneration::FIRST, fe, ns.retry_epoch, op)
                    .unwrap()
                    .is_none(),
                "receipt {op:?} orphaned by GC"
            );
        }
    }

    #[test]
    fn add_memory_records_pending_projection() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "hello");
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
            },
        )
        .unwrap();
        assert!(
            repo.has_pending_projection(eid(1)).unwrap(),
            "add memory must atomically record a pending projection"
        );
    }

    #[test]
    fn hard_delete_invalidates_projection_and_severs_edges() {
        let (repo, _dir) = repo_with_ns();
        let a = memory(eid(1), "a");
        let b = memory(eid(2), "b");
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: a,
                session: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::AddMemory {
                memory: b,
                session: None,
            },
        )
        .unwrap();
        // Link a -> b.
        let rel = rel(eid(3), eid(1), eid(2), RelationType::Supersedes);
        repo.apply(&ctx(3, "d3"), &DomainCommand::Relate { relation: rel })
            .unwrap();
        assert_eq!(repo.neighbors(eid(1)).unwrap().len(), 1);
        assert!(repo.has_pending_projection(eid(1)).unwrap());

        // Hard delete a.
        repo.apply(
            &ctx(4, "d4"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
        .unwrap();

        // Projection invalidated via a durable tombstone job (the worker will
        // remove rows); edges severed; canonical tombstone preserved.
        let job = repo
            .projection_job(eid(1))
            .unwrap()
            .expect("tombstone job recorded");
        assert!(
            job.is_tombstone,
            "hard delete must record a tombstone projection job"
        );
        assert_eq!(
            repo.neighbors(eid(1)).unwrap().len(),
            0,
            "hard delete must sever adjacency"
        );
        let tombstone = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
        assert!(matches!(
            tombstone.lifecycle,
            MemoryLifecycle::Deleted { .. }
        ));
    }

    #[test]
    fn invalidate_preserves_edges_and_invalidates_projection() {
        let (repo, _dir) = repo_with_ns();
        let a = memory(eid(1), "a");
        let b = memory(eid(2), "b");
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: a,
                session: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::AddMemory {
                memory: b,
                session: None,
            },
        )
        .unwrap();
        let rel = rel(eid(3), eid(1), eid(2), RelationType::Supersedes);
        repo.apply(&ctx(3, "d3"), &DomainCommand::Relate { relation: rel })
            .unwrap();

        repo.apply(
            &ctx(4, "d4"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Invalidate,
            },
        )
        .unwrap();

        // Invalidation preserves edges as history; a tombstone job is recorded.
        let job = repo
            .projection_job(eid(1))
            .unwrap()
            .expect("tombstone job recorded");
        assert!(
            job.is_tombstone,
            "invalidation must record a tombstone projection job"
        );
        assert_eq!(
            repo.neighbors(eid(1)).unwrap().len(),
            1,
            "invalidation preserves edges as history"
        );
        let rec = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
        assert!(matches!(rec.lifecycle, MemoryLifecycle::Invalidated { .. }));
    }

    #[test]
    fn forget_preserves_receipt_history() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "hello");
        let add_receipt = repo
            .apply(
                &ctx(1, "d1"),
                &DomainCommand::AddMemory {
                    memory: m,
                    session: None,
                },
            )
            .unwrap();
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
        .unwrap();

        // The add receipt survives the forget: audit history is never removed.
        assert!(
            repo.lookup_receipt(
                StoreGeneration::FIRST,
                add_receipt.frontend_id,
                add_receipt.retry_epoch,
                add_receipt.operation_id
            )
            .unwrap()
            .is_some(),
            "receipt history must survive deletion"
        );
    }

    #[test]
    fn feedback_updates_counters_and_records_event() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "hello");
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
            },
        )
        .unwrap();

        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::Feedback {
                memory_id: eid(1),
                useful: true,
            },
        )
        .unwrap();

        // Domain state: observable counters updated.
        let rec = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
        assert_eq!(rec.positive_feedback, 1);
        assert!(rec.confidence > 0.5);

        // Diagnostic telemetry: a separate event log records the feedback.
        let events = repo.feedback_events().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].useful);
        assert_eq!(events[0].memory_id, eid(1));
    }

    #[test]
    fn feedback_replay_does_not_double_record() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "hello");
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
            },
        )
        .unwrap();

        let c = ctx(2, "d2");
        repo.apply(
            &c,
            &DomainCommand::Feedback {
                memory_id: eid(1),
                useful: true,
            },
        )
        .unwrap();
        // Replay the same operation (same key + digest).
        repo.apply(
            &c,
            &DomainCommand::Feedback {
                memory_id: eid(1),
                useful: true,
            },
        )
        .unwrap();

        // One logical feedback produces one event, not two.
        let events = repo.feedback_events().unwrap();
        assert_eq!(events.len(), 1, "replay must not double-record the event");
        let rec = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
        assert_eq!(rec.positive_feedback, 1);
    }

    #[test]
    fn injected_unknown_outcome_leaves_store_consistent() {
        let (repo, _dir) = repo_with_ns();
        // Inject one unknown-outcome fault on the next commit.
        repo.fault_injector().set_commit_unknown_outcomes(1);

        // The apply should report an unknown outcome (no receipt recorded).
        let result = repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "hello"),
                session: None,
            },
        );
        // The unknown outcome is resolved: no receipt exists, so it's an error.
        assert!(
            result.is_err(),
            "unknown outcome with no receipt must error"
        );

        // The store must be consistent: no partial write, no memory, no receipt.
        assert!(repo.get_memories(&[eid(1)]).unwrap().is_empty());
        let c = ctx(1, "d1");
        assert!(
            repo.lookup_receipt(
                c.store_generation,
                c.frontend_id,
                c.retry_epoch,
                c.operation_id
            )
            .unwrap()
            .is_none()
        );

        // A fresh apply of the same operation succeeds (the fault was consumed).
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "hello"),
                session: None,
            },
        )
        .unwrap();
        assert_eq!(repo.get_memories(&[eid(1)]).unwrap().len(), 1);
    }

    #[test]
    fn gc_does_not_delete_other_frontends_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        // Two frontends, each at epoch 1 (same retry_epoch value), but issued
        // at different times so A expires before B.
        let fe_a = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
        let fe_b = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(2));
        let ns_a = repo.issue_namespace(fe_a, ch(2), 1000).unwrap();
        let ns_b = repo.issue_namespace(fe_b, ch(2), 2000).unwrap();
        assert_eq!(
            ns_a.retry_epoch, ns_b.retry_epoch,
            "precondition: shared epoch"
        );

        // Apply one command under each frontend's namespace.
        let mut c_a = ctx(1, "d1");
        c_a.retry_epoch = ns_a.retry_epoch;
        repo.apply(
            &c_a,
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "a"),
                session: None,
            },
        )
        .unwrap();
        let mut c_b = ctx(2, "d2");
        c_b.frontend_id = fe_b;
        c_b.retry_epoch = ns_b.retry_epoch;
        repo.apply(
            &c_b,
            &DomainCommand::AddMemory {
                memory: memory(eid(2), "b"),
                session: None,
            },
        )
        .unwrap();

        // Both receipts present.
        assert!(
            repo.lookup_receipt(
                StoreGeneration::FIRST,
                fe_a,
                ns_a.retry_epoch,
                c_a.operation_id
            )
            .unwrap()
            .is_some()
        );
        assert!(
            repo.lookup_receipt(
                StoreGeneration::FIRST,
                fe_b,
                ns_b.retry_epoch,
                c_b.operation_id
            )
            .unwrap()
            .is_some()
        );

        // GC at A's expiry: A is expired, B (issued 1000ms later) is still
        // valid. Both share retry_epoch=1, so without the frontend scoping fix
        // B's receipt would be wrongly deleted.
        let removed = repo.gc_expired(ns_a.expires_at + 1).unwrap();
        assert!(removed >= 1, "A's expired receipt should be removed");

        // Frontend B's receipt must NOT have been deleted (different frontend).
        assert!(
            repo.lookup_receipt(
                StoreGeneration::FIRST,
                fe_b,
                ns_b.retry_epoch,
                c_b.operation_id
            )
            .unwrap()
            .is_some(),
            "GC of A's namespace must not delete B's receipts"
        );
    }

    #[test]
    fn injected_migration_fault_fails_atomically() {
        use crate::migrations::{
            MigrationOutcome, MigrationPlan, MigrationRunner, MigrationSafetyRules,
        };
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db = OptimisticTxDatabase::builder(dir.path().to_str().unwrap())
            .open()
            .unwrap();

        // Inject a migration fault: the first migration step fails.
        let fi = Arc::new(FaultInjector::new());
        fi.set_migration_failures(1);
        let runner = MigrationRunner::new(
            MigrationPlan::default_plan(),
            MigrationSafetyRules::default(),
        )
        .with_fault_injector(fi);

        // The migration step fails atomically (no partial version stamp).
        let err = runner.assess(&db).unwrap_err();
        assert!(
            err.message.contains("injected migration fault"),
            "migration fault must fail the assess, got: {}",
            err.message
        );

        // The store is left in a recoverable state: no version stamped.
        // A fresh runner (no fault) can complete the migration.
        let runner2 = MigrationRunner::new(
            MigrationPlan::default_plan(),
            MigrationSafetyRules::default(),
        );
        let outcome2 = runner2.assess(&db).unwrap();
        assert!(matches!(outcome2, MigrationOutcome::Migrated { .. }));
    }

    // ---- WP-05 task 4: durable desired-state jobs + compare-and-clear ----

    #[test]
    fn add_memory_records_versioned_projection_job() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "hello"),
                session: None,
            },
        )
        .unwrap();

        let job = repo.projection_job(eid(1)).unwrap().expect("job recorded");
        assert_eq!(job.memory_id, eid(1));
        // The helper view still reports pending work.
        assert!(repo.has_pending_projection(eid(1)).unwrap());
    }

    #[test]
    fn acknowledge_clears_matching_seq_only() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "hello"),
                session: None,
            },
        )
        .unwrap();
        let job = repo.projection_job(eid(1)).unwrap().unwrap();

        // Wrong seq (a stale worker) must NOT clear the work.
        assert!(
            !repo.acknowledge_projection(eid(1), job.seq + 1).unwrap(),
            "stale acknowledgement must leave work pending"
        );
        assert!(repo.has_pending_projection(eid(1)).unwrap());

        // The correct seq clears exactly once.
        assert!(repo.acknowledge_projection(eid(1), job.seq).unwrap());
        assert!(!repo.has_pending_projection(eid(1)).unwrap());
    }

    #[test]
    fn content_update_advances_desired_revision_and_seq() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "hello"),
                session: None,
            },
        )
        .unwrap();
        let job1 = repo.projection_job(eid(1)).unwrap().unwrap();

        // A content-changing update re-enqueues work at the new document
        // revision with a higher seq.
        let patch = ltmrs_domain::command::MemoryPatch {
            fragment: Some("updated body".into()),
            ..Default::default()
        };
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch,
            },
        )
        .unwrap();

        let job2 = repo.projection_job(eid(1)).unwrap().unwrap();
        assert!(job2.seq > job1.seq, "seq must advance monotonically");
        assert!(
            job2.desired_document_revision.as_u64() > job1.desired_document_revision.as_u64(),
            "desired revision must track the canonical document revision"
        );

        // A delayed worker holding the OLD seq cannot clear the newer work.
        assert!(!repo.acknowledge_projection(eid(1), job1.seq).unwrap());
        assert!(repo.has_pending_projection(eid(1)).unwrap());
    }

    #[test]
    fn forget_writes_tombstone_job_and_stale_worker_cannot_ack() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "hello"),
                session: None,
            },
        )
        .unwrap();
        let job = repo.projection_job(eid(1)).unwrap().unwrap();

        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
        .unwrap();

        // The forget atomically records a tombstone job at a higher seq — the
        // worker will remove rows; it is NOT silently dropped.
        let tomb = repo
            .projection_job(eid(1))
            .unwrap()
            .expect("tombstone job recorded");
        assert!(tomb.is_tombstone);
        assert!(tomb.seq > job.seq);

        // A delayed worker with the old seq must not clear it.
        assert!(
            !repo.acknowledge_projection(eid(1), job.seq).unwrap(),
            "forget must make stale acknowledgements fail"
        );
    }

    #[test]
    fn projection_jobs_lists_all_pending() {
        let (repo, _dir) = repo_with_ns();
        for n in [1u64, 2, 3] {
            repo.apply(
                &ctx(n, &format!("d{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), &format!("m{n}")),
                    session: None,
                },
            )
            .unwrap();
        }

        let jobs = repo.projection_jobs().unwrap();
        assert_eq!(jobs.len(), 3);
        let ids: Vec<EntityId> = jobs.iter().map(|j| j.memory_id).collect();
        for n in [1u64, 2, 3] {
            assert!(ids.contains(&eid(n)));
        }

        // Acknowledge one; it drops out of the list.
        let target = repo.projection_job(eid(2)).unwrap().unwrap();
        repo.acknowledge_projection(eid(2), target.seq).unwrap();
        assert_eq!(repo.projection_jobs().unwrap().len(), 2);
    }

    #[test]
    fn jobs_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        {
            let repo =
                CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
            let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
            let ns = repo.issue_namespace(fe, ch(2), 1000).unwrap();
            let mut c = ctx(1, "d1");
            c.retry_epoch = ns.retry_epoch;
            repo.apply(
                &c,
                &DomainCommand::AddMemory {
                    memory: memory(eid(1), "hello"),
                    session: None,
                },
            )
            .unwrap();
        }

        // Reopen: the unacknowledged job must still be pending (crash-safe).
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
        assert!(repo.projection_job(eid(1)).unwrap().is_some());
    }

    // ---- WP-05 task 9: readiness and lag metrics ----

    #[test]
    fn projection_lag_reflects_pending_work() {
        let (repo, _dir) = repo_with_ns();
        assert_eq!(repo.projection_lag().unwrap(), 0);

        for n in [1u64, 2] {
            repo.apply(
                &ctx(n, &format!("d{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), "m"),
                    session: None,
                },
            )
            .unwrap();
        }
        assert_eq!(repo.projection_lag().unwrap(), 2);

        let j = repo.projection_job(eid(1)).unwrap().unwrap();
        repo.acknowledge_projection(eid(1), j.seq).unwrap();
        assert_eq!(repo.projection_lag().unwrap(), 1);
    }

    #[test]
    fn oldest_pending_age_is_measured_from_enqueue() {
        let (repo, _dir) = repo_with_ns();
        // Frozen clock at 1000; the job is stamped with enqueued_at_millis=1000.
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m"),
                session: None,
            },
        )
        .unwrap();

        // At the same instant, age is 0.
        assert_eq!(repo.oldest_pending_age_millis(1000).unwrap(), Some(0));
        // 500ms later, age is 500.
        assert_eq!(repo.oldest_pending_age_millis(1500).unwrap(), Some(500));

        // Acknowledging clears it: no pending work means None.
        let j = repo.projection_job(eid(1)).unwrap().unwrap();
        repo.acknowledge_projection(eid(1), j.seq).unwrap();
        assert_eq!(repo.oldest_pending_age_millis(2000).unwrap(), None);
    }

    #[test]
    fn oldest_pending_uses_the_minimum_enqueue_time() {
        let (repo, _dir) = repo_with_ns();
        // Two jobs enqueued at the same frozen instant; both age identically.
        for n in [1u64, 2] {
            repo.apply(
                &ctx(n, &format!("d{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), "m"),
                    session: None,
                },
            )
            .unwrap();
        }
        assert_eq!(repo.oldest_pending_age_millis(1200).unwrap(), Some(200));

        // Clear the older one; age now derives from the remaining job.
        let j = repo.projection_job(eid(1)).unwrap().unwrap();
        repo.acknowledge_projection(eid(1), j.seq).unwrap();
        assert_eq!(repo.oldest_pending_age_millis(1200).unwrap(), Some(200));
    }

    /// Merged results register aliases like added memories do: a duplicate
    /// alias fails, a fresh alias resolves.
    #[test]
    fn merge_registers_alias_with_uniqueness() {
        use ltmrs_domain::id::ExternalAlias;
        let (repo, _dir) = repo_with_ns();
        let mut first = memory(eid(1), "first");
        first.external_alias = Some(ExternalAlias::new("taken"));
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: first,
                session: None,
            },
        )
        .unwrap();
        // Duplicate alias on merge fails instead of shadowing.
        let mut dup = memory(eid(10), "merged-dup");
        dup.external_alias = Some(ExternalAlias::new("taken"));
        let err = repo
            .apply(
                &ctx(2, "d2"),
                &DomainCommand::Merge {
                    source_ids: vec![eid(1)],
                    result: dup,
                },
            )
            .unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::DuplicateAlias
        );
        // Fresh alias registers and resolves.
        let mut fresh = memory(eid(11), "merged-fresh");
        fresh.external_alias = Some(ExternalAlias::new("fresh-alias"));
        repo.apply(
            &ctx(3, "d3"),
            &DomainCommand::Merge {
                source_ids: vec![eid(1)],
                result: fresh,
            },
        )
        .unwrap();
        assert_eq!(repo.resolve_id("fresh-alias").unwrap(), eid(11));
    }

    /// Absolute-only writes still advance the entity revision (concurrent
    /// same-expected writers conflict instead of last-writer-winning), and
    /// non-live memories refuse content writes.
    #[test]
    fn absolute_writes_advance_revision_and_respect_lifecycle() {
        use ltmrs_domain::command::MemoryPatch;
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m"),
                session: None,
            },
        )
        .unwrap();
        let rev = repo.get_memories(&[eid(1)]).unwrap()[0].entity_revision;
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: Some(rev),
                patch: MemoryPatch {
                    confidence: Some(0.9),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        // Same expected revision twice: the second write conflicts.
        let err = repo
            .apply(
                &ctx(3, "d3"),
                &DomainCommand::UpdateMemory {
                    id: eid(1),
                    expected_revision: Some(rev),
                    patch: MemoryPatch {
                        confidence: Some(0.1),
                        ..Default::default()
                    },
                },
            )
            .unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::RevisionConflict
        );
        // Archived memories refuse content writes.
        repo.apply(
            &ctx(4, "d4"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Archive,
            },
        )
        .unwrap();
        let err = repo
            .apply(
                &ctx(5, "d5"),
                &DomainCommand::UpdateMemory {
                    id: eid(1),
                    expected_revision: None,
                    patch: MemoryPatch {
                        confidence: Some(0.2),
                        ..Default::default()
                    },
                },
            )
            .unwrap_err();
        assert_eq!(err.code, ltmrs_domain::command::DomainErrorCode::Validation);
    }

    /// Relation ids are bound to their endpoints: reuse with different
    /// endpoints fails instead of silently overwriting.
    #[test]
    fn relation_id_reuse_with_different_endpoints_fails() {
        use ltmrs_domain::memory::Instant;
        let (repo, _dir) = repo_with_ns();
        for (n, title) in [(1u64, "a"), (2, "b"), (3, "c")] {
            repo.apply(
                &ctx(n, &format!("d{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), title),
                    session: None,
                },
            )
            .unwrap();
        }
        let rel = Relation::new(
            EntityId::new(uuid::Uuid::from_u128(100)),
            eid(1),
            eid(2),
            RelationType::Supports,
            None,
            Instant::new(1000),
        );
        repo.apply(&ctx(10, "d10"), &DomainCommand::Relate { relation: rel })
            .unwrap();
        let moved = Relation::new(
            EntityId::new(uuid::Uuid::from_u128(100)),
            eid(1),
            eid(3),
            RelationType::Supports,
            None,
            Instant::new(1000),
        );
        let err = repo
            .apply(&ctx(11, "d11"), &DomainCommand::Relate { relation: moved })
            .unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput
        );
    }

    /// Relation ids are bound to their full input: reuse with a different
    /// note fails instead of silently overwriting the annotation.
    #[test]
    fn relation_id_reuse_with_different_note_fails() {
        use ltmrs_domain::memory::Instant;
        let (repo, _dir) = repo_with_ns();
        for (n, title) in [(1u64, "a"), (2, "b")] {
            repo.apply(
                &ctx(n, &format!("d{n}")),
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), title),
                    session: None,
                },
            )
            .unwrap();
        }
        let rel = Relation::new(
            EntityId::new(uuid::Uuid::from_u128(100)),
            eid(1),
            eid(2),
            RelationType::Supports,
            None,
            Instant::new(1000),
        );
        repo.apply(&ctx(10, "d10"), &DomainCommand::Relate { relation: rel })
            .unwrap();
        // Same endpoints, different note: reject (an exact duplicate falls
        // through to DuplicateEdge — unchanged pre-existing behavior).
        let noted = Relation::new(
            EntityId::new(uuid::Uuid::from_u128(100)),
            eid(1),
            eid(2),
            RelationType::Supports,
            Some("changed".to_string()),
            Instant::new(1000),
        );
        let err = repo
            .apply(&ctx(12, "d12"), &DomainCommand::Relate { relation: noted })
            .unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput
        );
    }

    /// Contention contract: under same-frontend concurrent issuance every
    /// conflicting attempt must report transient Contention (retryable),
    /// never a fatal Validation — and every issue still succeeds on retry
    /// with a distinct epoch.
    #[test]
    fn namespace_contention_is_transient_typed() {
        use ltmrs_domain::id::FrontendId;
        use std::sync::{Arc, Barrier};
        let (repo, _dir) = repo_with_ns();
        let repo = Arc::new(repo);
        let fe = FrontendId::new(Uuid::from_u128(99));
        let start = Arc::new(Barrier::new(9));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let repo = Arc::clone(&repo);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                let mut epochs = Vec::new();
                for _ in 0..25 {
                    for _ in 0..20 {
                        match repo.issue_namespace(fe, ch(2), 1000) {
                            Ok(ns) => {
                                epochs.push(ns.retry_epoch);
                                break;
                            }
                            Err(e) => assert_eq!(
                                e.code,
                                ltmrs_domain::command::DomainErrorCode::Contention,
                                "contention must be typed transient, got {e:?}"
                            ),
                        }
                    }
                }
                epochs
            }));
        }
        start.wait();
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        assert_eq!(all.len(), 200, "every issue must succeed after retries");
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 200, "epochs must be distinct");
    }

    /// A corrupt epoch counter fails closed (epoch reuse would confuse
    /// replays across channels): short/long payloads at the epoch key are
    /// Validation, never silently reset to epoch 1.
    #[test]
    fn corrupt_epoch_counter_fails_closed() {
        use ltmrs_domain::id::FrontendId;
        let (repo, _dir) = repo_with_ns();
        let fe = FrontendId::new(Uuid::from_u128(99));
        let mut tx = repo.db.write_tx().unwrap();
        tx.insert(&repo.namespaces, namespace_key(fe), [0xFFu8; 3]);
        tx.commit().unwrap().unwrap();
        let err = repo.issue_namespace(fe, ch(2), 1000).unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        assert!(
            err.message.contains("corrupt namespace epoch counter"),
            "must name the corruption, got: {err:?}"
        );
    }

    /// Undecodable namespace records are healed by GC: every reader fails
    /// closed on them already, so removal changes no observable outcome
    /// but stops the entry leaking forever past every GC pass. Their
    /// orphaned receipts go too (no trigger could ever match them);
    /// other frontends' receipts are untouched. Malformed watermark keys
    /// heal the same way so op_seq un-bricks.
    #[test]
    fn gc_removes_corrupt_namespace_records() {
        use ltmrs_domain::id::FrontendId;
        let (repo, _dir) = repo_with_ns();
        let fe = FrontendId::new(Uuid::from_u128(1));
        // Two receipted commands under fe1/epoch1.
        for (n, op) in [(1u64, 10u64), (2, 11)] {
            let mut c = ctx(op, &format!("d{op}"));
            c.retry_epoch = 1;
            repo.apply(
                &c,
                &DomainCommand::AddMemory {
                    memory: memory(eid(n), "x"),
                    session: None,
                },
            )
            .unwrap();
        }
        // Corrupt fe1's namespace record + the watermark, and plant a
        // foreign-frontend receipt row (raw bytes: GC must not touch it).
        let mut tx = repo.db.write_tx().unwrap();
        let bad_ns = format!("ns:{}:1", fe.as_uuid());
        tx.insert(&repo.namespaces, &bad_ns, [0xFFu8; 5]);
        tx.insert(&repo.namespaces, "op_seq:direct", [0xFFu8; 3]);
        let foreign_key = format!(
            "1:ffffffff-ffff-ffff-ffff-ffffffffffff:9:{}",
            Uuid::from_u128(77)
        );
        tx.insert(&repo.receipts, &foreign_key, [0xFFu8; 1]);
        tx.commit().unwrap().unwrap();

        repo.gc_expired(1000).unwrap();

        let snapshot = repo.db.read_tx();
        let get = |ks: &_, k: &str| snapshot.get(ks, k).unwrap().is_some();
        assert!(!get(&repo.namespaces, &bad_ns), "corrupt ns healed");
        assert!(!get(&repo.namespaces, "op_seq:direct"), "watermark healed");
        assert!(
            get(&repo.receipts, &foreign_key),
            "foreign receipts untouched"
        );
        drop(snapshot);
        // fe1's orphaned receipts went with the corrupt record.
        for op in [10u64, 11] {
            assert!(
                repo.lookup_receipt(
                    StoreGeneration::FIRST,
                    fe,
                    1,
                    OperationId::new(Uuid::from_u128(op as u128))
                )
                .unwrap()
                .is_none(),
                "orphaned receipt must not leak"
            );
        }
        // Watermark reads again: fe1's two executions.
        assert_eq!(repo.op_seq().unwrap(), 2);
    }

    /// Corrupt healing is epoch-scoped, not frontend-scoped: a corrupt
    /// epoch-1 record must not take down live epoch-2 receipts (RQ-06
    /// replay for the live epoch keeps working).
    #[test]
    fn gc_corrupt_healing_preserves_live_epochs() {
        use ltmrs_domain::id::FrontendId;
        let (repo, _dir) = repo_with_ns();
        let fe = FrontendId::new(Uuid::from_u128(1));
        let mut c = ctx(10, "d10");
        c.retry_epoch = 1;
        repo.apply(
            &c,
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "x"),
                session: None,
            },
        )
        .unwrap();
        // Second epoch, live: must survive the healing below.
        let ns2 = repo.issue_namespace(fe, ch(2), 1000).unwrap();
        assert_eq!(ns2.retry_epoch, 2);
        let mut c2 = ctx(11, "d11");
        c2.retry_epoch = 2;
        repo.apply(
            &c2,
            &DomainCommand::AddMemory {
                memory: memory(eid(2), "x"),
                session: None,
            },
        )
        .unwrap();
        // Corrupt epoch 1's record only.
        let mut tx = repo.db.write_tx().unwrap();
        tx.insert(
            &repo.namespaces,
            format!("ns:{}:1", fe.as_uuid()),
            [0xFFu8; 5],
        );
        tx.commit().unwrap().unwrap();

        repo.gc_expired(1000).unwrap();

        assert!(
            repo.lookup_receipt(
                StoreGeneration::FIRST,
                fe,
                1,
                OperationId::new(Uuid::from_u128(10))
            )
            .unwrap()
            .is_none(),
            "corrupt epoch's receipt goes with it"
        );
        assert!(
            repo.lookup_receipt(
                StoreGeneration::FIRST,
                fe,
                2,
                OperationId::new(Uuid::from_u128(11))
            )
            .unwrap()
            .is_some(),
            "live epoch's receipt must survive healing"
        );
        assert!(
            repo.lookup_namespace(fe, 2).unwrap().is_some(),
            "live epoch's namespace must survive healing"
        );
    }

    /// The epoch counter cannot wrap: u64::MAX advances fail closed
    /// instead of panicking (debug) or reusing epoch 0 (release).
    #[test]
    fn epoch_counter_overflow_fails_closed() {
        use ltmrs_domain::id::FrontendId;
        let (repo, _dir) = repo_with_ns();
        let fe = FrontendId::new(Uuid::from_u128(99));
        let mut tx = repo.db.write_tx().unwrap();
        tx.insert(&repo.namespaces, namespace_key(fe), u64::MAX.to_le_bytes());
        tx.commit().unwrap().unwrap();
        let err = repo.issue_namespace(fe, ch(2), 1000).unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
    }

    /// Mutation watermark: every executed command advances op_seq
    /// atomically with its receipt (restore preview/confirm binding).
    /// Replays record nothing and advance nothing.
    #[test]
    fn op_seq_advances_per_execution_not_replay() {
        let (repo, _dir) = repo_with_ns();
        assert_eq!(repo.op_seq().unwrap(), 0);
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m"),
                session: None,
            },
        )
        .unwrap();
        assert_eq!(repo.op_seq().unwrap(), 1);
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::AddMemory {
                memory: memory(eid(2), "m"),
                session: None,
            },
        )
        .unwrap();
        assert_eq!(repo.op_seq().unwrap(), 2);
        // Same operation key + digest replays: no new execution, no advance.
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m"),
                session: None,
            },
        )
        .unwrap();
        assert_eq!(repo.op_seq().unwrap(), 2);
    }

    /// Direct-write primitives advance the watermark too: guide/distill
    /// writes bypass the command bus, but a restore must still count them.
    #[test]
    fn op_seq_counts_direct_writes() {
        use ltmrs_domain::guide::Guide;
        use ltmrs_domain::memory::Instant;
        let (repo, _dir) = repo_with_ns();
        assert_eq!(repo.op_seq().unwrap(), 0);
        repo.put_guide(&Guide {
            name: "g".into(),
            category: "c".into(),
            description: String::new(),
            contexts: vec![],
            learnings: vec![],
            usage_count: 0,
            last_used: None,
            success_count: 0,
            failure_count: 0,
            anti_patterns: vec![],
            pitfalls: vec![],
            depends_on: vec![],
            enables: vec![],
            source_memories: vec![],
            validated_by: vec![],
            superseded_by: None,
            deprecated: false,
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            created_at: Instant::new(0),
            updated_at: Instant::new(0),
        })
        .unwrap();
        assert_eq!(repo.op_seq().unwrap(), 1);
        // The watermark sums across writer keys (per-frontend + direct):
        // a command execution lands on top.
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m"),
                session: None,
            },
        )
        .unwrap();
        assert_eq!(repo.op_seq().unwrap(), 2);
    }

    /// Lookalike keys do not pollute the watermark: only `op_seq:`-prefixed
    /// counters sum (tight prefix, no decoys).
    #[test]
    fn op_seq_ignores_lookalike_keys() {
        let (repo, _dir) = repo_with_ns();
        let mut tx = repo.db.write_tx().unwrap();
        tx.insert(&repo.namespaces, "op_seq_backup", 100u64.to_le_bytes());
        tx.insert(&repo.namespaces, "op_seqx", 100u64.to_le_bytes());
        tx.commit().unwrap().unwrap();
        assert_eq!(
            repo.op_seq().unwrap(),
            0,
            "decoy keys must not enter the sum"
        );
        // Empty suffix and nested colons: only exact `op_seq:`-prefixed
        // counters sum; anything else is ignored, never failed closed.
        let mut tx = repo.db.write_tx().unwrap();
        tx.insert(&repo.namespaces, "op_seq:", 5u64.to_le_bytes());
        tx.insert(&repo.namespaces, "op_seq:direct:extra", 7u64.to_le_bytes());
        tx.commit().unwrap().unwrap();
        assert_eq!(
            repo.op_seq().unwrap(),
            12,
            "op_seq:-prefixed counters sum regardless of suffix shape"
        );
    }

    /// Same-frontend parallel writes share one watermark key: disjoint
    /// memories must all commit via the standard retry discipline (no
    /// spurious exhaustion), each advancing the watermark exactly once.
    /// Retry dynamics proven here; the transient typing of any residual
    /// exhaustion is pinned by `exhaustion_reports_transient_contention`
    /// (conflicts are too rare here to assert their code deterministically).
    #[test]
    fn concurrent_same_frontend_writes_all_commit() {
        use std::sync::{Arc, Barrier};
        let (repo, _dir) = repo_with_ns();
        let repo = Arc::new(repo);
        let start = Arc::new(Barrier::new(5));
        let mut handles = Vec::new();
        for t in 0..4u64 {
            let repo = Arc::clone(&repo);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                let mut done = 0;
                for i in 0..10u64 {
                    let n = t * 100 + i + 10;
                    // Retry on conflicts (shared watermark key discipline);
                    // exhaustion would be the bug.
                    for _ in 0..20 {
                        let r = repo.apply(
                            &ctx(t * 1000 + i, &format!("c{t}-{i}")),
                            &DomainCommand::AddMemory {
                                memory: memory(eid(n), "x"),
                                session: None,
                            },
                        );
                        if r.is_ok() {
                            done += 1;
                            break;
                        }
                    }
                }
                done
            }));
        }
        start.wait();
        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 40, "every parallel write must commit after retries");
        assert_eq!(repo.op_seq().unwrap(), 40);
        // Safety, not just liveness: every disjoint commit persisted.
        let live: std::collections::BTreeSet<u128> = repo
            .export_full()
            .unwrap()
            .memories
            .iter()
            .map(|m| m.id.as_uuid().as_u128())
            .collect();
        for t in 0..4u64 {
            for i in 0..10u64 {
                let n = t * 100 + i + 10;
                assert!(
                    live.contains(&(n as u128)),
                    "disjoint commit {n} lost despite Ok"
                );
            }
        }
    }

    /// SSI exhaustion is transient, never fatal: every write path that
    /// runs out of conflict budget reports Contention (safe to retry)
    /// instead of Validation (refuse). Pinned here; retry dynamics are
    /// proven by the concurrent tests.
    #[test]
    fn exhaustion_reports_transient_contention() {
        let err = CanonicalRepository::exhausted_contention("probe op");
        assert_eq!(err.code, DomainErrorCode::Contention);
        assert!(
            err.message.contains("probe op"),
            "site message preserved, got: {err:?}"
        );
    }

    /// End-to-end exhaustion typing: hammered parallel applies on one
    /// frontend collide on the shared watermark key; whatever conflicts
    /// surface must be transient Contention, never fatal Validation.
    /// (Conflicts are scheduled by the engine, so the kind assertion is
    /// opportunistic — the constructor test pins the mapping
    /// deterministically; all 200 commits succeeding proves liveness.)
    #[test]
    fn exhausted_apply_reports_transient_contention() {
        use std::sync::{Arc, Barrier};
        let (repo, _dir) = repo_with_ns();
        let repo = Arc::new(repo);
        let start = Arc::new(Barrier::new(9));
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let repo = Arc::clone(&repo);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                let mut done = 0;
                for i in 0..25u64 {
                    let n = t * 100 + i + 10;
                    for _ in 0..20 {
                        match repo.apply(
                            &ctx(t * 1000 + i, &format!("e{t}-{i}")),
                            &DomainCommand::AddMemory {
                                memory: memory(eid(n), "x"),
                                session: None,
                            },
                        ) {
                            Ok(_) => {
                                done += 1;
                                break;
                            }
                            Err(e) => assert_eq!(
                                e.code,
                                DomainErrorCode::Contention,
                                "contention must be typed transient, got {e:?}"
                            ),
                        }
                    }
                }
                done
            }));
        }
        start.wait();
        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 200, "every hammered write must commit after retries");
        assert_eq!(repo.op_seq().unwrap(), 200);
    }

    /// Feedback survives a backup/restore round trip under a stable key
    /// (no silent re-keying, no double-recording on replay).
    #[test]
    fn feedback_survives_restore_round_trip() {
        let (repo, _dir) = repo_with_ns();
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m"),
                session: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(2, "d2"),
            &DomainCommand::Feedback {
                memory_id: eid(1),
                useful: true,
            },
        )
        .unwrap();
        let before = repo.export_full().unwrap().feedback;
        assert_eq!(before.len(), 1);
        let snapshot = repo.export_full().unwrap();
        let marked = repo
            .restore_replace(
                &snapshot.memories,
                &snapshot.relations,
                &snapshot.guides,
                &snapshot.feedback,
                &snapshot.suggestions,
                &snapshot.sessions,
                StoreGeneration::new(2),
            )
            .unwrap();
        assert_eq!(marked, 0, "no live sessions to retire here");
        let after = repo.export_full().unwrap().feedback;
        assert_eq!(after, before, "feedback must round-trip identically");
    }

    /// P1 (bfe8844-review follow-up): the native backup must be one coherent
    /// cut. Sessions live in Fjall now, so `export_full_with_generation`
    /// must read them from the SAME snapshot as memories/guides/etc. — a
    /// caller-side `all_sessions()` from a second snapshot can tear across
    /// a concurrent `session_end` (Active session + already-bumped guide
    /// counts that never coexisted).
    #[test]
    fn export_full_covers_canonical_sessions() {
        use ltmrs_domain::id::{ChannelId, SessionHandle};
        use ltmrs_domain::session::SessionOp;
        let (repo, _dir) = repo_with_ns();
        let handle = SessionHandle::new(Uuid::from_u128(100));
        match repo
            .session_start_tx(
                "op-export",
                "digest-export",
                handle,
                ChannelId::new(Uuid::from_u128(9)),
                None,
                None,
                vec![],
                None,
                None,
                1000,
            )
            .unwrap()
        {
            SessionOp::Applied(h) => assert_eq!(h, handle),
            other => panic!("expected Applied, got {other:?}"),
        }
        let (export, _) = repo.export_full_with_generation().unwrap();
        assert!(
            export.sessions.iter().any(|s| s.handle == handle),
            "export must carry live sessions from its own snapshot"
        );
    }

    /// P2-B: continuity-recall boosts apply exactly once per session-start
    /// operation. The first claim applies and returns true; a second claim
    /// (crash-window continuation) returns false without touching
    /// confidence; a changed digest rejects.
    #[test]
    fn continuity_boost_claim_applies_once() {
        use ltmrs_domain::id::{ChannelId, SessionHandle};
        use ltmrs_domain::session::{AttemptOutcome, SessionOp};
        let (repo, _dir) = repo_with_ns();
        let handle = SessionHandle::new(Uuid::from_u128(100));
        match repo
            .session_start_tx(
                "op-start",
                "digest-start",
                handle,
                ChannelId::new(Uuid::from_u128(9)),
                None,
                None,
                vec![],
                None,
                None,
                1000,
            )
            .unwrap()
        {
            SessionOp::Applied(h) => assert_eq!(h, handle),
            other => panic!("expected Applied, got {other:?}"),
        }
        match repo
            .session_attempt_tx(
                "op-attempt",
                "digest-attempt",
                handle,
                "try X".to_string(),
                AttemptOutcome::Rejected,
                None,
                None,
                None,
                1000,
            )
            .unwrap()
        {
            SessionOp::Applied((_, 1)) => {}
            other => panic!("expected Applied seq 1, got {other:?}"),
        }
        // Lower first: fresh attempts start at the 1.0 ceiling where a
        // small positive delta clamps invisibly.
        repo.adjust_attempt(handle, 1, -0.5, 1000).unwrap();
        let targets = vec![(handle, 1)];
        assert!(
            repo.claim_continuity_boost("op-start", "digest-start", &targets, 0.015, 1000)
                .unwrap()
        );
        let after_first = repo.get_session(handle).unwrap().unwrap().attempts[0].confidence;
        assert!((after_first - 0.515).abs() < 1e-9, "got {after_first}");
        assert!(
            !repo
                .claim_continuity_boost("op-start", "digest-start", &targets, 0.015, 1000)
                .unwrap()
        );
        let after_second = repo.get_session(handle).unwrap().unwrap().attempts[0].confidence;
        assert_eq!(after_second, after_first, "second claim must not re-boost");
        let err = repo
            .claim_continuity_boost("op-start", "DIFFERENT", &targets, 0.015, 1000)
            .unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput
        );
    }

    /// No ACK without a barrier: an injected persist failure must fail the
    /// command (not silently succeed with buffered-only data).
    #[test]
    fn ack_requires_durability_barrier() {
        let (repo, _dir) = repo_with_ns();
        repo.fault_injector().set_persist_failures(1);
        let err = repo
            .apply(
                &ctx(1, "d1"),
                &DomainCommand::AddMemory {
                    memory: memory(eid(1), "m"),
                    session: None,
                },
            )
            .unwrap_err();
        assert!(
            err.message.contains("persist"),
            "barrier failure must fail the ACK, got: {err:?}"
        );
        // Counter consumed: the retry succeeds and the write is real.
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m"),
                session: None,
            },
        )
        .unwrap();
        assert_eq!(repo.get_memories(&[eid(1)]).unwrap().len(), 1);
    }

    /// The restore path honors the same barrier: an injected persist
    /// failure fails the replace instead of reporting a durable cutover
    /// on buffered-only data.
    #[test]
    fn restore_replace_requires_durability_barrier() {
        let (repo, _dir) = repo_with_ns();
        repo.fault_injector().set_persist_failures(1);
        let err = repo
            .restore_replace(&[], &[], &[], &[], &[], &[], StoreGeneration::new(2))
            .unwrap_err();
        assert!(
            err.message.contains("persist"),
            "barrier failure must fail the restore, got: {err:?}"
        );
    }

    /// Direct writes ACK through the same barrier.
    #[test]
    fn direct_write_requires_durability_barrier() {
        let (repo, _dir) = repo_with_ns();
        repo.fault_injector().set_persist_failures(1);
        let err = repo.put_memory_direct(&memory(eid(9), "x")).unwrap_err();
        assert!(
            err.message.contains("persist"),
            "barrier failure must fail the write, got: {err:?}"
        );
    }

    /// P1-B: resuming a live retry namespace returns the SAME epoch without
    /// minting a new one, so a reconnected frontend keeps resolving its
    /// pre-failure receipts. Unknown or expired namespaces refuse as stale
    /// (the caller surfaces an unknown outcome, never a silent fresh epoch).
    #[test]
    fn resume_namespace_returns_same_epoch() {
        let (repo, _dir) = repo_with_ns();
        let fe = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1));
        // repo_with_ns issued epoch 1 at t=1000.
        let ns = repo.resume_namespace(fe, ch(2), 1, 1000).unwrap();
        assert_eq!(ns.retry_epoch, 1);
        // Resuming does not consume an epoch: the next issue still yields 2.
        let next = repo.issue_namespace(fe, ch(2), 1000).unwrap();
        assert_eq!(next.retry_epoch, 2);
        // Unknown epoch refuses.
        let err = repo.resume_namespace(fe, ch(2), 99, 1000).unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::StaleReplay
        );
        // Another frontend's epoch is not resumable here.
        let other = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(2));
        let err = repo.resume_namespace(other, ch(2), 1, 1000).unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::StaleReplay
        );
    }

    /// P1-B: an expired namespace (past the 24h TTL) refuses resume.
    #[test]
    fn resume_namespace_rejects_expired_epoch() {
        let (repo, _dir) = repo_with_ns();
        let fe = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1));
        let expired_at = 1000 + crate::repository::DEFAULT_NAMESPACE_TTL_MILLIS + 1;
        let err = repo.resume_namespace(fe, ch(2), 1, expired_at).unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::StaleReplay
        );
        assert!(err.message.contains("expired"), "got: {err:?}");
    }

    /// NEW P1/P2 (channel isolation): a retry namespace belongs to the
    /// (frontend, channel) pair that issued it. A sibling channel resuming
    /// the same epoch is refused as stale — never silently adopted.
    #[test]
    fn resume_namespace_refuses_cross_channel() {
        let (repo, _dir) = repo_with_ns();
        let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
        // Same channel resumes fine.
        let ns = repo.resume_namespace(fe, ch(2), 1, 1000).unwrap();
        assert_eq!(ns.retry_epoch, 1);
        assert_eq!(ns.channel_id, ch(2));
        // Sibling channel is refused.
        let err = repo.resume_namespace(fe, ch(3), 1, 1000).unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::StaleReplay
        );
        assert!(err.message.contains("another channel"), "got: {err:?}");
    }

    /// Receipt replay is channel-scoped: the same operation retried from a
    /// sibling channel must neither replay nor re-execute — refused as
    /// stale at the namespace gate.
    #[test]
    fn apply_refuses_cross_channel_replay() {
        let (repo, _dir) = repo_with_ns();
        let m = memory(eid(1), "ch-scoped");
        let mut ca = ctx(1, "cross-channel");
        ca.channel_id = ch(2);
        repo.apply(
            &ca,
            &DomainCommand::AddMemory {
                memory: m.clone(),
                session: None,
            },
        )
        .unwrap();
        // Same op id + digest from a sibling channel: refused, never replayed.
        let mut cb = ctx(1, "cross-channel");
        cb.channel_id = ch(3);
        let err = repo
            .apply(
                &cb,
                &DomainCommand::AddMemory {
                    memory: m,
                    session: None,
                },
            )
            .unwrap_err();
        assert_eq!(
            err.code,
            ltmrs_domain::command::DomainErrorCode::StaleReplay
        );
        assert!(err.message.contains("another channel"), "got: {err:?}");
        // Exactly one effect: no re-execution slipped through.
        assert_eq!(repo.get_memories(&[eid(1)]).unwrap().len(), 1);
    }

    /// P1/P2 session_end exactly-once: improvement suggestions are filed
    /// inside the end transaction, so concurrent duplicate deliveries of
    /// the same end operation yield exactly one Suggestion per line —
    /// never one per delivery (file_suggestion's atomic IDs alone only
    /// prevent overwrites, not duplicates).
    #[test]
    fn session_end_files_suggestions_exactly_once_per_operation() {
        use ltmrs_domain::guide::Guide;
        use ltmrs_domain::id::{ChannelId, SessionHandle};
        use ltmrs_domain::memory::Instant;
        use ltmrs_domain::session::{SessionOp, TaskOutcome};
        use std::sync::{Arc, Barrier};
        let (repo, _dir) = repo_with_ns();
        // Guide with a failing record (total 3, rate 0.00): a Failure end
        // using it yields exactly one improvement line.
        repo.put_guide(&Guide {
            name: "git".into(),
            category: "dev-tool".into(),
            description: String::new(),
            contexts: vec![],
            learnings: vec![],
            usage_count: 0,
            last_used: None,
            success_count: 0,
            failure_count: 3,
            anti_patterns: vec![],
            pitfalls: vec![],
            depends_on: vec![],
            enables: vec![],
            source_memories: vec![],
            validated_by: vec![],
            superseded_by: None,
            deprecated: false,
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            created_at: Instant::new(0),
            updated_at: Instant::new(0),
        })
        .unwrap();
        let handle = SessionHandle::new(Uuid::from_u128(700));
        match repo
            .session_start_tx(
                "op-start-700",
                "digest-start",
                handle,
                ChannelId::new(Uuid::from_u128(2)),
                None,
                None,
                vec![],
                None,
                None,
                1000,
            )
            .unwrap()
        {
            SessionOp::Applied(h) => assert_eq!(h, handle),
            other => panic!("expected Applied, got {other:?}"),
        }
        repo.track_session_link(handle, SessionLinkField::GuideUsed, &["git".to_string()])
            .unwrap();
        // Eight duplicate deliveries of the same end operation, released
        // together: exactly one Applies, the rest Replay.
        let repo = Arc::new(repo);
        let start = Arc::new(Barrier::new(9));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let repo = Arc::clone(&repo);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                repo.session_end_tx(
                    "op-end-700",
                    "digest-end",
                    handle,
                    TaskOutcome::Failure,
                    None,
                    vec![],
                    1000,
                )
                .unwrap()
            }));
        }
        start.wait();
        let mut applied = 0;
        for h in handles {
            match h.join().unwrap() {
                SessionOp::Applied((_, lines, true)) => {
                    applied += 1;
                    assert_eq!(lines.len(), 1, "one improvement line expected");
                }
                SessionOp::Replayed((_, lines, _)) => {
                    assert_eq!(lines.len(), 1, "replay renders the same line");
                }
                other => panic!("unexpected end outcome: {other:?}"),
            }
        }
        assert_eq!(applied, 1, "exactly one delivery may apply");
        // Exactly one Suggestion for the line — never one per delivery.
        let suggestions = repo.get_suggestions().unwrap();
        assert_eq!(
            suggestions.len(),
            1,
            "exactly one suggestion per line, got {suggestions:?}"
        );
        assert_eq!(
            suggestions[0].session_id.as_deref(),
            Some(handle.as_uuid().to_string().as_str())
        );
    }

    /// Concurrent issuance for one frontend must yield distinct epochs
    /// (shared retry namespace would confuse replays across channels).
    #[test]
    fn concurrent_namespace_issue_yields_distinct_epochs() {
        use std::sync::{Arc, Barrier};
        let (repo, _dir) = repo_with_ns();
        let repo = std::sync::Arc::new(repo);
        let fe = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(99));
        let start = Arc::new(Barrier::new(9));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let repo = Arc::clone(&repo);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                let mut epochs = Vec::new();
                for _ in 0..50 {
                    // Retry on conflicts (concurrent issuance discipline);
                    // duplicates are the bug, conflicts are not.
                    for _ in 0..20 {
                        match repo.issue_namespace(fe, ch(2), 1000) {
                            Ok(ns) => {
                                epochs.push(ns.retry_epoch);
                                break;
                            }
                            Err(_) => continue,
                        }
                    }
                }
                epochs
            }));
        }
        start.wait();
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        assert_eq!(all.len(), 400, "every issue must succeed after retries");
        all.sort_unstable();
        let distinct: Vec<u64> = {
            let mut d = all.clone();
            d.dedup();
            d
        };
        assert_eq!(
            distinct.len(),
            400,
            "epochs must be distinct (double-issue reuses an epoch)"
        );
    }

    /// Concurrent suggestion filing must yield distinct ids: max+1 computed
    /// outside the insert transaction lets two writers claim the same id
    /// and silently overwrite each other.
    #[test]
    fn concurrent_suggestion_filing_yields_distinct_ids() {
        use std::sync::{Arc, Barrier};
        let (repo, _dir) = repo_with_ns();
        let repo = Arc::new(repo);
        let start = Arc::new(Barrier::new(9));
        let mut handles = Vec::new();
        for t in 0..8 {
            let repo = Arc::clone(&repo);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                let mut ids = Vec::new();
                for i in 0..50 {
                    // Retry on conflicts (concurrent filing discipline);
                    // duplicates are the bug, conflicts are not (namespace
                    // issuance precedent).
                    let mut filed = None;
                    for _ in 0..20 {
                        match repo.file_suggestion(
                            Some(format!("session-{t}")),
                            format!("suggestion {t}-{i}"),
                            1000,
                        ) {
                            Ok(s) => {
                                filed = Some(s.id);
                                break;
                            }
                            Err(_) => continue,
                        }
                    }
                    ids.push(filed.expect("filing must succeed after retries"));
                }
                ids
            }));
        }
        start.wait();
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        assert_eq!(all.len(), 400, "every filing must succeed after retries");
        all.sort_unstable();
        let distinct: Vec<u64> = {
            let mut d = all.clone();
            d.dedup();
            d
        };
        assert_eq!(
            distinct.len(),
            400,
            "suggestion ids must be distinct (double-claim overwrites a record)"
        );
        assert_eq!(repo.get_suggestions().unwrap().len(), 400);
    }

    /// P2-1 wake-up: a committed mutation fires the commit hook exactly once;
    /// an idempotent replay fires nothing (no new work to project).
    #[test]
    fn commit_hook_fires_once_per_commit_not_replay() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (repo, _dir) = repo_with_ns();
        let fired = std::sync::Arc::new(AtomicUsize::new(0));
        let hook_fired = std::sync::Arc::clone(&fired);
        repo.set_commit_hook(std::sync::Arc::new(move || {
            hook_fired.fetch_add(1, Ordering::SeqCst);
        }));
        let m = memory(eid(1), "hello");
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
            },
        )
        .unwrap();
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // Same operation + digest replays the receipt: no new commit, no fire.
        let m2 = memory(eid(1), "hello");
        repo.apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: m2,
                session: None,
            },
        )
        .unwrap();
        assert_eq!(
            fired.load(Ordering::SeqCst),
            1,
            "replay must not wake the projection worker"
        );
    }

    fn test_guide(name: &str) -> ltmrs_domain::guide::Guide {
        use ltmrs_domain::memory::Instant;
        ltmrs_domain::guide::Guide {
            name: name.into(),
            category: "dev-tool".into(),
            description: String::new(),
            contexts: vec![],
            learnings: vec![],
            usage_count: 0,
            last_used: None,
            success_count: 0,
            failure_count: 0,
            anti_patterns: vec![],
            pitfalls: vec![],
            depends_on: vec![],
            enables: vec![],
            source_memories: vec![],
            validated_by: vec![],
            superseded_by: None,
            deprecated: false,
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            created_at: Instant::new(0),
            updated_at: Instant::new(0),
        }
    }

    /// Re-review R5: a session_end guide effect applies exactly once per
    /// operation — retry resumes via the marker without double-counting,
    /// and a missing guide is a skip (forget wins), not an error.
    #[test]
    fn session_guide_effect_applies_once_per_operation() {
        let (repo, _dir) = repo_with_ns();
        repo.put_guide(&test_guide("git")).unwrap();
        assert!(
            repo.apply_session_guide_effect("end-1", "digest-1", "git", true, 1000)
                .unwrap()
        );
        assert_eq!(repo.get_guide("git").unwrap().unwrap().success_count, 1);
        // Same operation again: marker hit, no recount.
        assert!(
            !repo
                .apply_session_guide_effect("end-1", "digest-1", "git", true, 1000)
                .unwrap()
        );
        assert_eq!(repo.get_guide("git").unwrap().unwrap().success_count, 1);
        // Same operation with changed arguments after a partial effect:
        // reject, never complete a mixed outcome (re-review P1-3).
        let err = repo
            .apply_session_guide_effect("end-1", "digest-CHANGED", "git", false, 1000)
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::KeyReuseDifferentInput);
        let g = repo.get_guide("git").unwrap().unwrap();
        assert_eq!((g.success_count, g.failure_count), (1, 0));
        // Same guide, different operation: applies (independent outcome).
        assert!(
            repo.apply_session_guide_effect("end-2", "digest-2", "git", false, 1000)
                .unwrap()
        );
        let g = repo.get_guide("git").unwrap().unwrap();
        assert_eq!((g.success_count, g.failure_count), (1, 1));
        // Missing guide: skip, no marker (a later retry re-checks).
        assert!(
            !repo
                .apply_session_guide_effect("end-3", "digest-3", "gone", true, 1000)
                .unwrap()
        );
        assert!(
            !repo
                .apply_session_guide_effect("end-3", "digest-3", "gone", true, 1000)
                .unwrap()
        );
    }

    /// First freeze wins: two in-flight executions of the same operation
    /// that both pass the unfrozen check must not let the second silently
    /// replace the first — callers would receive divergent "verbatim"
    /// responses for one operation id.
    #[test]
    fn session_response_freeze_keeps_first() {
        use ltmrs_domain::session::FrozenToolResponse;
        let (repo, _dir) = repo_with_ns();
        let handle = SessionHandle::new(Uuid::from_u128(100));
        let channel = ChannelId::new(Uuid::from_u128(9));
        match repo
            .session_start_tx(
                "op-freeze",
                "digest-freeze",
                handle,
                channel,
                None,
                None,
                vec![],
                None,
                None,
                1000,
            )
            .unwrap()
        {
            SessionOp::Applied(h) => assert_eq!(h, handle),
            other => panic!("expected Applied, got {other:?}"),
        }
        let first = FrozenToolResponse {
            text: "first".to_string(),
            structured: None,
            is_error: false,
        };
        let second = FrozenToolResponse {
            text: "second".to_string(),
            structured: None,
            is_error: false,
        };
        repo.store_session_response("op-freeze", "digest-freeze", &first)
            .unwrap();
        repo.store_session_response("op-freeze", "digest-freeze", &second)
            .unwrap();
        let stored = repo
            .session_receipt("op-freeze")
            .unwrap()
            .expect("receipt must exist");
        assert_eq!(
            stored.response.as_ref().map(|r| r.text.as_str()),
            Some("first"),
            "second freeze must not replace the first"
        );
    }

    /// Re-review R3: a merge planned against stale source revisions rejects
    /// explicitly instead of discarding a concurrent update. Sources stay
    /// intact and no result appears.
    #[test]
    fn merge_with_stale_source_revisions_conflicts() {
        let (repo, _dir) = repo_with_ns();
        repo.put_guide(&test_guide("alpha")).unwrap();
        repo.put_guide(&test_guide("beta")).unwrap();
        let rev_alpha = repo.get_guide("alpha").unwrap().unwrap().entity_revision;
        let rev_beta = repo.get_guide("beta").unwrap().unwrap().entity_revision;
        // Concurrent update AFTER planning (practice bumps the revision).
        repo.practice_guide_idempotent(
            "practice-1",
            "digest-1",
            "alpha",
            "dev-tool",
            None,
            &[],
            &["new learning".to_string()],
            &[],
            None,
            1000,
        )
        .unwrap();
        let mut merged = test_guide("gamma");
        merged.usage_count = 2;
        let stale = vec![
            ("alpha".to_string(), rev_alpha),
            ("beta".to_string(), rev_beta),
        ];
        let err = repo
            .merge_guides_atomically(&["alpha".to_string(), "beta".to_string()], &stale, &merged)
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::RevisionConflict);
        // Nothing published: sources intact with the concurrent update, no
        // half-merge, no result.
        let alpha = repo.get_guide("alpha").unwrap().unwrap();
        assert!(alpha.learnings.contains(&"new learning".to_string()));
        assert!(repo.get_guide("beta").unwrap().is_some());
        assert!(repo.get_guide("gamma").unwrap().is_none());
        // Fresh revisions commit cleanly.
        let fresh = vec![
            (
                "alpha".to_string(),
                repo.get_guide("alpha").unwrap().unwrap().entity_revision,
            ),
            (
                "beta".to_string(),
                repo.get_guide("beta").unwrap().unwrap().entity_revision,
            ),
        ];
        repo.merge_guides_atomically(&["alpha".to_string(), "beta".to_string()], &fresh, &merged)
            .unwrap();
        assert!(repo.get_guide("gamma").unwrap().is_some());
    }

    /// Re-review R3: rename/forget of a missing guide fail without
    /// publishing anything (no half-rename, no half-forget).
    #[test]
    fn rename_forget_missing_guide_fail_cleanly() {
        let (repo, _dir) = repo_with_ns();
        repo.put_guide(&test_guide("solo")).unwrap();
        let mut renamed = test_guide("renamed");
        renamed.usage_count = 5;
        let err = repo
            .rename_guide_atomically(
                "missing",
                ltmrs_domain::id::EntityRevision::new(1),
                &renamed,
            )
            .unwrap_err();
        assert_eq!(err.code, DomainErrorCode::NotFound);
        assert!(repo.get_guide("solo").unwrap().is_some());
        assert!(repo.get_guide("renamed").unwrap().is_none());
        assert!(!repo.forget_guide_atomically("missing").unwrap());
        assert!(repo.get_guide("solo").unwrap().is_some());
    }

    /// Re-review P1-2: an ordinary guide write through the checked path
    /// rejects a stale revision instead of overwriting; create-if-absent
    /// refuses to clobber an existing guide.
    #[test]
    fn checked_guide_write_rejects_stale_revision() {
        let (repo, _dir) = repo_with_ns();
        repo.put_guide(&test_guide("g")).unwrap();
        let rev = repo.get_guide("g").unwrap().unwrap().entity_revision;
        // Concurrent writer bumps the revision (practice path).
        repo.practice_guide_idempotent("p1", "d1", "g", "dev-tool", None, &[], &[], &[], None, 1)
            .unwrap();
        let mut stale = repo.get_guide("g").unwrap().unwrap();
        // Simulate the stale plan: revision captured before the practice.
        let err = repo.put_guide_checked(Some(rev), &stale).unwrap_err();
        assert_eq!(err.code, DomainErrorCode::RevisionConflict);
        // Fresh revision commits.
        stale = repo.get_guide("g").unwrap().unwrap();
        let rev2 = stale.entity_revision;
        stale.description = "updated".into();
        repo.put_guide_checked(Some(rev2), &stale).unwrap();
        assert_eq!(repo.get_guide("g").unwrap().unwrap().description, "updated");
        // Create-if-absent refuses to overwrite.
        let err = repo.put_guide_checked(None, &test_guide("g")).unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
    }

    /// Re-review P1-1: a durability-barrier failure fails the ack, and a
    /// replay while the barrier still fails fails too — a visible receipt
    /// never fabricates durable success. Once the barrier works, the replay
    /// resolves to the recorded outcome without recounting.
    #[test]
    fn practice_replay_without_durability_fails() {
        let (repo, _dir) = repo_with_ns();
        let practice = || {
            repo.practice_guide_idempotent(
                "op-p",
                "digest-p",
                "git",
                "dev-tool",
                None,
                &[],
                &["learn it".to_string()],
                &[],
                Some(true),
                1000,
            )
        };
        // First execution: barrier fails → error, no ack. (The mutation +
        // receipt committed in-tx; only durability is unestablished.)
        repo.fault_injector().set_persist_failures(1);
        let err = practice().unwrap_err();
        assert!(
            err.message.contains("persist"),
            "barrier failure must fail loudly, got: {err:?}"
        );
        // Retry with the barrier STILL failing: must fail again, never flip
        // the in-tx receipt into a successful ack.
        repo.fault_injector().set_persist_failures(1);
        let err = practice().unwrap_err();
        assert!(
            err.message.contains("persist"),
            "replay without durability must fail loudly, got: {err:?}"
        );
        // Barrier healthy: replay resolves to the recorded outcome, counted
        // exactly once across all three attempts.
        let guide = practice().unwrap();
        assert_eq!(guide.usage_count, 1);
        assert_eq!(guide.success_count, 1);
        assert_eq!(repo.get_guide("git").unwrap().unwrap().usage_count, 1);
    }
}

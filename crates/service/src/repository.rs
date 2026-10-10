//! Hardened canonical repository over Fjall (AD-01 Option B).
//!
//! A single `apply` entry point centralizes command application, precondition
//! validation and atomic receipt storage. The receipt is committed in the same
//! Fjall transaction as the command it records — never in a later best-effort
//! write. Storage conflicts retry from a fresh snapshot; stale revisions are
//! surfaced, not blindly rebased; unknown commit outcomes are resolved via the
//! receipt and the same operation key.

use fjall::{KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::migrations::{
    MigrationOutcome, MigrationPlan, MigrationRunner, MigrationSafetyRules, REQUIRED_KEYSPACES,
};
use ltmrs_domain::command::{
    CommandReceipt, DomainError, DomainErrorCode, DomainResult, OperationScope, ReceiptOutcome,
    RetryNamespace,
};

use ltmrs_domain::id::{ChannelId, EntityId, FrontendId, OperationId, StoreGeneration};
use ltmrs_domain::projection::GenerationRecord;
mod admission;
#[cfg(test)]
mod admission_tests;
mod apply;
#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod gc_tests;
mod generations;
#[cfg(test)]
mod generations_tests;
mod guide_mutations;
mod guide_ops;
mod guides;
#[cfg(test)]
mod guides_tests;
mod memories;
#[cfg(test)]
mod memories_tests;
mod projections;
#[cfg(test)]
mod projections_tests;
mod session_tools;
mod sessions;
#[cfg(test)]
mod sessions_tests;
mod store;
#[cfg(test)]
mod store_tests;
mod suggestions;
#[cfg(test)]
mod suggestions_tests;
#[cfg(test)]
mod test_support;

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
    pub(crate) db: OptimisticTxDatabase,
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
    /// Frozen compatibility-tool results, keyed by the primary
    /// sub-command's scoped receipt key: the exact tool response bytes
    /// rendered at first execution. Replay returns them verbatim —
    /// no planning, no mutation, no recomputation.
    tool_results: OptimisticTxKeyspace,
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
    /// (a write guard is not re-entrant with a waiting writer). Poison
    /// policy: readers keep plain `unwrap` (fail-daemon) — this fence
    /// guards a genuine exclusion invariant, and durability itself comes
    /// from fjall transactions, so a poisoned fence must never be silently
    /// recovered into a possibly concurrent restore. Plain data locks
    /// (counters, registries, queues, hooks) recover via `into_inner`.
    restore_lock: std::sync::RwLock<()>,
    /// Live-operation pins per retry namespace (`frontend:epoch` →
    /// admitted-operation count). Admission pins its namespace so
    /// `gc_expired` cannot collect it (or its receipts) mid-operation;
    /// the pin releases when the admitted operation completes. Expired
    /// && unpinned collects as before.
    ns_pins: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
}

/// Proof that a tool operation's scope was admitted (RQ-06): the retry
/// namespace existed, belonged to the calling channel, and was within
/// TTL at admission, and stays pinned against GC expiry collection for
/// the admitted operation's lifetime. Continuation steps of one admitted
/// operation (response freeze, receipt replay, continuity claim, link
/// tracking) take this instead of a raw scope, so a namespace expiring
/// between primary commit and response finalization can never turn an
/// executed operation into an error. Constructible only via
/// [`CanonicalRepository::admit_scope`].
#[derive(Debug)]
pub struct AdmittedScope {
    scope: OperationScope,
    _pin: NamespacePin,
}

impl AdmittedScope {
    /// The admitted operation's scope (keys, digests, watermarks).
    pub fn scope(&self) -> &OperationScope {
        &self.scope
    }
}

/// One live-operation pin on a retry namespace. Cloned per admission;
/// dropping the last pin for a namespace makes it collectible again.
#[derive(Clone, Debug)]
struct NamespacePin {
    pins: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    key: String,
}

impl Drop for NamespacePin {
    fn drop(&mut self) {
        let mut map = self.pins.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
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
    #[serde(default)]
    scope: Option<OperationScope>,
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
    #[serde(default)]
    scope: Option<OperationScope>,
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
    #[serde(default)]
    scope: Option<OperationScope>,
}

/// Frozen compatibility-tool result: the exact response bytes rendered
/// at first execution, replayed verbatim (no planning, no mutation, no
/// recomputation). Keyed by the primary sub-command's scoped receipt
/// key, alongside its receipt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FrozenToolRecord {
    digest: String,
    response: ltmrs_domain::session::FrozenToolResponse,
    #[serde(default)]
    scope: Option<OperationScope>,
}

/// One read of a tool operation's replay state (see
/// `check_tool_replay`).
#[derive(Debug)]
pub enum ToolReplayStatus {
    /// Never ran (or collected): fresh execution proceeds.
    Miss,
    /// Ran and froze: return the response verbatim (barrier already run).
    Frozen(ltmrs_domain::session::FrozenToolResponse),
    /// Ran but froze nothing yet (crash window): rebuild from the
    /// recorded receipt, freeze, and return.
    Unfrozen(CommandReceipt),
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

        // Opened from REQUIRED_KEYSPACES (single source with migration
        // validation): adding a runtime keyspace means extending the
        // const, never a local literal. `meta` rides along (bound lazily
        // by the paths that version the store).
        let mut open = std::collections::HashMap::with_capacity(REQUIRED_KEYSPACES.len());
        for name in REQUIRED_KEYSPACES {
            open.insert(name, Self::keyspace(&db, name)?);
        }
        let mut take = |name: &str| open.remove(name).expect("required keyspace opened above");
        let memories = take("memories");
        let relations = take("relations");
        let receipts = take("receipts");
        let aliases = take("aliases");
        let namespaces = take("namespaces");
        let projections = take("projections");
        let generations = take("generations");
        let feedback_events = take("feedback_events");
        let guides = take("guides");
        let suggestions = take("suggestions");
        let guide_ops = take("guide_ops");
        let sessions = take("sessions");
        let session_ops = take("session_ops");
        let suggestion_ops = take("suggestion_ops");
        let tool_results = take("tool_results");

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
            tool_results,
            commit_hook: std::sync::Mutex::new(None),
            fault_injector,
            clock,
            restore_lock: std::sync::RwLock::new(()),
            ns_pins: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }

    pub(crate) fn keyspace(
        db: &OptimisticTxDatabase,
        name: &str,
    ) -> DomainResult<OptimisticTxKeyspace> {
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
        *self.commit_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// Fire the commit hook after a durable commit (best-effort: a panicking
    /// hook must never fail the already-committed write).
    fn fire_commit_hook(&self) {
        let hook = self
            .commit_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
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

    // ---- Snapshot-consistent reads ----

    // ---- Guide and suggestion storage (WP-09) ----
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
/// Mutation-watermark key for one interactive channel: every tool write —
/// canonical or direct — advances its own channel's counter, so
/// otherwise-disjoint concurrent agents never contend on a shared key
/// (Fjall OCC would turn independent work into transient conflicts).
/// Pre-scope-upgrade rows (`op_seq:{frontend}`, `op_seq:direct`) stop
/// advancing but stay in the total as a frozen offset: the watermark is
/// only ever differenced, never reset.
fn op_seq_key_for_scope(frontend: FrontendId, channel: ChannelId) -> String {
    format!("op_seq:{}:{}", frontend.as_uuid(), channel.as_uuid())
}

/// Mutation-watermark key for genuinely internal writers (imports,
/// maintenance, test fixtures): sharded per area, never the one global
/// key the per-channel counters replaced.
fn op_seq_key_system(area: &str) -> String {
    format!("op_seq:system:{area}")
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

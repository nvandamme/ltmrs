//! Hardened canonical repository over Fjall (AD-01 Option B).
//!
//! A single `apply` entry point centralizes command application, precondition
//! validation and atomic receipt storage. The receipt is committed in the same
//! Fjall transaction as the command it records — never in a later best-effort
//! write. Storage conflicts retry from a fresh snapshot; stale revisions are
//! surfaced, not blindly rebased; unknown commit outcomes are resolved via the
//! receipt and the same operation key.

use fjall::{
    KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace, OptimisticWriteTx, Readable,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::domain::command::{
    CommandContext, CommandReceipt, DomainCommand, DomainError, DomainErrorCode, DomainResult,
    ReceiptOutcome, RetryNamespace,
};
use crate::domain::export::CanonicalExport;
use crate::domain::id::{EntityId, FrontendId, ModelFingerprint, OperationId, StoreGeneration};
use crate::domain::memory::Memory;
use crate::domain::projection::{GenerationRecord, GenerationStatus};
use crate::domain::relation::Relation;
use crate::service::migrations::{
    MigrationOutcome, MigrationPlan, MigrationRunner, MigrationSafetyRules,
};
use crate::service::repository_internal::{CommandState, TxAction, apply_command};

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

    /// Consume one unknown-outcome fault. Returns true if a fault was injected.
    pub fn inject_unknown_outcome(&self) -> bool {
        Self::consume(&self.commit_unknown_outcomes)
    }

    /// Consume one migration fault. Returns true if a fault was injected.
    pub fn inject_migration_fault(&self) -> bool {
        Self::consume(&self.migration_failures)
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
    fault_injector: std::sync::Arc<FaultInjector>,
    clock: std::sync::Arc<dyn crate::domain::clock::Clock + Send + Sync>,
}

impl CanonicalRepository {
    /// Open a repository with the production wall-clock.
    pub fn open(base_path: &str) -> DomainResult<Self> {
        Self::open_with_clock(
            base_path,
            std::sync::Arc::new(crate::domain::clock::SystemClock),
        )
    }

    /// Open a repository with an explicit clock (for tests / deterministic time).
    pub fn open_with_clock(
        base_path: &str,
        clock: std::sync::Arc<dyn crate::domain::clock::Clock + Send + Sync>,
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
            fault_injector,
            clock,
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

    /// Issue a new retry namespace for a frontend with the default TTL.
    /// Called by the daemon when a frontend authenticates.
    pub fn issue_namespace(
        &self,
        frontend_id: FrontendId,
        now_millis: u64,
    ) -> DomainResult<RetryNamespace> {
        // Find the current epoch for this frontend.
        let snapshot = self.db.read_tx();
        let key = namespace_key(frontend_id);
        let current_epoch = match snapshot
            .get(&self.namespaces, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
        {
            Some(raw) => {
                let bytes = raw.as_ref();
                if bytes.len() == 8 {
                    let mut arr = [0u8; 8];
                    arr.copy_from_slice(bytes);
                    u64::from_le_bytes(arr) + 1
                } else {
                    1
                }
            }
            None => 1,
        };

        let ns = RetryNamespace::new(
            frontend_id,
            current_epoch,
            now_millis,
            DEFAULT_NAMESPACE_TTL_MILLIS,
        );

        // Persist the namespace and its fixed expiry.
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.namespaces, &key, current_epoch.to_le_bytes());
        let ns_raw = serde_json::to_vec(&ns)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.namespaces, ns_key(&ns), &ns_raw);
        match tx.commit() {
            Ok(Ok(())) => Ok(ns),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "namespace issue conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
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

    /// Garbage-collect expired namespaces and their receipts.
    /// Called periodically by the daemon. Returns the number of receipts removed.
    pub fn gc_expired(&self, now_millis: u64) -> DomainResult<usize> {
        // Find expired namespaces on a read snapshot.
        let snapshot = self.db.read_tx();
        let mut expired: Vec<RetryNamespace> = Vec::new();
        for kv in snapshot.iter(&self.namespaces) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let key_str = String::from_utf8_lossy(k.as_ref());
            if key_str.starts_with("ns:")
                && let Ok(ns) = decode::<RetryNamespace>(v.as_ref())
                && !ns.is_valid_at(now_millis)
            {
                expired.push(ns);
            }
        }

        if expired.is_empty() {
            return Ok(0);
        }

        // Remove expired namespaces and their receipts in one transaction.
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut removed = 0;
        for ns in &expired {
            // Remove all receipts issued under this retry_epoch.
            for kv in tx.iter(&self.receipts) {
                let (k, _) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let key_str = String::from_utf8_lossy(k.as_ref()).to_string();
                if receipt_matches_namespace(&key_str, ns) {
                    tx.remove(&self.receipts, &key_str);
                    removed += 1;
                }
            }
            let ns_key = ns_key(ns);
            tx.remove(&self.namespaces, &ns_key);
        }

        match tx.commit() {
            Ok(Ok(())) => Ok(removed),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "gc conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Validate that the command's retry namespace is still valid.
    /// Expired or unknown namespaces are refused as stale.
    fn validate_namespace(&self, ctx: &CommandContext) -> DomainResult<()> {
        let now = self.clock.now_millis();
        match self.lookup_namespace(ctx.frontend_id, ctx.retry_epoch)? {
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
            return self.replay_or_conflict(r, ctx);
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
                        Ok(Ok(())) => return Ok(receipt),
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
                    return Ok(receipt);
                }
            }
        }
        Err(DomainError::new(
            DomainErrorCode::Validation,
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
            Some(r) if r.request_digest == ctx.request_digest => Ok(r),
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
        if existing.request_digest == ctx.request_digest {
            Ok(existing)
        } else {
            Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ))
        }
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
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "generation switch conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Stage a blue-green generation build (design §8.2): allocates the next
    /// generation number, snapshots the watermark denominator (recallable
    /// canonical memories now), and records the build fingerprint. Exactly
    /// one pipeline (Staged/Building/Ready) may exist at a time. The active
    /// pointer is untouched — staging is never observable to readers.
    pub fn stage_generation(&self, fingerprint: ModelFingerprint) -> DomainResult<StoreGeneration> {
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
        };
        tx.insert(
            &self.generations,
            generation_key(next),
            serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        );
        match tx.commit() {
            Ok(Ok(())) => Ok(next),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "generation staging conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Report build progress for a staged generation. Promotes Staged to
    /// Building and to Ready once the watermark is met (projected >=
    /// desired). Ready is sticky upward only through this path — a lower
    /// recount moves it back to Building rather than silently holding Ready.
    pub fn note_generation_progress(
        &self,
        generation: StoreGeneration,
        projected: u64,
    ) -> DomainResult<GenerationRecord> {
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
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "generation progress conflicted",
            )),
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
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "generation abandon conflicted",
            )),
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
    /// Mid-build-write window (explicit, not closed here): a canonical write
    /// that lands after the build's final pass but before activation is
    /// published by the old generation's worker only. The operator protocol
    /// is build → quiesce → final rebuild → note → activate; per-generation
    /// pending work that closes the window structurally is a follow-up.
    /// The watermark numerator is operator-reported; the projector's
    /// `rebuild()` return is its honest source. Table-measured verification
    /// of the numerator is a follow-up.
    pub fn activate_generation(&self, generation: StoreGeneration) -> DomainResult<()> {
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
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
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
    ) -> DomainResult<Option<crate::domain::projection::ProjectionJob>> {
        let snapshot = self.db.read_tx();
        let key = id.as_uuid().to_string();
        let raw = snapshot
            .get(&self.projections, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode::<crate::domain::projection::ProjectionJob>(v.as_ref()))
            .transpose()
    }

    /// List every pending projection job (the worker's durable work queue).
    pub fn projection_jobs(&self) -> DomainResult<Vec<crate::domain::projection::ProjectionJob>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.projections) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<crate::domain::projection::ProjectionJob>(
                v.as_ref(),
            )?);
        }
        Ok(out)
    }

    /// Compare-and-clear a projection job. Returns true only if the stored job
    /// still carries exactly this seq — a stale worker (whose desired revision
    /// was superseded, or whose memory was forgotten) gets false and leaves no
    /// trace. Retries on storage conflict from a fresh snapshot.
    pub fn acknowledge_projection(&self, id: EntityId, seq: u64) -> DomainResult<bool> {
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
                    let job: crate::domain::projection::ProjectionJob = decode(raw.as_ref())?;
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
        Err(DomainError::new(
            DomainErrorCode::Validation,
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
        desired_document_revision: crate::domain::id::DocumentRevision,
        seq: u64,
        is_tombstone: bool,
    ) -> DomainResult<()> {
        let job = crate::domain::projection::ProjectionJob {
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
                Ok(Ok(())) => return Ok(()),
                Ok(Err(_conflict)) => continue,
                Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
            }
        }
        Err(DomainError::new(
            DomainErrorCode::Validation,
            "max transaction retries exceeded",
        ))
    }

    pub fn feedback_events(&self) -> DomainResult<Vec<crate::domain::session::FeedbackEvent>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.feedback_events) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<crate::domain::session::FeedbackEvent>(v.as_ref())?);
        }
        Ok(out)
    }

    // ---- Guide and suggestion storage (WP-09) ----

    /// All guides from a single snapshot.
    pub fn get_guides(&self) -> DomainResult<Vec<crate::domain::guide::Guide>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.guides) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<crate::domain::guide::Guide>(v.as_ref())?);
        }
        Ok(out)
    }

    /// A single guide by name (case-insensitive, matching upstream COLLATE NOCASE).
    pub fn get_guide(&self, name: &str) -> DomainResult<Option<crate::domain::guide::Guide>> {
        let target = name.to_lowercase();
        Ok(self
            .get_guides()?
            .into_iter()
            .find(|g| g.name.eq_ignore_ascii_case(&target)))
    }

    /// Store a guide (keyed by lowercased name).
    pub fn put_guide(&self, guide: &crate::domain::guide::Guide) -> DomainResult<()> {
        let key = guide.name.to_lowercase();
        let raw = serde_json::to_vec(guide)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.guides, &key, raw.as_slice());
        match tx.commit() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "guide write conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Write a memory record directly (WP-09 guide_distill side-effect on a
    /// fragment's related_guides / distill_candidate). Bypasses the command
    /// gateway: used only for the compatibility adapter's derived writes, which
    /// are not themselves user-addressable operations.
    pub fn put_memory_direct(&self, memory: &Memory) -> DomainResult<()> {
        let key = memory.id.as_uuid().to_string();
        let raw = serde_json::to_vec(memory)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.memories, &key, raw.as_slice());
        match tx.commit() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "memory write conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Delete a guide by name (case-insensitive). Returns true if removed.
    pub fn delete_guide(&self, name: &str) -> DomainResult<bool> {
        let key = name.to_lowercase();
        let snapshot = self.db.read_tx();
        let exists = snapshot
            .get(&self.guides, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            .is_some();
        if !exists {
            return Ok(false);
        }
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.remove(&self.guides, &key);
        match tx.commit() {
            Ok(Ok(())) => Ok(true),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "guide delete conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// All suggestions from a single snapshot.
    pub fn get_suggestions(&self) -> DomainResult<Vec<crate::domain::session::Suggestion>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.suggestions) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<crate::domain::session::Suggestion>(v.as_ref())?);
        }
        Ok(out)
    }

    /// A single suggestion by ID.
    pub fn get_suggestion(
        &self,
        id: u64,
    ) -> DomainResult<Option<crate::domain::session::Suggestion>> {
        Ok(self.get_suggestions()?.into_iter().find(|s| s.id == id))
    }

    /// Store a suggestion (keyed by ID).
    pub fn put_suggestion(
        &self,
        suggestion: &crate::domain::session::Suggestion,
    ) -> DomainResult<()> {
        let key = suggestion.id.to_string();
        let raw = serde_json::to_vec(suggestion)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.suggestions, &key, raw.as_slice());
        match tx.commit() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "suggestion write conflicted",
            )),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Next suggestion ID (max existing + 1, or 1 if none).
    pub fn next_suggestion_id(&self) -> DomainResult<u64> {
        Ok(self
            .get_suggestions()?
            .iter()
            .map(|s| s.id)
            .max()
            .unwrap_or(0)
            + 1)
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
        frontend_id: crate::domain::id::FrontendId::new(
            uuid::Uuid::parse_str(&record.frontend_id)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
        ),
        channel_id: crate::domain::id::ChannelId::new(
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
    use crate::domain::command::{DomainCommand, ForgetMode};
    use crate::domain::id::{EntityId, ModelFingerprint, StoreGeneration};
    use crate::domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
    use crate::domain::relation::{Relation, RelationType};
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
            entity_revision: crate::domain::id::EntityRevision::new(1),
            document_revision: crate::domain::id::DocumentRevision::new(1),
            eligibility_revision: crate::domain::id::EligibilityRevision::new(1),
            created_at: crate::domain::memory::Instant::new(1),
            updated_at: crate::domain::memory::Instant::new(1),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn ctx(op_num: u64, digest: &str) -> CommandContext {
        CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: crate::domain::id::FrontendId::new(Uuid::from_u128(1)),
            channel_id: crate::domain::id::ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: OperationId::new(Uuid::from_u128(op_num as u128)),
            request_digest: digest.to_string(),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch: 1,
        }
    }

    /// Open a repo and issue a namespace for frontend 1 at epoch 1.
    fn repo_with_ns() -> (CanonicalRepository, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        // Frozen clock at 1000 so namespace validity checks are deterministic
        // and consistent with the issue_namespace time below.
        let clock = std::sync::Arc::new(crate::domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        let ns = repo
            .issue_namespace(crate::domain::id::FrontendId::new(Uuid::from_u128(1)), 1000)
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
        let patch = crate::domain::command::MemoryPatch {
            title: Some("new".into()),
            ..Default::default()
        };
        let err = repo
            .apply(
                &ctx(2, "d2"),
                &DomainCommand::UpdateMemory {
                    id: eid(1),
                    expected_revision: Some(crate::domain::id::EntityRevision::new(99)),
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

    use crate::domain::projection::GenerationStatus;

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
        let clock = std::sync::Arc::new(crate::domain::clock::FrozenClock::new(1000));
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
        Relation::new(id, s, t, ty, None, crate::domain::memory::Instant::new(1))
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
        let fe = crate::domain::id::FrontendId::new(Uuid::from_u128(1));
        let ns1 = repo.issue_namespace(fe, 1000).unwrap();
        let ns2 = repo.issue_namespace(fe, 2000).unwrap();
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
        let clock = std::sync::Arc::new(crate::domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        let fe = crate::domain::id::FrontendId::new(Uuid::from_u128(1));
        let ns = repo.issue_namespace(fe, 1000).unwrap();

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
        let clock = std::sync::Arc::new(crate::domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        // Two frontends, each at epoch 1 (same retry_epoch value), but issued
        // at different times so A expires before B.
        let fe_a = crate::domain::id::FrontendId::new(Uuid::from_u128(1));
        let fe_b = crate::domain::id::FrontendId::new(Uuid::from_u128(2));
        let ns_a = repo.issue_namespace(fe_a, 1000).unwrap();
        let ns_b = repo.issue_namespace(fe_b, 2000).unwrap();
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
        use crate::service::migrations::{
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
        let patch = crate::domain::command::MemoryPatch {
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
        let clock = std::sync::Arc::new(crate::domain::clock::FrozenClock::new(1000));
        {
            let repo =
                CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
            let fe = crate::domain::id::FrontendId::new(Uuid::from_u128(1));
            let ns = repo.issue_namespace(fe, 1000).unwrap();
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
}

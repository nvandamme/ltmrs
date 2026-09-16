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

use crate::canonical::repository_internal::{CommandState, TxAction, apply_command};
use crate::domain::command::{
    CommandContext, CommandReceipt, DomainCommand, DomainError, DomainErrorCode, DomainResult,
    ReceiptOutcome,
};
use crate::domain::export::CanonicalExport;
use crate::domain::id::{EntityId, OperationId, StoreGeneration};
use crate::domain::memory::Memory;
use crate::domain::relation::Relation;

const MAX_RETRIES: u32 = 8;
const CURRENT_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ReceiptRecord {
    store_generation: u64,
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

pub struct CanonicalRepository {
    db: OptimisticTxDatabase,
    memories: OptimisticTxKeyspace,
    relations: OptimisticTxKeyspace,
    receipts: OptimisticTxKeyspace,
    aliases: OptimisticTxKeyspace,
}

impl CanonicalRepository {
    pub fn open(base_path: &str) -> DomainResult<Self> {
        let db = OptimisticTxDatabase::builder(base_path)
            .open()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let memories = Self::keyspace(&db, "memories")?;
        let relations = Self::keyspace(&db, "relations")?;
        let receipts = Self::keyspace(&db, "receipts")?;
        let aliases = Self::keyspace(&db, "aliases")?;
        let meta = Self::keyspace(&db, "meta")?;

        Self::check_schema_version(&db, &meta)?;

        Ok(Self {
            db,
            memories,
            relations,
            receipts,
            aliases,
        })
    }

    fn keyspace(db: &OptimisticTxDatabase, name: &str) -> DomainResult<OptimisticTxKeyspace> {
        db.keyspace(name, KeyspaceCreateOptions::default)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
    }

    fn check_schema_version(
        db: &OptimisticTxDatabase,
        meta: &OptimisticTxKeyspace,
    ) -> DomainResult<()> {
        let snapshot = db.read_tx();
        let raw = snapshot
            .get(meta, "schema_version")
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        match raw {
            None => {
                let mut tx = db
                    .write_tx()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                tx.insert(meta, "schema_version", CURRENT_SCHEMA_VERSION.to_le_bytes());
                match tx.commit() {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err(DomainError::new(
                        DomainErrorCode::Validation,
                        "schema version write conflicted",
                    )),
                    Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
                }
            }
            Some(bytes) => {
                let version = u64::from_le_bytes(bytes.as_ref().try_into().map_err(|_| {
                    DomainError::new(DomainErrorCode::Validation, "corrupt schema version")
                })?);
                if version > CURRENT_SCHEMA_VERSION {
                    return Err(DomainError::new(
                        DomainErrorCode::Validation,
                        format!(
                            "refusing to open: store schema {version} is newer than supported {CURRENT_SCHEMA_VERSION}"
                        ),
                    ));
                }
                Ok(())
            }
        }
    }

    /// Centralized command application: idempotent, atomic, precondition-checked.
    pub fn apply(&self, ctx: &CommandContext, cmd: &DomainCommand) -> DomainResult<CommandReceipt> {
        // Fast-path idempotency on a read-only snapshot.
        if let Some(r) = self.lookup_receipt(ctx.store_generation, ctx.operation_id)? {
            return self.replay_or_conflict(r, ctx);
        }

        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            match self.apply_in_tx(&mut tx, ctx, cmd)? {
                TxAction::Commit(receipt) => match tx.commit() {
                    Ok(Ok(())) => return Ok(receipt),
                    Ok(Err(_conflict)) => {
                        // Storage conflict: retry from a fresh snapshot.
                        continue;
                    }
                    Err(io_err) => {
                        // Unknown commit outcome: resolve via the receipt.
                        return self.resolve_unknown_outcome(ctx, io_err);
                    }
                },
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
        let key = receipt_key(ctx.store_generation, ctx.operation_id);
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
        let mut state = CommandState::new(tx, &self.memories, &self.relations, &self.aliases);
        let outcome = apply_command(&mut state, ctx, cmd)?;

        // Store the receipt atomically with the command.
        let receipt = CommandReceipt {
            operation_id: ctx.operation_id,
            store_generation: ctx.store_generation,
            frontend_id: ctx.frontend_id,
            channel_id: ctx.channel_id,
            request_digest: ctx.request_digest.clone(),
            outcome: outcome.clone(),
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
        match self.lookup_receipt(ctx.store_generation, ctx.operation_id)? {
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

    pub fn lookup_receipt(
        &self,
        generation: StoreGeneration,
        op: OperationId,
    ) -> DomainResult<Option<CommandReceipt>> {
        let key = receipt_key(generation, op);
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

fn receipt_key(generation: StoreGeneration, op: OperationId) -> String {
    format!("{}:{}", generation.as_u64(), op.as_uuid())
}

fn encode_receipt(receipt: &CommandReceipt) -> DomainResult<Vec<u8>> {
    let record = ReceiptRecord {
        store_generation: receipt.store_generation.as_u64(),
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
    use crate::domain::id::{EntityId, StoreGeneration};
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
        }
    }

    #[test]
    fn apply_add_memory_stores_receipt_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
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
            .lookup_receipt(StoreGeneration::FIRST, r.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.operation_id, r.operation_id);
        assert_eq!(stored.request_digest, "d1");
    }

    #[test]
    fn idempotent_replay_returns_recorded_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
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

    fn rel(id: EntityId, s: EntityId, t: EntityId, ty: RelationType) -> Relation {
        Relation::new(id, s, t, ty, None, crate::domain::memory::Instant::new(1))
    }

    #[test]
    fn forget_transitions_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let repo =
            std::sync::Arc::new(CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap());

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
}

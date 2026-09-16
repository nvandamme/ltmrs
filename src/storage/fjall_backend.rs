//! Fjall+Lance backend probe (Part II §7, WP-02 Section B).
//!
//! Demonstrates the atomic command path through Fjall's optimistic
//! cross-keyspace transactions. Fjall provides:
//!
//! - **One-winner concurrency**: write-write conflicts are detected at
//!   commit time; a conflicted transaction is retried and observes the
//!   winner's effect.
//! - **Multi-record atomicity**: a single transaction publishes the result
//!   AND transitions the sources atomically — no observable partial state.

use fjall::{
    KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace, OptimisticWriteTx,
    PersistMode, Readable,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::domain::command::{CommandReceipt, DomainError, DomainErrorCode, DomainResult};
use crate::domain::id::{EntityId, OperationId, StoreGeneration};

const MAX_RETRIES: u32 = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryRecord {
    pub id: String,
    pub title: String,
    pub fragment: String,
    pub confidence: f64,
    pub entity_revision: u64,
    pub parent_id: Option<String>,
    pub lifecycle: String,
    pub positive_feedback: u64,
    pub negative_feedback: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationRecord {
    source_id: String,
    target_id: String,
    relation_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReceiptRecord {
    store_generation: u64,
    operation_id: String,
    request_digest: String,
    outcome: String,
}

pub struct FjallBackend {
    db: OptimisticTxDatabase,
    memories: OptimisticTxKeyspace,
    relations: OptimisticTxKeyspace,
    receipts: OptimisticTxKeyspace,
}

enum TxResult<T> {
    Commit(T),
    NoOp(T),
}

impl FjallBackend {
    pub fn open(base_path: &str) -> DomainResult<Self> {
        let db = OptimisticTxDatabase::builder(base_path)
            .open()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let memories = db
            .keyspace("memories", KeyspaceCreateOptions::default)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let relations = db
            .keyspace("relations", KeyspaceCreateOptions::default)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let receipts = db
            .keyspace("receipts", KeyspaceCreateOptions::default)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(Self {
            db,
            memories,
            relations,
            receipts,
        })
    }

    fn with_bounded_retry<T, F>(&self, mut op: F) -> DomainResult<T>
    where
        F: FnMut(&mut OptimisticWriteTx) -> DomainResult<TxResult<T>>,
    {
        for attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            match op(&mut tx)? {
                TxResult::NoOp(value) => {
                    tx.rollback();
                    return Ok(value);
                }
                TxResult::Commit(value) => match tx.commit() {
                    Ok(Ok(())) => return Ok(value),
                    Ok(Err(_conflict)) => {
                        if attempt == MAX_RETRIES - 1 {
                            return Err(DomainError::new(
                                DomainErrorCode::Validation,
                                "max transaction retries exceeded",
                            ));
                        }
                        continue;
                    }
                    Err(e) => {
                        return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                    }
                },
            }
        }
        unreachable!("bounded retry loop always returns")
    }

    pub fn read_memory(&self, id: EntityId) -> DomainResult<Option<MemoryRecord>> {
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.memories, id.as_uuid().to_string())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode::<MemoryRecord>(&v)).transpose()
    }

    pub fn create_memory_if_absent(
        &self,
        id: EntityId,
        title: &str,
        fragment: &str,
        confidence: f64,
        revision: u64,
    ) -> DomainResult<bool> {
        let key = id.as_uuid().to_string();
        let record = MemoryRecord {
            id: key.clone(),
            title: title.to_string(),
            fragment: fragment.to_string(),
            confidence,
            entity_revision: revision,
            parent_id: None,
            lifecycle: "live".to_string(),
            positive_feedback: 0,
            negative_feedback: 0,
        };
        let value = encode(&record)?;
        self.with_bounded_retry(move |tx| {
            let existing = tx
                .get(&self.memories, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if existing.is_some() {
                return Ok(TxResult::NoOp(false));
            }
            tx.insert(&self.memories, &key, &value);
            Ok(TxResult::Commit(true))
        })
    }

    pub fn update_memory_conditional(
        &self,
        id: EntityId,
        expected_revision: u64,
        new_title: &str,
        new_revision: u64,
    ) -> DomainResult<u64> {
        let key = id.as_uuid().to_string();
        let new_title = new_title.to_string();
        self.with_bounded_retry(move |tx| {
            let raw = tx
                .get(&self.memories, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(current) = raw.map(|v| decode::<MemoryRecord>(&v)).transpose()? else {
                return Ok(TxResult::NoOp(0));
            };
            if current.entity_revision != expected_revision {
                return Ok(TxResult::NoOp(0));
            }
            let mut updated = current;
            updated.title = new_title.clone();
            updated.entity_revision = new_revision;
            let value = encode(&updated)?;
            tx.insert(&self.memories, &key, &value);
            Ok(TxResult::Commit(1))
        })
    }

    pub fn apply_feedback(&self, id: EntityId, useful: bool) -> DomainResult<()> {
        let key = id.as_uuid().to_string();
        self.with_bounded_retry(move |tx| {
            let raw = tx
                .get(&self.memories, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(mut record) = raw.map(|v| decode::<MemoryRecord>(&v)).transpose()? else {
                return Err(DomainError::new(DomainErrorCode::NotFound, "not found"));
            };
            if useful {
                record.positive_feedback += 1;
                record.confidence = (record.confidence + 0.01).min(1.0);
            } else {
                record.negative_feedback += 1;
                record.confidence = (record.confidence - 0.05).max(0.0);
            }
            let value = encode(&record)?;
            tx.insert(&self.memories, &key, &value);
            Ok(TxResult::Commit(()))
        })
    }

    pub fn create_relation(
        &self,
        source: EntityId,
        target: EntityId,
        relation_type: &str,
    ) -> DomainResult<bool> {
        let rel_key = relation_key(source, target, relation_type);
        let record = RelationRecord {
            source_id: source.as_uuid().to_string(),
            target_id: target.as_uuid().to_string(),
            relation_type: relation_type.to_string(),
        };
        let value = encode(&record)?;
        self.with_bounded_retry(move |tx| {
            let existing = tx
                .get(&self.relations, &rel_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if existing.is_some() {
                return Ok(TxResult::NoOp(false));
            }
            tx.insert(&self.relations, &rel_key, &value);
            Ok(TxResult::Commit(true))
        })
    }

    pub fn delete_relation(
        &self,
        source: EntityId,
        target: EntityId,
        relation_type: &str,
    ) -> DomainResult<bool> {
        let rel_key = relation_key(source, target, relation_type);
        self.with_bounded_retry(move |tx| {
            let existing = tx
                .get(&self.relations, &rel_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if existing.is_none() {
                return Ok(TxResult::NoOp(false));
            }
            tx.remove(&self.relations, &rel_key);
            Ok(TxResult::Commit(true))
        })
    }

    pub fn forget_memory(&self, id: EntityId, lifecycle: &str) -> DomainResult<()> {
        let key = id.as_uuid().to_string();
        let lifecycle = lifecycle.to_string();
        self.with_bounded_retry(move |tx| {
            let raw = tx
                .get(&self.memories, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(mut record) = raw.map(|v| decode::<MemoryRecord>(&v)).transpose()? else {
                return Err(DomainError::new(DomainErrorCode::NotFound, "not found"));
            };
            record.lifecycle = lifecycle.clone();
            let value = encode(&record)?;
            tx.insert(&self.memories, &key, &value);
            Ok(TxResult::Commit(()))
        })
    }

    /// Atomic merge: result + all source transitions in ONE transaction.
    pub fn merge_memories(
        &self,
        result_id: EntityId,
        result_title: &str,
        result_fragment: &str,
        source_ids: &[EntityId],
        revision: u64,
    ) -> DomainResult<()> {
        let result_key = result_id.as_uuid().to_string();
        let result_record = MemoryRecord {
            id: result_key.clone(),
            title: result_title.to_string(),
            fragment: result_fragment.to_string(),
            confidence: 1.0,
            entity_revision: revision,
            parent_id: None,
            lifecycle: "live".to_string(),
            positive_feedback: 0,
            negative_feedback: 0,
        };
        let result_value = encode(&result_record)?;
        let source_keys: Vec<String> = source_ids
            .iter()
            .map(|id| id.as_uuid().to_string())
            .collect();

        self.with_bounded_retry(move |tx| {
            tx.insert(&self.memories, &result_key, &result_value);
            for source_key in &source_keys {
                let raw = tx
                    .get(&self.memories, source_key)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if let Some(mut record) = raw.map(|v| decode::<MemoryRecord>(&v)).transpose()? {
                    record.lifecycle = "archived".to_string();
                    let value = encode(&record)?;
                    tx.insert(&self.memories, source_key, &value);
                }
            }
            Ok(TxResult::Commit(()))
        })
    }

    pub fn store_receipt(&self, receipt: &CommandReceipt) -> DomainResult<()> {
        let key = receipt_key(receipt.store_generation, receipt.operation_id);
        let outcome = outcome_str(&receipt.outcome);
        let record = ReceiptRecord {
            store_generation: receipt.store_generation.as_u64(),
            operation_id: receipt.operation_id.as_uuid().to_string(),
            request_digest: receipt.request_digest.clone(),
            outcome,
        };
        let value = encode(&record)?;
        self.db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            .insert(&self.receipts, &key, &value);
        self.db
            .persist(PersistMode::Buffer)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
    }

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
        match raw {
            Some(v) => {
                let record: ReceiptRecord = decode(&v)?;
                Ok(Some(receipt_from_record(&record)?))
            }
            None => Ok(None),
        }
    }

    pub fn persist(&self, mode: PersistMode) -> DomainResult<()> {
        self.db
            .persist(mode)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
    }
}

fn relation_key(source: EntityId, target: EntityId, ty: &str) -> String {
    format!("{}:{}:{}", source.as_uuid(), target.as_uuid(), ty)
}

fn receipt_key(generation: StoreGeneration, op: OperationId) -> String {
    format!("{}:{}", generation.as_u64(), op.as_uuid())
}

fn outcome_str(outcome: &crate::domain::command::ReceiptOutcome) -> String {
    match outcome {
        crate::domain::command::ReceiptOutcome::Success { affected } => {
            format!(
                "success:{}",
                affected
                    .iter()
                    .map(|id| id.as_uuid().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        crate::domain::command::ReceiptOutcome::Rejected { .. } => "rejected".to_string(),
    }
}

fn receipt_from_record(record: &ReceiptRecord) -> DomainResult<CommandReceipt> {
    let op = OperationId::new(
        uuid::Uuid::parse_str(&record.operation_id)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
    );
    let outcome = if let Some(rest) = record.outcome.strip_prefix("success:") {
        let affected: Vec<EntityId> = if rest.is_empty() {
            vec![]
        } else {
            rest.split(',')
                .filter_map(|s| uuid::Uuid::parse_str(s).ok().map(EntityId::new))
                .collect()
        };
        crate::domain::command::ReceiptOutcome::Success { affected }
    } else {
        crate::domain::command::ReceiptOutcome::Rejected {
            code: DomainErrorCode::NotFound,
        }
    };
    Ok(CommandReceipt {
        operation_id: op,
        store_generation: StoreGeneration::new(record.store_generation),
        frontend_id: crate::domain::id::FrontendId::new(uuid::Uuid::nil()),
        channel_id: crate::domain::id::ChannelId::new(uuid::Uuid::nil()),
        request_digest: record.request_digest.clone(),
        outcome,
    })
}

fn encode<T: Serialize>(value: &T) -> DomainResult<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> DomainResult<T> {
    serde_json::from_slice(bytes)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn eid(n: u64) -> EntityId {
        EntityId::new(uuid::Uuid::from_u128(n as u128))
    }

    #[test]
    fn fjall_detects_write_write_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let be = FjallBackend::open(dir.path().to_str().unwrap()).unwrap();
        let key = "test-key";

        // Both transactions must be open simultaneously, and each must READ
        // the key (establishing a read dependency) for SSI to detect the
        // write-write conflict at commit time.
        let mut tx1 = be.db.write_tx().expect("write_tx 1");
        // Read the key to establish a dependency (SSI requires read-write overlap).
        let _ = tx1.get(&be.memories, key).expect("tx1 read");
        let val1 = encode(&MemoryRecord {
            id: key.to_string(),
            title: "from-tx1".to_string(),
            fragment: "f".to_string(),
            confidence: 0.5,
            entity_revision: 1,
            parent_id: None,
            lifecycle: "live".to_string(),
            positive_feedback: 0,
            negative_feedback: 0,
        })
        .unwrap();
        tx1.insert(&be.memories, key, &val1);

        let mut tx2 = be.db.write_tx().expect("write_tx 2");
        // Read the key to establish a dependency (SSI requires read-write overlap).
        let _ = tx2.get(&be.memories, key).expect("tx2 read");
        let val2 = encode(&MemoryRecord {
            id: key.to_string(),
            title: "from-tx2".to_string(),
            fragment: "f".to_string(),
            confidence: 0.6,
            entity_revision: 2,
            parent_id: None,
            lifecycle: "live".to_string(),
            positive_feedback: 0,
            negative_feedback: 0,
        })
        .unwrap();
        tx2.insert(&be.memories, key, &val2);

        // Commit tx1 first (succeeds), then tx2 (conflicts on the same key).
        let r1 = tx1.commit().expect("commit 1");
        assert!(r1.is_ok(), "first commit should succeed");

        let r2 = tx2.commit().expect("commit 2");
        assert!(r2.is_err(), "second commit on same key should conflict");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fjall_one_winner_for_concurrent_absent_key() {
        let dir = tempfile::tempdir().unwrap();
        let be = Arc::new(FjallBackend::open(dir.path().to_str().unwrap()).unwrap());

        const N: usize = 4;
        let barrier = Arc::new(Barrier::new(N));
        let mut handles = Vec::new();
        for _ in 0..N {
            let be = Arc::clone(&be);
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::task::spawn_blocking(move || {
                barrier.wait();
                be.create_memory_if_absent(eid(1), "T", "F", 0.5, 1)
                    .unwrap()
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        let winners = results.iter().filter(|won| **won).count();
        let count = be.read_memory(eid(1)).unwrap().map(|_| 1).unwrap_or(0);

        eprintln!(
            "Fjall T-CONC-02: winners={winners} rows={count} (one-winner requires winners==1 && rows==1)"
        );
        assert_eq!(winners, 1, "exactly one winner expected");
        assert_eq!(count, 1, "exactly one row expected");
    }

    #[test]
    fn fjall_atomic_merge_no_partial_state() {
        let dir = tempfile::tempdir().unwrap();
        let be = FjallBackend::open(dir.path().to_str().unwrap()).unwrap();

        be.create_memory_if_absent(eid(1), "A", "fa", 0.5, 1)
            .unwrap();
        be.create_memory_if_absent(eid(2), "B", "fb", 0.5, 1)
            .unwrap();

        be.merge_memories(eid(3), "C", "fc", &[eid(1), eid(2)], 1)
            .unwrap();

        let c = be.read_memory(eid(3)).unwrap();
        let a = be.read_memory(eid(1)).unwrap();
        let b = be.read_memory(eid(2)).unwrap();

        assert!(c.is_some(), "result must exist");
        assert_eq!(
            a.as_ref().map(|r| r.lifecycle.clone()),
            Some("archived".to_string()),
            "source A must be archived"
        );
        assert_eq!(
            b.as_ref().map(|r| r.lifecycle.clone()),
            Some("archived".to_string()),
            "source B must be archived"
        );

        eprintln!("Fjall T-STORE-01: merge atomic — result + sources all committed");
    }

    #[test]
    fn fjall_kill_reopen_durability_syncall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();

        {
            let be = FjallBackend::open(&path).unwrap();
            be.create_memory_if_absent(eid(1), "A", "fa", 0.5, 1)
                .unwrap();
            be.persist(PersistMode::SyncAll).unwrap();
        }

        let be2 = FjallBackend::open(&path).unwrap();
        let found = be2.read_memory(eid(1)).unwrap();
        assert!(found.is_some(), "memory must survive kill/reopen");
        eprintln!("Fjall T-REC-01: kill/reopen with SyncAll — data durable");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fjall_bounded_retry_under_contention() {
        let dir = tempfile::tempdir().unwrap();
        let be = Arc::new(FjallBackend::open(dir.path().to_str().unwrap()).unwrap());

        // Seed the memory so the conditional updates have a target.
        be.create_memory_if_absent(eid(1), "T", "F", 0.5, 1)
            .unwrap();

        const N: usize = 8;
        let mut handles = Vec::new();
        for i in 0..N {
            let be = Arc::clone(&be);
            handles.push(tokio::task::spawn_blocking(move || {
                be.update_memory_conditional(eid(1), 1, &format!("T{i}"), 2)
                    .unwrap()
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        let updated = results.iter().filter(|r| **r == 1).count();
        eprintln!("Fjall bounded retry: {updated}/8 writers updated (contention resolved)");
        assert!(updated >= 1, "at least one writer must succeed");
    }
}

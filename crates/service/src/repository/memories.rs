//! Memory / relation / ID reads (moved verbatim from `repository.rs`).

use fjall::Readable;

use super::{
    CanonicalRepository, MAX_RETRIES, decode, decode_receipt, op_seq_key_system, receipt_key,
};
use ltmrs_domain::command::{CommandReceipt, DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::{EntityId, FrontendId, OperationId, StoreGeneration};
use ltmrs_domain::memory::Memory;
use ltmrs_domain::relation::Relation;

impl CanonicalRepository {
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
        let seq_key = op_seq_key_system("memory");
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
}

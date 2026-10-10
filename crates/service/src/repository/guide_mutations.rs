//! Atomic guide mutations: delete, renames, merges, forgets (moved verbatim from `repository.rs`).

use fjall::{OptimisticWriteTx, Readable};

use super::{CanonicalRepository, MAX_RETRIES, op_seq_key_system};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::memory::Memory;

impl CanonicalRepository {
    /// Delete a guide by name (case-insensitive). Returns true if removed.
    /// Blind delete (no reference cleanup or receipt): tests and offline
    /// repair only — production forgets go through `guide_mutation_idempotent`.
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
        let seq_key = op_seq_key_system("guide");
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
        let seq_key = op_seq_key_system("guide");
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
        let seq_key = op_seq_key_system("guide");
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
        let seq_key = op_seq_key_system("guide");
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
    pub(crate) fn merge_guides_apply_tx(
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
        let seq_key = op_seq_key_system("guide");
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
    pub(crate) fn rename_guide_apply_tx(
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
        let seq_key = op_seq_key_system("guide");
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
    pub(crate) fn forget_guide_apply_tx(
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
}

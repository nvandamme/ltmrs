//! Embedding-generation lifecycle (staging, activation, rollback) (moved verbatim from `repository.rs`).

use fjall::{OptimisticWriteTx, Readable};

use super::{CanonicalRepository, decode_generation, generation_key};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::{ModelFingerprint, StoreGeneration};
use ltmrs_domain::memory::Memory;
use ltmrs_domain::projection::{GenerationRecord, GenerationStatus};

impl CanonicalRepository {
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
        Self::decode_meta_generation(raw.as_ref().map(|v| v.as_ref()))
    }

    /// Decode the raw meta generation pointer: missing means a fresh store
    /// (FIRST); present-but-truncated is corruption and fails closed —
    /// generation identity is a fencing token, never a default.
    pub(crate) fn decode_meta_generation(raw: Option<&[u8]>) -> DomainResult<StoreGeneration> {
        match raw {
            Some(bytes) if bytes.len() >= 8 => {
                let value = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
                Ok(StoreGeneration::new(value))
            }
            Some(_) => Err(DomainError::new(
                DomainErrorCode::Validation,
                "corrupt store_generation metadata: present but truncated",
            )),
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
    pub(crate) fn read_generation_records(
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
    pub(crate) fn resolve_generation(
        &self,
        tx: &OptimisticWriteTx,
    ) -> DomainResult<StoreGeneration> {
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
        Self::decode_meta_generation(raw.as_ref().map(|v| v.as_ref()))
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
}

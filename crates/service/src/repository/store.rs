//! Snapshot export and guarded restore (moved verbatim from `repository.rs`).

use fjall::{OptimisticTxDatabase, OptimisticTxKeyspace, OptimisticWriteTx, PersistMode, Readable};

use super::{CanonicalRepository, decode, decode_generation, generation_key};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::export::CanonicalExport;
use ltmrs_domain::id::StoreGeneration;
use ltmrs_domain::memory::Memory;
use ltmrs_domain::projection::{GenerationRecord, GenerationStatus};
use ltmrs_domain::relation::Relation;
use ltmrs_domain::session::Session;

impl CanonicalRepository {
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
        Self::decode_meta_generation(raw.as_ref().map(|v| v.as_ref()))
    }

    /// Acquire the exclusive restore fence (P1 restore quiescence): while
    /// held, every mutating entry point blocks at its shared fence instead
    /// of committing, and in-flight mutations drain before the holder
    /// proceeds. The restore flow holds this across confirm → safety
    /// snapshot → replacement → context reset, so no acknowledged write
    /// can land between the safety backup and the replace and be drained
    /// unseen. All other paths must use the shared (read) fence via the
    /// normal entry points — never hold this guard except across one
    /// restore envelope (see `restore_replace_guarded`).
    pub fn restore_write_guard(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.restore_lock.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Non-blocking variant for tests: `Some` iff no restore holds the
    /// exclusive fence right now.
    pub fn try_restore_write_guard(&self) -> Option<std::sync::RwLockWriteGuard<'_, ()>> {
        self.restore_lock.try_write().ok()
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
        let guard = self.restore_write_guard();
        self.restore_replace_guarded(
            memories,
            relations,
            guides,
            feedback,
            suggestions,
            sessions,
            new_generation,
            &guard,
        )
    }

    /// Replacement under an already-held restore fence (see
    /// `restore_write_guard`): the exec restore flow holds the fence
    /// across confirm → safety snapshot → this call, so the safety
    /// backup and the replace are atomic with respect to every mutating
    /// entry point. Must not be called without holding the fence.
    #[allow(clippy::too_many_arguments)]
    pub fn restore_replace_guarded(
        &self,
        memories: &[Memory],
        relations: &[Relation],
        guides: &[ltmrs_domain::guide::Guide],
        feedback: &[ltmrs_domain::session::FeedbackEvent],
        suggestions: &[ltmrs_domain::session::Suggestion],
        sessions: &[ltmrs_domain::session::Session],
        new_generation: StoreGeneration,
        _guard: &std::sync::RwLockWriteGuard<'_, ()>,
    ) -> DomainResult<u64> {
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
            // guide_ops / suggestion_ops carry scoped
            // generation:frontend:epoch:operation keys. Leaving any behind
            // would preserve pre-restore state and let stale receipts replay
            // across the generation cut.
            &self.sessions,
            &self.session_ops,
            &self.guide_ops,
            &self.suggestion_ops,
            &self.tool_results,
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

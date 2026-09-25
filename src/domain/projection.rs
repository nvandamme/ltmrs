//! Projection desired-state and acknowledgement types (WP-05 task 4).
//!
//! A projection job is the durable record that a memory's canonical state must
//! be rendered into the search projection. It carries a monotonic sequence so
//! acknowledgements are compare-and-clear, never UUID-ordered (RV-07).

use serde::{Deserialize, Serialize};

use crate::domain::id::{DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};

/// A durable desired-state item: "project memory `memory_id` at this document
/// revision". The worker acknowledges by compare-and-clearing on `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionJob {
    pub memory_id: EntityId,
    /// The canonical document revision that must be projected. A late worker
    /// publishing an older revision cannot clear a job whose desired revision
    /// has moved on.
    pub desired_document_revision: DocumentRevision,
    /// Monotonic per-memory sequence assigned when the work was enqueued. This
    /// is the compare-and-clear token; it advances with every content mutation
    /// and is never derived from an identifier.
    pub seq: u64,
    /// Wall-clock millis when this job version was enqueued. Used to expose the
    /// oldest pending age (a freshness signal), not for ordering.
    pub enqueued_at_millis: u64,
    /// When true, the worker must remove every projected row for this memory
    /// instead of publishing one. Forget writes this atomically with the
    /// lifecycle change so deletion propagates without an external sweep.
    #[serde(default)]
    pub is_tombstone: bool,
}

/// Lifecycle of a store generation under blue-green cutover (design §8.2):
/// build the new generation alongside the old, verify its watermark, then
/// publish the active pointer atomically while the old rows stay retained
/// for rollback. UUIDs are identities, never commit order (RV-07) — the
/// watermark counts projected vs desired memories instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerationStatus {
    /// Staged but no build progress reported yet.
    Staged,
    /// Build in progress, watermark not yet met.
    Building,
    /// Watermark met (projected >= desired); eligible for activation.
    Ready,
    /// The live generation readers resolve by default.
    Active,
    /// Superseded but retained until the reaper's retention expires.
    Retired,
}

/// Durable per-generation cutover record (WP-05 task 8). One record per
/// store generation; transitions are owned by the canonical repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub generation: StoreGeneration,
    /// Model fingerprint the generation was built with. None for records
    /// auto-created for pre-record generations (unknown vector space).
    pub model_fingerprint: Option<ModelFingerprint>,
    pub status: GenerationStatus,
    /// Recallable canonical memories at stage time (watermark denominator).
    pub desired_memories: u64,
    /// Memories projected into this generation (watermark numerator).
    pub projected_memories: u64,
    /// Wall-clock millis of the last transition (staging, progress,
    /// activation, retirement). The reaper measures retention from the
    /// retirement timestamp.
    pub updated_at_millis: u64,
    /// True when canonical state mutated after the last build report: a
    /// mid-build write the new generation may not have converged yet.
    /// Set atomically with memory add/update/forget/merge while a pipeline
    /// is open; cleared by the next progress note (which attests a fresh
    /// build); blocks activation until cleared.
    /// Fail-closed on upgrade: records predating the flag decode as dirty,
    /// so an open pipeline of unknown build state must be re-reported.
    #[serde(default = "dirty_by_default")]
    pub build_dirty: bool,
}

/// Records predating the dirty flag never observed a build report under the
/// new regime, so they load dirty rather than trusted clean.
fn dirty_by_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    #[test]
    fn job_round_trips_through_serde() {
        let job = ProjectionJob {
            memory_id: eid(5),
            desired_document_revision: DocumentRevision::new(3),
            seq: 7,
            enqueued_at_millis: 12_000,
            is_tombstone: false,
        };
        let bytes = serde_json::to_vec(&job).unwrap();
        let decoded: ProjectionJob = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.memory_id, eid(5));
        assert_eq!(decoded.desired_document_revision.as_u64(), 3);
        assert_eq!(decoded.seq, 7);
        assert_eq!(decoded.enqueued_at_millis, 12_000);
    }

    #[test]
    fn seq_is_the_token_not_an_identity() {
        // Two jobs for the same memory differ only by seq and desired revision;
        // neither is ordered by any UUID.
        let a = ProjectionJob {
            memory_id: eid(1),
            desired_document_revision: DocumentRevision::new(1),
            seq: 1,
            enqueued_at_millis: 1000,
            is_tombstone: false,
        };
        let b = ProjectionJob {
            memory_id: eid(1),
            desired_document_revision: DocumentRevision::new(2),
            seq: 2,
            enqueued_at_millis: 2000,
            is_tombstone: false,
        };
        assert_ne!(a.seq, b.seq);
        assert_eq!(a.memory_id, b.memory_id);
    }
}

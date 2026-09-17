//! Projection desired-state and acknowledgement types (WP-05 task 4).
//!
//! A projection job is the durable record that a memory's canonical state must
//! be rendered into the search projection. It carries a monotonic sequence so
//! acknowledgements are compare-and-clear, never UUID-ordered (RV-07).

use serde::{Deserialize, Serialize};

use crate::domain::id::{DocumentRevision, EntityId};

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

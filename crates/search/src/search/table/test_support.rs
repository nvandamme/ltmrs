//! Table test helpers (moved verbatim from `table.rs`).

use super::SearchRow;
use ltmrs_domain::id::{ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};
use uuid::Uuid;

pub(crate) fn eid(n: u64) -> EntityId {
    EntityId::new(Uuid::from_u128(n as u128))
}

pub(crate) fn row(id: u64, text: &str, rev: u64, embedding: Option<Vec<f32>>) -> SearchRow {
    SearchRow {
        store_generation: StoreGeneration::FIRST,
        memory_id: eid(id),
        document_revision: DocumentRevision::new(rev),
        model_fingerprint: ModelFingerprint::new(1),
        chunk_id: ChunkId::new(0),
        chunker_version: "single-chunk-v1".to_string(),
        lexical_text: text.to_string(),
        char_start: 0,
        char_end: text.len() as u64,
        project: Some("ltmrs".into()),
        fragment_type: "fact".into(),
        created_at_millis: 1000,
        confidence: 0.5,
        updated_at_millis: 2000,
        embedding,
    }
}

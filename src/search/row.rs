//! Search projection row: the unit published to Lance (WP-05 task 1).

use crate::domain::id::{ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};

/// One chunk of one document revision under one model generation.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRow {
    pub store_generation: StoreGeneration,
    pub memory_id: EntityId,
    pub document_revision: DocumentRevision,
    pub model_fingerprint: ModelFingerprint,
    pub chunk_id: ChunkId,
    /// Rendered searchable text (title prefix + fragment content).
    pub lexical_text: String,
    /// Byte offsets of this chunk within the rendered source.
    pub char_start: u64,
    pub char_end: u64,
    pub project: Option<String>,
    pub fragment_type: String,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
    /// Null until the embedding worker fills it (lexical-ready rows have none).
    pub embedding: Option<Vec<f32>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    #[test]
    fn row_holds_all_identity_and_scope_fields() {
        let row = SearchRow {
            store_generation: StoreGeneration::new(7),
            memory_id: eid(42),
            document_revision: DocumentRevision::new(3),
            model_fingerprint: ModelFingerprint::new(99),
            chunk_id: ChunkId::new(1),
            lexical_text: "hello world".into(),
            char_start: 0,
            char_end: 11,
            project: Some("ltmrs".into()),
            fragment_type: "fact".into(),
            created_at_millis: 1000,
            updated_at_millis: 2000,
            embedding: None,
        };

        assert_eq!(row.store_generation.as_u64(), 7);
        assert_eq!(row.memory_id, eid(42));
        assert_eq!(row.document_revision.as_u64(), 3);
        assert_eq!(row.model_fingerprint.as_u64(), 99);
        assert_eq!(row.chunk_id.as_u32(), 1);
        assert_eq!(row.lexical_text, "hello world");
        assert_eq!((row.char_start, row.char_end), (0, 11));
        assert_eq!(row.project.as_deref(), Some("ltmrs"));
        assert_eq!(row.fragment_type, "fact");
    }

    #[test]
    fn row_is_lexical_ready_without_embedding() {
        let mut row = SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: eid(1),
            document_revision: DocumentRevision::new(0),
            model_fingerprint: ModelFingerprint::new(1),
            chunk_id: ChunkId::new(0),
            lexical_text: "text".into(),
            char_start: 0,
            char_end: 4,
            project: None,
            fragment_type: "fact".into(),
            created_at_millis: 0,
            updated_at_millis: 0,
            embedding: None,
        };
        assert!(row.embedding.is_none());

        row.embedding = Some(vec![0.1, 0.2]);
        assert_eq!(row.embedding.as_ref().unwrap(), &vec![0.1, 0.2]);
    }
}

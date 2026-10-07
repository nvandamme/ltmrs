//! Relation types and edge records.

use crate::id::EntityId;
use crate::memory::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RelationType {
    Supports,
    Contradicts,
    Supersedes,
    SupersededBy,
    RelatedTo,
}

impl RelationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RelationType::Supports => "supports",
            RelationType::Contradicts => "contradicts",
            RelationType::Supersedes => "supersedes",
            RelationType::SupersededBy => "superseded_by",
            RelationType::RelatedTo => "related_to",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "supports" => Some(RelationType::Supports),
            "contradicts" => Some(RelationType::Contradicts),
            "supersedes" => Some(RelationType::Supersedes),
            "superseded_by" => Some(RelationType::SupersededBy),
            "related_to" => Some(RelationType::RelatedTo),
            _ => None,
        }
    }

    pub fn is_symmetric(&self) -> bool {
        matches!(
            self,
            RelationType::Supports | RelationType::Contradicts | RelationType::RelatedTo
        )
    }

    pub fn inverse(&self) -> Self {
        match self {
            RelationType::Supersedes => RelationType::SupersededBy,
            RelationType::SupersededBy => RelationType::Supersedes,
            other => *other,
        }
    }

    pub fn is_supersession(&self) -> bool {
        matches!(self, RelationType::Supersedes | RelationType::SupersededBy)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Relation {
    pub id: EntityId,
    pub source: EntityId,
    pub target: EntityId,
    pub relation_type: RelationType,
    pub note: Option<String>,
    pub created_at: Instant,
}

impl Relation {
    /// The consolidation edge for an atomic merge: result→source
    /// Supersedes, id-bound to the merged pair (stable across retries;
    /// unique per merge). Both endpoints must be live when it is
    /// recorded — which is why merge writes it before archiving.
    pub fn consolidation_edge(result: EntityId, source: EntityId, now: Instant) -> Self {
        let id = EntityId::new(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("ltmrs:rel:merge:{}:{}", result.as_uuid(), source.as_uuid()).as_bytes(),
        ));
        Self {
            id,
            source: result,
            target: source,
            relation_type: RelationType::Supersedes,
            note: Some("consolidated".to_string()),
            created_at: now,
        }
    }
}

impl Relation {
    pub fn new(
        id: EntityId,
        source: EntityId,
        target: EntityId,
        relation_type: RelationType,
        note: Option<String>,
        created_at: Instant,
    ) -> Self {
        Self {
            id,
            source,
            target,
            relation_type,
            note,
            created_at,
        }
    }

    pub fn reverse(&self) -> Self {
        Self {
            id: self.id,
            source: self.target,
            target: self.source,
            relation_type: self.relation_type.inverse(),
            note: self.note.clone(),
            created_at: self.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetry_and_direction() {
        assert!(RelationType::Supports.is_symmetric());
        assert!(RelationType::Contradicts.is_symmetric());
        assert!(RelationType::RelatedTo.is_symmetric());
        assert!(!RelationType::Supersedes.is_symmetric());
        assert!(!RelationType::SupersededBy.is_symmetric());

        // Supersession is directed; inverse is derived.
        assert_eq!(
            RelationType::Supersedes.inverse(),
            RelationType::SupersededBy
        );
        assert_eq!(
            RelationType::SupersededBy.inverse(),
            RelationType::Supersedes
        );
        // Symmetric types invert to themselves.
        assert_eq!(RelationType::Supports.inverse(), RelationType::Supports);
    }

    #[test]
    fn roundtrip_string_forms() {
        for t in [
            RelationType::Supports,
            RelationType::Contradicts,
            RelationType::Supersedes,
            RelationType::SupersededBy,
            RelationType::RelatedTo,
        ] {
            assert_eq!(RelationType::parse(t.as_str()), Some(t));
        }
        assert_eq!(RelationType::parse("bogus"), None);
    }
}

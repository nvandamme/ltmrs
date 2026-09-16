//! Relation types and edge records.

use crate::domain::id::EntityId;
use crate::domain::memory::Instant;

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

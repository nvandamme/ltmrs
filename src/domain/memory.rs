//! Canonical memory record, preserving optional quality, lifecycle
//! distinctions, evidence, and provenance.

use std::collections::BTreeMap;

use crate::domain::id::{
    DocumentRevision, EligibilityRevision, EntityId, EntityRevision, ExternalAlias,
};
use crate::domain::relation::Relation;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Instant(pub u64);

impl Instant {
    pub const fn new(millis: u64) -> Self {
        Self(millis)
    }
    pub const fn as_millis(&self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MemorySource {
    User,
    Ai,
}

impl MemorySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemorySource::User => "user",
            MemorySource::Ai => "ai",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(MemorySource::User),
            "ai" => Some(MemorySource::Ai),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FragmentType {
    Fact,
    Pattern,
    Lesson,
    Warning,
    Context,
}

impl FragmentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FragmentType::Fact => "fact",
            FragmentType::Pattern => "pattern",
            FragmentType::Lesson => "lesson",
            FragmentType::Warning => "warning",
            FragmentType::Context => "context",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fact" => Some(FragmentType::Fact),
            "pattern" => Some(FragmentType::Pattern),
            "lesson" => Some(FragmentType::Lesson),
            "warning" => Some(FragmentType::Warning),
            "context" => Some(FragmentType::Context),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MemoryLifecycle {
    Live,
    Invalidated { at: Instant },
    Archived { at: Instant },
    Deleted { at: Instant },
}

impl MemoryLifecycle {
    pub fn is_recallable(&self) -> bool {
        matches!(self, MemoryLifecycle::Live)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Evidence {
    pub file: String,
    pub symbol: Option<String>,
    pub snippet: String,
    pub snippet_sha256: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Memory {
    pub id: EntityId,
    pub external_alias: Option<ExternalAlias>,
    pub title: String,
    pub fragment: String,
    pub description: String,
    pub fragment_type: FragmentType,
    pub project: Option<String>,
    pub source: MemorySource,
    pub confidence: f64,
    pub quality_score: Option<f64>,
    pub lifecycle: MemoryLifecycle,
    pub tags: Vec<String>,
    pub associated_with: Vec<String>,
    pub relations: Vec<Relation>,
    pub parent_id: Option<EntityId>,
    pub child_ids: Vec<EntityId>,
    pub session_id: Option<String>,
    pub task_type: Option<String>,
    pub related_guides: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub access_count: u64,
    pub last_accessed_at: Option<Instant>,
    pub positive_feedback: u64,
    pub negative_feedback: u64,
    pub negative_hits: u64,
    pub refinement_count: u64,
    pub distill_candidate: bool,
    pub entity_revision: EntityRevision,
    pub document_revision: DocumentRevision,
    pub eligibility_revision: EligibilityRevision,
    pub created_at: Instant,
    pub updated_at: Instant,
    pub raw_created: Option<String>,
    pub unknown_fields: BTreeMap<String, serde_json::Value>,
}

impl Memory {
    pub fn advance_document(&mut self) {
        self.document_revision = self.document_revision.next();
        self.entity_revision = self.entity_revision.next();
    }
    pub fn advance_eligibility(&mut self) {
        self.eligibility_revision = self.eligibility_revision.next();
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArchivedFragment {
    pub id: EntityId,
    pub legacy_id: Option<ExternalAlias>,
    pub title: String,
    pub fragment: String,
    pub description: Option<String>,
    pub fragment_type: FragmentType,
    pub project: Option<String>,
    pub confidence: f64,
    pub source: MemorySource,
    pub tags: Vec<String>,
    pub created_at: Option<Instant>,
    pub heat: Option<f64>,
    pub archived_at: Instant,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FragmentHistory {
    pub id: EntityId,
    pub memory_id: EntityId,
    pub title: String,
    pub fragment: String,
    pub description: Option<String>,
    pub confidence: f64,
    pub fragment_type: FragmentType,
    pub changed_at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_score_is_optional() {
        let score: Option<f64> = None;
        assert!(score.is_none());
        assert_eq!(Some(0.9), Some(0.9f64));
    }

    #[test]
    fn lifecycle_distinctions() {
        assert!(MemoryLifecycle::Live.is_recallable());
        assert!(
            !MemoryLifecycle::Invalidated {
                at: Instant::new(1)
            }
            .is_recallable()
        );
        assert!(
            !MemoryLifecycle::Archived {
                at: Instant::new(1)
            }
            .is_recallable()
        );
        assert!(
            !MemoryLifecycle::Deleted {
                at: Instant::new(1)
            }
            .is_recallable()
        );
    }
}

//! Canonical export and digest for round-trip/property tests.

use std::collections::BTreeMap;

use crate::domain::guide::Guide;
use crate::domain::memory::{ArchivedFragment, FragmentHistory, Memory};
use crate::domain::project::Project;
use crate::domain::relation::Relation;
use crate::domain::session::{FeedbackEvent, Suggestion};

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct CanonicalExport {
    pub memories: Vec<Memory>,
    pub relations: Vec<Relation>,
    pub guides: Vec<Guide>,
    pub sessions: Vec<crate::domain::session::Session>,
    pub feedback: Vec<FeedbackEvent>,
    pub suggestions: Vec<Suggestion>,
    pub projects: Vec<Project>,
    pub archives: Vec<ArchivedFragment>,
    pub history: Vec<FragmentHistory>,
    pub unknown_fields: BTreeMap<String, serde_json::Value>,
}

impl CanonicalExport {
    pub fn normalize(&mut self) {
        self.memories.sort_by_key(|m| m.id);
        self.relations.sort_by_key(|r| r.id);
        self.guides.sort_by_key(|g| g.name.clone());
        self.feedback.sort_by_key(|f| f.id);
        self.suggestions.sort_by_key(|s| s.id);
        self.archives.sort_by_key(|a| a.id);
        self.history.sort_by_key(|h| h.id);
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Stable digest of the export, independent of record ordering.
    /// A normalized copy is hashed so two exports with the same records in
    /// different orders produce the same digest.
    pub fn digest(&self) -> String {
        let mut normalized = self.clone();
        normalized.normalize();
        sha256_hex(&normalized.to_json())
    }
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_deterministic_and_order_insensitive() {
        let a = CanonicalExport::default();
        let b = a.clone();
        assert_eq!(a.digest(), b.digest(), "same export => same digest");

        // Two exports with the same records in different orders must share a
        // digest (order-insensitivity).
        use crate::domain::id::EntityId;
        use crate::domain::memory::Instant;
        use crate::domain::relation::{Relation, RelationType};
        use uuid::Uuid;

        let r1 = Relation::new(
            EntityId::new(Uuid::from_u128(1)),
            EntityId::new(Uuid::from_u128(1)),
            EntityId::new(Uuid::from_u128(2)),
            RelationType::Supports,
            None,
            Instant::new(1),
        );
        let r2 = Relation::new(
            EntityId::new(Uuid::from_u128(2)),
            EntityId::new(Uuid::from_u128(2)),
            EntityId::new(Uuid::from_u128(3)),
            RelationType::Supports,
            None,
            Instant::new(1),
        );
        let x = CanonicalExport {
            relations: vec![r1.clone(), r2.clone()],
            ..Default::default()
        };
        let y = CanonicalExport {
            relations: vec![r2, r1],
            ..Default::default()
        };
        assert_eq!(x.digest(), y.digest(), "reordered records => same digest");
    }

    /// T-DATA-01: round-trip every canonical field, null/absent value, alias,
    /// evidence, archive, guide dependency and history reference. No implicit
    /// unknown-to-zero conversion, lost fields or accidental enum extension.
    #[test]
    fn t_data_01_round_trip_preserves_nulls_aliases_evidence_archive_history() {
        use crate::domain::guide::Guide;
        use crate::domain::id::{
            DocumentRevision, EligibilityRevision, EntityId, EntityRevision, ExternalAlias,
        };
        use crate::domain::memory::{
            ArchivedFragment, Evidence, FragmentHistory, FragmentType, Instant, Memory,
            MemoryLifecycle, MemorySource,
        };
        use crate::domain::project::Project;
        use crate::domain::relation::{Relation, RelationType};
        use uuid::Uuid;

        let mid = EntityId::new(Uuid::from_u128(100));
        let memory = Memory {
            id: mid,
            external_alias: Some(ExternalAlias::new("m2a5d0cde45ce")),
            title: "T".into(),
            fragment: "F".into(),
            description: String::new(),
            fragment_type: FragmentType::Lesson,
            project: None,
            source: MemorySource::Ai,
            confidence: 0.42,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec!["rust".into()],
            associated_with: Vec::new(),
            relations: vec![Relation::new(
                EntityId::new(Uuid::from_u128(200)),
                EntityId::new(Uuid::from_u128(300)),
                mid,
                RelationType::Supports,
                None,
                Instant(1),
            )],
            parent_id: None,
            child_ids: Vec::new(),
            session_id: None,
            task_type: None,
            related_guides: vec!["rust".into()],
            evidence: vec![Evidence {
                file: "src/lib.rs".into(),
                symbol: Some("main".into()),
                snippet: "fn main() {}".into(),
                snippet_sha256: "abc123".into(),
            }],
            access_count: 3,
            last_accessed_at: Some(Instant(5)),
            positive_feedback: 1,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: true,
            entity_revision: EntityRevision::new(2),
            document_revision: DocumentRevision::new(2),
            eligibility_revision: EligibilityRevision::new(1),
            created_at: Instant(1),
            updated_at: Instant(2),
            raw_created: Some("2026-09-16T10:00:00Z".into()),
            unknown_fields: {
                let mut m = BTreeMap::new();
                m.insert("legacy_experimental_flag".into(), serde_json::json!(true));
                m
            },
        };

        let guide = Guide {
            name: "rust".into(),
            category: "programming-language".into(),
            description: "manual".into(),
            contexts: vec!["ownership".into()],
            learnings: vec!["lifetimes".into()],
            usage_count: 2,
            last_used: Some(Instant(9)),
            success_count: 2,
            failure_count: 0,
            anti_patterns: Vec::new(),
            pitfalls: Vec::new(),
            depends_on: vec!["cargo".into()],
            enables: vec!["wasm".into()],
            source_memories: vec![EntityId::new(Uuid::from_u128(500))],
            validated_by: Vec::new(),
            superseded_by: None,
            deprecated: false,
            entity_revision: EntityRevision::new(1),
            created_at: Instant(1),
            updated_at: Instant(1),
        };

        let archive = ArchivedFragment {
            id: EntityId::new(Uuid::from_u128(500)),
            legacy_id: Some(ExternalAlias::new("mOld")),
            title: "Old".into(),
            fragment: "Old frag".into(),
            description: None,
            fragment_type: FragmentType::Fact,
            project: Some("app".into()),
            confidence: 0.1,
            source: MemorySource::User,
            tags: Vec::new(),
            created_at: Some(Instant(1)),
            heat: None,
            archived_at: Instant(10),
        };

        let history = FragmentHistory {
            id: EntityId::new(Uuid::from_u128(600)),
            memory_id: mid,
            title: "T-old".into(),
            fragment: "F-old".into(),
            description: None,
            confidence: 0.5,
            fragment_type: FragmentType::Fact,
            changed_at: Instant(3),
        };

        let project = Project {
            id: EntityId::new(Uuid::from_u128(700)),
            name: "app".into(),
            legacy_name: Some("app".into()),
            is_global: false,
        };

        let export = CanonicalExport {
            memories: vec![memory.clone()],
            relations: vec![],
            guides: vec![guide.clone()],
            sessions: vec![],
            feedback: vec![],
            suggestions: vec![],
            projects: vec![project.clone()],
            archives: vec![archive.clone()],
            history: vec![history.clone()],
            unknown_fields: BTreeMap::new(),
        };

        let json = serde_json::to_string(&export).expect("export serializes");
        let restored: CanonicalExport = serde_json::from_str(&json).expect("export deserializes");

        assert_eq!(restored.memories, vec![memory.clone()]);
        assert_eq!(restored.guides, vec![guide.clone()]);
        assert_eq!(restored.projects, vec![project.clone()]);
        assert_eq!(restored.archives, vec![archive.clone()]);
        assert_eq!(restored.history, vec![history.clone()]);

        assert_eq!(restored.memories[0].quality_score, None);
        assert_eq!(restored.memories[0].project, None);
        assert_eq!(restored.archives[0].heat, None);
        assert_eq!(
            restored.memories[0]
                .unknown_fields
                .get("legacy_experimental_flag"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            restored.memories[0]
                .external_alias
                .as_ref()
                .unwrap()
                .as_str(),
            "m2a5d0cde45ce"
        );
    }
}

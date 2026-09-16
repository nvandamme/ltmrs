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

    pub fn digest(&self) -> String {
        let normalized = self.to_json();
        sha256_hex(&normalized)
    }
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

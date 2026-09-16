//! Frozen legacy wire DTOs (Lemma 0.21.0 compatibility).

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyMemoryRelation {
    pub source_id: String,
    pub target_id: String,
    pub relation_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyMemoryFragment {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fragment: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fragment_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_guides: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub associated_with: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_relations: Option<Vec<LegacyMemoryRelation>>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyGuide {
    pub name: String,
    pub category: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contexts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub learnings: Option<Vec<String>>,
    pub usage_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacySession {
    pub task_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_approach: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lessons: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyAttempt {
    pub approach: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critique: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyImprovementSuggestion {
    pub id: u64,
    pub suggestion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_memory_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_wire_preserves_null_and_absent() {
        // quality_score null, project null, description absent => all stay None,
        // never silently converted to a default (RQ-16 / T-DATA-01).
        let json = r#"{
            "id": "m1",
            "title": "T",
            "fragment": "F",
            "project": null,
            "confidence": 0.5,
            "quality_score": null
        }"#;
        let f: LegacyMemoryFragment = serde_json::from_str(json).expect("parses");
        assert_eq!(f.quality_score, None, "null quality stays None, not 0.0");
        assert_eq!(f.project, None);
        // description is absent in the input and must stay None.
        assert_eq!(f.description, None);
        assert_eq!(f.evidence, None);
        assert_eq!(f.related_relations, None);
    }
}

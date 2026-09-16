//! Guide record type.

use crate::domain::id::EntityRevision;
use crate::domain::memory::Instant;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Guide {
    pub name: String,
    pub category: String,
    pub description: String,
    pub contexts: Vec<String>,
    pub learnings: Vec<String>,
    pub usage_count: u32,
    pub last_used: Option<Instant>,
    pub success_count: u32,
    pub failure_count: u32,
    pub anti_patterns: Vec<String>,
    pub pitfalls: Vec<String>,
    pub depends_on: Vec<String>,
    pub enables: Vec<String>,
    pub source_memories: Vec<crate::domain::id::EntityId>,
    pub validated_by: Vec<String>,
    pub superseded_by: Option<String>,
    pub deprecated: bool,
    pub entity_revision: EntityRevision,
    pub created_at: Instant,
    pub updated_at: Instant,
}

impl Guide {
    pub fn is_actionable(&self) -> bool {
        !self.deprecated && self.superseded_by.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_guide() -> Guide {
        Guide {
            name: "react".into(),
            category: "web-frontend".into(),
            description: String::new(),
            contexts: vec![],
            learnings: vec![],
            usage_count: 0,
            last_used: None,
            success_count: 0,
            failure_count: 0,
            anti_patterns: vec![],
            pitfalls: vec![],
            depends_on: vec![],
            enables: vec![],
            source_memories: vec![],
            validated_by: vec![],
            superseded_by: None,
            deprecated: false,
            entity_revision: EntityRevision::new(1),
            created_at: Instant::new(0),
            updated_at: Instant::new(0),
        }
    }

    #[test]
    fn guide_actionability() {
        let mut g = sample_guide();
        assert!(g.is_actionable());
        g.deprecated = true;
        assert!(!g.is_actionable());
        g.deprecated = false;
        g.superseded_by = Some("react-v2".into());
        assert!(!g.is_actionable());
    }
}

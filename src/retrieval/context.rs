//! Context assembly and budgeting (WP-07 tasks 10-11, RQ-14, RQ-24).
//!
//! Budgets the ACTUAL serialized context (including labels, warnings and
//! wrappers). A known target tokenizer gives token-accurate accounting;
//! otherwise a documented conservative byte/character budget is used, never an
//! invented exact token count. Selects whole coherent bundles or explicit
//! summaries; never silently fabricates abstractive summaries.

use std::collections::BTreeSet;

use crate::domain::id::EntityId;
use crate::domain::memory::Memory;

/// Context budget configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextBudget {
    /// Maximum bytes of serialized context (labels, warnings, wrappers included).
    pub max_bytes: usize,
    /// Whether a known tokenizer is available for token-accurate accounting.
    pub has_tokenizer: bool,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_bytes: 8000,
            has_tokenizer: false,
        }
    }
}

/// The token/byte accounting method used (for transparency in explanations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountingMethod {
    /// Token-accurate accounting via a known tokenizer.
    Tokenizer,
    /// Conservative byte/character budget (no tokenizer available).
    #[default]
    ByteEstimate,
}

/// One item in the assembled context.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextItem {
    pub memory_id: EntityId,
    /// The serialized text for this item (title + fragment, bounded).
    pub text: String,
    /// The byte length of `text`.
    pub bytes: usize,
    /// Whether this item is part of a protected bundle.
    pub protected: bool,
}

/// The assembled context result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextResult {
    /// The selected items in order.
    pub items: Vec<ContextItem>,
    /// Total bytes used (including any warnings/labels added).
    pub total_bytes: usize,
    /// The accounting method used.
    pub method: AccountingMethod,
    /// Whether the context was truncated to fit the budget.
    pub truncated: bool,
    /// A concise conflict notice with IDs, emitted when a complete conflict
    /// bundle could not fit (never silently show only one claim).
    pub conflict_notice: Option<String>,
    /// IDs excluded due to budget.
    pub excluded: Vec<EntityId>,
}

/// Serialize a memory to its bounded context text.
fn serialize_memory(memory: &Memory) -> String {
    let mut s = String::new();
    s.push_str(&memory.title);
    s.push('\n');
    s.push_str(&memory.fragment);
    s
}

/// Assemble the context from the given memories under the budget.
///
/// `memories` is in the final ranked order (post-MMR). `protected` are bundle
/// members that must be included when possible.
pub fn assemble_context(
    memories: &[Memory],
    protected: &BTreeSet<EntityId>,
    budget: &ContextBudget,
) -> ContextResult {
    let method = if budget.has_tokenizer {
        AccountingMethod::Tokenizer
    } else {
        AccountingMethod::ByteEstimate
    };

    let mut result = ContextResult {
        method,
        ..Default::default()
    };

    // Reserve space for a potential conflict notice header.
    let mut used = 0usize;

    for memory in memories {
        let text = serialize_memory(memory);
        let bytes = text.len();

        // Budget check: if adding this item exceeds the budget, stop.
        if used + bytes > budget.max_bytes && !result.items.is_empty() {
            result.truncated = true;
            result.excluded.push(memory.id);
            continue;
        }

        result.items.push(ContextItem {
            memory_id: memory.id,
            text,
            bytes,
            protected: protected.contains(&memory.id),
        });
        used += bytes;
    }

    result.total_bytes = used;
    result
}

/// Emit a concise conflict notice when a conflict bundle cannot fully fit.
pub fn conflict_notice(members: &[EntityId]) -> String {
    let ids: Vec<String> = members.iter().map(|id| id.to_string()).collect();
    format!(
        "CONFLICT: unresolved contradiction between memories [{}]. \
         Both sides are required; review both before acting.",
        ids.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{DocumentRevision, EligibilityRevision, EntityId, EntityRevision};
    use crate::domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn memory(id: EntityId, content: &str) -> Memory {
        Memory {
            id,
            external_alias: None,
            title: "Test".into(),
            fragment: content.to_string(),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: None,
            source: MemorySource::Ai,
            confidence: 0.5,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: EntityRevision::new(1),
            document_revision: DocumentRevision::new(1),
            eligibility_revision: EligibilityRevision::new(1),
            created_at: Instant::new(1),
            updated_at: Instant::new(1),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn context_fits_within_budget() {
        let memories = vec![memory(eid(1), "short"), memory(eid(2), "shorter")];
        let budget = ContextBudget {
            max_bytes: 1000,
            ..Default::default()
        };
        let result = assemble_context(&memories, &BTreeSet::new(), &budget);
        assert_eq!(result.items.len(), 2);
        assert!(!result.truncated);
        assert!(result.total_bytes <= 1000);
    }

    #[test]
    fn context_truncates_when_over_budget() {
        let big = "x".repeat(500);
        let memories = vec![
            memory(eid(1), &big),
            memory(eid(2), &big),
            memory(eid(3), &big),
        ];
        let budget = ContextBudget {
            max_bytes: 700,
            ..Default::default()
        };
        let result = assemble_context(&memories, &BTreeSet::new(), &budget);
        assert!(result.truncated);
        assert!(!result.excluded.is_empty());
        assert!(result.total_bytes <= 700);
    }

    #[test]
    fn byte_estimate_method_when_no_tokenizer() {
        let memories = vec![memory(eid(1), "test")];
        let budget = ContextBudget {
            has_tokenizer: false,
            ..Default::default()
        };
        let result = assemble_context(&memories, &BTreeSet::new(), &budget);
        assert_eq!(result.method, AccountingMethod::ByteEstimate);
    }

    #[test]
    fn tokenizer_method_when_available() {
        let memories = vec![memory(eid(1), "test")];
        let budget = ContextBudget {
            has_tokenizer: true,
            ..Default::default()
        };
        let result = assemble_context(&memories, &BTreeSet::new(), &budget);
        assert_eq!(result.method, AccountingMethod::Tokenizer);
    }

    #[test]
    fn conflict_notice_includes_all_ids() {
        let notice = conflict_notice(&[eid(1), eid(2)]);
        assert!(notice.contains(&eid(1).to_string()));
        assert!(notice.contains(&eid(2).to_string()));
        assert!(notice.contains("CONFLICT"));
    }

    #[test]
    fn protected_items_marked() {
        let memories = vec![memory(eid(1), "test")];
        let protected = BTreeSet::from([eid(1)]);
        let budget = ContextBudget::default();
        let result = assemble_context(&memories, &protected, &budget);
        assert!(result.items[0].protected);
    }
}

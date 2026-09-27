//! Effective scope resolution and enforcement (WP-07 task 2, RQ-13).
//!
//! The effective scope is computed ONCE from the profile and request, then
//! applied uniformly to every retrieval leg, canonical hydration, and graph
//! step. This prevents the class of bugs where filters are applied only after
//! retrieval or are lost during graph expansion (RV-13).

use crate::domain::command::{DomainError, DomainErrorCode, DomainResult, Scope};
use crate::domain::id::EntityId;
use crate::domain::memory::{FragmentType, Memory};

/// The resolved, immutable scope for a single retrieval call.
///
/// Constructed once via [`EffectiveScope::resolve`], then shared by reference
/// across the lexical leg, dense leg, hydration, and graph expansion. Every
/// predicate check MUST flow through this type so no leg can drift.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveScope {
    /// The raw request scope (project, filters, etc.).
    pub raw: Scope,
    /// Whether the scope includes global (project-less) memories.
    /// Project + global inheritance is deliberate, not "all projects".
    pub includes_global: bool,
    /// The specific project this scope targets, if any.
    pub project: Option<String>,
    /// Whether to include all projects (explicit cross-project mode).
    pub all_projects: bool,
}

impl EffectiveScope {
    /// Resolve the effective scope from the raw request scope.
    ///
    /// This is the single entry point — every retrieval call resolves its
    /// scope exactly once here and threads the result through. A
    /// non-finite min_confidence (only constructible natively; JSON has no
    /// NaN/inf literals) is rejected: it must never reach a filter, where
    /// NaN comparisons would silently match nothing. (The struct fields
    /// stay constructible for tests; production code must go through
    /// here.)
    pub fn resolve(raw: &Scope) -> DomainResult<Self> {
        if let Some(min) = raw.min_confidence
            && !min.is_finite()
        {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "min_confidence must be finite",
            ));
        }
        // Project + global inheritance is deliberate (design §6.3): global
        // (project-less) memories are always inherited by any scope. This is
        // not "all projects" — a project scope still excludes other projects.
        Ok(Self {
            includes_global: true,
            project: raw.project.clone(),
            all_projects: raw.all_projects,
            raw: raw.clone(),
        })
    }

    /// Whether a memory's project field is in scope.
    pub fn project_includes(&self, record_project: Option<&str>) -> bool {
        if self.all_projects {
            return true;
        }
        match (&self.project, record_project) {
            // Global (None) memory: included only if we allow global.
            (None, None) => self.includes_global,
            (Some(_), None) => self.includes_global,
            (Some(p), Some(rp)) => p == rp,
            // A memory with a project when we have no project scope: excluded.
            (None, Some(_)) => false,
        }
    }

    /// Whether a memory's fragment type is in scope.
    pub fn type_includes(&self, ft: &FragmentType) -> bool {
        match &self.raw.fragment_types {
            None => true,
            Some(types) => types.iter().any(|t| t == ft),
        }
    }

    /// Whether a memory's created_at is in scope (after/before bounds).
    pub fn date_includes(&self, created_at_millis: u64) -> bool {
        if let Some(after) = self.raw.after
            && created_at_millis < after
        {
            return false;
        }
        if let Some(before) = self.raw.before
            && created_at_millis > before
        {
            return false;
        }
        true
    }

    /// Whether a memory's confidence is in scope.
    pub fn confidence_includes(&self, confidence: f64) -> bool {
        if let Some(min) = self.raw.min_confidence
            && confidence < min
        {
            return false;
        }
        true
    }

    /// Whether a memory is lifecycle-eligible (recallable).
    pub fn lifecycle_includes(&self, memory: &Memory) -> bool {
        memory.lifecycle.is_recallable()
    }

    /// The complete eligibility predicate for a memory.
    ///
    /// This is the canonical check used by hydration and backfill. A memory is
    /// eligible only if it passes ALL scope dimensions AND is lifecycle-recallable.
    pub fn is_eligible(&self, memory: &Memory) -> bool {
        self.project_includes(memory.project.as_deref())
            && self.type_includes(&memory.fragment_type)
            && self.date_includes(memory.created_at.as_millis())
            && self.confidence_includes(memory.confidence)
            && self.lifecycle_includes(memory)
    }

    /// Build a DataFusion filter predicate for the lexical/dense legs.
    ///
    /// This is applied to the Lance query so out-of-scope candidates are
    /// filtered at the source, not just after retrieval.
    pub fn to_lance_filter(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();

        if !self.all_projects {
            if let Some(ref p) = self.project {
                // Project scope: include this project OR global (NULL).
                parts.push(format!("(project = '{}' OR project IS NULL)", sql_quote(p)));
            } else {
                // No project scope: only global (NULL) memories.
                parts.push("project IS NULL".to_string());
            }
        }

        if let Some(ref types) = self.raw.fragment_types
            && !types.is_empty()
        {
            let quoted: Vec<String> = types
                .iter()
                .map(|t| format!("'{}'", sql_quote(t.as_str())))
                .collect();
            parts.push(format!("fragment_type IN ({})", quoted.join(", ")));
        }

        if let Some(after) = self.raw.after {
            parts.push(format!("created_at_millis >= {after}"));
        }
        if let Some(before) = self.raw.before {
            parts.push(format!("created_at_millis <= {before}"));
        }

        // Confidence pre-filter at the source (RV-13): without it,
        // candidate-limit truncation happens before eligibility checks and
        // eligible rows are silently lost. resolve() rejects non-finite
        // bounds, so this interpolation only ever carries finite numbers;
        // the projection column is non-null Float64.
        if let Some(min) = self.raw.min_confidence {
            parts.push(format!("confidence >= {min}"));
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" AND "))
        }
    }

    /// Whether this scope is "unrestricted" (no filters at all).
    pub fn is_unrestricted(&self) -> bool {
        self.all_projects
            && self.raw.fragment_types.is_none()
            && self.raw.after.is_none()
            && self.raw.before.is_none()
            && self.raw.min_confidence.is_none()
    }
}

/// Escape a string literal for safe interpolation into a Lance/DataFusion
/// filter expression. DataFusion follows SQL string-literal rules, so a
/// single quote is escaped by doubling it. Prevents a malicious or
/// accidental project name from breaking or injecting into the predicate.
///
/// CENTRAL RULE: every string interpolated into a DataFusion filter anywhere
/// in the codebase must go through this function (integers and UUIDs are
/// safe by construction and need no quoting). New string columns re-audit
/// against this rule; bind parameters do not exist in the Lance query API
/// used here, so this function IS the injection boundary.
pub(crate) fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// A scoped candidate: a memory ID that passed scope filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopedCandidate {
    pub id: EntityId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{DocumentRevision, EligibilityRevision, EntityId, EntityRevision};
    use crate::domain::memory::{MemoryLifecycle, MemorySource};
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn memory(id: EntityId, project: Option<&str>, ft: FragmentType, conf: f64) -> Memory {
        Memory {
            id,
            external_alias: None,
            title: "t".into(),
            fragment: "f".into(),
            description: String::new(),
            fragment_type: ft,
            project: project.map(|s| s.to_string()),
            source: MemorySource::Ai,
            confidence: conf,
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
            created_at: crate::domain::memory::Instant::new(1000),
            updated_at: crate::domain::memory::Instant::new(1000),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn project_scope_includes_project_and_global() {
        let scope = EffectiveScope::resolve(&Scope {
            project: Some("app".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(scope.project_includes(Some("app")));
        assert!(scope.project_includes(None)); // global inheritance
        assert!(!scope.project_includes(Some("other")));
    }

    #[test]
    fn no_project_scope_only_global() {
        let scope = EffectiveScope::resolve(&Scope::default()).unwrap();
        assert!(scope.project_includes(None));
        assert!(!scope.project_includes(Some("app")));
    }

    #[test]
    fn all_projects_includes_everything() {
        let scope = EffectiveScope::resolve(&Scope {
            all_projects: true,
            ..Default::default()
        })
        .unwrap();
        assert!(scope.project_includes(Some("app")));
        assert!(scope.project_includes(Some("other")));
        assert!(scope.project_includes(None));
    }

    #[test]
    fn type_filter_respected() {
        let scope = EffectiveScope::resolve(&Scope {
            fragment_types: Some(vec![FragmentType::Warning]),
            ..Default::default()
        })
        .unwrap();
        assert!(scope.type_includes(&FragmentType::Warning));
        assert!(!scope.type_includes(&FragmentType::Fact));
    }

    #[test]
    fn date_filter_respected() {
        let scope = EffectiveScope::resolve(&Scope {
            after: Some(500),
            before: Some(1500),
            ..Default::default()
        })
        .unwrap();
        assert!(scope.date_includes(1000));
        assert!(!scope.date_includes(400));
        assert!(!scope.date_includes(1600));
    }

    #[test]
    fn confidence_filter_respected() {
        let scope = EffectiveScope::resolve(&Scope {
            min_confidence: Some(0.7),
            ..Default::default()
        })
        .unwrap();
        assert!(scope.confidence_includes(0.8));
        assert!(!scope.confidence_includes(0.5));
    }

    #[test]
    fn full_eligibility_check() {
        let scope = EffectiveScope::resolve(&Scope {
            project: Some("app".into()),
            fragment_types: Some(vec![FragmentType::Fact]),
            min_confidence: Some(0.5),
            ..Default::default()
        })
        .unwrap();

        let in_scope = memory(eid(1), Some("app"), FragmentType::Fact, 0.6);
        let wrong_project = memory(eid(2), Some("other"), FragmentType::Fact, 0.6);
        let wrong_type = memory(eid(3), Some("app"), FragmentType::Warning, 0.6);
        let low_conf = memory(eid(4), Some("app"), FragmentType::Fact, 0.3);

        assert!(scope.is_eligible(&in_scope));
        assert!(!scope.is_eligible(&wrong_project));
        assert!(!scope.is_eligible(&wrong_type));
        assert!(!scope.is_eligible(&low_conf));
    }

    #[test]
    fn lance_filter_project_scope() {
        let scope = EffectiveScope::resolve(&Scope {
            project: Some("app".into()),
            ..Default::default()
        })
        .unwrap();
        let filter = scope.to_lance_filter().unwrap();
        assert!(filter.contains("project = 'app'"));
        assert!(filter.contains("project IS NULL"));
    }

    #[test]
    fn lance_filter_all_projects_is_none() {
        let scope = EffectiveScope::resolve(&Scope {
            all_projects: true,
            ..Default::default()
        })
        .unwrap();
        assert!(scope.to_lance_filter().is_none());
    }

    #[test]
    fn lance_filter_escapes_single_quotes_in_project() {
        // A project name containing a single quote must not break or inject
        // into the filter predicate.
        let scope = EffectiveScope::resolve(&Scope {
            project: Some("o'brien".into()),
            ..Default::default()
        })
        .unwrap();
        let filter = scope.to_lance_filter().unwrap();
        assert!(
            filter.contains("project = 'o''brien'"),
            "single quote must be doubled: {filter}"
        );
        assert!(
            !filter.contains("o'brien' OR"),
            "unescaped quote must not terminate the literal early: {filter}"
        );
    }

    /// The central quoting rule, pinned: doubling, empty passthrough,
    /// backslashes untouched (literal in DataFusion, not escapes).
    #[test]
    fn sql_quote_doubles_only_quotes() {
        assert_eq!(sql_quote("plain"), "plain");
        assert_eq!(sql_quote(""), "");
        assert_eq!(sql_quote("o'brien"), "o''brien");
        assert_eq!(sql_quote("'''"), "''''''");
        assert_eq!(sql_quote("a\\b"), "a\\b");
        assert_eq!(sql_quote("x'; DROP TABLE t;--"), "x''; DROP TABLE t;--");
    }

    /// A non-finite min_confidence (only constructible natively) is
    /// rejected at the single entry point, never interpolated into a
    /// filter where NaN would silently match nothing.
    #[test]
    fn non_finite_min_confidence_is_rejected() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = EffectiveScope::resolve(&Scope {
                min_confidence: Some(bad),
                ..Default::default()
            })
            .unwrap_err();
            assert_eq!(err.code, DomainErrorCode::Validation);
        }
        // Finite bounds still resolve.
        assert!(
            EffectiveScope::resolve(&Scope {
                min_confidence: Some(0.5),
                ..Default::default()
            })
            .is_ok()
        );
    }
}

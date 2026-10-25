//! Guide rendering and construction (moved verbatim from `tools.rs`).

use crate::envelope::DomainPayload;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::guide::Guide;
use ltmrs_domain::id::EntityRevision;
use ltmrs_domain::memory::Instant;
use ltmrs_service::repository::{AdmittedScope, RecordedGuideOp};
use serde_json::{Value, json};

use super::{err_result, ok_result};

pub(crate) fn format_guide_detail(guide: &Guide) -> String {
    let mut detail = format!("=== GUIDE: {} ===\n", guide.name);
    detail.push_str(&format!("Category: {}\n", guide.category));
    detail.push_str(&format!("Usage Count: {}\n", guide.usage_count));
    detail.push_str(&format!(
        "Last Used: {}\n",
        guide
            .last_used
            .map(|i| super::recall::date_only(i.as_millis()))
            .unwrap_or_default()
    ));
    if !guide.description.is_empty() {
        detail.push_str(&format!(
            "\n=== DESCRIPTION / PROTOCOLS ===\n{}\n===============================\n",
            guide.description
        ));
    }
    if !guide.contexts.is_empty() {
        detail.push_str(&format!("Contexts: {}\n", guide.contexts.join(", ")));
    }
    if !guide.learnings.is_empty() {
        detail.push_str("Learnings:\n");
        for l in &guide.learnings {
            detail.push_str(&format!("  - {l}\n"));
        }
    }
    let total_attempts = guide.success_count + guide.failure_count;
    if total_attempts > 0 {
        let rate = guide.success_count as f64 / total_attempts as f64;
        detail.push_str(&format!(
            "Success Rate: {:.2} ({}/{})\n",
            rate, guide.success_count, total_attempts
        ));
    }
    if !guide.anti_patterns.is_empty() {
        detail.push_str("Anti-patterns:\n");
        for ap in &guide.anti_patterns {
            detail.push_str(&format!("  - {ap}\n"));
        }
    }
    if !guide.pitfalls.is_empty() {
        detail.push_str("Known Pitfalls:\n");
        for kp in &guide.pitfalls {
            detail.push_str(&format!("  - {kp}\n"));
        }
    }
    if !guide.depends_on.is_empty() {
        detail.push_str(&format!("Depends on: {}\n", guide.depends_on.join(", ")));
    }
    if !guide.enables.is_empty() {
        detail.push_str(&format!("Enables: {}\n", guide.enables.join(", ")));
    }
    if let Some(s) = &guide.superseded_by {
        detail.push_str(&format!("Superseded by: {s}\n"));
    }
    detail.push_str("====================");
    detail
}

pub(crate) fn guide_json(g: &Guide) -> Value {
    json!({
        "guide": g.name,
        "category": g.category,
        "description": g.description,
        "usage_count": g.usage_count,
        "last_used": g.last_used.map(|i| super::recall::date_only(i.as_millis())),
        "success_count": g.success_count,
        "failure_count": g.failure_count,
        "contexts": g.contexts,
        "learnings": g.learnings,
    })
}

/// Build a fresh guide record (upstream createGuide).
pub(crate) fn create_guide(
    name: &str,
    category: &str,
    description: &str,
    contexts: &[String],
    learnings: &[String],
    now: u64,
) -> Guide {
    Guide {
        name: name.to_lowercase().trim().to_string(),
        category: category.to_lowercase().trim().to_string(),
        description: description.trim().to_string(),
        contexts: contexts
            .iter()
            .map(|c| c.to_lowercase().trim().to_string())
            .filter(|c| !c.is_empty())
            .collect(),
        learnings: learnings
            .iter()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        usage_count: 1,
        last_used: Some(Instant::new(now)),
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
        created_at: Instant::new(now),
        updated_at: Instant::new(now),
    }
}

pub(crate) fn merge_guide_refs(
    existing: &[String],
    additions: &[String],
    self_name: &str,
) -> Vec<String> {
    let mut out: Vec<String> = existing.to_vec();
    for a in additions {
        let lower = a.to_lowercase().trim().to_string();
        if lower.is_empty() || lower == self_name {
            continue;
        }
        if !out.iter().any(|e| e.eq_ignore_ascii_case(&lower)) {
            out.push(lower);
        }
    }
    out
}

/// Rebuild one guide tool response from its recorded outcome (P1-2): pure
/// function of the recorded snapshot (+ merge sources), so first execution
/// and replay return byte-identical responses without re-applying anything.
pub(crate) fn guide_op_response(recorded: &RecordedGuideOp) -> DomainResult<DomainPayload> {
    use ltmrs_service::repository::GuideOpKind;
    let guide = recorded.guide.as_ref().ok_or_else(|| {
        DomainError::new(
            DomainErrorCode::Validation,
            "recorded guide outcome missing",
        )
    })?;
    Ok(match recorded.kind {
        GuideOpKind::Create => ok_result(
            format!(
                "Created new guide \"{}\" ({}) with a detailed manual.",
                guide.name, guide.category
            ),
            json!({ "success": true, "guide": guide.name }),
        ),
        GuideOpKind::CreateUpdate => ok_result(
            format!(
                "Updated manual for existing guide \"{}\" ({})",
                guide.name, guide.category
            ),
            json!({ "success": true, "guide": guide.name }),
        ),
        GuideOpKind::Update => ok_result(
            format!(
                "Updated guide \"{}\":\n{}",
                guide.name,
                format_guide_detail(guide)
            ),
            json!({ "success": true, "guide": guide.name }),
        ),
        GuideOpKind::Forget => ok_result(
            format!("Successfully forgot guide: {}", guide.name),
            json!({ "success": true, "guide": guide.name }),
        ),
        GuideOpKind::Merge => format_guide_merge_response(guide, &recorded.merged_sources),
    })
}

/// Map a guide tool receipt outcome to the legacy surface (P1-2): key reuse
/// and missing guides are tool errors; anything else is a wire error.
pub(crate) fn map_guide_tool_error(e: DomainError) -> DomainResult<DomainPayload> {
    if matches!(
        e.code,
        DomainErrorCode::NotFound
            | DomainErrorCode::KeyReuseDifferentInput
            | DomainErrorCode::RevisionConflict
            | DomainErrorCode::Validation
    ) {
        return Ok(err_result(&e.message));
    }
    Err(e)
}

/// Replay shortcut shared by the receipted guide tools (P1-2): when this
/// operation already completed, return its recorded response before any
/// planning read (a concurrent rename/forget/merge must not turn a replay
/// into a spurious "not found").
pub(crate) fn replay_recorded_guide_op(
    repo: &ltmrs_service::repository::CanonicalRepository,
    admitted: &AdmittedScope,
) -> DomainResult<Option<DomainPayload>> {
    match repo.read_recorded_guide_op(admitted) {
        Ok(None) => Ok(None),
        Ok(Some(recorded)) => Ok(Some(guide_op_response(&recorded)?)),
        Err(e)
            if e.code == DomainErrorCode::KeyReuseDifferentInput
                || e.code == DomainErrorCode::Validation =>
        {
            Ok(Some(err_result(&e.message)))
        }
        Err(e) => Err(e),
    }
}

/// Rebuild the merge tool response from the recorded outcome (P1-2): pure
/// function of the result snapshot + sources, identical on first execution
/// and on replay.
pub(crate) fn format_guide_merge_response(result: &Guide, sources: &[String]) -> DomainPayload {
    let mut response = format!(
        "Merged {} guides into \"{}\" ({})\n",
        sources.len(),
        result.name,
        result.category
    );
    response.push_str(&format!(
        "Total usage: {}x | Contexts: {} | Learnings: {}\n",
        result.usage_count,
        result.contexts.len(),
        result.learnings.len()
    ));
    response.push_str(&format!("Removed: {}", sources.join(", ")));

    let mut hook: Vec<String> = Vec::new();
    if !result.anti_patterns.is_empty() {
        hook.push(format!(
            "Anti-patterns inherited: {}",
            result.anti_patterns.len()
        ));
    }
    if !result.pitfalls.is_empty() {
        hook.push(format!("Pitfalls inherited: {}", result.pitfalls.len()));
    }
    if !result.source_memories.is_empty() {
        hook.push(format!(
            "Source memories linked: {} fragment(s)",
            result.source_memories.len()
        ));
    }
    if !result.validated_by.is_empty() {
        hook.push(format!(
            "Validated by: {} fragment(s)",
            result.validated_by.len()
        ));
    }
    if !hook.is_empty() {
        response.push_str("\n\n--- HOOK SUGGESTIONS ---\n");
        for h in &hook {
            response.push_str(&format!("{h}\n"));
        }
    }

    ok_result(
        response,
        json!({
            "success": true,
            "guide": result.name,
            "merged": sources,
        }),
    )
}

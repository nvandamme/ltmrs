//! conflict_scan / proactive_analysis / project_analytics tools (moved verbatim from `tools.rs`).

use std::collections::BTreeSet;

use crate::dispatcher::Dispatcher;
use crate::envelope::DomainPayload;
use ltmrs_compat::lemma::tool_args::{
    ConflictScanArgs, ProactiveAnalysisArgs, ProjectAnalyticsArgs,
};
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;
use ltmrs_search::similarity::{SimilarityPurpose, SimilarityQuery};
use serde_json::json;

use super::{format_result, ok_result, similarity_service};

// ---- conflict_scan ----

/// Conflict pairs over Lance-nearest-neighbor candidates: each recallable
/// memory proposes its top neighbors through the one similarity contract
/// and the frozen contradiction rules score each unordered pair once.
/// Without an indexed table the degraded snapshot proposes every other
/// memory, so small knowledge bases keep exact all-pairs behavior.
pub(crate) fn find_conflicts(
    disp: &Dispatcher,
    memories: &[Memory],
    project: Option<&str>,
) -> DomainResult<Vec<ltmrs_compat::lemma::intelligence::ConflictPair>> {
    use std::collections::BTreeMap;
    let repo = disp.repo();
    let svc = similarity_service(disp);
    let in_scope = |m: &Memory| match (project, m.project.as_deref()) {
        (None, _) => true,
        (Some(p), Some(mp)) => p == mp,
        (Some(_), None) => true,
    };
    let by_id: BTreeMap<EntityId, &Memory> = memories.iter().map(|m| (m.id, m)).collect();
    let mut seen: BTreeSet<(EntityId, EntityId)> = BTreeSet::new();
    let mut conflicts = Vec::new();
    for m in memories {
        let neighbors = svc.find_similar_sync(&SimilarityQuery {
            text: m.fragment.clone(),
            project: None,
            exclude: Some(m.id),
            limit: 20,
            purpose: SimilarityPurpose::ConflictCandidates,
        })?;
        for hit in neighbors {
            let other = match by_id.get(&hit.memory_id) {
                Some(other) => *other,
                None => continue,
            };
            // The neighbor index may lag canonical state, or admit
            // out-of-scope peers: re-validate both endpoints here.
            if !other.lifecycle.is_recallable() || !in_scope(other) {
                continue;
            }
            let (first, second): (&Memory, &Memory) = if m.id < other.id {
                (m, other)
            } else {
                (other, m)
            };
            if !seen.insert((first.id, second.id)) {
                continue;
            }
            if let Some(pair) = ltmrs_compat::lemma::intelligence::score_conflict_pair(
                first,
                second,
                repo.legacy_id(first),
                repo.legacy_id(second),
            ) {
                conflicts.push(pair);
            }
        }
    }
    conflicts.sort_by(|a, b| {
        b.overlap_score
            .partial_cmp(&a.overlap_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(conflicts)
}

pub(crate) fn exec_conflict_scan(
    disp: &Dispatcher,
    args: &ConflictScanArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;
    let export = repo.export_snapshot()?;
    let memories: Vec<Memory> = export
        .memories
        .iter()
        .filter(|m| m.lifecycle.is_recallable())
        .filter(|m| {
            args.project
                .as_deref()
                .map(|p| m.project.as_deref() == Some(p) || m.project.is_none())
                .unwrap_or(true)
        })
        .cloned()
        .collect();

    let conflicts = find_conflicts(disp, &memories, args.project.as_deref())?;
    let text = ltmrs_compat::lemma::intelligence::format_conflict_results(&conflicts);
    let data = json!({ "count": conflicts.len(), "conflicts": conflicts });
    Ok(format_result(text, data, format))
}

// ---- proactive_analysis ----

pub(crate) fn exec_proactive_analysis(
    disp: &Dispatcher,
    args: &ProactiveAnalysisArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;
    let export = repo.export_snapshot()?;
    let guides = repo.get_guides()?;
    let memories: Vec<Memory> = export
        .memories
        .iter()
        .filter(|m| m.lifecycle.is_recallable())
        .filter(|m| {
            args.project
                .as_deref()
                .map(|p| m.project.as_deref() == Some(p) || m.project.is_none())
                .unwrap_or(true)
        })
        .cloned()
        .collect();

    let legacy_id_of = |m: &Memory| repo.legacy_id(m);
    let now = disp.clock().now_millis();
    let mut suggestions =
        ltmrs_compat::lemma::intelligence::run_full_analysis(&memories, &guides, now, legacy_id_of);

    // Conflict count suggestion.
    let conflicts = find_conflicts(disp, &memories, args.project.as_deref())?;
    if !conflicts.is_empty() {
        suggestions.push(ltmrs_compat::lemma::intelligence::ProactiveSuggestion {
            r#type: "conflict".into(),
            priority: "high".into(),
            message: format!(
                "{} conflicting memory pair(s) detected. Run conflict_scan for details.",
                conflicts.len()
            ),
            suggested_action: None,
        });
    }

    let formatted = ltmrs_compat::lemma::intelligence::format_suggestions(&suggestions);
    let mut output = format!(
        "=== PROACTIVE ANALYSIS ===\nAnalyzed {} memories and {} guides.\n\n",
        memories.len(),
        guides.len()
    );
    output.push_str(&formatted);
    if suggestions.is_empty() {
        output = "=== PROACTIVE ANALYSIS ===\nNo issues detected. Knowledge base looks healthy."
            .to_string();
    }
    let data = json!({
        "count": suggestions.len(),
        "analyzed_memories": memories.len(),
        "analyzed_guides": guides.len(),
        "suggestions": suggestions,
    });
    Ok(format_result(output, data, format))
}

// ---- project_analytics ----

// ---- project_analytics ----

pub(crate) fn exec_project_analytics(
    disp: &Dispatcher,
    args: &ProjectAnalyticsArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;
    let now = disp.clock().now_millis();
    let export = repo.export_snapshot()?;
    let mut guides = repo.get_guides()?;
    guides.sort_by_key(|g| std::cmp::Reverse(g.usage_count));
    let sessions = disp.repo().all_sessions().unwrap();
    let memories: Vec<Memory> = export
        .memories
        .iter()
        .filter(|m| m.lifecycle.is_recallable())
        .cloned()
        .collect();

    match &args.project {
        None => {
            let all = ltmrs_compat::lemma::intelligence::get_all_projects_analytics(
                &sessions, &memories, &guides, now,
            );
            if all.is_empty() {
                return Ok(ok_result(
                    "No projects found with session or memory data.".to_string(),
                    json!({
                        "project": null,
                        "health_score": null,
                        "recent_insights": [],
                        "count": 0,
                        "projects": []
                    }),
                ));
            }
            let mut output = String::from("=== ALL PROJECTS OVERVIEW ===\n\n");
            for p in &all {
                output.push_str(&format!(
                    "{}: {} sessions, {} memories, health {:.0}%\n",
                    p.project,
                    p.total_sessions,
                    p.total_memories,
                    p.health_score * 100.0
                ));
            }
            let data = json!({
                "project": null,
                "health_score": null,
                "recent_insights": [],
                "count": all.len(),
                "projects": all,
            });
            Ok(format_result(output, data, format))
        }
        Some(project) => {
            let progress = ltmrs_compat::lemma::intelligence::get_project_analytics(
                project, &sessions, &memories, &guides, now,
            );
            let formatted = ltmrs_compat::lemma::intelligence::format_project_progress(&progress);
            let data = json!({
                "project": project,
                "total_sessions": progress.total_sessions,
                "total_memories": progress.total_memories,
                "total_guides": progress.total_guides,
                "knowledge_growth_rate": progress.knowledge_growth_rate,
                "skill_coverage": progress.skill_coverage,
                "recent_insights": progress.recent_insights,
                "health_score": progress.health_score,
            });
            Ok(format_result(formatted, data, format))
        }
    }
}

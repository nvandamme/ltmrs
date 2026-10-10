//! Recall domain: browse paths, graph expansion, explanations and
//! rendering (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use ltmrs_compat::lemma::tool_args::MemoryReadArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::{Memory, MemorySource};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::ids::legacy_id_of;

pub(crate) fn fragment_detail_json(
    repo: &ltmrs_service::repository::CanonicalRepository,
    m: &Memory,
) -> Value {
    json!({
        "id": legacy_id_of(repo, m),
        "title": m.title,
        "description": if m.description.is_empty() { Value::Null } else { json!(m.description) },
        "type": m.fragment_type.as_str(),
        "confidence": m.confidence,
        "project": m.project,
        "fragment": m.fragment,
        "created": m.created_at.as_millis(),
    })
}

/// Bounded graph expansion from a memory (depth ≤ 2).
pub(crate) fn expand_graph(
    repo: &ltmrs_service::repository::CanonicalRepository,
    root: EntityId,
) -> DomainResult<Vec<(Memory, u32, f64)>> {
    let relations = repo.all_relations()?;
    let mut seen: BTreeMap<EntityId, u32> = BTreeMap::new();
    seen.insert(root, 0);
    let mut frontier: Vec<(EntityId, u32)> = vec![(root, 0)];
    while let Some((id, depth)) = frontier.pop()
        && depth < 2
    {
        for r in &relations {
            let next = if r.source == id {
                Some(r.target)
            } else if r.target == id {
                Some(r.source)
            } else {
                None
            };
            if let Some(nid) = next
                && !seen.contains_key(&nid)
            {
                seen.insert(nid, depth + 1);
                frontier.push((nid, depth + 1));
            }
        }
    }
    let mut out = Vec::new();
    for (id, depth) in &seen {
        if *id == root {
            continue;
        }
        if let Some(m) = repo.get_memories(&[*id])?.first() {
            out.push((m.clone(), *depth, 1.0 / *depth as f64));
        }
    }
    out.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    Ok(out)
}

/// Recall memories for browse/query mode, with the provenance method of
/// whatever produced them. An attached backend serves authoritatively: a
/// Complete result stands even when empty (a converged no-match is a
/// legitimate answer, never papered over). Anything else — no backend, a
/// failed call, or a Partial result — falls back to the explicitly
/// labelled canonical snapshot scan, which never hides knowledge behind
/// an unconverged index.
pub(crate) fn recall_browse(
    disp: &Dispatcher,
    args: &MemoryReadArgs,
) -> DomainResult<(Vec<Memory>, &'static str)> {
    // Page through the engine, not around it: postfilters below mirror
    // these predicates, so the request carries them (plus headroom-free
    // exact paging) instead of truncating at a fixed 100 first.
    let limit = args.limit.unwrap_or(30).clamp(1, 100);
    let offset = args.offset.unwrap_or(0);
    if let Some(sb) = disp.search() {
        let req = ltmrs_search::retrieval::engine::RetrievalRequest {
            query: args.query.clone().unwrap_or_default(),
            scope: ltmrs_domain::command::Scope {
                project: args.project.clone(),
                all_projects: args.all,
                min_confidence: args.min_confidence,
                after: args.after_date.as_deref().and_then(parse_iso_date),
                before: args.before_date.as_deref().and_then(parse_iso_date),
                ..Default::default()
            },
            // Dense only when this backend serves vectors (lexical-only
            // tables run the lexical leg and report Complete when converged).
            model_fingerprint: sb.model_fingerprint(),
            result_limit: offset + limit,
            ..Default::default()
        };
        match sb.retrieve_sync(&req) {
            Ok(result) => {
                let method = recall_method(&result.explanation);
                let memories: Vec<Memory> = result.results.into_iter().map(|r| r.memory).collect();
                // Engine results stand, Complete or Partial alike: only a
                // Complete empty is a legitimate no-answer. A Partial empty
                // falls through to the labelled snapshot scan below.
                if !memories.is_empty() || !result.explanation.partial {
                    return Ok((memories, method));
                }
            }
            // Failed call: the labelled substring fallback below.
            Err(_) => return Ok((snapshot_recall(disp, args)?, "substring_fallback")),
        }
    } else {
        // No backend: the snapshot scan below is the only source.
        return Ok((snapshot_recall(disp, args)?, "degraded_snapshot"));
    }

    // Canonical snapshot fallback (Partial result).
    Ok((snapshot_recall(disp, args)?, "degraded_snapshot"))
}

/// Provenance method for an engine result, from its own explanation.
pub(crate) fn recall_method(
    explanation: &ltmrs_search::retrieval::explain::RetrievalExplanation,
) -> &'static str {
    if explanation.empty_query {
        return "confidence_browse";
    }
    if explanation
        .candidates
        .values()
        .any(|c| c.leg_ranks.iter().any(|l| l.leg == "dense"))
    {
        return "hybrid_rrf_mmr";
    }
    "lance_fts"
}

/// Canonical snapshot scan: the explicitly labelled degraded path, used
/// only when no backend is attached, the call failed, or the engine
/// reported Partial. Same ordering recipe as before (term overlap for
/// queries, stored confidence for browse).
pub(crate) fn snapshot_recall(
    disp: &Dispatcher,
    args: &MemoryReadArgs,
) -> DomainResult<Vec<Memory>> {
    let repo = disp.repo();
    let export = repo.export_snapshot()?;
    let mut memories: Vec<Memory> = export
        .memories
        .into_iter()
        .filter(|m| m.lifecycle.is_recallable())
        .filter(|m| {
            if args.all {
                true
            } else {
                match (&args.project, m.project.as_deref()) {
                    (None, None) => true,
                    (Some(_), None) => true,
                    (Some(p), Some(mp)) => p == mp,
                    (None, Some(_)) => false,
                }
            }
        })
        .collect();

    if let Some(query) = &args.query
        && !query.is_empty()
    {
        let q = query.to_lowercase();
        memories.sort_by(|a, b| {
            let ra = relevance(a, &q);
            let rb = relevance(b, &q);
            rb.partial_cmp(&ra)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
    } else {
        memories.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
    }
    Ok(memories)
}

pub(crate) fn relevance(m: &Memory, query: &str) -> f64 {
    let text = format!("{} {}", m.title, m.fragment).to_lowercase();
    query
        .split_whitespace()
        .filter(|t| text.contains(*t))
        .count() as f64
}

/// Format epoch millis as a date-only string (upstream `Created:` field).
pub(crate) fn date_only(millis: u64) -> String {
    let days = millis / 86_400_000;
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Format epoch millis as an ISO-8601 UTC timestamp.
pub(crate) fn iso8601(millis: u64) -> String {
    let secs = millis / 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{:03}Z",
        millis % 1000
    )
}

pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    (if mo <= 2 { y + 1 } else { y }, mo as u32, d as u32)
}

/// One selected memory for the recall explanation (frozen before the read
/// side effects so provenance shows pre-read values).
pub(crate) struct RecallSelection<'a> {
    pub(crate) memory: &'a Memory,
    pub(crate) method: &'static str,
    pub(crate) rank: Option<u64>,
    pub(crate) score: Option<f64>,
    pub(crate) graph: Option<(String, u32)>,
}

const RECALL_METHODS: &[(&str, &str, Option<&str>)] = &[
    (
        "explicit_id",
        "Requested by ID; no relevance ranking or project filter was applied.",
        None,
    ),
    (
        "graph_expansion",
        "Reached through stored relations from the requested ID; the graph score includes a depth penalty.",
        Some("graph_score_with_depth_penalty"),
    ),
    (
        "lance_fts",
        "Matched the keyword query; ordered by Lance full-text search rank.",
        Some("lance_fts_rank"),
    ),
    (
        "degraded_snapshot",
        "No search backend (or an unconverged index); matched over a canonical snapshot ordered by term overlap or stored confidence.",
        Some("stored_confidence"),
    ),
    (
        "confidence_browse",
        "No usable keyword terms; browsed records ordered by stored confidence.",
        Some("stored_confidence"),
    ),
    (
        "substring_fallback",
        "FTS search failed; the fallback matched text with LIKE and ordered by stored confidence.",
        Some("stored_confidence"),
    ),
    (
        "hybrid_rrf_mmr",
        "Selected from fused lexical and dense ranks, adjusted by recall priority and diversity. Display order includes MMR diversity reranking.",
        Some("hybrid_relevance_before_diversity"),
    ),
];

pub(crate) fn method_info(method: &str) -> (&'static str, Option<&'static str>) {
    RECALL_METHODS
        .iter()
        .find(|(m, _, _)| *m == method)
        .map(|(_, reason, kind)| (*reason, *kind))
        .unwrap_or(("", None))
}

/// Build the recall explanation for this call (upstream explainRecall).
/// Provenance is read from the pre-boost memories passed in `selections`.
pub(crate) fn explain_recall(
    repo: &ltmrs_service::repository::CanonicalRepository,
    selections: &[RecallSelection<'_>],
    mode: &str,
    project: Option<&str>,
) -> Value {
    let items: Vec<Value> = selections
        .iter()
        .map(|sel| {
            let m = sel.memory;
            let (reason, score_kind) = method_info(sel.method);
            let mut citations: Vec<Value> = Vec::new();
            for ev in m.evidence.iter().take(5) {
                citations.push(json!({
                    "file": ev.file,
                    "symbol": ev.symbol,
                    "recorded_at": iso8601(m.created_at.as_millis()),
                }));
            }
            let truncated = m.evidence.len() > 5;
            json!({
                "id": legacy_id_of(repo, m),
                "selection": {
                    "method": sel.method,
                    "reason": reason,
                    "rank": sel.rank,
                    "score": sel.score,
                    "score_kind": score_kind,
                    "graph": sel.graph.as_ref().map(|(root, depth)| json!({
                        "root_id": root,
                        "depth": depth,
                    })),
                },
                "provenance": {
                    "recorded_source": m.source.as_str(),
                    "created_at": iso8601(m.created_at.as_millis()),
                    "project": m.project,
                    "recorded_session_id": m.session_id,
                    "confidence_before_read": m.confidence,
                    "last_accessed_before_read": m.last_accessed_at.map(|t| iso8601(t.as_millis())),
                    "invalidated_at": null,
                    "citations": citations,
                },
                "freshness": {
                    "status": if m.evidence.is_empty() {
                        "no_evidence"
                    } else {
                        "not_checked"
                    },
                    "checked_at": null,
                    "checks": [],
                    "citations_truncated": truncated,
                },
            })
        })
        .collect();

    json!({
        "applies_to": "this_call",
        "scope": {
            "mode": mode,
            "project": project,
        },
        "notice": "Explains this call, not a past recall. Scores, access times and source labels are not proof of correctness. Evidence checks only test whether cited snippets are present; at most five citations per record are checked, and only when verification.stale_check is enabled.",
        "correction_tools": [
            { "tool": "memory_update", "use": "Correct the title or content after reviewing the record." },
            { "tool": "memory_forget", "use": "Use invalidate=true to hide outdated knowledge while retaining its history." },
            { "tool": "memory_relate", "use": "Link a confirmed replacement with supersedes, or record a contradiction with contradicts." },
        ],
        "items": items,
    })
}

/// Render the recall explanation as the appended text block (upstream
/// formatRecallExplanation).
pub(crate) fn format_recall_explanation(exp: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    for item in exp["items"].as_array().unwrap_or(&Vec::new()) {
        let id = item["id"].as_str().unwrap_or("?");
        let selection = &item["selection"];
        let reason = selection["reason"].as_str().unwrap_or("");
        let rank_s = selection["rank"]
            .as_u64()
            .map(|r| format!(" Rank {r}."))
            .unwrap_or_default();
        let score_s = match (
            selection["score"].as_f64(),
            selection["score_kind"].as_str(),
        ) {
            (Some(s), Some(k)) => format!(" Score {s} ({k})."),
            _ => String::new(),
        };
        let graph_s = selection["graph"].as_object().map(|g| {
            format!(
                " Root: {}, depth: {}.",
                g["root_id"].as_str().unwrap_or(""),
                g["depth"].as_u64().unwrap_or(0)
            )
        });
        let graph_s = graph_s.unwrap_or_default();
        let prov = &item["provenance"];
        let sources: String = match prov["citations"].as_array() {
            Some(c) if !c.is_empty() => c
                .iter()
                .map(|c| match c["symbol"].as_str() {
                    Some(s) => format!("{} ({s})", c["file"].as_str().unwrap_or("")),
                    None => c["file"].as_str().unwrap_or("").to_string(),
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "no citations".to_string(),
        };
        let status = item["freshness"]["status"].as_str().unwrap_or("");
        let truncated = item["freshness"]["citations_truncated"]
            .as_bool()
            .unwrap_or(false);
        let trunc_s = if truncated {
            " (first five citations only)"
        } else {
            ""
        };
        let invalid_s = prov["invalidated_at"]
            .as_str()
            .map(|v| format!(" Invalidated: {v}."))
            .unwrap_or_default();
        lines.push(format!(
            "- [{id}] {reason}{rank_s}{score_s}{graph_s}\n  Source label: {}. project: {}. created: {}. Evidence: {sources}. Status: {status}{trunc_s}{invalid_s}",
            prov["recorded_source"].as_str().unwrap_or(""),
            prov["project"].as_str().unwrap_or("global"),
            prov["created_at"].as_str().unwrap_or(""),
        ));
    }
    format!(
        "\n\n## Why these memories?\n{}\n{}\nTo correct: memory_update; to hide outdated knowledge reversibly: memory_forget invalidate=true; to link a replacement: memory_relate supersedes. Review before changing records.",
        lines.join("\n"),
        exp["notice"].as_str().unwrap_or("")
    )
}

/// Parse an ISO date (YYYY-MM-DD) to epoch millis at 00:00:00 UTC.
pub(crate) fn parse_iso_date(s: &str) -> Option<u64> {
    let d = s.trim();
    let bytes = d.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = d.get(0..4)?.parse().ok()?;
    let month: i64 = d.get(5..7)?.parse().ok()?;
    let day: i64 = d.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(1970..=9999).contains(&year) {
        return None;
    }
    // Days since epoch (civil-from-days algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days as u64 * 86_400_000)
}

pub(crate) fn render_summary_index(
    memories: &[Memory],
    scope_info: &str,
    legacy_id: &dyn Fn(&Memory) -> String,
) -> String {
    let project_header = if scope_info == "all projects" || scope_info == "global" {
        String::new()
    } else {
        format!(" ({scope_info})")
    };
    if memories.is_empty() {
        return format!("## Memory Fragments{project_header}\n---\n(no fragments)\n---");
    }
    let mut out = format!("## Memory Fragments{project_header}\n---\n");
    for m in memories {
        let scope_tag = m.project.as_deref().unwrap_or("global");
        let summary = if m.description.is_empty() {
            m.title.clone()
        } else {
            m.description.clone()
        };
        out.push_str(&format!(
            "[{}] [{scope_tag}] {} — {summary}\n",
            legacy_id(m),
            m.title
        ));
    }
    out.push_str("---");
    out
}

pub(crate) fn render_detail(
    legacy_id: &str,
    m: &Memory,
    resolve: &dyn Fn(&EntityId) -> String,
) -> String {
    let bar_count = (m.confidence / 0.2).round() as usize;
    let confidence_bar = format!(
        "{}{}",
        "█".repeat(bar_count.min(5)),
        "░".repeat(5 - bar_count.min(5))
    );
    let source_icon = if m.source == MemorySource::Ai {
        "🤖"
    } else {
        "👤"
    };
    let scope_tag = m
        .project
        .as_ref()
        .map(|p| format!("[{p}]"))
        .unwrap_or_else(|| "[global]".to_string());

    let mut detail = String::from("=== MEMORY FRAGMENT DETAIL ===\n");
    detail.push_str(&format!(
        "ID: [{legacy_id}] {confidence_bar} ({source_icon}) {scope_tag}\n"
    ));
    detail.push_str(&format!("Title: {}\n", m.title));
    if !m.description.is_empty() && m.description != m.title {
        detail.push_str(&format!("Summary: {}\n", m.description));
    }
    detail.push_str(&format!(
        "Created: {} | Confidence: {:.2}\n",
        date_only(m.created_at.as_millis()),
        m.confidence
    ));
    if !m.tags.is_empty() {
        detail.push_str(&format!("Tags: {}\n", m.tags.join(", ")));
    }
    if !m.associated_with.is_empty() {
        detail.push_str(&format!("Related: {}\n", m.associated_with.join(", ")));
    }
    if !m.relations.is_empty() {
        detail.push_str("Relations:\n");
        for rel in &m.relations {
            detail.push_str(&format!(
                "  {} → [{}]{}\n",
                rel.relation_type.as_str(),
                resolve(&rel.target),
                rel.note
                    .as_deref()
                    .map(|n| format!(" — {n}"))
                    .unwrap_or_default()
            ));
        }
    }
    if m.positive_feedback > 0 || m.negative_feedback > 0 {
        detail.push_str(&format!(
            "Feedback: {} positive, {} negative\n",
            m.positive_feedback, m.negative_feedback
        ));
    }
    if m.refinement_count > 0 {
        detail.push_str(&format!("Refinements: {}\n", m.refinement_count));
    }
    if let Some(parent) = &m.parent_id {
        detail.push_str(&format!("Refined from: [{}]\n", resolve(parent)));
    }
    if !m.child_ids.is_empty() {
        let children: Vec<String> = m
            .child_ids
            .iter()
            .map(|c| format!("[{}]", resolve(c)))
            .collect();
        detail.push_str(&format!("Refined into: {}\n", children.join(", ")));
    }
    detail.push_str(&format!("--- CONTENT ---\n{}\n==============", m.fragment));
    detail
}

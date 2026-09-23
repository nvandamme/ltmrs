//! Legacy MCP tool execution (WP-08).
//!
//! Executes the 11 WP-08 tools against the canonical repository and search
//! backend, shaping responses to match the frozen Lemma 0.21.0 wire contract
//! (text + structuredContent + error flag). This is the compatibility
//! adapter: it maps ltmrs canonical state to the legacy observable surface.
//!
//! Design 11.1: preserves the full supported Lemma API/workflow surface.
//! Design 12.1: memory_add privacy (confirm override, DEV-002).
//! RQ-17: read side effects persisted before success.

use std::collections::{BTreeMap, BTreeSet};

use crate::compatibility::lemma::privacy;
use crate::compatibility::lemma::tool_args::{
    MemoryAddArgs, MemoryAuditArgs, MemoryFeedbackArgs, MemoryForgetArgs, MemoryLibraryArgs,
    MemoryMergeArgs, MemoryReadArgs, MemoryRelateArgs, MemoryStatsArgs, MemoryUpdateArgs,
    ResponseFormat, SemanticSearchArgs, ToolArgs,
};
use crate::daemon::dispatcher::Dispatcher;
use crate::daemon::envelope::{DomainPayload, IpcEnvelope};
use crate::domain::command::{
    CommandContext, DomainCommand, DomainError, DomainErrorCode, DomainResult, ForgetMode,
    MemoryPatch,
};
use crate::domain::id::{EntityId, OperationId};
use crate::domain::memory::{Evidence, FragmentType, Instant, Memory, MemorySource};
use crate::domain::relation::{Relation, RelationType};
use serde_json::{Value, json};

/// Execute a typed tool call, returning the shaped legacy result.
pub fn execute_tool(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    tool: &ToolArgs,
) -> DomainResult<DomainPayload> {
    match tool {
        ToolArgs::MemoryRead(args) => exec_memory_read(disp, envelope, args),
        ToolArgs::MemoryAdd(args) => exec_memory_add(disp, envelope, args),
        ToolArgs::MemoryUpdate(args) => exec_memory_update(disp, envelope, args),
        ToolArgs::MemoryFeedback(args) => exec_memory_feedback(disp, envelope, args),
        ToolArgs::MemoryForget(args) => exec_memory_forget(disp, envelope, args),
        ToolArgs::MemoryMerge(args) => exec_memory_merge(disp, envelope, args),
        ToolArgs::MemoryRelate(args) => exec_memory_relate(disp, envelope, args),
        ToolArgs::MemoryStats(args) => exec_memory_stats(disp, args),
        ToolArgs::MemoryAudit(args) => exec_memory_audit(disp, args),
        ToolArgs::MemoryLibrary(args) => exec_memory_library(disp, args),
        ToolArgs::SemanticSearch(args) => exec_semantic_search(disp, envelope, args),
    }
}

// ---- Result constructors ----

fn ok_result(text: String, structured: Value) -> DomainPayload {
    DomainPayload::ToolResult {
        text,
        structured: Some(structured),
        is_error: false,
    }
}

fn err_result(message: &str) -> DomainPayload {
    DomainPayload::ToolResult {
        text: format!("Error: {message}"),
        structured: None,
        is_error: true,
    }
}

/// Build a result honoring the frozen response_format (json => text is the
/// JSON-encoded data, matching upstream buildResult).
fn format_result(text: String, data: Value, format: Option<ResponseFormat>) -> DomainPayload {
    if format == Some(ResponseFormat::Json) {
        return ok_result(data.to_string(), data);
    }
    ok_result(text, data)
}

fn legacy_id_of(repo: &crate::service::repository::CanonicalRepository, m: &Memory) -> String {
    repo.legacy_id(m)
}

fn legacy_id_of_from_id(
    repo: &crate::service::repository::CanonicalRepository,
    id: EntityId,
) -> String {
    repo.get_memories(&[id])
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|m| repo.legacy_id(&m))
        .unwrap_or_else(|| id.as_uuid().to_string())
}

fn resolve_id(
    repo: &crate::service::repository::CanonicalRepository,
    id: &str,
) -> DomainResult<EntityId> {
    repo.resolve_id(id)
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Legacy string ID for a new memory: derived deterministically from the
/// operation ID (unique per operation, stable across retries).
fn new_legacy_id(envelope: &IpcEnvelope) -> String {
    format!(
        "m{}",
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("ltmrs:memory:{}", envelope.operation_id.as_uuid()).as_bytes(),
        )
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect::<String>()
    )
}

/// Canonical project key: trimmed + lowercased, path collapsed to basename.
/// "global" (case-insensitive) maps to None.
fn normalize_project(raw: &str) -> Option<String> {
    let mut p = raw.trim().to_string();
    if p.is_empty() {
        return None;
    }
    p = p.replace('\\', "/");
    p = p.trim_end_matches('/').to_string();
    if p.contains('/') {
        p = p.rsplit('/').next().unwrap_or("").to_string();
    }
    p = p.trim().to_lowercase();
    if p.is_empty() || p == "global" {
        None
    } else {
        Some(p)
    }
}

/// Auto-title: first 40 chars (truncated with "...") or the fragment itself.
fn generate_title(fragment: &str) -> String {
    if fragment.len() > 40 {
        format!("{}...", &fragment[..40])
    } else {
        fragment.to_string()
    }
}

/// Auto-description: first sentence if short, else first 80 chars.
fn generate_description(fragment: &str) -> String {
    // Upstream (JavaScript) measures length and slices in UTF-16 code units,
    // not bytes or Unicode code points. Replicate that exactly.
    let units: Vec<u16> = fragment.encode_utf16().collect();
    if units.len() <= 80 {
        return fragment.to_string();
    }
    let first = fragment.split(['.', '!', '?', '\n']).next().unwrap_or("");
    let first_units = first.encode_utf16().count();
    if !first.is_empty() && first_units <= 100 {
        // `first` is everything before the first delimiter, so it never ends
        // with '.' — upstream always appends '...'.
        return format!("{}...", first.trim());
    }
    // Upstream: fragment.substring(0, 80).trim() + '...'
    // from_utf16_lossy handles surrogate pairs correctly and maps a lone
    // surrogate (from cutting mid-emoji) to U+FFFD, matching JS behavior.
    let truncated = String::from_utf16_lossy(&units[..80]);
    format!("{}...", truncated.trim())
}

/// Word-overlap similarity (Jaccard on whitespace tokens).
fn word_overlap(a: &str, b: &str) -> f64 {
    let ta: BTreeSet<String> = a
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.to_string())
        .collect();
    let tb: BTreeSet<String> = b
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.to_string())
        .collect();
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count();
    let union = ta.union(&tb).count();
    inter as f64 / union as f64
}

/// Build a command context for the Nth sub-command of a tool call.
///
/// A tool call may issue several canonical commands (e.g. memory_add also
/// auto-links). Each needs its own operation key so the receipt ledger cannot
/// replay the first command in place of the second. The derived keys are
/// deterministic in the envelope, so a retried tool call replays cleanly.
fn sub_command_ctx(envelope: &IpcEnvelope, index: u32) -> DomainResult<CommandContext> {
    let base_digest = envelope.request_digest()?;
    let op = OperationId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:cmd{}", envelope.operation_id.as_uuid(), index).as_bytes(),
    ));
    let mut ctx = envelope.to_command_context(format!("{base_digest}:cmd{index}"));
    ctx.operation_id = op;
    Ok(ctx)
}

/// Record read side effects (RQ-17) via the canonical gateway: confidence
/// boost, access counters, last-accessed timestamp and the optional context
/// tag — persisted before the read response reports success.
fn record_access(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    ids: &[EntityId],
    context: Option<&str>,
) -> DomainResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let ctx = sub_command_ctx(envelope, 0)?;
    let cmd = DomainCommand::Access {
        memory_ids: ids.to_vec(),
        context: context.map(|c| c.to_string()),
    };
    disp.repo().apply(&ctx, &cmd)?;
    Ok(())
}

fn relation_exists(
    repo: &crate::service::repository::CanonicalRepository,
    source: EntityId,
    target: EntityId,
    rtype: RelationType,
) -> DomainResult<bool> {
    Ok(repo
        .all_relations()?
        .iter()
        .any(|r| r.source == source && r.target == target && r.relation_type == rtype))
}

/// Create a relation with a deterministic ID derived from the operation.
fn new_relation(
    envelope: &IpcEnvelope,
    source: EntityId,
    target: EntityId,
    rtype: RelationType,
    note: Option<String>,
) -> Relation {
    let rid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!(
            "ltmrs:rel:{}:{}:{}:{}",
            envelope.operation_id.as_uuid(),
            source.as_uuid(),
            target.as_uuid(),
            rtype.as_str()
        )
        .as_bytes(),
    ));
    Relation::new(rid, source, target, rtype, note, Instant::new(0))
}

// ---- memory_read ----

fn exec_memory_read(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &MemoryReadArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;

    // Single-ID detail.
    if let Some(id) = &args.id {
        let Ok(eid) = resolve_id(repo, id) else {
            return Ok(err_result(&format!("Fragment with ID '{id}' not found")));
        };
        let mems = repo.get_memories(&[eid])?;
        let Some(m_pre) = mems.first() else {
            return Ok(err_result(&format!("Fragment with ID '{id}' not found")));
        };
        let mut selections: Vec<RecallSelection> = vec![RecallSelection {
            memory: m_pre,
            method: "explicit_id",
            rank: None,
            score: None,
            graph: None,
        }];
        record_access(disp, envelope, &[m_pre.id], args.context.as_deref())?;
        // Render the post-boost record (upstream boostOnAccess returns the
        // boosted fragment which is what gets formatted).
        let mems = repo.get_memories(&[eid])?;
        let Some(m) = mems.first() else {
            return Ok(err_result(&format!("Fragment with ID '{id}' not found")));
        };
        let resolver = |eid: &EntityId| legacy_id_of_from_id(repo, *eid);
        let mut text = render_detail(&legacy_id_of(repo, m), m, &resolver);
        let mut related_graph: Vec<Value> = Vec::new();
        let graph = if args.expand_graph {
            expand_graph(repo, m.id)?
        } else {
            Vec::new()
        };
        if !graph.is_empty() {
            text.push_str("\n\n## Related knowledge (graph, depth ≤ 2)");
            for (gm, depth, score) in &graph {
                related_graph.push(json!({
                    "id": legacy_id_of(repo, gm),
                    "title": gm.title,
                    "depth": depth,
                    "score": score,
                }));
                selections.push(RecallSelection {
                    memory: gm,
                    method: "graph_expansion",
                    rank: None,
                    score: Some(*score),
                    graph: Some((legacy_id_of(repo, m), *depth)),
                });
                text.push_str(&format!(
                    "\n- [{}] (depth {}, score {:.2}) {}: {}",
                    legacy_id_of(repo, gm),
                    depth,
                    score,
                    gm.title,
                    if gm.description.is_empty() {
                        gm.fragment.chars().take(80).collect::<String>()
                    } else {
                        gm.description.clone()
                    }
                ));
            }
        }
        let recall_explanation = if args.explain {
            Some(explain_recall(repo, &selections, "explicit_ids", None))
        } else {
            None
        };
        if let Some(exp) = &recall_explanation {
            text.push_str(&format_recall_explanation(exp));
        }
        let mut data = json!({
            "count": 1,
            "fragments": [fragment_detail_json(repo, m)],
            "has_more": false,
            "next_offset": null,
        });
        if !related_graph.is_empty() {
            data["related_graph"] = json!(related_graph);
        }
        if let Some(exp) = recall_explanation {
            data["recall_explanation"] = json!(exp);
        }
        return Ok(format_result(text, data, format));
    }

    // Batch-ID detail.
    if let Some(ids) = &args.ids
        && !ids.is_empty()
    {
        let mut results: Vec<String> = Vec::new();
        let mut fragments: Vec<Value> = Vec::new();
        let mut accessed: Vec<EntityId> = Vec::new();
        let mut found: Vec<Memory> = Vec::new();
        let resolver = |eid: &EntityId| legacy_id_of_from_id(repo, *eid);
        for id in ids {
            let m = match resolve_id(repo, id) {
                Ok(eid) => repo.get_memories(&[eid])?.first().cloned(),
                Err(_) => None,
            };
            match m {
                Some(m) => {
                    accessed.push(m.id);
                    fragments.push(fragment_detail_json(repo, &m));
                    results.push(render_detail(&legacy_id_of(repo, &m), &m, &resolver));
                    found.push(m);
                }
                None => results.push(format!("Fragment [{id}] not found.")),
            }
        }
        record_access(disp, envelope, &accessed, args.context.as_deref())?;
        let selections: Vec<RecallSelection> = found
            .iter()
            .map(|m| RecallSelection {
                memory: m,
                method: "explicit_id",
                rank: None,
                score: None,
                graph: None,
            })
            .collect();
        let recall_explanation = if args.explain && !selections.is_empty() {
            Some(explain_recall(repo, &selections, "explicit_ids", None))
        } else {
            None
        };
        let mut data = json!({
            "count": fragments.len(),
            "fragments": fragments,
            "has_more": false,
            "next_offset": null,
        });
        let mut text = results.join("\n\n");
        if let Some(exp) = &recall_explanation {
            text.push_str(&format_recall_explanation(exp));
        }
        if let Some(exp) = recall_explanation {
            data["recall_explanation"] = json!(exp);
        }
        return Ok(format_result(text, data, format));
    }

    // Browse or query mode.
    let limit = args.limit.unwrap_or(30).clamp(1, 100);
    let offset = args.offset.unwrap_or(0);

    let mut memories = recall_browse(disp, args)?;

    // Post-filters (dates, min_confidence) applied on top of the search result.
    memories.retain(|m| {
        if let Some(min) = args.min_confidence
            && m.confidence < min
        {
            return false;
        }
        if let Some(after) = args.after_date.as_deref().and_then(parse_iso_date)
            && m.created_at.as_millis() < after
        {
            return false;
        }
        if let Some(before) = args.before_date.as_deref().and_then(parse_iso_date)
            && m.created_at.as_millis() > before
        {
            return false;
        }
        true
    });

    let total = memories.len();
    let page: Vec<Memory> = memories.iter().skip(offset).take(limit).cloned().collect();
    let has_more = offset + limit < total;
    let next_offset = if has_more { offset + limit } else { 0 };

    // Build the recall explanation from the pre-boost page (upstream captures
    // provenance before boostOnAccess mutates confidence/access counters).
    let method = if args.query.is_some() && !args.query.as_deref().unwrap().is_empty() {
        "fts5_bm25"
    } else {
        "confidence_browse"
    };
    let selections: Vec<RecallSelection> = page
        .iter()
        .enumerate()
        .map(|(i, m)| RecallSelection {
            memory: m,
            method,
            rank: Some(offset as u64 + i as u64 + 1),
            score: None,
            graph: None,
        })
        .collect();
    let recall_explanation = if args.explain && !selections.is_empty() {
        Some(explain_recall(
            repo,
            &selections,
            if args.all {
                "all_projects"
            } else {
                "project_and_global"
            },
            if args.all {
                None
            } else {
                args.project.as_deref()
            },
        ))
    } else {
        None
    };

    let accessed: Vec<EntityId> = page.iter().map(|m| m.id).collect();
    record_access(disp, envelope, &accessed, args.context.as_deref())?;

    let scope_info = if args.all {
        "all projects".to_string()
    } else {
        args.project.clone().unwrap_or_else(|| "global".to_string())
    };
    let lid = |m: &Memory| legacy_id_of(repo, m);
    let mut text = render_summary_index(&page, &scope_info, &lid);
    if has_more {
        text.push_str(&format!(
            "\nShowing {} of {} fragments (offset {}). Pass offset={next_offset} for the next page, or use the query parameter to search.",
            page.len(),
            total,
            offset
        ));
    } else if args.query.is_none() {
        text.push_str(&format!(
            "\nShowing top {} fragments (ranked by relevance). Use query parameter to search, or id for a specific fragment.",
            page.len()
        ));
    }

    if let Some(exp) = &recall_explanation {
        text.push_str(&format_recall_explanation(exp));
    }

    let fragments: Vec<Value> = page
        .iter()
        .map(|m| {
            json!({
                "id": legacy_id_of(repo, m),
                "title": m.title,
                "description": if m.description.is_empty() { Value::Null } else { json!(m.description) },
                "type": m.fragment_type.as_str(),
                "confidence": m.confidence,
                "project": m.project,
            })
        })
        .collect();

    let mut data = json!({
        "count": page.len(),
        "total": total,
        "fragments": fragments,
        "has_more": has_more,
        "next_offset": if has_more { Some(next_offset) } else { None },
    });
    if let Some(exp) = recall_explanation {
        data["recall_explanation"] = json!(exp);
    }
    Ok(format_result(text, data, format))
}

fn fragment_detail_json(
    repo: &crate::service::repository::CanonicalRepository,
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
fn expand_graph(
    repo: &crate::service::repository::CanonicalRepository,
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

/// Recall memories for browse/query mode: search backend when available,
/// else a canonical snapshot scan (lexical fallback).
fn recall_browse(disp: &Dispatcher, args: &MemoryReadArgs) -> DomainResult<Vec<Memory>> {
    if let Some(sb) = disp.search() {
        let req = crate::retrieval::engine::RetrievalRequest {
            query: args.query.clone().unwrap_or_default(),
            scope: crate::domain::command::Scope {
                project: args.project.clone(),
                all_projects: args.all,
                min_confidence: args.min_confidence,
                ..Default::default()
            },
            result_limit: 100,
            ..Default::default()
        };
        if let Ok(result) = sb.retrieve_sync(&req) {
            return Ok(result.results.into_iter().map(|r| r.memory).collect());
        }
    }

    // Canonical snapshot fallback.
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

fn relevance(m: &Memory, query: &str) -> f64 {
    let text = format!("{} {}", m.title, m.fragment).to_lowercase();
    query
        .split_whitespace()
        .filter(|t| text.contains(*t))
        .count() as f64
}

/// Format epoch millis as a date-only string (upstream `Created:` field).
fn date_only(millis: u64) -> String {
    let days = millis / 86_400_000;
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Format epoch millis as an ISO-8601 UTC timestamp.
fn iso8601(millis: u64) -> String {
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

fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
struct RecallSelection<'a> {
    memory: &'a Memory,
    method: &'static str,
    rank: Option<u64>,
    score: Option<f64>,
    graph: Option<(String, u32)>,
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
        "fts5_bm25",
        "Matched the keyword query; ordered by FTS5 BM25 (lower scores rank first).",
        Some("bm25_lower_is_better"),
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
        "tfidf_cosine",
        "Selected by TF-IDF cosine similarity to the query.",
        Some("tfidf_cosine_similarity"),
    ),
    (
        "hybrid_rrf_mmr",
        "Selected from fused lexical and TF-IDF ranks, adjusted by recall priority and diversity. Display order includes MMR diversity reranking.",
        Some("hybrid_relevance_before_diversity"),
    ),
];

fn method_info(method: &str) -> (&'static str, Option<&'static str>) {
    RECALL_METHODS
        .iter()
        .find(|(m, _, _)| *m == method)
        .map(|(_, reason, kind)| (*reason, *kind))
        .unwrap_or(("", None))
}

/// Build the recall explanation for this call (upstream explainRecall).
/// Provenance is read from the pre-boost memories passed in `selections`.
fn explain_recall(
    repo: &crate::service::repository::CanonicalRepository,
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
fn format_recall_explanation(exp: &Value) -> String {
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
fn parse_iso_date(s: &str) -> Option<u64> {
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

fn render_summary_index(
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

fn render_detail(legacy_id: &str, m: &Memory, resolve: &dyn Fn(&EntityId) -> String) -> String {
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

// ---- memory_add ----

fn exec_memory_add(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &MemoryAddArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    // Privacy: redact secrets unless confirm=true (design 12.1, DEV-002).
    let has_secrets = privacy::contains_secret(&args.fragment);
    let final_fragment = if has_secrets && !args.confirm {
        privacy::redact(&args.fragment)
    } else {
        args.fragment.clone()
    };

    // Deduplication: reject if a similar fragment already exists.
    let export = repo.export_snapshot()?;
    let similar = export
        .memories
        .iter()
        .filter(|m| m.lifecycle.is_recallable())
        .filter(|m| match (&args.project, m.project.as_deref()) {
            (None, _) => true,
            (Some(p), Some(mp)) => p == mp,
            (Some(_), None) => true,
        })
        .find(|m| word_overlap(&final_fragment, &m.fragment) >= 0.80);
    if let Some(similar) = similar {
        let sid = legacy_id_of(repo, similar);
        return Ok(err_result(&format!(
            "A similar memory already exists [{sid}]: \"{}\"\nUse memory_update on [{sid}] if you want to modify it.",
            similar.title
        )));
    }

    // Resolve fragment type.
    let fragment_type = args
        .fragment_type
        .as_deref()
        .and_then(FragmentType::parse)
        .unwrap_or(FragmentType::Fact);

    // Resolve source.
    let source = args
        .source
        .as_deref()
        .and_then(MemorySource::parse)
        .unwrap_or(MemorySource::Ai);

    // Resolve project.
    let project = args.project.as_deref().and_then(normalize_project);

    // Generate title and description.
    let title = args
        .title
        .clone()
        .unwrap_or_else(|| generate_title(&final_fragment));
    let description = args
        .description
        .clone()
        .unwrap_or_else(|| generate_description(&final_fragment));

    // Build evidence.
    let evidence = args
        .evidence
        .as_ref()
        .map(|e| {
            vec![Evidence {
                file: e.file.clone(),
                symbol: e.symbol.clone(),
                snippet: e.snippet.clone(),
                snippet_sha256: sha256_hex(&e.snippet),
            }]
        })
        .unwrap_or_default();

    // Build the memory.
    let legacy_id = new_legacy_id(envelope);
    let eid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:entity:{}", envelope.operation_id.as_uuid()).as_bytes(),
    ));
    let now = disp.clock().now_millis();
    let memory = Memory {
        id: eid,
        external_alias: Some(crate::domain::id::ExternalAlias::new(legacy_id.clone())),
        title: title.clone(),
        fragment: final_fragment.clone(),
        description: description.clone(),
        fragment_type,
        project: project.clone(),
        source,
        confidence: 1.0,
        quality_score: None,
        lifecycle: crate::domain::memory::MemoryLifecycle::Live,
        tags: Vec::new(),
        associated_with: Vec::new(),
        relations: Vec::new(),
        parent_id: None,
        child_ids: Vec::new(),
        session_id: None,
        task_type: None,
        related_guides: Vec::new(),
        evidence,
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: 0,
        negative_feedback: 0,
        negative_hits: 0,
        refinement_count: 0,
        distill_candidate: matches!(fragment_type, FragmentType::Pattern | FragmentType::Lesson),
        entity_revision: crate::domain::id::EntityRevision::new(0),
        document_revision: crate::domain::id::DocumentRevision::new(0),
        eligibility_revision: crate::domain::id::EligibilityRevision::new(0),
        created_at: Instant::new(now),
        updated_at: Instant::new(now),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    };

    // Apply the command.
    let cmd = DomainCommand::AddMemory {
        memory: memory.clone(),
        session: None,
    };
    let ctx = sub_command_ctx(envelope, 0)?;
    disp.repo().apply(&ctx, &cmd)?;

    // Find topic overlaps for auto-linking.
    let overlaps: Vec<&Memory> = export
        .memories
        .iter()
        .filter(|m| m.lifecycle.is_recallable())
        .filter(|m| m.id != eid)
        .filter(|m| {
            let score = word_overlap(&final_fragment, &m.fragment);
            (0.25..0.95).contains(&score)
        })
        .take(5)
        .collect();

    // Build response text.
    let scope_info = project
        .as_deref()
        .map(|p| format!(" (project: {p})"))
        .unwrap_or_else(|| " (global)".to_string());
    let mut response =
        format!("Added fragment [{legacy_id}]{scope_info}: \"{title}\"\nSummary: {description}");
    if memory.distill_candidate {
        response.push_str(&format!(
            "\nFlagged as distill candidate (type: {}).",
            fragment_type.as_str()
        ));
    }
    if has_secrets && !args.confirm {
        response.push_str(
            "\n\n⚠️ Privacy: potential secret(s) detected and auto-redacted. Use confirm: true to store as-is.",
        );
    }

    // Auto-link to topic overlaps.
    if !overlaps.is_empty() {
        let strongest = &overlaps[0];
        let strongest_id = legacy_id_of(repo, strongest);
        let rel = new_relation(
            envelope,
            eid,
            strongest.id,
            RelationType::RelatedTo,
            Some(format!(
                "Auto-linked: topic overlap ({:.2})",
                word_overlap(&final_fragment, &strongest.fragment)
            )),
        );
        let rel_cmd = DomainCommand::Relate { relation: rel };
        let rel_ctx = sub_command_ctx(envelope, 1)?;
        if disp.repo().apply(&rel_ctx, &rel_cmd).is_ok() {
            response.push_str("\n\nRelated memories (auto-linked to strongest match):");
            response.push_str(&format!(
                "\n  [{strongest_id}] \"{}\" ({:.2}) — AUTO-LINKED",
                strongest.title, strongest.confidence
            ));
            for o in &overlaps[1..] {
                response.push_str(&format!(
                    "\n  [{}] \"{}\" ({:.2})",
                    legacy_id_of(repo, o),
                    o.title,
                    o.confidence
                ));
            }
        }
    }

    // Add suggestions for pattern/lesson.
    if matches!(fragment_type, FragmentType::Pattern | FragmentType::Lesson) {
        response.push_str(&format!(
            "\n\nSUGGESTED ACTIONS:\n- This is a {}. Consider guide_distill to promote it into a reusable skill.",
            fragment_type.as_str()
        ));
    }

    // Distill candidate count suggestion.
    let distill_count = export
        .memories
        .iter()
        .filter(|m| m.distill_candidate && m.lifecycle.is_recallable())
        .count()
        + 1; // +1 for the new one
    if distill_count >= 3 {
        response.push_str(&format!(
            "\n--- SUGGESTIONS ---\n  [*] {distill_count} memories marked as distill candidates. Consider promoting them to guides.\n---"
        ));
    }

    let structured = json!({
        "success": true,
        "id": legacy_id,
        "conflicts": [],
    });
    Ok(ok_result(response, structured))
}

// ---- memory_update ----

fn exec_memory_update(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &MemoryUpdateArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    let Ok(eid) = resolve_id(repo, &args.id) else {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    };
    let mems = repo.get_memories(&[eid])?;
    let Some(m) = mems.first() else {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    };

    // Validate confidence range.
    if let Some(c) = args.confidence
        && !(0.0..=1.0).contains(&c)
    {
        return Ok(err_result("'confidence' must be a number between 0 and 1"));
    }

    // Duplicate detection on fragment change.
    if let Some(fragment) = &args.fragment {
        let export = repo.export_snapshot()?;
        let similar = export
            .memories
            .iter()
            .filter(|m2| m2.id != eid && m2.lifecycle.is_recallable())
            .find(|m2| word_overlap(fragment, &m2.fragment) >= 0.80);
        if let Some(similar) = similar {
            let sid = legacy_id_of(repo, similar);
            return Ok(err_result(&format!(
                "Similar fragment already exists: [{sid}] \"{}\". Use a different content or update the existing one.",
                similar.title
            )));
        }
    }

    let patch = MemoryPatch {
        title: args.title.clone(),
        fragment: args.fragment.clone(),
        description: None,
        fragment_type: None,
        project: None,
        confidence: args.confidence,
        quality_score: None,
        tags: None,
        evidence: None,
    };

    let cmd = DomainCommand::UpdateMemory {
        id: eid,
        expected_revision: None,
        patch,
    };
    let ctx = sub_command_ctx(envelope, 0)?;
    disp.repo().apply(&ctx, &cmd)?;

    let display_title = args.title.clone().unwrap_or_else(|| m.title.clone());
    let mut response = format!("Updated fragment [{}]: \"{}\"", args.id, display_title);
    if args.fragment.is_some() {
        response.push_str("\nOrphan relations cleaned up after content change.");
    }

    let structured = json!({
        "success": true,
        "id": args.id,
    });
    Ok(ok_result(response, structured))
}

// ---- memory_feedback ----

fn exec_memory_feedback(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &MemoryFeedbackArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    let Ok(eid) = resolve_id(repo, &args.id) else {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    };
    let mems = repo.get_memories(&[eid])?;
    if mems.is_empty() {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    }

    let cmd = DomainCommand::Feedback {
        memory_id: eid,
        useful: args.useful,
    };
    let ctx = sub_command_ctx(envelope, 0)?;
    disp.repo().apply(&ctx, &cmd)?;

    // Re-read to get the updated confidence.
    let updated_mems = repo.get_memories(&[eid])?;
    let updated = updated_mems.first().unwrap();
    let new_confidence = updated.confidence;

    let response = if args.useful {
        format!(
            "Positive feedback recorded for [{}]. Confidence boosted to {:.2}.",
            args.id, new_confidence
        )
    } else {
        format!(
            "Negative feedback recorded for [{}]. Confidence reduced to {:.2}.",
            args.id, new_confidence
        )
    };

    let structured = json!({
        "success": true,
        "id": args.id,
        "confidence": new_confidence,
    });
    Ok(ok_result(response, structured))
}

// ---- memory_forget ----

fn exec_memory_forget(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &MemoryForgetArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    let Ok(eid) = resolve_id(repo, &args.id) else {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    };
    let mems = repo.get_memories(&[eid])?;
    if mems.is_empty() {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    }

    let response = if args.invalidate {
        // Logical invalidation: hide from recall, keep content + history.
        let cmd = DomainCommand::Forget {
            id: eid,
            mode: ForgetMode::Invalidate,
        };
        let ctx = sub_command_ctx(envelope, 0)?;
        disp.repo().apply(&ctx, &cmd)?;
        format!(
            "Invalidated fragment [{}] — hidden from recall but preserved (content + history kept). Reversible.",
            args.id
        )
    } else if args.consolidate {
        // Non-destructive archive: down-weight to 0.05, keep the row.
        let patch = MemoryPatch {
            confidence: Some(0.05),
            ..Default::default()
        };
        let cmd = DomainCommand::UpdateMemory {
            id: eid,
            expected_revision: None,
            patch,
        };
        let ctx = sub_command_ctx(envelope, 0)?;
        disp.repo().apply(&ctx, &cmd)?;
        format!(
            "Archived fragment [{}] — down-weighted to 0.05 (kept and reversible), not deleted. Pass consolidate=false to hard-delete.",
            args.id
        )
    } else {
        // Hard delete.
        let cmd = DomainCommand::Forget {
            id: eid,
            mode: ForgetMode::Delete,
        };
        let ctx = sub_command_ctx(envelope, 0)?;
        disp.repo().apply(&ctx, &cmd)?;
        format!("Forgot fragment with ID: {}", args.id)
    };

    let structured = json!({
        "success": true,
        "id": args.id,
    });
    Ok(ok_result(response, structured))
}

// ---- memory_merge ----

fn exec_memory_merge(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &MemoryMergeArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    if args.ids.len() < 2 {
        return Ok(err_result(
            "'ids' must be an array with at least 2 fragment IDs",
        ));
    }

    // Resolve all source IDs; any missing is a hard error.
    let mut source_ids: Vec<EntityId> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    for id in &args.ids {
        match resolve_id(repo, id) {
            Ok(eid) => {
                if repo.get_memories(&[eid])?.is_empty() {
                    not_found.push(id.clone());
                } else {
                    source_ids.push(eid);
                }
            }
            Err(_) => not_found.push(id.clone()),
        }
    }
    if !not_found.is_empty() {
        return Ok(err_result(&format!(
            "Fragment(s) not found: {}",
            not_found.join(", ")
        )));
    }

    // Build the merged memory.
    let legacy_id = new_legacy_id(envelope);
    let eid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:entity:{}", envelope.operation_id.as_uuid()).as_bytes(),
    ));
    let project = args.project.as_deref().and_then(normalize_project);
    let now = disp.clock().now_millis();
    let result = Memory {
        id: eid,
        external_alias: Some(crate::domain::id::ExternalAlias::new(legacy_id.clone())),
        title: args.title.clone(),
        fragment: args.fragment.clone(),
        description: generate_description(&args.fragment),
        fragment_type: FragmentType::Fact,
        project: project.clone(),
        source: MemorySource::Ai,
        confidence: 1.0,
        quality_score: None,
        lifecycle: crate::domain::memory::MemoryLifecycle::Live,
        tags: Vec::new(),
        associated_with: Vec::new(),
        relations: Vec::new(),
        parent_id: None,
        child_ids: Vec::new(),
        session_id: None,
        task_type: None,
        related_guides: Vec::new(),
        evidence: Vec::new(),
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: 0,
        negative_feedback: 0,
        negative_hits: 0,
        refinement_count: 0,
        distill_candidate: false,
        entity_revision: crate::domain::id::EntityRevision::new(0),
        document_revision: crate::domain::id::DocumentRevision::new(0),
        eligibility_revision: crate::domain::id::EligibilityRevision::new(0),
        created_at: Instant::new(now),
        updated_at: Instant::new(now),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    };

    let source_ids_cloned = source_ids.clone();
    let cmd = DomainCommand::Merge { source_ids, result };
    let ctx = sub_command_ctx(envelope, 0)?;
    disp.repo().apply(&ctx, &cmd)?;

    // For the consolidate path, record explicit supersession edges from the
    // merged fragment to each source (upstream marks sources superseded).
    if args.consolidate {
        let sources = repo.get_memories(&source_ids_cloned)?;
        for (i, src) in sources.iter().enumerate() {
            let rel = new_relation(
                envelope,
                eid,
                src.id,
                RelationType::Supersedes,
                Some("consolidated".to_string()),
            );
            let rel_ctx = sub_command_ctx(envelope, 1 + i as u32)?;
            let _ = disp
                .repo()
                .apply(&rel_ctx, &DomainCommand::Relate { relation: rel });
        }
    }

    let scope_info = project
        .as_ref()
        .map(|p| format!(" (project: {p})"))
        .unwrap_or_else(|| " (global)".to_string());
    let response = if args.consolidate {
        format!(
            "Consolidated {} fragments into [{legacy_id}]{scope_info}: \"{}\"\nSuperseded (kept, down-weighted, reversible) IDs: {}",
            args.ids.len(),
            args.title,
            args.ids.join(", ")
        )
    } else {
        format!(
            "Merged {} fragments into [{legacy_id}]{scope_info}: \"{}\"\nRemoved IDs: {}",
            args.ids.len(),
            args.title,
            args.ids.join(", ")
        )
    };

    let structured = json!({
        "success": true,
        "id": legacy_id,
        "merged_ids": args.ids,
    });
    Ok(ok_result(response, structured))
}

// ---- memory_relate ----

fn exec_memory_relate(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &MemoryRelateArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    let rtype = RelationType::parse(&args.relation_type).ok_or_else(|| {
        DomainError::new(
            DomainErrorCode::Validation,
            "'type' must be one of: contradicts, supersedes, supports, related_to".to_string(),
        )
    })?;

    let Ok(source_eid) = resolve_id(repo, &args.source_id) else {
        return Ok(err_result(&format!(
            "Source fragment [{}] not found",
            args.source_id
        )));
    };
    let Ok(target_eid) = resolve_id(repo, &args.target_id) else {
        return Ok(err_result(&format!(
            "Target fragment [{}] not found",
            args.target_id
        )));
    };

    if source_eid == target_eid {
        return Ok(err_result("sourceId and targetId cannot be the same"));
    }

    if repo.get_memories(&[source_eid])?.is_empty() {
        return Ok(err_result(&format!(
            "Source fragment [{}] not found",
            args.source_id
        )));
    }
    if repo.get_memories(&[target_eid])?.is_empty() {
        return Ok(err_result(&format!(
            "Target fragment [{}] not found",
            args.target_id
        )));
    }

    // Reject an identical existing edge.
    if relation_exists(repo, source_eid, target_eid, rtype)? {
        return Ok(err_result(&format!(
            "Relation already exists between [{}] and [{}] with type '{}'",
            args.source_id, args.target_id, args.relation_type
        )));
    }

    let relation = new_relation(envelope, source_eid, target_eid, rtype, args.note.clone());
    let cmd = DomainCommand::Relate { relation };
    let ctx = sub_command_ctx(envelope, 0)?;
    disp.repo().apply(&ctx, &cmd)?;

    let response = format!(
        "Created relation: [{}] --{}--> [{}]{}",
        args.source_id,
        args.relation_type,
        args.target_id,
        args.note
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default()
    );

    let structured = json!({
        "success": true,
        "relation": args.relation_type,
    });
    Ok(ok_result(response, structured))
}

// ---- memory_stats ----

/// Filter memories by project scope (upstream `filterByProject` semantics).
/// No project → only global fragments. Project → that project's fragments
/// (case-insensitive) plus global fragments (project inheritance is deliberate).
#[cfg(test)]
fn filter_by_project<'a>(memories: &'a [Memory], current_project: Option<&str>) -> Vec<&'a Memory> {
    let project = current_project
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty());
    memories
        .iter()
        .filter(|m| match (&project, m.project.as_deref()) {
            (None, mp) => mp.is_none(),
            (Some(p), Some(mp)) => mp.to_lowercase() == *p,
            (Some(_), None) => true,
        })
        .collect()
}

/// Calculate memory statistics (upstream `getMemoryStats` SQL semantics —
/// the real memory_stats tool path, NOT the dead-code pure `calculateStats`).
/// - No project → all fragments. Project → strict case-insensitive match on
///   the stored project; global fragments are EXCLUDED.
/// - Globals are labeled `(global)` (SQL COALESCE).
/// - avg_confidence is the raw mean (no rounding).
/// - low/high_confidence are null (not 0) on an empty store (SQL SUM).
/// - by_source/by_project keys in byte order (SQLite GROUP BY, BINARY collation).
fn calculate_stats(memories: &[Memory], project: Option<&str>) -> Value {
    let filter = project
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty());

    let filtered: Vec<&Memory> = memories
        .iter()
        .filter(|m| match (&filter, m.project.as_deref()) {
            (Some(p), Some(mp)) => mp.to_lowercase() == *p,
            (Some(_), None) => false,
            (None, _) => true,
        })
        .collect();

    let total = filtered.len();
    let avg_confidence = if total > 0 {
        filtered.iter().map(|m| m.confidence).sum::<f64>() / total as f64
    } else {
        0.0
    };
    let mut by_source: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_project: BTreeMap<String, u64> = BTreeMap::new();
    let mut low: Option<u64> = None;
    let mut high: Option<u64> = None;
    for m in &filtered {
        *by_source.entry(m.source.as_str().to_string()).or_insert(0) += 1;
        let scope = m.project.clone().unwrap_or_else(|| "(global)".to_string());
        *by_project.entry(scope).or_insert(0) += 1;
        low = Some(low.unwrap_or(0) + u64::from(m.confidence < 0.3));
        high = Some(high.unwrap_or(0) + u64::from(m.confidence > 0.8));
    }

    json!({
        "total": total,
        "avg_confidence": avg_confidence,
        "by_source": by_source,
        "by_project": by_project,
        "low_confidence": low,
        "high_confidence": high,
    })
}

/// Format statistics as text (upstream `formatStats` semantics).
fn format_stats(stats: &Value) -> String {
    let total = stats["total"].as_u64().unwrap_or(0);
    let avg_confidence = stats["avg_confidence"].as_f64().unwrap_or(0.0);
    let high = stats["high_confidence"].as_u64().unwrap_or(0);
    let low = stats["low_confidence"].as_u64().unwrap_or(0);

    let mut text = String::from("## Memory Stats\n");
    text.push_str(&format!(
        "Total: {total} fragments | Avg confidence: {avg_confidence}\n"
    ));
    if total > 0 {
        text.push_str(&format!(
            "High confidence (>0.8): {high} | Low (<0.3): {low}\n"
        ));
        if let Some(sources) = stats["by_source"].as_object() {
            let sources_str = sources
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("Sources: {sources_str}\n"));
        }
        if let Some(projects) = stats["by_project"].as_object() {
            let projects_str = projects
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("Projects: {projects_str}\n"));
        }
    }
    text
}

fn exec_memory_stats(disp: &Dispatcher, args: &MemoryStatsArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let export = repo.export_snapshot()?;

    let stats = calculate_stats(&export.memories, args.project.as_deref());
    let text = format_stats(&stats);

    Ok(format_result(text, stats, args.response_format))
}

// ---- memory_audit ----

/// Audit memory integrity (upstream `auditMemory` semantics).
/// `legacy_id_of` maps a memory to its display ID; `assoc_exists` reports
/// whether a legacy associated-with ID resolves to a live memory;
/// `legacy_id_for` resolves an entity ID to its display ID (for relation edges).
fn audit_memory<F, G, H>(
    memories: &[Memory],
    relations: &[Relation],
    legacy_id_of: F,
    assoc_exists: G,
    legacy_id_for: H,
) -> Value
where
    F: Fn(&Memory) -> String,
    G: Fn(&str) -> bool,
    H: Fn(EntityId) -> String,
{
    let mut issues: Vec<String> = Vec::new();
    let ids: BTreeSet<EntityId> = memories.iter().map(|m| m.id).collect();
    let mut seen: BTreeSet<EntityId> = BTreeSet::new();
    let mut duplicates: Vec<String> = Vec::new();

    for m in memories {
        if !seen.insert(m.id) {
            duplicates.push(legacy_id_of(m));
        }
        if !(0.0..=1.0).contains(&m.confidence) {
            issues.push(format!(
                "Fragment [{}] has invalid confidence: {}",
                legacy_id_of(m),
                m.confidence
            ));
        }
        if m.fragment.is_empty() {
            issues.push(format!(
                "Fragment [{}] has missing or invalid fragment text",
                legacy_id_of(m)
            ));
        }
        for assoc in &m.associated_with {
            if !assoc_exists(assoc) {
                issues.push(format!(
                    "Fragment [{}] references non-existent associated fragment [{assoc}]",
                    legacy_id_of(m)
                ));
            }
        }
    }
    // Dangling relation edges (relations live in a separate keyspace; the
    // observable equivalent of upstream's per-fragment relation check).
    for rel in relations {
        if !ids.contains(&rel.source) {
            issues.push(format!(
                "Fragment [{}] has relation to non-existent fragment [{}]",
                legacy_id_for(rel.target),
                legacy_id_for(rel.source)
            ));
        }
        if !ids.contains(&rel.target) {
            issues.push(format!(
                "Fragment [{}] has relation to non-existent fragment [{}]",
                legacy_id_for(rel.source),
                legacy_id_for(rel.target)
            ));
        }
    }
    if !duplicates.is_empty() {
        issues.push(format!("Duplicate IDs found: {}", duplicates.join(", ")));
    }

    json!({
        "total_fragments": memories.len(),
        "issues_found": issues.len(),
        "issues": issues,
        "healthy": issues.is_empty(),
    })
}

/// Format an audit report as text (upstream `formatAuditReport` semantics).
fn format_audit_report(result: &Value) -> String {
    let total_fragments = result["total_fragments"].as_u64().unwrap_or(0);
    let issues_found = result["issues_found"].as_u64().unwrap_or(0);

    let mut text = String::from("## Memory Audit\n");
    text.push_str(&format!(
        "Total fragments: {total_fragments} | Issues: {issues_found}\n"
    ));
    if issues_found > 0 {
        if let Some(issues) = result["issues"].as_array() {
            for issue in issues {
                text.push_str(&format!("  ! {}\n", issue.as_str().unwrap_or("")));
            }
        }
    } else {
        text.push_str("All clear — no issues found.\n");
    }
    text
}

fn exec_memory_audit(disp: &Dispatcher, args: &MemoryAuditArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let export = repo.export_snapshot()?;

    let result = audit_memory(
        &export.memories,
        &export.relations,
        |m| legacy_id_of(repo, m),
        |assoc| {
            repo.resolve_id(assoc)
                .ok()
                .and_then(|eid| repo.get_memories(&[eid]).ok())
                .map(|v| !v.is_empty())
                .unwrap_or(false)
        },
        |id| legacy_id_of_from_id(repo, id),
    );
    let text = format_audit_report(&result);

    Ok(format_result(text, result, args.response_format))
}

// ---- memory_library ----

fn exec_memory_library(disp: &Dispatcher, args: &MemoryLibraryArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let export = repo.export_snapshot()?;
    let now = disp.clock().now_millis();

    let focus = args.focus.as_deref().unwrap_or("full");
    let limit = args.limit.unwrap_or(50).clamp(1, 200);
    let offset = args.offset.unwrap_or(0);

    let project_filter = args.project.as_deref().map(|p| p.trim().to_lowercase());

    let need_fragments = matches!(focus, "full" | "stale" | "duplicates" | "orphans");
    let need_guides = matches!(focus, "full" | "guides");
    let need_relations = matches!(focus, "full" | "orphans");
    let need_signals = matches!(
        focus,
        "full" | "stale" | "duplicates" | "distill" | "guides"
    );

    // Fragments (project-scoped).
    let mut fragments: Vec<&Memory> = export
        .memories
        .iter()
        .filter(|m| {
            if let Some(pf) = &project_filter {
                m.project.as_deref().map(|p| p == pf).unwrap_or(false)
            } else {
                true
            }
        })
        .collect();
    fragments.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let frag_total = fragments.len();
    let page: Vec<&Memory> = fragments.iter().skip(offset).take(limit).copied().collect();
    let has_more = offset + limit < frag_total;
    let next_offset = if has_more { offset + limit } else { 0 };

    let fragment_jsons: Vec<Value> = page
        .iter()
        .map(|m| {
            let age_days = now.saturating_sub(m.created_at.as_millis()) / 86_400_000;
            json!({
                "id": legacy_id_of(repo, m),
                "title": m.title,
                "type": m.fragment_type.as_str(),
                "project": m.project,
                "confidence": m.confidence,
                "age_days": age_days,
                "access_count": m.access_count,
                "positive_feedback": m.positive_feedback,
                "negative_feedback": m.negative_feedback,
                "distill_candidate": m.distill_candidate,
                "fragment_preview": m.fragment.chars().take(80).collect::<String>(),
            })
        })
        .collect();

    // Guides.
    let guides_jsons: Vec<Value> = if need_guides {
        export
            .guides
            .iter()
            .map(|g| {
                json!({
                    "name": g.name,
                    "category": g.category,
                    "usage_count": g.usage_count,
                    "success_count": g.success_count,
                    "failure_count": g.failure_count,
                    "last_used": g.last_used.map(|i| i.as_millis()),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // Relations.
    let (rel_total, rel_by_type, isolated, hubs) = if need_relations {
        let mut by_type: BTreeMap<String, u64> = BTreeMap::new();
        for r in &export.relations {
            *by_type
                .entry(r.relation_type.as_str().to_string())
                .or_insert(0) += 1;
        }
        let connected: BTreeSet<EntityId> = export
            .relations
            .iter()
            .flat_map(|r| [r.source, r.target])
            .collect();
        let isolated: Vec<String> = export
            .memories
            .iter()
            .filter(|m| !connected.contains(&m.id))
            .map(|m| legacy_id_of(repo, m))
            .collect();
        let mut counts: BTreeMap<EntityId, u64> = BTreeMap::new();
        for r in &export.relations {
            *counts.entry(r.source).or_insert(0) += 1;
        }
        let hubs: Vec<Value> = counts
            .iter()
            .filter(|(_, c)| **c >= 5)
            .filter_map(|(id, c)| {
                export.memories.iter().find(|m| &m.id == id).map(|m| {
                    json!({
                        "id": legacy_id_of(repo, m),
                        "title": m.title,
                        "count": c,
                    })
                })
            })
            .collect();
        (export.relations.len(), by_type, isolated, hubs)
    } else {
        (0, BTreeMap::new(), Vec::new(), Vec::new())
    };

    // Signals.
    let (signals, suggestions) = if need_signals {
        let mut conf_dist: BTreeMap<String, u64> = BTreeMap::new();
        for b in ["0.0-0.2", "0.2-0.4", "0.4-0.6", "0.6-0.8", "0.8-1.0"] {
            conf_dist.insert(b.to_string(), 0);
        }
        let mut age_dist: BTreeMap<String, u64> = BTreeMap::new();
        for b in ["< 7", "7-30", "30-90", "> 90"] {
            age_dist.insert(b.to_string(), 0);
        }
        let mut stale: Vec<Value> = Vec::new();
        let mut distill: Vec<Value> = Vec::new();
        let mut never_accessed = 0u64;
        for m in &fragments {
            let conf = m.confidence;
            let bucket = if conf < 0.2 {
                "0.0-0.2"
            } else if conf < 0.4 {
                "0.2-0.4"
            } else if conf < 0.6 {
                "0.4-0.6"
            } else if conf < 0.8 {
                "0.6-0.8"
            } else {
                "0.8-1.0"
            };
            *conf_dist.get_mut(bucket).unwrap() += 1;
            let age = now.saturating_sub(m.created_at.as_millis()) / 86_400_000;
            let abucket = if age < 7 {
                "< 7"
            } else if age < 30 {
                "7-30"
            } else if age < 90 {
                "30-90"
            } else {
                "> 90"
            };
            *age_dist.get_mut(abucket).unwrap() += 1;
            if m.access_count == 0 {
                never_accessed += 1;
                if age > 30 && conf < 0.5 {
                    stale.push(json!({
                        "id": legacy_id_of(repo, m),
                        "title": m.title,
                        "age_days": age,
                        "confidence": m.confidence,
                    }));
                }
            }
            if m.distill_candidate {
                distill.push(json!({
                    "id": legacy_id_of(repo, m),
                    "title": m.title,
                    "type": m.fragment_type.as_str(),
                }));
            }
        }
        let mut suggestions: Vec<String> = Vec::new();
        if distill.len() >= 3 {
            suggestions.push(format!(
                "{} memories marked as distill candidates. Consider promoting them to guides.",
                distill.len()
            ));
        }
        let signals = json!({
            "confidence_distribution": conf_dist,
            "age_distribution": age_dist,
            "stale_fragments": stale,
            "distill_candidates": distill,
            "never_accessed_count": never_accessed,
        });
        (signals, suggestions)
    } else {
        (json!({}), Vec::new())
    };

    let structured = json!({
        "fragments": fragment_jsons,
        "guides": guides_jsons,
        "relations": {
            "total": rel_total,
            "by_type": rel_by_type,
            "isolated_fragment_ids": isolated,
            "hub_fragments": hubs,
        },
        "signals": signals,
        "suggestions": if suggestions.is_empty() { Value::Null } else { json!(suggestions) },
        "fragments_total": frag_total,
        "has_more": has_more,
        "next_offset": next_offset,
    });

    // Text rendering.
    let mut text = String::from("== LIBRARY MODE SNAPSHOT ==\n");
    text.push_str(&format!("Generated: {now}\n"));
    text.push_str(&format!(
        "Total memories: {} | Total guides: {} | Total sessions: 0\n",
        frag_total,
        guides_jsons.len()
    ));
    if need_fragments {
        text.push_str(&format!(
            "\n== MEMORY FRAGMENTS ({} shown of {}) ==\n",
            page.len(),
            frag_total
        ));
        for f in &page {
            text.push_str(&format!(
                "[{}] {} (conf {:.2}, {})\n",
                legacy_id_of(repo, f),
                f.title,
                f.confidence,
                f.project.as_deref().unwrap_or("global")
            ));
        }
    }
    if has_more {
        text.push_str(&format!(
            "\nShowing {} of {} fragments (offset {}). Pass offset={next_offset} for the next page.",
            page.len(),
            frag_total,
            offset
        ));
    }

    Ok(format_result(text, structured, args.response_format))
}

// ---- semantic_search ----

fn exec_semantic_search(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    args: &SemanticSearchArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;

    let top_k = args.top_k.unwrap_or(10).clamp(1, 30);
    let offset = args.offset.unwrap_or(0);

    // Use the search backend when available.
    let mut scored: Vec<(Memory, f64)> = Vec::new();
    if let Some(sb) = disp.search() {
        let req = crate::retrieval::engine::RetrievalRequest {
            query: args.query.clone(),
            scope: crate::domain::command::Scope {
                project: args.project.clone(),
                all_projects: false,
                ..Default::default()
            },
            result_limit: top_k + offset,
            ..Default::default()
        };
        if let Ok(result) = sb.retrieve_sync(&req) {
            // Map each result to its engine score (native calibrated score,
            // falling back to the legacy reference score). Not a claim of
            // identical TF-IDF — the legacy `score` field is a display value.
            let scores = &result.explanation.candidates;
            for r in result.results {
                let s = scores
                    .get(&r.memory.id)
                    .map(|c| c.scores.native_score.max(c.scores.legacy_reference))
                    .unwrap_or(0.5);
                scored.push((r.memory, s));
            }
        }
    }

    // Lexical fallback when the backend is unavailable or returned nothing.
    if scored.is_empty() {
        let export = repo.export_snapshot()?;
        let q = args.query.to_lowercase();
        let mut candidates: Vec<(Memory, f64)> = export
            .memories
            .iter()
            .filter(|m| m.lifecycle.is_recallable())
            .filter(|m| {
                if let Some(p) = &args.project {
                    m.project.as_deref() == Some(p.as_str()) || m.project.is_none()
                } else {
                    true
                }
            })
            .map(|m| (m.clone(), relevance(m, &q)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        candidates.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        scored = candidates;
    }

    let total = scored.len();
    let page: Vec<(Memory, f64)> = scored
        .iter()
        .skip(offset)
        .take(top_k)
        .map(|(m, s)| (m.clone(), *s))
        .collect();
    let has_more = offset + top_k < total;
    let next_offset = if has_more { offset + top_k } else { 0 };

    if page.is_empty() {
        let text = format!(
            "No semantically similar memories found for: \"{}\"",
            args.query
        );
        let data = json!({
            "count": 0,
            "total": total,
            "results": [],
            "has_more": has_more,
            "next_offset": if has_more { Some(next_offset) } else { None },
        });
        return Ok(format_result(text, data, format));
    }

    let mut text = format!(
        "=== SEMANTIC SEARCH RESULTS ===\nQuery: \"{}\"\nFound {} similar memories:\n\n",
        args.query,
        page.len()
    );
    let mut results_json: Vec<Value> = Vec::new();
    for (m, score) in &page {
        let preview: String = m.fragment.chars().take(100).collect();
        text.push_str(&format!(
            "  [{}%] [{}] \"{}\"\n      {}...\n",
            (*score * 100.0).round() as u64,
            legacy_id_of(repo, m),
            m.title,
            preview
        ));
        results_json.push(json!({
            "id": legacy_id_of(repo, m),
            "title": m.title,
            "score": score,
            "fragment_preview": m.fragment.chars().take(200).collect::<String>(),
        }));
    }
    if has_more {
        text.push_str(&format!(
            "\nMore results available. Pass offset={next_offset} for the next page."
        ));
    }

    let data = json!({
        "count": page.len(),
        "total": total,
        "results": results_json,
        "has_more": has_more,
        "next_offset": if has_more { Some(next_offset) } else { None },
    });
    Ok(format_result(text, data, format))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::dispatcher::Dispatcher;
    use crate::daemon::envelope::{DomainRequest, IpcEnvelope, PROTOCOL_VERSION};
    use crate::domain::clock::FrozenClock;
    use crate::domain::command::Scope;
    use crate::domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
    use crate::service::repository::CanonicalRepository;
    use std::sync::Arc;
    use uuid::Uuid;

    fn fe(n: u64) -> FrontendId {
        FrontendId::new(Uuid::from_u128(n as u128))
    }
    fn ch(n: u64) -> ChannelId {
        ChannelId::new(Uuid::from_u128(n as u128))
    }

    fn test_dispatcher() -> (Dispatcher, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let registry = crate::daemon::registry::FrontendRegistry::new();
        (Dispatcher::new(repo, registry, clock), dir)
    }

    fn envelope(op: u64, body: DomainRequest) -> IpcEnvelope {
        IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe(1),
            channel_id: ch(1),
            operation_id: OperationId::new(Uuid::from_u128(op as u128)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body,
        }
    }

    fn tool_call(op: u64, tool: ToolArgs) -> IpcEnvelope {
        envelope(op, DomainRequest::ToolCall { tool })
    }

    fn run(disp: &Dispatcher, env: &IpcEnvelope, tool: &ToolArgs) -> DomainPayload {
        execute_tool(disp, env, tool).unwrap()
    }

    fn add_fragment(disp: &Dispatcher, op: u64, fragment: &str) -> String {
        let env = tool_call(
            op,
            ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: fragment.to_string(),
                ..Default::default()
            }),
        );
        let result = run(
            disp,
            &env,
            &ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: fragment.to_string(),
                ..Default::default()
            }),
        );
        if let DomainPayload::ToolResult {
            structured: Some(s),
            is_error: false,
            ..
        } = &result
        {
            s["id"].as_str().unwrap().to_string()
        } else {
            panic!("memory_add failed: {}", result_text(&result));
        }
    }

    fn result_text(p: &DomainPayload) -> String {
        match p {
            DomainPayload::ToolResult { text, .. } => text.clone(),
            _ => String::new(),
        }
    }

    fn result_structured(p: &DomainPayload) -> Option<Value> {
        match p {
            DomainPayload::ToolResult { structured, .. } => structured.clone(),
            _ => None,
        }
    }

    fn result_is_error(p: &DomainPayload) -> bool {
        match p {
            DomainPayload::ToolResult { is_error, .. } => *is_error,
            _ => false,
        }
    }

    // ---- memory_add ----

    #[test]
    fn memory_add_stores_and_returns_id() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: "## Test Fragment\n\n### Context\nA test memory.\n".to_string(),
                project: Some("testproj".to_string()),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: "## Test Fragment\n\n### Context\nA test memory.\n".to_string(),
                project: Some("testproj".to_string()),
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        let text = result_text(&result);
        assert!(text.contains("Added fragment [m"));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["success"], json!(true));
        assert!(structured["id"].as_str().unwrap().starts_with("m"));
    }

    #[test]
    fn memory_add_redacts_secrets_by_default() {
        let (disp, _dir) = test_dispatcher();
        let frag = "api_key = sk_abc1234567890";
        let env = tool_call(
            1,
            ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: frag.to_string(),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: frag.to_string(),
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        // The stored fragment must be redacted.
        let structured = result_structured(&result).unwrap();
        let id = structured["id"].as_str().unwrap().to_string();
        let eid = disp.repo().resolve_id(&id).unwrap();
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(mems[0].fragment.contains("[REDACTED]"));
        assert!(!mems[0].fragment.contains("sk_abc1234567890"));
    }

    #[test]
    fn memory_add_confirm_stores_verbatim() {
        let (disp, _dir) = test_dispatcher();
        let frag = "api_key = sk_abc1234567890";
        let env = tool_call(
            1,
            ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: frag.to_string(),
                confirm: true,
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: frag.to_string(),
                confirm: true,
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        let id = structured["id"].as_str().unwrap().to_string();
        let eid = disp.repo().resolve_id(&id).unwrap();
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(mems[0].fragment.contains("sk_abc1234567890"));
    }

    #[test]
    fn memory_add_rejects_duplicates() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Unique Topic Alpha\n\n### Context\nSomething unique about alpha topics and testing.",
        );
        let env = tool_call(2, ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Unique Topic Alpha\n\n### Context\nSomething unique about alpha topics and testing.".to_string(),
            ..Default::default()
        }));
        let result = run(&disp, &env, &ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Unique Topic Alpha\n\n### Context\nSomething unique about alpha topics and testing.".to_string(),
            ..Default::default()
        }));
        assert!(result_is_error(&result));
        assert!(result_text(&result).contains("similar memory already exists"));
    }

    #[test]
    fn memory_add_flags_distill_candidate() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: "## A Lesson Learned\n\n### Context\nA lesson about testing.".to_string(),
                fragment_type: Some("lesson".to_string()),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: "## A Lesson Learned\n\n### Context\nA lesson about testing.".to_string(),
                fragment_type: Some("lesson".to_string()),
                ..Default::default()
            }),
        );
        assert!(result_text(&result).contains("distill candidate"));
    }

    // ---- memory_read ----

    #[test]
    fn memory_read_browse_returns_summary() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Browseable Fragment One\n\n### Context\nFirst memory for browse testing.",
        );
        add_fragment(
            &disp,
            2,
            "## Browseable Fragment Two\n\n### Context\nSecond memory for browse testing.",
        );
        let env = tool_call(3, ToolArgs::MemoryRead(MemoryReadArgs::default()));
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryRead(MemoryReadArgs::default()),
        );
        assert!(!result_is_error(&result));
        let text = result_text(&result);
        assert!(text.contains("## Memory Fragments"));
        assert!(text.contains("Browseable Fragment One"));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["count"].as_u64().unwrap(), 2);
    }

    #[test]
    fn memory_read_by_id_returns_detail() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Detail Fragment\n\n### Context\nA detail memory.",
        );
        let env = tool_call(
            2,
            ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some(id.clone()),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some(id.clone()),
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        let text = result_text(&result);
        assert!(text.contains("=== MEMORY FRAGMENT DETAIL ==="));
        assert!(text.contains(&id));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["count"].as_u64().unwrap(), 1);
        assert_eq!(structured["fragments"][0]["id"], json!(id));
    }

    #[test]
    fn memory_read_unknown_id_is_error() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some("nonexistent".to_string()),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some("nonexistent".to_string()),
                ..Default::default()
            }),
        );
        assert!(result_is_error(&result));
    }

    #[test]
    fn memory_read_explain_includes_recall_explanation() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Explain Fragment\n\n### Context\nFor explain testing.",
        );
        let env = tool_call(
            2,
            ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some(id.clone()),
                explain: true,
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some(id.clone()),
                explain: true,
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        let text = result_text(&result);
        assert!(text.contains("## Why these memories?"));
        assert!(text.contains("Requested by ID; no relevance ranking"));
        let structured = result_structured(&result).unwrap();
        let exp = &structured["recall_explanation"];
        assert_eq!(exp["applies_to"], json!("this_call"));
        assert_eq!(exp["scope"]["mode"], json!("explicit_ids"));
        assert_eq!(exp["items"].as_array().unwrap().len(), 1);
        assert_eq!(exp["items"][0]["selection"]["method"], json!("explicit_id"));
        assert_eq!(
            exp["items"][0]["provenance"]["recorded_source"],
            json!("ai")
        );
        assert_eq!(exp["items"][0]["freshness"]["status"], json!("no_evidence"));
    }

    #[test]
    fn memory_read_explain_query_mode_uses_ranked_method() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Query Explain Alpha\n\n### Context\nAbout alpha query explain testing.",
        );
        let env = tool_call(
            2,
            ToolArgs::MemoryRead(MemoryReadArgs {
                query: Some("alpha query explain".to_string()),
                explain: true,
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryRead(MemoryReadArgs {
                query: Some("alpha query explain".to_string()),
                explain: true,
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        let exp = &structured["recall_explanation"];
        assert_eq!(exp["scope"]["mode"], json!("project_and_global"));
        let items = exp["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert_eq!(items[0]["selection"]["method"], json!("fts5_bm25"));
        assert_eq!(items[0]["selection"]["rank"], json!(1));
    }

    #[test]
    fn memory_read_without_explain_has_no_explanation() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## No Explain Fragment\n\n### Context\nExplain flag off.",
        );
        let env = tool_call(
            2,
            ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some(id.clone()),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some(id.clone()),
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        assert!(!result_text(&result).contains("Why these memories?"));
        let structured = result_structured(&result).unwrap();
        assert!(structured.get("recall_explanation").is_none());
    }

    #[test]
    fn memory_read_records_access_side_effect() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Access Test Fragment\n\n### Context\nTesting access tracking.",
        );
        let eid = disp.repo().resolve_id(&id).unwrap();
        // Lower confidence so the +0.015 boost is observable (default is 1.0).
        let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: id.clone(),
            confidence: Some(0.5),
            ..Default::default()
        });
        let env = tool_call(2, upd.clone());
        run(&disp, &env, &upd);

        // Read it with a context tag.
        let read = ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(id.clone()),
            context: Some("refactoring".to_string()),
            ..Default::default()
        });
        let env = tool_call(3, read.clone());
        let _ = run(&disp, &env, &read);

        // The contract-visible read side effects must be persisted:
        // access_count +1, last_accessed_at, confidence +0.015, context tag.
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert_eq!(mems[0].access_count, 1);
        assert!(mems[0].last_accessed_at.is_some());
        assert!((mems[0].confidence - 0.515).abs() < 1e-9);
        assert!(mems[0].tags.contains(&"refactoring".to_string()));
    }

    // ---- memory_update ----

    #[test]
    fn memory_update_changes_content() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(&disp, 1, "## Original Content\n\n### Context\nOriginal.");
        let env = tool_call(
            2,
            ToolArgs::MemoryUpdate(MemoryUpdateArgs {
                id: id.clone(),
                fragment: Some("## Updated Content\n\n### Context\nUpdated now.".to_string()),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryUpdate(MemoryUpdateArgs {
                id: id.clone(),
                fragment: Some("## Updated Content\n\n### Context\nUpdated now.".to_string()),
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Updated fragment"));
        let eid = disp.repo().resolve_id(&id).unwrap();
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(mems[0].fragment.contains("Updated Content"));
    }

    #[test]
    fn memory_update_unknown_id_is_error() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::MemoryUpdate(MemoryUpdateArgs {
                id: "missing".to_string(),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryUpdate(MemoryUpdateArgs {
                id: "missing".to_string(),
                ..Default::default()
            }),
        );
        assert!(result_is_error(&result));
    }

    // ---- memory_feedback ----

    #[test]
    fn memory_feedback_positive_boosts_confidence() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Feedback Target\n\n### Context\nFor feedback testing.",
        );
        let eid = disp.repo().resolve_id(&id).unwrap();
        // Lower confidence first (default is 1.0, already at the ceiling).
        let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: id.clone(),
            confidence: Some(0.5),
            ..Default::default()
        });
        let env = tool_call(2, upd.clone());
        run(&disp, &env, &upd);
        let before = disp.repo().get_memories(&[eid]).unwrap()[0].confidence;
        let fb = ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
            id: id.clone(),
            useful: true,
        });
        let env = tool_call(3, fb.clone());
        let result = run(&disp, &env, &fb);
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Positive feedback"));
        // Upstream contract: +0.015 confidence + access_count bump.
        let m = &disp.repo().get_memories(&[eid]).unwrap()[0];
        assert!((m.confidence - (before + 0.015)).abs() < 1e-9);
        assert_eq!(m.access_count, 1);
        assert_eq!(m.positive_feedback, 1);
    }

    #[test]
    fn memory_feedback_negative_reduces_confidence() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Feedback Target Neg\n\n### Context\nFor negative feedback.",
        );
        let eid = disp.repo().resolve_id(&id).unwrap();
        let before = disp.repo().get_memories(&[eid]).unwrap()[0].confidence;
        let env = tool_call(
            2,
            ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
                id: id.clone(),
                useful: false,
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
                id: id.clone(),
                useful: false,
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Negative feedback"));
        // Upstream contract: -0.02 confidence + negative_hits increment.
        let m = &disp.repo().get_memories(&[eid]).unwrap()[0];
        assert!((m.confidence - (before - 0.02)).abs() < 1e-9);
        assert_eq!(m.negative_hits, 1);
        assert_eq!(m.negative_feedback, 1);
    }

    // ---- memory_forget ----

    #[test]
    fn memory_forget_hard_delete() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(&disp, 1, "## Forget Me\n\n### Context\nTo be deleted.");
        let eid = disp.repo().resolve_id(&id).unwrap();
        let env = tool_call(
            2,
            ToolArgs::MemoryForget(MemoryForgetArgs {
                id: id.clone(),
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryForget(MemoryForgetArgs {
                id: id.clone(),
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Forgot fragment"));
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(matches!(
            mems[0].lifecycle,
            crate::domain::memory::MemoryLifecycle::Deleted { .. }
        ));
    }

    #[test]
    fn memory_forget_invalidate_hides_from_recall() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Invalidate Me\n\n### Context\nTo be invalidated.",
        );
        let eid = disp.repo().resolve_id(&id).unwrap();
        let env = tool_call(
            2,
            ToolArgs::MemoryForget(MemoryForgetArgs {
                id: id.clone(),
                invalidate: true,
                ..Default::default()
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryForget(MemoryForgetArgs {
                id: id.clone(),
                invalidate: true,
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Invalidated fragment"));
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(!mems[0].lifecycle.is_recallable());
    }

    // ---- memory_relate ----

    #[test]
    fn memory_relate_creates_relation() {
        let (disp, _dir) = test_dispatcher();
        let id1 = add_fragment(
            &disp,
            1,
            "## Relate Source A\n\n### Context\nSource of relation.",
        );
        let id2 = add_fragment(
            &disp,
            2,
            "## Relate Target B\n\n### Context\nTarget of relation.",
        );
        let env = tool_call(
            3,
            ToolArgs::MemoryRelate(MemoryRelateArgs {
                source_id: id1.clone(),
                target_id: id2.clone(),
                relation_type: "supports".to_string(),
                note: None,
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryRelate(MemoryRelateArgs {
                source_id: id1.clone(),
                target_id: id2.clone(),
                relation_type: "supports".to_string(),
                note: None,
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Created relation"));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["relation"], json!("supports"));
    }

    #[test]
    fn memory_relate_rejects_duplicate() {
        let (disp, _dir) = test_dispatcher();
        let id1 = add_fragment(
            &disp,
            1,
            "## Dup Relate A\n\n### Context\nFirst relation source.",
        );
        let id2 = add_fragment(
            &disp,
            2,
            "## Dup Relate B\n\n### Context\nFirst relation target.",
        );
        let tool = ToolArgs::MemoryRelate(MemoryRelateArgs {
            source_id: id1.clone(),
            target_id: id2.clone(),
            relation_type: "supports".to_string(),
            note: None,
        });
        let env1 = tool_call(3, tool.clone());
        let _ = run(&disp, &env1, &tool);
        let env2 = tool_call(4, tool.clone());
        let result = run(&disp, &env2, &tool);
        assert!(result_is_error(&result));
        assert!(result_text(&result).contains("already exists"));
    }

    #[test]
    fn memory_relate_same_id_is_error() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Self Relate\n\n### Context\nSelf relation test.",
        );
        let env = tool_call(
            2,
            ToolArgs::MemoryRelate(MemoryRelateArgs {
                source_id: id.clone(),
                target_id: id.clone(),
                relation_type: "supports".to_string(),
                note: None,
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryRelate(MemoryRelateArgs {
                source_id: id.clone(),
                target_id: id.clone(),
                relation_type: "supports".to_string(),
                note: None,
            }),
        );
        assert!(result_is_error(&result));
        assert!(result_text(&result).contains("cannot be the same"));
    }

    // ---- memory_merge ----

    #[test]
    fn memory_merge_combines_fragments() {
        let (disp, _dir) = test_dispatcher();
        let id1 = add_fragment(
            &disp,
            1,
            "## Merge Source One\n\n### Context\nFirst source of merge.",
        );
        let id2 = add_fragment(
            &disp,
            2,
            "## Merge Source Two\n\n### Context\nSecond source of merge.",
        );
        let env = tool_call(
            3,
            ToolArgs::MemoryMerge(MemoryMergeArgs {
                ids: vec![id1.clone(), id2.clone()],
                title: "Merged Result".to_string(),
                fragment: "## Merged Result\n\n### Context\nCombined content.".to_string(),
                project: None,
                consolidate: false,
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryMerge(MemoryMergeArgs {
                ids: vec![id1.clone(), id2.clone()],
                title: "Merged Result".to_string(),
                fragment: "## Merged Result\n\n### Context\nCombined content.".to_string(),
                project: None,
                consolidate: false,
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Merged 2 fragments"));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["success"], json!(true));
        assert_eq!(structured["merged_ids"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn memory_merge_requires_two_ids() {
        let (disp, _dir) = test_dispatcher();
        let id1 = add_fragment(
            &disp,
            1,
            "## Single Source\n\n### Context\nOnly one source.",
        );
        let env = tool_call(
            2,
            ToolArgs::MemoryMerge(MemoryMergeArgs {
                ids: vec![id1.clone()],
                title: "Bad Merge".to_string(),
                fragment: "## Bad\n\n### Context\nToo few.".to_string(),
                project: None,
                consolidate: false,
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryMerge(MemoryMergeArgs {
                ids: vec![id1.clone()],
                title: "Bad Merge".to_string(),
                fragment: "## Bad\n\n### Context\nToo few.".to_string(),
                project: None,
                consolidate: false,
            }),
        );
        assert!(result_is_error(&result));
    }

    // ---- memory_stats ----

    #[test]
    fn memory_stats_reports_counts() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Stats Fragment A\n\n### Context\nFirst stats memory.",
        );
        add_fragment(
            &disp,
            2,
            "## Stats Fragment B\n\n### Context\nSecond stats memory.",
        );
        let env = tool_call(3, ToolArgs::MemoryStats(MemoryStatsArgs::default()));
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryStats(MemoryStatsArgs::default()),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["total"].as_u64().unwrap(), 2);
        assert!(result_text(&result).contains("## Memory Stats"));
    }

    // ---- memory_audit ----

    #[test]
    fn memory_audit_reports_healthy() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Audit Fragment\n\n### Context\nFor audit testing.",
        );
        let env = tool_call(2, ToolArgs::MemoryAudit(MemoryAuditArgs::default()));
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryAudit(MemoryAuditArgs::default()),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["total_fragments"].as_u64().unwrap(), 1);
        assert!(result_text(&result).contains("## Memory Audit"));
    }

    // ---- memory_library ----

    #[test]
    fn memory_library_returns_snapshot() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Library Fragment\n\n### Context\nFor library testing.",
        );
        let env = tool_call(2, ToolArgs::MemoryLibrary(MemoryLibraryArgs::default()));
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryLibrary(MemoryLibraryArgs::default()),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["fragments_total"].as_u64().unwrap(), 1);
        assert!(result_text(&result).contains("LIBRARY MODE SNAPSHOT"));
    }

    // ---- semantic_search ----

    #[test]
    fn semantic_search_finds_relevant() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Rust Testing Patterns\n\n### Context\nHow to write tests in Rust with cargo test.",
        );
        add_fragment(
            &disp,
            2,
            "## Cooking Recipes\n\n### Context\nHow to bake bread at home.",
        );
        let ss_args = |q: &str| SemanticSearchArgs {
            query: q.to_string(),
            project: None,
            top_k: None,
            offset: None,
            hybrid: false,
            explain: false,
            response_format: None,
        };
        let env = tool_call(3, ToolArgs::SemanticSearch(ss_args("rust testing cargo")));
        let result = run(
            &disp,
            &env,
            &ToolArgs::SemanticSearch(ss_args("rust testing cargo")),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert!(structured["count"].as_u64().unwrap() >= 1);
        assert!(result_text(&result).contains("SEMANTIC SEARCH RESULTS"));
    }

    #[test]
    fn semantic_search_empty_result() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Unrelated Content\n\n### Context\nNothing matching here.",
        );
        let ss_args = |q: &str| SemanticSearchArgs {
            query: q.to_string(),
            project: None,
            top_k: None,
            offset: None,
            hybrid: false,
            explain: false,
            response_format: None,
        };
        let env = tool_call(
            2,
            ToolArgs::SemanticSearch(ss_args("quantum chromodynamics lattice")),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::SemanticSearch(ss_args("quantum chromodynamics lattice")),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["count"].as_u64().unwrap(), 0);
        assert!(result_text(&result).contains("No semantically similar memories found"));
    }

    // ---- Differential wire-contract tests (T-MCP-02 legacy_oracle) ----
    //
    // These replay an anonymized fixture derived from the real upstream
    // Lemma 0.21.0 database (structure preserved: legacy IDs, relations,
    // confidence, dates, counts, projects, tags; private text replaced).
    // The upstream rendering of that data is the oracle reference, captured
    // by running the pinned upstream code. ltmrs must reproduce it
    // byte-for-byte.

    fn fixture_path(name: &str) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/compat/lemma_0_21_0");
        p.push(name);
        p
    }

    /// Parse a "YYYY-MM-DD" date into UTC epoch millis.
    fn parse_date_only(s: &str) -> u64 {
        let parts: Vec<&str> = s.split('-').collect();
        let y: i64 = parts[0].parse().unwrap();
        let m: i64 = parts[1].parse().unwrap();
        let d: i64 = parts[2].parse().unwrap();
        // Days from civil date (Howard Hinnant's algorithm).
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + (mp * 306 + 5) / 10 + (d - 1);
        let doe = doe.max(0);
        (era * 146_097 + doe - 719_468) as u64 * 86_400_000
    }

    fn fixture_memory(rec: &Value, id_map: &std::collections::HashMap<String, EntityId>) -> Memory {
        let f = &rec["fields"];
        let eid = id_map[&f["id"].as_str().unwrap().to_string()];
        let created_millis = parse_date_only(f["created"].as_str().unwrap());
        Memory {
            id: eid,
            external_alias: Some(crate::domain::id::ExternalAlias::new(
                f["id"].as_str().unwrap().to_string(),
            )),
            title: f["title"].as_str().unwrap_or("").to_string(),
            fragment: f["fragment"].as_str().unwrap_or("").to_string(),
            description: f["description"].as_str().unwrap_or("").to_string(),
            fragment_type: FragmentType::Fact,
            project: f["project"].as_str().map(|s| s.to_string()),
            // Upstream icon: "ai" → 🤖, anything else → 👤. Map accordingly.
            source: if f["source"].as_str() == Some("ai") {
                MemorySource::Ai
            } else {
                MemorySource::User
            },
            confidence: f["confidence"].as_f64().unwrap_or(0.5),
            quality_score: None,
            lifecycle: crate::domain::memory::MemoryLifecycle::Live,
            tags: f["tags"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|t| t.as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default(),
            associated_with: f["associatedWith"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|t| t.as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default(),
            relations: f["relations"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| {
                            let target = id_map[&r["id"].as_str().unwrap().to_string()];
                            crate::domain::relation::Relation::new(
                                EntityId::new(Uuid::new_v5(
                                    &Uuid::NAMESPACE_URL,
                                    format!("rel:{}", r["id"].as_str().unwrap()).as_bytes(),
                                )),
                                eid,
                                target,
                                crate::domain::relation::RelationType::parse(
                                    r["type"].as_str().unwrap_or("related_to"),
                                )
                                .unwrap_or(crate::domain::relation::RelationType::RelatedTo),
                                r["note"].as_str().map(|s| s.to_string()),
                                Instant::new(created_millis),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            parent_id: f["parent_id"].as_str().and_then(|p| id_map.get(p).copied()),
            child_ids: f["child_ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| id_map.get(c.as_str().unwrap_or("")).copied())
                        .collect()
                })
                .unwrap_or_default(),
            session_id: None,
            task_type: None,
            related_guides: Vec::new(),
            evidence: Vec::new(),
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: f["positive_feedback"].as_u64().unwrap_or(0),
            negative_feedback: f["negative_feedback"].as_u64().unwrap_or(0),
            negative_hits: 0,
            refinement_count: f["refinement_count"].as_u64().unwrap_or(0),
            distill_candidate: false,
            entity_revision: crate::domain::id::EntityRevision::new(1),
            document_revision: crate::domain::id::DocumentRevision::new(1),
            eligibility_revision: crate::domain::id::EligibilityRevision::new(1),
            created_at: Instant::new(created_millis),
            updated_at: Instant::new(created_millis),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn differential_detail_matches_upstream_wire() {
        let raw = std::fs::read_to_string(fixture_path("rendering_fixture.json"))
            .expect("rendering_fixture.json must exist");
        let fixture: Value = serde_json::from_str(&raw).expect("valid JSON fixture");
        let recs = fixture.as_array().expect("fixture is an array");

        // Map each legacy ID to a deterministic EntityId.
        let mut id_map: std::collections::HashMap<String, EntityId> =
            std::collections::HashMap::new();
        for rec in recs {
            let id = rec["fields"]["id"].as_str().unwrap().to_string();
            id_map.entry(id.clone()).or_insert_with(|| {
                EntityId::new(Uuid::new_v5(
                    &Uuid::NAMESPACE_URL,
                    format!("ltmrs:entity:{}", id).as_bytes(),
                ))
            });
        }

        let mut checked = 0;
        for rec in recs {
            let m = fixture_memory(rec, &id_map);
            let legacy_id = m.external_alias.as_ref().unwrap().as_str().to_string();
            let resolver = |eid: &EntityId| {
                id_map
                    .iter()
                    .find(|(_, e)| **e == *eid)
                    .map(|(s, _)| s.clone())
                    .unwrap_or_else(|| eid.as_uuid().to_string())
            };
            let actual = render_detail(&legacy_id, &m, &resolver);
            let expected = rec["detail"].as_str().unwrap();
            assert_eq!(actual, expected, "detail mismatch for {}", legacy_id);
            checked += 1;
        }
        assert!(
            checked >= 100,
            "fixture should cover many fragments, got {checked}"
        );
    }

    #[test]
    fn differential_detail_matches_traffic_log_wire() {
        // Uses actual upstream wire responses from traffic logs as the oracle
        let raw = std::fs::read_to_string(fixture_path("traffic_fixture.json"))
            .expect("traffic_fixture.json must exist");
        let fixture: Value = serde_json::from_str(&raw).expect("valid JSON fixture");
        let recs = fixture.as_array().expect("fixture is an array");

        // Build id_map for all fragments AND all referenced IDs (relation targets, etc.)
        let mut id_map: std::collections::HashMap<String, EntityId> =
            std::collections::HashMap::new();
        let add_id = |id_map: &mut std::collections::HashMap<String, EntityId>, id: &str| {
            id_map.entry(id.to_string()).or_insert_with(|| {
                EntityId::new(Uuid::new_v5(
                    &Uuid::NAMESPACE_URL,
                    format!("ltmrs:entity:{}", id).as_bytes(),
                ))
            });
        };

        for rec in recs {
            let f = &rec["fields"];
            add_id(&mut id_map, f["id"].as_str().unwrap());
            // Add relation targets
            if let Some(rels) = f["relations"].as_array() {
                for r in rels {
                    add_id(&mut id_map, r["id"].as_str().unwrap());
                }
            }
            // Add parent_id
            if let Some(parent) = f["parent_id"].as_str() {
                add_id(&mut id_map, parent);
            }
            // Add child_ids
            if let Some(children) = f["child_ids"].as_array() {
                for c in children {
                    add_id(&mut id_map, c.as_str().unwrap());
                }
            }
        }

        for rec in recs {
            let m = fixture_memory(rec, &id_map);
            let legacy_id = m.external_alias.as_ref().unwrap().as_str().to_string();
            let resolver = |eid: &EntityId| {
                id_map
                    .iter()
                    .find(|(_, e)| **e == *eid)
                    .map(|(s, _)| s.clone())
                    .unwrap_or_else(|| eid.as_uuid().to_string())
            };
            let actual = render_detail(&legacy_id, &m, &resolver);
            let expected = rec["detail"].as_str().unwrap();
            assert_eq!(
                actual, expected,
                "traffic log detail mismatch for {}",
                legacy_id
            );
        }
    }

    #[test]
    fn differential_summary_matches_upstream_wire() {
        let raw = std::fs::read_to_string(fixture_path("rendering_fixture.json"))
            .expect("rendering_fixture.json must exist");
        let fixture: Value = serde_json::from_str(&raw).expect("valid JSON fixture");
        let recs = fixture.as_array().expect("fixture is an array");

        let mut id_map: std::collections::HashMap<String, EntityId> =
            std::collections::HashMap::new();
        for rec in recs {
            let id = rec["fields"]["id"].as_str().unwrap().to_string();
            id_map.entry(id.clone()).or_insert_with(|| {
                EntityId::new(Uuid::new_v5(
                    &Uuid::NAMESPACE_URL,
                    format!("ltmrs:entity:{}", id).as_bytes(),
                ))
            });
        }

        for rec in recs {
            let m = fixture_memory(rec, &id_map);
            let project = m.project.as_deref();
            let scope_info = match project {
                Some(p) => p.to_string(),
                None => "global".to_string(),
            };
            let lid = |m: &Memory| m.external_alias.as_ref().unwrap().as_str().to_string();
            let actual = render_summary_index(std::slice::from_ref(&m), &scope_info, &lid);
            let expected = rec["summary"].as_str().unwrap();
            assert_eq!(
                actual,
                expected,
                "summary mismatch for {}",
                m.external_alias.as_ref().unwrap().as_str()
            );
        }
    }

    // ---- Pure-function differential tests (derived from upstream source) ----
    //
    // The oracle is generated by running the pinned upstream code on a set of
    // inputs (tests/compat/lemma_0_21_0/pure_functions.json). ltmrs must
    // reproduce the upstream outputs byte-for-byte.

    fn pure_oracle_path() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/compat/lemma_0_21_0/pure_functions.json");
        p
    }

    /// The frozen clock reference baked into the oracle (2026-09-23T12:00:00Z).
    const ORACLE_NOW_MILLIS: u64 = 1_790_164_800_000;

    #[test]
    fn calculate_quality_score_matches_upstream() {
        use crate::compatibility::lemma::reference::{QualityCounters, calculate_quality_score};
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["calculateQualityScore"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let input = &case["input"];
            let counters = QualityCounters {
                confidence: input["confidence"].as_f64().unwrap(),
                positive_feedback: input["positive_feedback"].as_u64().unwrap(),
                negative_feedback: input["negative_feedback"].as_u64().unwrap(),
                accessed: input["accessed"].as_u64().unwrap(),
                refinement_count: input["refinement_count"].as_u64().unwrap(),
                last_accessed_millis: input["lastAccessedMillis"].as_u64(),
                negative_hits: input["negativeHits"].as_u64().unwrap(),
            };
            let actual = calculate_quality_score(&counters, ORACLE_NOW_MILLIS);
            let expected = case["output"].as_f64().unwrap();
            assert!(
                (actual - expected).abs() < 1e-9,
                "calculate_quality_score mismatch on case {i}: {actual} vs {expected}"
            );
        }
    }

    #[test]
    fn injection_score_matches_upstream() {
        use crate::compatibility::lemma::reference::injection_score;
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["injectionScore"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let input = &case["input"];
            let actual = injection_score(
                input["confidence"].as_f64().unwrap(),
                input["createdMillis"].as_u64().unwrap(),
                ORACLE_NOW_MILLIS,
            );
            let expected = case["output"].as_f64().unwrap();
            assert!(
                (actual - expected).abs() < 1e-9,
                "injection_score mismatch on case {i}: {actual} vs {expected}"
            );
        }
    }

    #[test]
    fn generate_description_matches_upstream() {
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["generateDescription"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let input = case["input"].as_str().unwrap();
            let expected = case["output"].as_str().unwrap();
            let actual = generate_description(input);
            assert_eq!(
                actual, expected,
                "generate_description mismatch on case {i}: input={input:?}"
            );
        }
    }

    #[test]
    fn normalize_project_matches_upstream() {
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["resolveProjectScope"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let Some(input) = case["input"].as_str() else {
                continue;
            };
            let expected: Option<String> = case["output"].as_str().map(|s| s.to_string());
            let actual = normalize_project(input);
            assert_eq!(
                actual, expected,
                "normalize_project mismatch on case {i}: input={input:?}"
            );
        }
    }

    fn make_memory(
        id: &str,
        project: Option<&str>,
        source: &str,
        confidence: f64,
        fragment: &str,
    ) -> Memory {
        use crate::domain::id::{
            DocumentRevision, EligibilityRevision, EntityRevision, ExternalAlias,
        };
        Memory {
            id: EntityId::new(Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                format!("ltmrs:entity:{}", id).as_bytes(),
            )),
            external_alias: Some(ExternalAlias::new(id.to_string())),
            title: String::new(),
            fragment: fragment.to_string(),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: project.map(|s| s.to_string()),
            source: MemorySource::parse(source).expect("valid source in oracle"),
            confidence,
            quality_score: None,
            lifecycle: crate::domain::memory::MemoryLifecycle::Live,
            tags: Vec::new(),
            associated_with: Vec::new(),
            relations: Vec::new(),
            parent_id: None,
            child_ids: Vec::new(),
            session_id: None,
            task_type: None,
            related_guides: Vec::new(),
            evidence: Vec::new(),
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: EntityRevision::new(0),
            document_revision: DocumentRevision::new(0),
            eligibility_revision: EligibilityRevision::new(0),
            created_at: Instant::new(0),
            updated_at: Instant::new(0),
            raw_created: None,
            unknown_fields: Default::default(),
        }
    }

    #[test]
    fn calculate_stats_matches_upstream() {
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["calculateStats"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let memories: Vec<Memory> = case["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    make_memory(
                        f["id"].as_str().unwrap(),
                        f["project"].as_str(),
                        f["source"].as_str().unwrap_or("ai"),
                        f["confidence"].as_f64().unwrap(),
                        "",
                    )
                })
                .collect();

            let project_filter = case["project"].as_str();
            let actual = calculate_stats(&memories, project_filter);
            let expected = &case["output"];

            assert_eq!(
                actual["total"], expected["total"],
                "stats.total mismatch on case {i}"
            );
            assert!(
                (actual["avg_confidence"].as_f64().unwrap()
                    - expected["avg_confidence"].as_f64().unwrap())
                .abs()
                    < 0.001,
                "stats.avg_confidence mismatch on case {i}: {} vs {}",
                actual["avg_confidence"],
                expected["avg_confidence"]
            );
            assert_eq!(
                actual["low_confidence"], expected["low_confidence"],
                "stats.low_confidence mismatch on case {i}"
            );
            assert_eq!(
                actual["high_confidence"], expected["high_confidence"],
                "stats.high_confidence mismatch on case {i}"
            );
            assert_eq!(
                actual["by_source"], expected["by_source"],
                "stats.by_source mismatch on case {i}"
            );
            assert_eq!(
                actual["by_project"], expected["by_project"],
                "stats.by_project mismatch on case {i}"
            );
        }
    }

    #[test]
    fn format_stats_matches_upstream() {
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["formatStats"].as_array().unwrap().iter().enumerate() {
            let stats = &case["input"];
            let actual = format_stats(stats);
            let expected = case["output"].as_str().unwrap();
            assert_eq!(actual, expected, "format_stats mismatch on case {i}");
        }
    }

    #[test]
    fn audit_memory_matches_upstream() {
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["auditMemory"].as_array().unwrap().iter().enumerate() {
            let memories: Vec<Memory> = case["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    let mut m = make_memory(
                        f["id"].as_str().unwrap(),
                        None,
                        "ai",
                        f["confidence"].as_f64().unwrap(),
                        f.get("fragment").and_then(|v| v.as_str()).unwrap_or(""),
                    );
                    m.associated_with = f
                        .get("associatedWith")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    m
                })
                .collect();

            // Convert per-fragment relations (upstream model) to ltmrs's
            // global relation list (separate keyspace), tracking the legacy
            // ID behind each derived entity UUID for issue-text parity.
            let entity_id = |id: &str| {
                EntityId::new(Uuid::new_v5(
                    &Uuid::NAMESPACE_URL,
                    format!("ltmrs:entity:{}", id).as_bytes(),
                ))
            };
            let mut legacy_by_uuid: std::collections::HashMap<uuid::Uuid, String> =
                std::collections::HashMap::new();
            let relations: Vec<Relation> = case["input"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|f| {
                    let src = f["id"].as_str().unwrap().to_string();
                    f.get("relations")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .map(|r| {
                                    let tgt = r["id"].as_str().unwrap().to_string();
                                    let (su, tu) = (entity_id(&src), entity_id(&tgt));
                                    legacy_by_uuid.insert(su.as_uuid(), src.clone());
                                    legacy_by_uuid.insert(tu.as_uuid(), tgt.clone());
                                    Relation::new(
                                        EntityId::new(Uuid::new_v5(
                                            &Uuid::NAMESPACE_URL,
                                            format!("ltmrs:rel:{}-{}", src, tgt).as_bytes(),
                                        )),
                                        su,
                                        tu,
                                        crate::domain::relation::RelationType::parse(
                                            r["type"].as_str().unwrap_or("related_to"),
                                        )
                                        .unwrap_or(
                                            crate::domain::relation::RelationType::RelatedTo,
                                        ),
                                        None,
                                        Instant::new(0),
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect();

            // associated_with references resolve against the current memory set.
            let memory_ids: std::collections::HashSet<String> = memories
                .iter()
                .filter_map(|m| m.external_alias.as_ref().map(|a| a.as_str().to_string()))
                .collect();

            let actual = audit_memory(
                &memories,
                &relations,
                |m| m.external_alias.as_ref().unwrap().as_str().to_string(),
                |assoc| memory_ids.contains(assoc),
                |id| {
                    legacy_by_uuid
                        .get(&id.as_uuid())
                        .cloned()
                        .unwrap_or_else(|| id.as_uuid().to_string())
                },
            );
            let expected = &case["output"];

            assert_eq!(
                actual["total_fragments"], expected["total_fragments"],
                "audit.total_fragments mismatch on case {i}"
            );
            assert_eq!(
                actual["issues_found"], expected["issues_found"],
                "audit.issues_found mismatch on case {i}"
            );
            assert_eq!(
                actual["healthy"], expected["healthy"],
                "audit.healthy mismatch on case {i}"
            );
            let actual_issues: Vec<String> = actual["issues"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect();
            let expected_issues: Vec<String> = expected["issues"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect();
            assert_eq!(
                actual_issues, expected_issues,
                "audit.issues mismatch on case {i}"
            );
        }
    }

    #[test]
    fn format_audit_report_matches_upstream() {
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["formatAuditReport"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let result = &case["input"];
            let actual = format_audit_report(result);
            let expected = case["output"].as_str().unwrap();
            assert_eq!(actual, expected, "format_audit_report mismatch on case {i}");
        }
    }

    #[test]
    fn filter_by_project_matches_upstream() {
        let raw =
            std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
        let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
        for (i, case) in oracle["filterByProject"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let memories: Vec<Memory> = case["input"]["fragments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    make_memory(
                        f["id"].as_str().unwrap(),
                        f["project"].as_str(),
                        "ai",
                        0.5,
                        "",
                    )
                })
                .collect();

            let current_project = case["input"]["currentProject"].as_str();
            let actual: Vec<String> = filter_by_project(&memories, current_project)
                .iter()
                .map(|m| m.external_alias.as_ref().unwrap().as_str().to_string())
                .collect();
            let expected: Vec<String> = case["output"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect();
            assert_eq!(actual, expected, "filter_by_project mismatch on case {i}");
        }
    }
}

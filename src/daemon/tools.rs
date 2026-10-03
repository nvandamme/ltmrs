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
    BackupCreateArgs, BackupPreviewArgs, BackupRestoreArgs, ConflictScanArgs, GuideCreateArgs,
    GuideDistillArgs, GuideForgetArgs, GuideGetArgs, GuideMergeArgs, GuidePracticeArgs,
    GuideUpdateArgs, MemoryAddArgs, MemoryAuditArgs, MemoryFeedbackArgs, MemoryForgetArgs,
    MemoryLibraryArgs, MemoryMergeArgs, MemoryReadArgs, MemoryRelateArgs, MemoryStatsArgs,
    MemoryUpdateArgs, ProactiveAnalysisArgs, ProjectAnalyticsArgs, ResponseFormat,
    SemanticSearchArgs, SessionAttemptArgs, SessionEndArgs, SessionStartArgs, SessionStatsArgs,
    SuggestionRespondArgs, ToolArgs,
};
use crate::daemon::dispatcher::Dispatcher;
use crate::daemon::envelope::{DomainPayload, IpcEnvelope};
use crate::domain::command::{
    CommandContext, DomainCommand, DomainError, DomainErrorCode, DomainResult, ForgetMode,
    MemoryPatch,
};
use crate::domain::guide::Guide;
use crate::domain::id::{EntityId, EntityRevision, OperationId, SessionHandle};
use crate::domain::memory::{Evidence, FragmentType, Instant, Memory, MemorySource};
use crate::domain::relation::{Relation, RelationType};
use crate::domain::session::{
    Attempt, AttemptOutcome, Session, Suggestion, SuggestionStatus, TaskOutcome,
};
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
        ToolArgs::GuideGet(args) => exec_guide_get(disp, args),
        ToolArgs::GuidePractice(args) => exec_guide_practice(disp, envelope, args),
        ToolArgs::GuideCreate(args) => exec_guide_create(disp, args),
        ToolArgs::GuideDistill(args) => exec_guide_distill(disp, args),
        ToolArgs::GuideUpdate(args) => exec_guide_update(disp, args),
        ToolArgs::GuideForget(args) => exec_guide_forget(disp, args),
        ToolArgs::GuideMerge(args) => exec_guide_merge(disp, args),
        ToolArgs::SessionStart(args) => exec_session_start(disp, envelope, args),
        ToolArgs::SessionAttempt(args) => exec_session_attempt(disp, envelope, args),
        ToolArgs::SessionEnd(args) => exec_session_end(disp, envelope, args),
        ToolArgs::SessionStats(args) => exec_session_stats(disp, envelope, args),
        ToolArgs::SuggestionRespond(args) => exec_suggestion_respond(disp, args),
        ToolArgs::ConflictScan(args) => exec_conflict_scan(disp, args),
        ToolArgs::ProactiveAnalysis(args) => exec_proactive_analysis(disp, args),
        ToolArgs::ProjectAnalytics(args) => exec_project_analytics(disp, args),
        ToolArgs::BackupCreate(args) => exec_backup_create(disp, args),
        ToolArgs::BackupPreview(args) => exec_backup_preview(disp, envelope, args),
        ToolArgs::BackupRestore(args) => exec_backup_restore(disp, envelope, args),
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
    crate::compatibility::lemma::tool_args::normalize_project(raw)
}

/// Auto-title: first 40 chars (truncated with "...") or the fragment itself.
/// Char-boundary truncation: byte slicing would panic on multi-byte titles.
/// Compat nuance: upstream JS substring counts UTF-16 units, so astral-plane
/// chars (emoji/CJK-ext) count 1 here vs 2 there — strictly better than
/// panicking, and titles are display hints, not protocol.
fn generate_title(fragment: &str) -> String {
    if fragment.chars().count() > 40 {
        format!("{}...", fragment.chars().take(40).collect::<String>())
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
/// else a canonical snapshot scan (lexical fallback). An attached but empty
/// backend (e.g. a fresh E5 start before projection runs) also falls back —
/// an empty dense/lexical index must never hide canonical knowledge.
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
            // An attached backend means dense retrieval is available: run
            // the dense leg in the pinned E5 space (empty tables yield no
            // dense hits and fall back gracefully below).
            model_fingerprint: Some(crate::embeddings::e5_small::E5_SMALL_FINGERPRINT),
            result_limit: 100,
            ..Default::default()
        };
        if let Ok(result) = sb.retrieve_sync(&req)
            && !result.results.is_empty()
        {
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

    // Resolve the session link BEFORE building the record so attribution
    // lands in the single AddMemory apply: the channel's traced session
    // when present, else its virtual session — session-less calls are
    // attributed per channel, never silently dropped (DEV-003: no
    // daemon-global session). A two-step link would blind-overwrite a
    // concurrent change with no revision check and no receipt.
    let (session_handle, session_task_type) = {
        let mut reg = disp.registry();
        let handle = reg.ensure_virtual_session(
            envelope.frontend_id,
            envelope.channel_id,
            disp.clock().now_millis(),
        );
        let task_type = reg
            .session(handle)
            .and_then(|s| s.task_type.clone())
            .unwrap_or_default();
        (handle, task_type)
    };

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
        session_id: Some(session_handle.as_uuid().to_string()),
        task_type: Some(session_task_type),
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

    // Attribute the created memory to the session in the registry. The
    // canonical record already carries the link (set before AddMemory);
    // this is registry-side bookkeeping only.
    {
        let mut reg = disp.registry();
        if let Some(s) = reg.session_mut(session_handle)
            && !s.memories_created.contains(&legacy_id)
        {
            s.memories_created.push(legacy_id.clone());
        }
    }

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
    let mut engine_explanation: Option<crate::retrieval::explain::RetrievalExplanation> = None;
    if let Some(sb) = disp.search() {
        // hybrid:false forces lexical-only (upstream parity on demand);
        // absent/true runs the hybrid engine when a backend is attached.
        let use_dense = args.hybrid != Some(false);
        let req = crate::retrieval::engine::RetrievalRequest {
            query: args.query.clone(),
            scope: crate::domain::command::Scope {
                project: args.project.clone(),
                all_projects: false,
                ..Default::default()
            },
            // Dense leg in the pinned E5 space when a backend is attached;
            // no backend (or no dense hits) falls back to lexical below.
            model_fingerprint: if use_dense {
                Some(crate::embeddings::e5_small::E5_SMALL_FINGERPRINT)
            } else {
                None
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
            engine_explanation = Some(result.explanation);
        }
    }

    /// Explain how the answer was produced when requested: the effective
    /// mode (hybrid only when the dense leg ran) plus engine readiness,
    /// or the fallback mode otherwise. The fallback candidate count is
    /// threaded in (the engine arm reports its own examined pool).
    fn explain_search(
        explanation: &Option<crate::retrieval::explain::RetrievalExplanation>,
        fallback_candidates: usize,
    ) -> serde_json::Value {
        match explanation {
            Some(exp) => serde_json::json!({
                "mode": if exp.model_fingerprint.is_some() { "hybrid" } else { "lexical" },
                "dense_ready": exp.dense_ready,
                "fts_ready": exp.fts_ready,
                "partial": exp.partial,
                "no_match": exp.no_match,
                "candidates": exp.candidates.len(),
                "conflict_notice": exp.conflict_notice,
            }),
            None => serde_json::json!({
                "mode": "lexical-fallback",
                "dense_ready": false,
                // Substring scan over a canonical snapshot, not the FTS
                // index: never claim FTS readiness here.
                "fts_ready": false,
                "partial": false,
                "no_match": false,
                "candidates": fallback_candidates,
                "conflict_notice": null,
            }),
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
        // The served rows come from the substring fallback, not the engine:
        // drop the engine explanation so explain:true reports the effective
        // fallback mode instead of a hybrid that produced nothing. (When the
        // fallback also finds nothing, the engine no-match is preserved.)
        if !scored.is_empty() {
            engine_explanation = None;
        }
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
        let mut data = json!({
            "count": 0,
            "total": total,
            "results": [],
            "has_more": has_more,
            "next_offset": if has_more { Some(next_offset) } else { None },
        });
        if args.explain {
            data["explanation"] = explain_search(&engine_explanation, scored.len());
        }
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

    let mut data = json!({
        "count": page.len(),
        "total": total,
        "results": results_json,
        "has_more": has_more,
        "next_offset": if has_more { Some(next_offset) } else { None },
    });
    if args.explain {
        data["explanation"] = explain_search(&engine_explanation, scored.len());
    }
    Ok(format_result(text, data, format))
}

// =====================================================================
// WP-09: Guides, sessions, intelligence
// =====================================================================

/// The upstream task→guide keyword map (guides/task-map.ts).
fn task_guide_defs() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    vec![
        (
            "html",
            "web-frontend",
            &["web", "sayfa", "ui", "arayüz", "html"],
        ),
        (
            "css",
            "web-frontend",
            &["stil", "style", "tasarım", "design", "css"],
        ),
        (
            "javascript",
            "programming-language",
            &["js", "web", "frontend"],
        ),
        (
            "react",
            "web-frontend",
            &["component", "jsx", "hook", "state", "react"],
        ),
        ("vue", "web-frontend", &["vue", "component", "template"]),
        (
            "angular",
            "web-frontend",
            &["angular", "component", "service"],
        ),
        ("tailwind", "web-frontend", &["tailwind", "css", "utility"]),
        (
            "nextjs",
            "web-frontend",
            &["next", "nextjs", "ssr", "app router"],
        ),
        (
            "typescript",
            "programming-language",
            &["ts", "tip", "type", "interface"],
        ),
        (
            "nodejs",
            "web-backend",
            &["node", "server", "api", "express"],
        ),
        (
            "express",
            "web-backend",
            &["express", "router", "middleware"],
        ),
        (
            "nestjs",
            "web-backend",
            &["nestjs", "module", "controller", "service"],
        ),
        (
            "python",
            "programming-language",
            &["py", "django", "flask", "fastapi"],
        ),
        ("fastapi", "web-backend", &["fastapi", "async", "python"]),
        ("django", "web-backend", &["django", "orm", "python"]),
        ("rest", "web-backend", &["api", "rest", "endpoint", "http"]),
        (
            "graphql",
            "web-backend",
            &["graphql", "query", "mutation", "schema"],
        ),
        ("trpc", "web-backend", &["trpc", "typescript", "rpc"]),
        (
            "postgresql",
            "data-storage",
            &["postgres", "sql", "relational", "pg"],
        ),
        ("mongodb", "data-storage", &["mongo", "nosql", "document"]),
        ("redis", "data-storage", &["redis", "cache", "key-value"]),
        ("prisma", "data-storage", &["prisma", "orm", "schema"]),
        ("sqlite", "data-storage", &["sqlite", "local", "embedded"]),
        (
            "supabase",
            "data-storage",
            &["supabase", "postgres", "auth", "storage"],
        ),
        (
            "pinecone",
            "data-storage",
            &["pinecone", "vector", "embedding"],
        ),
        (
            "elasticsearch",
            "data-storage",
            &["elastic", "search", "index"],
        ),
        ("git", "dev-tool", &["git", "commit", "branch", "merge"]),
        ("docker", "infra-devops", &["docker", "container", "image"]),
        ("webpack", "dev-tool", &["webpack", "bundle", "build"]),
        ("vite", "dev-tool", &["vite", "build", "dev", "hmr"]),
        ("jest", "dev-tool", &["jest", "test", "unit", "spec"]),
        ("vitest", "dev-tool", &["vitest", "test", "vite"]),
        (
            "playwright",
            "dev-tool",
            &["playwright", "e2e", "browser", "test"],
        ),
        ("eslint", "dev-tool", &["eslint", "lint", "format"]),
        (
            "react-native",
            "mobile-frontend",
            &["react native", "mobile", "expo", "rn"],
        ),
        (
            "flutter",
            "mobile-frontend",
            &["flutter", "dart", "mobile", "widget"],
        ),
        (
            "expo",
            "mobile-frontend",
            &["expo", "react native", "mobile"],
        ),
        (
            "swift",
            "mobile-frontend",
            &["swift", "ios", "iphone", "swiftui"],
        ),
        (
            "kotlin",
            "mobile-frontend",
            &["kotlin", "android", "jetpack"],
        ),
        (
            "threejs",
            "game-frontend",
            &["threejs", "three.js", "webgl", "3d"],
        ),
        (
            "canvas",
            "game-frontend",
            &["canvas", "html5", "2d", "drawing"],
        ),
        (
            "phaser",
            "game-frontend",
            &["phaser", "game", "html5", "2d"],
        ),
        ("webgl", "game-frontend", &["webgl", "shader", "gpu", "3d"]),
        (
            "godot",
            "game-backend",
            &["godot", "gdscript", "game engine"],
        ),
        (
            "game-loop",
            "game-backend",
            &["game loop", "update", "render", "fixed timestep"],
        ),
        (
            "state-machine",
            "game-backend",
            &["state", "fsm", "transition"],
        ),
        (
            "ecs",
            "game-backend",
            &["ecs", "entity", "component", "system"],
        ),
        (
            "object-pooling",
            "game-backend",
            &["pool", "reuse", "spawn", "bullet"],
        ),
        (
            "ai-art-generation",
            "game-tool",
            &["ai art", "stable diffusion", "flux", "dalle"],
        ),
        (
            "pixel-art",
            "game-design",
            &["pixel", "sprite", "8bit", "16bit", "retro"],
        ),
        (
            "aseprite",
            "game-tool",
            &["aseprite", "sprite", "animation"],
        ),
        (
            "spritesheet",
            "game-tool",
            &["spritesheet", "atlas", "texture", "export"],
        ),
        (
            "background-removal",
            "game-tool",
            &["bg remove", "transparent", "cutout"],
        ),
        (
            "image-upscaling",
            "game-tool",
            &["upscale", "esrgan", "hd", "4k"],
        ),
        (
            "level-design",
            "game-design",
            &["level", "map", "blockout", "flow"],
        ),
        (
            "character-design",
            "game-design",
            &["character", "silhouette", "shape language"],
        ),
        (
            "texture-art",
            "game-design",
            &["texture", "pbr", "normal map", "material"],
        ),
        (
            "animation",
            "game-design",
            &["animation", "walk cycle", "frame", "sprite"],
        ),
        (
            "tileset",
            "game-design",
            &["tileset", "tile", "autotile", "seamless"],
        ),
        (
            "oauth",
            "app-security",
            &["oauth", "auth", "login", "token"],
        ),
        ("jwt", "app-security", &["jwt", "token", "authentication"]),
        (
            "owasp",
            "app-security",
            &["owasp", "security", "vulnerability", "xss", "sql injection"],
        ),
        (
            "cryptography",
            "app-security",
            &["crypto", "encrypt", "hash", "ssl", "tls"],
        ),
        (
            "clerk",
            "app-security",
            &["clerk", "auth", "user management"],
        ),
        (
            "figma",
            "ui-design",
            &["figma", "design", "prototype", "ui"],
        ),
        (
            "accessibility",
            "ui-design",
            &["a11y", "accessibility", "wcag", "aria"],
        ),
        (
            "design-system",
            "ui-design",
            &["design system", "tokens", "components"],
        ),
        (
            "ci-cd",
            "infra-devops",
            &["ci", "cd", "pipeline", "github actions"],
        ),
        (
            "kubernetes",
            "infra-devops",
            &["k8s", "kubernetes", "pod", "deployment"],
        ),
        ("aws", "infra-devops", &["aws", "s3", "lambda", "ec2"]),
        (
            "vercel",
            "infra-devops",
            &["vercel", "deploy", "edge", "serverless"],
        ),
        (
            "terraform",
            "infra-devops",
            &["terraform", "iac", "infrastructure"],
        ),
        ("rust", "programming-language", &["rust", "rustlang"]),
        ("golang", "programming-language", &["go", "golang"]),
        ("java", "programming-language", &["java", "jvm"]),
    ]
}

/// A guide suggestion entry (upstream GuideSuggestion).
#[derive(Debug, Clone, serde::Serialize)]
struct GuideSuggestion {
    guide: String,
    category: String,
    keywords: Vec<String>,
    tracked: bool,
    usage_count: u32,
    last_used: Option<String>,
    learnings: Vec<String>,
    contexts: Vec<String>,
}

fn tokenize(str_: &str) -> BTreeSet<String> {
    str_.to_lowercase()
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

fn has_token_match(text: &str, target: &str) -> bool {
    let text_tokens = tokenize(text);
    let target_tokens = tokenize(target);
    for token in &text_tokens {
        if target_tokens.contains(token) {
            return true;
        }
    }
    for text_token in &text_tokens {
        for target_token in &target_tokens {
            if text_token.contains(target_token) || target_token.contains(text_token) {
                return true;
            }
        }
    }
    false
}

/// Max dense-only guides appended after the token matches (heuristic cap:
/// tail cosine candidates are noise; pinned by tests).
pub const DENSE_GUIDE_APPEND_CAP: usize = 5;

/// Render a guide as passage-role embedding input: catalog text only.
/// Usage counters and timestamps are deliberately excluded — they are not
/// relevance signals.
fn guide_catalog_text(guide: &Guide) -> String {
    let mut parts = vec![guide.name.clone(), guide.description.clone()];
    parts.extend(guide.contexts.iter().cloned());
    parts.extend(guide.learnings.iter().cloned());
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dense candidate proposal over the guide catalog (WP-09): cosine-rank the
/// task (Query role) against each guide (Passage role) and return up to
/// `DENSE_GUIDE_APPEND_CAP` positively-similar guides not already
/// suggested. Scores propose candidates only — they are never displayed
/// nor treated as proof of anything. Returns empty when the task is blank,
/// the catalog is empty, dimensions mismatch, or any embedding fails, so
/// callers fall back to the token-only suggestions byte-identically.
fn suggest_guides_dense(
    backend: &crate::search::backend::SearchBackend,
    task: &str,
    existing: &[Guide],
    seen: &std::collections::BTreeSet<String>,
) -> Vec<GuideSuggestion> {
    if task.trim().is_empty() || existing.is_empty() {
        return Vec::new();
    }
    let task_vec = match backend.embed_query_sync(task) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let texts: Vec<String> = existing.iter().map(guide_catalog_text).collect();
    let guide_vecs = match backend.embed_passages_sync(&texts) {
        Ok(v) if v.len() == existing.len() => v,
        _ => return Vec::new(),
    };
    let mut ranked: Vec<(usize, f64)> = guide_vecs
        .iter()
        .enumerate()
        .filter(|(i, _)| !seen.contains(&existing[*i].name))
        .map(|(i, g)| {
            (
                i,
                crate::retrieval::ranking::cosine(Some(&task_vec), Some(g)),
            )
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
        .into_iter()
        .take(DENSE_GUIDE_APPEND_CAP)
        .map(|(i, _)| {
            let g = &existing[i];
            GuideSuggestion {
                guide: g.name.clone(),
                category: g.category.clone(),
                keywords: g.contexts.clone(),
                tracked: true,
                usage_count: g.usage_count,
                last_used: g.last_used.map(|t| date_only(t.as_millis())),
                learnings: g.learnings.clone(),
                contexts: g.contexts.clone(),
            }
        })
        .collect()
}

/// Suggest guides for a task description (upstream guides.suggestGuides).
fn suggest_guides(task_description: &str, existing_guides: &[Guide]) -> Vec<GuideSuggestion> {
    let mut suggestions: Vec<GuideSuggestion> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let desc_lower = task_description.to_lowercase();

    for (guide, category, keywords) in task_guide_defs() {
        if seen.contains(guide) {
            continue;
        }
        let guide_match = desc_lower.contains(guide);
        let keyword_match = keywords
            .iter()
            .any(|kw| desc_lower.contains(&kw.to_lowercase()));
        if guide_match || keyword_match {
            seen.insert(guide.to_string());
            let existing = existing_guides.iter().find(|g| g.name == guide);
            suggestions.push(GuideSuggestion {
                guide: guide.to_string(),
                category: category.to_string(),
                keywords: keywords.iter().map(|s| s.to_string()).collect(),
                tracked: existing.is_some(),
                usage_count: existing.map(|g| g.usage_count).unwrap_or(0),
                last_used: existing.and_then(|g| g.last_used.map(|i| date_only(i.as_millis()))),
                learnings: existing.map(|g| g.learnings.clone()).unwrap_or_default(),
                contexts: existing.map(|g| g.contexts.clone()).unwrap_or_default(),
            });
        }
    }

    for existing in existing_guides {
        if seen.contains(&existing.name) {
            continue;
        }
        if has_token_match(&desc_lower, &existing.name)
            || existing
                .contexts
                .iter()
                .any(|ctx| has_token_match(&desc_lower, ctx))
            || existing
                .learnings
                .iter()
                .any(|l| has_token_match(&desc_lower, l))
        {
            seen.insert(existing.name.clone());
            suggestions.push(GuideSuggestion {
                guide: existing.name.clone(),
                category: existing.category.clone(),
                keywords: existing.contexts.clone(),
                tracked: true,
                usage_count: existing.usage_count,
                last_used: existing.last_used.map(|i| date_only(i.as_millis())),
                learnings: existing.learnings.clone(),
                contexts: existing.contexts.clone(),
            });
        }
    }
    suggestions
}

fn format_guide_suggestions(suggestions: &[GuideSuggestion]) -> String {
    let tracked: Vec<&GuideSuggestion> = suggestions.iter().filter(|s| s.tracked).collect();
    let missing: Vec<&GuideSuggestion> = suggestions.iter().filter(|s| !s.tracked).collect();
    let summary = format!(
        "Found {} relevant guides ({} tracked, {} new)",
        suggestions.len(),
        tracked.len(),
        missing.len()
    );
    let mut output = String::from("=== GUIDE SUGGESTIONS ===\n");
    output.push_str(&format!("{summary}\n\n"));
    if !tracked.is_empty() {
        output.push_str("TRACKED (you have experience):\n");
        for s in &tracked {
            output.push_str(&format!(
                "  ✓ [{}] {} ({}x, last: {})\n",
                s.category,
                s.guide,
                s.usage_count,
                s.last_used.as_deref().unwrap_or("n/a")
            ));
            if !s.learnings.is_empty() {
                for l in s.learnings.iter().take(3) {
                    output.push_str(&format!("      💡 {l}\n"));
                }
                if s.learnings.len() > 3 {
                    output.push_str(&format!(
                        "      ... and {} more learnings\n",
                        s.learnings.len() - 3
                    ));
                }
            }
        }
        output.push('\n');
    }
    if !missing.is_empty() {
        output.push_str("SUGGESTED (not tracked yet):\n");
        for s in &missing {
            output.push_str(&format!("  + [{}] {}\n", s.category, s.guide));
            if !s.keywords.is_empty() {
                output.push_str(&format!(
                    "      keywords: {}\n",
                    s.keywords
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        output.push('\n');
    }
    if suggestions.is_empty() {
        output.push_str("No relevant guides found for this task.\n");
        output.push_str("Try describing the task with more specific terms.\n");
    }
    output.push_str("========================");
    output
}

fn format_guide_detail(guide: &Guide) -> String {
    let mut detail = format!("=== GUIDE: {} ===\n", guide.name);
    detail.push_str(&format!("Category: {}\n", guide.category));
    detail.push_str(&format!("Usage Count: {}\n", guide.usage_count));
    detail.push_str(&format!(
        "Last Used: {}\n",
        guide
            .last_used
            .map(|i| date_only(i.as_millis()))
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

fn guide_json(g: &Guide) -> Value {
    json!({
        "guide": g.name,
        "category": g.category,
        "description": g.description,
        "usage_count": g.usage_count,
        "last_used": g.last_used.map(|i| date_only(i.as_millis())),
        "success_count": g.success_count,
        "failure_count": g.failure_count,
        "contexts": g.contexts,
        "learnings": g.learnings,
    })
}

/// Build a fresh guide record (upstream createGuide).
fn create_guide(
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

fn merge_guide_refs(existing: &[String], additions: &[String], self_name: &str) -> Vec<String> {
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

// ---- guide_get ----

fn exec_guide_get(disp: &Dispatcher, args: &GuideGetArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;
    let guides = repo.get_guides()?;

    // Task-based suggestions.
    if let Some(task) = &args.task {
        let mut suggestions = suggest_guides(task, &guides);
        // Dense candidate proposal (WP-09): appends positively-similar
        // catalog guides the token path missed. Any dense failure adds
        // nothing, keeping the token-only output byte-identical.
        if let Some(backend) = disp.search() {
            let seen: std::collections::BTreeSet<String> =
                suggestions.iter().map(|s| s.guide.clone()).collect();
            suggestions.extend(suggest_guides_dense(backend, task, &guides, &seen));
        }
        let text = format_guide_suggestions(&suggestions);
        let data = json!({
            "count": suggestions.len(),
            "guides": suggestions.iter().map(|s| s.guide.clone()).collect::<Vec<_>>(),
            "guide": null,
        });
        return Ok(format_result(text, data, format));
    }

    // Single guide detail.
    if let Some(name) = &args.guide {
        let g = repo.get_guide(name)?;
        let (text, data) = match g {
            Some(g) => (
                format_guide_detail(&g),
                json!({
                    "count": 1,
                    "guides": [guide_json(&g)],
                    "guide": guide_json(&g),
                }),
            ),
            None => (
                "Guide not found.".to_string(),
                json!({ "count": 0, "guides": [], "guide": null }),
            ),
        };
        return Ok(format_result(text, data, format));
    }

    // Category filter or all.
    let filtered: Vec<&Guide> = match &args.category {
        Some(cat) => guides
            .iter()
            .filter(|g| g.category.eq_ignore_ascii_case(cat))
            .collect(),
        None => guides.iter().collect(),
    };
    let mut text = String::from("## Guides\n---\n");
    if filtered.is_empty() {
        text.push_str("(no guides tracked yet)\n---");
    } else {
        let lines: Vec<String> = filtered
            .iter()
            .take(30)
            .map(|g| {
                format!(
                    "[{}] {} — {}x usage, {} learnings",
                    g.category,
                    g.name,
                    g.usage_count,
                    g.learnings.len()
                )
            })
            .collect();
        text.push_str(&lines.join("\n"));
        text.push_str("\n---");
    }
    let data = json!({
        "count": filtered.len(),
        "guides": filtered.iter().map(|g| guide_json(g)).collect::<Vec<_>>(),
        "guide": null,
    });
    Ok(format_result(text, data, format))
}

// ---- guide_practice ----

fn exec_guide_practice(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &GuidePracticeArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guide.trim().is_empty() || args.category.trim().is_empty() {
        return Ok(err_result("'guide' and 'category' parameters are required"));
    }
    // Like the sibling tools (session_attempt/session_end): an unrecognized
    // outcome errors instead of silently dropping the signal.
    if let Some(outcome) = args.outcome.as_deref()
        && outcome != "success"
        && outcome != "failure"
    {
        return Ok(err_result("'outcome' must be one of: success, failure."));
    }
    let now = disp.clock().now_millis();
    // Track into the active session (best-effort) before the guide mutation
    // so validated_by links the session's pre-loaded reads.
    let validated: Vec<String> = {
        let mut reg = disp.registry();
        reg.track_guide_used(
            envelope.frontend_id,
            envelope.channel_id,
            args.guide.to_lowercase().trim(),
        );
        reg.resolve_session(envelope.frontend_id, envelope.channel_id)
            .and_then(|h| reg.session(h).map(|s| s.memories_read.clone()))
            .unwrap_or_default()
    };
    // Session link is durable state too (P1 eager persist).
    disp.persist_sessions();
    let outcome_bool = match args.outcome.as_deref() {
        Some("success") => Some(true),
        Some("failure") => Some(false),
        _ => None,
    };
    // Idempotent guide mutation (P1): same operation ID replays without
    // double-counting usage/success counters.
    let op_id = envelope.operation_id.as_uuid().to_string();
    // Pre-check for a practiced-then-forgotten replay: the op log survives
    // the guide, so replaying after forget is a typed error, not a silent
    // recreate.
    let updated = match repo.practice_guide_idempotent(
        &op_id,
        &args.guide,
        &args.category,
        args.description.as_deref(),
        &args.contexts,
        &args.learnings,
        &validated,
        outcome_bool,
        now,
    ) {
        Ok(g) => g,
        Err(e) if e.code == crate::domain::command::DomainErrorCode::NotFound => {
            return Ok(err_result(&e.message));
        }
        Err(e) => return Err(e),
    };

    let is_new = updated.usage_count == 1;
    let action = if is_new { "Created" } else { "Updated" };
    let mut response = format!(
        "{action} guide \"{}\" ({}): {}x usage, {} learnings, {} contexts",
        updated.name,
        updated.category,
        updated.usage_count,
        updated.learnings.len(),
        updated.contexts.len()
    );

    let total_attempts = updated.success_count + updated.failure_count;
    if total_attempts >= 3 {
        let rate = updated.success_count as f64 / total_attempts as f64;
        if rate < 0.4 {
            response.push_str(&format!(
                "\n\n--- HOOK SUGGESTIONS ---\nGuide \"{}\" success rate is {:.2} ({}/{}). Consider guide_update to refine.",
                updated.name,
                rate,
                updated.success_count,
                total_attempts
            ));
        }
    }

    let data = json!({
        "success": true,
        "guide": updated.name,
        "usage_count": updated.usage_count,
    });
    Ok(ok_result(response, data))
}

// ---- guide_create ----

fn exec_guide_create(disp: &Dispatcher, args: &GuideCreateArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guide.trim().is_empty()
        || args.category.trim().is_empty()
        || args.description.trim().is_empty()
    {
        return Ok(err_result(
            "'guide', 'category', and 'description' parameters are required",
        ));
    }
    let now = disp.clock().now_millis();

    if let Some(existing) = repo.get_guide(&args.guide)? {
        let mut updated = existing;
        updated.description = args.description.clone();
        updated.updated_at = Instant::new(now);
        repo.put_guide(&updated)?;
        return Ok(ok_result(
            format!(
                "Updated manual for existing guide \"{}\" ({})",
                updated.name, updated.category
            ),
            json!({ "success": true, "guide": updated.name }),
        ));
    }

    let guides = repo.get_guides()?;
    let normalized_lower = args.guide.to_lowercase();
    let normalized = normalized_lower.trim();
    if let Some(similar) = guides
        .iter()
        .find(|g| g.name.contains(normalized) || normalized.contains(g.name.as_str()))
    {
        let mut updated = similar.clone();
        updated.description = args.description.clone();
        updated.updated_at = Instant::new(now);
        repo.put_guide(&updated)?;
        return Ok(ok_result(
            format!(
                "Updated manual for existing guide \"{}\" ({})",
                updated.name, updated.category
            ),
            json!({ "success": true, "guide": updated.name }),
        ));
    }

    let new_guide = create_guide(
        &args.guide,
        &args.category,
        &args.description,
        &args.contexts,
        &args.learnings,
        now,
    );
    repo.put_guide(&new_guide)?;
    Ok(ok_result(
        format!(
            "Created new guide \"{}\" ({}) with a detailed manual.",
            new_guide.name, new_guide.category
        ),
        json!({ "success": true, "guide": new_guide.name }),
    ))
}

// ---- guide_distill ----

fn exec_guide_distill(disp: &Dispatcher, args: &GuideDistillArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.memory_id.trim().is_empty() || args.guide.trim().is_empty() {
        return Ok(err_result(
            "'memory_id' and 'guide' parameters are required",
        ));
    }
    let now = disp.clock().now_millis();
    let eid = match resolve_id(repo, &args.memory_id) {
        Ok(id) => id,
        Err(_) => {
            return Ok(err_result(&format!(
                "Memory fragment with ID '{}' not found.",
                args.memory_id
            )));
        }
    };
    let fragment = match repo.get_memories(&[eid])?.first() {
        Some(m) => m.clone(),
        None => {
            return Ok(err_result(&format!(
                "Memory fragment with ID '{}' not found.",
                args.memory_id
            )));
        }
    };

    let category = args
        .category
        .clone()
        .unwrap_or_else(|| "dev-tool".to_string());
    let mut updated = match repo.get_guide(&args.guide)? {
        Some(mut g) => {
            if !g.learnings.contains(&fragment.fragment) {
                g.learnings.push(fragment.fragment.clone());
            }
            let ctx = fragment
                .project
                .clone()
                .unwrap_or_else(|| "global".to_string())
                .to_lowercase()
                .trim()
                .to_string();
            if !ctx.is_empty() && !g.contexts.contains(&ctx) {
                g.contexts.push(ctx);
            }
            g.usage_count += 1;
            g.last_used = Some(Instant::new(now));
            g
        }
        None => create_guide(
            &args.guide,
            &category,
            "Created via distillation from memory.",
            &[fragment
                .project
                .clone()
                .unwrap_or_else(|| "global".to_string())
                .to_lowercase()
                .trim()
                .to_string()],
            std::slice::from_ref(&fragment.fragment),
            now,
        ),
    };

    if !updated.source_memories.contains(&eid) {
        updated.source_memories.push(eid);
    }
    updated.updated_at = Instant::new(now);
    repo.put_guide(&updated)?;

    // Update the fragment: related_guides + clear distill_candidate.
    let mut frag = fragment;
    let normalized_name = args.guide.to_lowercase().trim().to_string();
    if !frag.related_guides.contains(&normalized_name) {
        frag.related_guides.push(normalized_name);
    }
    frag.distill_candidate = false;
    repo.put_memory_direct(&frag)?;

    let response = format!(
        "Successfully distilled memory [{}] into guide \"{}\" ({}).\n\n{}",
        args.memory_id,
        updated.name,
        updated.category,
        format_guide_detail(&updated)
    );
    Ok(ok_result(
        response,
        json!({
            "success": true,
            "guide": updated.name,
            "memory_id": args.memory_id,
        }),
    ))
}

// ---- guide_update ----

fn exec_guide_update(disp: &Dispatcher, args: &GuideUpdateArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guide.trim().is_empty() {
        return Ok(err_result("'guide' parameter is required"));
    }
    let now = disp.clock().now_millis();
    let mut guide = match repo.get_guide(&args.guide)? {
        Some(g) => g,
        None => return Ok(err_result(&format!("Guide \"{}\" not found.", args.guide))),
    };

    let old_name = guide.name.clone();
    if let Some(new_name) = &args.new_name
        && !new_name.trim().is_empty()
    {
        guide.name = new_name.to_lowercase().trim().to_string();
    }
    if let Some(category) = &args.category
        && !category.trim().is_empty()
    {
        guide.category = category.to_lowercase().trim().to_string();
    }
    if let Some(description) = &args.description
        && !description.trim().is_empty()
    {
        guide.description = description.trim().to_string();
    }
    if !args.add_anti_patterns.is_empty() {
        guide.anti_patterns.extend(args.add_anti_patterns.clone());
    }
    if !args.add_pitfalls.is_empty() {
        guide.pitfalls.extend(args.add_pitfalls.clone());
    }
    if !args.add_depends_on.is_empty() {
        guide.depends_on = merge_guide_refs(&guide.depends_on, &args.add_depends_on, &guide.name);
    }
    if !args.add_enables.is_empty() {
        guide.enables = merge_guide_refs(&guide.enables, &args.add_enables, &guide.name);
    }
    if let Some(superseded_by) = &args.superseded_by
        && !superseded_by.trim().is_empty()
    {
        guide.superseded_by = Some(superseded_by.clone());
    }
    if args.deprecated {
        guide.deprecated = true;
    }
    guide.updated_at = Instant::new(now);

    // Rename path (P1 atomic, no tear): put the renamed guide first so a
    // later failure never loses it, then move references fresh-read, then
    // delete the old key. Every step propagates errors.
    if !old_name.eq_ignore_ascii_case(&guide.name) {
        repo.put_guide(&guide)?;
        rename_guide_in_memories(repo, &old_name, &guide.name)?;
        repo.delete_guide(&old_name)?;
        // Already stored above; return without a second put.
        return Ok(ok_result(
            format!(
                "Updated guide \"{}\":\n{}",
                guide.name,
                format_guide_detail(&guide)
            ),
            json!({ "success": true, "guide": guide.name }),
        ));
    }
    repo.put_guide(&guide)?;

    Ok(ok_result(
        format!(
            "Updated guide \"{}\":\n{}",
            guide.name,
            format_guide_detail(&guide)
        ),
        json!({ "success": true, "guide": guide.name }),
    ))
}

// ---- guide_forget ----

fn exec_guide_forget(disp: &Dispatcher, args: &GuideForgetArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guide.trim().is_empty() {
        return Ok(err_result("'guide' parameter is required"));
    }
    let existing = repo.get_guide(&args.guide)?;
    if existing.is_none() {
        return Ok(err_result(&format!("Guide \"{}\" not found.", args.guide)));
    }
    // Remove-then-delete (P1, no tear): references go first so a failure
    // leaves the guide with refs (retryable), never dangling refs to a
    // deleted guide. Errors propagate.
    remove_guide_from_memories(repo, &args.guide)?;
    repo.delete_guide(&args.guide)?;
    Ok(ok_result(
        format!("Successfully forgot guide: {}", args.guide),
        json!({ "success": true, "guide": args.guide }),
    ))
}

// ---- guide_merge ----

fn exec_guide_merge(disp: &Dispatcher, args: &GuideMergeArgs) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guides.len() < 2 {
        return Ok(err_result(
            "'guides' must be an array with at least 2 guide names",
        ));
    }
    if args.guide.trim().is_empty() || args.category.trim().is_empty() {
        return Ok(err_result("'guide' and 'category' parameters are required"));
    }
    let now = disp.clock().now_millis();

    let mut source_guides: Vec<Guide> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    for name in &args.guides {
        match repo.get_guide(name)? {
            Some(g) => source_guides.push(g),
            None => not_found.push(name.clone()),
        }
    }
    if !not_found.is_empty() {
        return Ok(err_result(&format!(
            "Guide(s) not found: {}",
            not_found.join(", ")
        )));
    }

    let contexts = args.contexts.clone().unwrap_or_else(|| {
        let mut set: Vec<String> = Vec::new();
        for g in &source_guides {
            for c in &g.contexts {
                if !set.contains(c) {
                    set.push(c.clone());
                }
            }
        }
        set
    });
    let learnings = args.learnings.clone().unwrap_or_else(|| {
        let mut set: Vec<String> = Vec::new();
        for g in &source_guides {
            for l in &g.learnings {
                if !set.contains(l) {
                    set.push(l.clone());
                }
            }
        }
        set
    });
    let anti_patterns = dedup(
        source_guides
            .iter()
            .flat_map(|g| g.anti_patterns.iter().cloned())
            .collect::<Vec<_>>(),
    );
    let pitfalls = dedup(
        source_guides
            .iter()
            .flat_map(|g| g.pitfalls.iter().cloned())
            .collect::<Vec<_>>(),
    );

    let total_usage: u32 = source_guides.iter().map(|g| g.usage_count).sum();
    let mut new_guide = create_guide(
        &args.guide,
        &args.category,
        args.description.as_deref().unwrap_or(""),
        &contexts,
        &learnings,
        now,
    );
    new_guide.usage_count = total_usage;
    new_guide.anti_patterns = anti_patterns.clone();
    new_guide.pitfalls = pitfalls.clone();
    new_guide.source_memories = dedup(
        source_guides
            .iter()
            .flat_map(|g| g.source_memories.iter().cloned())
            .collect::<Vec<_>>(),
    );
    new_guide.validated_by = dedup(
        source_guides
            .iter()
            .flat_map(|g| g.validated_by.iter().cloned())
            .collect::<Vec<_>>(),
    );

    // Atomic single-transaction merge (P1, no tear): references, source
    // deletes and the merged put commit together. Errors propagate.
    repo.merge_guides_atomically(&args.guides, &new_guide)?;

    let mut response = format!(
        "Merged {} guides into \"{}\" ({})\n",
        args.guides.len(),
        new_guide.name,
        new_guide.category
    );
    response.push_str(&format!(
        "Total usage: {}x | Contexts: {} | Learnings: {}\n",
        total_usage,
        contexts.len(),
        learnings.len()
    ));
    response.push_str(&format!("Removed: {}", args.guides.join(", ")));

    let mut hook: Vec<String> = Vec::new();
    if !anti_patterns.is_empty() {
        hook.push(format!("Anti-patterns inherited: {}", anti_patterns.len()));
    }
    if !pitfalls.is_empty() {
        hook.push(format!("Pitfalls inherited: {}", pitfalls.len()));
    }
    let all_source_mems: Vec<_> = source_guides
        .iter()
        .flat_map(|g| g.source_memories.iter())
        .collect();
    if !all_source_mems.is_empty() {
        hook.push(format!(
            "Source memories linked: {} fragment(s)",
            all_source_mems.len()
        ));
    }
    let all_validated: Vec<_> = source_guides
        .iter()
        .flat_map(|g| g.validated_by.iter())
        .collect();
    if !all_validated.is_empty() {
        hook.push(format!("Validated by: {} fragment(s)", all_validated.len()));
    }
    if !hook.is_empty() {
        response.push_str("\n\n--- HOOK SUGGESTIONS ---\n");
        for h in &hook {
            response.push_str(&format!("{h}\n"));
        }
    }

    Ok(ok_result(
        response,
        json!({
            "success": true,
            "guide": new_guide.name,
            "merged": args.guides,
        }),
    ))
}

/// Order-preserving deduplication (upstream `[...new Set(...)]`).
fn dedup<T: PartialEq>(items: Vec<T>) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// Rename a guide reference in every memory's `related_guides`
/// (upstream `core.renameGuideInMemories`). Atomic via fresh-read patch:
/// concurrent content updates are preserved, errors propagate.
fn rename_guide_in_memories(
    repo: &crate::service::repository::CanonicalRepository,
    old_name: &str,
    new_name: &str,
) -> DomainResult<usize> {
    repo.rename_guide_references(old_name, new_name)
}

/// Remove a guide reference from every memory's `related_guides`
/// (upstream `core.removeGuideFromMemories`). Atomic via fresh-read patch.
fn remove_guide_from_memories(
    repo: &crate::service::repository::CanonicalRepository,
    guide_name: &str,
) -> DomainResult<usize> {
    repo.remove_guide_references(guide_name)
}

// ---- session_start ----

fn exec_session_start(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &SessionStartArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.task_type.trim().is_empty() {
        return Ok(err_result("'task_type' parameter is required"));
    }
    let now = disp.clock().now_millis();
    // The frozen session_start schema carries no project field; the channel's
    // session is project-less (upstream resolves it from cwd, which the daemon
    // does not observe).
    let project: Option<String> = None;

    // Abandon any existing active session for this channel; create a fresh one.
    let handle = {
        let mut reg = disp.registry();
        reg.decay_attempts(0.002);
        let h = reg.start_legacy_session(
            envelope.frontend_id,
            envelope.channel_id,
            args.task_type.clone(),
            args.technologies.clone(),
            now,
        );
        if let Some(s) = reg.session_mut(h) {
            s.initial_approach = args.initial_approach.clone();
        }
        h
    };

    // Guide suggestions for the task description.
    let task_desc = format!("{} {}", args.task_type, args.technologies.join(" "));
    let guides = repo.get_guides()?;
    let suggestions = suggest_guides(&task_desc, &guides);
    let formatted_suggestions = format_guide_suggestions(&suggestions);

    // Pre-load relevant memories: dense-ranked recall when a search
    // backend is attached, identical lexical fallback otherwise.
    // recall_browse owns both paths (engine ranking + snapshot scan),
    // so preload never diverges from browse recall.
    let browse_args = MemoryReadArgs {
        query: Some(task_desc.clone()),
        project: None,
        all: true,
        ..Default::default()
    };
    let mut relevant: Vec<Memory> = recall_browse(disp, &browse_args)?;
    relevant.truncate(3);

    // Boost pre-loaded memories (upstream boostConfidence 0.02).
    let boosted: Vec<EntityId> = relevant.iter().map(|m| m.id).collect();
    if !boosted.is_empty() {
        let ctx = sub_command_ctx(envelope, 0)?;
        let cmd = DomainCommand::BoostConfidence {
            memory_ids: boosted,
        };
        let _ = disp.repo().apply(&ctx, &cmd);
    }

    // Track read memories into the session.
    let read_ids: Vec<String> = relevant.iter().map(|m| legacy_id_of(repo, m)).collect();
    {
        let mut reg = disp.registry();
        reg.track_memories_read(envelope.frontend_id, envelope.channel_id, &read_ids);
    }
    // Durable before ack (P1): a kill after start loses nothing.
    disp.persist_sessions();

    let mut response = format!(
        "Session started: {} ({})\n",
        handle.as_uuid(),
        args.task_type
    );
    if !args.technologies.is_empty() {
        response.push_str(&format!("Technologies: {}\n", args.technologies.join(", ")));
    }

    if !relevant.is_empty() {
        response.push_str("\nPre-loaded memories:\n");
        for m in &relevant {
            let scope_tag = m.project.clone().unwrap_or_else(|| "global".to_string());
            response.push_str(&format!(
                "  [{}] [{}] {} ({:.2})\n    {}\n",
                legacy_id_of(repo, m),
                scope_tag,
                m.title,
                m.confidence,
                m.description
            ));
        }
    }

    response.push_str(&format!("\n{formatted_suggestions}"));

    // Continuity recall: dead-ends + lessons + warnings from prior sessions.
    let continuity = build_continuity_recall(disp, &args.task_type, project.as_deref(), now);
    if !continuity.is_empty() {
        response.push_str(&continuity);
    }

    // Surface pending improvement suggestions.
    let pending = repo
        .get_suggestions()?
        .into_iter()
        .filter(|s| s.status == SuggestionStatus::Pending)
        .take(3)
        .collect::<Vec<_>>();
    if !pending.is_empty() {
        response
            .push_str("\n\n## Past improvement suggestions (consider; dismiss if not relevant)\n");
        for s in &pending {
            response.push_str(&format!("- [{}] {}\n", s.id, s.suggestion));
        }
    }

    let guide_names: Vec<String> = suggestions.iter().map(|s| s.guide.clone()).collect();
    let data = json!({
        "session_id": handle.as_uuid().to_string(),
        "guides": guide_names,
        "preloaded_memories": read_ids,
    });
    Ok(ok_result(response, data))
}

/// Continuity recall: dead-ends, lessons and warnings from prior sessions
/// (upstream buildContinuityRecall).
fn build_continuity_recall(
    disp: &Dispatcher,
    task_type: &str,
    project: Option<&str>,
    now: u64,
) -> String {
    let sessions = disp.registry().all_sessions_owned();

    // Layer 1 — dead ends from similar prior sessions.
    let mut dead_ends: Vec<(SessionHandle, u32, String, Option<String>)> = Vec::new();
    for s in &sessions {
        if s.task_type.as_deref() != Some(task_type) {
            continue;
        }
        if let Some(p) = project
            && let Some(sp) = &s.project
            && sp != p
        {
            continue;
        }
        for a in &s.attempts {
            if matches!(
                a.outcome,
                AttemptOutcome::Rejected | AttemptOutcome::Partial
            ) && a.confidence >= 0.2
            {
                dead_ends.push((s.handle, a.seq, a.approach.clone(), a.critique.clone()));
            }
        }
    }
    dead_ends.sort_by(|a, b| {
        b.3.clone()
            .unwrap_or_default()
            .len()
            .cmp(&a.3.clone().unwrap_or_default().len())
    });
    dead_ends.truncate(15);

    // Layer 2 — lessons from completed similar sessions.
    let mut lessons: Vec<String> = Vec::new();
    for s in &sessions {
        if s.task_type.as_deref() != Some(task_type) || s.outcome.is_none() {
            continue;
        }
        if let Some(p) = project
            && let Some(sp) = &s.project
            && sp != p
        {
            continue;
        }
        for l in &s.lessons {
            if !l.trim().is_empty() && !lessons.contains(l) {
                lessons.push(l.clone());
            }
        }
    }
    lessons.truncate(5);

    // Layer 3 — warning fragments for this project (or global).
    let repo = disp.repo();
    let export = repo.export_snapshot().unwrap_or_default();
    let warnings: Vec<&Memory> = export
        .memories
        .iter()
        .filter(|m| m.lifecycle.is_recallable())
        .filter(|m| matches!(m.fragment_type, FragmentType::Warning))
        .filter(|m| {
            project
                .map(|p| m.project.as_deref() == Some(p) || m.project.is_none())
                .unwrap_or(true)
        })
        .collect();

    if dead_ends.is_empty() && lessons.is_empty() && warnings.is_empty() {
        return String::new();
    }

    let mut block = format!("\n\n## Prior reasoning on similar {task_type} tasks");
    if !dead_ends.is_empty() {
        block.push_str("\n### Dead ends (don't repeat)");
        let mut boosted: Vec<(SessionHandle, u32)> = Vec::new();
        for (handle, seq, approach, critique) in &dead_ends {
            block.push_str(&format!(
                "\n- Tried: {approach}. Rejected because: {}",
                critique.as_deref().unwrap_or("unknown")
            ));
            boosted.push((*handle, *seq));
        }
        // Boost recalled attempts (best-effort).
        let mut reg = disp.registry();
        for (handle, seq) in boosted {
            reg.boost_attempt(handle, seq, 0.015, now);
        }
    }
    if !lessons.is_empty() {
        block.push_str("\n### What worked / lessons");
        for l in &lessons {
            block.push_str(&format!("\n- {l}"));
        }
    }
    if !warnings.is_empty() {
        block.push_str("\n### Warnings");
        for w in warnings.iter().take(5) {
            let text = if !w.title.trim().is_empty() {
                w.title.clone()
            } else {
                w.fragment.chars().take(120).collect::<String>()
            };
            block.push_str(&format!("\n- {text}"));
        }
    }
    block
}

// ---- session_attempt ----

fn exec_session_attempt(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &SessionAttemptArgs,
) -> DomainResult<DomainPayload> {
    if args.approach.trim().is_empty() || args.outcome.trim().is_empty() {
        return Ok(err_result(
            "'approach' and 'outcome' are required for session_attempt.",
        ));
    }
    let outcome = match AttemptOutcome::parse(&args.outcome) {
        Some(o) => o,
        None => {
            return Ok(err_result(
                "'outcome' must be one of: rejected, partial, promising.",
            ));
        }
    };

    // Redact secrets from free-text fields (upstream redactSecrets).
    let approach_redacted = privacy::redact(&args.approach);
    let critique_redacted = args.critique.as_deref().map(privacy::redact);

    // Resolve the channel's active session.
    let session = {
        let reg = disp.registry();
        reg.resolve_session(envelope.frontend_id, envelope.channel_id)
    };
    let Some(handle) = session else {
        return Ok(err_result(
            "No active session. Call session_start before recording attempts.",
        ));
    };

    // Resolve the related memory ID (best-effort).
    let related_memory_id = args
        .related_memory_id
        .as_deref()
        .and_then(|id| disp.repo().resolve_id(id).ok());

    let now = disp.clock().now_millis();
    let attempt_id = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:attempt:{}", envelope.operation_id.as_uuid()).as_bytes(),
    ));

    // Record the attempt and compute its seq. Replay-safe (P1): the attempt
    // ID derives deterministically from the operation ID, so a retried
    // operation reuses its ID — `record_attempt` dedups and we skip counter
    // increments, reporting the original seq.
    let seq = {
        let mut reg = disp.registry();
        if let Some(existing) = reg
            .session(handle)
            .and_then(|s| s.attempts.iter().find(|a| a.id == attempt_id))
        {
            existing.seq
        } else {
            let next_seq = reg
                .session(handle)
                .map(|s| s.attempts.len() as u32 + 1)
                .unwrap_or(1);
            let attempt = Attempt {
                id: attempt_id,
                session_id: handle,
                seq: next_seq,
                approach: approach_redacted.clone(),
                outcome,
                critique: critique_redacted.clone(),
                rationale: args.rationale.clone(),
                related_memory_id,
                confidence: 1.0,
                access_count: 0,
                last_accessed_at: None,
                created_at: Instant::new(now),
            };
            reg.record_attempt(envelope.frontend_id, envelope.channel_id, attempt);
            // Self-critique + refinement counters: exactly once per operation.
            if let Some(s) = reg.session_mut(handle) {
                s.refinement_attempts += 1;
                if matches!(outcome, AttemptOutcome::Rejected | AttemptOutcome::Partial)
                    && critique_redacted.is_some()
                {
                    s.self_critique_count += 1;
                }
            }
            next_seq
        }
    };
    // Durable before ack (P1): kill-after-ack loses nothing.
    disp.persist_sessions();

    let value_tag = match outcome {
        AttemptOutcome::Rejected => "(dead end — most valuable)",
        AttemptOutcome::Partial => "(partial)",
        AttemptOutcome::Promising => "(promising)",
    };
    let preview = if approach_redacted.len() > 80 {
        let mut end = 80;
        while !approach_redacted.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &approach_redacted[..end])
    } else {
        approach_redacted.clone()
    };
    let response = format!("Recorded attempt #{seq} — {preview} {value_tag}.");
    let data = json!({
        "recorded": true,
        "attempt_id": format!("{}#{}", handle.as_uuid(), seq),
    });
    Ok(ok_result(response, data))
}

// ---- session_end ----

fn exec_session_end(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &SessionEndArgs,
) -> DomainResult<DomainPayload> {
    if args.outcome.trim().is_empty() {
        return Ok(err_result("'outcome' parameter is required"));
    }
    let outcome = match TaskOutcome::parse(&args.outcome) {
        Some(o) => o,
        None => {
            return Ok(err_result(
                "'outcome' must be one of: success, partial, failure, abandoned.",
            ));
        }
    };

    let now = disp.clock().now_millis();
    let session = {
        let reg = disp.registry();
        reg.resolve_session(envelope.frontend_id, envelope.channel_id)
    };
    let Some(handle) = session else {
        return Ok(err_result("No active session to end."));
    };

    // End the session (only this channel's).
    {
        let mut reg = disp.registry();
        reg.end_session(
            envelope.frontend_id,
            envelope.channel_id,
            outcome,
            args.final_approach.clone(),
            args.lessons.clone(),
            now,
        );
    }
    // Durable before ack (P1): outcome/lessons survive kill-after-ack.
    disp.persist_sessions();

    let repo = disp.repo();
    let mut improvement_lines: Vec<String> = Vec::new();

    // Evaluate guides used in this session.
    let guides_used = {
        let reg = disp.registry();
        reg.session(handle)
            .map(|s| s.guides_used.clone())
            .unwrap_or_default()
    };
    for guide_name in &guides_used {
        if let Some(mut guide) = repo.get_guide(guide_name)? {
            if outcome == TaskOutcome::Success {
                guide.success_count += 1;
            } else if outcome == TaskOutcome::Failure {
                guide.failure_count += 1;
                let total = guide.success_count + guide.failure_count;
                if total >= 3 {
                    let rate = guide.success_count as f64 / total as f64;
                    if rate < 0.4 {
                        improvement_lines.push(format!(
                            "  [!] Guide \"{}\" success rate is {:.2} ({}/{total}). Consider refining with guide_update.",
                            guide.name, rate, guide.success_count
                        ));
                    }
                }
            }
            guide.updated_at = Instant::new(now);
            repo.put_guide(&guide)?;
        }
    }

    // Persist improvement suggestions (best-effort).
    for line in &improvement_lines {
        let id = repo.next_suggestion_id()?;
        let suggestion = Suggestion {
            id,
            session_id: Some(handle.as_uuid().to_string()),
            suggestion: line.trim().to_string(),
            status: SuggestionStatus::Pending,
            created_at: Instant::new(now),
            resolved_at: None,
        };
        let _ = repo.put_suggestion(&suggestion);
    }

    let session_end_info = {
        let reg = disp.registry();
        reg.session(handle).cloned()
    };
    let started = session_end_info
        .as_ref()
        .map(|s| s.started_at.as_millis())
        .unwrap_or(now);

    let mut response = format!("Session {} ended: {}\n", handle.as_uuid(), args.outcome);
    if let Some(s) = &session_end_info {
        response.push_str(&format!(
            "Task: {} | Duration: {} → {}\n",
            s.task_type.clone().unwrap_or_default(),
            iso8601(started),
            iso8601(now)
        ));
        if !s.lessons.is_empty() {
            response.push_str(&format!("Lessons: {} recorded\n", s.lessons.len()));
        }
    }
    if !improvement_lines.is_empty() {
        response.push_str(&format!(
            "\nIMPROVEMENT SUGGESTIONS:\n{}\n",
            improvement_lines.join("\n")
        ));
    }

    // Session review.
    if let Some(s) = &session_end_info
        && (!s.memories_read.is_empty()
            || !s.memories_created.is_empty()
            || !s.guides_used.is_empty())
    {
        response.push_str("\nSESSION REVIEW:");
        if !s.memories_read.is_empty() {
            response.push_str(&format!(
                "\n  Memories read: {}",
                s.memories_read
                    .iter()
                    .map(|m| format!("[{m}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !s.memories_created.is_empty() {
            response.push_str(&format!(
                "\n  Memories created: {}",
                s.memories_created
                    .iter()
                    .map(|m| format!("[{m}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !s.guides_used.is_empty() {
            response.push_str(&format!("\n  Guides used: {}", s.guides_used.join(", ")));
        }
    }

    let data = json!({
        "outcome_recorded": true,
        "suggestions": improvement_lines,
    });
    Ok(ok_result(response, data))
}

// ---- session_stats ----

fn exec_session_stats(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &SessionStatsArgs,
) -> DomainResult<DomainPayload> {
    let count = args.count.unwrap_or(10);
    let format = args.response_format;

    // One guard for the whole read: chaining disp.registry() calls in a
    // single expression would deadlock (the first temporary guard outlives
    // the nested lock on a non-reentrant Mutex).
    let reg = disp.registry();
    let sessions = reg.all_sessions_owned();

    // Recent completed sessions (most recent first).
    let mut completed: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.status == crate::domain::session::SessionStatus::Ended)
        .collect();
    completed.sort_by_key(|s| std::cmp::Reverse(s.started_at.as_millis()));
    completed.truncate(count.min(5));

    // Active session for this channel.
    let active = reg
        .resolve_session(envelope.frontend_id, envelope.channel_id)
        .and_then(|h| reg.session(h).cloned());

    let mut output = String::from("## Session Stats\n");
    if let Some(current) = &active {
        output.push_str(&format!(
            "Active session: {} tool calls\n",
            current.attempts.len()
        ));
        if !current.technologies.is_empty() {
            output.push_str(&format!(
                "Technologies: {}\n",
                current.technologies.join(", ")
            ));
        }
        if !current.guides_used.is_empty() {
            output.push_str(&format!(
                "Guides used: {}\n",
                current.guides_used.join(", ")
            ));
        }
        output.push('\n');
    }

    if !completed.is_empty() {
        output.push_str(&format!("Recent sessions ({}):\n", completed.len()));
        for s in &completed {
            let techs = if !s.technologies.is_empty() {
                format!(" [{}]", s.technologies.join(", "))
            } else {
                String::new()
            };
            output.push_str(&format!(
                "  {}: {} calls{techs}\n",
                s.handle.as_uuid(),
                s.attempts.len()
            ));
        }
    } else {
        output.push_str("No past sessions recorded yet.\n");
    }

    let data = json!({
        "active_session": active.as_ref().map(|c| {
            json!({
                "tool_calls": c.attempts.len(),
                "technologies": c.technologies,
                "guides_used": c.guides_used,
            })
        }),
        "recent_sessions": completed.iter().map(|s| {
            json!({
                "id": s.handle.as_uuid().to_string(),
                "duration_tool_calls": s.attempts.len(),
                "technologies": s.technologies,
            })
        }).collect::<Vec<_>>(),
    });
    Ok(format_result(output, data, format))
}

// ---- suggestion_respond ----

fn exec_suggestion_respond(
    disp: &Dispatcher,
    args: &SuggestionRespondArgs,
) -> DomainResult<DomainPayload> {
    let action = args.action.to_lowercase();
    if !matches!(action.as_str(), "accept" | "dismiss") {
        return Ok(err_result("'action' must be one of: accept, dismiss."));
    }
    let repo = disp.repo();
    let status = if action == "accept" {
        SuggestionStatus::Accepted
    } else {
        SuggestionStatus::Dismissed
    };

    let now = disp.clock().now_millis();
    let mut suggestion = match repo.get_suggestion(args.id)? {
        Some(s) => s,
        None => return Ok(err_result("Could not update this suggestion in the store.")),
    };
    suggestion.status = status;
    suggestion.resolved_at = Some(Instant::new(now));
    repo.put_suggestion(&suggestion)?;

    // Adjust attempt confidence based on the action (best-effort).
    if let Some(session_id) = &suggestion.session_id {
        let handle_uuid = uuid::Uuid::parse_str(session_id).ok();
        if let Some(h) = handle_uuid {
            let handle = crate::domain::id::SessionHandle::new(h);
            let attempts = disp
                .registry()
                .session(handle)
                .map(|s| s.attempts.clone())
                .unwrap_or_default();
            let mut reg = disp.registry();
            if action == "dismiss" {
                for a in &attempts {
                    if matches!(
                        a.outcome,
                        AttemptOutcome::Rejected | AttemptOutcome::Partial
                    ) {
                        reg.penalize_attempt(handle, a.seq, 0.02, now);
                    }
                }
            } else {
                for a in &attempts {
                    if a.outcome == AttemptOutcome::Promising {
                        reg.boost_attempt(handle, a.seq, 0.02, now);
                    }
                }
            }
        }
    }

    let message = if action == "accept" {
        format!(
            "Accepted suggestion #{}. It will no longer be surfaced; related promising attempts were reinforced.",
            args.id
        )
    } else {
        format!(
            "Dismissed suggestion #{}. It will no longer be surfaced; related dead ends were de-prioritized.",
            args.id
        )
    };
    let data = json!({ "resolved": true, "id": args.id });
    Ok(ok_result(message, data))
}

// ---- conflict_scan ----

fn exec_conflict_scan(disp: &Dispatcher, args: &ConflictScanArgs) -> DomainResult<DomainPayload> {
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

    let legacy_id_of = |m: &Memory| repo.legacy_id(m);
    let conflicts =
        crate::compatibility::lemma::intelligence::scan_for_conflicts(&memories, legacy_id_of);
    let text = crate::compatibility::lemma::intelligence::format_conflict_results(&conflicts);
    let data = json!({ "count": conflicts.len(), "conflicts": conflicts });
    Ok(format_result(text, data, format))
}

// ---- proactive_analysis ----

fn exec_proactive_analysis(
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
    let mut suggestions = crate::compatibility::lemma::intelligence::run_full_analysis(
        &memories,
        &guides,
        now,
        legacy_id_of,
    );

    // Conflict count suggestion.
    let conflicts =
        crate::compatibility::lemma::intelligence::scan_for_conflicts(&memories, legacy_id_of);
    if !conflicts.is_empty() {
        suggestions.push(
            crate::compatibility::lemma::intelligence::ProactiveSuggestion {
                r#type: "conflict".into(),
                priority: "high".into(),
                message: format!(
                    "{} conflicting memory pair(s) detected. Run conflict_scan for details.",
                    conflicts.len()
                ),
                suggested_action: None,
            },
        );
    }

    let formatted = crate::compatibility::lemma::intelligence::format_suggestions(&suggestions);
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

// ---- backup_create (WP-11a; native tool) ----

/// Back up the canonical store plus registry sessions to one verified
/// native archive. `directory` is required (Usage-style soft error when
/// absent — ltmrs invents no default backup location, unlike the upstream
/// default; recorded in the native tool description).
fn exec_backup_create(disp: &Dispatcher, args: &BackupCreateArgs) -> DomainResult<DomainPayload> {
    let dir = match args.directory.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => {
            return Ok(err_result(
                "backup_create requires a destination `directory` (created when missing)",
            ));
        }
    };
    let now = disp.clock().now_millis();
    let sessions = disp.registry().all_sessions_owned();
    let report = crate::interchange::backup::export_backup(
        disp.repo(),
        &sessions,
        std::path::Path::new(&dir),
        "ltmrs",
        now,
    )
    .map_err(|e| {
        crate::domain::command::DomainError::new(
            crate::domain::command::DomainErrorCode::Validation,
            format!("backup failed: {e}"),
        )
    })?;
    let count = |key: &str| report.counts.get(key).copied().unwrap_or(0);
    let text = format!(
        "Backed up {} memories, {} guides ({} sessions) to {}\nDigest: {}",
        count("memories"),
        count("guides"),
        count("sessions"),
        report.path.display(),
        report.digest,
    );
    Ok(ok_result(
        text,
        serde_json::json!({
            "path": report.path.to_string_lossy(),
            "digest": report.digest,
            "counts": report.counts,
        }),
    ))
}

/// Live per-collection counts in manifest shape (for preview comparison).
fn live_counts(disp: &Dispatcher) -> DomainResult<BTreeMap<String, u64>> {
    let export = disp.repo().export_full()?;
    let sessions = disp.registry().all_sessions_owned();
    let count = |n: usize| n as u64;
    Ok(BTreeMap::from([
        ("memories".to_string(), count(export.memories.len())),
        ("relations".to_string(), count(export.relations.len())),
        ("guides".to_string(), count(export.guides.len())),
        ("sessions".to_string(), count(sessions.len())),
        ("feedback".to_string(), count(export.feedback.len())),
        ("suggestions".to_string(), count(export.suggestions.len())),
        ("projects".to_string(), count(export.projects.len())),
        ("archives".to_string(), count(export.archives.len())),
        ("history".to_string(), count(export.history.len())),
    ]))
}

// ---- backup_preview / backup_restore (WP-11b; native tools) ----

/// Preview a native backup without replacing anything: verify the file,
/// compare counts, check cooperating connections, and on readiness issue a
/// single-use TTL-bound token (bound to file digest, store generation and
/// channel, mirroring the upstream readiness contract).
fn exec_backup_preview(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &BackupPreviewArgs,
) -> DomainResult<DomainPayload> {
    use crate::interchange::backup::{MAX_BACKUP_BYTES, verify_backup_file};
    let path = match args.path.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            return Ok(err_result(
                "backup_preview requires a `path` to a .ltmrs-backup file",
            ));
        }
    };
    let verified =
        verify_backup_file(std::path::Path::new(&path), MAX_BACKUP_BYTES).map_err(|e| {
            crate::domain::command::DomainError::new(
                crate::domain::command::DomainErrorCode::Validation,
                format!("backup preview failed: {e}"),
            )
        })?;
    let live = live_counts(disp)?;
    let generation = disp
        .repo()
        .store_generation()
        .map_err(|e| {
            crate::domain::command::DomainError::new(
                crate::domain::command::DomainErrorCode::Validation,
                format!("backup preview failed: {}", e.message),
            )
        })?
        .as_u64();
    let now = disp.clock().now_millis();
    // Readiness counts LIVE connections: persisted channel bindings
    // outlive their runs (every restart binds anew) and must never
    // block a restore after a daemon restart.
    let channels = disp.registry().live_connection_count();
    let channel = envelope.channel_id.as_uuid().to_string();
    let live_op_seq = disp.repo().op_seq().map_err(|e| {
        crate::domain::command::DomainError::new(
            crate::domain::command::DomainErrorCode::Validation,
            format!("backup preview failed: {}", e.message),
        )
    })?;
    let preview = disp
        .restore_coordinator()
        .preview(crate::interchange::restore::PreviewRequest {
            backup: &verified,
            source_path: std::path::Path::new(&path),
            channel: &channel,
            live_counts: &live,
            live_generation: generation,
            active_channels: channels,
            now_millis: now,
            live_op_seq,
        });
    let text = if preview.ready {
        format!(
            "Restore preview: READY. {}\nConfirm replaces the live store (never merges): call backup_restore with the confirmation token and confirm=true.",
            preview.message
        )
    } else {
        format!(
            "Restore preview: BLOCKED. {}\nKeep this connection open and preview again after other connections close.",
            preview.message
        )
    };
    Ok(ok_result(
        text,
        serde_json::json!({
            "readiness": {"status": if preview.ready { "ready" } else { "blocked" }, "message": preview.message},
            "unknown_top_level": preview.unknown_top_level,
            "confirmation_token": preview.confirmation_token,
            "expires_at": preview.expires_at,
        }),
    ))
}

/// Restore a previewed backup (REPLACE, never merge): re-verify the file,
/// consume the single-use token, write a safety backup first, replace the
/// records, bump the generation (invalidating pre-restore pipelines) and
/// abandon live sessions. Rollback is a second restore of the safety file.
fn exec_backup_restore(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &BackupRestoreArgs,
) -> DomainResult<DomainPayload> {
    use crate::domain::command::{DomainError, DomainErrorCode};
    use crate::interchange::backup::{
        MAX_BACKUP_BYTES, encode_backup, export_backup_to, verify_backup_file,
    };
    use crate::interchange::restore::{RestoreError, restore_verified, safety_backup_path};
    let fail = |message: String| DomainError::new(DomainErrorCode::Validation, message);
    let token = match args.confirmation_token.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return Ok(err_result(
                "backup_restore requires the `confirmation_token` from backup_preview (preview again for a fresh one)",
            ));
        }
    };
    if args.confirm != Some(true) {
        return Ok(err_result(
            "backup_restore replaces the live store (never merges). Pass confirm=true to acknowledge, or preview again.",
        ));
    }
    let now = disp.clock().now_millis();
    let channel = envelope.channel_id.as_uuid().to_string();
    let source = match disp.restore_coordinator().source_path(&token) {
        Some(p) => p,
        None => {
            return Ok(err_result(
                "unknown confirmation token (preview again for a fresh one)",
            ));
        }
    };
    let verified = verify_backup_file(&source, MAX_BACKUP_BYTES)
        .map_err(|e| fail(format!("backup restore failed: {e}")))?;
    let live_generation = disp
        .repo()
        .store_generation()
        .map_err(|e| fail(format!("backup restore failed: {}", e.message)))?
        .as_u64();
    // Re-check the preview lease: a connection that arrived after the
    // preview may hold acknowledged writes the replace would drain unseen.
    // Writes that landed anyway (same channel, transient writers) are
    // counted, not refused: the replace drains them, so the report must
    // acknowledge the delta (recoverable from the safety backup).
    // Live connections again (see preview): history never blocks.
    let active_channels = disp.registry().live_connection_count();
    let live_op_seq = disp
        .repo()
        .op_seq()
        .map_err(|e| fail(format!("backup restore failed: {}", e.message)))?;
    let (_, live_writes_since_preview) = disp
        .restore_coordinator()
        .confirm(crate::interchange::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: &verified.digest,
            live_generation,
            channel: &channel,
            active_channels,
            now_millis: now,
            live_op_seq,
        })
        .map_err(|e| match e {
            RestoreError::InvalidToken
            | RestoreError::Expired
            | RestoreError::AlreadyUsed
            | RestoreError::NeedsConfirm => fail(format!(
                "backup restore refused: {e} (preview again for a fresh token)"
            )),
            other => fail(format!("backup restore refused: {other}")),
        })?;
    // Safety backup of the live store first (rollback source on failure).
    let safety_path = safety_backup_path(&source, now);
    let mut live_export = disp
        .repo()
        .export_full()
        .map_err(|e| fail(format!("safety backup failed: {}", e.message)))?;
    live_export.sessions = disp.registry().all_sessions_owned();
    let (safety_bytes, _, _) = encode_backup(&live_export, live_generation, now)
        .map_err(|e| fail(format!("safety backup failed: {e}")))?;
    export_backup_to(&safety_path, &safety_bytes)
        .map_err(|e| fail(format!("safety backup failed: {e}")))?;
    // Replace, bump, abandon.
    let new_generation = crate::domain::id::StoreGeneration::new(live_generation + 1);
    let report = restore_verified(disp.repo(), &verified, new_generation)
        .map_err(|e| fail(format!("backup restore failed: {e}")))?;
    let abandoned = disp.registry().abandon_all_sessions(now);
    let quarantined = report
        .quarantined
        .iter()
        .map(|q| format!("{} (missing {})", q.relation, q.missing))
        .collect::<Vec<_>>()
        .join("; ");
    let text = format!(
        "Restored {} memories, {} guides from {}\nGeneration {} active; {} live sessions abandoned; safety backup at {}.{}{}{}",
        report.restored.get("memories").copied().unwrap_or(0),
        report.restored.get("guides").copied().unwrap_or(0),
        source.display(),
        report.generation,
        abandoned,
        safety_path.display(),
        if quarantined.is_empty() {
            String::new()
        } else {
            format!("\nQuarantined (skipped, kept for repair): {quarantined}")
        },
        if report.unknown_top_level == 0 {
            String::new()
        } else {
            format!(
                "\n{} unknown top-level key(s) dropped (counted, not restored).",
                report.unknown_top_level
            )
        },
        if live_writes_since_preview == 0 {
            String::new()
        } else {
            format!(
                "\n{live_writes_since_preview} live write(s) landed after the preview and were replaced (recoverable from the safety backup)."
            )
        },
    );
    Ok(ok_result(
        text,
        serde_json::json!({
            "restored": report.restored,
            "quarantined": report.quarantined,
            "unknown_top_level": report.unknown_top_level,
            "generation": report.generation,
            "safety_backup": safety_path.to_string_lossy(),
            "abandoned_sessions": abandoned,
            "live_writes_since_preview": live_writes_since_preview,
        }),
    ))
}

fn exec_project_analytics(
    disp: &Dispatcher,
    args: &ProjectAnalyticsArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;
    let now = disp.clock().now_millis();
    let export = repo.export_snapshot()?;
    let mut guides = repo.get_guides()?;
    guides.sort_by_key(|g| std::cmp::Reverse(g.usage_count));
    let sessions = disp.registry().all_sessions_owned();
    let memories: Vec<Memory> = export
        .memories
        .iter()
        .filter(|m| m.lifecycle.is_recallable())
        .cloned()
        .collect();

    match &args.project {
        None => {
            let all = crate::compatibility::lemma::intelligence::get_all_projects_analytics(
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
            let progress = crate::compatibility::lemma::intelligence::get_project_analytics(
                project, &sessions, &memories, &guides, now,
            );
            let formatted =
                crate::compatibility::lemma::intelligence::format_project_progress(&progress);
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

    /// Auto-title truncates by characters, never by bytes (multi-byte input
    /// must not panic at the boundary).
    #[test]
    fn generate_title_truncates_by_chars() {
        assert_eq!(generate_title("short"), "short");
        let long_ascii = "a".repeat(41);
        assert_eq!(
            generate_title(&long_ascii),
            format!("{}...", "a".repeat(40))
        );
        // Emoji past the char boundary: byte slicing would panic.
        let emoji = "😀".repeat(41);
        let titled = generate_title(&emoji);
        assert_eq!(titled.chars().count(), 43, "40 chars + ellipsis");
        assert!(titled.ends_with("..."));
        // Byte-boundary only (20 emoji = 20 chars): no truncation, no panic.
        let short_emoji = "😀".repeat(20);
        assert!(short_emoji.len() > 40, "fixture crosses the byte boundary");
        assert_eq!(generate_title(&short_emoji), short_emoji);
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

    /// An attached but empty search backend must not hide canonical
    /// knowledge: browse falls back to the snapshot scan (fresh-E5-start
    /// regression test — an empty index is not a no-answer).
    #[tokio::test]
    async fn memory_read_browse_falls_back_on_empty_backend() {
        use crate::search::backend::{ClosureEmbedder, SearchBackend};
        use crate::search::table::SearchTable;

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let seed = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            Arc::clone(&clock),
        );
        add_fragment(
            &seed,
            1,
            "## Fallback Fragment\n\n### Context\nVisible without an index.",
        );

        // Empty projection table behind a working embedder.
        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let embedder = Arc::new(ClosureEmbedder::new(|_| Ok(vec![0.0; 384])));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(crate::search::backend::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = Dispatcher::new(
            repo,
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        )
        .with_search(backend);
        let args = MemoryReadArgs {
            query: Some("fallback".to_string()),
            all: true,
            ..Default::default()
        };

        // retrieve_sync bridges onto the runtime and must run from a
        // synchronous context, exactly like the dispatcher's spawn_blocking.
        let out = tokio::task::spawn_blocking(move || recall_browse(&disp, &args))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            out.len(),
            1,
            "empty backend must fall back to the snapshot scan"
        );
    }

    /// Fingerprint plumbing: with a backend attached, browse requests run
    /// the dense leg (embedder invoked once) instead of skipping it for a
    /// missing fingerprint. Empty table → fallback results, dense attempted.
    #[tokio::test]
    async fn recall_browse_passes_fingerprint_to_dense_leg() {
        use crate::search::backend::{ClosureEmbedder, SearchBackend};
        use crate::search::table::SearchTable;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let seed = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            Arc::clone(&clock),
        );
        add_fragment(
            &seed,
            1,
            "## Plumbed Fragment\n\n### Context\nDense leg must run.",
        );

        // Empty table behind a counting embedder.
        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let embedder = Arc::new(ClosureEmbedder::new({
            let calls = Arc::clone(&calls);
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![0.0; 384])
            }
        }));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(crate::search::backend::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = Dispatcher::new(
            repo,
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        )
        .with_search(backend);
        let args = MemoryReadArgs {
            query: Some("plumbed".to_string()),
            all: true,
            ..Default::default()
        };

        let out = tokio::task::spawn_blocking(move || recall_browse(&disp, &args))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.len(), 1, "empty table falls back to the snapshot");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "attached backend must run the dense leg"
        );
    }

    /// Without a traced session, memory_add links the fragment to the
    /// channel's virtual session (per-channel upstream parity) instead of
    /// leaving it unlinked.
    #[test]
    fn memory_add_links_virtual_session_without_traced() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Virtual Link\n\n### Context\nUnlinked without virtual sessions.",
        );
        let eid = disp.repo().resolve_id(&id).unwrap();
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        let virtual_handle = disp
            .registry()
            .virtual_session(fe(1), ch(1))
            .expect("virtual session must exist after session-less add");
        assert_eq!(
            mems[0].session_id.as_deref(),
            Some(virtual_handle.as_uuid().to_string()).as_deref(),
            "fragment must link the virtual session"
        );
        let session = disp.registry().session(virtual_handle).unwrap().clone();
        assert!(
            session.is_virtual && session.memories_created.contains(&id),
            "virtual session must track the created memory"
        );
    }

    /// A traced session shadows the virtual one: new fragments link the
    /// traced handle, and the virtual session stays separate.
    #[test]
    fn memory_add_prefers_traced_over_virtual() {
        let (disp, _dir) = test_dispatcher();
        let first = add_fragment(
            &disp,
            1,
            "## Before Traced\n\n### Context\nLinks virtual first.",
        );
        let start = ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec![],
            initial_approach: None,
        });
        run(&disp, &tool_call(2, start.clone()), &start);
        let second = add_fragment(
            &disp,
            3,
            "## After Traced\n\n### Context\nLinks traced now.",
        );
        let traced = disp
            .registry()
            .resolve_session(fe(1), ch(1))
            .expect("traced session must be active");
        let virtual_handle = disp
            .registry()
            .virtual_session(fe(1), ch(1))
            .expect("virtual session persists alongside");
        assert_ne!(traced, virtual_handle);
        let get = |id: &str| {
            let eid = disp.repo().resolve_id(id).unwrap();
            disp.repo().get_memories(&[eid]).unwrap().pop().unwrap()
        };
        assert_eq!(
            get(&first).session_id.as_deref(),
            Some(virtual_handle.as_uuid().to_string()).as_deref()
        );
        assert_eq!(
            get(&second).session_id.as_deref(),
            Some(traced.as_uuid().to_string()).as_deref(),
            "traced session must shadow the virtual one"
        );
    }

    /// Dense preload: with a backend attached, a memory with zero lexical
    /// overlap but a perfect dense match is proposed at session start,
    /// while pure lexical ranking would truncate it away.
    #[tokio::test]
    async fn session_start_preload_uses_dense_when_attached() {
        use crate::embeddings::e5_small::E5_SMALL_FINGERPRINT;
        use crate::search::backend::{ClosureEmbedder, SearchBackend};
        use crate::search::projector::{FixedEmbedder, Projector, render_text};
        use crate::search::table::SearchTable;

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();

        // Three lexically strong memories plus one zero-overlap tail.
        let seed = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            Arc::clone(&clock),
        );
        for (n, title, frag) in [
            (1, "Rust Async", "tokio runtime task spawn"),
            (2, "Rust Errors", "result option unwrap expect"),
            (3, "Rust Tests", "cargo test assert module"),
        ] {
            add_fragment(&seed, n, &format!("## {title}\n\n### Context\n{frag}."));
        }
        let tail_id = add_fragment(
            &seed,
            4,
            "## Tail Memory\n\n### Context\nQuantum bananas orbit pluto.",
        );

        // Project all four with fixed vectors (no FTS index: lexical leg
        // stays empty, dense decides alone).
        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let mut proj = Projector::new(
            Arc::clone(&repo),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            E5_SMALL_FINGERPRINT,
            crate::domain::id::StoreGeneration::FIRST,
        );
        proj.run_until_idle().await.unwrap();

        // Query embedder returns the tail row's exact vector whatever the
        // task text is: dense similarity 1.0 for the tail only.
        use crate::search::projector::Embedder as _;
        let mut fx = FixedEmbedder { dim: 384 };
        let tail_vec = fx
            .embed(&render_text("Tail Memory", "Quantum bananas orbit pluto."))
            .unwrap();
        let embedder = Arc::new(ClosureEmbedder::new(move |_| Ok(tail_vec.clone())));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(crate::search::backend::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = Dispatcher::new(
            repo,
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        )
        .with_search(backend);

        let start = ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec!["rust".to_string()],
            initial_approach: None,
        });
        let env = tool_call(10, start.clone());
        // Same sync-context rule as the dispatcher: bridge from blocking code.
        let result = tokio::task::spawn_blocking(move || run(&disp, &env, &start))
            .await
            .unwrap();
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        let preloaded: Vec<String> = structured["preloaded_memories"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        assert!(
            preloaded.contains(&tail_id),
            "dense perfect match must be proposed, got: {preloaded:?}"
        );
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

    /// Provenance flows end-to-end: a `paper` source stores as Paper and
    /// groups under its own stats bucket (unknown strings still coerce
    /// to `ai`, the documented residual).
    #[test]
    fn memory_stats_groups_expanded_provenance() {
        use crate::compatibility::lemma::tool_args::MemoryAddArgs;

        let (disp, _dir) = test_dispatcher();
        for (op, title, source) in [
            (1u64, "Paper Memory", Some("paper".to_string())),
            (2, "AI Memory", None),
            (
                3,
                "Exotic Memory",
                Some("user-corrected formal review".to_string()),
            ),
        ] {
            let args = ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: format!("## {title}\n\n### Context\nProvenance fixture."),
                title: Some(title.to_string()),
                source,
                ..Default::default()
            });
            let result = run(&disp, &tool_call(op, args.clone()), &args);
            assert!(
                !result_is_error(&result),
                "add failed: {}",
                result_text(&result)
            );
        }
        let env = tool_call(4, ToolArgs::MemoryStats(MemoryStatsArgs::default()));
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryStats(MemoryStatsArgs::default()),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        let by_source = structured["by_source"].as_object().unwrap();
        assert_eq!(by_source["paper"].as_u64().unwrap(), 1);
        // Default (absent) and exotic sources both land in `ai`.
        assert_eq!(by_source["ai"].as_u64().unwrap(), 2);
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

    /// Golden wire pin: the exact stats text on a fixed two-fragment store
    /// (frozen clock, no reads yet). Any rendering drift fails loudly here
    /// instead of slipping into the compatibility surface.
    ///
    /// Recorded outputs: captured from the reviewed implementation on the
    /// fixed fixtures in each test; any rendering change must update these
    /// deliberately, never silently. Regression pins, not upstream
    /// differentials (see the differential harness for memory_read).
    const GOLDEN_STATS_TEXT: &str = "## Memory Stats\nTotal: 2 fragments | Avg confidence: 1\nHigh confidence (>0.8): 2 | Low (<0.3): 0\nSources: ai: 2\nProjects: (global): 2\n";
    const GOLDEN_AUDIT_TEXT: &str =
        "## Memory Audit\nTotal fragments: 1 | Issues: 0\nAll clear — no issues found.\n";

    #[test]
    fn memory_stats_golden_text() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Golden Stats One\n\n### Context\nFirst golden memory.",
        );
        add_fragment(
            &disp,
            2,
            "## Golden Stats Two\n\n### Context\nSecond golden memory.",
        );
        let env = tool_call(3, ToolArgs::MemoryStats(MemoryStatsArgs::default()));
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryStats(MemoryStatsArgs::default()),
        );
        assert!(!result_is_error(&result));
        assert_eq!(result_text(&result), GOLDEN_STATS_TEXT);
    }

    /// Golden wire pin: the exact audit text on a fixed healthy store.
    #[test]
    fn memory_audit_golden_text() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Golden Audit\n\n### Context\nHealthy golden memory.",
        );
        let env = tool_call(2, ToolArgs::MemoryAudit(MemoryAuditArgs::default()));
        let result = run(
            &disp,
            &env,
            &ToolArgs::MemoryAudit(MemoryAuditArgs::default()),
        );
        assert!(!result_is_error(&result));
        assert_eq!(result_text(&result), GOLDEN_AUDIT_TEXT);
    }

    /// Error envelopes: unknown IDs and out-of-range values fail loudly
    /// with actionable messages (never silent success or empty results).
    #[test]
    fn error_envelope_unknown_ids_and_bad_values() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(
            &disp,
            1,
            "## Envelope Fragment\n\n### Context\nFor error testing.",
        );

        // Feedback on an unknown memory.
        let fb = ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
            id: "missing".to_string(),
            useful: true,
        });
        let result = run(&disp, &tool_call(2, fb.clone()), &fb);
        assert!(result_is_error(&result));

        // Forget on an unknown memory.
        let forget = ToolArgs::MemoryForget(MemoryForgetArgs {
            id: "missing".to_string(),
            ..Default::default()
        });
        let result = run(&disp, &tool_call(3, forget.clone()), &forget);
        assert!(result_is_error(&result));

        // Relate to an unknown target.
        let relate = ToolArgs::MemoryRelate(MemoryRelateArgs {
            source_id: id.clone(),
            target_id: "missing".to_string(),
            relation_type: "supports".to_string(),
            note: None,
        });
        let result = run(&disp, &tool_call(4, relate.clone()), &relate);
        assert!(result_is_error(&result));

        // Update with an out-of-range confidence.
        let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id,
            confidence: Some(5.0),
            ..Default::default()
        });
        let result = run(&disp, &tool_call(5, upd.clone()), &upd);
        assert!(result_is_error(&result));
    }

    // ---- semantic_search ----

    /// Semantic fallback: with a backend attached but an empty table, the
    /// dense leg runs and finds nothing, and the lexical snapshot fallback
    /// still answers (mirrors the browse fallback above).
    #[tokio::test]
    async fn semantic_search_falls_back_on_empty_backend() {
        use crate::search::backend::{ClosureEmbedder, SearchBackend};
        use crate::search::table::SearchTable;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let seed = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            Arc::clone(&clock),
        );
        add_fragment(
            &seed,
            1,
            "## Semantic Fallback\n\n### Context\nLexical fallback must answer.",
        );

        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let embedder = Arc::new(ClosureEmbedder::new({
            let calls = Arc::clone(&calls);
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![0.0; 384])
            }
        }));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(crate::search::backend::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = Dispatcher::new(
            repo,
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        )
        .with_search(backend);
        let args = SemanticSearchArgs {
            query: "fallback".to_string(),
            project: None,
            top_k: None,
            offset: None,
            hybrid: None,
            explain: false,
            response_format: None,
        };
        let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
        // Same sync-context rule as the dispatcher: bridge from blocking code.
        let tool = ToolArgs::SemanticSearch(args);
        let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
            .await
            .unwrap();
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert!(
            structured["count"].as_u64().unwrap() >= 1,
            "lexical fallback must answer on an empty backend"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "dense leg must have run before falling back"
        );
    }

    /// hybrid:false is honored as lexical-only: the dense leg never runs,
    /// and lexical results still answer (upstream parity on demand).
    #[tokio::test]
    async fn semantic_search_hybrid_false_skips_dense_leg() {
        use crate::search::backend::{ClosureEmbedder, SearchBackend};
        use crate::search::table::SearchTable;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let seed = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            Arc::clone(&clock),
        );
        add_fragment(
            &seed,
            1,
            "## Lexical Only\n\n### Context\nDense must stay silent.",
        );

        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let embedder = Arc::new(ClosureEmbedder::new({
            let calls = Arc::clone(&calls);
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![0.0; 384])
            }
        }));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(crate::search::backend::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = Dispatcher::new(
            repo,
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        )
        .with_search(backend);
        let args = SemanticSearchArgs {
            query: "lexical silent".to_string(),
            project: None,
            top_k: None,
            offset: None,
            hybrid: Some(false),
            explain: false,
            response_format: None,
        };
        let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
        let tool = ToolArgs::SemanticSearch(args);
        let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
            .await
            .unwrap();
        assert!(!result_is_error(&result));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "hybrid:false must not invoke the embedder"
        );
        let structured = result_structured(&result).unwrap();
        assert!(
            structured["count"].as_u64().unwrap() >= 1,
            "lexical results must answer"
        );
    }

    /// Publish one projection row directly (no worker): lets explain tests
    /// drive the engine instead of falling through to the substring
    /// fallback on an empty table.
    async fn publish_search_row(
        repo: &CanonicalRepository,
        table: &crate::search::table::SearchTable,
        lexical_text: &str,
        fingerprint: crate::domain::id::ModelFingerprint,
    ) {
        use crate::domain::id::{ChunkId, DocumentRevision, StoreGeneration};
        let memory_id = repo.export_snapshot().unwrap().memories[0].id;
        table
            .publish_rows(&[crate::search::row::SearchRow {
                store_generation: StoreGeneration::FIRST,
                memory_id,
                document_revision: DocumentRevision::new(1),
                model_fingerprint: fingerprint,
                chunk_id: ChunkId::new(0),
                chunker_version: "v1".to_string(),
                lexical_text: lexical_text.to_string(),
                char_start: 0,
                char_end: lexical_text.len() as u64,
                project: None,
                fragment_type: "fact".to_string(),
                created_at_millis: 1000,
                confidence: 0.9,
                updated_at_millis: 1000,
                embedding: Some(vec![0.0; 384]),
            }])
            .await
            .unwrap();
    }

    /// explain:true reports how the answer was produced: engine readiness
    /// when the backend ran, fallback mode otherwise.
    #[tokio::test]
    async fn semantic_search_explain_reports_engine_explanation() {
        use crate::search::backend::{ClosureEmbedder, SearchBackend};
        use crate::search::table::SearchTable;

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let seed = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            Arc::clone(&clock),
        );
        add_fragment(
            &seed,
            1,
            "## Explained Search\n\n### Context\nRecall with reasons.",
        );

        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        // Publish one row so the engine (not the fallback) answers.
        publish_search_row(
            &repo,
            &table,
            "explained reasons recall",
            crate::embeddings::e5_small::E5_SMALL_FINGERPRINT,
        )
        .await;
        table.create_fts_index().await.unwrap();
        let embedder = Arc::new(ClosureEmbedder::new(|_| Ok(vec![0.0; 384])));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(crate::search::backend::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = Dispatcher::new(
            repo,
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        )
        .with_search(backend);
        let args = SemanticSearchArgs {
            query: "explained reasons".to_string(),
            project: None,
            top_k: None,
            offset: None,
            hybrid: None,
            explain: true,
            response_format: None,
        };
        let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
        let tool = ToolArgs::SemanticSearch(args);
        let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
            .await
            .unwrap();
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert_eq!(
            structured["explanation"]["mode"], "hybrid",
            "backend path must report hybrid mode"
        );
        assert!(
            structured["explanation"]["dense_ready"].is_boolean(),
            "readiness must be reported"
        );
    }

    /// explain:true with hybrid:false reports the lexical mode actually
    /// run, not hybrid: the mode names the effective legs, and the
    /// no-backend fallback does not claim a ready FTS index it never used.
    #[tokio::test]
    async fn semantic_search_explain_reports_effective_mode() {
        use crate::search::backend::{ClosureEmbedder, SearchBackend};
        use crate::search::table::SearchTable;

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let seed = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            Arc::clone(&clock),
        );
        add_fragment(
            &seed,
            1,
            "## Explained Search\n\n### Context\nRecall with reasons.",
        );

        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        // Publish one row so the engine serves lexically (hybrid:false
        // skips the dense leg): the mode must name the effective legs.
        publish_search_row(
            &repo,
            &table,
            "explained reasons recall",
            crate::embeddings::e5_small::E5_SMALL_FINGERPRINT,
        )
        .await;
        table.create_fts_index().await.unwrap();
        let embedder = Arc::new(ClosureEmbedder::new(|_| Ok(vec![0.0; 384])));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(crate::search::backend::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = Dispatcher::new(
            repo,
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        )
        .with_search(backend);
        let args = SemanticSearchArgs {
            query: "explained reasons".to_string(),
            project: None,
            top_k: None,
            offset: None,
            hybrid: Some(false),
            explain: true,
            response_format: None,
        };
        let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
        let tool = ToolArgs::SemanticSearch(args);
        let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
            .await
            .unwrap();
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert_eq!(
            structured["explanation"]["mode"], "lexical",
            "hybrid:false must report the lexical mode actually run"
        );
    }

    /// explain:true with an empty engine result served by the substring
    /// fallback reports the fallback mode (never a hybrid that produced
    /// nothing) and never claims FTS readiness for a scan that used no
    /// index.
    #[tokio::test]
    async fn semantic_search_explain_reports_fallback_mode() {
        use crate::search::backend::{ClosureEmbedder, SearchBackend};
        use crate::search::table::SearchTable;

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
                .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let seed = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            Arc::clone(&clock),
        );
        add_fragment(
            &seed,
            1,
            "## Explained Search\n\n### Context\nRecall with reasons.",
        );

        // Empty table: the engine finds nothing, the substring fallback
        // serves from the canonical snapshot.
        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let embedder = Arc::new(ClosureEmbedder::new(|_| Ok(vec![0.0; 384])));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(crate::search::backend::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = Dispatcher::new(
            repo,
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        )
        .with_search(backend);
        let args = SemanticSearchArgs {
            query: "explained reasons".to_string(),
            project: None,
            top_k: None,
            offset: None,
            hybrid: None,
            explain: true,
            response_format: None,
        };
        let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
        let tool = ToolArgs::SemanticSearch(args);
        let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
            .await
            .unwrap();
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert!(
            structured["count"].as_u64().unwrap() >= 1,
            "fallback must serve the snapshot row"
        );
        assert_eq!(
            structured["explanation"]["mode"], "lexical-fallback",
            "fallback-served rows must not report hybrid"
        );
        assert_eq!(
            structured["explanation"]["fts_ready"], false,
            "substring scan must not claim FTS readiness"
        );
        assert_eq!(
            structured["explanation"]["candidates"], structured["total"],
            "fallback candidates must match the examined pool"
        );
    }

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
            hybrid: None,
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
            hybrid: None,
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

    /// Normalize a replayed text the same way the fixture was normalized:
    /// m-hex ids -> $Mn (appearance order in `ids`), UUIDs and upstream
    /// session ids -> $SID, datetimes -> $TS, projects -> $PROJ shape.
    fn wf_normalize(text: &str, ids: &mut Vec<String>) -> String {
        let mid = regex::Regex::new(r"\bm[0-9a-f]{12}\b").unwrap();
        let mut out = mid
            .replace_all(text, |caps: &regex::Captures| {
                let hit = caps[0].to_string();
                let pos = match ids.iter().position(|id| *id == hit) {
                    Some(i) => i + 1,
                    None => {
                        ids.push(hit);
                        ids.len()
                    }
                };
                format!("$M{pos}")
            })
            .into_owned();
        let uuid =
            regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
                .unwrap();
        out = uuid.replace_all(&out, "$$SID").into_owned();
        let sess = regex::Regex::new(r"\bs[a-z][0-9a-f]{11}\b").unwrap();
        out = sess.replace_all(&out, "$$SID").into_owned();
        // Any 4-digit year: replay runs under a frozen test clock (1970).
        let ts = regex::Regex::new(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?").unwrap();
        out = ts.replace_all(&out, "$$TS").into_owned();
        out = out.replace("upstream-lemma", "$PROJ");
        // Project attribution differs by design (cwd-derived vs explicit):
        // compare the segment shape, not the project name.
        out = out.replace("(global)", "(project: $PROJ)");
        out
    }

    fn wf_normalize_value(value: &Value, ids: &mut Vec<String>) -> Value {
        match value {
            Value::String(s) => Value::String(wf_normalize(s, ids)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| wf_normalize_value(v, ids)).collect())
            }
            Value::Object(map) => Value::Object(
                // Keys pass through untouched: the text-only project-shape
                // rule would otherwise rewrite aggregation buckets like
                // `(global)` (the fixture keeps them verbatim too).
                map.iter()
                    .map(|(k, v)| (k.clone(), wf_normalize_value(v, ids)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    /// Differential workflow replay (WP-09 remaining task): the 8-step
    /// recall -> act -> persist script from workflow_fixture.json (captured
    /// from pinned upstream lemma 0.21.0) replayed through the public tool
    /// handlers. Exact text+shape where parity is claimed; structural
    /// sets/deltas with declared divergences everywhere else:
    /// - upstream ships 4 seed fragments (counts, preload, read hits);
    /// - upstream appends coaching blocks (`**[Lemma] ...**`) to session
    ///   lifecycle and stats texts;
    /// - memory_read relevance order differs (sets compared, not order);
    /// - upstream auto-detects technologies and suggests distill on end;
    /// - project attribution is cwd-derived upstream, explicit here.
    #[test]
    fn differential_workflow_replay_matches_upstream() {
        let raw = std::fs::read_to_string(fixture_path("workflow_fixture.json"))
            .expect("workflow_fixture.json must exist");
        let fixture: Value = serde_json::from_str(&raw).expect("valid fixture");
        let steps = fixture["steps"].as_array().expect("steps array");
        assert_eq!(steps.len(), 8, "fixture must hold the 8-step workflow");
        let seed_count = fixture["provenance"]["upstream_seed_fragments"]
            .as_u64()
            .expect("seed count") as usize;

        let (disp, _dir) = test_dispatcher();
        let mut op = 1u64;
        let mut run_tool = |args: ToolArgs| -> DomainPayload {
            let env = tool_call(op, args.clone());
            op += 1;
            run(&disp, &env, &args)
        };
        // Replay the fixture args verbatim (same calls both sides).
        let mut replayed: Vec<(String, String, Value)> = Vec::new();
        for step in steps {
            let tool = step["tool"].as_str().unwrap();
            // Arguments come from the fixture (same calls both sides);
            // shapes absent from the capture stay None/defaulted.
            let str_vec = |key: &str| -> Vec<String> {
                step["args"][key]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let req_str = |key: &str| -> String {
                step["args"][key]
                    .as_str()
                    .unwrap_or_else(|| panic!("fixture args lack {key}"))
                    .to_string()
            };
            let args = match tool {
                "session_start" => ToolArgs::SessionStart(SessionStartArgs {
                    task_type: req_str("task_type"),
                    technologies: str_vec("technologies"),
                    initial_approach: None,
                }),
                "memory_add" => ToolArgs::MemoryAdd(MemoryAddArgs {
                    fragment: req_str("fragment"),
                    ..Default::default()
                }),
                "memory_read" => ToolArgs::MemoryRead(MemoryReadArgs {
                    query: Some(req_str("query")),
                    ..Default::default()
                }),
                "guide_create" => ToolArgs::GuideCreate(GuideCreateArgs {
                    guide: req_str("guide"),
                    category: req_str("category"),
                    description: req_str("description"),
                    // Absent from the captured call: empty on both sides.
                    contexts: Vec::new(),
                    learnings: Vec::new(),
                }),
                "guide_practice" => ToolArgs::GuidePractice(GuidePracticeArgs {
                    guide: req_str("guide"),
                    category: req_str("category"),
                    contexts: str_vec("contexts"),
                    learnings: str_vec("learnings"),
                    ..Default::default()
                }),
                "session_end" => ToolArgs::SessionEnd(SessionEndArgs {
                    outcome: req_str("outcome"),
                    ..Default::default()
                }),
                "memory_stats" => ToolArgs::MemoryStats(MemoryStatsArgs {
                    ..Default::default()
                }),
                other => panic!("fixture holds an unexpected tool: {other}"),
            };
            let result = run_tool(args);
            assert!(
                !result_is_error(&result),
                "{tool} must succeed in replay: {}",
                result_text(&result)
            );
            let structured = result_structured(&result).unwrap_or(Value::Null);
            replayed.push((tool.to_string(), result_text(&result), structured));
        }

        // Normalize our side globally (same rules as the fixture).
        let mut ids: Vec<String> = Vec::new();
        let ours: Vec<(String, String, Value)> = replayed
            .into_iter()
            .map(|(tool, text, structured)| {
                (
                    tool,
                    wf_normalize(&text, &mut ids),
                    wf_normalize_value(&structured, &mut ids),
                )
            })
            .collect();
        assert_eq!(
            ids.len(),
            2,
            "exactly the two added fragments take $M ids, got: {ids:?}"
        );

        // Step 0 session_start: same session line, techs and new guides;
        // seeds/coaching/tracked-debugging are declared divergences.
        assert!(ours[0].1.contains("Session started: $SID (research)"));
        assert!(ours[0].1.contains("Technologies: rust"));
        assert_eq!(ours[0].2["session_id"], Value::String("$SID".to_string()));
        assert_eq!(
            ours[0].2["guides"],
            serde_json::json!(["elasticsearch", "rust"])
        );
        let fx_guides: Vec<String> = steps[0]["structured"]["guides"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .filter(|g| g != "debugging")
            .collect();
        assert_eq!(
            fx_guides,
            vec!["elasticsearch".to_string(), "rust".to_string()],
            "upstream new-guide suggestions match ours (tracked debugging excluded)"
        );
        assert_eq!(ours[0].2["preloaded_memories"], Value::Array(vec![]));
        assert_eq!(
            steps[0]["structured"]["preloaded_memories"][0],
            Value::String("$SEED".to_string())
        );

        // Steps 1-2 memory_add: exact normalized parity (text + shape).
        for i in [1usize, 2] {
            assert_eq!(
                ours[i].1,
                steps[i]["text"].as_str().unwrap(),
                "add text parity"
            );
            assert_eq!(ours[i].2, steps[i]["structured"], "add shape parity");
        }

        // Step 3 memory_read: same added-fragment set (order differs by
        // design); upstream additionally hits one seed.
        let ours_ids: std::collections::BTreeSet<String> = ours[3].2["fragments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            ours_ids,
            std::collections::BTreeSet::from(["$M1".to_string(), "$M2".to_string()])
        );
        let fx_ids: std::collections::BTreeSet<String> = steps[3]["structured"]["fragments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            fx_ids,
            std::collections::BTreeSet::from([
                "$M1".to_string(),
                "$M2".to_string(),
                "$SEED".to_string()
            ])
        );
        assert_eq!(ours[3].2["count"], serde_json::json!(2));
        assert_eq!(steps[3]["structured"]["count"], serde_json::json!(3));

        // Steps 4-5 guide_create/practice: exact normalized parity.
        for i in [4usize, 5] {
            assert_eq!(
                ours[i].1,
                steps[i]["text"].as_str().unwrap(),
                "guide text parity"
            );
            assert_eq!(ours[i].2, steps[i]["structured"], "guide shape parity");
        }

        // Step 6 session_end: ours equals the upstream text minus the
        // coaching tail; same memories/guides attribution.
        let fx_end = steps[6]["text"].as_str().unwrap();
        let (fx_head, _) = fx_end
            .split_once("\nAuto-detected technologies:")
            .expect("upstream end carries auto-detected techs");
        assert_eq!(ours[6].1, fx_head, "end text parity before coaching tail");
        assert_eq!(ours[6].2["outcome_recorded"], Value::Bool(true));
        assert_eq!(ours[6].2["suggestions"], Value::Array(vec![]));
        assert!(
            steps[6]["structured"]["suggestions"]
                .as_array()
                .unwrap()
                .len()
                == 1,
            "upstream suggests distill on end (declared divergence)"
        );

        // Step 7 memory_stats: totals differ by exactly the seed count.
        let fx_total = steps[7]["structured"]["total"].as_u64().unwrap() as usize;
        let ours_total = ours[7].2["total"].as_u64().unwrap() as usize;
        assert_eq!(fx_total - ours_total, seed_count);
        assert_eq!(ours_total, 2, "our two adds, no seeds");
        assert_eq!(ours[7].2["by_source"]["ai"], serde_json::json!(2));
        assert_eq!(ours[7].2["by_project"]["(global)"], serde_json::json!(2));
        assert_eq!(ours[7].2["avg_confidence"], serde_json::json!(1.0));
        assert!(ours[7].1.contains("Total: 2 fragments"));
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
            child_ids: vec![],
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

    // ---- WP-09: guide tools ----

    /// Biased passage double: task text maps to TASK_VEC; guide catalog
    /// texts map by guide-name substring (beta ~= task, everything else
    /// orthogonal). Query role always returns the task vector.
    struct BiasedGuideEmbedder {
        fail_passages: bool,
    }

    impl crate::retrieval::engine::QueryEmbedder for BiasedGuideEmbedder {
        fn embed_query<'a>(
            &'a self,
            _query: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = crate::domain::command::DomainResult<Vec<f32>>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(vec![1.0f32, 0.0, 0.0]) })
        }

        fn embed_passages<'a>(
            &'a self,
            texts: &'a [String],
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = crate::domain::command::DomainResult<Vec<Vec<f32>>>,
                    > + Send
                    + 'a,
            >,
        > {
            let fail = self.fail_passages;
            Box::pin(async move {
                if fail {
                    return Err(crate::domain::command::DomainError::new(
                        crate::domain::command::DomainErrorCode::Validation,
                        "boom".to_string(),
                    ));
                }
                Ok(texts
                    .iter()
                    .map(|t| {
                        if t.to_lowercase().contains("beta") {
                            vec![1.0f32, 0.0, 0.0]
                        } else {
                            vec![0.0f32, 1.0, 0.0]
                        }
                    })
                    .collect())
            })
        }
    }

    fn dense_alpha_guide() -> Guide {
        Guide {
            name: "alpha".into(),
            category: "test".into(),
            description: "Alpha rendering protocols".into(),
            contexts: vec!["alpha pixels".into()],
            learnings: vec!["alpha compositing".into()],
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
            entity_revision: crate::domain::id::EntityRevision::new(1),
            created_at: crate::domain::memory::Instant::new(0),
            updated_at: crate::domain::memory::Instant::new(0),
        }
    }

    fn dense_beta_guide() -> Guide {
        let mut g = dense_alpha_guide();
        g.name = "beta".into();
        g.description = "Beta estimation protocols".into();
        g.contexts = vec!["covariance matrices".into()];
        g.learnings = vec!["kalman gain tuning".into()];
        g
    }

    async fn dense_test_backend(
        fail_passages: bool,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        crate::search::backend::SearchBackend,
    ) {
        let store_dir = tempfile::tempdir().unwrap();
        let repo = std::sync::Arc::new(
            crate::service::repository::CanonicalRepository::open(
                store_dir.path().join("store").to_str().unwrap(),
            )
            .unwrap(),
        );
        let lance_dir = tempfile::tempdir().unwrap();
        let table = crate::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let backend = crate::search::backend::SearchBackend::new(
            repo,
            table,
            std::sync::Arc::new(BiasedGuideEmbedder { fail_passages }),
        );
        (store_dir, lance_dir, backend)
    }

    /// Dense leg appends the token-missed guide (beta ~= task) while
    /// leaving the already-suggested alpha out.
    #[tokio::test]
    async fn dense_guide_leg_appends_token_missed_guide() {
        let (_s, _l, backend) = dense_test_backend(false).await;
        let guides = vec![dense_alpha_guide(), dense_beta_guide()];
        let seen = std::collections::BTreeSet::from(["alpha".to_string()]);
        let out = tokio::task::spawn_blocking(move || {
            suggest_guides_dense(&backend, "alpha zonkblat", &guides, &seen)
        })
        .await
        .unwrap();
        assert_eq!(out.len(), 1, "only beta is dense-new, got: {out:?}");
        assert_eq!(out[0].guide, "beta");
        assert!(out[0].tracked, "dense additions come from the catalog");
    }

    /// A blank task proposes nothing dense (no noise vectors).
    #[tokio::test]
    async fn dense_guide_leg_ignores_blank_task() {
        let (_s, _l, backend) = dense_test_backend(false).await;
        let guides = vec![dense_beta_guide()];
        let seen = std::collections::BTreeSet::new();
        let out = tokio::task::spawn_blocking(move || {
            suggest_guides_dense(&backend, "   ", &guides, &seen)
        })
        .await
        .unwrap();
        assert!(out.is_empty(), "blank task must stay token-only");
    }

    /// Embedding failure degrades to no dense candidates (the caller
    /// keeps the token-only suggestions byte-identically).
    #[tokio::test]
    async fn dense_guide_errors_fall_back_silently() {
        let (_s, _l, backend) = dense_test_backend(true).await;
        let guides = vec![dense_beta_guide()];
        let seen = std::collections::BTreeSet::new();
        let out = tokio::task::spawn_blocking(move || {
            suggest_guides_dense(&backend, "alpha zonkblat", &guides, &seen)
        })
        .await
        .unwrap();
        assert!(out.is_empty(), "failed dense leg must add nothing");
    }

    /// Build a dispatcher with the biased dense backend attached.
    async fn dense_wiring_dispatcher() -> (tempfile::TempDir, tempfile::TempDir, Dispatcher) {
        let dir = tempfile::tempdir().unwrap();
        let clock: std::sync::Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            std::sync::Arc::new(FrozenClock::new(1000));
        let repo = std::sync::Arc::new(
            crate::service::repository::CanonicalRepository::open_with_clock(
                dir.path().to_str().unwrap(),
                std::sync::Arc::clone(&clock),
            )
            .unwrap(),
        );
        let disp = Dispatcher::new(
            std::sync::Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            std::sync::Arc::clone(&clock),
        );
        let lance_dir = tempfile::tempdir().unwrap();
        let table = crate::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let backend = std::sync::Arc::new(crate::search::backend::SearchBackend::new(
            std::sync::Arc::clone(&repo),
            table,
            std::sync::Arc::new(BiasedGuideEmbedder {
                fail_passages: false,
            }),
        ));
        let disp = disp.with_search(backend);
        (dir, lance_dir, disp)
    }

    fn create_alpha_beta(disp: &Dispatcher) {
        for (op, name, desc, ctx, learn) in [
            (
                1u64,
                "alpha",
                "Alpha rendering protocols",
                "alpha pixels",
                "alpha compositing",
            ),
            (
                2u64,
                "beta",
                "Beta estimation protocols",
                "covariance matrices",
                "kalman gain tuning",
            ),
        ] {
            let args = ToolArgs::GuideCreate(GuideCreateArgs {
                guide: name.to_string(),
                category: "test".to_string(),
                description: desc.to_string(),
                contexts: vec![ctx.to_string()],
                learnings: vec![learn.to_string()],
            });
            let env = tool_call(op, args.clone());
            let result = run(disp, &env, &args);
            assert!(!result_is_error(&result), "guide {name} must create");
        }
    }

    fn suggest_task_text(disp: &Dispatcher) -> String {
        let args = ToolArgs::GuideGet(GuideGetArgs {
            task: Some("alpha zonkblat".to_string()),
            ..Default::default()
        });
        let env = tool_call(9, args.clone());
        let result = run(disp, &env, &args);
        assert!(!result_is_error(&result));
        result_text(&result)
    }

    /// End to end: token path finds alpha, dense leg appends beta after it.
    #[tokio::test]
    async fn dense_guide_wiring_appends_after_token() {
        let (_d, _l, disp) = dense_wiring_dispatcher().await;
        // run() bridges onto the runtime (block_on), so the whole flow must
        // execute off the async worker like the recall_browse tests.
        let text = tokio::task::spawn_blocking(move || {
            create_alpha_beta(&disp);
            suggest_task_text(&disp)
        })
        .await
        .unwrap();
        let alpha = text.find("alpha").expect("alpha must be suggested");
        let beta = text.find("beta").expect("beta must be dense-suggested");
        assert!(
            alpha < beta,
            "token match first, dense addition after:\n{text}"
        );
    }

    /// Without a backend the same catalog stays token-only (beta absent):
    /// the dense leg changes nothing when unavailable.
    #[tokio::test]
    async fn token_only_without_backend() {
        let (disp, _dir) = test_dispatcher();
        create_alpha_beta(&disp);
        let text = suggest_task_text(&disp);
        assert!(text.contains("alpha"), "alpha must be suggested:\n{text}");
        assert!(
            !text.contains("beta"),
            "beta must stay absent without dense:\n{text}"
        );
    }

    /// backup_create backs up through the tool surface and verifies the
    /// archive; a missing directory fails explicitly (never invented).
    /// backup_preview reports readiness with a token; missing path fails.
    #[test]
    fn backup_preview_reports_ready_with_token() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(&disp, 1, "## Preview Me\n\n### Context\nPreview fixture.");
        let out = tempfile::tempdir().unwrap();
        let create = ToolArgs::BackupCreate(BackupCreateArgs {
            directory: Some(out.path().to_str().unwrap().to_string()),
        });
        let result = run(&disp, &tool_call(2, create.clone()), &create);
        assert!(!result_is_error(&result));
        let path = result_structured(&result).unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
            path: Some(path.clone()),
        });
        let result = run(&disp, &tool_call(3, preview.clone()), &preview);
        assert!(!result_is_error(&result));
        let text = result_text(&result);
        assert!(text.contains("READY"), "got: {text}");
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["readiness"]["status"], "ready");
        let token = structured["confirmation_token"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!token.is_empty());
        assert!(structured["expires_at"].as_u64().unwrap() > 0);

        // Missing path fails explicitly.
        let missing = ToolArgs::BackupPreview(BackupPreviewArgs { path: None });
        let result = run(&disp, &tool_call(4, missing.clone()), &missing);
        assert!(result_is_error(&result));
    }

    /// Dead channels must not block restore: after two sessions bound two
    /// channels (prior runs leave persisted bindings behind), preview with
    /// no LIVE connection still reports READY — readiness counts live
    /// connections, not registry history.
    #[test]
    fn backup_preview_ignores_dead_channels() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(&disp, 1, "## Preview Me\n\n### Context\nPreview fixture.");
        for (n, op) in [(1u64, 10u64), (2, 11)] {
            let start = ToolArgs::SessionStart(SessionStartArgs {
                task_type: "debugging".to_string(),
                technologies: vec![],
                initial_approach: None,
            });
            let env = IpcEnvelope {
                frontend_id: fe(n),
                channel_id: ch(n),
                operation_id: OperationId::new(Uuid::from_u128(op as u128)),
                ..tool_call(op, start.clone())
            };
            run(&disp, &env, &start);
        }
        assert_eq!(disp.registry().channel_count(), 2);
        let out = tempfile::tempdir().unwrap();
        let create = ToolArgs::BackupCreate(BackupCreateArgs {
            directory: Some(out.path().to_str().unwrap().to_string()),
        });
        let result = run(&disp, &tool_call(2, create.clone()), &create);
        assert!(!result_is_error(&result));
        let path = result_structured(&result).unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
            path: Some(path.clone()),
        });
        let result = run(&disp, &tool_call(3, preview.clone()), &preview);
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert_eq!(
            structured["readiness"]["status"], "ready",
            "dead channels must not block restore, got: {structured:?}"
        );
        assert!(
            structured["confirmation_token"].as_str().is_some(),
            "ready preview must issue a token"
        );
    }

    /// Loss accounting through the tool surface: an evolved backup carrying
    /// a future top-level snapshot key reports the unknown count at preview
    /// (before the destructive step) and again in the restore report.
    #[test]
    fn backup_preview_and_restore_report_unknown_keys() {
        use crate::domain::export::CanonicalExport;
        let (disp, _dir) = test_dispatcher();
        add_fragment(&disp, 1, "## Evolve Me\n\n### Context\nLoss fixture.");
        let out = tempfile::tempdir().unwrap();
        let create = ToolArgs::BackupCreate(BackupCreateArgs {
            directory: Some(out.path().to_str().unwrap().to_string()),
        });
        let result = run(&disp, &tool_call(2, create.clone()), &create);
        assert!(!result_is_error(&result));
        let path = result_structured(&result).unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        // Future-producer simulation: extra snapshot key, manifest re-signed.
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["snapshot"]["future_collection"] = serde_json::json!([{"kept": true}]);
        let evolved_snap: CanonicalExport = serde_json::from_value(v["snapshot"].clone()).unwrap();
        v["manifest"]["digest"] = serde_json::Value::String(evolved_snap.digest());
        let evolved = out.path().join("evolved.ltmrs-backup");
        std::fs::write(&evolved, serde_json::to_vec(&v).unwrap()).unwrap();

        let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
            path: Some(evolved.to_str().unwrap().to_string()),
        });
        let result = run(&disp, &tool_call(3, preview.clone()), &preview);
        assert!(
            !result_is_error(&result),
            "preview failed: {}",
            result_text(&result)
        );
        let text = result_text(&result);
        assert!(text.contains("1 unknown"), "got: {text}");
        let structured = result_structured(&result).unwrap();
        assert_eq!(
            structured["unknown_top_level"].as_u64(),
            Some(1),
            "preview must surface the count"
        );
        let token = structured["confirmation_token"]
            .as_str()
            .unwrap()
            .to_string();

        let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
            confirmation_token: Some(token),
            confirm: Some(true),
        });
        let result = run(&disp, &tool_call(4, restore.clone()), &restore);
        assert!(
            !result_is_error(&result),
            "restore failed: {}",
            result_text(&result)
        );
        let text = result_text(&result);
        assert!(text.contains("1 unknown"), "got: {text}");
        assert_eq!(
            result_structured(&result).unwrap()["unknown_top_level"].as_u64(),
            Some(1),
            "restore report must surface the count"
        );
    }

    /// Full restore cycle with rollback through the safety file: alpha live,
    /// backup alpha, add beta, restore (beta gone), restore safety (beta back).
    /// Generation advances on every restore; sessions are abandoned.
    #[test]
    fn backup_restore_end_to_end_with_rollback() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(
            &disp,
            1,
            "## Restore Alpha\n\n### Context\nPre-restore content.",
        );
        let out = tempfile::tempdir().unwrap();
        let create = ToolArgs::BackupCreate(BackupCreateArgs {
            directory: Some(out.path().to_str().unwrap().to_string()),
        });
        let result = run(&disp, &tool_call(2, create.clone()), &create);
        let backup_path = result_structured(&result).unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        let gen_before = disp.repo().store_generation().unwrap().as_u64();
        add_fragment(
            &disp,
            3,
            "## Restore Beta\n\n### Context\nPost-backup content.",
        );

        // Preview + restore the backup (beta disappears).
        let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
            path: Some(backup_path.clone()),
        });
        let result = run(&disp, &tool_call(4, preview.clone()), &preview);
        let token = result_structured(&result).unwrap()["confirmation_token"]
            .as_str()
            .unwrap()
            .to_string();
        // A write lands between preview and confirm (same channel): the
        // replace drains it, so the report must acknowledge the delta
        // instead of dropping it silently.
        add_fragment(
            &disp,
            6,
            "## Restore Gamma\n\n### Context\nMid-window content.",
        );
        let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
            confirmation_token: Some(token),
            confirm: Some(true),
        });
        let result = run(&disp, &tool_call(5, restore.clone()), &restore);
        assert!(
            !result_is_error(&result),
            "restore failed: {}",
            result_text(&result)
        );
        let text = result_text(&result);
        assert!(text.contains("Restored 1 memories"), "got: {text}");
        assert!(text.contains("safety backup at"), "got: {text}");
        let structured = result_structured(&result).unwrap();
        // Gamma's add plus its topical auto-link both executed mid-window:
        // every executed write counts, each would have been drained.
        assert!(
            structured["live_writes_since_preview"].as_u64().unwrap() >= 1,
            "mid-window writes must be acknowledged, got: {structured:?}"
        );
        assert!(
            text.contains("live write(s) landed after the preview"),
            "report must name the delta, got: {text}"
        );
        let safety = structured["safety_backup"].as_str().unwrap().to_string();
        assert!(
            std::path::Path::new(&safety).exists(),
            "safety file published"
        );
        assert_eq!(
            disp.repo().store_generation().unwrap().as_u64(),
            gen_before + 1
        );
        let titles: Vec<String> = disp
            .repo()
            .export_full()
            .unwrap()
            .memories
            .iter()
            .map(|m| m.title.clone())
            .collect();
        assert!(
            !titles.iter().any(|t| t.contains("Beta")),
            "got: {titles:?}"
        );

        // Rollback: preview + restore the safety file (beta returns).
        let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
            path: Some(safety.clone()),
        });
        let result = run(&disp, &tool_call(6, preview.clone()), &preview);
        assert!(!result_is_error(&result));
        let token = result_structured(&result).unwrap()["confirmation_token"]
            .as_str()
            .unwrap()
            .to_string();
        let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
            confirmation_token: Some(token),
            confirm: Some(true),
        });
        let result = run(&disp, &tool_call(7, restore.clone()), &restore);
        assert!(
            !result_is_error(&result),
            "rollback failed: {}",
            result_text(&result)
        );
        assert_eq!(
            disp.repo().store_generation().unwrap().as_u64(),
            gen_before + 2
        );
        let titles: Vec<String> = disp
            .repo()
            .export_full()
            .unwrap()
            .memories
            .iter()
            .map(|m| m.title.clone())
            .collect();
        assert!(titles.iter().any(|t| t.contains("Beta")), "got: {titles:?}");
    }

    /// Restore demands an unused token plus explicit confirmation; a
    /// refused confirm does not burn the token.
    #[test]
    fn backup_restore_requires_confirmation() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(&disp, 1, "## Confirm Me\n\n### Context\nConfirm fixture.");
        let out = tempfile::tempdir().unwrap();
        let create = ToolArgs::BackupCreate(BackupCreateArgs {
            directory: Some(out.path().to_str().unwrap().to_string()),
        });
        let result = run(&disp, &tool_call(2, create.clone()), &create);
        let path = result_structured(&result).unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        let preview = ToolArgs::BackupPreview(BackupPreviewArgs { path: Some(path) });
        let result = run(&disp, &tool_call(3, preview.clone()), &preview);
        let token = result_structured(&result).unwrap()["confirmation_token"]
            .as_str()
            .unwrap()
            .to_string();

        // Unknown token rejected.
        let bad = ToolArgs::BackupRestore(BackupRestoreArgs {
            confirmation_token: Some("nope".to_string()),
            confirm: Some(true),
        });
        let result = run(&disp, &tool_call(4, bad.clone()), &bad);
        assert!(result_is_error(&result));

        // Missing confirmation explains REPLACE without consuming the token.
        let unconfirmed = ToolArgs::BackupRestore(BackupRestoreArgs {
            confirmation_token: Some(token.clone()),
            confirm: None,
        });
        let result = run(&disp, &tool_call(5, unconfirmed.clone()), &unconfirmed);
        assert!(result_is_error(&result));
        assert!(result_text(&result).contains("confirm=true"));

        // The same token still works after the refused confirm.
        let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
            confirmation_token: Some(token),
            confirm: Some(true),
        });
        let result = run(&disp, &tool_call(6, restore.clone()), &restore);
        assert!(!result_is_error(&result), "got: {}", result_text(&result));
    }

    #[test]
    fn backup_create_tool_backs_up_and_verifies() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(&disp, 1, "## Backup One\n\n### Context\nFirst.");
        add_fragment(&disp, 2, "## Backup Two\n\n### Context\nSecond.");
        let out = tempfile::tempdir().unwrap();
        let args = ToolArgs::BackupCreate(BackupCreateArgs {
            directory: Some(out.path().to_str().unwrap().to_string()),
        });
        let env = tool_call(3, args.clone());
        let result = run(&disp, &env, &args);
        assert!(!result_is_error(&result));
        let text = result_text(&result);
        assert!(text.contains("Backed up 2 memories"), "got: {text}");
        assert!(text.contains("Digest: "), "got: {text}");
        let structured = result_structured(&result).unwrap();
        let path = structured["path"].as_str().unwrap().to_string();
        assert!(path.ends_with(".ltmrs-backup"), "got: {path}");
        assert!(std::path::Path::new(&path).exists());
        // Re-verify the produced file through the library boundary.
        let verified = crate::interchange::backup::verify_backup_file(
            std::path::Path::new(&path),
            crate::interchange::backup::MAX_BACKUP_BYTES,
        )
        .unwrap();
        assert_eq!(verified.counts["memories"], 2);

        // Missing directory fails explicitly.
        let missing = ToolArgs::BackupCreate(BackupCreateArgs { directory: None });
        let env = tool_call(4, missing.clone());
        let result = run(&disp, &env, &missing);
        assert!(result_is_error(&result));
        assert!(
            result_text(&result).contains("requires"),
            "got: {}",
            result_text(&result)
        );
    }

    #[test]
    fn guide_create_then_get_roundtrip() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::GuideCreate(GuideCreateArgs {
                guide: "react".to_string(),
                category: "web-frontend".to_string(),
                description: "## React Guide\n\n### Protocol\nUse hooks.".to_string(),
                contexts: vec!["hooks".to_string()],
                learnings: vec!["useCallback prevents re-renders".to_string()],
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::GuideCreate(GuideCreateArgs {
                guide: "react".to_string(),
                category: "web-frontend".to_string(),
                description: "## React Guide\n\n### Protocol\nUse hooks.".to_string(),
                contexts: vec!["hooks".to_string()],
                learnings: vec!["useCallback prevents re-renders".to_string()],
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Created new guide \"react\""));

        // Fetch it back.
        let env2 = tool_call(
            2,
            ToolArgs::GuideGet(GuideGetArgs {
                guide: Some("react".to_string()),
                ..Default::default()
            }),
        );
        let result2 = run(
            &disp,
            &env2,
            &ToolArgs::GuideGet(GuideGetArgs {
                guide: Some("react".to_string()),
                ..Default::default()
            }),
        );
        let text2 = result_text(&result2);
        assert!(text2.contains("=== GUIDE: react ==="));
        assert!(text2.contains("useCallback prevents re-renders"));
    }

    /// Guide renames rewrite references without churning revisions:
    /// related_guides is not indexed text, so the document revision must
    /// stay put (a bump without a projection job would skew canonical
    /// ahead of the projection with no refresh coming).
    #[test]
    fn guide_rename_keeps_document_revision() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(&disp, 1, "## Linked\n\n### Context\nbody");
        let repo = disp.repo();
        let mut m = repo.export_snapshot().unwrap().memories.remove(0);
        m.related_guides = vec!["old".to_string()];
        repo.put_memory_direct(&m).unwrap();
        let rev = m.document_revision;
        let rev_entity = m.entity_revision;
        rename_guide_in_memories(repo, "old", "new").unwrap();
        let after = repo.get_memories(&[m.id]).unwrap().remove(0);
        assert_eq!(after.related_guides, vec!["new".to_string()]);
        assert_eq!(
            after.document_revision, rev,
            "unindexed rename must not churn the revision"
        );
        assert_eq!(
            after.entity_revision,
            rev_entity.next(),
            "entity revision still advances for conflict detection"
        );
    }

    /// Guide removal rewrites references the same way: no document churn,
    /// entity revision advance preserved.
    #[test]
    fn guide_remove_keeps_document_revision() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(&disp, 1, "## Linked\n\n### Context\nbody");
        let repo = disp.repo();
        let mut m = repo.export_snapshot().unwrap().memories.remove(0);
        m.related_guides = vec!["old".to_string()];
        repo.put_memory_direct(&m).unwrap();
        let rev = m.document_revision;
        let rev_entity = m.entity_revision;
        remove_guide_from_memories(repo, "old").unwrap();
        let after = repo.get_memories(&[m.id]).unwrap().remove(0);
        assert!(after.related_guides.is_empty());
        assert_eq!(
            after.document_revision, rev,
            "unindexed remove must not churn the revision"
        );
        assert_eq!(
            after.entity_revision,
            rev_entity.next(),
            "entity revision still advances for conflict detection"
        );
    }

    /// An unrecognized practice outcome errors like the sibling tools
    /// (session_attempt/session_end): silently dropping it would lose the
    /// signal and skew the success hook.
    #[test]
    fn guide_practice_rejects_unknown_outcome() {
        let (disp, _dir) = test_dispatcher();
        let args = GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec![],
            learnings: vec!["x".to_string()],
            outcome: Some("maybe".to_string()),
        };
        let env = tool_call(1, ToolArgs::GuidePractice(args.clone()));
        let result = run(&disp, &env, &ToolArgs::GuidePractice(args));
        assert!(
            result_is_error(&result),
            "unknown outcome must error, got: {}",
            result_text(&result)
        );
    }

    #[test]
    fn guide_practice_increments_usage() {
        let (disp, _dir) = test_dispatcher();
        // First practice creates the guide (usage_count = 1).
        let env = tool_call(
            1,
            ToolArgs::GuidePractice(GuidePracticeArgs {
                guide: "git".to_string(),
                category: "dev-tool".to_string(),
                description: None,
                contexts: vec!["commits".to_string()],
                learnings: vec!["always stage selectively".to_string()],
                outcome: Some("success".to_string()),
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::GuidePractice(GuidePracticeArgs {
                guide: "git".to_string(),
                category: "dev-tool".to_string(),
                description: None,
                contexts: vec!["commits".to_string()],
                learnings: vec!["always stage selectively".to_string()],
                outcome: Some("success".to_string()),
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Created guide \"git\""));
        let structured = result_structured(&result).unwrap();
        assert_eq!(structured["usage_count"], json!(1));

        // Second practice increments usage.
        let env2 = tool_call(
            2,
            ToolArgs::GuidePractice(GuidePracticeArgs {
                guide: "git".to_string(),
                category: "dev-tool".to_string(),
                description: None,
                contexts: vec!["branches".to_string()],
                learnings: vec!["rebase before merge".to_string()],
                outcome: Some("failure".to_string()),
            }),
        );
        let result2 = run(
            &disp,
            &env2,
            &ToolArgs::GuidePractice(GuidePracticeArgs {
                guide: "git".to_string(),
                category: "dev-tool".to_string(),
                description: None,
                contexts: vec!["branches".to_string()],
                learnings: vec!["rebase before merge".to_string()],
                outcome: Some("failure".to_string()),
            }),
        );
        assert!(result_text(&result2).contains("Updated guide \"git\""));
        let structured2 = result_structured(&result2).unwrap();
        assert_eq!(structured2["usage_count"], json!(2));
    }

    /// P1 replay: repeating the same guide-practice operation (same envelope
    /// operation ID) must not double-count usage/success counters.
    #[test]
    fn guide_practice_replay_does_not_double_count() {
        let (disp, _dir) = test_dispatcher();
        let args = GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec!["commits".to_string()],
            learnings: vec!["stage selectively".to_string()],
            outcome: Some("success".to_string()),
        };
        let env = tool_call(1, ToolArgs::GuidePractice(args.clone()));
        let first = run(&disp, &env, &ToolArgs::GuidePractice(args.clone()));
        assert!(!result_is_error(&first));
        let second = run(&disp, &env, &ToolArgs::GuidePractice(args));
        assert!(!result_is_error(&second));
        let guide = disp.repo().get_guide("git").unwrap().expect("guide exists");
        assert_eq!(guide.usage_count, 1, "replay must not recount usage");
        assert_eq!(guide.success_count, 1, "replay must not recount success");
    }

    /// P1 race: a guide-reference rename preserves a concurrent content
    /// update (fresh-read patch, no stale-clone overwrite).
    #[test]
    fn guide_rename_preserves_concurrent_content_update() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(&disp, 1, "## Linked\n\n### Context\nbody");
        let repo = disp.repo();
        let m = repo.export_snapshot().unwrap().memories.remove(0);
        let mut seeded = m.clone();
        seeded.related_guides = vec!["old".to_string()];
        repo.put_memory_direct(&seeded).unwrap();
        // Concurrent content update through the canonical tool path.
        let update = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: id.clone(),
            fragment: Some("## Linked\n\n### Context\nnew body".to_string()),
            ..Default::default()
        });
        let update_env = tool_call(2, update.clone());
        let update_result = run(&disp, &update_env, &update);
        assert!(!result_is_error(&update_result));
        // Guide rename after the content commit must keep the new text.
        rename_guide_in_memories(repo, "old", "new").unwrap();
        let after = repo.get_memories(&[m.id]).unwrap().remove(0);
        assert!(
            after.fragment.contains("new body"),
            "rename must preserve concurrent content, got: {}",
            after.fragment
        );
        assert_eq!(after.related_guides, vec!["new".to_string()]);
    }

    /// P1 atomic merge: success moves all references and removes sources
    /// together; a missing source fails without a half-merge.
    #[test]
    fn guide_merge_is_all_or_nothing() {
        let (disp, _dir) = test_dispatcher();
        for (op, name) in [(1, "alpha"), (2, "beta")] {
            let args = GuideCreateArgs {
                guide: name.to_string(),
                category: "dev-tool".to_string(),
                description: format!("{name} guide"),
                contexts: vec![],
                learnings: vec![],
            };
            let env = tool_call(op, ToolArgs::GuideCreate(args.clone()));
            run(&disp, &env, &ToolArgs::GuideCreate(args));
        }
        add_fragment(&disp, 10, "## M\n\n### Context\nbody");
        let repo = disp.repo();
        let mut m = repo.export_snapshot().unwrap().memories.remove(0);
        m.related_guides = vec!["alpha".to_string(), "beta".to_string()];
        repo.put_memory_direct(&m).unwrap();
        let args = GuideMergeArgs {
            guides: vec!["alpha".to_string(), "beta".to_string()],
            guide: "gamma".to_string(),
            category: "dev-tool".to_string(),
            description: Some("merged".to_string()),
            contexts: None,
            learnings: None,
        };
        let env = tool_call(11, ToolArgs::GuideMerge(args.clone()));
        let result = run(&disp, &env, &ToolArgs::GuideMerge(args));
        assert!(!result_is_error(&result));
        assert!(repo.get_guide("alpha").unwrap().is_none());
        assert!(repo.get_guide("beta").unwrap().is_none());
        assert!(repo.get_guide("gamma").unwrap().is_some());
        let after = repo.get_memories(&[m.id]).unwrap().remove(0);
        assert!(
            after.related_guides.iter().any(|g| g == "gamma"),
            "merged refs must point at gamma, got: {:?}",
            after.related_guides
        );
        assert!(
            !after
                .related_guides
                .iter()
                .any(|g| g == "alpha" || g == "beta"),
            "no stale source refs, got: {:?}",
            after.related_guides
        );
        // Missing source: no partial state (gamma stays, no new guide).
        let bad = GuideMergeArgs {
            guides: vec!["gamma".to_string(), "missing".to_string()],
            guide: "delta".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: None,
            learnings: None,
        };
        let bad_env = tool_call(12, ToolArgs::GuideMerge(bad.clone()));
        let bad_result = run(&disp, &bad_env, &ToolArgs::GuideMerge(bad));
        assert!(result_is_error(&bad_result));
        assert!(repo.get_guide("gamma").unwrap().is_some());
        assert!(repo.get_guide("delta").unwrap().is_none());
    }

    #[test]
    fn guide_forget_removes_guide() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::GuideCreate(GuideCreateArgs {
                guide: "temp".to_string(),
                category: "dev-tool".to_string(),
                description: "temp guide".to_string(),
                contexts: vec![],
                learnings: vec![],
            }),
        );
        run(
            &disp,
            &env,
            &ToolArgs::GuideCreate(GuideCreateArgs {
                guide: "temp".to_string(),
                category: "dev-tool".to_string(),
                description: "temp guide".to_string(),
                contexts: vec![],
                learnings: vec![],
            }),
        );

        let env2 = tool_call(
            2,
            ToolArgs::GuideForget(GuideForgetArgs {
                guide: "temp".to_string(),
            }),
        );
        let result = run(
            &disp,
            &env2,
            &ToolArgs::GuideForget(GuideForgetArgs {
                guide: "temp".to_string(),
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Successfully forgot guide: temp"));

        // Verify it's gone.
        let env3 = tool_call(
            3,
            ToolArgs::GuideGet(GuideGetArgs {
                guide: Some("temp".to_string()),
                ..Default::default()
            }),
        );
        let result3 = run(
            &disp,
            &env3,
            &ToolArgs::GuideGet(GuideGetArgs {
                guide: Some("temp".to_string()),
                ..Default::default()
            }),
        );
        assert!(result_text(&result3).contains("Guide not found"));
    }

    #[test]
    fn guide_distill_links_memory_to_guide() {
        let (disp, _dir) = test_dispatcher();
        // Create a memory.
        let mem_id = add_fragment(
            &disp,
            1,
            "## A pattern worth distilling\n\n### Context\nReusable skill.",
        );
        // Distill it.
        let env = tool_call(
            2,
            ToolArgs::GuideDistill(GuideDistillArgs {
                memory_id: mem_id.clone(),
                guide: "react".to_string(),
                category: Some("web-frontend".to_string()),
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::GuideDistill(GuideDistillArgs {
                memory_id: mem_id.clone(),
                guide: "react".to_string(),
                category: Some("web-frontend".to_string()),
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Successfully distilled memory"));

        // The guide now contains the memory's fragment as a learning.
        let env2 = tool_call(
            3,
            ToolArgs::GuideGet(GuideGetArgs {
                guide: Some("react".to_string()),
                ..Default::default()
            }),
        );
        let result2 = run(
            &disp,
            &env2,
            &ToolArgs::GuideGet(GuideGetArgs {
                guide: Some("react".to_string()),
                ..Default::default()
            }),
        );
        assert!(result_text(&result2).contains("A pattern worth distilling"));
    }

    #[test]
    fn guide_forget_removes_memory_references() {
        let (disp, _dir) = test_dispatcher();
        // Create a memory and distill it into a guide (sets related_guides).
        let mem_id = add_fragment(
            &disp,
            1,
            "## Distillable pattern\n\n### Context\nReusable skill.",
        );
        let distill = ToolArgs::GuideDistill(GuideDistillArgs {
            memory_id: mem_id.clone(),
            guide: "react".to_string(),
            category: Some("web-frontend".to_string()),
        });
        let env = tool_call(2, distill.clone());
        run(&disp, &env, &distill);

        // Confirm the memory now references the guide.
        let eid = disp.repo().resolve_id(&mem_id).unwrap();
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(mems[0].related_guides.iter().any(|g| g == "react"));

        // Forget the guide.
        let forget = ToolArgs::GuideForget(GuideForgetArgs {
            guide: "react".to_string(),
        });
        let env2 = tool_call(3, forget.clone());
        let result = run(&disp, &env2, &forget);
        assert!(!result_is_error(&result));

        // The memory must no longer reference the forgotten guide.
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(
            !mems[0]
                .related_guides
                .iter()
                .any(|g| g.eq_ignore_ascii_case("react")),
            "related_guides should not reference a forgotten guide"
        );
    }

    #[test]
    fn guide_update_renames_memory_references() {
        let (disp, _dir) = test_dispatcher();
        // Create a memory and distill it into a guide.
        let mem_id = add_fragment(
            &disp,
            1,
            "## Renamable pattern\n\n### Context\nReusable skill.",
        );
        let distill = ToolArgs::GuideDistill(GuideDistillArgs {
            memory_id: mem_id.clone(),
            guide: "react".to_string(),
            category: Some("web-frontend".to_string()),
        });
        let env = tool_call(2, distill.clone());
        run(&disp, &env, &distill);

        let eid = disp.repo().resolve_id(&mem_id).unwrap();
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(mems[0].related_guides.iter().any(|g| g == "react"));

        // Rename the guide.
        let update = ToolArgs::GuideUpdate(GuideUpdateArgs {
            guide: "react".to_string(),
            new_name: Some("react18".to_string()),
            ..Default::default()
        });
        let env2 = tool_call(3, update.clone());
        let result = run(&disp, &env2, &update);
        assert!(!result_is_error(&result));

        // The memory must now reference the new name.
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(
            mems[0]
                .related_guides
                .iter()
                .any(|g| g.eq_ignore_ascii_case("react18")),
            "related_guides should reference the renamed guide"
        );
        assert!(
            !mems[0]
                .related_guides
                .iter()
                .any(|g| g.eq_ignore_ascii_case("react")),
            "related_guides should not reference the old name"
        );
    }

    #[test]
    fn guide_merge_combines_guides() {
        let (disp, _dir) = test_dispatcher();
        // Create two guides.
        for (op, name) in [(1, "alpha"), (2, "beta")] {
            let env = tool_call(
                op,
                ToolArgs::GuideCreate(GuideCreateArgs {
                    guide: name.to_string(),
                    category: "dev-tool".to_string(),
                    description: format!("{name} desc"),
                    contexts: vec![format!("{name}-ctx")],
                    learnings: vec![format!("{name}-learn")],
                }),
            );
            run(
                &disp,
                &env,
                &ToolArgs::GuideCreate(GuideCreateArgs {
                    guide: name.to_string(),
                    category: "dev-tool".to_string(),
                    description: format!("{name} desc"),
                    contexts: vec![format!("{name}-ctx")],
                    learnings: vec![format!("{name}-learn")],
                }),
            );
        }

        // Merge them.
        let env = tool_call(
            3,
            ToolArgs::GuideMerge(GuideMergeArgs {
                guides: vec!["alpha".to_string(), "beta".to_string()],
                guide: "gamma".to_string(),
                category: "dev-tool".to_string(),
                description: Some("merged".to_string()),
                contexts: None,
                learnings: None,
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::GuideMerge(GuideMergeArgs {
                guides: vec!["alpha".to_string(), "beta".to_string()],
                guide: "gamma".to_string(),
                category: "dev-tool".to_string(),
                description: Some("merged".to_string()),
                contexts: None,
                learnings: None,
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Merged 2 guides into \"gamma\""));

        // Sources are gone, merged guide exists with combined learnings.
        let env2 = tool_call(
            4,
            ToolArgs::GuideGet(GuideGetArgs {
                guide: Some("gamma".to_string()),
                ..Default::default()
            }),
        );
        let result2 = run(
            &disp,
            &env2,
            &ToolArgs::GuideGet(GuideGetArgs {
                guide: Some("gamma".to_string()),
                ..Default::default()
            }),
        );
        let text2 = result_text(&result2);
        assert!(text2.contains("alpha-learn"));
        assert!(text2.contains("beta-learn"));
    }

    // ---- WP-09: session tools ----

    #[test]
    fn session_start_attempt_end_lifecycle() {
        let (disp, _dir) = test_dispatcher();
        // Start.
        let env = tool_call(
            1,
            ToolArgs::SessionStart(SessionStartArgs {
                task_type: "debugging".to_string(),
                technologies: vec!["rust".to_string()],
                initial_approach: Some("read the code".to_string()),
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::SessionStart(SessionStartArgs {
                task_type: "debugging".to_string(),
                technologies: vec!["rust".to_string()],
                initial_approach: Some("read the code".to_string()),
            }),
        );
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Session started:"));

        // Attempt.
        let env2 = tool_call(
            2,
            ToolArgs::SessionAttempt(SessionAttemptArgs {
                approach: "try X".to_string(),
                outcome: "rejected".to_string(),
                critique: Some("didn't work".to_string()),
                rationale: None,
                related_memory_id: None,
            }),
        );
        let result2 = run(
            &disp,
            &env2,
            &ToolArgs::SessionAttempt(SessionAttemptArgs {
                approach: "try X".to_string(),
                outcome: "rejected".to_string(),
                critique: Some("didn't work".to_string()),
                rationale: None,
                related_memory_id: None,
            }),
        );
        assert!(!result_is_error(&result2));
        assert!(text_contains(&result2, "Recorded attempt #1"));

        // End.
        let env3 = tool_call(
            3,
            ToolArgs::SessionEnd(SessionEndArgs {
                outcome: "success".to_string(),
                final_approach: Some("fixed it".to_string()),
                lessons: vec!["lesson one".to_string()],
            }),
        );
        let result3 = run(
            &disp,
            &env3,
            &ToolArgs::SessionEnd(SessionEndArgs {
                outcome: "success".to_string(),
                final_approach: Some("fixed it".to_string()),
                lessons: vec!["lesson one".to_string()],
            }),
        );
        assert!(!result_is_error(&result3));
        assert!(text_contains(&result3, "ended: success"));
    }

    /// Whole learning workflow (S6/WP-09 trace): recall, act, persist,
    /// practice a guide, record attempts and end — with correct cross-tool
    /// attribution through the public tool surface (no hidden reasoning).
    #[test]
    fn whole_learning_workflow_recall_act_persist() {
        let (disp, _dir) = test_dispatcher();
        // Seed: one task-relevant memory, one unrelated.
        let rust_id = add_fragment(
            &disp,
            1,
            "## Rust Async\n\n### Context\nUse tokio spawn_blocking for blocking work.",
        );
        add_fragment(
            &disp,
            2,
            "## Sourdough\n\n### Context\nBake bread at 240C with steam.",
        );

        // RECALL: start a session; the relevant memory is pre-loaded.
        let start = ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec!["rust".to_string(), "tokio".to_string()],
            initial_approach: Some("read the code".to_string()),
        });
        let result = run(&disp, &tool_call(10, start.clone()), &start);
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        let preloaded: Vec<String> = structured["preloaded_memories"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        assert!(
            preloaded.contains(&rust_id),
            "task-relevant memory must preload, got: {preloaded:?}"
        );
        let session_id = structured["session_id"].as_str().unwrap().to_string();

        // RECALL: read the preloaded memory (access recorded).
        let read = ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(rust_id.clone()),
            ..Default::default()
        });
        let result = run(&disp, &tool_call(11, read.clone()), &read);
        assert!(!result_is_error(&result));
        // The explicit read leaves its own observable mark (access count):
        // one from the preload boost plus one from this read.
        let read_eid = disp.repo().resolve_id(&rust_id).unwrap();
        let read_mem = disp
            .repo()
            .get_memories(&[read_eid])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            read_mem.access_count, 2,
            "explicit read must record access on top of the preload boost"
        );

        // ACT: record a rejected attempt explicitly.
        let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
            approach: "guess from prose".to_string(),
            outcome: "rejected".to_string(),
            critique: Some("no evidence".to_string()),
            rationale: None,
            related_memory_id: Some(rust_id.clone()),
        });
        let result = run(&disp, &tool_call(12, attempt.clone()), &attempt);
        assert!(!result_is_error(&result));

        // PERSIST: save the lesson; it links to the active session.
        let new_id = add_fragment(
            &disp,
            13,
            "## Blocking Lessons\n\n### Context\nNever block Tokio core workers; use spawn_blocking.",
        );

        // PRACTICE: create + practice a guide for the session.
        let create = ToolArgs::GuideCreate(GuideCreateArgs {
            guide: "tokio-discipline".to_string(),
            category: "dev-tool".to_string(),
            description: "## Tokio Discipline\n\n### Protocol\nSpawn blocking.".to_string(),
            contexts: vec!["async".to_string()],
            learnings: vec![],
        });
        let result = run(&disp, &tool_call(14, create.clone()), &create);
        assert!(!result_is_error(&result));
        let practice = ToolArgs::GuidePractice(GuidePracticeArgs {
            guide: "tokio-discipline".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec!["async".to_string()],
            learnings: vec!["spawn_blocking reviewed".to_string()],
            outcome: Some("success".to_string()),
        });
        let result = run(&disp, &tool_call(15, practice.clone()), &practice);
        assert!(!result_is_error(&result));

        // END: close the session; cross-tool attribution must hold.
        let end = ToolArgs::SessionEnd(SessionEndArgs {
            outcome: "success".to_string(),
            final_approach: Some("spawn_blocking everywhere".to_string()),
            lessons: vec!["verify before claiming".to_string()],
        });
        let result = run(&disp, &tool_call(16, end.clone()), &end);
        assert!(!result_is_error(&result));

        // Coherence across the loop, read back from canonical state.
        let handle =
            crate::domain::id::SessionHandle::new(uuid::Uuid::parse_str(&session_id).unwrap());
        let session = disp.registry().session(handle).unwrap().clone();
        assert!(session.memories_read.contains(&rust_id));
        assert!(session.memories_created.contains(&new_id));
        assert_eq!(session.attempts.len(), 1);
        assert!(
            session
                .guides_used
                .contains(&"tokio-discipline".to_string())
        );
        assert!(session.status.is_terminal(), "session must be ended");
        let guide = disp.repo().get_guide("tokio-discipline").unwrap().unwrap();
        // Create seeds usage at 1; the explicit practice adds exactly one more.
        assert_eq!(guide.usage_count, 2);
        let eid = disp.repo().resolve_id(&new_id).unwrap();
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert_eq!(
            mems[0].session_id.as_deref(),
            Some(session_id.as_str()),
            "persisted memory links the session"
        );
    }

    #[test]
    fn session_stats_reports_active_completed_and_empty() {
        let stats = |disp: &Dispatcher, op: u64| {
            let args = ToolArgs::SessionStats(SessionStatsArgs {
                count: Some(10),
                response_format: None,
            });
            run(disp, &tool_call(op, args.clone()), &args)
        };
        // Empty store: no sessions recorded yet.
        let (disp, _dir) = test_dispatcher();
        let empty = stats(&disp, 1);
        assert!(!result_is_error(&empty));
        assert!(result_text(&empty).contains("No past sessions recorded yet."));

        // Active session with one attempt and technologies.
        let start = ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec!["rust".to_string()],
            initial_approach: None,
        });
        run(&disp, &tool_call(2, start.clone()), &start);
        let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
            approach: "try X".to_string(),
            outcome: "rejected".to_string(),
            critique: None,
            rationale: None,
            related_memory_id: None,
        });
        run(&disp, &tool_call(3, attempt.clone()), &attempt);
        let active = stats(&disp, 4);
        assert!(!result_is_error(&active));
        let text = result_text(&active);
        assert!(text.contains("Active session: 1 tool calls"));
        assert!(text.contains("Technologies: rust"));

        // Ended session moves to recent history.
        let end = ToolArgs::SessionEnd(SessionEndArgs {
            outcome: "success".to_string(),
            final_approach: None,
            lessons: vec![],
        });
        run(&disp, &tool_call(5, end.clone()), &end);
        let done = stats(&disp, 6);
        assert!(!result_is_error(&done));
        let text = result_text(&done);
        assert!(text.contains("Recent sessions (1):"));
        assert!(!text.contains("Active session:"));
    }

    #[test]
    fn session_start_preload_boosts_confidence_by_002() {
        let (disp, _dir) = test_dispatcher();
        // Add a memory matching the task description so it gets pre-loaded.
        let id = add_fragment(
            &disp,
            1,
            "## Rust debugging fragment\n\n### Context\nRust debugging notes.",
        );
        let eid = disp.repo().resolve_id(&id).unwrap();
        // Lower confidence so the +0.02 boost is observable.
        let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: id.clone(),
            confidence: Some(0.5),
            ..Default::default()
        });
        let env = tool_call(2, upd.clone());
        run(&disp, &env, &upd);

        // Start a session matching "rust debugging".
        let start = ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec!["rust".to_string()],
            initial_approach: None,
        });
        let env = tool_call(3, start.clone());
        let result = run(&disp, &env, &start);
        assert!(!result_is_error(&result));
        assert!(result_text(&result).contains("Pre-loaded memories:"));

        // The pre-load boost must be +0.02 (upstream boostConfidence), not +0.015.
        let mems = disp.repo().get_memories(&[eid]).unwrap();
        assert!(
            (mems[0].confidence - 0.52).abs() < 1e-9,
            "expected 0.52, got {}",
            mems[0].confidence
        );
        assert_eq!(mems[0].access_count, 1);
    }

    #[test]
    fn session_end_is_retry_safe_no_double_count() {
        let (disp, _dir) = test_dispatcher();
        // Start a session and practice a guide into it.
        let start = ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec![],
            initial_approach: None,
        });
        let env = tool_call(1, start.clone());
        run(&disp, &env, &start);

        let practice = ToolArgs::GuidePractice(GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec![],
            learnings: vec![],
            outcome: None,
        });
        let env = tool_call(2, practice.clone());
        run(&disp, &env, &practice);

        // First end: success. The guide's success_count should be 1.
        let end = ToolArgs::SessionEnd(SessionEndArgs {
            outcome: "success".to_string(),
            final_approach: None,
            lessons: vec![],
        });
        let env = tool_call(3, end.clone());
        let result = run(&disp, &env, &end);
        assert!(!result_is_error(&result));
        let guide = disp.repo().get_guide("git").unwrap().unwrap();
        assert_eq!(guide.success_count, 1, "first end should count once");

        // Second end (retry): must be rejected and must NOT double-count.
        let env = tool_call(4, end.clone());
        let result2 = run(&disp, &env, &end);
        assert!(result_is_error(&result2));
        assert!(result_text(&result2).contains("No active session"));
        let guide = disp.repo().get_guide("git").unwrap().unwrap();
        assert_eq!(
            guide.success_count, 1,
            "a retried session_end must not double-count guide outcomes"
        );
    }

    #[test]
    fn session_attempt_without_session_is_error() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::SessionAttempt(SessionAttemptArgs {
                approach: "try X".to_string(),
                outcome: "rejected".to_string(),
                critique: None,
                rationale: None,
                related_memory_id: None,
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::SessionAttempt(SessionAttemptArgs {
                approach: "try X".to_string(),
                outcome: "rejected".to_string(),
                critique: None,
                rationale: None,
                related_memory_id: None,
            }),
        );
        assert!(result_is_error(&result));
        assert!(result_text(&result).contains("No active session"));
    }

    #[test]
    fn session_end_without_session_is_error() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::SessionEnd(SessionEndArgs {
                outcome: "success".to_string(),
                final_approach: None,
                lessons: vec![],
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::SessionEnd(SessionEndArgs {
                outcome: "success".to_string(),
                final_approach: None,
                lessons: vec![],
            }),
        );
        assert!(result_is_error(&result));
        assert!(result_text(&result).contains("No active session"));
    }

    /// P1 replay safety at the tool layer: repeating the same
    /// `session_attempt` operation (same envelope operation ID) records
    /// exactly one attempt and increments counters exactly once.
    #[test]
    fn session_attempt_tool_replay_records_once() {
        let (disp, _dir) = test_dispatcher();
        let start = ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec![],
            initial_approach: None,
        });
        run(&disp, &tool_call(1, start.clone()), &start);
        let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
            approach: "try X".to_string(),
            outcome: "rejected".to_string(),
            critique: Some("bad".to_string()),
            rationale: None,
            related_memory_id: None,
        });
        // Same operation ID twice = one retried operation.
        let env = tool_call(2, attempt.clone());
        let first = run(&disp, &env, &attempt);
        assert!(!result_is_error(&first));
        let second = run(&disp, &env, &attempt);
        assert!(!result_is_error(&second));
        let sessions = disp.registry().all_sessions_owned();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].attempts.len(),
            1,
            "tool replay must not duplicate the attempt"
        );
        assert_eq!(
            sessions[0].refinement_attempts, 1,
            "tool replay must not double-count refinement"
        );
        assert_eq!(
            sessions[0].self_critique_count, 1,
            "tool replay must not double-count self-critique"
        );
    }

    // ---- WP-09: intelligence tools ----

    #[test]
    fn conflict_scan_detects_opposing_fragments() {
        let (disp, _dir) = test_dispatcher();
        // Shared topic (redis/caching/session/data) with opposing negation
        // ("always" vs "never") — distinct enough to pass add-dedup, overlapping
        // enough for the conflict heuristic to fire.
        add_fragment(
            &disp,
            1,
            "Always use Redis for caching session data in the API layer.",
        );
        add_fragment(
            &disp,
            2,
            "Never use Redis for caching session data; pick Memcached instead.",
        );

        let env = tool_call(3, ToolArgs::ConflictScan(ConflictScanArgs::default()));
        let result = run(
            &disp,
            &env,
            &ToolArgs::ConflictScan(ConflictScanArgs::default()),
        );
        assert!(!result_is_error(&result));
        let structured = result_structured(&result).unwrap();
        assert!(
            structured["count"].as_u64().unwrap() >= 1,
            "expected a conflict pair"
        );
        assert!(result_text(&result).contains("CONFLICT DETECTION"));
    }

    #[test]
    fn proactive_analysis_runs_cleanly() {
        let (disp, _dir) = test_dispatcher();
        add_fragment(&disp, 1, "## A fact\n\n### Context\nSome durable fact.");

        let env = tool_call(
            2,
            ToolArgs::ProactiveAnalysis(ProactiveAnalysisArgs::default()),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::ProactiveAnalysis(ProactiveAnalysisArgs::default()),
        );
        assert!(!result_is_error(&result));
        let text = result_text(&result);
        assert!(text.contains("PROACTIVE ANALYSIS"));
    }

    #[test]
    fn project_analytics_all_projects_overview() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::ProjectAnalytics(ProjectAnalyticsArgs::default()),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::ProjectAnalytics(ProjectAnalyticsArgs::default()),
        );
        assert!(!result_is_error(&result));
        // No projects yet.
        assert!(result_text(&result).contains("No projects found"));
    }

    // ---- WP-09: suggestion_respond ----

    #[test]
    fn suggestion_respond_requires_valid_action() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::SuggestionRespond(SuggestionRespondArgs {
                id: 1,
                action: "bogus".to_string(),
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::SuggestionRespond(SuggestionRespondArgs {
                id: 1,
                action: "bogus".to_string(),
            }),
        );
        assert!(result_is_error(&result));
        assert!(result_text(&result).contains("must be one of: accept, dismiss"));
    }

    #[test]
    fn suggestion_respond_missing_suggestion_is_error() {
        let (disp, _dir) = test_dispatcher();
        let env = tool_call(
            1,
            ToolArgs::SuggestionRespond(SuggestionRespondArgs {
                id: 999,
                action: "accept".to_string(),
            }),
        );
        let result = run(
            &disp,
            &env,
            &ToolArgs::SuggestionRespond(SuggestionRespondArgs {
                id: 999,
                action: "accept".to_string(),
            }),
        );
        assert!(result_is_error(&result));
        assert!(result_text(&result).contains("Could not update this suggestion"));
    }

    fn text_contains(p: &DomainPayload, needle: &str) -> bool {
        result_text(p).contains(needle)
    }

    /// Session linkage lands in the single AddMemory apply: no blind second
    /// write (no clobber window), no document bump, one projection job.
    #[test]
    fn memory_add_links_session_in_single_apply() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(&disp, 1, "## Linked\n\n### Context\nSession link fixture.");
        let eid = disp.repo().resolve_id(&id).unwrap();
        let stored = disp.repo().get_memories(&[eid]).unwrap();
        assert_eq!(stored.len(), 1);
        assert!(
            stored[0].session_id.is_some(),
            "session link must be stored on the record"
        );
        assert_eq!(
            stored[0].document_revision.as_u64(),
            0,
            "single apply performs no link bump"
        );
        let job = disp
            .repo()
            .projection_job(eid)
            .unwrap()
            .expect("add enqueues one pending job");
        assert_eq!(job.seq, 1, "single enqueue, no re-point");
    }
}

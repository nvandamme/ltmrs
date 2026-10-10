//! memory_read tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::MemoryReadArgs;
use ltmrs_domain::command::{DomainCommand, DomainResult};
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;
use serde_json::{Value, json};

use super::ids::{legacy_id_of, legacy_id_of_from_id, resolve_id};
use super::recall::{
    RecallSelection, expand_graph, explain_recall, format_recall_explanation, fragment_detail_json,
    parse_iso_date, recall_browse, render_detail, render_summary_index,
};

use super::replay::sub_command_ctx;
use super::{err_result, format_result};

/// Record read side effects (RQ-17) via the canonical gateway: confidence
/// boost, access counters, last-accessed timestamp and the optional context
/// tag — persisted before the read response reports success.
pub(crate) fn record_access(
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

pub(crate) fn exec_memory_read(
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

    let (mut memories, method) = recall_browse(disp, args)?;

    // Post-filters (dates, min_confidence) applied on top of the search result.
    // The engine request already carries these predicates; this re-check is a
    // cheap idempotent net for the degraded snapshot path.
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
    // `method` is whatever produced the memories (engine legs or the
    // labelled degraded scan), never a hardcoded guess.
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

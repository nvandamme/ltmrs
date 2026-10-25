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
use super::{effective_for_read, err_result, format_result, stable_visible};
use ltmrs_service::projects::ResolvedScope;
use ltmrs_service::repository::AdmittedScope;

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

/// Attribute a successful read to the channel's active session (Wave B2):
/// traced sessions get canonical MemoryRead links (fail-loud via `?`,
/// same dedup as the preload helper); virtual sessions get an in-memory
/// routing record only; no traced session is a canonical no-op.
fn attribute_read_to_session(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    legacy_ids: &[String],
) -> DomainResult<()> {
    if legacy_ids.is_empty() {
        return Ok(());
    }
    if let Some(handle) = disp.resolve_session(envelope.frontend_id, envelope.channel_id) {
        disp.repo().track_session_link(
            admitted,
            handle,
            ltmrs_service::repository::SessionLinkField::MemoryRead,
            legacy_ids,
        )?;
    } else {
        let now = disp.clock().now_millis();
        let vhandle =
            disp.registry()
                .ensure_virtual_session(envelope.frontend_id, envelope.channel_id, now);
        disp.registry().track_virtual_read(vhandle, legacy_ids);
    }
    Ok(())
}

/// Plan co-read links for one returned set (Wave B2): gate every pair
/// with the stable visibility predicate (no cross-boundary edges, even
/// under all=true), return all gated pairs for associatedWith plus up to
/// 3 relations star-anchored on the highest-confidence memory.
#[allow(clippy::type_complexity)]
fn plan_coread(
    envelope: &IpcEnvelope,
    resolved: &ResolvedScope,
    memories: &[Memory],
    now_millis: u64,
) -> DomainResult<(
    Vec<(EntityId, EntityId)>,
    Vec<ltmrs_domain::relation::Relation>,
)> {
    if memories.len() < 2 {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut gated: Vec<&Memory> = Vec::new();
    for m in memories {
        if stable_visible(resolved, m)? {
            gated.push(m);
        }
    }
    if gated.len() < 2 {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut pairs: Vec<(EntityId, EntityId)> = Vec::new();
    for i in 0..gated.len() {
        for j in (i + 1)..gated.len() {
            let (a, b) = (gated[i].id, gated[j].id);
            let both_visible =
                stable_visible(resolved, gated[i])? && stable_visible(resolved, gated[j])?;
            if both_visible {
                pairs.push((a, b));
            }
        }
    }
    if pairs.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut by_conf: Vec<&Memory> = gated.clone();
    by_conf.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut relations: Vec<ltmrs_domain::relation::Relation> = Vec::new();
    let top = by_conf[0].id;
    for other in by_conf.iter().skip(1).take(3) {
        if pairs
            .iter()
            .any(|(a, b)| (*a == top && *b == other.id) || (*a == other.id && *b == top))
        {
            relations.push(super::new_relation(
                envelope,
                now_millis,
                top,
                other.id,
                ltmrs_domain::relation::RelationType::RelatedTo,
                Some("Co-read: returned together".to_string()),
            ));
        }
        if relations.len() >= 3 {
            break;
        }
    }
    Ok((pairs, relations))
}

pub(crate) fn exec_memory_read(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &MemoryReadArgs,
    resolved: &ResolvedScope,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;
    // Stable-id direct reads obey scope (Part I 01:§6.3): a stable identity
    // never resolves foreign rows by ID (treated as not found, never
    // leaked). Only the project dimension applies to explicit IDs.
    let id_scope = effective_for_read(resolved, None, None, None, args.all)?;
    let in_scope = |m: &Memory| {
        resolved.project_id.is_none()
            || id_scope.project_includes_id(m.project_id, m.project.as_deref())
    };

    // Single-ID detail.
    if let Some(id) = &args.id {
        let Ok(eid) = resolve_id(repo, id) else {
            return Ok(err_result(&format!("Fragment with ID '{id}' not found")));
        };
        let mems = repo.get_memories(&[eid])?;
        let Some(m_pre) = mems.first() else {
            return Ok(err_result(&format!("Fragment with ID '{id}' not found")));
        };
        if !in_scope(m_pre) {
            return Ok(err_result(&format!("Fragment with ID '{id}' not found")));
        }
        let mut selections: Vec<RecallSelection> = vec![RecallSelection {
            memory: m_pre,
            method: "explicit_id",
            rank: None,
            score: None,
            graph: None,
        }];
        record_access(disp, envelope, &[m_pre.id], args.context.as_deref())?;
        attribute_read_to_session(
            disp,
            envelope,
            admitted,
            std::slice::from_ref(&repo.legacy_id(m_pre)),
        )?;
        // Render the post-boost record (upstream boostOnAccess returns the
        // boosted fragment which is what gets formatted).
        let mems = repo.get_memories(&[eid])?;
        let Some(m) = mems.first() else {
            return Ok(err_result(&format!("Fragment with ID '{id}' not found")));
        };
        let resolver = |eid: &EntityId| legacy_id_of_from_id(repo, *eid);
        let mut text = render_detail(&legacy_id_of(repo, m), m, &resolver);
        let mut related_graph: Vec<Value> = Vec::new();
        // Bounded tool-level expansion (Wave B5a): BFS with per-node
        // fanout cap, total-node budget and confidence x 0.6^depth scoring,
        // reusing the engine's traversal core. Eligibility is the call's
        // full scope (project, lifecycle, confidence) plus the stable
        // visibility gate, so expansion — a derived read — never leaks
        // across the boundary even when the seed was read with all=true.
        // Bound outside the flag branch: selections borrows the reached
        // memories past the traversal.
        let (nodes, graph_truncated): (super::recall::ExpandedNodes, bool) = if args.expand_graph {
            let scope = effective_for_read(
                resolved,
                args.min_confidence,
                args.after_date.as_deref().and_then(parse_iso_date),
                args.before_date.as_deref().and_then(parse_iso_date),
                args.all,
            )?;
            expand_graph(repo, &scope, resolved, m.id)?
        } else {
            (Vec::new(), false)
        };
        if !nodes.is_empty() {
            text.push_str("\n\n## Related knowledge (graph, depth ≤ 2)");
            if graph_truncated {
                text.push_str(" (truncated: bounded traversal)");
            }
        }
        for (gm, depth, score) in &nodes {
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
        // Minimal shape extension (Wave B5a): the frozen related_graph
        // items are untouched; a top-level graph_truncated flag carries the
        // traversal budget outcome, mirroring the citations_truncated
        // precedent in recall explanations.
        if args.expand_graph {
            data["graph_truncated"] = json!(graph_truncated);
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
        let mut accessed: Vec<EntityId> = Vec::new();
        let mut found: Vec<Memory> = Vec::new();
        for id in ids {
            let m = match resolve_id(repo, id) {
                Ok(eid) => repo.get_memories(&[eid])?.first().cloned(),
                Err(_) => None,
            };
            // Stable scopes never resolve foreign rows by ID (no leak:
            // out-of-scope renders exactly like a missing fragment).
            let m = m.filter(|m| in_scope(m));
            if let Some(m) = m {
                accessed.push(m.id);
                found.push(m);
            }
        }
        record_access(disp, envelope, &accessed, args.context.as_deref())?;
        let legacy_ids: Vec<String> = found.iter().map(|m| repo.legacy_id(m)).collect();
        attribute_read_to_session(disp, envelope, admitted, &legacy_ids)?;
        let now = disp.clock().now_millis();
        let (pairs, relations) = plan_coread(envelope, resolved, &found, now)?;
        if !pairs.is_empty() || !relations.is_empty() {
            disp.repo()
                .apply_coread_links(admitted, &pairs, &relations)?;
        }
        // Re-fetch post-access (and post-link) records so detail reflects
        // boosted counters and updated associatedWith. Shapes unchanged,
        // input order preserved.
        let fresh: Vec<Memory> = repo.get_memories(&accessed)?;
        let fresh_by_id: std::collections::BTreeMap<EntityId, &Memory> =
            fresh.iter().map(|m| (m.id, m)).collect();
        let resolver = |eid: &EntityId| legacy_id_of_from_id(repo, *eid);
        let mut results: Vec<String> = Vec::new();
        let mut fragments: Vec<Value> = Vec::new();
        for id in ids {
            let eid_opt = resolve_id(repo, id).ok();
            if let Some(eid) = eid_opt
                && let Some(m) = fresh_by_id.get(&eid)
                && in_scope(m)
            {
                fragments.push(fragment_detail_json(repo, m));
                results.push(render_detail(&legacy_id_of(repo, m), m, &resolver));
            } else {
                results.push(format!("Fragment [{id}] not found."));
            }
        }
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

    // Browse or query mode: one validated window, paged by the retrieval
    // choke point (recall_browse) which owns the total. Totals never come
    // from a truncated vector.
    let browsed = recall_browse(disp, args, resolved)?;
    let (pairs, total, offset, has_more, next_offset) = (
        &browsed.page.items,
        browsed.page.total,
        browsed.page.offset,
        browsed.page.has_more,
        browsed.page.next_offset,
    );
    let method = browsed.method;

    // Build the recall explanation from the pre-boost page (upstream captures
    // provenance before boostOnAccess mutates confidence/access counters).
    // Scores are the values that determined the served order on each path.
    let selections: Vec<RecallSelection> = pairs
        .iter()
        .enumerate()
        .map(|(i, (m, s))| RecallSelection {
            memory: m,
            method,
            rank: Some(offset.saturating_add(i).saturating_add(1) as u64),
            score: Some(*s),
            graph: None,
        })
        .collect();
    let page_mem: Vec<Memory> = pairs.iter().map(|(m, _)| m.clone()).collect();
    let page = &page_mem;
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
    let legacy_ids: Vec<String> = page.iter().map(|m| repo.legacy_id(m)).collect();
    attribute_read_to_session(disp, envelope, admitted, &legacy_ids)?;
    let now = disp.clock().now_millis();
    let (pairs, relations) = plan_coread(envelope, resolved, page, now)?;
    if !pairs.is_empty() || !relations.is_empty() {
        disp.repo()
            .apply_coread_links(admitted, &pairs, &relations)?;
    }
    // Re-fetch post-access records so summary confidence values are
    // post-boost. Explain stays pre-boost (provenance). Shapes unchanged.
    let fresh_page: Vec<Memory> = repo.get_memories(&accessed)?;
    let fresh_by_id: std::collections::BTreeMap<EntityId, &Memory> =
        fresh_page.iter().map(|m| (m.id, m)).collect();
    let ordered_fresh: Vec<&Memory> = page
        .iter()
        .filter_map(|m| fresh_by_id.get(&m.id).copied())
        .collect();

    let scope_info = if args.all {
        "all projects".to_string()
    } else {
        args.project.clone().unwrap_or_else(|| "global".to_string())
    };
    let lid = |m: &Memory| legacy_id_of(repo, m);
    let render_page: Vec<Memory> = ordered_fresh.into_iter().cloned().collect();
    let mut text = render_summary_index(&render_page, &scope_info, &lid);
    if has_more {
        text.push_str(&format!(
            "\nShowing {} of {} fragments (offset {}). Pass offset={} for the next page, or use the query parameter to search.",
            render_page.len(),
            total,
            offset,
            next_offset.unwrap_or(offset),
        ));
    } else if args.query.is_none() {
        text.push_str(&format!(
            "\nShowing top {} fragments (ranked by relevance). Use query parameter to search, or id for a specific fragment.",
            render_page.len()
        ));
    }

    if let Some(exp) = &recall_explanation {
        text.push_str(&format_recall_explanation(exp));
    }

    let fragments: Vec<Value> = render_page
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
        "count": render_page.len(),
        "total": total,
        "fragments": fragments,
        "has_more": has_more,
        "next_offset": next_offset,
    });
    if let Some(exp) = recall_explanation {
        data["recall_explanation"] = json!(exp);
    }
    Ok(format_result(text, data, format))
}

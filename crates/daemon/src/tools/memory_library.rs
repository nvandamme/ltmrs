//! memory_library tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::DomainPayload;
use ltmrs_compat::lemma::tool_args::MemoryLibraryArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use super::format_result;
use super::ids::legacy_id_of;

pub(crate) fn exec_memory_library(
    disp: &Dispatcher,
    args: &MemoryLibraryArgs,
) -> DomainResult<DomainPayload> {
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

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::ToolArgs;

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

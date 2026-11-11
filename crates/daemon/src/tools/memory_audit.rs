//! memory_audit tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::DomainPayload;
use ltmrs_compat::lemma::tool_args::MemoryAuditArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;
use ltmrs_domain::relation::Relation;
use serde_json::{Value, json};
use std::collections::BTreeSet;

use super::format_result;
use super::ids::{legacy_id_of, legacy_id_of_from_id};

/// Audit memory integrity (upstream `auditMemory` semantics).
/// `legacy_id_of` maps a memory to its display ID; `assoc_exists` reports
/// whether a legacy associated-with ID resolves to a live memory;
/// `legacy_id_for` resolves an entity ID to its display ID (for relation edges).
pub(crate) fn audit_memory<F, G, H>(
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
pub(crate) fn format_audit_report(result: &Value) -> String {
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

pub(crate) fn exec_memory_audit(
    disp: &Dispatcher,
    args: &MemoryAuditArgs,
    resolved: &ltmrs_service::projects::ResolvedScope,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let export = disp.scan_snapshot()?;
    // Stable-scope isolation: audit only in-scope rows, like memory_library
    // fragments. Legacy/global scopes stay permissive via eligibility.
    let scope = super::effective_for_read(resolved, None, None, None, false)?;
    let memories: Vec<ltmrs_domain::memory::Memory> = export
        .memories
        .iter()
        .filter(|m| scope.is_eligible(m))
        .cloned()
        .collect();
    let ids: std::collections::BTreeSet<ltmrs_domain::id::EntityId> =
        memories.iter().map(|m| m.id).collect();
    let relations: Vec<ltmrs_domain::relation::Relation> = export
        .relations
        .iter()
        .filter(|r| ids.contains(&r.source) && ids.contains(&r.target))
        .cloned()
        .collect();

    let result = audit_memory(
        &memories,
        &relations,
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
#[cfg(test)]
const GOLDEN_AUDIT_TEXT: &str =
    "## Memory Audit\nTotal fragments: 1 | Issues: 0\nAll clear — no issues found.\n";

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::ToolArgs;
#[cfg(test)]
use ltmrs_domain::memory::Instant;
#[cfg(test)]
use uuid::Uuid;

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
/// Stable-scope isolation (FuncReview critical): a foreign stable row must
/// not surface in audit issues under identity A.
#[test]
fn memory_audit_excludes_foreign_stable_row() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, _id_a) = generate_project_id("shop");
    let (_full_b, id_b) = generate_project_id("shop");
    // A writes under its identity (registers A).
    let out = add_with(
        &disp,
        1,
        "## Shop A\n\n### Context\nA-only fragment.",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&out));
    // B's row seeded direct (bypasses the add gate like the conflict fixture).
    let mut foreign = make_memory(
        "foreign-b",
        Some("shop"),
        "ai",
        0.5,
        "B-only fragment with an AQWERTYUIOP edge",
    );
    foreign.project_id = Some(id_b);
    disp.repo().put_memory_direct(&foreign).unwrap();
    // Audit under A must not name the foreign row.
    let tool = ToolArgs::MemoryAudit(MemoryAuditArgs::default());
    let env = tool_call_with(
        2,
        tool.clone(),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let result = run(&disp, &env, &tool);
    assert!(!result_is_error(&result));
    // Healthy rows surface via total_fragments even when no issue names
    // them: under A the count must be 1, not 2.
    let structured = result_structured(&result).unwrap();
    assert_eq!(
        structured["total_fragments"].as_u64().unwrap(),
        1,
        "audit counted foreign stable row: {}",
        result_text(&result)
    );
}
/// Snapshot freshness: a write between two audits must be visible in the
/// second audit (no stale cached snapshot served across a commit).
#[test]
fn memory_audit_sees_write_between_calls() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(&disp, 1, "## First\n\n### Context\nFirst fragment.");
    let tool = ToolArgs::MemoryAudit(MemoryAuditArgs::default());
    let env1 = tool_call(2, tool.clone());
    let first = run(&disp, &env1, &tool);
    assert_eq!(
        result_structured(&first).unwrap()["total_fragments"]
            .as_u64()
            .unwrap(),
        1
    );
    add_fragment(&disp, 3, "## Second\n\n### Context\nSecond fragment.");
    let env2 = tool_call(4, tool.clone());
    let second = run(&disp, &env2, &tool);
    assert_eq!(
        result_structured(&second).unwrap()["total_fragments"]
            .as_u64()
            .unwrap(),
        2,
        "second audit must see the committed write"
    );
}

/// Scan budget: the tracker gate trips just over the row limit so an
/// oversized store refuses visibly instead of decoding unboundedly.
/// Read retry policy (T-CONC-03): the same audit envelope twice
/// returns the identical response (reads are side-effect-free).
#[test]
fn memory_audit_same_envelope_replays_identical() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(&disp, 1, "## Audit RT\n\n### Context\nRetry fixture.");
    let tool = ToolArgs::MemoryAudit(MemoryAuditArgs::default());
    let env = tool_call(2, tool.clone());
    let first = run(&disp, &env, &tool);
    let second = run(&disp, &env, &tool);
    assert!(!result_is_error(&first));
    assert_eq!(result_text(&second), result_text(&first));
}

#[test]
fn scan_budget_gate_trips_over_limit() {
    use crate::limits::{QuotaTracker, ResourceLimits};
    let quotas = QuotaTracker::new(ResourceLimits::default());
    assert!(quotas.check_scan_budget(10_000).is_ok());
    let err = quotas.check_scan_budget(10_001).unwrap_err();
    assert_eq!(err, crate::limits::QuotaError::ScanBudgetExceeded);
}

/// Configured scan caps are honored: a 2-row store refuses under a 1-row
/// dispatcher cap and serves under the default cap (not the hardcoded
/// default — the configured value wins).
#[test]
fn scan_snapshot_honors_configured_budget() {
    use crate::dispatcher::Dispatcher;
    use crate::registry::FrontendRegistry;
    use ltmrs_domain::clock::FrozenClock;
    use std::sync::Arc;
    fn dispatcher_with_cap(cap: usize) -> Dispatcher {
        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            ltmrs_service::repository::CanonicalRepository::open_with_clock(
                dir.path().to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        std::mem::forget(dir);
        Dispatcher::new(repo, FrontendRegistry::new(), clock).with_scan_budget(cap)
    }
    fn seed_two(disp: &Dispatcher) {
        for key in [11u128, 22u128] {
            let mut m = make_memory(&format!("m{key}"), None, "ai", 0.5, "Budget probe.");
            m.id = ltmrs_domain::id::EntityId::new(uuid::Uuid::from_u128(key));
            disp.repo().put_memory_direct(&m).unwrap();
        }
    }
    let tight = dispatcher_with_cap(1);
    seed_two(&tight);
    let err = tight.scan_snapshot().unwrap_err();
    assert!(
        err.message.contains("scan budget exceeded"),
        "tight cap must refuse, got: {}",
        err.message
    );
    let roomy = dispatcher_with_cap(10_000);
    seed_two(&roomy);
    assert_eq!(roomy.scan_snapshot().unwrap().memories.len(), 2);
}

#[test]
fn audit_memory_matches_upstream() {
    let raw = std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
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
                                    ltmrs_domain::relation::RelationType::parse(
                                        r["type"].as_str().unwrap_or("related_to"),
                                    )
                                    .unwrap_or(ltmrs_domain::relation::RelationType::RelatedTo),
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
    let raw = std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
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

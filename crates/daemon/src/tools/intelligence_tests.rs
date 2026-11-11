//! intelligence tool tests (moved verbatim from `tools.rs`).

use super::test_support::*;
use ltmrs_compat::lemma::tool_args::{
    ConflictScanArgs, ProactiveAnalysisArgs, ProjectAnalyticsArgs, ToolArgs,
};

#[test]
fn conflict_scan_detects_opposing_fragments() {
    let (disp, _dir) = test_dispatcher();
    // Shared topic (redis/caching/session/data) with opposing negation
    // ("always" vs "never") — distinct enough to pass add-dedup, overlapping
    // enough for the conflict heuristic to fire. Seeded repository-direct:
    // the B3b tool gate refuses a conflicting live add, so the scan
    // fixture bypasses the gate (pre-existing rows must still scan).
    for (id, frag) in [
        (
            "opposing-a",
            "Always use Redis for caching session data in the API layer.",
        ),
        (
            "opposing-b",
            "Never use Redis for caching session data; pick Memcached instead.",
        ),
    ] {
        let m = make_memory(id, None, "ai", 0.5, frag);
        disp.repo().put_memory_direct(&m).unwrap();
    }

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
/// Stable-scope isolation (FuncReview critical): conflict pairs, proactive
/// suggestions and project analytics under identity A must not surface
/// foreign stable rows from identity B.
/// Read retry policy: the same scan envelope twice returns the
/// identical response (conflict/proactive/analytics are side-effect-free).
#[test]
fn intelligence_same_envelope_replays_identical() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(&disp, 1, "## Intel RT\n\n### Context\nRetry fixture.");
    for (op, tool) in [
        (2u64, ToolArgs::ConflictScan(ConflictScanArgs::default())),
        (
            3u64,
            ToolArgs::ProactiveAnalysis(ProactiveAnalysisArgs::default()),
        ),
        (
            4u64,
            ToolArgs::ProjectAnalytics(ProjectAnalyticsArgs::default()),
        ),
    ] {
        let env = tool_call(op, tool.clone());
        let first = run(&disp, &env, &tool);
        let second = run(&disp, &env, &tool);
        assert!(!result_is_error(&first), "first failed for {tool:?}");
        assert_eq!(
            result_text(&second),
            result_text(&first),
            "retry mismatch for {tool:?}"
        );
    }
}

#[test]
fn conflict_scan_excludes_foreign_stable_row() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, id_a) = generate_project_id("shop");
    let (_full_b, id_b) = generate_project_id("shop");
    assert_ne!(id_a, id_b);
    let out = add_with(
        &disp,
        1,
        "## Shop A\n\n### Context\nA-only fragment.",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&out));
    // B's opposing pair seeded direct under B's stable id: if the scan
    // input leaks across the boundary, the pair fires and names B's rows.
    for (id, frag) in [
        (
            "opposing-a",
            "Always use Redis for caching session data in the API layer.",
        ),
        (
            "opposing-b",
            "Never use Redis for caching session data; pick Memcached instead.",
        ),
    ] {
        let mut m = make_memory(id, Some("shop"), "ai", 0.5, frag);
        m.project_id = Some(id_b);
        disp.repo().put_memory_direct(&m).unwrap();
    }
    let tool = ToolArgs::ConflictScan(ConflictScanArgs::default());
    let env = tool_call_with(
        2,
        tool.clone(),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let result = run(&disp, &env, &tool);
    assert!(!result_is_error(&result));
    // B's pair must not fire under A: with correct scoping the scan input
    // is A's row only, so no pair exists. A fired pair names B's rows.
    assert_eq!(
        result_structured(&result).unwrap()["count"]
            .as_u64()
            .unwrap(),
        0,
        "conflict pair fired on foreign stable rows under identity A: {}",
        result_text(&result)
    );
}

#[test]
fn proactive_analysis_excludes_foreign_stable_row() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, _id_a) = generate_project_id("shop");
    let (_full_b, id_b) = generate_project_id("shop");
    let out = add_with(
        &disp,
        1,
        "## Shop A\n\n### Context\nA-only fragment.",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&out));
    let mut foreign = make_memory(
        "foreign-b",
        Some("shop"),
        "ai",
        0.5,
        "B-only fragment with a QWERTZXCVBN marker.",
    );
    foreign.project_id = Some(id_b);
    disp.repo().put_memory_direct(&foreign).unwrap();
    let tool = ToolArgs::ProactiveAnalysis(ProactiveAnalysisArgs::default());
    let env = tool_call_with(
        2,
        tool.clone(),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let result = run(&disp, &env, &tool);
    assert!(!result_is_error(&result));
    assert!(
        !result_text(&result).contains("QWERTZXCVBN"),
        "foreign stable row leaked into proactive_analysis under identity A"
    );
    assert_eq!(
        result_structured(&result).unwrap()["analyzed_memories"]
            .as_u64()
            .unwrap(),
        1,
        "proactive scanned foreign stable row"
    );
}

#[test]
fn project_analytics_excludes_foreign_stable_project() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, _id_a) = generate_project_id("shop");
    let (_full_b, id_b) = generate_project_id("shop");
    let out = add_with(
        &disp,
        1,
        "## Shop A\n\n### Context\nA-only fragment.",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&out));
    let mut foreign = make_memory(
        "foreign-b",
        Some("shop"),
        "ai",
        0.5,
        "B-only fragment with a POIUYTREWQ marker.",
    );
    foreign.project_id = Some(id_b);
    disp.repo().put_memory_direct(&foreign).unwrap();
    let tool = ToolArgs::ProjectAnalytics(ProjectAnalyticsArgs {
        project: None,
        ..Default::default()
    });
    let env = tool_call_with(
        2,
        tool.clone(),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let result = run(&disp, &env, &tool);
    assert!(!result_is_error(&result));
    // All-projects overview under A must count only A's memory: the shared
    // slug collapses to one row with total_memories == 1, not 2.
    let structured = result_structured(&result).unwrap();
    let projects = structured["projects"].as_array().unwrap();
    assert_eq!(
        projects.len(),
        1,
        "expected one scoped project row: {projects:?}"
    );
    assert_eq!(
        projects[0]["total_memories"].as_u64().unwrap(),
        1,
        "analytics counted foreign stable row: {}",
        result_text(&result)
    );
}

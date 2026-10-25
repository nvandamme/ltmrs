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

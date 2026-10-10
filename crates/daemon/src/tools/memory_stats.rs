//! memory_stats tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::DomainPayload;
use ltmrs_compat::lemma::tool_args::MemoryStatsArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::memory::Memory;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::format_result;

/// Calculate memory statistics (upstream `getMemoryStats` SQL semantics —
/// the real memory_stats tool path, NOT the dead-code pure `calculateStats`).
/// - No project → all fragments. Project → strict case-insensitive match on
///   the stored project; global fragments are EXCLUDED.
/// - Globals are labeled `(global)` (SQL COALESCE).
/// - avg_confidence is the raw mean (no rounding).
/// - low/high_confidence are null (not 0) on an empty store (SQL SUM).
/// - by_source/by_project keys in byte order (SQLite GROUP BY, BINARY collation).
pub(crate) fn calculate_stats(memories: &[Memory], project: Option<&str>) -> Value {
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
pub(crate) fn format_stats(stats: &Value) -> String {
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
pub(crate) fn exec_memory_stats(
    disp: &Dispatcher,
    args: &MemoryStatsArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let export = repo.export_snapshot()?;

    let stats = calculate_stats(&export.memories, args.project.as_deref());
    let text = format_stats(&stats);

    Ok(format_result(text, stats, args.response_format))
}

#[cfg(test)]
const GOLDEN_STATS_TEXT: &str = "## Memory Stats\nTotal: 2 fragments | Avg confidence: 1\nHigh confidence (>0.8): 2 | Low (<0.3): 0\nSources: ai: 2\nProjects: (global): 2\n";

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::ToolArgs;

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
    use ltmrs_compat::lemma::tool_args::MemoryAddArgs;

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

#[test]
fn calculate_stats_matches_upstream() {
    let raw = std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
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
    let raw = std::fs::read_to_string(pure_oracle_path()).expect("pure_functions.json must exist");
    let oracle: Value = serde_json::from_str(&raw).expect("valid JSON oracle");
    for (i, case) in oracle["formatStats"].as_array().unwrap().iter().enumerate() {
        let stats = &case["input"];
        let actual = format_stats(stats);
        let expected = case["output"].as_str().unwrap();
        assert_eq!(actual, expected, "format_stats mismatch on case {i}");
    }
}

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

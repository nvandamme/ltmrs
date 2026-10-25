//! guide_get tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::DomainPayload;
use ltmrs_compat::lemma::tool_args::GuideGetArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::guide::Guide;
use serde_json::json;

use super::format_result;

pub(crate) fn exec_guide_get(
    disp: &Dispatcher,
    args: &GuideGetArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;
    let guides = repo.get_guides()?;

    // Task-based suggestions.
    if let Some(task) = &args.task {
        let mut suggestions = super::guide_catalog::suggest_guides(task, &guides);
        // Dense candidate proposal (WP-09): appends positively-similar
        // catalog guides the token path missed. Any dense failure adds
        // nothing, keeping the token-only output byte-identical.
        if let Some(backend) = disp.search() {
            let seen: std::collections::BTreeSet<String> =
                suggestions.iter().map(|s| s.guide.clone()).collect();
            suggestions.extend(super::guide_catalog::suggest_guides_dense(
                backend, task, &guides, &seen,
            ));
        }
        let text = super::guide_catalog::format_guide_suggestions(&suggestions);
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
                super::guide_render::format_guide_detail(&g),
                json!({
                    "count": 1,
                    "guides": [super::guide_render::guide_json(&g)],
                    "guide": super::guide_render::guide_json(&g),
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
        "guides": filtered.iter().map(|g| super::guide_render::guide_json(g)).collect::<Vec<_>>(),
        "guide": null,
    });
    Ok(format_result(text, data, format))
}

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::{GuideCreateArgs, ToolArgs};

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

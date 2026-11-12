//! reflect tool (Slice G1b): omp-compat server-side synthesis.
//!
//! Query (+ optional context) over the Hindsight bank through the bridge.
//! Read-only and best-effort: empty synthesis when the bridge is disabled
//! or down (never a failure — mirrors omp's "No relevant information"
//! text path, but honest about the cause).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::ReflectArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_service::projects::ResolvedScope;

use super::{err_result, ok_result};

pub(crate) fn exec_reflect(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    args: &ReflectArgs,
    resolved: &ResolvedScope,
) -> DomainResult<DomainPayload> {
    if args.query.trim().is_empty() {
        return Ok(err_result(
            "reflect needs `query` (empty questions synthesize nothing)",
        ));
    }
    let query = match args.context.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(ctx) => format!("{}\n\nAdditional context:\n{ctx}", args.query.trim()),
        None => args.query.trim().to_string(),
    };
    let project = resolved.name.clone().or_else(|| resolved.legacy.clone());
    let text = disp.bridge_reflect_scoped(&query, project.as_deref());
    if text.trim().is_empty() {
        return Ok(ok_result(
            "No relevant information found to reflect on (bridge disabled or unreachable)."
                .to_string(),
            serde_json::json!({"success": true, "synthesized": false}),
        ));
    }
    Ok(ok_result(
        text,
        serde_json::json!({"success": true, "synthesized": true}),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use ltmrs_compat::lemma::tool_args::{ReflectArgs, ToolArgs};

    fn reflect_tool(query: &str) -> ToolArgs {
        ToolArgs::Reflect(ReflectArgs {
            query: query.to_string(),
            context: None,
        })
    }

    /// G1b: empty query rejects loudly (never a silent empty synthesis).
    #[test]
    fn reflect_empty_query_rejects() {
        let (disp, _dir) = test_dispatcher();
        let tool = reflect_tool("   ");
        let out = run(&disp, &tool_call(1, tool.clone()), &tool);
        assert!(result_is_error(&out));
        assert!(
            result_text(&out).contains("needs `query`"),
            "got: {}",
            result_text(&out)
        );
    }

    /// G1b: bridge disabled degrades to honest "no information" (never a failure).
    #[test]
    fn reflect_without_bridge_degrades_honestly() {
        let (disp, _dir) = test_dispatcher();
        let tool = reflect_tool("What did we decide about caching?");
        let out = run(&disp, &tool_call(1, tool.clone()), &tool);
        assert!(
            !result_is_error(&out),
            "degraded reflect must not fail: {}",
            result_text(&out)
        );
        assert!(result_structured(&out).unwrap()["synthesized"] == serde_json::json!(false));
    }
}

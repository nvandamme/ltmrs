//! learn tool (Slice G3): omp-compat lesson capture + optional managed skill.
//!
//! - `memory` (+ optional `context`) → lesson-typed `memory_add`
//!   (`fragment_type: "lesson"`; newest-first/dedup/caps already live in
//!   the add path, secret redaction included).
//! - Optional `guide` (+ `guide_description`, `guide_body`) → `guide_create`
//!   AFTER the lesson persists (omp order: skill failure is a partial
//!   outcome, never a rollback of the stored lesson).
//!
//! Empty lessons reject loudly (never a silent no-op).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::{GuideCreateArgs, LearnArgs, MemoryAddArgs};
use ltmrs_domain::command::DomainResult;
use ltmrs_service::projects::ResolvedScope;
use ltmrs_service::repository::AdmittedScope;

use super::{err_result, ok_result};

pub(crate) fn exec_learn(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &LearnArgs,
    resolved: &ResolvedScope,
) -> DomainResult<DomainPayload> {
    if args.memory.trim().is_empty() {
        return Ok(err_result(
            "learn needs `memory` (empty lessons are not stored)",
        ));
    }
    // Lesson first (omp order): a lesson-typed add through the canonical path.
    let fragment = match args.context.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(ctx) => format!("{}\n\nContext: {ctx}", args.memory.trim()),
        None => args.memory.trim().to_string(),
    };
    let lesson = super::memory_add::exec_memory_add(
        disp,
        envelope,
        admitted,
        &MemoryAddArgs {
            fragment,
            fragment_type: Some("lesson".to_string()),
            ..Default::default()
        },
        resolved,
    )?;
    // No guide requested: the lesson result stands alone.
    let (Some(guide), Some(body)) = (
        args.guide.as_deref().filter(|s| !s.trim().is_empty()),
        args.guide_body.as_deref().filter(|s| !s.trim().is_empty()),
    ) else {
        return Ok(lesson);
    };
    // Skill second: failure here is partial (lesson already stored).
    let skill = super::guide_create::exec_guide_create(
        disp,
        envelope,
        admitted,
        &GuideCreateArgs {
            guide: guide.to_string(),
            category: "lesson".to_string(),
            description: args
                .guide_description
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Lesson-captured procedure".to_string()),
            contexts: Vec::new(),
            learnings: vec![body.to_string()],
        },
    );
    match skill {
        Ok(_) => Ok(ok_result(
            "Lesson stored. Created managed skill.".to_string(),
            serde_json::json!({"success": true, "skill": guide}),
        )),
        Err(e) => Ok(ok_result(
            format!(
                "Lesson stored, but the managed skill could not be written: {}",
                e.message
            ),
            serde_json::json!({"success": true, "skill": null, "partial": e.message}),
        )),
    }
}

#[cfg(test)]
mod tests {

    use super::super::test_support::*;
    use ltmrs_compat::lemma::tool_args::{LearnArgs, ToolArgs};

    fn learn_tool(memory: &str, guide: Option<&str>, body: Option<&str>) -> ToolArgs {
        ToolArgs::Learn(LearnArgs {
            memory: memory.to_string(),
            context: None,
            guide: guide.map(|s| s.to_string()),
            guide_description: None,
            guide_body: body.map(|s| s.to_string()),
        })
    }

    /// G3: lesson-only learn stores a lesson-typed memory.
    #[test]
    fn learn_stores_lesson() {
        let (disp, _dir) = test_dispatcher();
        let tool = learn_tool("Always quote identifiers in SQL.", None, None);
        let out = run(&disp, &tool_call(1, tool.clone()), &tool);
        assert!(
            !result_is_error(&out),
            "learn failed: {}",
            result_text(&out)
        );
        assert!(result_structured(&out).unwrap()["id"].is_string());
    }

    /// G3: empty lesson rejects loudly (never a silent no-op).
    #[test]
    fn learn_empty_memory_rejects() {
        let (disp, _dir) = test_dispatcher();
        let tool = learn_tool("   ", None, None);
        let out = run(&disp, &tool_call(1, tool.clone()), &tool);
        assert!(result_is_error(&out));
        assert!(
            result_text(&out).contains("needs `memory`"),
            "got: {}",
            result_text(&out)
        );
    }

    /// G3: lesson + guide stores both (skill second, partial on failure).
    #[test]
    fn learn_with_guide_stores_both() {
        let (disp, _dir) = test_dispatcher();
        let tool = learn_tool(
            "Quote SQL identifiers.",
            Some("sql-quoting"),
            Some("# SQL quoting\n\nAlways quote."),
        );
        let out = run(&disp, &tool_call(1, tool.clone()), &tool);
        assert!(
            !result_is_error(&out),
            "learn+guide failed: {}",
            result_text(&out)
        );
        let structured = result_structured(&out).unwrap();
        assert_eq!(structured["skill"], serde_json::json!("sql-quoting"));
    }
}

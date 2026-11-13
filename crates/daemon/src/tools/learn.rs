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

    /// Transitive legacy oracle (alias equivalence): `learn` without a
    /// guide stores through the SAME `exec_memory_add` the native
    /// `memory_add` uses, so upstream-verified add behavior is inherited
    /// iff the alias forwards identical args. Twin fixtures, alias on one
    /// side, native lesson-typed add on the other, then compare canonical
    /// effects. No upstream counterpart is invented.
    #[test]
    fn learn_without_guide_matches_native_add_effects() {
        use ltmrs_compat::lemma::tool_args::{MemoryAddArgs, ToolArgs};
        let (disp_a, _d1) = test_dispatcher();
        let (disp_b, _d2) = test_dispatcher();
        let learn = learn_tool("Prefer explicit error returns.", None, None);
        let env_a = tool_call(1, learn.clone());
        let out_a = run(&disp_a, &env_a, &learn);
        assert!(
            !result_is_error(&out_a),
            "learn failed: {}",
            result_text(&out_a)
        );
        let learn_id = result_structured(&out_a).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let native = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "Prefer explicit error returns.".to_string(),
            fragment_type: Some("lesson".to_string()),
            ..Default::default()
        });
        let env_b = tool_call(1, native.clone());
        let out_b = run(&disp_b, &env_b, &native);
        assert!(
            !result_is_error(&out_b),
            "native add failed: {}",
            result_text(&out_b)
        );
        let snap = |d: &crate::dispatcher::Dispatcher| {
            let m = d.repo().export_full().unwrap().memories;
            assert_eq!(m.len(), 1);
            (m[0].fragment.clone(), m[0].fragment_type, m[0].confidence)
        };
        // Same fragment text, same lesson typing, same earned confidence.
        let (fa, ta, ca) = snap(&disp_a);
        let (fb, tb, cb) = snap(&disp_b);
        assert_eq!(fa, fb, "alias must store the same fragment as native");
        assert_eq!(ta, tb, "alias must type the lesson like native");
        assert!(
            (ca - cb).abs() < f64::EPSILON,
            "confidence must match native"
        );
        let _ = learn_id;
    }
}

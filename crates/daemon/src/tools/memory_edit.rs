//! memory_edit tool (Slice omp P2): omp tri-op alias over canonical ops.
//!
//! - `update` + content → `memory_update` (fragment patch).
//! - `update` with importance only → loud reject (confidence is earned).
//! - `forget` → `memory_forget`.
//! - `invalidate` (+ optional replacement_id) → `memory_forget` with
//!   `invalidate=true` (tombstone, never delete).
//!
//! No read-only-fact guard: every in-scope fragment is editable.

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::{
    MemoryEditArgs, MemoryForgetArgs, MemoryUpdateArgs, ToolArgs,
};
use ltmrs_domain::command::DomainResult;
use ltmrs_service::projects::ResolvedScope;
use ltmrs_service::repository::AdmittedScope;

use super::{err_result, ok_result};

pub(crate) fn exec_memory_edit(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &MemoryEditArgs,
    resolved: &ResolvedScope,
) -> DomainResult<DomainPayload> {
    match args.op.as_str() {
        "update" => {
            let Some(content) = args.content.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Ok(err_result(
                    "memory_edit update needs `content` (importance is earned in ltmrs, never set)",
                ));
            };
            super::memory_update::exec_memory_update(
                disp,
                envelope,
                admitted,
                &MemoryUpdateArgs {
                    id: args.id.clone(),
                    title: None,
                    fragment: Some(content.to_string()),
                    confidence: None,
                },
                resolved,
            )
        }
        "forget" => super::memory_forget::exec_memory_forget(
            disp,
            envelope,
            admitted,
            &MemoryForgetArgs {
                id: args.id.clone(),
                consolidate: false,
                invalidate: false,
            },
            resolved,
        ),
        "invalidate" => super::memory_forget::exec_memory_forget(
            disp,
            envelope,
            admitted,
            &MemoryForgetArgs {
                id: args.id.clone(),
                consolidate: false,
                invalidate: true,
            },
            resolved,
        ),
        other => Ok(err_result(&format!(
            "unknown memory_edit op '{other}' (want update | forget | invalidate)"
        ))),
    }
}

/// Route helper: build the underlying ToolArgs for tests.
#[allow(dead_code)]
pub(crate) fn routed_args(args: &MemoryEditArgs) -> Option<ToolArgs> {
    match args.op.as_str() {
        "update" => args.content.as_deref().map(|c| {
            ToolArgs::MemoryUpdate(MemoryUpdateArgs {
                id: args.id.clone(),
                title: None,
                fragment: Some(c.to_string()),
                confidence: None,
            })
        }),
        "forget" => Some(ToolArgs::MemoryForget(MemoryForgetArgs {
            id: args.id.clone(),
            consolidate: false,
            invalidate: false,
        })),
        "invalidate" => Some(ToolArgs::MemoryForget(MemoryForgetArgs {
            id: args.id.clone(),
            consolidate: false,
            invalidate: true,
        })),
        _ => None,
    }
}

/// Human-readable result passthrough marker (kept for the CLI surface).
#[allow(dead_code)]
pub(crate) fn ok_result_text(text: String) -> DomainPayload {
    ok_result(text, serde_json::json!({"success": true}))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use ltmrs_compat::lemma::tool_args::ToolArgs;

    fn edit(op: &str, id: &str, content: Option<&str>) -> ToolArgs {
        ToolArgs::MemoryEdit(ltmrs_compat::lemma::tool_args::MemoryEditArgs {
            op: op.to_string(),
            id: id.to_string(),
            content: content.map(|s| s.to_string()),
            importance: None,
            replacement_id: None,
        })
    }

    /// P2: update patches content through memory_update.
    #[test]
    fn edit_update_patches_content() {
        let (disp, _dir) = test_dispatcher();
        let id = ident(None, None, None, false);
        let add = run(
            &disp,
            &tool_call_with(
                1,
                ToolArgs::MemoryAdd(ltmrs_compat::lemma::tool_args::MemoryAddArgs {
                    fragment: "## Old\n\n### Context\nStale.".to_string(),
                    ..Default::default()
                }),
                id.clone(),
            ),
            &ToolArgs::MemoryAdd(ltmrs_compat::lemma::tool_args::MemoryAddArgs {
                fragment: "## Old\n\n### Context\nStale.".to_string(),
                ..Default::default()
            }),
        );
        assert!(!result_is_error(&add));
        let mid = result_structured(&add).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let out = run(
            &disp,
            &tool_call_with(
                2,
                edit("update", &mid, Some("## New\n\n### Context\nFresh.")),
                id,
            ),
            &edit("update", &mid, Some("## New\n\n### Context\nFresh.")),
        );
        assert!(
            !result_is_error(&out),
            "update failed: {}",
            result_text(&out)
        );
        assert!(result_text(&out).contains("Updated fragment"));
    }

    /// P2: unknown op rejects loudly (never silent).
    #[test]
    fn edit_unknown_op_rejects() {
        let (disp, _dir) = test_dispatcher();
        let id = ident(None, None, None, false);
        let out = run(
            &disp,
            &tool_call_with(1, edit("vaporize", "m1", None), id.clone()),
            &edit("vaporize", "m1", None),
        );
        assert!(result_is_error(&out));
        assert!(
            result_text(&out).contains("unknown memory_edit op"),
            "got: {}",
            result_text(&out)
        );
    }

    /// P2: importance-only update rejects loudly (confidence is earned).
    #[test]
    fn edit_importance_only_rejects() {
        use ltmrs_compat::lemma::tool_args::MemoryEditArgs;
        let (disp, _dir) = test_dispatcher();
        let id = ident(None, None, None, false);
        let tool = ToolArgs::MemoryEdit(MemoryEditArgs {
            op: "update".to_string(),
            id: "m1".to_string(),
            content: None,
            importance: Some(0.9),
            replacement_id: None,
        });
        let out = run(&disp, &tool_call_with(1, tool.clone(), id), &tool);
        assert!(result_is_error(&out));
        assert!(
            result_text(&out).contains("importance is earned"),
            "got: {}",
            result_text(&out)
        );
    }

    /// P2: forget maps through; unknown id errors loudly.
    #[test]
    fn edit_forget_unknown_id_errors() {
        let (disp, _dir) = test_dispatcher();
        let id = ident(None, None, None, false);
        let out = run(
            &disp,
            &tool_call_with(1, edit("forget", "m-does-not-exist", None), id.clone()),
            &edit("forget", "m-does-not-exist", None),
        );
        assert!(result_is_error(&out), "must not silently succeed");
    }

    /// P2: invalidate maps to the archive path (tombstone, never delete).
    #[test]
    fn edit_invalidate_archives() {
        let (disp, _dir) = test_dispatcher();
        let id = ident(None, None, None, false);
        let add = run(
            &disp,
            &tool_call_with(
                1,
                ToolArgs::MemoryAdd(ltmrs_compat::lemma::tool_args::MemoryAddArgs {
                    fragment: "## Doom\n\n### Context\nTo archive.".to_string(),
                    ..Default::default()
                }),
                id.clone(),
            ),
            &ToolArgs::MemoryAdd(ltmrs_compat::lemma::tool_args::MemoryAddArgs {
                fragment: "## Doom\n\n### Context\nTo archive.".to_string(),
                ..Default::default()
            }),
        );
        let mid = result_structured(&add).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let out = run(
            &disp,
            &tool_call_with(2, edit("invalidate", &mid, None), id),
            &edit("invalidate", &mid, None),
        );
        assert!(
            !result_is_error(&out),
            "invalidate failed: {}",
            result_text(&out)
        );
    }
}

#[cfg(test)]
mod scope_retry_tests {
    use super::super::test_support::*;
    use ltmrs_compat::lemma::tool_args::{
        MemoryEditArgs, MemoryForgetArgs, MemoryUpdateArgs, ToolArgs,
    };
    use ltmrs_domain::project_id::generate_project_id;

    fn edit_tool(op: &str, id: &str, content: Option<&str>) -> ToolArgs {
        ToolArgs::MemoryEdit(MemoryEditArgs {
            op: op.to_string(),
            id: id.to_string(),
            content: content.map(|s| s.to_string()),
            importance: None,
            replacement_id: None,
        })
    }

    /// Scope: update through the alias naming a foreign stable row refuses
    /// as not-found (inherits the memory_update isolation, never leaks).
    #[test]
    fn edit_update_excludes_foreign_stable_row() {
        let (disp, _dir) = test_dispatcher();
        let (full_a, _) = generate_project_id("shop");
        let (full_b, _) = generate_project_id("shop");
        let out_a = add_with(
            &disp,
            1,
            "## Shop A\n\n### Context\nA-only fragment.",
            Some("shop"),
            ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
        );
        assert!(!result_is_error(&out_a));
        let id_a = result_structured(&out_a).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let tool = edit_tool("update", &id_a, Some("hijacked"));
        let env = tool_call_with(
            2,
            tool.clone(),
            ident(Some(&full_b), Some("/tmp/b/shop"), Some("shop"), true),
        );
        let result = run(&disp, &env, &tool);
        assert!(
            result_text(&result).contains("not found"),
            "foreign edit-update must refuse, got: {}",
            result_text(&result)
        );
    }

    /// Scope: forget through the alias naming a foreign stable row refuses
    /// as not-found (inherits the memory_forget isolation).
    #[test]
    fn edit_forget_excludes_foreign_stable_row() {
        let (disp, _dir) = test_dispatcher();
        let (full_a, _) = generate_project_id("shop");
        let (full_b, _) = generate_project_id("shop");
        let out_a = add_with(
            &disp,
            1,
            "## Shop A\n\n### Context\nA-only fragment.",
            Some("shop"),
            ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
        );
        assert!(!result_is_error(&out_a));
        let id_a = result_structured(&out_a).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let tool = edit_tool("forget", &id_a, None);
        let env = tool_call_with(
            2,
            tool.clone(),
            ident(Some(&full_b), Some("/tmp/b/shop"), Some("shop"), true),
        );
        let result = run(&disp, &env, &tool);
        assert!(
            result_text(&result).contains("not found"),
            "foreign edit-forget must refuse, got: {}",
            result_text(&result)
        );
    }

    /// Retry: the same edit-update envelope twice replays the recorded
    /// response byte-identically (inherits the update idempotency, one effect).
    #[test]
    fn edit_update_same_envelope_replays_recorded() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(&disp, 1, "## Replay Edit\n\n### Context\nReplay fixture.");
        let args = edit_tool("update", &id, Some("## Updated\n\n### Context\nReplayed."));
        let env = tool_call(2, args.clone());
        let first = run(&disp, &env, &args);
        assert!(
            !result_is_error(&first),
            "first failed: {}",
            result_text(&first)
        );
        let second = run(&disp, &env, &args);
        assert!(
            !result_is_error(&second),
            "replay failed: {}",
            result_text(&second)
        );
        assert_eq!(
            result_text(&second),
            result_text(&first),
            "replay must be byte-identical"
        );
    }

    /// Straggler guard: the alias struct keeps no behavior of its own — the
    /// match arms delegate to the canonical executors, so stragglers hide
    /// only in the arg mapping. Every op maps to a distinct executor path:
    /// update carries content, forget/forget-clear never set invalidate,
    /// invalidate always sets it.
    #[test]
    fn edit_arms_map_to_distinct_executor_paths() {
        let upd = MemoryEditArgs {
            op: "update".to_string(),
            id: "m1".to_string(),
            content: Some("## F\n\n### Context\nC.".to_string()),
            importance: None,
            replacement_id: None,
        };
        let routed = super::routed_args(&upd).expect("update must route");
        assert!(matches!(
            routed,
            ToolArgs::MemoryUpdate(MemoryUpdateArgs {
                fragment: Some(_),
                ..
            })
        ));
        for op in ["forget", "invalidate"] {
            let arg = MemoryEditArgs {
                op: op.to_string(),
                id: "m1".to_string(),
                content: None,
                importance: None,
                replacement_id: None,
            };
            let routed = super::routed_args(&arg).expect("must route");
            match routed {
                ToolArgs::MemoryForget(MemoryForgetArgs { invalidate, .. }) => {
                    assert_eq!(invalidate, op == "invalidate", "{op} invalidate flag");
                }
                other => panic!("{op} must route to forget, got {other:?}"),
            }
        }
    }

    /// Straggler guard: update without content is a loud error, never a
    /// silent no-op passthrough.
    #[test]
    fn edit_update_without_content_rejects() {
        let (disp, _dir) = test_dispatcher();
        let id = add_fragment(&disp, 1, "## No-op guard\n\n### Context\nFixture.");
        let tool = edit_tool("update", &id, None);
        let out = run(&disp, &tool_call(2, tool.clone()), &tool);
        assert!(result_is_error(&out));
        assert!(
            result_text(&out).contains("needs `content`"),
            "got: {}",
            result_text(&out)
        );
    }

    /// Transitive legacy oracle (alias equivalence): `memory_edit` routes
    /// to the SAME exec fns the native tools use, so upstream-verified
    /// native behavior is inherited iff the alias forwards identical args.
    /// Twin fixtures, alias on one side, native on the other, then compare
    /// canonical effects (fragment + lifecycle). No upstream counterpart
    /// is invented; the oracle is the native tool's own verified path.
    #[test]
    fn edit_update_matches_native_update_effects() {
        use ltmrs_compat::lemma::tool_args::MemoryUpdateArgs;
        {
            let content = "## New\n\n### Context\nFresh.";
            let (disp_a, _d1) = test_dispatcher();
            let (disp_b, _d2) = test_dispatcher();
            let id = ident(None, None, None, false);
            let seed = "## Old\n\n### Context\nStale.";
            let mid_a = add_fragment(&disp_a, 11, seed);
            let mid_b = add_fragment(&disp_b, 21, seed);
            // Alias side.
            let edit_tool = edit_tool("update", &mid_a, Some(content));
            let out_a = run(
                &disp_a,
                &tool_call_with(12, edit_tool.clone(), id.clone()),
                &edit_tool,
            );
            assert!(
                !result_is_error(&out_a),
                "alias update failed: {}",
                result_text(&out_a)
            );
            // Native side.
            let native = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
                id: mid_b.clone(),
                title: None,
                fragment: Some(content.to_string()),
                confidence: None,
            });
            let out_b = run(
                &disp_b,
                &tool_call_with(22, native.clone(), id.clone()),
                &native,
            );
            assert!(
                !result_is_error(&out_b),
                "native update failed: {}",
                result_text(&out_b)
            );
            // Identical canonical effects.
            let snap = |d: &crate::dispatcher::Dispatcher| {
                let m = d.repo().export_full().unwrap().memories;
                assert_eq!(m.len(), 1);
                (m[0].fragment.clone(), m[0].lifecycle.clone())
            };
            assert_eq!(
                snap(&disp_a),
                snap(&disp_b),
                "alias must equal native effects"
            );
        }
    }

    /// Transitive legacy oracle: `forget` and `invalidate` match native
    /// `memory_forget` effects (live vs tombstoned lifecycle).
    #[test]
    fn edit_forget_invalidate_match_native_forget_effects() {
        use ltmrs_compat::lemma::tool_args::{MemoryForgetArgs, ToolArgs};
        for (op, expect_recallable) in [("forget", false), ("invalidate", false)] {
            let (disp_a, _d1) = test_dispatcher();
            let (disp_b, _d2) = test_dispatcher();
            let id = ident(None, None, None, false);
            let seed = "## Doomed\n\n### Context\nGone.";
            let mid_a = add_fragment(&disp_a, 31, seed);
            let mid_b = add_fragment(&disp_b, 41, seed);
            let edit_tool = edit_tool(op, &mid_a, None);
            let out_a = run(
                &disp_a,
                &tool_call_with(32, edit_tool.clone(), id.clone()),
                &edit_tool,
            );
            assert!(
                !result_is_error(&out_a),
                "alias {op} failed: {}",
                result_text(&out_a)
            );
            let native = ToolArgs::MemoryForget(MemoryForgetArgs {
                id: mid_b.clone(),
                consolidate: false,
                invalidate: op == "invalidate",
            });
            let out_b = run(
                &disp_b,
                &tool_call_with(42, native.clone(), id.clone()),
                &native,
            );
            assert!(
                !result_is_error(&out_b),
                "native forget failed: {}",
                result_text(&out_b)
            );
            let life = |d: &crate::dispatcher::Dispatcher| {
                let m = d.repo().export_full().unwrap().memories;
                assert_eq!(m.len(), 1);
                format!("{:?}", m[0].lifecycle)
            };
            assert_eq!(
                life(&disp_a),
                life(&disp_b),
                "alias {op} must equal native lifecycle"
            );
            assert!(
                life(&disp_a).contains("ecallable") == expect_recallable,
                "unexpected recallability for {op}: {}",
                life(&disp_a)
            );
        }
    }
}

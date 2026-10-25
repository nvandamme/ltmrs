//! Legacy MCP tool execution (WP-08).
//!
//! Executes the 11 WP-08 tools against the canonical repository and search
//! backend, shaping responses to match the frozen Lemma 0.21.0 wire contract
//! (text + structuredContent + error flag). This is the compatibility
//! adapter: it maps ltmrs canonical state to the legacy observable surface.
//!
//! Design 11.1: preserves the full supported Lemma API/workflow surface.
//! Design 12.1: memory_add privacy (confirm override, DEV-002).
//! RQ-17: read side effects persisted before success.

mod backup;
#[cfg(test)]
mod backup_tests;
#[cfg(test)]
mod differential_tests;
mod guide_catalog;
#[cfg(test)]
mod guide_catalog_tests;
mod guide_create;
mod guide_distill;
mod guide_forget;
mod guide_get;
mod guide_merge;
mod guide_practice;
mod guide_render;
mod guide_update;
mod ids;
mod intelligence;
#[cfg(test)]
mod intelligence_tests;
mod memory_add;
#[cfg(test)]
mod memory_add_tests;
mod memory_audit;
mod memory_feedback;
#[cfg(test)]
mod memory_feedback_tests;
mod memory_forget;
#[cfg(test)]
mod memory_forget_tests;
mod memory_library;
mod memory_merge;
mod memory_read;
#[cfg(test)]
mod memory_read_tests;
mod memory_relate;
mod memory_stats;
mod memory_update;
#[cfg(test)]
mod memory_update_tests;
mod recall;
mod replay;
#[cfg(test)]
mod replay_tests;
mod semantic_search;
mod session_start;
mod sessions;
#[cfg(test)]
mod sessions_tests;
mod suggest;
#[cfg(test)]
mod suggest_tests;
#[cfg(test)]
mod test_support;
mod text;
#[cfg(test)]
mod text_tests;

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::{ResponseFormat, ToolArgs};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Instant;
use ltmrs_service::projects::{IdentityClaim, ResolvedScope};

use ltmrs_domain::relation::{Relation, RelationType};
use ltmrs_search::similarity::SimilarityService;
use serde_json::Value;

/// Execute a typed tool call, returning the shaped legacy result.
pub fn execute_tool(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    tool: &ToolArgs,
) -> DomainResult<DomainPayload> {
    // Admit once at tool entry (RQ-06 admission-once): every mutating
    // tool executes under this admission, so no step after the first
    // durable commit can fail on namespace expiry — the admitted
    // lifetime is exactly the tool-call lifetime. Continuations take
    // `admitted`; single-primitive tools validate inside as before.
    // Read-only tools run unadmitted.
    let admitted = if tool.mutates_store() {
        Some(
            disp.repo()
                .admit_scope(&envelope.operation_scope(envelope.request_digest()?))?,
        )
    } else {
        None
    };
    // All mutating tools are admitted above; unwrapping here keeps each
    // exec signature honest (needs-admission is visible in the type).
    let adm = || {
        admitted
            .as_ref()
            .expect("execute_tool admits every mutating tool before dispatching it")
    };
    // Project identity (Wave A3b): resolve the envelope identity (env /
    // bind file / flag winner) plus the wire legacy project into one
    // scope. Memory reads never register (reads stay side-effect-free for
    // identity) but obey the resolved scope; mutations may register,
    // learn a moved root, or refuse with candidates + remediation.
    let wire_project = ltmrs_compat::lemma::tool_args::tool_project(tool);
    let claim = IdentityClaim {
        project_id: envelope.identity.project_id.as_deref(),
        canonical_root: envelope.identity.canonical_root.as_deref(),
        legacy_project: wire_project.as_deref(),
        legacy_explicit: envelope.identity.legacy_explicit,
        all: matches!(tool, ToolArgs::MemoryRead(a) if a.all),
    };
    let is_write = tool.mutates_store() && !matches!(tool, ToolArgs::MemoryRead(_));
    let resolved = disp.repo().resolve_identity(&claim, is_write)?;
    let mut payload = match tool {
        ToolArgs::MemoryRead(args) => {
            memory_read::exec_memory_read(disp, envelope, adm(), args, &resolved)
        }
        ToolArgs::MemoryAdd(args) => {
            memory_add::exec_memory_add(disp, envelope, adm(), args, &resolved)
        }
        ToolArgs::MemoryUpdate(args) => {
            memory_update::exec_memory_update(disp, envelope, adm(), args, &resolved)
        }
        ToolArgs::MemoryFeedback(args) => {
            memory_feedback::exec_memory_feedback(disp, envelope, adm(), args, &resolved)
        }
        ToolArgs::MemoryForget(args) => {
            memory_forget::exec_memory_forget(disp, envelope, adm(), args, &resolved)
        }
        ToolArgs::MemoryMerge(args) => {
            memory_merge::exec_memory_merge(disp, envelope, adm(), args, &resolved)
        }
        ToolArgs::MemoryRelate(args) => {
            memory_relate::exec_memory_relate(disp, envelope, adm(), args, &resolved)
        }
        ToolArgs::MemoryStats(args) => memory_stats::exec_memory_stats(disp, args, &resolved),
        ToolArgs::MemoryAudit(args) => memory_audit::exec_memory_audit(disp, args),
        ToolArgs::MemoryLibrary(args) => memory_library::exec_memory_library(disp, args, &resolved),
        ToolArgs::SemanticSearch(args) => {
            semantic_search::exec_semantic_search(disp, envelope, args, &resolved)
        }
        ToolArgs::GuideGet(args) => guide_get::exec_guide_get(disp, args),
        ToolArgs::GuidePractice(args) => {
            guide_practice::exec_guide_practice(disp, envelope, adm(), args)
        }
        ToolArgs::GuideCreate(args) => guide_create::exec_guide_create(disp, envelope, adm(), args),
        ToolArgs::GuideDistill(args) => {
            guide_distill::exec_guide_distill(disp, envelope, adm(), args)
        }
        ToolArgs::GuideUpdate(args) => guide_update::exec_guide_update(disp, envelope, adm(), args),
        ToolArgs::GuideForget(args) => guide_forget::exec_guide_forget(disp, envelope, adm(), args),
        ToolArgs::GuideMerge(args) => guide_merge::exec_guide_merge(disp, envelope, adm(), args),
        ToolArgs::SessionStart(args) => {
            session_start::exec_session_start(disp, envelope, adm(), args, &resolved)
        }
        ToolArgs::SessionAttempt(args) => {
            sessions::exec_session_attempt(disp, envelope, adm(), args)
        }
        ToolArgs::SessionEnd(args) => sessions::exec_session_end(disp, envelope, adm(), args),
        ToolArgs::SessionStats(args) => sessions::exec_session_stats(disp, envelope, args),
        ToolArgs::SuggestionRespond(args) => {
            suggest::exec_suggestion_respond(disp, envelope, adm(), args)
        }
        ToolArgs::ConflictScan(args) => intelligence::exec_conflict_scan(disp, args),
        ToolArgs::ProactiveAnalysis(args) => intelligence::exec_proactive_analysis(disp, args),
        ToolArgs::ProjectAnalytics(args) => intelligence::exec_project_analytics(disp, args),
        ToolArgs::BackupCreate(args) => backup::exec_backup_create(disp, args),
        ToolArgs::BackupPreview(args) => backup::exec_backup_preview(disp, envelope, args),
        ToolArgs::BackupRestore(args) => backup::exec_backup_restore(disp, envelope, args),
        ToolArgs::BackupImportLemma(args) => backup::exec_backup_import_lemma(disp, envelope, args),
    }?;
    // Proceed-with-warning (Wave A3b shadow matrix): the resolution note
    // rides the human-readable text; the frozen structured shape is
    // untouched and frozen replays stay verbatim (they return above).
    if let Some(note) = resolved.warning.as_deref()
        && let DomainPayload::ToolResult { text, .. } = &mut payload
    {
        text.push_str(&format!("\n\nNote: {note}"));
    }
    Ok(payload)
}

// ---- Result constructors ----

fn ok_result(text: String, structured: Value) -> DomainPayload {
    DomainPayload::ToolResult {
        text,
        structured: Some(structured),
        is_error: false,
    }
}

fn err_result(message: &str) -> DomainPayload {
    DomainPayload::ToolResult {
        text: format!("Error: {message}"),
        structured: None,
        is_error: true,
    }
}

/// Persist sessions before ack, failing the tool loudly when the save fails
/// (re-review R1): a failed save must never report success.
fn persist_before_ack(disp: &Dispatcher) -> DomainResult<()> {
    disp.persist_sessions()
        .map_err(|msg| DomainError::new(DomainErrorCode::Validation, msg))
}

/// Key-reuse rejection for retried operations with changed arguments.
fn key_reuse_result() -> DomainResult<DomainPayload> {
    Ok(err_result("operation key reused with different input"))
}

/// Build a result honoring the frozen response_format (json => text is the
/// JSON-encoded data, matching upstream buildResult).
fn format_result(text: String, data: Value, format: Option<ResponseFormat>) -> DomainPayload {
    if format == Some(ResponseFormat::Json) {
        return ok_result(data.to_string(), data);
    }
    ok_result(text, data)
}

/// One similarity service over this dispatcher's repo and table (when
/// attached): every compatibility similarity decision — dedup, auto-link,
/// conflict candidates — routes through it instead of ad-hoc scans.
fn similarity_service(disp: &Dispatcher) -> SimilarityService {
    SimilarityService::new(disp.repo_arc(), disp.search().map(|sb| sb.table()))
}

/// Create a relation with a deterministic ID derived from the operation.
fn new_relation(
    envelope: &IpcEnvelope,
    now_millis: u64,
    source: EntityId,
    target: EntityId,
    rtype: RelationType,
    note: Option<String>,
) -> Relation {
    let rid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!(
            "ltmrs:rel:{}:{}:{}:{}",
            envelope.operation_id.as_uuid(),
            source.as_uuid(),
            target.as_uuid(),
            rtype.as_str()
        )
        .as_bytes(),
    ));
    Relation::new(rid, source, target, rtype, note, Instant::new(now_millis))
}

/// Build the retrieval scope for a read path (Wave A3b): the stable id
/// (with its attached legacy slugs) or the legacy/global fallback, plus
/// the call's filters. `all` is the wire cross-project flag; the resolved
/// claim already carries it for the calls that have one.
fn effective_for_read(
    resolved: &ResolvedScope,
    min_confidence: Option<f64>,
    after: Option<u64>,
    before: Option<u64>,
    all: bool,
) -> DomainResult<ltmrs_search::retrieval::scope::EffectiveScope> {
    let raw = ltmrs_domain::command::Scope {
        project: if resolved.project_id.is_some() {
            None
        } else {
            resolved.legacy.clone()
        },
        project_id: resolved.project_id_str.clone(),
        all_projects: resolved.all || all,
        min_confidence,
        after,
        before,
        ..Default::default()
    };
    ltmrs_search::retrieval::scope::EffectiveScope::resolve_for(&raw, &resolved.attached)
}

/// Stable-scope existence gate for single-target tools (Wave A3b): under
/// a stable identity a foreign row is treated as missing (no leak, no
/// cross-project write). Legacy/global scopes stay permissive.
fn stable_visible(
    resolved: &ResolvedScope,
    m: &ltmrs_domain::memory::Memory,
) -> DomainResult<bool> {
    if resolved.project_id.is_none() {
        return Ok(true);
    }
    let scope = effective_for_read(resolved, None, None, None, false)?;
    Ok(scope.project_includes_id(m.project_id, m.project.as_deref()))
}

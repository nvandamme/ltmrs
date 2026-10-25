//! Shared test fixtures for tool tests (moved verbatim from `tools.rs`).
//!
//! Every per-tool `*_tests.rs` module uses these instead of cloning them
//! (rule 3: shared-first). Test-only: gated out of non-test builds.

use std::sync::Arc;

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, DomainRequest, IpcEnvelope, PROTOCOL_VERSION};
use ltmrs_compat::lemma::tool_args::{MemoryAddArgs, ToolArgs};
use ltmrs_domain::clock::FrozenClock;
use ltmrs_domain::command::Scope;
use ltmrs_domain::id::{
    ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, ExternalAlias,
    FrontendId, OperationId, StoreGeneration,
};
use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemorySource};
use ltmrs_service::repository::CanonicalRepository;
use serde_json::Value;
use uuid::Uuid;

use super::execute_tool;

pub(crate) fn fe(n: u64) -> FrontendId {
    FrontendId::new(Uuid::from_u128(n as u128))
}

pub(crate) fn ch(n: u64) -> ChannelId {
    ChannelId::new(Uuid::from_u128(n as u128))
}

pub(crate) fn test_dispatcher() -> (Dispatcher, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
            .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let registry = crate::registry::FrontendRegistry::new();
    (Dispatcher::new(repo, registry, clock), dir)
}

pub(crate) fn envelope(op: u64, body: DomainRequest) -> IpcEnvelope {
    IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: StoreGeneration::FIRST,
        frontend_id: fe(1),
        channel_id: ch(1),
        operation_id: OperationId::new(Uuid::from_u128(op as u128)),
        session: None,
        retry_epoch: 1,
        deadline_millis: None,
        scope: Scope::default(),
        body,
        identity: Default::default(),
    }
}

pub(crate) fn tool_call(op: u64, tool: ToolArgs) -> IpcEnvelope {
    envelope(op, DomainRequest::ToolCall { tool })
}

/// Same as `tool_call` for an explicit (frontend, channel) identity with a
/// caller-chosen operation number. Op numbers may repeat across channels:
/// receipt keys and session bindings are per (frontend, channel), so a
/// reused numeric op must never collide globally (T-SESS-01).
pub(crate) fn channel_tool_call(fe_n: u64, ch_n: u64, op: u64, tool: ToolArgs) -> IpcEnvelope {
    IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: StoreGeneration::FIRST,
        frontend_id: fe(fe_n),
        channel_id: ch(ch_n),
        operation_id: OperationId::new(Uuid::from_u128(op as u128)),
        session: None,
        retry_epoch: 1,
        deadline_millis: None,
        scope: Scope::default(),
        body: DomainRequest::ToolCall { tool },
        identity: Default::default(),
    }
}

pub(crate) fn run(disp: &Dispatcher, env: &IpcEnvelope, tool: &ToolArgs) -> DomainPayload {
    execute_tool(disp, env, tool).unwrap()
}

pub(crate) fn add_fragment(disp: &Dispatcher, op: u64, fragment: &str) -> String {
    let env = tool_call(
        op,
        ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: fragment.to_string(),
            ..Default::default()
        }),
    );
    let result = run(
        disp,
        &env,
        &ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: fragment.to_string(),
            ..Default::default()
        }),
    );
    if let DomainPayload::ToolResult {
        structured: Some(s),
        is_error: false,
        ..
    } = &result
    {
        s["id"].as_str().unwrap().to_string()
    } else {
        panic!("memory_add failed: {}", result_text(&result));
    }
}

pub(crate) fn result_text(p: &DomainPayload) -> String {
    match p {
        DomainPayload::ToolResult { text, .. } => text.clone(),
        _ => String::new(),
    }
}

pub(crate) fn result_structured(p: &DomainPayload) -> Option<Value> {
    match p {
        DomainPayload::ToolResult { structured, .. } => structured.clone(),
        _ => None,
    }
}

pub(crate) fn result_is_error(p: &DomainPayload) -> bool {
    match p {
        DomainPayload::ToolResult { is_error, .. } => *is_error,
        _ => false,
    }
}

pub(crate) fn text_contains(p: &DomainPayload, needle: &str) -> bool {
    result_text(p).contains(needle)
}

pub(crate) fn ident(
    id: Option<&str>,
    root: Option<&str>,
    legacy: Option<&str>,
    explicit: bool,
) -> crate::envelope::EnvelopeIdentity {
    crate::envelope::EnvelopeIdentity {
        project_id: id.map(|s| s.to_string()),
        canonical_root: root.map(|s| s.to_string()),
        legacy_project: legacy.map(|s| s.to_string()),
        legacy_explicit: explicit,
    }
}

pub(crate) fn tool_call_with(
    op: u64,
    tool: ToolArgs,
    identity: crate::envelope::EnvelopeIdentity,
) -> IpcEnvelope {
    IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: StoreGeneration::FIRST,
        frontend_id: fe(1),
        channel_id: ch(1),
        operation_id: OperationId::new(Uuid::from_u128(op as u128)),
        session: None,
        retry_epoch: 1,
        deadline_millis: None,
        scope: Scope::default(),
        identity,
        body: DomainRequest::ToolCall { tool },
    }
}

pub(crate) fn add_with(
    disp: &Dispatcher,
    op: u64,
    fragment: &str,
    project: Option<&str>,
    identity: crate::envelope::EnvelopeIdentity,
) -> DomainPayload {
    let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: fragment.to_string(),
        project: project.map(|s| s.to_string()),
        ..Default::default()
    });
    let env = tool_call_with(op, tool.clone(), identity);
    execute_tool(disp, &env, &tool).unwrap()
}

pub(crate) fn read_count(
    disp: &Dispatcher,
    op: u64,
    identity: crate::envelope::EnvelopeIdentity,
) -> usize {
    use ltmrs_compat::lemma::tool_args::MemoryReadArgs;
    let tool = ToolArgs::MemoryRead(MemoryReadArgs::default());
    let env = tool_call_with(op, tool.clone(), identity);
    match execute_tool(disp, &env, &tool).unwrap() {
        DomainPayload::ToolResult {
            structured: Some(s),
            is_error: false,
            ..
        } => s["count"].as_u64().unwrap() as usize,
        other => panic!("memory_read failed: {other:?}"),
    }
}

pub(crate) fn seed_pagetoken_memories(disp: &Dispatcher, start_op: u64, n: usize) {
    for i in 0..n {
        add_fragment(
            disp,
            start_op + i as u64,
            &format!(
                "## Pagetoken fragment {i:04}\n\n### Context\nPagetoken shared query terms uniqueword{i} quux{i} zebra{i}.\n"
            ),
        );
    }
}

/// Shared page-shape assertion: exact total, exact page length, honest
/// has_more/next_offset for the given limit/offset over `total` matches.
pub(crate) fn expect_pagination_page(structured: &Value, total: u64, limit: u64, offset: u64) {
    assert_eq!(
        structured["total"].as_u64().unwrap(),
        total,
        "total must count the full matching set"
    );
    assert_eq!(
        structured["count"].as_u64().unwrap(),
        total.saturating_sub(offset).min(limit),
        "page length wrong for total={total} limit={limit} offset={offset}"
    );
    let more = offset + limit < total;
    assert_eq!(
        structured["has_more"].as_bool().unwrap(),
        more,
        "has_more wrong for total={total} limit={limit} offset={offset}"
    );
    if more {
        assert_eq!(
            structured["next_offset"].as_u64().unwrap(),
            offset + limit,
            "next_offset must continue the page"
        );
    } else {
        assert!(
            structured["next_offset"].is_null(),
            "next_offset must be null on the last page"
        );
    }
}

pub(crate) async fn publish_search_row(
    repo: &CanonicalRepository,
    table: &ltmrs_search::search::table::SearchTable,
    lexical_text: &str,
    fingerprint: ltmrs_domain::id::ModelFingerprint,
) {
    use ltmrs_domain::id::{ChunkId, DocumentRevision, StoreGeneration};
    let memory_id = repo.export_snapshot().unwrap().memories[0].id;
    table
        .publish_rows(&[ltmrs_search::search::row::SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id,
            document_revision: DocumentRevision::new(1),
            model_fingerprint: fingerprint,
            chunk_id: ChunkId::new(0),
            chunker_version: "v1".to_string(),
            lexical_text: lexical_text.to_string(),
            char_start: 0,
            char_end: lexical_text.len() as u64,
            project: None,
            project_id: None,
            fragment_type: "fact".to_string(),
            created_at_millis: 1000,
            confidence: 0.9,
            updated_at_millis: 1000,
            embedding: Some(vec![0.0; 384]),
        }])
        .await
        .unwrap();
}

pub(crate) async fn publish_pagetoken_rows(
    repo: &CanonicalRepository,
    table: &ltmrs_search::search::table::SearchTable,
    token: &str,
) {
    use ltmrs_domain::id::{ChunkId, DocumentRevision, StoreGeneration};
    let memories = repo.export_snapshot().unwrap().memories;
    let rows: Vec<ltmrs_search::search::row::SearchRow> = memories
        .iter()
        .map(|m| {
            let lexical_text = format!(
                "{} {} {}",
                token,
                m.title.to_lowercase(),
                m.fragment.to_lowercase()
            );
            ltmrs_search::search::row::SearchRow {
                store_generation: StoreGeneration::FIRST,
                memory_id: m.id,
                document_revision: DocumentRevision::new(1),
                model_fingerprint: ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT,
                chunk_id: ChunkId::new(0),
                chunker_version: "v1".to_string(),
                lexical_text: lexical_text.clone(),
                char_start: 0,
                char_end: lexical_text.len() as u64,
                project: None,
                project_id: None,
                fragment_type: m.fragment_type.as_str().to_string(),
                created_at_millis: m.created_at.as_millis(),
                confidence: m.confidence,
                updated_at_millis: m.created_at.as_millis(),
                embedding: None,
            }
        })
        .collect();
    table.publish_rows(&rows).await.unwrap();
    table.create_fts_index().await.unwrap();
}

pub(crate) fn import_staging_text() -> String {
    let snapshot = serde_json::json!({
        "memories": [{
            "id": "22222222-2222-2222-2222-222222222222",
            "external_alias": "m9",
            "title": "Imported One",
            "fragment": "## Imported One",
            "description": "",
            "fragment_type": "Fact",
            "project": null,
            "project_id": null,
            "source": "Ai",
            "confidence": 0.6,
            "quality_score": null,
            "lifecycle": "Live",
            "tags": [],
            "associated_with": [],
            "relations": [],
            "parent_id": null,
            "child_ids": [],
            "session_id": null,
            "task_type": null,
            "related_guides": [],
            "evidence": [],
            "access_count": 0,
            "last_accessed_at": null,
            "positive_feedback": 0,
            "negative_feedback": 0,
            "negative_hits": 0,
            "refinement_count": 0,
            "distill_candidate": false,
            "entity_revision": 1,
            "document_revision": 1,
            "eligibility_revision": 1,
            "created_at": 1000,
            "updated_at": 1000,
            "raw_created": null,
            "unknown_fields": {}
        }],
        "relations": [],
        "guides": [],
        "sessions": [],
        "feedback": [],
        "suggestions": [],
        "projects": [],
        "archives": [],
        "history": []
    });
    let export: ltmrs_domain::export::CanonicalExport =
        serde_json::from_value(snapshot.clone()).unwrap();
    serde_json::json!({
        "ok": true,
        "lemma_version": "0.21.0",
        "schema_version": 8,
        "digest": export.digest(),
        "counts": {"memories": 1, "relations": 0, "guides": 0, "sessions": 0, "feedback": 0, "suggestions": 0, "projects": 0, "archives": 0, "history": 0},
        "snapshot": snapshot,
        "loss": [{"kind": "folded_guide_manual", "count": 1, "detail": "guide=\"g\""}]
    })
    .to_string()
}

/// Wave B5a test helper: set an exact confidence (validates the update
/// succeeded; panics otherwise).
pub(crate) fn set_confidence(disp: &Dispatcher, op: u64, id: &str, confidence: f64) {
    use ltmrs_compat::lemma::tool_args::MemoryUpdateArgs;
    let tool = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: id.to_string(),
        confidence: Some(confidence),
        ..Default::default()
    });
    let result = run(disp, &tool_call(op, tool.clone()), &tool);
    assert!(
        !result_is_error(&result),
        "set_confidence failed: {}",
        result_text(&result)
    );
}

/// Wave B5a test helper: link two fragments (panics on failure).
pub(crate) fn relate(disp: &Dispatcher, op: u64, source: &str, target: &str, ty: &str) {
    use ltmrs_compat::lemma::tool_args::{MemoryRelateArgs, ToolArgs as TA};
    let tool = TA::MemoryRelate(MemoryRelateArgs {
        source_id: source.to_string(),
        target_id: target.to_string(),
        relation_type: ty.to_string(),
        note: None,
    });
    let result = run(disp, &tool_call(op, tool.clone()), &tool);
    assert!(
        !result_is_error(&result),
        "relate failed: {}",
        result_text(&result)
    );
}

/// Wave B5a test helper: read one fragment's related_graph ids in order.
pub(crate) fn related_ids(result: &DomainPayload) -> Vec<String> {
    let structured = result_structured(result).unwrap();
    structured["related_graph"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|n| n["id"].as_str().map(|s| s.to_string()))
        .collect()
}

/// Path to the pinned upstream pure-functions oracle
/// (tests/compat/lemma_0_21_0/pure_functions.json): generated by running
/// the pinned upstream code on a set of inputs; ltmrs must reproduce the
/// upstream outputs byte-for-byte.
pub(crate) fn pure_oracle_path() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../tests/compat/lemma_0_21_0/pure_functions.json");
    p
}

/// Minimal memory fixture for oracle/stats tests.
pub(crate) fn make_memory(
    id: &str,
    project: Option<&str>,
    source: &str,
    confidence: f64,
    fragment: &str,
) -> Memory {
    Memory {
        id: EntityId::new(Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("ltmrs:entity:{}", id).as_bytes(),
        )),
        external_alias: Some(ExternalAlias::new(id.to_string())),
        title: String::new(),
        fragment: fragment.to_string(),
        description: String::new(),
        fragment_type: FragmentType::Fact,
        project: project.map(|s| s.to_string()),
        project_id: None,
        source: MemorySource::parse(source).expect("valid source in oracle"),
        confidence,
        quality_score: None,
        lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
        tags: Vec::new(),
        associated_with: Vec::new(),
        relations: Vec::new(),
        parent_id: None,
        child_ids: vec![],
        session_id: None,
        task_type: None,
        related_guides: Vec::new(),
        evidence: Vec::new(),
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: 0,
        negative_feedback: 0,
        negative_hits: 0,
        refinement_count: 0,
        distill_candidate: false,
        entity_revision: EntityRevision::new(0),
        document_revision: DocumentRevision::new(0),
        eligibility_revision: EligibilityRevision::new(0),
        created_at: Instant::new(0),
        updated_at: Instant::new(0),
        raw_created: None,
        unknown_fields: Default::default(),
    }
}

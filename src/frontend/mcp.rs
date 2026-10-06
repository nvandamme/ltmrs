//! rmcp stdio boundary: routes MCP tool calls to typed IPC envelopes.
//!
//! No domain logic lives here — each tool call maps to a `DomainRequest`
//! carried in an IPC envelope. The mapping is pure and testable.
//!
//! WP-08: serves the 11 frozen Lemma 0.21.0 tools verbatim (T-MCP-01) and
//! routes each call to a typed `ToolCall` envelope. Argument validation and
//! error classes are enforced here (T-MCP-03).

use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerNotification, Tool, ToolAnnotations, ToolListChangedNotification,
    ToolListChangedNotificationMethod,
};
use rmcp::service::{MaybeSendFuture, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use crate::compatibility::lemma::schemas::frozen_tools;
use crate::compatibility::lemma::tool_args::{
    BackupCreateArgs, BackupPreviewArgs, BackupRestoreArgs, ConflictScanArgs, GuideCreateArgs,
    GuideDistillArgs, GuideForgetArgs, GuideGetArgs, GuideMergeArgs, GuidePracticeArgs,
    GuideUpdateArgs, MemoryAddArgs, MemoryAuditArgs, MemoryFeedbackArgs, MemoryForgetArgs,
    MemoryLibraryArgs, MemoryMergeArgs, MemoryReadArgs, MemoryRelateArgs, MemoryStatsArgs,
    MemoryUpdateArgs, ProactiveAnalysisArgs, ProjectAnalyticsArgs, ResponseFormat,
    SemanticSearchArgs, SessionAttemptArgs, SessionEndArgs, SessionStartArgs, SessionStatsArgs,
    SuggestionRespondArgs, ToolArgs,
};
use crate::daemon::client::IpcClient;
use crate::daemon::envelope::{
    DomainRequest, HandshakeRequest, IpcEnvelope, IpcError, IpcResponse, IpcResult,
    PROTOCOL_VERSION,
};
use crate::domain::command::{DomainErrorCode, Scope};
use crate::domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
use crate::domain::memory::Memory;
use uuid::Uuid;

/// The frontend's identity, fixed per process/channel except the store
/// generation, which tracks the daemon across restores (a restore bumps
/// the generation; a frontend that never adopts it bricks on its next
/// handshake).
#[derive(Debug)]
pub struct FrontendIdentity {
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
    store_generation: std::sync::Mutex<StoreGeneration>,
}

impl FrontendIdentity {
    pub fn new(frontend_id: FrontendId, channel_id: ChannelId) -> Self {
        Self {
            frontend_id,
            channel_id,
            store_generation: std::sync::Mutex::new(StoreGeneration::FIRST),
        }
    }

    pub fn generation(&self) -> StoreGeneration {
        *self.store_generation.lock().unwrap()
    }

    fn set_generation(&self, generation: StoreGeneration) {
        *self.store_generation.lock().unwrap() = generation;
    }
}

impl Clone for FrontendIdentity {
    fn clone(&self) -> Self {
        Self {
            frontend_id: self.frontend_id,
            channel_id: self.channel_id,
            store_generation: std::sync::Mutex::new(self.generation()),
        }
    }
}

/// The static teaching template (frozen from the Lemma 0.21.0 baseline).
/// Always returned in full in the MCP `instructions` field.
const INSTRUCTIONS_TEMPLATE: &str = "# Lemma — Persistent Memory

You start every session blank — knowledge survives only via tool calls. If you
learn something and don't save it (memory_add), it's gone permanently.

## Layers
- Memory fragments (memory_read/add): fact / pattern / lesson / warning / context.
  Confidence evolves with use.
- Guides (guide_get/distill/practice): procedural skills distilled from experience.
  Track usage + success rate.
Pipeline: experience -> memory_add -> pattern/lesson -> guide_distill -> guide_practice.

## How to work
1. RECALL: memory_read.  2. ACT.  3. PERSIST: insight -> memory_add, guide applied -> guide_practice.
Store fragments in ENGLISH (required for search). Never ask permission to save.

## Writing a fragment
## [Title] / ### Context (1-2 sentences) / ### [Content] (bullets).
One idea, 30-2000 chars. Types: fact/pattern/lesson/warning/context.

## Relations (memory_relate)
supports / contradicts / supersedes / related_to (bidirectional).

## Background intelligence
Conflict detection, suggestions (distill/merge/refine), and auto-linking run
automatically — act on signals when sensible.

## Commands
-lib -> memory_library (full snapshot). -vis -> launch visualizer.";

/// Build the dynamic memory index appended to the instructions (upstream
/// `buildInstructions`): top project fragments, then top global fragments.
fn build_instructions_index(memories: &[Memory], project: Option<&str>) -> String {
    let global: Vec<&Memory> = memories.iter().filter(|m| m.project.is_none()).collect();
    let proj: Vec<&Memory> = memories
        .iter()
        .filter(|m| match (project, m.project.as_deref()) {
            (Some(p), Some(mp)) => p == mp,
            _ => false,
        })
        .collect();

    let mut index = String::from("\n\n## Your current memory\n");
    if global.is_empty() && proj.is_empty() {
        index.push_str(
            "You have no saved memories yet. Call memory_add to start building your knowledge base.\n",
        );
        return index;
    }
    if !proj.is_empty() {
        let mut proj = proj;
        proj.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let name = project.unwrap_or("current");
        index.push_str(&format!("- Project \"{name}\": {} fragments\n", proj.len()));
        for f in proj.iter().take(8) {
            index.push_str(&format!(
                "  [{}] {} ({:.2})\n",
                f.external_alias
                    .as_ref()
                    .map(|a| a.as_str().to_string())
                    .unwrap_or_else(|| f.id.as_uuid().to_string()),
                f.title,
                f.confidence
            ));
        }
    }
    if !global.is_empty() {
        let mut global = global;
        global.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        index.push_str(&format!("- Global: {} fragments\n", global.len()));
        for f in global.iter().take(5) {
            index.push_str(&format!(
                "  [{}] {} ({:.2})\n",
                f.external_alias
                    .as_ref()
                    .map(|a| a.as_str().to_string())
                    .unwrap_or_else(|| f.id.as_uuid().to_string()),
                f.title,
                f.confidence
            ));
        }
    }
    index.push_str("\nUse memory_read to load full details of any fragment.\n");
    index
}

/// The MCP frontend handler: terminates stdio and routes tools over IPC.
pub struct LtmrsFrontend {
    identity: FrontendIdentity,
    client: AsyncMutex<IpcClient>,
    /// Cached memory snapshot for the dynamic instructions index. Prefetched
    /// after the first connect; None until then (empty-state instructions).
    snapshot: Arc<Mutex<Option<Vec<Memory>>>>,
}

impl LtmrsFrontend {
    pub fn new(identity: FrontendIdentity, client: IpcClient) -> Self {
        Self {
            identity,
            client: AsyncMutex::new(client),
            snapshot: Arc::new(Mutex::new(None)),
        }
    }

    /// Update the cached snapshot used for the dynamic instructions index.
    pub fn set_snapshot(&self, memories: Vec<Memory>) {
        *self.snapshot.lock().unwrap() = Some(memories);
    }

    /// Ensure the daemon handshake, adopting the live store generation. A
    /// restore bumps the generation; a first-attempt mismatch adopts the
    /// daemon generation and retries once instead of bricking new clients.
    async fn ensure_handshaked(&self) -> Result<(), McpError> {
        let mut client = self.client.lock().await;
        if client.retry_epoch().is_some() {
            return Ok(());
        }
        if !client.is_connected() {
            client
                .connect()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        let request = |generation: StoreGeneration| HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: generation,
            frontend_id: self.identity.frontend_id,
            channel_id: self.identity.channel_id,
            resume_retry_epoch: None,
        };
        match client.handshake(&request(self.identity.generation())).await {
            Ok(hs) => {
                self.identity.set_generation(hs.store_generation);
            }
            Err(IpcError::GenerationMismatch { daemon, .. }) => {
                // The server closes a rejected handshake, so reconnect
                // before retrying with the adopted generation.
                let live = StoreGeneration::new(daemon);
                self.identity.set_generation(live);
                client
                    .connect()
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                // A second failure must not leave a dead-but-connected
                // client: close so the next call reconnects fresh instead
                // of erroring on a closed stream forever.
                match client.handshake(&request(live)).await {
                    Ok(hs) => self.identity.set_generation(hs.store_generation),
                    Err(e) => {
                        client.close();
                        return Err(McpError::internal_error(e.to_string(), None));
                    }
                }
            }
            Err(e) => {
                // The daemon closes rejected handshakes: drop our end too,
                // or every later call reuses a dead-but-connected stream.
                client.close();
                return Err(McpError::internal_error(e.to_string(), None));
            }
        }
        // Prefetch the memory snapshot for the dynamic instructions index.
        // Best-effort: a failure leaves the empty-state instructions.
        if let Ok(memories) = self.fetch_snapshot(&mut client).await {
            self.set_snapshot(memories);
        }
        Ok(())
    }

    /// Fetch all canonical memories via IPC (read-only, no receipt needed).
    async fn fetch_snapshot(&self, client: &mut IpcClient) -> Result<Vec<Memory>, McpError> {
        let op = OperationId::new(Uuid::now_v7());
        let env = IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: self.identity.generation(),
            frontend_id: self.identity.frontend_id,
            channel_id: self.identity.channel_id,
            operation_id: op,
            session: None,
            retry_epoch: client.retry_epoch().unwrap_or(0),
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::ListMemories,
        };
        // Read-only prefetch: must never disturb the handshake (P1-B) — a
        // failed prefetch leaves epoch and stream intact so the main call
        // still sends handshaked instead of drawing a certain StaleReplay.
        let resp = client
            .roundtrip(&env)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match resp.result {
            crate::daemon::envelope::IpcResult::Success {
                payload: crate::daemon::envelope::DomainPayload::Memories(m),
                ..
            } => Ok(m),
            crate::daemon::envelope::IpcResult::Error { message, .. } => {
                Err(McpError::internal_error(message, None))
            }
            _ => Err(McpError::internal_error(
                "unexpected response shape for ListMemories",
                None,
            )),
        }
    }

    /// Build the IPC envelope for a tool call. This is the pure routing logic.
    /// The `retry_epoch` comes from the daemon handshake.
    pub fn build_envelope(
        &self,
        request: &CallToolRequestParams,
        retry_epoch: u64,
    ) -> Result<IpcEnvelope, McpError> {
        let body = route_tool(&request.name, &request.arguments)?;
        Ok(self.envelope_for(body, retry_epoch))
    }

    /// Envelope construction shared by tool calls and the resilient
    /// round-trip: one operation id per MCP call for the first attempt; a
    /// transport-ambiguous retry resends the same envelope (P1-B), while the
    /// post-restore stale-generation retry still mints a fresh one (certain
    /// no-commit under the retired generation).
    fn envelope_for(&self, body: DomainRequest, retry_epoch: u64) -> IpcEnvelope {
        let op = OperationId::new(Uuid::now_v7());
        IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: self.identity.generation(),
            frontend_id: self.identity.frontend_id,
            channel_id: self.identity.channel_id,
            operation_id: op,
            session: None,
            retry_epoch,
            deadline_millis: None,
            scope: Scope::default(),
            body,
        }
    }

    /// Round-trip with exactly one re-handshake retry: a restore can bump
    /// the live generation between the handshake and the call, so an
    /// established connection that draws StaleGeneration forgets its epoch,
    /// re-handshakes (adopting the live generation), and retries once with
    /// a fresh operation id. A second stale answer is returned as-is —
    /// unbounded retry would mask a daemon that never converges.
    ///
    /// Transport failures are unknown outcomes (P1-B), NOT fresh attempts:
    /// the envelope is built ONCE per MCP call and, after reconnect plus a
    /// resumed same-epoch handshake, the SAME envelope is resent so the
    /// daemon replays its receipt instead of executing twice. A refused
    /// resume (or an unreconnectable transport) surfaces an unknown-outcome
    /// error to the host instead of silently minting a fresh operation.
    async fn roundtrip_with_rehandshake(
        &self,
        body: DomainRequest,
    ) -> Result<IpcResponse, McpError> {
        let mut client = self.client.lock().await;
        if !client.is_connected() {
            client
                .connect()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        if client.retry_epoch().is_none() {
            drop(client);
            self.ensure_handshaked().await?;
            client = self.client.lock().await;
        }
        let epoch = client.retry_epoch().unwrap_or(0);
        let envelope = self.envelope_for(body.clone(), epoch);
        let resp = match client.roundtrip(&envelope).await {
            Ok(resp) => resp,
            Err(e) => {
                drop(client);
                return self
                    .resend_after_reconnect(&envelope, epoch, e.to_string())
                    .await;
            }
        };
        let stale = matches!(
            resp.result,
            IpcResult::Error {
                code: DomainErrorCode::StaleGeneration,
                ..
            }
        );
        if !stale {
            return Ok(resp);
        }
        // Epoch-only reset: the stale reply arrived over a healthy stream,
        // so the connection stays up in both modes (redialing a live
        // socket is wasteful; redialing a bridge is fatal — no listener).
        client.forget_epoch();
        drop(client);
        self.ensure_handshaked().await?;
        let mut client = self.client.lock().await;
        let epoch = client.retry_epoch().unwrap_or(0);
        client
            .roundtrip_or_forget(&self.envelope_for(body, epoch))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    /// Resend an envelope whose outcome is unknown (P1-B): reconnect, resume
    /// the SAME retry epoch, and resend the SAME envelope when the generation
    /// still matches. Never mints a fresh operation for an uncertain
    /// mutation: a refused resume or a moved generation takes the explicit
    /// stale/unknown paths instead.
    async fn resend_after_reconnect(
        &self,
        envelope: &IpcEnvelope,
        epoch: u64,
        first_error: String,
    ) -> Result<IpcResponse, McpError> {
        let mut client = self.client.lock().await;
        // Drop the possibly half-dead stream and stale epoch; the resume
        // handshake below re-establishes both or fails explicitly.
        client.forget_handshake();
        if let Err(e) = client.connect().await {
            return Err(McpError::internal_error(
                format!(
                    "request failed with unknown outcome ({first_error}) and reconnect failed ({e}); inspect state before retrying as new work"
                ),
                None,
            ));
        }
        let resume = HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: envelope.store_generation,
            frontend_id: self.identity.frontend_id,
            channel_id: self.identity.channel_id,
            resume_retry_epoch: Some(epoch),
        };
        match client.handshake(&resume).await {
            Ok(hs) => {
                self.identity.set_generation(hs.store_generation);
                if hs.retry_epoch != epoch {
                    client.close();
                    return Err(McpError::internal_error(
                        "reconnect issued a different retry epoch for an uncertain mutation; inspect state before retrying as new work",
                        None,
                    ));
                }
            }
            Err(IpcError::GenerationMismatch { .. }) => {
                // Generation moved: the gate rejects the old envelope before
                // execution (certain no-commit), so a fresh epoch plus a
                // fresh operation is safe — the established stale path.
                drop(client);
                self.ensure_handshaked().await?;
                let mut client = self.client.lock().await;
                let fresh_epoch = client.retry_epoch().unwrap_or(0);
                let fresh = self.envelope_for(envelope.body.clone(), fresh_epoch);
                return client
                    .roundtrip_or_forget(&fresh)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None));
            }
            Err(IpcError::StaleNamespace(msg)) => {
                client.close();
                return Err(McpError::internal_error(
                    format!(
                        "request failed with unknown outcome ({first_error}); retry namespace unavailable ({msg}) — inspect state before retrying as new work"
                    ),
                    None,
                ));
            }
            Err(e) => {
                client.close();
                return Err(McpError::internal_error(e.to_string(), None));
            }
        }
        // Same generation, same epoch: the daemon resolves the original
        // operation ID against its receipt (replay) or executes it (if the
        // first send never arrived). A StaleGeneration answer here means the
        // generation moved between handshake and resend — take the
        // established fresh-operation path.
        match client.roundtrip(envelope).await {
            Ok(resp) => {
                let stale = matches!(
                    resp.result,
                    IpcResult::Error {
                        code: DomainErrorCode::StaleGeneration,
                        ..
                    }
                );
                if !stale {
                    return Ok(resp);
                }
                drop(client);
                self.ensure_handshaked().await?;
                let mut client = self.client.lock().await;
                let fresh_epoch = client.retry_epoch().unwrap_or(0);
                let fresh = self.envelope_for(envelope.body.clone(), fresh_epoch);
                client
                    .roundtrip_or_forget(&fresh)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))
            }
            Err(e) => Err(McpError::internal_error(
                format!(
                    "resend failed with unknown outcome (first failure: {first_error}; resend: {e}); inspect state before retrying as new work"
                ),
                None,
            )),
        }
    }

    /// The tool list advertised to the host: the 26 frozen tools, served
    /// verbatim from the baseline capture (T-MCP-01), plus native ltmrs
    /// extensions (backup_create/preview/restore). Native tools are marked
    /// in their descriptions.
    pub fn tools() -> Vec<Tool> {
        let mut tools: Vec<Tool> = frozen_tools()
            .into_iter()
            .map(|ft| {
                let annotations = ToolAnnotations::new()
                    .read_only(ft.read_only_hint)
                    .destructive(ft.destructive_hint)
                    .idempotent(ft.idempotent_hint)
                    .open_world(ft.open_world_hint);
                let mut tool = Tool::new(ft.name, ft.description, Arc::new(ft.input_schema))
                    .with_annotations(annotations);
                if let Some(os) = ft.output_schema {
                    tool = tool.with_raw_output_schema(Arc::new(os));
                }
                tool
            })
            .collect();
        tools.push(native_backup_create_tool());
        tools.push(native_backup_preview_tool());
        tools.push(native_backup_restore_tool());
        tools
    }
}

/// Route a tool name + arguments to a typed `DomainRequest`.
///
/// Validates required fields and enums; returns protocol-level errors for
/// unknown tools and invalid parameters (T-MCP-03).
pub fn route_tool(
    name: &str,
    args: &Option<Map<String, Value>>,
) -> Result<DomainRequest, McpError> {
    let args = args.clone().unwrap_or_default();
    // Unknown argument keys fail instead of silently dropping (a typo'd
    // filter must never become "no filter"). The allowlist derives from the
    // served schemas (frozen + native), so it cannot drift from them.
    if let Some(allowed) = allowed_arguments(name) {
        for key in args.keys() {
            if !allowed.iter().any(|k| k == key) {
                return Err(McpError::invalid_params(
                    format!("unknown argument for {name}: {key}"),
                    None,
                ));
            }
        }
    }
    let tool_args = match name {
        "memory_read" => ToolArgs::MemoryRead(parse_memory_read(&args)?),
        "memory_add" => ToolArgs::MemoryAdd(parse_memory_add(&args)?),
        "memory_update" => ToolArgs::MemoryUpdate(parse_memory_update(&args)?),
        "memory_feedback" => ToolArgs::MemoryFeedback(parse_memory_feedback(&args)?),
        "memory_forget" => ToolArgs::MemoryForget(parse_memory_forget(&args)?),
        "memory_merge" => ToolArgs::MemoryMerge(parse_memory_merge(&args)?),
        "memory_relate" => ToolArgs::MemoryRelate(parse_memory_relate(&args)?),
        "memory_stats" => ToolArgs::MemoryStats(parse_memory_stats(&args)?),
        "memory_audit" => ToolArgs::MemoryAudit(parse_memory_audit(&args)?),
        "memory_library" => ToolArgs::MemoryLibrary(parse_memory_library(&args)?),
        "semantic_search" => ToolArgs::SemanticSearch(parse_semantic_search(&args)?),
        "guide_get" => ToolArgs::GuideGet(parse_guide_get(&args)?),
        "guide_practice" => ToolArgs::GuidePractice(parse_guide_practice(&args)?),
        "guide_create" => ToolArgs::GuideCreate(parse_guide_create(&args)?),
        "guide_distill" => ToolArgs::GuideDistill(parse_guide_distill(&args)?),
        "guide_update" => ToolArgs::GuideUpdate(parse_guide_update(&args)?),
        "guide_forget" => ToolArgs::GuideForget(parse_guide_forget(&args)?),
        "guide_merge" => ToolArgs::GuideMerge(parse_guide_merge(&args)?),
        "session_start" => ToolArgs::SessionStart(parse_session_start(&args)?),
        "session_attempt" => ToolArgs::SessionAttempt(parse_session_attempt(&args)?),
        "session_end" => ToolArgs::SessionEnd(parse_session_end(&args)?),
        "session_stats" => ToolArgs::SessionStats(parse_session_stats(&args)?),
        "suggestion_respond" => ToolArgs::SuggestionRespond(parse_suggestion_respond(&args)?),
        "conflict_scan" => ToolArgs::ConflictScan(parse_conflict_scan(&args)?),
        "proactive_analysis" => ToolArgs::ProactiveAnalysis(parse_proactive_analysis(&args)?),
        "project_analytics" => ToolArgs::ProjectAnalytics(parse_project_analytics(&args)?),
        "backup_create" => ToolArgs::BackupCreate(parse_backup_create(&args)?),
        "backup_preview" => ToolArgs::BackupPreview(parse_backup_preview(&args)?),
        "backup_restore" => ToolArgs::BackupRestore(parse_backup_restore(&args)?),
        other => {
            return Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            ));
        }
    };
    Ok(DomainRequest::ToolCall { tool: tool_args })
}

// ---- Argument parsers (validate against the frozen schema) ----

/// Allowed argument keys per tool, derived from the served input schemas
/// (frozen baseline + native tools). `route_tool` rejects anything else.
static ALLOWED_ARGUMENTS: std::sync::LazyLock<std::collections::HashMap<String, Vec<String>>> =
    std::sync::LazyLock::new(|| {
        let mut map = std::collections::HashMap::new();
        for tool in crate::compatibility::lemma::schemas::frozen_tools() {
            let keys = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            map.insert(tool.name, keys);
        }
        map.insert("backup_create".to_string(), vec!["directory".to_string()]);
        map.insert("backup_preview".to_string(), vec!["path".to_string()]);
        map.insert(
            "backup_restore".to_string(),
            vec!["confirmation_token".to_string(), "confirm".to_string()],
        );
        map
    });

fn allowed_arguments(name: &str) -> Option<Vec<String>> {
    ALLOWED_ARGUMENTS.get(name).cloned()
}

// Explicit nulls are absent (lenient: hosts send null for "not set").
// Any other present-but-wrong-typed value is an error, never a silent
// default: a string limit must not become "unbounded".
fn str_field<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a string"),
            None,
        )),
    }
}

fn bool_field(args: &Map<String, Value>, key: &str) -> Result<bool, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a boolean"),
            None,
        )),
    }
}

fn usize_field(args: &Map<String, Value>, key: &str) -> Result<Option<usize>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) => Ok(Some(v as usize)),
            None => Err(McpError::invalid_params(
                format!("{key} must be an integer"),
                None,
            )),
        },
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be an integer"),
            None,
        )),
    }
}

fn f64_field(args: &Map<String, Value>, key: &str) -> Result<Option<f64>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(v) => Ok(Some(v)),
            None => Err(McpError::invalid_params(
                format!("{key} must be a number"),
                None,
            )),
        },
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a number"),
            None,
        )),
    }
}

fn response_format_field(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<ResponseFormat>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(ResponseFormat::parse(s).ok_or_else(|| {
            McpError::invalid_params(format!("{key} must be a known response format"), None)
        })?)),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a string"),
            None,
        )),
    }
}

fn require_str(args: &Map<String, Value>, key: &str) -> Result<String, McpError> {
    str_field(args, key)?
        .map(|s| s.to_string())
        .ok_or_else(|| McpError::invalid_params(format!("{key} is required"), None))
}

/// Optional boolean: absent/null means unset (caller default applies);
/// present values must be booleans.
fn opt_bool_field(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a boolean"),
            None,
        )),
    }
}

/// Project scope normalized at the parse boundary (trimmed, basename,
/// lowercased): reads and writes meet on the canonical form instead of
/// comparing raw input against normalized storage.
fn project_field(args: &Map<String, Value>, key: &str) -> Result<Option<String>, McpError> {
    Ok(match str_field(args, key)? {
        None => None,
        Some(raw) => crate::compatibility::lemma::tool_args::normalize_project(raw),
    })
}

fn parse_memory_read(args: &Map<String, Value>) -> Result<MemoryReadArgs, McpError> {
    Ok(MemoryReadArgs {
        project: project_field(args, "project")?,
        query: str_field(args, "query")?.map(|s| s.to_string()),
        id: str_field(args, "id")?.map(|s| s.to_string()),
        context: str_field(args, "context")?.map(|s| s.to_string()),
        all: bool_field(args, "all")?,
        ids: match args.get("ids") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|x| {
                        x.as_str()
                            .map(|s| s.to_string())
                            .ok_or_else(|| McpError::invalid_params("ids must be strings", None))
                    })
                    .collect::<Result<_, _>>()?,
            ),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "ids must be an array of strings",
                    None,
                ));
            }
        },
        min_confidence: f64_field(args, "minConfidence")?,
        after_date: str_field(args, "afterDate")?.map(|s| s.to_string()),
        before_date: str_field(args, "beforeDate")?.map(|s| s.to_string()),
        limit: usize_field(args, "limit")?,
        offset: usize_field(args, "offset")?,
        response_format: response_format_field(args, "response_format")?,
        expand_graph: bool_field(args, "expand_graph")?,
        explain: bool_field(args, "explain")?,
    })
}

/// Parse the frozen evidence sub-object (memory_add): closed shape —
/// unknown keys error like top-level args instead of silently dropping
/// hints; wrong-typed fields name the field instead of defaulting.
fn parse_evidence_object(
    o: &Map<String, Value>,
) -> Result<crate::compatibility::lemma::tool_args::MemoryEvidence, McpError> {
    for key in o.keys() {
        if !matches!(key.as_str(), "file" | "symbol" | "snippet") {
            return Err(McpError::invalid_params(
                format!("unknown evidence field: {key}"),
                None,
            ));
        }
    }
    let file = o
        .get("file")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::invalid_params("evidence.file is required", None))?
        .to_string();
    let snippet = o
        .get("snippet")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::invalid_params("evidence.snippet is required", None))?
        .to_string();
    let symbol = match o.get("symbol") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.to_string()),
        Some(_) => {
            return Err(McpError::invalid_params(
                "evidence.symbol must be a string",
                None,
            ));
        }
    };
    Ok(crate::compatibility::lemma::tool_args::MemoryEvidence {
        file,
        symbol,
        snippet,
    })
}

fn parse_memory_add(args: &Map<String, Value>) -> Result<MemoryAddArgs, McpError> {
    let fragment = require_str(args, "fragment")?;
    let evidence = args
        .get("evidence")
        .map(|v| match v {
            Value::Null => Ok(None),
            Value::Object(o) => parse_evidence_object(o).map(Some),
            _ => Err(McpError::invalid_params("evidence must be an object", None)),
        })
        .transpose()?
        .flatten();
    Ok(MemoryAddArgs {
        fragment,
        title: str_field(args, "title")?.map(|s| s.to_string()),
        description: str_field(args, "description")?.map(|s| s.to_string()),
        project: project_field(args, "project")?,
        source: str_field(args, "source")?.map(|s| s.to_string()),
        confirm: bool_field(args, "confirm")?,
        fragment_type: str_field(args, "type")?.map(|s| s.to_string()),
        evidence,
    })
}

fn parse_memory_update(args: &Map<String, Value>) -> Result<MemoryUpdateArgs, McpError> {
    Ok(MemoryUpdateArgs {
        id: require_str(args, "id")?,
        title: str_field(args, "title")?.map(|s| s.to_string()),
        fragment: str_field(args, "fragment")?.map(|s| s.to_string()),
        confidence: f64_field(args, "confidence")?,
    })
}

fn parse_memory_feedback(args: &Map<String, Value>) -> Result<MemoryFeedbackArgs, McpError> {
    // Absent/null and wrong-typed fail distinctly: "required" means the
    // caller omitted it, "must be a boolean" means they sent garbage.
    // (bool_field would silently default absent to false — wrong here.)
    let useful = match args.get("useful") {
        None | Some(Value::Null) => {
            return Err(McpError::invalid_params("useful is required", None));
        }
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(McpError::invalid_params("useful must be a boolean", None));
        }
    };
    Ok(MemoryFeedbackArgs {
        id: require_str(args, "id")?,
        useful,
    })
}

fn parse_memory_forget(args: &Map<String, Value>) -> Result<MemoryForgetArgs, McpError> {
    Ok(MemoryForgetArgs {
        id: require_str(args, "id")?,
        consolidate: bool_field(args, "consolidate")?,
        invalidate: bool_field(args, "invalidate")?,
    })
}

fn parse_memory_merge(args: &Map<String, Value>) -> Result<MemoryMergeArgs, McpError> {
    let ids = args
        .get("ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| McpError::invalid_params("ids is required", None))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| McpError::invalid_params("ids must be strings", None))
        })
        .collect::<Result<_, _>>()?;
    Ok(MemoryMergeArgs {
        ids,
        title: require_str(args, "title")?,
        fragment: require_str(args, "fragment")?,
        project: project_field(args, "project")?,
        consolidate: bool_field(args, "consolidate")?,
    })
}

fn parse_memory_relate(args: &Map<String, Value>) -> Result<MemoryRelateArgs, McpError> {
    let relation_type = require_str(args, "type")?;
    if crate::domain::relation::RelationType::parse(&relation_type).is_none() {
        return Err(McpError::invalid_params(
            format!("invalid relation type: {relation_type}"),
            None,
        ));
    }
    Ok(MemoryRelateArgs {
        source_id: require_str(args, "sourceId")?,
        target_id: require_str(args, "targetId")?,
        relation_type,
        note: str_field(args, "note")?.map(|s| s.to_string()),
    })
}

fn parse_memory_stats(args: &Map<String, Value>) -> Result<MemoryStatsArgs, McpError> {
    Ok(MemoryStatsArgs {
        project: project_field(args, "project")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

fn parse_memory_audit(args: &Map<String, Value>) -> Result<MemoryAuditArgs, McpError> {
    Ok(MemoryAuditArgs {
        response_format: response_format_field(args, "response_format")?,
    })
}

fn parse_memory_library(args: &Map<String, Value>) -> Result<MemoryLibraryArgs, McpError> {
    Ok(MemoryLibraryArgs {
        project: project_field(args, "project")?,
        focus: str_field(args, "focus")?.map(|s| s.to_string()),
        limit: usize_field(args, "limit")?,
        offset: usize_field(args, "offset")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

fn parse_semantic_search(args: &Map<String, Value>) -> Result<SemanticSearchArgs, McpError> {
    Ok(SemanticSearchArgs {
        query: require_str(args, "query")?,
        project: project_field(args, "project")?,
        top_k: usize_field(args, "topK")?,
        offset: usize_field(args, "offset")?,
        hybrid: opt_bool_field(args, "hybrid")?,
        explain: bool_field(args, "explain")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

fn str_array_field(args: &Map<String, Value>, key: &str) -> Result<Vec<String>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| McpError::invalid_params(format!("{key} must be strings"), None))
            })
            .collect(),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be an array of strings"),
            None,
        )),
    }
}

fn parse_guide_get(args: &Map<String, Value>) -> Result<GuideGetArgs, McpError> {
    Ok(GuideGetArgs {
        category: str_field(args, "category")?.map(|s| s.to_string()),
        guide: str_field(args, "guide")?.map(|s| s.to_string()),
        task: str_field(args, "task")?.map(|s| s.to_string()),
        response_format: response_format_field(args, "response_format")?,
    })
}

fn parse_guide_practice(args: &Map<String, Value>) -> Result<GuidePracticeArgs, McpError> {
    Ok(GuidePracticeArgs {
        guide: require_str(args, "guide")?,
        category: require_str(args, "category")?,
        description: str_field(args, "description")?.map(|s| s.to_string()),
        contexts: str_array_field(args, "contexts")?,
        learnings: str_array_field(args, "learnings")?,
        outcome: str_field(args, "outcome")?.map(|s| s.to_string()),
    })
}

fn parse_guide_create(args: &Map<String, Value>) -> Result<GuideCreateArgs, McpError> {
    Ok(GuideCreateArgs {
        guide: require_str(args, "guide")?,
        category: require_str(args, "category")?,
        description: require_str(args, "description")?,
        contexts: str_array_field(args, "contexts")?,
        learnings: str_array_field(args, "learnings")?,
    })
}

fn parse_guide_distill(args: &Map<String, Value>) -> Result<GuideDistillArgs, McpError> {
    Ok(GuideDistillArgs {
        memory_id: require_str(args, "memory_id")?,
        guide: require_str(args, "guide")?,
        category: str_field(args, "category")?.map(|s| s.to_string()),
    })
}

fn parse_guide_update(args: &Map<String, Value>) -> Result<GuideUpdateArgs, McpError> {
    Ok(GuideUpdateArgs {
        guide: require_str(args, "guide")?,
        new_name: str_field(args, "new_name")?.map(|s| s.to_string()),
        category: str_field(args, "category")?.map(|s| s.to_string()),
        description: str_field(args, "description")?.map(|s| s.to_string()),
        add_anti_patterns: str_array_field(args, "add_anti_patterns")?,
        add_pitfalls: str_array_field(args, "add_pitfalls")?,
        add_depends_on: str_array_field(args, "add_depends_on")?,
        add_enables: str_array_field(args, "add_enables")?,
        superseded_by: str_field(args, "superseded_by")?.map(|s| s.to_string()),
        deprecated: bool_field(args, "deprecated")?,
    })
}

fn parse_guide_forget(args: &Map<String, Value>) -> Result<GuideForgetArgs, McpError> {
    Ok(GuideForgetArgs {
        guide: require_str(args, "guide")?,
    })
}

fn parse_guide_merge(args: &Map<String, Value>) -> Result<GuideMergeArgs, McpError> {
    Ok(GuideMergeArgs {
        guides: str_array_field(args, "guides")?,
        guide: require_str(args, "guide")?,
        category: require_str(args, "category")?,
        description: str_field(args, "description")?.map(|s| s.to_string()),
        contexts: {
            match args.get("contexts") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .map(|x| {
                            x.as_str().map(|s| s.to_string()).ok_or_else(|| {
                                McpError::invalid_params("contexts must be strings", None)
                            })
                        })
                        .collect::<Result<_, _>>()?,
                ),
                Some(_) => {
                    return Err(McpError::invalid_params(
                        "contexts must be an array of strings",
                        None,
                    ));
                }
            }
        },
        learnings: {
            match args.get("learnings") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .map(|x| {
                            x.as_str().map(|s| s.to_string()).ok_or_else(|| {
                                McpError::invalid_params("learnings must be strings", None)
                            })
                        })
                        .collect::<Result<_, _>>()?,
                ),
                Some(_) => {
                    return Err(McpError::invalid_params(
                        "learnings must be an array of strings",
                        None,
                    ));
                }
            }
        },
    })
}

fn parse_session_start(args: &Map<String, Value>) -> Result<SessionStartArgs, McpError> {
    Ok(SessionStartArgs {
        task_type: require_str(args, "task_type")?,
        technologies: str_array_field(args, "technologies")?,
        initial_approach: str_field(args, "initial_approach")?.map(|s| s.to_string()),
    })
}

fn parse_session_attempt(args: &Map<String, Value>) -> Result<SessionAttemptArgs, McpError> {
    Ok(SessionAttemptArgs {
        approach: require_str(args, "approach")?,
        outcome: require_str(args, "outcome")?,
        critique: str_field(args, "critique")?.map(|s| s.to_string()),
        rationale: str_field(args, "rationale")?.map(|s| s.to_string()),
        related_memory_id: str_field(args, "related_memory_id")?.map(|s| s.to_string()),
    })
}

fn parse_session_end(args: &Map<String, Value>) -> Result<SessionEndArgs, McpError> {
    Ok(SessionEndArgs {
        outcome: require_str(args, "outcome")?,
        final_approach: str_field(args, "final_approach")?.map(|s| s.to_string()),
        lessons: str_array_field(args, "lessons")?,
    })
}

fn parse_session_stats(args: &Map<String, Value>) -> Result<SessionStatsArgs, McpError> {
    Ok(SessionStatsArgs {
        count: usize_field(args, "count")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

fn parse_suggestion_respond(args: &Map<String, Value>) -> Result<SuggestionRespondArgs, McpError> {
    Ok(SuggestionRespondArgs {
        id: args
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| McpError::invalid_params("id is required", None))?,
        action: require_str(args, "action")?,
    })
}

fn parse_conflict_scan(args: &Map<String, Value>) -> Result<ConflictScanArgs, McpError> {
    Ok(ConflictScanArgs {
        project: project_field(args, "project")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

fn parse_proactive_analysis(args: &Map<String, Value>) -> Result<ProactiveAnalysisArgs, McpError> {
    Ok(ProactiveAnalysisArgs {
        project: project_field(args, "project")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

fn parse_project_analytics(args: &Map<String, Value>) -> Result<ProjectAnalyticsArgs, McpError> {
    Ok(ProjectAnalyticsArgs {
        project: project_field(args, "project")?,
        response_format: response_format_field(args, "response_format")?,
    })
}

/// Parse backup_preview args (`path` optional at the wire level, required
/// at execution).
fn parse_backup_preview(args: &Map<String, Value>) -> Result<BackupPreviewArgs, McpError> {
    Ok(BackupPreviewArgs {
        path: str_field(args, "path")?.map(|s| s.to_string()),
    })
}

/// Parse backup_restore args (both optional at the wire level; execution
/// requires an unused token plus explicit confirmation).
fn parse_backup_restore(args: &Map<String, Value>) -> Result<BackupRestoreArgs, McpError> {
    Ok(BackupRestoreArgs {
        confirmation_token: str_field(args, "confirmation_token")?.map(|s| s.to_string()),
        confirm: opt_bool_field(args, "confirm")?,
    })
}

/// Parse backup_create args. `directory` is optional at the wire level;
/// execution requires it (ltmrs invents no default backup location).
fn parse_backup_create(args: &Map<String, Value>) -> Result<BackupCreateArgs, McpError> {
    Ok(BackupCreateArgs {
        directory: str_field(args, "directory")?.map(|s| s.to_string()),
    })
}

/// A native (non-frozen) tool definition: same shape as frozen entries,
/// clearly marked so tools/list consumers can tell parity from extension.
fn native_tool(
    name: &'static str,
    description: &'static str,
    properties: serde_json::Value,
) -> Tool {
    let schema: Map<String, Value> = serde_json::json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    })
    .as_object()
    .expect("native schema is an object")
    .clone();
    Tool::new(name, description, Arc::new(schema)).with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(false)
            .open_world(false),
    )
}

fn native_backup_create_tool() -> Tool {
    native_tool(
        "backup_create",
        "Back up the canonical store (memories, relations, guides, feedback, suggestions, sessions) to one portable .ltmrs-backup file and verify it. NATIVE ltmrs tool (not a verbatim Lemma port): output shapes follow ltmrs conventions.",
        serde_json::json!({
            "directory": {
                "type": "string",
                "description": "Destination directory for the .ltmrs-backup file (created when missing). Required: ltmrs invents no default backup location.",
            },
        }),
    )
}

/// Native backup_preview definition (readiness + single-use token).
fn native_backup_preview_tool() -> Tool {
    native_tool(
        "backup_preview",
        "Validate an ltmrs backup, compare record counts, and check cooperating connections without replacing anything. Also reports unknown (future-producer) top-level keys the restore would drop (counted, not restored). NATIVE ltmrs tool: on readiness, returns a single-use confirmation token (10-minute TTL, bound to file digest, store generation and channel); closing other connections may be required first.",
        serde_json::json!({
            "path": {
                "type": "string",
                "description": "Absolute path to the .ltmrs-backup file on this computer.",
            },
        }),
    )
}

/// Native backup_restore definition (explicit confirmation, replace semantics).
fn native_backup_restore_tool() -> Tool {
    native_tool(
        "backup_restore",
        "Restore a previewed ltmrs backup, REPLACING the live store (never merging). NATIVE ltmrs tool: requires the single-use confirmation_token from backup_preview plus explicit confirm=true. A safety backup is written first; canonical sessions restore from the backup while channel bindings and virtual live sessions stay registry-side. The report includes quarantined references and dropped unknown-key counts for manual repair.",
        serde_json::json!({
            "confirmation_token": {
                "type": "string",
                "description": "Single-use token from backup_preview.",
            },
            "confirm": {
                "type": "boolean",
                "description": "Must be true: acknowledges replacement semantics.",
            },
        }),
    )
}

impl ServerHandler for LtmrsFrontend {
    fn get_info(&self) -> InitializeResult {
        // Static teaching template always present; the dynamic memory index is
        // appended when a snapshot has been cached (upstream buildInstructions).
        let memories = self.snapshot.lock().unwrap().clone().unwrap_or_default();
        let project = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_lowercase()))
            .filter(|p| !p.is_empty());
        let instructions = format!(
            "{}{}",
            INSTRUCTIONS_TEMPLATE,
            build_instructions_index(&memories, project.as_deref())
        );
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ltmrs", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions)
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + MaybeSendFuture + '_
    {
        std::future::ready(Ok(ListToolsResult {
            result_type: None,
            meta: None,
            next_cursor: None,
            ttl_ms: None,
            cache_scope: None,
            tools: Self::tools(),
        }))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // The client is shared across calls; the resilient round-trip owns
        // the lock, (re-)handshakes as needed, and recovers once from a
        // post-restore StaleGeneration on an established connection.
        let body = route_tool(&request.name, &request.arguments)?;
        let resp = self.roundtrip_with_rehandshake(body).await?;

        // Memory-mutating tools trigger a tools/list_changed notification
        // (upstream notifyMemoryChange). Best-effort; failures are silent.
        if is_mutating_tool(&request.name) {
            let _ = context
                .peer
                .send_notification(ServerNotification::ToolListChangedNotification(
                    ToolListChangedNotification {
                        method: ToolListChangedNotificationMethod,
                        extensions: Default::default(),
                    },
                ))
                .await;
            // Refresh the snapshot so the next instructions reflect the change.
            let mut client = self.client.lock().await;
            if let Ok(memories) = self.fetch_snapshot(&mut client).await {
                self.set_snapshot(memories);
            }
        }

        // Shape the daemon's tool result into the wire response.
        Ok(CallToolResponse::Complete(shape_result(&resp)))
    }
}

/// Tools that mutate canonical memory state (upstream calls notifyMemoryChange
/// after these). backup_restore replaces the whole store, so hosts must
/// re-observe after it above all others.
fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "memory_add"
            | "memory_update"
            | "memory_feedback"
            | "memory_forget"
            | "memory_merge"
            | "memory_relate"
            | "backup_restore"
    )
}

/// Convert an IPC response into a shaped MCP `CallToolResult`.
fn shape_result(resp: &crate::daemon::envelope::IpcResponse) -> CallToolResult {
    use crate::daemon::envelope::{DomainPayload, IpcResult};
    match &resp.result {
        IpcResult::Success { payload, .. } => match payload {
            DomainPayload::ToolResult {
                text,
                structured,
                is_error,
            } => {
                let mut result = if *is_error {
                    CallToolResult::error(vec![ContentBlock::text(text.clone())])
                } else if let Some(s) = structured {
                    CallToolResult::structured(s.clone())
                } else {
                    CallToolResult::success(vec![ContentBlock::text(text.clone())])
                };
                // For structured results, also carry the human-readable text.
                if !*is_error && structured.is_some() {
                    result.content = vec![ContentBlock::text(text.clone())];
                }
                result
            }
            _ => CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string(resp).unwrap_or_default(),
            )]),
        },
        IpcResult::Error { message, .. } => {
            CallToolResult::error(vec![ContentBlock::text(format!("Error: {message}"))])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::EntityId;
    use serde_json::json;

    fn args(map: Map<String, Value>) -> Option<Map<String, Value>> {
        Some(map)
    }

    #[test]
    fn serves_all_frozen_tools() {
        let tools = LtmrsFrontend::tools();
        assert_eq!(
            tools.len(),
            29,
            "26 frozen-verbatim + 3 native (backup_create/preview/restore)"
        );
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"memory_read".to_string()));
        assert!(names.contains(&"semantic_search".to_string()));
        assert!(names.contains(&"guide_get".to_string()));
        assert!(names.contains(&"session_start".to_string()));
        assert!(names.contains(&"conflict_scan".to_string()));
    }

    #[test]
    fn served_tools_match_frozen_schema() {
        // T-MCP-01: served tools must match the frozen baseline verbatim.
        let frozen: Vec<(String, String, bool, bool, bool, bool, bool)> =
            crate::compatibility::lemma::schemas::frozen_tools()
                .into_iter()
                .map(|ft| {
                    (
                        ft.name,
                        ft.description,
                        ft.read_only_hint,
                        ft.destructive_hint,
                        ft.idempotent_hint,
                        ft.open_world_hint,
                        ft.output_schema.is_some(),
                    )
                })
                .collect();
        let served = LtmrsFrontend::tools();
        // Frozen-verbatim prefix first, native extensions after. The native
        // tail is pinned by name below (count-agnostic if more land later).
        assert!(
            served.len() >= frozen.len(),
            "served tools must cover the frozen set"
        );
        let native: Vec<&str> = served[frozen.len()..]
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect();
        assert_eq!(
            native,
            vec!["backup_create", "backup_preview", "backup_restore"]
        );
        let served = &served[..frozen.len()];
        for (tool, (name, desc, ro, de, id, ow, has_os)) in served.iter().zip(frozen.iter()) {
            assert_eq!(tool.name.as_ref(), name, "tool name drift");
            assert_eq!(
                tool.description.as_deref(),
                Some(desc.as_str()),
                "description drift: {name}"
            );
            let ann = tool.annotations.as_ref().unwrap();
            assert_eq!(ann.read_only_hint, Some(*ro), "read_only drift: {name}");
            assert_eq!(ann.destructive_hint, Some(*de), "destructive drift: {name}");
            assert_eq!(ann.idempotent_hint, Some(*id), "idempotent drift: {name}");
            assert_eq!(ann.open_world_hint, Some(*ow), "open_world drift: {name}");
            assert_eq!(
                tool.output_schema.is_some(),
                *has_os,
                "output_schema presence drift: {name}"
            );
        }
    }

    fn mem(id: u64, project: Option<&str>, confidence: f64, title: &str) -> Memory {
        Memory {
            id: EntityId::new(Uuid::from_u128(id as u128)),
            external_alias: Some(crate::domain::id::ExternalAlias::new(format!("m{id:012x}"))),
            title: title.to_string(),
            fragment: format!("frag-{title}"),
            description: String::new(),
            fragment_type: crate::domain::memory::FragmentType::Fact,
            project: project.map(|p| p.to_string()),
            source: crate::domain::memory::MemorySource::Ai,
            confidence,
            quality_score: None,
            lifecycle: crate::domain::memory::MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: crate::domain::id::EntityRevision::new(1),
            document_revision: crate::domain::id::DocumentRevision::new(1),
            eligibility_revision: crate::domain::id::EligibilityRevision::new(1),
            created_at: crate::domain::memory::Instant::new(1),
            updated_at: crate::domain::memory::Instant::new(1),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn instructions_index_empty_state() {
        let idx = build_instructions_index(&[], Some("proj"));
        assert!(idx.contains("no saved memories yet"));
    }

    #[test]
    fn instructions_index_lists_project_and_global() {
        let memories = vec![
            mem(1, Some("proj"), 0.9, "Proj Frag A"),
            mem(2, Some("proj"), 0.7, "Proj Frag B"),
            mem(3, None, 0.8, "Global Frag"),
            mem(4, Some("other"), 0.99, "Other Proj"),
        ];
        let idx = build_instructions_index(&memories, Some("proj"));
        assert!(idx.contains("- Project \"proj\": 2 fragments"));
        assert!(idx.contains("- Global: 1 fragments"));
        assert!(idx.contains("[m000000000001] Proj Frag A (0.90)"));
        assert!(idx.contains("[m000000000003] Global Frag (0.80)"));
        // Other project must not leak into this frontend's index.
        assert!(!idx.contains("Other Proj"));
    }

    #[test]
    fn mutating_tools_trigger_notification() {
        for t in [
            "memory_add",
            "memory_update",
            "memory_feedback",
            "memory_forget",
            "memory_merge",
            "memory_relate",
            // Whole-store replace: hosts must re-observe above all others.
            "backup_restore",
        ] {
            assert!(is_mutating_tool(t), "{t} must be mutating");
        }
        for t in [
            "memory_read",
            "semantic_search",
            "memory_stats",
            "memory_library",
        ] {
            assert!(!is_mutating_tool(t), "{t} must be read-only");
        }
    }

    #[test]
    fn get_info_includes_instructions_template() {
        let id = FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        );
        let client = IpcClient::new(std::path::PathBuf::from("unused"));
        let fe = LtmrsFrontend::new(id, client);
        let info = fe.get_info();
        let instructions = info.instructions.as_deref().unwrap();
        assert!(instructions.starts_with("# Lemma — Persistent Memory"));
        assert!(instructions.contains("RECALL: memory_read"));
    }

    /// Post-restore regression: a frontend constructed at generation FIRST
    /// must handshake against a daemon at generation 2 by adopting the live
    /// generation (not brick with GenerationMismatch).
    #[tokio::test]
    async fn handshake_adopts_live_generation_after_restore() {
        use crate::daemon::runtime::RuntimePaths;
        use crate::daemon::server::{Daemon, DaemonConfig, handle_connection};

        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        // Simulate post-restore: the live generation moves to 2 while the
        // frontend still believes FIRST.
        daemon
            .dispatcher_arc()
            .repo()
            .set_store_generation(crate::domain::id::StoreGeneration::new(2))
            .unwrap();
        let dispatcher = daemon.dispatcher_arc();
        let quotas = daemon.quotas();
        let socket = dir.path().join("test.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server_handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let dispatcher = dispatcher.clone();
                let quotas = quotas.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, dispatcher, quotas).await;
                });
            }
        });

        let mut client = IpcClient::new(socket);
        client.connect().await.unwrap();
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(1)),
                ChannelId::new(Uuid::from_u128(2)),
            ),
            client,
        );
        fe.ensure_handshaked().await.unwrap();
        assert_eq!(
            fe.identity.generation(),
            crate::domain::id::StoreGeneration::new(2),
            "frontend must adopt the live generation"
        );

        drop(daemon);
        server_handle.abort();
    }

    /// Established-connection regression (review Critical): after a second
    /// restore bumps the live generation, an already-handshaked frontend
    /// must re-handshake and retry once instead of failing StaleGeneration
    /// on every subsequent call.
    #[tokio::test]
    async fn established_connection_rehandshakes_on_stale_generation() {
        use crate::daemon::runtime::RuntimePaths;
        use crate::daemon::server::{Daemon, DaemonConfig, handle_connection};

        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        daemon
            .dispatcher_arc()
            .repo()
            .set_store_generation(crate::domain::id::StoreGeneration::new(2))
            .unwrap();
        let dispatcher = daemon.dispatcher_arc();
        let quotas = daemon.quotas();
        let socket = dir.path().join("test.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server_handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let dispatcher = dispatcher.clone();
                let quotas = quotas.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, dispatcher, quotas).await;
                });
            }
        });

        let mut client = IpcClient::new(socket);
        client.connect().await.unwrap();
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(1)),
                ChannelId::new(Uuid::from_u128(2)),
            ),
            client,
        );
        fe.ensure_handshaked().await.unwrap();
        assert_eq!(
            fe.identity.generation(),
            crate::domain::id::StoreGeneration::new(2)
        );
        // Second restore while the connection is established.
        daemon
            .dispatcher_arc()
            .repo()
            .set_store_generation(crate::domain::id::StoreGeneration::new(3))
            .unwrap();
        let resp = fe
            .roundtrip_with_rehandshake(DomainRequest::ListMemories)
            .await
            .unwrap();
        assert!(
            matches!(
                resp.result,
                crate::daemon::envelope::IpcResult::Success { .. }
            ),
            "established connection must recover, got {:?}",
            resp.result
        );
        assert_eq!(
            fe.identity.generation(),
            crate::domain::id::StoreGeneration::new(3),
            "frontend must adopt the newest live generation"
        );

        drop(daemon);
        server_handle.abort();
    }

    /// P1-B unknown-outcome recovery, end to end: the server commits a
    /// mutation then drops the connection without responding. The frontend
    /// must reconnect, resume the SAME retry epoch, and resend the SAME
    /// envelope — the daemon replays its receipt (a duplicate-ID AddMemory
    /// would fail as "already exists" if it executed twice). Exactly one
    /// memory exists afterwards. Deterministic: the drop point is
    /// server-controlled, not timed.
    #[tokio::test]
    async fn unknown_outcome_resends_same_envelope_after_resume() {
        use crate::daemon::dispatcher::Dispatcher;
        use crate::daemon::registry::FrontendRegistry;
        use crate::domain::clock::{Clock, FrozenClock};
        use crate::service::repository::CanonicalRepository;
        use tokio::io::AsyncReadExt;

        async fn read_msg(
            stream: &mut tokio::net::UnixStream,
        ) -> crate::daemon::envelope::WireMessage {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.unwrap();
            serde_json::from_slice(&buf).unwrap()
        }
        async fn write_msg(
            stream: &mut tokio::net::UnixStream,
            reply: &crate::daemon::envelope::WireReply,
        ) {
            // Same streaming reply format as the production server
            // (8-byte total + chunk frames), so the client parses it.
            let payload = serde_json::to_vec(reply).unwrap();
            crate::daemon::envelope::write_response_payload(stream, &payload)
                .await
                .unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(
                dir.path().join("store").to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        // Epoch 1 will be issued by the first handshake below; the
        // frontend must resume exactly it after the drop.
        let dispatcher = Arc::new(Dispatcher::new(
            Arc::clone(&repo),
            FrontendRegistry::new(),
            Arc::clone(&clock),
        ));
        let socket = dir.path().join("resend.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();

        // Connection 1: handshake normally, then commit the request and
        // drop WITHOUT responding — a deterministic unknown outcome.
        // Connection 2 serves normally (real server path, including the
        // resume handshake).
        let server_task = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            async move {
                use crate::daemon::envelope::WireMessage;
                let (mut conn1, _) = listener.accept().await.unwrap();
                let m1 = read_msg(&mut conn1).await;
                let WireMessage::Handshake(hs_req) = m1 else {
                    panic!("expected handshake first");
                };
                assert_eq!(hs_req.resume_retry_epoch, None);
                let hs = dispatcher.handle_handshake(&hs_req).unwrap();
                // The epoch the frontend must resume on reconnect.
                let first_epoch = hs.retry_epoch;
                write_msg(
                    &mut conn1,
                    &crate::daemon::envelope::WireReply::Handshake(hs),
                )
                .await;
                let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                    panic!("expected snapshot prefetch second");
                };
                // Serve the snapshot prefetch normally (it is part of the
                // frontend handshake path, not the mutation under test).
                let prefetch_resp = dispatcher.handle(&env).unwrap();
                write_msg(
                    &mut conn1,
                    &crate::daemon::envelope::WireReply::Response(prefetch_resp),
                )
                .await;
                // The actual mutation: commit, then drop WITHOUT responding
                // — a deterministic unknown outcome.
                let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                    panic!("expected mutation request third");
                };
                // The operation ID the frontend must resend verbatim.
                let first_op = env.operation_id;
                dispatcher.handle(&env).unwrap();
                drop(conn1);
                let (mut conn2, _) = listener.accept().await.unwrap();
                let m2 = read_msg(&mut conn2).await;
                let WireMessage::Handshake(resume_req) = m2 else {
                    panic!("expected resume handshake on conn2");
                };
                assert_eq!(
                    resume_req.resume_retry_epoch,
                    Some(first_epoch),
                    "reconnect must resume the same epoch"
                );
                let hs2 = dispatcher.handle_handshake(&resume_req).unwrap();
                assert_eq!(hs2.retry_epoch, first_epoch);
                write_msg(
                    &mut conn2,
                    &crate::daemon::envelope::WireReply::Handshake(hs2),
                )
                .await;
                let WireMessage::Request(env2) = read_msg(&mut conn2).await else {
                    panic!("expected resent request on conn2");
                };
                assert_eq!(
                    env2.operation_id, first_op,
                    "reconnect must resend the same operation"
                );
                let resp = dispatcher.handle(&env2).unwrap();
                write_msg(
                    &mut conn2,
                    &crate::daemon::envelope::WireReply::Response(resp),
                )
                .await;
            }
        });

        let client = IpcClient::new(socket);
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(1)),
                ChannelId::new(Uuid::from_u128(2)),
            ),
            client,
        );
        let mut memory = mem(7, None, 0.5, "Resend Me");
        memory.external_alias = None;
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fe.roundtrip_with_rehandshake(DomainRequest::AddMemory { memory }),
        )
        .await
        .expect("resend must not hang")
        .expect("unknown-outcome call must resolve");
        assert!(
            matches!(resp.result, IpcResult::Success { .. }),
            "resend must replay the recorded outcome, got {:?}",
            resp.result
        );
        server_task.await.unwrap();
        // Exactly one memory: the resend replayed, never re-executed (a
        // second AddMemory with the same ID fails as "already exists").
        let memories = repo
            .export_snapshot()
            .unwrap()
            .memories
            .into_iter()
            .filter(|m| m.title == "Resend Me")
            .collect::<Vec<_>>();
        assert_eq!(memories.len(), 1, "exactly one effect allowed");

        drop(fe);
    }

    /// Bridged post-restore regression (release Critical): the stdio bridge
    /// has no socket listener to redial, so an established bridged frontend
    /// must survive a generation bump transparently — stale answer,
    /// epoch-only forget, same-stream re-handshake, one retry. Timeout
    /// guarded: the pre-fix shape parked forever on a dead socket dial.
    #[tokio::test]
    async fn bridged_frontend_survives_generation_bump() {
        use crate::daemon::runtime::RuntimePaths;
        use crate::daemon::server::{Daemon, DaemonConfig, handle_connection};

        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        let dispatcher = daemon.dispatcher_arc();
        let quotas = daemon.quotas();
        let (server_end, client_end) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let _ = handle_connection(server_end, dispatcher, quotas).await;
        });

        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_end);
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(1)),
                ChannelId::new(Uuid::from_u128(2)),
            ),
            client,
        );
        // Establish at generation 1.
        let first = fe
            .roundtrip_with_rehandshake(DomainRequest::ListMemories)
            .await
            .unwrap();
        assert!(
            matches!(
                first.result,
                crate::daemon::envelope::IpcResult::Success { .. }
            ),
            "baseline call must serve, got {:?}",
            first.result
        );
        // Restore bumps the live generation mid-session.
        daemon
            .dispatcher_arc()
            .repo()
            .set_store_generation(crate::domain::id::StoreGeneration::new(2))
            .unwrap();
        let recovered = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            fe.roundtrip_with_rehandshake(DomainRequest::ListMemories),
        )
        .await
        .expect("post-restore call must complete, never park");
        let resp = recovered.unwrap();
        assert!(
            matches!(
                resp.result,
                crate::daemon::envelope::IpcResult::Success { .. }
            ),
            "established bridged connection must recover, got {:?}",
            resp.result
        );
        assert_eq!(
            fe.identity.generation(),
            crate::domain::id::StoreGeneration::new(2),
            "frontend must adopt the live generation"
        );

        drop(daemon);
    }

    /// Sticky-stream regression: a generically rejected handshake must close
    /// the (daemon-closed) stream instead of keeping a dead-but-connected
    /// client that fails every later call on the same stream.
    #[tokio::test]
    async fn rejected_handshake_closes_dead_stream() {
        use crate::daemon::envelope::{WireError, WireReply};
        use tokio::io::AsyncReadExt;

        let (server_end, client_end) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let mut server_end = server_end;
            let mut len_buf = [0u8; 4];
            if server_end.read_exact(&mut len_buf).await.is_err() {
                return;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            if server_end.read_exact(&mut buf).await.is_err() {
                return;
            }
            let reply = WireReply::Error(WireError {
                kind: "handshake_rejected".into(),
                message: "boom".into(),
            });
            let payload = serde_json::to_vec(&reply).unwrap();
            let _ =
                crate::daemon::envelope::write_response_payload(&mut server_end, &payload).await;
            // Daemon closes rejected handshakes: drop our end.
        });

        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_end);
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(1)),
                ChannelId::new(Uuid::from_u128(2)),
            ),
            client,
        );
        assert!(fe.ensure_handshaked().await.is_err());
        assert!(
            !fe.client.lock().await.is_connected(),
            "rejected handshake must not leave a dead-but-connected stream"
        );
    }

    /// Sticky-stream regression: a transport failure mid-call (broken pipe,
    /// daemon restart) must forget the handshake so the next call
    /// reconnects instead of failing on the dead stream forever.
    #[tokio::test]
    async fn transport_failure_forgets_handshake() {
        use crate::daemon::runtime::RuntimePaths;
        use crate::daemon::server::{Daemon, DaemonConfig, handle_connection};

        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        let dispatcher = daemon.dispatcher_arc();
        let quotas = daemon.quotas();
        let socket = dir.path().join("test.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server_handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let dispatcher = dispatcher.clone();
                let quotas = quotas.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, dispatcher, quotas).await;
                });
            }
        });

        let mut client = IpcClient::new(socket);
        client.connect().await.unwrap();
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(1)),
                ChannelId::new(Uuid::from_u128(2)),
            ),
            client,
        );
        fe.ensure_handshaked().await.unwrap();
        assert!(fe.client.lock().await.retry_epoch().is_some());
        // Simulate a dead stream: swap in a pair end whose peer is gone.
        let (dead, peer) = tokio::net::UnixStream::pair().unwrap();
        drop(peer);
        fe.client.lock().await.set_stream(dead);
        assert!(
            fe.roundtrip_with_rehandshake(DomainRequest::ListMemories)
                .await
                .is_err()
        );
        assert!(
            fe.client.lock().await.retry_epoch().is_none(),
            "transport failure must forget the handshake epoch"
        );
        assert!(
            !fe.client.lock().await.is_connected(),
            "transport failure must drop the dead stream"
        );

        drop(daemon);
        server_handle.abort();
    }

    #[test]
    fn tools_carry_annotations_and_output_schema() {
        let tools = LtmrsFrontend::tools();
        let ss = tools.iter().find(|t| t.name == "semantic_search").unwrap();
        assert!(ss.annotations.as_ref().unwrap().read_only_hint == Some(true));
        assert!(
            ss.output_schema.is_some(),
            "semantic_search must expose output schema"
        );
    }

    #[test]
    fn routes_memory_add() {
        let mut m = Map::new();
        m.insert("fragment".into(), json!("hello world"));
        let req = route_tool("memory_add", &args(m)).unwrap();
        match req {
            DomainRequest::ToolCall { tool } => {
                assert!(matches!(tool, ToolArgs::MemoryAdd(_)));
            }
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn memory_add_requires_fragment() {
        assert!(route_tool("memory_add", &None).is_err());
    }

    /// I2 strictness: a present-but-wrong-typed evidence.symbol errors
    /// instead of silently dropping the hint.
    #[test]
    fn memory_add_rejects_non_string_evidence_symbol() {
        let mut m = Map::new();
        m.insert("fragment".into(), json!("hello world"));
        m.insert(
            "evidence".into(),
            json!({"file": "a.rs", "snippet": "x", "symbol": 42}),
        );
        assert!(route_tool("memory_add", &args(m)).is_err());
    }

    /// I2 strictness: unknown evidence fields error like unknown top-level
    /// args instead of silently dropping hints.
    #[test]
    fn memory_add_rejects_unknown_evidence_field() {
        let mut m = Map::new();
        m.insert("fragment".into(), json!("hello world"));
        m.insert(
            "evidence".into(),
            json!({"file": "a.rs", "snippet": "x", "symobl": "f"}),
        );
        assert!(route_tool("memory_add", &args(m)).is_err());
    }

    /// I2 strictness: non-object evidence errors instead of storing the
    /// memory with the evidence silently dropped.
    #[test]
    fn memory_add_rejects_non_object_evidence() {
        for bad in [json!("a.rs"), json!(42), json!(["a.rs"])] {
            let mut m = Map::new();
            m.insert("fragment".into(), json!("hello world"));
            m.insert("evidence".into(), bad);
            assert!(
                route_tool("memory_add", &args(m)).is_err(),
                "non-object evidence must error"
            );
        }
    }

    /// Null evidence stays absent (lenient); boolean evidence errors like
    /// every other wrong-typed value.
    #[test]
    fn memory_add_null_evidence_absent_bool_errors() {
        let mut m = Map::new();
        m.insert("fragment".into(), json!("hello world"));
        m.insert("evidence".into(), Value::Null);
        assert!(route_tool("memory_add", &args(m)).is_ok());
        let mut m = Map::new();
        m.insert("fragment".into(), json!("hello world"));
        m.insert("evidence".into(), json!(true));
        assert!(route_tool("memory_add", &args(m)).is_err());
    }

    /// I2 strictness: a present-but-wrong-typed backup_restore confirm
    /// errors instead of silently becoming unset.
    #[test]
    fn backup_restore_rejects_non_boolean_confirm() {
        let mut m = Map::new();
        m.insert("confirm".into(), json!("yes"));
        assert!(route_tool("backup_restore", &args(m)).is_err());
    }

    #[test]
    fn routes_memory_read() {
        let req = route_tool("memory_read", &None).unwrap();
        assert!(matches!(req, DomainRequest::ToolCall { .. }));
    }

    #[test]
    fn memory_feedback_requires_useful() {
        let mut m = Map::new();
        m.insert("id".into(), json!("abc"));
        assert!(route_tool("memory_feedback", &args(m)).is_err());
    }

    /// Wrong-typed `useful` names the type problem instead of masquerading
    /// as a missing parameter; absent stays "required".
    #[test]
    fn memory_feedback_wrong_typed_useful_names_type() {
        let mut m = Map::new();
        m.insert("id".into(), json!("abc"));
        m.insert("useful".into(), json!("yes"));
        let err = parse_memory_feedback(&m).unwrap_err();
        assert!(
            err.to_string().contains("must be a boolean"),
            "wrong type must name the type, got: {err}"
        );
        let mut m = Map::new();
        m.insert("id".into(), json!("abc"));
        let err = parse_memory_feedback(&m).unwrap_err();
        assert!(
            err.to_string().contains("is required"),
            "absent must stay required, got: {err}"
        );
    }

    #[test]
    fn memory_relate_rejects_bad_type() {
        let mut m = Map::new();
        m.insert("sourceId".into(), json!("a"));
        m.insert("targetId".into(), json!("b"));
        m.insert("type".into(), json!("bogus"));
        assert!(route_tool("memory_relate", &args(m)).is_err());
    }

    #[test]
    fn semantic_search_requires_query() {
        assert!(route_tool("semantic_search", &None).is_err());
    }

    #[test]
    fn unknown_tool_rejected() {
        assert!(route_tool("nonexistent", &None).is_err());
    }

    /// Project arguments normalize at the parse boundary (trim, basename,
    /// lowercase): reads and writes meet on the canonical form.
    #[test]
    fn project_args_normalize_at_parse() {
        let mut args = serde_json::Map::new();
        args.insert(
            "project".to_string(),
            serde_json::Value::String("  MyProj/ ".into()),
        );
        args.insert("query".to_string(), serde_json::Value::String("x".into()));
        let req = route_tool("memory_read", &Some(args)).unwrap();
        match req {
            DomainRequest::ToolCall { tool } => match tool {
                ToolArgs::MemoryRead(parsed) => assert_eq!(
                    parsed.project,
                    Some("myproj".to_string()),
                    "parse must normalize"
                ),
                _ => panic!("wrong tool routed"),
            },
            _ => panic!("expected tool call"),
        }
    }

    /// Unknown argument keys fail instead of silently dropping (a typo'd
    /// filter must never become "no filter").
    #[test]
    fn unknown_argument_keys_rejected() {
        let mut args = serde_json::Map::new();
        args.insert(
            "fragment".to_string(),
            serde_json::Value::String("x".into()),
        );
        args.insert(
            "fragmant".to_string(),
            serde_json::Value::String("typo".into()),
        );
        let err = route_tool("memory_add", &Some(args)).unwrap_err();
        assert!(
            err.to_string().contains("fragmant"),
            "must name the unknown key, got: {err}"
        );
        // Native tools are covered too.
        let mut args = serde_json::Map::new();
        args.insert(
            "path".to_string(),
            serde_json::Value::String("/tmp/x".into()),
        );
        args.insert("bogus".to_string(), serde_json::Value::Bool(true));
        let err = route_tool("backup_preview", &Some(args)).unwrap_err();
        assert!(
            err.to_string().contains("bogus"),
            "must name the unknown key, got: {err}"
        );
    }

    /// The allowlist covers every served tool (no silent gaps) and matches
    /// the frozen schemas exactly (no drift between validation and docs).
    #[test]
    fn argument_allowlist_matches_served_schemas() {
        for tool in LtmrsFrontend::tools() {
            let allowed = allowed_arguments(&tool.name)
                .unwrap_or_else(|| panic!("tool {} has no argument allowlist", tool.name));
            let schema_props: Vec<String> = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            let mut allowed_sorted = allowed.clone();
            allowed_sorted.sort();
            let mut schema_sorted = schema_props.clone();
            schema_sorted.sort();
            assert_eq!(
                allowed_sorted, schema_sorted,
                "allowlist drift for tool {}",
                tool.name
            );
        }
    }

    /// Wrong-typed values fail instead of coercing to defaults (a string
    /// limit must never become "unbounded").
    #[test]
    fn wrong_typed_values_rejected() {
        let mut args = serde_json::Map::new();
        args.insert("limit".to_string(), serde_json::Value::String("10".into()));
        let err = route_tool("memory_read", &Some(args)).unwrap_err();
        assert!(err.to_string().contains("limit"), "got: {err}");
        let mut args = serde_json::Map::new();
        args.insert("all".to_string(), serde_json::Value::String("yes".into()));
        let err = route_tool("memory_read", &Some(args)).unwrap_err();
        assert!(err.to_string().contains("all"), "got: {err}");
        // Explicit nulls stay absent (lenient), not errors.
        let mut args = serde_json::Map::new();
        args.insert("limit".to_string(), serde_json::Value::Null);
        assert!(route_tool("memory_read", &Some(args)).is_ok());
    }

    /// String arrays reject non-string elements (no silent drops).
    #[test]
    fn string_array_elements_are_strict() {
        let mut args = serde_json::Map::new();
        args.insert("ids".to_string(), serde_json::json!(["a", 42]));
        let err = route_tool("memory_read", &Some(args)).unwrap_err();
        assert!(err.to_string().contains("ids"), "got: {err}");
    }

    /// The native backup tools route by name like every frozen tool (their
    /// descriptions and schemas are ltmrs-native, marked in tools/list).
    #[test]
    fn native_backup_tools_route() {
        assert!(route_tool("backup_create", &None).is_ok());
        let mut with_path = serde_json::Map::new();
        with_path.insert(
            "path".to_string(),
            serde_json::Value::String("/tmp/x.ltmrs-backup".to_string()),
        );
        assert!(route_tool("backup_preview", &Some(with_path)).is_ok());
        assert!(route_tool("backup_restore", &None).is_ok());
    }

    /// Tool routing depends only on the tool name: every frozen
    /// compatibility name resolves without consulting any configured server
    /// name. With no arguments the outcome is either `Ok` or an
    /// argument-validation error — never "unknown tool". (Hosts configure
    /// their own server name/namespaces; see `skills::hosts`.)
    #[test]
    fn routing_needs_no_server_name() {
        let tools = frozen_tools();
        assert!(!tools.is_empty(), "frozen set must not be empty");
        for tool in &tools {
            match route_tool(&tool.name, &None) {
                Ok(_) => {}
                Err(e) => assert!(
                    !e.to_string().contains("unknown tool"),
                    "{} must resolve by name alone, got: {e}",
                    tool.name
                ),
            }
        }
    }

    #[test]
    fn envelope_carries_identity() {
        let id = FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        );
        let client = IpcClient::new(std::path::PathBuf::from("unused"));
        let fe = LtmrsFrontend::new(id.clone(), client);
        let params = CallToolRequestParams::new("memory_add")
            .with_arguments(json!({ "fragment": "x" }).as_object().unwrap().clone());
        let env = fe.build_envelope(&params, 7).unwrap();
        assert_eq!(env.frontend_id, id.frontend_id);
        assert_eq!(env.channel_id, id.channel_id);
        assert_eq!(env.protocol_version, PROTOCOL_VERSION);
        assert_eq!(env.retry_epoch, 7);
    }

    #[test]
    fn shapes_structured_success_with_text() {
        use crate::daemon::envelope::DomainPayload;
        let resp = crate::daemon::envelope::IpcResponse::success(
            OperationId::new(Uuid::from_u128(1)),
            crate::domain::command::ReceiptOutcome::Success { affected: vec![] },
            DomainPayload::ToolResult {
                text: "## Stats".into(),
                structured: Some(json!({"total": 4})),
                is_error: false,
            },
        );
        let result = shape_result(&resp);
        assert!(result.structured_content.is_some());
        assert_eq!(
            result.content[0].as_text().map(|t| t.text.as_str()),
            Some("## Stats")
        );
    }

    #[test]
    fn shapes_tool_error() {
        use crate::daemon::envelope::DomainPayload;
        let resp = crate::daemon::envelope::IpcResponse::success(
            OperationId::new(Uuid::from_u128(1)),
            crate::domain::command::ReceiptOutcome::Success { affected: vec![] },
            DomainPayload::ToolResult {
                text: "Error: Fragment not found".into(),
                structured: None,
                is_error: true,
            },
        );
        let result = shape_result(&resp);
        assert_eq!(result.is_error, Some(true));
    }
}

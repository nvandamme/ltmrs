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

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolRequestParams, Tool, ToolAnnotations};
use serde_json::{Map, Value};

use ltmrs_compat::lemma::schemas::frozen_tools;
use ltmrs_compat::lemma::tool_args::ToolArgs;
use ltmrs_daemon::client::IpcClient;
use ltmrs_daemon::envelope::{
    DomainRequest, HandshakeRequest, IpcEnvelope, IpcError, IpcResponse, IpcResult,
    PROTOCOL_VERSION,
};
use ltmrs_domain::command::{DomainErrorCode, Scope};
use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
use ltmrs_domain::memory::Memory;
use uuid::Uuid;

#[cfg(test)]
mod connection_tests;
mod handler;
mod parse_fields;
mod parse_guides;
mod parse_memory;
mod parse_sessions;
#[cfg(test)]
mod resume_tests;
#[cfg(test)]
mod route_tests;
#[cfg(test)]
mod test_support;

use parse_fields::allowed_arguments;
use parse_guides::{
    parse_guide_create, parse_guide_distill, parse_guide_forget, parse_guide_get,
    parse_guide_merge, parse_guide_practice, parse_guide_update,
};
use parse_memory::{
    parse_memory_add, parse_memory_audit, parse_memory_feedback, parse_memory_forget,
    parse_memory_library, parse_memory_merge, parse_memory_read, parse_memory_relate,
    parse_memory_stats, parse_memory_update, parse_semantic_search,
};
use parse_sessions::{
    parse_backup_create, parse_backup_preview, parse_backup_restore, parse_conflict_scan,
    parse_proactive_analysis, parse_project_analytics, parse_session_attempt, parse_session_end,
    parse_session_start, parse_session_stats, parse_suggestion_respond,
};

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
            ltmrs_daemon::envelope::IpcResult::Success {
                payload: ltmrs_daemon::envelope::DomainPayload::Memories(m),
                ..
            } => Ok(m),
            ltmrs_daemon::envelope::IpcResult::Error { message, .. } => {
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
    /// a fresh operation id. An explicit StaleReplay on a healthy stream
    /// likewise renews: the dead epoch refused this fresh operation before
    /// execution (certain no-commit), so forgetting it and executing once
    /// under a fresh epoch cannot double-apply. A second stale answer is
    /// returned as-is — unbounded retry would mask a daemon that never
    /// converges.
    ///
    /// Transport failures are unknown outcomes (P1-B), NOT fresh attempts:
    /// the envelope is built ONCE per MCP call and, after reconnect plus a
    /// resumed same-epoch handshake, the SAME envelope is resent so the
    /// daemon replays its receipt instead of executing twice. A refused
    /// resume (or an unreconnectable transport) surfaces an unknown-outcome
    /// error to the host instead of silently minting a fresh operation.
    /// `resend_after_reconnect` stays conservative: only transport
    /// ambiguity flows there, never an explicit healthy-stream refusal.
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
        let stale_generation = matches!(
            resp.result,
            IpcResult::Error {
                code: DomainErrorCode::StaleGeneration,
                ..
            }
        );
        let stale_epoch = matches!(
            resp.result,
            IpcResult::Error {
                code: DomainErrorCode::StaleReplay,
                ..
            }
        );
        if !stale_generation && !stale_epoch {
            return Ok(resp);
        }
        // Epoch-only reset: the stale reply arrived over a healthy stream,
        // so the connection stays up in both modes (redialing a live
        // socket is wasteful; redialing a bridge is fatal — no listener).
        client.forget_epoch();
        drop(client);
        self.ensure_handshaked().await?;
        self.send_fresh_with_resend(body).await
    }

    /// Send a freshly built envelope once, resending it (same op id) when
    /// the attempt fails with an ambiguous transport error (I-2): the fresh
    /// epoch from the just-completed handshake is resumable, so even the
    /// post-restore fresh operation keeps receipt identity across
    /// reconnects instead of surfacing a generic error the host would
    /// retry as new work. Bounded: the resend path never resends twice.
    async fn send_fresh_with_resend(&self, body: DomainRequest) -> Result<IpcResponse, McpError> {
        let mut client = self.client.lock().await;
        let fresh_epoch = client.retry_epoch().unwrap_or(0);
        let fresh = self.envelope_for(body, fresh_epoch);
        match client.roundtrip(&fresh).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                drop(client);
                self.resend_after_reconnect(&fresh, fresh_epoch, e.to_string())
                    .await
            }
        }
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
            Err(IpcError::GenerationMismatch {
                daemon,
                client: client_gen,
            }) => {
                // The generation moved under an uncertain mutation: the
                // dropped request may have committed before its response
                // was lost, and its receipt namespace is no longer
                // resolvable. A fresh mutation with the same body could
                // double-apply increment-like operations — surface the
                // unknown outcome, never mint a fresh operation here.
                // (Only an explicit StaleGeneration answer to a live
                // request proves no-commit; transport ambiguity proves
                // nothing.)
                client.close();
                return Err(McpError::internal_error(
                    format!(
                        "request failed with unknown outcome ({first_error}); generation moved from {client_gen} to {daemon} before the resume — inspect state before retrying as new work"
                    ),
                    None,
                ));
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
        // generation moved between handshake and resend under transport
        // ambiguity — the outcome is unknown, so surface it (never fresh).
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
                // Generation moved between resume handshake and resend:
                // the original send may have committed before the
                // transport loss, so the outcome is unknown — never mint
                // a fresh mutation, surface it instead.
                client.close();
                Err(McpError::internal_error(
                    format!(
                        "request failed with unknown outcome ({first_error}); generation moved before the resend — inspect state before retrying as new work"
                    ),
                    None,
                ))
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

#[cfg(test)]
mod tests {
    use super::handler::is_mutating_tool;
    use super::test_support::*;
    use super::*;
    use rmcp::ServerHandler;

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
            ltmrs_compat::lemma::schemas::frozen_tools()
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
}

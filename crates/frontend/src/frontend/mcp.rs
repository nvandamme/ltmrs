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
    InitializeResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerNotification, Tool, ToolAnnotations,
    ToolListChangedNotification, ToolListChangedNotificationMethod,
};
use rmcp::service::{MaybeSendFuture, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use ltmrs_compat::lemma::schemas::frozen_tools;
use ltmrs_compat::lemma::tool_args::{
    BackupCreateArgs, BackupImportLemmaArgs, BackupPreviewArgs, BackupRestoreArgs,
    ConflictScanArgs, GuideCreateArgs, GuideDistillArgs, GuideForgetArgs, GuideGetArgs,
    GuideMergeArgs, GuidePracticeArgs, GuideUpdateArgs, MemoryAddArgs, MemoryAuditArgs,
    MemoryFeedbackArgs, MemoryForgetArgs, MemoryLibraryArgs, MemoryMergeArgs, MemoryReadArgs,
    MemoryRelateArgs, MemoryStatsArgs, MemoryUpdateArgs, ProactiveAnalysisArgs,
    ProjectAnalyticsArgs, ResponseFormat, SemanticSearchArgs, SessionAttemptArgs, SessionEndArgs,
    SessionStartArgs, SessionStatsArgs, SuggestionRespondArgs, ToolArgs,
};
use ltmrs_daemon::client::IpcClient;
use ltmrs_daemon::envelope::{
    DomainPayload, DomainRequest, EnvelopeIdentity, HandshakeRequest, IpcEnvelope, IpcError,
    IpcResponse, IpcResult, PROTOCOL_VERSION,
};
use ltmrs_domain::command::{DomainErrorCode, Scope};
use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
use ltmrs_domain::memory::Memory;
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

    /// Best-effort prefetch for `serve_stdio` (Wave B1): handshake if
    /// needed, then one `ListMemories` fetch even when already
    /// handshaken (the managed path arrives with an epoch). Failures
    /// degrade to the lazy path (the first tool call handshakes +
    /// prefetches the same way).
    pub async fn prefetch_snapshot(&self) {
        let mut client = self.client.lock().await;
        if self.handshake_if_needed(&mut client).await.is_ok()
            && let Ok(memories) = self.fetch_snapshot(&mut client).await
        {
            self.set_snapshot(memories);
        }
    }

    /// Ensure the daemon handshake, adopting the live store generation. A
    /// restore bumps the generation; a first-attempt mismatch adopts the
    /// daemon generation and retries once instead of bricking new clients.
    async fn ensure_handshaked(&self) -> Result<(), McpError> {
        let mut client = self.client.lock().await;
        self.handshake_if_needed(&mut client).await?;
        // Prefetch the memory snapshot for the dynamic instructions index.
        // Best-effort: a failure leaves the empty-state instructions.
        if let Ok(memories) = self.fetch_snapshot(&mut client).await {
            self.set_snapshot(memories);
        }
        Ok(())
    }

    /// Connect + handshake only when no epoch is held. Already-handshaken
    /// clients pass through so callers can always follow with a fetch.
    async fn handshake_if_needed(&self, client: &mut IpcClient) -> Result<(), McpError> {
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
            identity: ltmrs_daemon::envelope::EnvelopeIdentity::default(),
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
        let legacy_explicit = request
            .arguments
            .as_ref()
            .is_some_and(|m| m.get("project").is_some_and(|v| !v.is_null()));
        let identity = self.tool_identity(&body, legacy_explicit)?;
        Ok(self.envelope_for(body, retry_epoch, identity))
    }

    /// Project identity for one tool call (Wave A3b): env override, else
    /// the checkout bind file, plus the frontend-canonicalized root and the
    /// wire legacy project. Malformed env values fail the call loudly.
    fn tool_identity(
        &self,
        body: &DomainRequest,
        legacy_explicit: bool,
    ) -> Result<ltmrs_daemon::envelope::EnvelopeIdentity, McpError> {
        let (project_id, canonical_root) = Self::base_identity()?;
        let legacy_project = match body {
            DomainRequest::ToolCall { tool } => ltmrs_compat::lemma::tool_args::tool_project(tool),
            _ => None,
        };
        Ok(ltmrs_daemon::envelope::EnvelopeIdentity {
            project_id,
            canonical_root,
            legacy_project,
            legacy_explicit,
        })
    }

    /// Shared env/bind/cwd identity resolution (Wave A3b + B4): the
    /// stable id (if any) plus the canonicalized checkout root. The twin
    /// `resource_identity` below must stay in sync — resources carry no
    /// wire project, so they always resolve unattributed legacy.
    fn base_identity() -> Result<(Option<String>, Option<String>), McpError> {
        let cwd = std::env::current_dir().ok();
        let env_raw = std::env::var(crate::project::ENV_PROJECT_ID).ok();
        let bound = crate::project::resolve_project_id(cwd.as_deref(), env_raw)
            .map_err(|e| McpError::internal_error(e, None))?;
        Ok((
            bound.map(|b| b.project_id),
            cwd.as_deref().and_then(crate::project::canonical_root),
        ))
    }

    /// Identity for resource reads: same env/bind/cwd resolution as tools,
    /// never a wire project (resources carry no args, so legacy stays
    /// unattributed). Reads inherit the tool path's admission + scope, so
    /// a stable identity sees exactly what the equivalent tool call sees.
    fn resource_identity(&self) -> Result<EnvelopeIdentity, McpError> {
        let (project_id, canonical_root) = Self::base_identity()?;
        Ok(EnvelopeIdentity {
            project_id,
            canonical_root,
            legacy_project: None,
            legacy_explicit: false,
        })
    }

    /// Envelope construction shared by tool calls and the resilient
    /// round-trip: one operation id per MCP call for the first attempt; a
    /// transport-ambiguous retry resends the same envelope (P1-B), while the
    /// post-restore stale-generation retry still mints a fresh one (certain
    /// no-commit under the retired generation).
    fn envelope_for(
        &self,
        body: DomainRequest,
        retry_epoch: u64,
        identity: EnvelopeIdentity,
    ) -> IpcEnvelope {
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
            identity,
            body,
        }
    }

    /// Send one IPC request under a pre-resolved identity (resource reads):
    /// connect + handshake-if-needed + a single send. Read-only: a stale
    /// generation surfaces as an internal error for the host to retry
    /// (reads are safe to repeat), mirroring the prefetch path rather than
    /// the mutation resend machinery.
    async fn send_with_identity(
        &self,
        body: DomainRequest,
        identity: &EnvelopeIdentity,
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
        let envelope = self.envelope_for(body, epoch, identity.clone());
        client
            .roundtrip(&envelope)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    /// Read one MCP resource. Memory/guide reads reuse the tool-call path
    /// (same admission, scope and visibility as the equivalent tool call),
    /// so a stable identity sees exactly what it could recall. The system
    /// prompt is the static frozen base template — never the dynamic index.
    async fn read_lemma_resource(
        &self,
        uri: &str,
        identity: &EnvelopeIdentity,
    ) -> Result<ReadResourceResult, McpError> {
        if uri == "lemma://system-prompt" {
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(INSTRUCTIONS_TEMPLATE, uri).with_mime_type("text/markdown"),
            ]));
        }
        if let Some(id) = uri.strip_prefix("lemma://memory/") {
            let body = DomainRequest::ToolCall {
                tool: ToolArgs::MemoryRead(MemoryReadArgs {
                    id: Some(id.to_string()),
                    ..Default::default()
                }),
            };
            let resp = self.send_with_identity(body, identity).await?;
            return memory_resource_record(uri, &resp);
        }
        if let Some(name) = uri.strip_prefix("lemma://guides/") {
            let body = DomainRequest::ToolCall {
                tool: ToolArgs::GuideGet(GuideGetArgs {
                    guide: Some(name.to_string()),
                    ..Default::default()
                }),
            };
            let resp = self.send_with_identity(body, identity).await?;
            return guide_resource_record(uri, &resp);
        }
        Err(McpError::resource_not_found(
            format!("Unknown resource: {uri}"),
            None,
        ))
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
        legacy_explicit: bool,
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
        let identity = self.tool_identity(&body, legacy_explicit)?;
        let envelope = self.envelope_for(body.clone(), epoch, identity);
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
        self.send_fresh_with_resend(body, legacy_explicit).await
    }

    /// Send a freshly built envelope once, resending it (same op id) when
    /// the attempt fails with an ambiguous transport error (I-2): the fresh
    /// epoch from the just-completed handshake is resumable, so even the
    /// post-restore fresh operation keeps receipt identity across
    /// reconnects instead of surfacing a generic error the host would
    /// retry as new work. Bounded: the resend path never resends twice.
    async fn send_fresh_with_resend(
        &self,
        body: DomainRequest,
        legacy_explicit: bool,
    ) -> Result<IpcResponse, McpError> {
        let mut client = self.client.lock().await;
        let fresh_epoch = client.retry_epoch().unwrap_or(0);
        let identity = self.tool_identity(&body, legacy_explicit)?;
        let fresh = self.envelope_for(body, fresh_epoch, identity);
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
    /// extensions (backup_create/preview/restore/import_lemma). Native tools are marked
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
        tools.push(native_backup_import_lemma_tool());
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
        "backup_import_lemma" => ToolArgs::BackupImportLemma(parse_backup_import_lemma(&args)?),
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
        for tool in ltmrs_compat::lemma::schemas::frozen_tools() {
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
        map.insert(
            "backup_import_lemma".to_string(),
            vec!["path".to_string(), "confirm".to_string()],
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
            // Gate the `as` cast behind the application window: values above
            // it are rejected here instead of wrapping into huge usizes.
            Some(v) if v <= ltmrs_compat::lemma::pagination::MAX_PAGINATION_WINDOW as u64 => {
                Ok(Some(v as usize))
            }
            Some(v) => Err(McpError::invalid_params(
                format!(
                    "{key} value {v} exceeds maximum pagination window {}",
                    ltmrs_compat::lemma::pagination::MAX_PAGINATION_WINDOW
                ),
                None,
            )),
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
        Some(raw) => ltmrs_compat::lemma::tool_args::normalize_project(raw),
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
) -> Result<ltmrs_compat::lemma::tool_args::MemoryEvidence, McpError> {
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
    Ok(ltmrs_compat::lemma::tool_args::MemoryEvidence {
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
    if ltmrs_domain::relation::RelationType::parse(&relation_type).is_none() {
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

/// Parse backup_import_lemma args (`path` optional at the wire level,
/// required at execution; `confirm` acknowledges replacement semantics).
fn parse_backup_import_lemma(args: &Map<String, Value>) -> Result<BackupImportLemmaArgs, McpError> {
    Ok(BackupImportLemmaArgs {
        path: str_field(args, "path")?.map(|s| s.to_string()),
        confirm: opt_bool_field(args, "confirm")?,
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

/// Native backup_import_lemma definition (legacy import via runtime
/// cdylib; explicit confirmation, replace semantics).
fn native_backup_import_lemma_tool() -> Tool {
    native_tool(
        "backup_import_lemma",
        "Import a legacy Lemma .lemma-backup file into the canonical store, REPLACING the live store (never merging). NATIVE ltmrs tool (not a verbatim Lemma port): the isolated importer cdylib reads the SQLite payload at runtime (never linked into the daemon). Unconfirmed calls only stage the backup and report loss-accounted preview counts; confirmed calls (confirm=true with the same path) execute the fenced replace with a safety backup first. The report includes quarantined references and counted loss items (folded manuals, dropped caches, unmapped values) for manual repair.",
        serde_json::json!({
            "path": {
                "type": "string",
                "description": "Absolute path to the .lemma-backup file on this computer.",
            },
            "confirm": {
                "type": "boolean",
                "description": "Must be true to execute: acknowledges replacement semantics.",
            },
        }),
    )
}

/// Bound-identity header line for the instructions (Wave A3b): when the
/// checkout carries a bind file, its bound id is named (the directory name
/// stays display only). Empty when unbound or unreadable.
fn bound_identity_line(cwd: Option<&std::path::Path>) -> String {
    cwd.and_then(|dir| crate::project::read_bind(dir).ok().flatten())
        .map(|b| {
            format!(
                "\nProject identity: {} (bound to this checkout; the directory name is display only).",
                b.project_id
            )
        })
        .unwrap_or_default()
}

/// Static resource list (Wave B4): exactly the system prompt, mirroring
/// upstream's single static `resources/list` entry. Per-item memory/guide
/// URIs are never enumerated — reads address them directly.
pub fn lemma_resource_list() -> Vec<Resource> {
    vec![
        Resource::new("lemma://system-prompt", "Lemma System Prompt")
            .with_description("System prompt for LLM clients using Lemma memory")
            .with_mime_type("text/markdown"),
    ]
}

/// A tool name is known when it appears in the served schemas (frozen +
/// native). `route_tool` still rejects unknown names (T-MCP-03 pin); the
/// caller decides the error shape.
fn is_known_tool(name: &str) -> bool {
    ALLOWED_ARGUMENTS.contains_key(name)
}

/// Unknown tools are MCP `isError` results with human-readable text
/// (upstream shape), never JSON-RPC errors.
fn unknown_tool_result(name: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "Error: Unknown tool '{name}'"
    ))])
}

/// Serve a memory record as a JSON resource. Visibility is inherited from
/// the memory_read tool path (the daemon refuses foreign stable rows as
/// not found); an error payload therefore maps to a not-found resource.
fn memory_resource_record(uri: &str, resp: &IpcResponse) -> Result<ReadResourceResult, McpError> {
    match &resp.result {
        IpcResult::Success {
            payload:
                DomainPayload::ToolResult {
                    text,
                    structured,
                    is_error,
                },
            ..
        } => {
            if *is_error {
                return Err(McpError::resource_not_found(text.clone(), None));
            }
            let record = structured
                .as_ref()
                .and_then(|s| s.get("fragments"))
                .and_then(|f| f.get(0))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(ReadResourceResult::new(vec![
                ResourceContents::text(
                    serde_json::to_string_pretty(&record).unwrap_or_default(),
                    uri,
                )
                .with_mime_type("application/json"),
            ]))
        }
        IpcResult::Error { message, .. } => Err(McpError::internal_error(message.clone(), None)),
        _ => Err(McpError::internal_error(
            "unexpected response shape for memory resource",
            None,
        )),
    }
}

/// Serve a guide record as a JSON resource. Guides are unscoped (global),
/// matching the guide_get tool path; a null guide maps to not-found.
fn guide_resource_record(uri: &str, resp: &IpcResponse) -> Result<ReadResourceResult, McpError> {
    match &resp.result {
        IpcResult::Success {
            payload:
                DomainPayload::ToolResult {
                    text,
                    structured,
                    is_error,
                },
            ..
        } => {
            if *is_error {
                return Err(McpError::resource_not_found(text.clone(), None));
            }
            let record = structured
                .as_ref()
                .and_then(|s| s.get("guide"))
                .cloned()
                .unwrap_or(Value::Null);
            if record.is_null() {
                return Err(McpError::resource_not_found(
                    format!("Guide not found: {uri}"),
                    None,
                ));
            }
            Ok(ReadResourceResult::new(vec![
                ResourceContents::text(
                    serde_json::to_string_pretty(&record).unwrap_or_default(),
                    uri,
                )
                .with_mime_type("application/json"),
            ]))
        }
        IpcResult::Error { message, .. } => Err(McpError::internal_error(message.clone(), None)),
        _ => Err(McpError::internal_error(
            "unexpected response shape for guide resource",
            None,
        )),
    }
}

impl ServerHandler for LtmrsFrontend {
    fn get_info(&self) -> InitializeResult {
        // Static teaching template always present; the dynamic memory index is
        // appended when a snapshot has been cached (upstream buildInstructions).
        let memories = self.snapshot.lock().unwrap().clone().unwrap_or_default();
        let cwd = std::env::current_dir().ok();
        // The cwd display stays (display only, never identity); when a bind
        // file exists its bound id rides the instructions header.
        let project = cwd
            .as_deref()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_lowercase()))
            .filter(|p| !p.is_empty());
        let bound = bound_identity_line(cwd.as_deref());
        let instructions = format!(
            "{}{}{}",
            INSTRUCTIONS_TEMPLATE,
            build_instructions_index(&memories, project.as_deref()),
            bound
        );
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
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

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>> + MaybeSendFuture + '_
    {
        std::future::ready(Ok(ListResourcesResult::with_all_items(
            lemma_resource_list(),
        )))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let identity = self.resource_identity()?;
        Ok(ReadResourceResponse::Complete(
            self.read_lemma_resource(&request.uri, &identity).await?,
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // Unknown tools are MCP isError results (upstream shape), not
        // JSON-RPC errors: route_tool still rejects them (T-MCP-03 pin) and
        // the mapping happens here, before any dispatch.
        if !is_known_tool(&request.name) {
            return Ok(CallToolResponse::Complete(unknown_tool_result(
                &request.name,
            )));
        }
        // The client is shared across calls; the resilient round-trip owns
        // the lock, (re-)handshakes as needed, and recovers once from a
        // post-restore StaleGeneration on an established connection.
        let body = route_tool(&request.name, &request.arguments)?;
        let legacy_explicit = request
            .arguments
            .as_ref()
            .is_some_and(|m| m.get("project").is_some_and(|v| !v.is_null()));
        let resp = self
            .roundtrip_with_rehandshake(body, legacy_explicit)
            .await?;

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
fn shape_result(resp: &ltmrs_daemon::envelope::IpcResponse) -> CallToolResult {
    use ltmrs_daemon::envelope::{DomainPayload, IpcResult};
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
    use ltmrs_domain::id::EntityId;
    use serde_json::json;

    fn args(map: Map<String, Value>) -> Option<Map<String, Value>> {
        Some(map)
    }

    #[test]
    fn serves_all_frozen_tools() {
        let tools = LtmrsFrontend::tools();
        assert_eq!(
            tools.len(),
            30,
            "26 frozen-verbatim + 4 native (backup_create/preview/restore/import_lemma)"
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
            vec![
                "backup_create",
                "backup_preview",
                "backup_restore",
                "backup_import_lemma"
            ]
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
            external_alias: Some(ltmrs_domain::id::ExternalAlias::new(format!("m{id:012x}"))),
            title: title.to_string(),
            fragment: format!("frag-{title}"),
            description: String::new(),
            fragment_type: ltmrs_domain::memory::FragmentType::Fact,
            project: project.map(|p| p.to_string()),
            project_id: None,
            source: ltmrs_domain::memory::MemorySource::Ai,
            confidence,
            quality_score: None,
            lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
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
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
            created_at: ltmrs_domain::memory::Instant::new(1),
            updated_at: ltmrs_domain::memory::Instant::new(1),
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
        use ltmrs_daemon::runtime::RuntimePaths;
        use ltmrs_daemon::server::{Daemon, DaemonConfig, handle_connection};

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
            .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
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
            ltmrs_domain::id::StoreGeneration::new(2),
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
        use ltmrs_daemon::runtime::RuntimePaths;
        use ltmrs_daemon::server::{Daemon, DaemonConfig, handle_connection};

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
            .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
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
            ltmrs_domain::id::StoreGeneration::new(2)
        );
        // Second restore while the connection is established.
        daemon
            .dispatcher_arc()
            .repo()
            .set_store_generation(ltmrs_domain::id::StoreGeneration::new(3))
            .unwrap();
        let resp = fe
            .roundtrip_with_rehandshake(DomainRequest::ListMemories, false)
            .await
            .unwrap();
        assert!(
            matches!(
                resp.result,
                ltmrs_daemon::envelope::IpcResult::Success { .. }
            ),
            "established connection must recover, got {:?}",
            resp.result
        );
        assert_eq!(
            fe.identity.generation(),
            ltmrs_domain::id::StoreGeneration::new(3),
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
        use ltmrs_daemon::dispatcher::Dispatcher;
        use ltmrs_daemon::registry::FrontendRegistry;
        use ltmrs_domain::clock::{Clock, FrozenClock};
        use ltmrs_service::repository::CanonicalRepository;
        use tokio::io::AsyncReadExt;

        async fn read_msg(
            stream: &mut tokio::net::UnixStream,
        ) -> ltmrs_daemon::envelope::WireMessage {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.unwrap();
            serde_json::from_slice(&buf).unwrap()
        }
        async fn write_msg(
            stream: &mut tokio::net::UnixStream,
            reply: &ltmrs_daemon::envelope::WireReply,
        ) {
            // Same streaming reply format as the production server
            // (8-byte total + chunk frames), so the client parses it.
            let payload = serde_json::to_vec(reply).unwrap();
            ltmrs_daemon::envelope::write_response_payload(stream, &payload)
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
                use ltmrs_daemon::envelope::WireMessage;
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
                    &ltmrs_daemon::envelope::WireReply::Handshake(hs),
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
                    &ltmrs_daemon::envelope::WireReply::Response(prefetch_resp),
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
                    &ltmrs_daemon::envelope::WireReply::Handshake(hs2),
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
                    &ltmrs_daemon::envelope::WireReply::Response(resp),
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
            fe.roundtrip_with_rehandshake(DomainRequest::AddMemory { memory }, false),
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

    /// P1 unknown-outcome + generation cut: after a transport loss, a
    /// restore bumps the generation before the resume handshake. The
    /// resume is refused as GenerationMismatch — but the dropped request
    /// may have committed before its response was lost, so a fresh
    /// mutation with the same body would double-apply increment-like
    /// operations (Feedback +0.02, with no duplicate-ID guard to hide
    /// behind). The frontend must surface an unknown-outcome error and
    /// send nothing further. Deterministic: every drop point is
    /// server-controlled, not timed (the only timeout proves the absence
    /// of a reconnect).
    #[tokio::test]
    async fn unknown_outcome_generation_cut_surfaces_unknown_without_fresh_mutation() {
        use ltmrs_daemon::dispatcher::Dispatcher;
        use ltmrs_daemon::registry::FrontendRegistry;
        use ltmrs_domain::clock::{Clock, FrozenClock};
        use ltmrs_service::repository::CanonicalRepository;
        use tokio::io::AsyncReadExt;

        async fn read_msg(
            stream: &mut tokio::net::UnixStream,
        ) -> ltmrs_daemon::envelope::WireMessage {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.unwrap();
            serde_json::from_slice(&buf).unwrap()
        }
        async fn write_msg(
            stream: &mut tokio::net::UnixStream,
            reply: &ltmrs_daemon::envelope::WireReply,
        ) {
            let payload = serde_json::to_vec(reply).unwrap();
            ltmrs_daemon::envelope::write_response_payload(stream, &payload)
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
        // Seed one memory at confidence 0.5: each positive Feedback adds
        // +0.015 and bumps positive_feedback by 1, so exactly-once reads
        // (0.515, 1) and a double-apply reads (0.53, 2) — no
        // duplicate-ID guard to hide behind.
        let mut seed = mem(7, None, 0.5, "Cut Feedback");
        seed.external_alias = None;
        repo.put_memory_direct(&seed).unwrap();
        let target = seed.id;
        let dispatcher = Arc::new(Dispatcher::new(
            Arc::clone(&repo),
            FrontendRegistry::new(),
            Arc::clone(&clock),
        ));
        let socket = dir.path().join("cut.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();

        let server_task = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            let repo = Arc::clone(&repo);
            async move {
                use ltmrs_daemon::envelope::WireMessage;
                let (mut conn1, _) = listener.accept().await.unwrap();
                let WireMessage::Handshake(hs_req) = read_msg(&mut conn1).await else {
                    panic!("expected handshake first");
                };
                assert_eq!(hs_req.resume_retry_epoch, None);
                let hs = dispatcher.handle_handshake(&hs_req).unwrap();
                let first_epoch = hs.retry_epoch;
                write_msg(
                    &mut conn1,
                    &ltmrs_daemon::envelope::WireReply::Handshake(hs),
                )
                .await;
                let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                    panic!("expected snapshot prefetch second");
                };
                let prefetch_resp = dispatcher.handle(&env).unwrap();
                write_msg(
                    &mut conn1,
                    &ltmrs_daemon::envelope::WireReply::Response(prefetch_resp),
                )
                .await;
                // The mutation under test: commit, then drop WITHOUT
                // responding — a deterministic unknown outcome.
                let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                    panic!("expected mutation request third");
                };
                dispatcher.handle(&env).unwrap();
                drop(conn1);
                // Generation cut AFTER the commit, BEFORE the reconnect:
                // the backup/restore the reviewer describes.
                let live = repo.store_generation().unwrap().as_u64();
                repo.set_store_generation(ltmrs_domain::id::StoreGeneration::new(live + 1))
                    .unwrap();
                // Reconnect with a resume for the pre-cut epoch, served
                // through the real handshake path (must refuse: the
                // generation moved under the uncertain mutation).
                let (mut conn2, _) = listener.accept().await.unwrap();
                let WireMessage::Handshake(resume_req) = read_msg(&mut conn2).await else {
                    panic!("expected resume handshake on conn2");
                };
                assert_eq!(
                    resume_req.resume_retry_epoch,
                    Some(first_epoch),
                    "reconnect must resume the same epoch"
                );
                let handshake_err = dispatcher.handle_handshake(&resume_req).unwrap_err();
                let ltmrs_daemon::envelope::IpcError::GenerationMismatch { .. } = handshake_err
                else {
                    panic!("resume across a generation cut must mismatch");
                };
                // Production wire encoding (server.rs): kind tag + the
                // typed error's Display, which the client parses back.
                write_msg(
                    &mut conn2,
                    &ltmrs_daemon::envelope::WireReply::Error(ltmrs_daemon::envelope::WireError {
                        kind: "generation_mismatch".to_string(),
                        message: handshake_err.to_string(),
                    }),
                )
                .await;
                // Production keeps the stream open after a mismatch
                // (same-stream retry): serve whatever follows. Pre-fix the
                // client sends a fresh handshake + prefetch + fresh
                // mutation here — the defect (a second Feedback
                // application). Post-fix it closes its end and sends
                // nothing: EOF (or a quiet timeout) is the fixed-world
                // signal.
                let mut len_buf = [0u8; 4];
                let followed = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::io::AsyncReadExt::read_exact(&mut conn2, &mut len_buf),
                )
                .await
                .is_ok_and(|r| r.is_ok());
                if !followed {
                    // Fixed world: nothing further sent.
                } else {
                    let len = u32::from_be_bytes(len_buf) as usize;
                    let mut buf = vec![0u8; len];
                    tokio::io::AsyncReadExt::read_exact(&mut conn2, &mut buf)
                        .await
                        .unwrap();
                    let next: ltmrs_daemon::envelope::WireMessage =
                        serde_json::from_slice(&buf).unwrap();
                    match next {
                        WireMessage::Handshake(fresh_req) => {
                            assert_eq!(fresh_req.resume_retry_epoch, None);
                            // The fresh handshake still carries the pre-cut
                            // generation: production refuses it again (kept
                            // open), the client adopts the live generation and
                            // redials. Serve that full dance faithfully.
                            let second = dispatcher.handle_handshake(&fresh_req).unwrap_err();
                            assert!(
                                matches!(
                                    second,
                                    ltmrs_daemon::envelope::IpcError::GenerationMismatch { .. }
                                ),
                                "fresh handshake at the old generation must mismatch, got {second:?}"
                            );
                            write_msg(
                                &mut conn2,
                                &ltmrs_daemon::envelope::WireReply::Error(
                                    ltmrs_daemon::envelope::WireError {
                                        kind: "generation_mismatch".to_string(),
                                        message: second.to_string(),
                                    },
                                ),
                            )
                            .await;
                            let (mut conn3, _) = tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                listener.accept(),
                            )
                            .await
                            .expect("client must redial after adopting the live generation")
                            .unwrap();
                            let WireMessage::Handshake(redial_req) = read_msg(&mut conn3).await
                            else {
                                panic!("expected redial handshake on conn3");
                            };
                            let hs = dispatcher.handle_handshake(&redial_req).unwrap();
                            write_msg(
                                &mut conn3,
                                &ltmrs_daemon::envelope::WireReply::Handshake(hs),
                            )
                            .await;
                            let WireMessage::Request(env) = read_msg(&mut conn3).await else {
                                panic!("expected prefetch after redial handshake");
                            };
                            let prefetch_resp = dispatcher.handle(&env).unwrap();
                            write_msg(
                                &mut conn3,
                                &ltmrs_daemon::envelope::WireReply::Response(prefetch_resp),
                            )
                            .await;
                            let WireMessage::Request(env) = read_msg(&mut conn3).await else {
                                panic!("expected fresh mutation after prefetch");
                            };
                            let fresh_resp = dispatcher.handle(&env).unwrap();
                            write_msg(
                                &mut conn3,
                                &ltmrs_daemon::envelope::WireReply::Response(fresh_resp),
                            )
                            .await;
                        }
                        _ => panic!("only a fresh handshake may follow a mismatch"),
                    }
                }
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
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fe.roundtrip_with_rehandshake(
                DomainRequest::Feedback {
                    memory_id: target,
                    useful: true,
                },
                false,
            ),
        )
        .await
        .expect("unknown-outcome call must not hang");
        // Server-side absence proof first: pre-fix this panics with the
        // defect named (a fresh mutation was sent); post-fix it passes.
        server_task.await.unwrap();
        let err = result.expect_err(
            "generation cut after transport loss must surface unknown outcome, never resolve",
        );
        assert!(
            err.message.contains("unknown outcome"),
            "error must name the unknown outcome, got: {}",
            err.message
        );
        // Exactly one Feedback application: +0.015 and counter 1 —
        // never +0.03 / counter 2.
        let memory = repo.get_memories(&[target]).unwrap().remove(0);
        assert_eq!(
            memory.positive_feedback, 1,
            "exactly one Feedback application allowed"
        );
        assert!(
            (memory.confidence - 0.515).abs() < 1e-9,
            "exactly one Feedback application allowed, got {}",
            memory.confidence
        );

        drop(fe);
    }

    /// P1 (tool replay over IPC): a ToolCall memory_add whose response is
    /// lost in transport must resolve on resend through the recorded
    /// receipt — not fail in the compatibility dedup scan against the
    /// memory the first delivery created. Same shape as the native
    /// unknown-outcome test, but driving the full ToolCall path.
    #[tokio::test]
    async fn toolcall_add_resend_after_resume_replays_success() {
        use ltmrs_daemon::dispatcher::Dispatcher;
        use ltmrs_daemon::registry::FrontendRegistry;
        use ltmrs_domain::clock::{Clock, FrozenClock};
        use ltmrs_service::repository::CanonicalRepository;
        use tokio::io::AsyncReadExt;

        async fn read_msg(
            stream: &mut tokio::net::UnixStream,
        ) -> ltmrs_daemon::envelope::WireMessage {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.unwrap();
            serde_json::from_slice(&buf).unwrap()
        }
        async fn write_msg(
            stream: &mut tokio::net::UnixStream,
            reply: &ltmrs_daemon::envelope::WireReply,
        ) {
            let payload = serde_json::to_vec(reply).unwrap();
            ltmrs_daemon::envelope::write_response_payload(stream, &payload)
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
        let dispatcher = Arc::new(Dispatcher::new(
            Arc::clone(&repo),
            FrontendRegistry::new(),
            Arc::clone(&clock),
        ));
        let socket = dir.path().join("tool-resend.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();

        let server_task = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            async move {
                use ltmrs_daemon::envelope::WireMessage;
                // conn1: handshake, prefetch, ToolCall add → commit, then
                // drop WITHOUT responding (deterministic unknown outcome).
                let (mut conn1, _) = listener.accept().await.unwrap();
                let WireMessage::Handshake(hs_req) = read_msg(&mut conn1).await else {
                    panic!("expected handshake first");
                };
                assert_eq!(hs_req.resume_retry_epoch, None);
                let hs = dispatcher.handle_handshake(&hs_req).unwrap();
                let first_epoch = hs.retry_epoch;
                write_msg(
                    &mut conn1,
                    &ltmrs_daemon::envelope::WireReply::Handshake(hs),
                )
                .await;
                let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                    panic!("expected snapshot prefetch second");
                };
                let prefetch_resp = dispatcher.handle(&env).unwrap();
                write_msg(
                    &mut conn1,
                    &ltmrs_daemon::envelope::WireReply::Response(prefetch_resp),
                )
                .await;
                let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                    panic!("expected ToolCall request third");
                };
                let first_op = env.operation_id;
                dispatcher.handle(&env).unwrap();
                drop(conn1);
                // conn2: resume the same epoch; the resent envelope must
                // replay through the recorded receipt.
                let (mut conn2, _) = listener.accept().await.unwrap();
                let WireMessage::Handshake(resume_req) = read_msg(&mut conn2).await else {
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
                    &ltmrs_daemon::envelope::WireReply::Handshake(hs2),
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
                    &ltmrs_daemon::envelope::WireReply::Response(resp),
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
        let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "IPC resend replay fixture fragment".to_string(),
            ..Default::default()
        });
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fe.roundtrip_with_rehandshake(DomainRequest::ToolCall { tool }, false),
        )
        .await
        .expect("resend must not hang")
        .expect("unknown-outcome ToolCall must resolve");
        let text = match &resp.result {
            ltmrs_daemon::envelope::IpcResult::Success { payload, .. } => match payload {
                ltmrs_daemon::envelope::DomainPayload::ToolResult { text, .. } => text.clone(),
                other => panic!("expected tool result, got {other:?}"),
            },
            other => panic!("resend must succeed, got {other:?}"),
        };
        assert!(
            text.contains("Added fragment"),
            "resend must replay the add, got: {text}"
        );
        server_task.await.unwrap();
        let memories = repo
            .export_snapshot()
            .unwrap()
            .memories
            .into_iter()
            .filter(|m| m.fragment == "IPC resend replay fixture fragment")
            .collect::<Vec<_>>();
        assert_eq!(memories.len(), 1, "exactly one effect allowed");

        drop(fe);
    }

    /// I-2 stale-path resend: after a StaleGeneration answer the frontend
    /// sends a FRESH envelope; when THAT send loses its response in
    /// transport, the frontend must resend the fresh envelope (same op id)
    /// after resume instead of surfacing a generic error the host would
    /// retry as new work (double-apply). Deterministic: every drop point
    /// is server-controlled, not timed.
    #[tokio::test]
    async fn stale_refresh_resends_fresh_envelope_after_drop() {
        use ltmrs_daemon::dispatcher::Dispatcher;
        use ltmrs_daemon::registry::FrontendRegistry;
        use ltmrs_domain::clock::{Clock, FrozenClock};
        use ltmrs_service::repository::CanonicalRepository;
        use tokio::io::AsyncReadExt;

        async fn read_msg(
            stream: &mut tokio::net::UnixStream,
        ) -> ltmrs_daemon::envelope::WireMessage {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.unwrap();
            serde_json::from_slice(&buf).unwrap()
        }
        async fn write_msg(
            stream: &mut tokio::net::UnixStream,
            reply: &ltmrs_daemon::envelope::WireReply,
        ) {
            let payload = serde_json::to_vec(reply).unwrap();
            ltmrs_daemon::envelope::write_response_payload(stream, &payload)
                .await
                .unwrap()
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
        let dispatcher = Arc::new(Dispatcher::new(
            Arc::clone(&repo),
            FrontendRegistry::new(),
            Arc::clone(&clock),
        ));
        let socket = dir.path().join("stale-resend.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();

        let server_task = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            let repo = Arc::clone(&repo);
            async move {
                use ltmrs_daemon::envelope::{IpcResponse, WireMessage, WireReply};
                // conn1: handshake, prefetch, main X (answered stale after
                // a real generation bump), then the fresh re-handshake.
                let (mut conn1, _) = listener.accept().await.unwrap();
                let WireMessage::Handshake(hs_req) = read_msg(&mut conn1).await else {
                    panic!("expected handshake first");
                };
                let hs = dispatcher.handle_handshake(&hs_req).unwrap();
                write_msg(&mut conn1, &WireReply::Handshake(hs)).await;
                let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                    panic!("expected snapshot prefetch second");
                };
                let prefetch_resp = dispatcher.handle(&env).unwrap();
                write_msg(&mut conn1, &WireReply::Response(prefetch_resp)).await;
                let WireMessage::Request(env) = read_msg(&mut conn1).await else {
                    panic!("expected main request third");
                };
                // Real generation bump: the live store moves under the call.
                let live = repo.store_generation().unwrap().as_u64();
                repo.set_store_generation(ltmrs_domain::id::StoreGeneration::new(live + 1))
                    .unwrap();
                match dispatcher.handle(&env) {
                    Ok(resp) => write_msg(&mut conn1, &WireReply::Response(resp)).await,
                    Err(e) => {
                        write_msg(
                            &mut conn1,
                            &WireReply::Response(IpcResponse::error(env.operation_id, &e)),
                        )
                        .await
                    }
                }
                // Fresh handshake after the stale answer (same stream):
                // the generation moved, so this is refused as a mismatch
                // and the frontend reconnects before retrying.
                let WireMessage::Handshake(hs_req) = read_msg(&mut conn1).await else {
                    panic!("expected fresh handshake fourth");
                };
                assert_eq!(hs_req.resume_retry_epoch, None);
                let mismatch = ltmrs_daemon::envelope::IpcError::GenerationMismatch {
                    daemon: 2,
                    client: 1,
                };
                write_msg(
                    &mut conn1,
                    &WireReply::Error(ltmrs_daemon::envelope::WireError {
                        kind: "generation_mismatch".to_string(),
                        message: mismatch.to_string(),
                    }),
                )
                .await;
                // conn1b: the frontend reconnects after the mismatch (see
                // ensure_handshaked) — handshake, prefetch, then the fresh
                // mutation, which commits and is dropped without responding.
                let (mut conn1b, _) = listener.accept().await.unwrap();
                let WireMessage::Handshake(hs_req) = read_msg(&mut conn1b).await else {
                    panic!("expected handshake on conn1b");
                };
                let hs = dispatcher.handle_handshake(&hs_req).unwrap();
                let fresh_epoch = hs.retry_epoch;
                write_msg(&mut conn1b, &WireReply::Handshake(hs)).await;
                let WireMessage::Request(env) = read_msg(&mut conn1b).await else {
                    panic!("expected prefetch on conn1b");
                };
                let prefetch_resp = dispatcher.handle(&env).unwrap();
                write_msg(&mut conn1b, &WireReply::Response(prefetch_resp)).await;
                let WireMessage::Request(env) = read_msg(&mut conn1b).await else {
                    panic!("expected fresh mutation on conn1b");
                };
                let fresh_op = env.operation_id;
                dispatcher.handle(&env).unwrap();
                drop(conn1);
                drop(conn1b);
                // conn2: resume the fresh epoch and resend the fresh op.
                let (mut conn2, _) = listener.accept().await.unwrap();
                let m2 = read_msg(&mut conn2).await;
                let WireMessage::Handshake(resume_req) = m2 else {
                    panic!("expected resume handshake on conn2");
                };
                assert_eq!(
                    resume_req.resume_retry_epoch,
                    Some(fresh_epoch),
                    "reconnect must resume the fresh epoch"
                );
                let hs2 = dispatcher.handle_handshake(&resume_req).unwrap();
                assert_eq!(hs2.retry_epoch, fresh_epoch);
                write_msg(&mut conn2, &WireReply::Handshake(hs2)).await;
                let WireMessage::Request(env2) = read_msg(&mut conn2).await else {
                    panic!("expected resent request on conn2");
                };
                assert_eq!(
                    env2.operation_id, fresh_op,
                    "reconnect must resend the fresh operation"
                );
                let resp = dispatcher.handle(&env2).unwrap();
                write_msg(&mut conn2, &WireReply::Response(resp)).await;
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
        let mut memory = mem(8, None, 0.5, "Stale Refresh");
        memory.external_alias = None;
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fe.roundtrip_with_rehandshake(DomainRequest::AddMemory { memory }, false),
        )
        .await
        .expect("resend must not hang")
        .expect("stale-refresh call must resolve");
        assert!(
            matches!(resp.result, IpcResult::Success { .. }),
            "fresh resend must replay the recorded outcome, got {:?}",
            resp.result
        );
        server_task.await.unwrap();
        // Exactly one memory: the fresh resend replayed, never re-executed.
        let memories = repo
            .export_snapshot()
            .unwrap()
            .memories
            .into_iter()
            .filter(|m| m.title == "Stale Refresh")
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
        use ltmrs_daemon::runtime::RuntimePaths;
        use ltmrs_daemon::server::{Daemon, DaemonConfig, handle_connection};

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
            .roundtrip_with_rehandshake(DomainRequest::ListMemories, false)
            .await
            .unwrap();
        assert!(
            matches!(
                first.result,
                ltmrs_daemon::envelope::IpcResult::Success { .. }
            ),
            "baseline call must serve, got {:?}",
            first.result
        );
        // Restore bumps the live generation mid-session.
        daemon
            .dispatcher_arc()
            .repo()
            .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
            .unwrap();
        let recovered = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            fe.roundtrip_with_rehandshake(DomainRequest::ListMemories, false),
        )
        .await
        .expect("post-restore call must complete, never park");
        let resp = recovered.unwrap();
        assert!(
            matches!(
                resp.result,
                ltmrs_daemon::envelope::IpcResult::Success { .. }
            ),
            "established bridged connection must recover, got {:?}",
            resp.result
        );
        assert_eq!(
            fe.identity.generation(),
            ltmrs_domain::id::StoreGeneration::new(2),
            "frontend must adopt the live generation"
        );

        drop(daemon);
    }

    /// Sticky-stream regression: a generically rejected handshake must close
    /// the (daemon-closed) stream instead of keeping a dead-but-connected
    /// client that fails every later call on the same stream.
    #[tokio::test]
    async fn rejected_handshake_closes_dead_stream() {
        use ltmrs_daemon::envelope::{WireError, WireReply};
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
            let _ = ltmrs_daemon::envelope::write_response_payload(&mut server_end, &payload).await;
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
        use ltmrs_daemon::runtime::RuntimePaths;
        use ltmrs_daemon::server::{Daemon, DaemonConfig, handle_connection};

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
            fe.roundtrip_with_rehandshake(DomainRequest::ListMemories, false)
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

    /// backup_import_lemma routes path+confirm through to the typed args.
    #[test]
    fn backup_import_lemma_routes_with_path_and_confirm() {
        let mut m = Map::new();
        m.insert("path".into(), json!("/tmp/x.lemma-backup"));
        m.insert("confirm".into(), json!(true));
        let req = route_tool("backup_import_lemma", &args(m)).unwrap();
        match req {
            DomainRequest::ToolCall { tool } => match tool {
                ToolArgs::BackupImportLemma(call) => {
                    assert_eq!(call.path.as_deref(), Some("/tmp/x.lemma-backup"));
                    assert_eq!(call.confirm, Some(true));
                }
                other => panic!("wrong tool routed: {:?}", other.tool()),
            },
            other => panic!("not a tool call: {other:?}"),
        }
        // Unknown keys still fail on the new tool.
        let mut m = Map::new();
        m.insert("bogus".into(), json!(true));
        assert!(route_tool("backup_import_lemma", &args(m)).is_err());
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

    /// Pagination values beyond the application window are rejected at the
    /// parse boundary (u64::MAX offsets must never reach the tools as
    /// wrapped/clamped windows).
    #[test]
    fn absurd_pagination_values_rejected_at_parse() {
        let mut args = serde_json::Map::new();
        args.insert("limit".to_string(), serde_json::json!(30));
        args.insert(
            "offset".to_string(),
            serde_json::json!(18446744073709551615u64),
        );
        let err = route_tool("memory_read", &Some(args)).unwrap_err();
        assert!(err.to_string().contains("offset"), "got: {err}");

        let mut args = serde_json::Map::new();
        args.insert("query".to_string(), serde_json::json!("q"));
        args.insert(
            "topK".to_string(),
            serde_json::json!(18446744073709551615u64),
        );
        let err = route_tool("semantic_search", &Some(args)).unwrap_err();
        assert!(err.to_string().contains("topK"), "got: {err}");

        // In-window values still parse.
        let mut args = serde_json::Map::new();
        args.insert("limit".to_string(), serde_json::json!(30));
        args.insert("offset".to_string(), serde_json::json!(0));
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
        use ltmrs_daemon::envelope::DomainPayload;
        let resp = ltmrs_daemon::envelope::IpcResponse::success(
            OperationId::new(Uuid::from_u128(1)),
            ltmrs_domain::command::ReceiptOutcome::Success { affected: vec![] },
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
        use ltmrs_daemon::envelope::DomainPayload;
        let resp = ltmrs_daemon::envelope::IpcResponse::success(
            OperationId::new(Uuid::from_u128(1)),
            ltmrs_domain::command::ReceiptOutcome::Success { affected: vec![] },
            DomainPayload::ToolResult {
                text: "Error: Fragment not found".into(),
                structured: None,
                is_error: true,
            },
        );
        let result = shape_result(&resp);
        assert_eq!(result.is_error, Some(true));
    }

    /// The instructions header names the bound identity when a bind file
    /// exists, and stays silent otherwise (cwd display is display only).
    #[test]
    fn bound_identity_line_names_bind_file() {
        use crate::project::{Binding, write_bind};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(bound_identity_line(Some(&root)), "");
        write_bind(
            &root,
            &Binding {
                project_id: "prj_shop_01ARZ3NDEKTSV4RRFFQ69G5FAV2".into(),
                name: "shop".into(),
                created_at: 1,
            },
        )
        .unwrap();
        let line = bound_identity_line(Some(&root));
        assert!(
            line.contains("prj_shop_01ARZ3NDEKTSV4RRFFQ69G5FAV2"),
            "got: {line}"
        );
        assert_eq!(bound_identity_line(None), "");
    }

    /// Wave B1: non-empty store must surface in the FIRST initialize,
    /// not just after the first tool call. Prefetch (as `serve_stdio`
    /// does) then `get_info` must list the stored fragment.
    #[tokio::test]
    async fn first_initialize_sees_nonempty_store() {
        use ltmrs_daemon::runtime::RuntimePaths;
        use ltmrs_daemon::server::{Daemon, DaemonConfig, handle_connection};

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
            .put_memory_direct(&mem(9001, None, 0.9, "Prefetch Fixture"))
            .unwrap();
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
        let instructions = fe.get_info().instructions.unwrap();
        assert!(
            instructions.contains("no saved memories yet"),
            "unprefetched first initialize still shows the empty window, got: {instructions}"
        );
        fe.prefetch_snapshot().await;
        let instructions = fe.get_info().instructions.unwrap();
        assert!(
            !instructions.contains("no saved memories yet"),
            "prefetched first initialize must list the non-empty store, got empty-state window"
        );
        assert!(
            instructions.contains("Prefetch Fixture"),
            "first initialize must name the stored fragment, got: {instructions}"
        );
        drop(daemon);
    }

    /// Wave B1: prefetch over an empty store keeps the current empty-state
    /// text (no behavior change for fresh stores).
    #[tokio::test]
    async fn prefetch_on_empty_store_keeps_empty_state() {
        use ltmrs_daemon::runtime::RuntimePaths;
        use ltmrs_daemon::server::{Daemon, DaemonConfig, handle_connection};

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
                FrontendId::new(Uuid::from_u128(3)),
                ChannelId::new(Uuid::from_u128(4)),
            ),
            client,
        );
        fe.prefetch_snapshot().await;
        let instructions = fe.get_info().instructions.unwrap();
        assert!(
            instructions.contains("no saved memories yet"),
            "empty store must keep the empty-state text, got: {instructions}"
        );
        drop(daemon);
    }

    /// Wave B1 managed path: the default `socket=None` path arrives
    /// already handshaken (probe handshake sets the retry epoch before
    /// the frontend is built). Prefetch must fetch regardless of
    /// handshake state, so `get_info` is populated here too.
    #[tokio::test]
    async fn prefetch_fetches_when_already_handshaken() {
        use ltmrs_daemon::runtime::RuntimePaths;
        use ltmrs_daemon::server::{Daemon, DaemonConfig, handle_connection};

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
            .put_memory_direct(&mem(9002, None, 0.9, "Managed Prefetch Fixture"))
            .unwrap();
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
                FrontendId::new(Uuid::from_u128(5)),
                ChannelId::new(Uuid::from_u128(6)),
            ),
            client,
        );
        // Mimic the managed path: probe handshake completes before the
        // frontend is built, so the epoch is already set while the
        // snapshot is still None.
        {
            let mut guard = fe.client.lock().await;
            let req = HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                store_generation: StoreGeneration::FIRST,
                frontend_id: FrontendId::new(Uuid::from_u128(5)),
                channel_id: ChannelId::new(Uuid::from_u128(6)),
                resume_retry_epoch: None,
            };
            guard.handshake(&req).await.unwrap();
            assert!(guard.retry_epoch().is_some());
        }
        assert!(
            fe.get_info()
                .instructions
                .unwrap()
                .contains("no saved memories yet"),
            "setup: snapshot must start empty while handshaken"
        );
        fe.prefetch_snapshot().await;
        let instructions = fe.get_info().instructions.unwrap();
        assert!(
            !instructions.contains("no saved memories yet"),
            "prefetch on a handshaken client must fetch, got empty-state window"
        );
        assert!(
            instructions.contains("Managed Prefetch Fixture"),
            "prefetch on a handshaken client must name the stored fragment, got: {instructions}"
        );
        drop(daemon);
    }

    // ---- Wave B4: MCP resources + unknown-tool errors ----

    /// Frontend with no daemon connection (pure read_lemma_resource paths
    /// that never touch the client still work; IO paths need live_frontend).
    fn unconnected_frontend() -> LtmrsFrontend {
        LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(41)),
                ChannelId::new(Uuid::from_u128(42)),
            ),
            IpcClient::new(std::path::PathBuf::from("unused")),
        )
    }

    /// Live frontend over a bridged daemon pair.
    /// Returns the frontend plus guards that must stay alive for IO.
    async fn live_frontend_with(
        seeds: &[Memory],
    ) -> (
        LtmrsFrontend,
        tempfile::TempDir,
        ltmrs_daemon::server::Daemon,
    ) {
        use ltmrs_daemon::runtime::RuntimePaths;
        use ltmrs_daemon::server::{Daemon, DaemonConfig, handle_connection};

        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        for m in seeds {
            daemon.dispatcher_arc().repo().put_memory_direct(m).unwrap();
        }
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
                FrontendId::new(Uuid::from_u128(43)),
                ChannelId::new(Uuid::from_u128(44)),
            ),
            client,
        );
        (fe, dir, daemon)
    }

    /// Unattributed identity: no stable id, no root, no wire project.
    fn unattributed_identity() -> EnvelopeIdentity {
        EnvelopeIdentity::default()
    }

    /// Stable identity for a named checkout that was never bound: a fresh
    /// id whose root the daemon has never seen.
    fn stable_identity_named(name: &str, root: &str) -> EnvelopeIdentity {
        let (full, _) = ltmrs_domain::project_id::generate_project_id(name);
        EnvelopeIdentity {
            project_id: Some(full),
            canonical_root: Some(root.to_string()),
            legacy_project: None,
            legacy_explicit: false,
        }
    }

    /// Minimal guide record for resource tests.
    fn sample_guide(name: &str) -> ltmrs_domain::guide::Guide {
        ltmrs_domain::guide::Guide {
            name: name.to_string(),
            category: "dev-tool".to_string(),
            description: "test guide".to_string(),
            contexts: vec![],
            learnings: vec![],
            usage_count: 0,
            last_used: None,
            success_count: 0,
            failure_count: 0,
            anti_patterns: vec![],
            pitfalls: vec![],
            depends_on: vec![],
            enables: vec![],
            source_memories: vec![],
            validated_by: vec![],
            superseded_by: None,
            deprecated: false,
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            created_at: ltmrs_domain::memory::Instant::new(0),
            updated_at: ltmrs_domain::memory::Instant::new(0),
        }
    }

    /// Extract the text of a single-text resource result in tests.
    fn resource_text(out: &ReadResourceResult) -> Result<String, String> {
        out.contents
            .iter()
            .filter_map(|c| match c {
                ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
                _ => None,
            })
            .next()
            .ok_or_else(|| format!("no text content in resource result: {out:?}"))
    }

    /// Add a memory through the tool path (registers the legacy id, unlike
    /// direct row writes) and return its legacy id. `project` is the wire
    /// legacy bucket (None = unattributed global), mirroring a real client.
    async fn add_via_tool(
        fe: &LtmrsFrontend,
        fragment: &str,
        project: Option<&str>,
        identity: &EnvelopeIdentity,
    ) -> String {
        let body = DomainRequest::ToolCall {
            tool: ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: fragment.to_string(),
                project: project.map(|p| p.to_string()),
                ..Default::default()
            }),
        };
        let resp = fe.send_with_identity(body, identity).await.unwrap();
        match resp.result {
            IpcResult::Success {
                payload:
                    DomainPayload::ToolResult {
                        structured: Some(s),
                        ..
                    },
                ..
            } => s
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("add response has no id: {s}"))
                .to_string(),
            other => panic!("add failed: {other:?}"),
        }
    }

    /// `resources/list` advertises exactly the system-prompt resource
    /// (upstream serves one static entry; per-item URIs are NOT enumerated).
    #[test]
    fn resources_list_advertises_system_prompt_only() {
        let listed = lemma_resource_list();
        assert_eq!(listed.len(), 1, "exactly one static entry, got: {listed:?}");
        assert_eq!(listed[0].uri, "lemma://system-prompt");
        assert_eq!(listed[0].mime_type.as_deref(), Some("text/markdown"));
    }

    /// `resources/read lemma://system-prompt` serves the frozen base prompt
    /// text (static, deterministic — never the dynamic index).
    #[tokio::test]
    async fn system_prompt_resource_returns_template() {
        let fe = unconnected_frontend();
        let out = fe
            .read_lemma_resource("lemma://system-prompt", &unattributed_identity())
            .await
            .unwrap();
        let text = resource_text(&out).unwrap();
        assert!(
            text.contains("Persistent Memory"),
            "system prompt must be the frozen base template, got: {text}"
        );
        assert!(
            text.contains("RECALL"),
            "system prompt must carry the recall contract, got: {text}"
        );
        assert!(
            !text.contains("no saved memories yet"),
            "resource must not append the dynamic index, got: {text}"
        );
    }

    /// `resources/read lemma://memory/{id}` returns the record as JSON.
    #[tokio::test]
    async fn memory_resource_returns_record_json() {
        let (fe, _dir, _daemon) = live_frontend_with(&[]).await;
        let id = add_via_tool(
            &fe,
            "## Resource Fixture\n\n### Context\nResource record body.",
            None,
            &unattributed_identity(),
        )
        .await;
        let out = fe
            .read_lemma_resource(&format!("lemma://memory/{id}"), &unattributed_identity())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&resource_text(&out).unwrap()).unwrap();
        assert_eq!(value["id"], json!(id));
        assert!(
            value["fragment"]
                .as_str()
                .unwrap()
                .contains("Resource record body")
        );
    }

    /// Unknown memory ids are resource errors, not tool errors.
    #[tokio::test]
    async fn unknown_memory_resource_errors() {
        let (fe, _dir, _daemon) = live_frontend_with(&[]).await;
        let err = fe
            .read_lemma_resource("lemma://memory/m does-not-exist", &unattributed_identity())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "unknown memory must be a not-found resource error, got: {err}"
        );
    }

    /// `resources/read lemma://guides/{name}` is case-insensitive;
    /// unknown guides are resource errors.
    #[tokio::test]
    async fn guide_resource_case_insensitive() {
        let (fe, _dir, daemon) = live_frontend_with(&[]).await;
        daemon
            .dispatcher_arc()
            .repo()
            .put_guide(&sample_guide("Rust"))
            .unwrap();
        let out = fe
            .read_lemma_resource("lemma://guides/RUST", &unattributed_identity())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&resource_text(&out).unwrap()).unwrap();
        assert_eq!(value["guide"], json!("Rust"));
        let err = fe
            .read_lemma_resource("lemma://guides/nope", &unattributed_identity())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "unknown guide must be a not-found resource error, got: {err}"
        );
    }

    /// Unknown resource URIs are resource errors.
    #[tokio::test]
    async fn unknown_resource_uri_errors() {
        let fe = unconnected_frontend();
        let err = fe
            .read_lemma_resource("lemma://nope/x", &unattributed_identity())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Unknown resource"),
            "unknown URI must be an unknown-resource error, got: {err}"
        );
    }

    /// Unknown tools return an isError tool result, not a JSON-RPC error.
    #[test]
    fn unknown_tool_returns_is_error_result() {
        assert!(!is_known_tool("nope"));
        assert!(is_known_tool("memory_read"));
        let result = unknown_tool_result("nope");
        assert!(
            result.is_error.unwrap_or(false),
            "unknown tool must set isError"
        );
        let text = result
            .content
            .iter()
            .filter_map(|c| match c {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("nope"),
            "unknown-tool result must name the tool, got: {text}"
        );
    }

    /// A stable identity cannot read a foreign project's memory resource,
    /// but the same record is served without identity.
    #[tokio::test]
    async fn memory_resource_stable_scope_refused() {
        let (fe, _dir, _daemon) = live_frontend_with(&[]).await;
        // File the fixture under the legacy "other" bucket through the
        // tool path (registers the legacy id for direct reads). The
        // identity mirrors what tool_identity would build for a wire
        // project="other" call: legacy set, explicit, no stable id.
        let other = EnvelopeIdentity {
            project_id: None,
            canonical_root: None,
            legacy_project: Some("other".to_string()),
            legacy_explicit: true,
        };
        let id = add_via_tool(
            &fe,
            "## Foreign Fixture\n\n### Context\nForeign body.",
            Some("other"),
            &other,
        )
        .await;
        // A fresh stable identity from an unrelated checkout: the write
        // registers it on first use, the foreign read must refuse.
        let foreign = stable_identity_named("shop", "/tmp/a/shop");
        add_via_tool(
            &fe,
            "## Shop Own Fixture\n\n### Context\nShop body.",
            None,
            &foreign,
        )
        .await;
        let err = fe
            .read_lemma_resource(&format!("lemma://memory/{id}"), &foreign)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "foreign resource must be refused to a stable identity, got: {err}"
        );
        let out = fe
            .read_lemma_resource(&format!("lemma://memory/{id}"), &unattributed_identity())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&resource_text(&out).unwrap()).unwrap();
        assert!(value["fragment"].as_str().unwrap().contains("Foreign body"));
    }
}

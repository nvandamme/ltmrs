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
    MemoryAddArgs, MemoryAuditArgs, MemoryFeedbackArgs, MemoryForgetArgs, MemoryLibraryArgs,
    MemoryMergeArgs, MemoryReadArgs, MemoryRelateArgs, MemoryStatsArgs, MemoryUpdateArgs,
    ResponseFormat, SemanticSearchArgs, ToolArgs,
};
use crate::daemon::client::IpcClient;
use crate::daemon::envelope::{DomainRequest, HandshakeRequest, IpcEnvelope, PROTOCOL_VERSION};
use crate::domain::command::Scope;
use crate::domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
use crate::domain::memory::Memory;
use uuid::Uuid;

/// The frontend's identity, fixed per process/channel.
#[derive(Debug, Clone)]
pub struct FrontendIdentity {
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
    pub store_generation: StoreGeneration,
}

impl FrontendIdentity {
    pub fn new(frontend_id: FrontendId, channel_id: ChannelId) -> Self {
        Self {
            frontend_id,
            channel_id,
            store_generation: StoreGeneration::FIRST,
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

    /// Fetch all canonical memories via IPC (read-only, no receipt needed).
    async fn fetch_snapshot(&self, client: &mut IpcClient) -> Result<Vec<Memory>, McpError> {
        let op = OperationId::new(Uuid::now_v7());
        let env = IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: self.identity.store_generation,
            frontend_id: self.identity.frontend_id,
            channel_id: self.identity.channel_id,
            operation_id: op,
            session: None,
            retry_epoch: client.retry_epoch().unwrap_or(0),
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::ListMemories,
        };
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
        let op = OperationId::new(Uuid::now_v7());
        let body = route_tool(&request.name, &request.arguments)?;
        Ok(IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: self.identity.store_generation,
            frontend_id: self.identity.frontend_id,
            channel_id: self.identity.channel_id,
            operation_id: op,
            session: None,
            retry_epoch,
            deadline_millis: None,
            scope: Scope::default(),
            body,
        })
    }

    /// The tool list advertised to the host: the 11 frozen WP-08 tools, served
    /// verbatim from the baseline capture (T-MCP-01).
    pub fn tools() -> Vec<Tool> {
        frozen_tools()
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
            .collect()
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

fn str_field<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn bool_field(args: &Map<String, Value>, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn usize_field(args: &Map<String, Value>, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn f64_field(args: &Map<String, Value>, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| v.as_f64())
}

fn response_format_field(args: &Map<String, Value>, key: &str) -> Option<ResponseFormat> {
    args.get(key)
        .and_then(|v| v.as_str())
        .and_then(ResponseFormat::parse)
}

fn require_str(args: &Map<String, Value>, key: &str) -> Result<String, McpError> {
    str_field(args, key)
        .map(|s| s.to_string())
        .ok_or_else(|| McpError::invalid_params(format!("{key} is required"), None))
}

fn parse_memory_read(args: &Map<String, Value>) -> Result<MemoryReadArgs, McpError> {
    Ok(MemoryReadArgs {
        project: str_field(args, "project").map(|s| s.to_string()),
        query: str_field(args, "query").map(|s| s.to_string()),
        id: str_field(args, "id").map(|s| s.to_string()),
        context: str_field(args, "context").map(|s| s.to_string()),
        all: bool_field(args, "all"),
        ids: args.get("ids").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        }),
        min_confidence: f64_field(args, "minConfidence"),
        after_date: str_field(args, "afterDate").map(|s| s.to_string()),
        before_date: str_field(args, "beforeDate").map(|s| s.to_string()),
        limit: usize_field(args, "limit"),
        offset: usize_field(args, "offset"),
        response_format: response_format_field(args, "response_format"),
        expand_graph: bool_field(args, "expand_graph"),
        explain: bool_field(args, "explain"),
    })
}

fn parse_memory_add(args: &Map<String, Value>) -> Result<MemoryAddArgs, McpError> {
    let fragment = require_str(args, "fragment")?;
    let evidence = args
        .get("evidence")
        .and_then(|v| v.as_object())
        .map(|o| {
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
            Ok(crate::compatibility::lemma::tool_args::MemoryEvidence {
                file,
                symbol: o
                    .get("symbol")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                snippet,
            })
        })
        .transpose()?;
    Ok(MemoryAddArgs {
        fragment,
        title: str_field(args, "title").map(|s| s.to_string()),
        description: str_field(args, "description").map(|s| s.to_string()),
        project: str_field(args, "project").map(|s| s.to_string()),
        source: str_field(args, "source").map(|s| s.to_string()),
        confirm: bool_field(args, "confirm"),
        fragment_type: str_field(args, "type").map(|s| s.to_string()),
        evidence,
    })
}

fn parse_memory_update(args: &Map<String, Value>) -> Result<MemoryUpdateArgs, McpError> {
    Ok(MemoryUpdateArgs {
        id: require_str(args, "id")?,
        title: str_field(args, "title").map(|s| s.to_string()),
        fragment: str_field(args, "fragment").map(|s| s.to_string()),
        confidence: f64_field(args, "confidence"),
    })
}

fn parse_memory_feedback(args: &Map<String, Value>) -> Result<MemoryFeedbackArgs, McpError> {
    let useful = args
        .get("useful")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| McpError::invalid_params("useful is required", None))?;
    Ok(MemoryFeedbackArgs {
        id: require_str(args, "id")?,
        useful,
    })
}

fn parse_memory_forget(args: &Map<String, Value>) -> Result<MemoryForgetArgs, McpError> {
    Ok(MemoryForgetArgs {
        id: require_str(args, "id")?,
        consolidate: bool_field(args, "consolidate"),
        invalidate: bool_field(args, "invalidate"),
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
        project: str_field(args, "project").map(|s| s.to_string()),
        consolidate: bool_field(args, "consolidate"),
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
        note: str_field(args, "note").map(|s| s.to_string()),
    })
}

fn parse_memory_stats(args: &Map<String, Value>) -> Result<MemoryStatsArgs, McpError> {
    Ok(MemoryStatsArgs {
        project: str_field(args, "project").map(|s| s.to_string()),
        response_format: response_format_field(args, "response_format"),
    })
}

fn parse_memory_audit(args: &Map<String, Value>) -> Result<MemoryAuditArgs, McpError> {
    Ok(MemoryAuditArgs {
        response_format: response_format_field(args, "response_format"),
    })
}

fn parse_memory_library(args: &Map<String, Value>) -> Result<MemoryLibraryArgs, McpError> {
    Ok(MemoryLibraryArgs {
        project: str_field(args, "project").map(|s| s.to_string()),
        focus: str_field(args, "focus").map(|s| s.to_string()),
        limit: usize_field(args, "limit"),
        offset: usize_field(args, "offset"),
        response_format: response_format_field(args, "response_format"),
    })
}

fn parse_semantic_search(args: &Map<String, Value>) -> Result<SemanticSearchArgs, McpError> {
    Ok(SemanticSearchArgs {
        query: require_str(args, "query")?,
        project: str_field(args, "project").map(|s| s.to_string()),
        top_k: usize_field(args, "topK"),
        offset: usize_field(args, "offset"),
        hybrid: bool_field(args, "hybrid"),
        explain: bool_field(args, "explain"),
        response_format: response_format_field(args, "response_format"),
    })
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
        // The client is shared across calls; lock it for the round-trip.
        let mut client = self.client.lock().await;
        // Ensure the connection is established and handshaked.
        if !client.is_connected() {
            client
                .connect()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        if client.retry_epoch().is_none() {
            let hs_req = HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                store_generation: self.identity.store_generation,
                frontend_id: self.identity.frontend_id,
                channel_id: self.identity.channel_id,
            };
            client
                .handshake(&hs_req)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            // Prefetch the memory snapshot for the dynamic instructions index.
            // Best-effort: a failure leaves the empty-state instructions.
            if let Ok(memories) = self.fetch_snapshot(&mut client).await {
                self.set_snapshot(memories);
            }
        }
        let retry_epoch = client.retry_epoch().unwrap_or(0);
        let envelope = self.build_envelope(&request, retry_epoch)?;
        let resp = client
            .roundtrip(&envelope)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        drop(client);

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
/// after these).
fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "memory_add"
            | "memory_update"
            | "memory_feedback"
            | "memory_forget"
            | "memory_merge"
            | "memory_relate"
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
    fn serves_eleven_frozen_tools() {
        let tools = LtmrsFrontend::tools();
        assert_eq!(tools.len(), 11, "must serve exactly 11 WP-08 tools");
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"memory_read".to_string()));
        assert!(names.contains(&"semantic_search".to_string()));
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
        assert_eq!(served.len(), frozen.len());
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

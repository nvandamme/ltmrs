//! MCP server handler: tool listing, dispatch, result shaping (moved verbatim from `mcp.rs`).

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ServerCapabilities,
    ServerNotification, ToolListChangedNotification, ToolListChangedNotificationMethod,
};
use rmcp::service::{MaybeSendFuture, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};

use super::{INSTRUCTIONS_TEMPLATE, LtmrsFrontend, build_instructions_index, route_tool};

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
        let bound = Self::bound_identity_line(cwd.as_deref());
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
            Self::lemma_resource_list(),
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
        if !Self::is_known_tool(&request.name) {
            return Ok(CallToolResponse::Complete(Self::unknown_tool_result(
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
pub(crate) fn is_mutating_tool(name: &str) -> bool {
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
pub(crate) fn shape_result(resp: &ltmrs_daemon::envelope::IpcResponse) -> CallToolResult {
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

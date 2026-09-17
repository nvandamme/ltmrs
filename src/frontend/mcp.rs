//! rmcp stdio boundary: routes MCP tool calls to typed IPC envelopes.
//!
//! No domain logic lives here — each tool call maps to a `DomainRequest`
//! carried in an IPC envelope. The mapping is pure and testable.

use std::sync::Arc;

use tokio::sync::Mutex;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};

use crate::daemon::client::IpcClient;
use crate::daemon::envelope::{DomainRequest, HandshakeRequest, IpcEnvelope, PROTOCOL_VERSION};
use crate::domain::command::Scope;
use crate::domain::id::{ChannelId, EntityId, FrontendId, OperationId, StoreGeneration};
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

/// The MCP frontend handler: terminates stdio and routes tools over IPC.
pub struct LtmrsFrontend {
    identity: FrontendIdentity,
    client: Mutex<IpcClient>,
}

impl LtmrsFrontend {
    pub fn new(identity: FrontendIdentity, client: IpcClient) -> Self {
        Self {
            identity,
            client: Mutex::new(client),
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

    /// The tool list advertised to the host.
    pub fn tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "memory_add",
                "Save a memory fragment",
                schema(&json!({
                    "type": "object",
                    "properties": {
                        "fragment": {"type": "string"},
                        "title": {"type": "string"}
                    },
                    "required": ["fragment"]
                })),
            ),
            Tool::new(
                "memory_read",
                "Read memories",
                schema(&json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"}
                    }
                })),
            ),
        ]
    }
}

/// Extract the object map from a JSON value for use as a tool input schema.
fn schema(v: &Value) -> Arc<Map<String, Value>> {
    Arc::new(
        v.as_object()
            .expect("tool schema must be a JSON object")
            .clone(),
    )
}

/// Route a tool name + arguments to a typed `DomainRequest`.
pub fn route_tool(
    name: &str,
    args: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<DomainRequest, McpError> {
    let args = args.clone().unwrap_or_default();
    match name {
        "memory_add" => {
            let fragment = args
                .get("fragment")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError::invalid_params("fragment is required", None))?
                .to_string();
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| fragment.chars().take(80).collect());
            Ok(DomainRequest::AddMemory {
                memory: build_memory(fragment, title),
            })
        }
        "memory_read" => Ok(DomainRequest::GetMemories { ids: vec![] }),
        other => Err(McpError::invalid_params(
            format!("unknown tool: {other}"),
            None,
        )),
    }
}

/// Build a minimal Memory for an add request.
fn build_memory(fragment: String, title: String) -> crate::domain::memory::Memory {
    use crate::domain::id::{DocumentRevision, EligibilityRevision, EntityRevision};
    use crate::domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
    Memory {
        id: EntityId::new(Uuid::now_v7()),
        external_alias: None,
        title,
        fragment,
        description: String::new(),
        fragment_type: FragmentType::Fact,
        project: None,
        source: MemorySource::Ai,
        confidence: 0.5,
        quality_score: None,
        lifecycle: MemoryLifecycle::Live,
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
        entity_revision: EntityRevision::new(1),
        document_revision: DocumentRevision::new(1),
        eligibility_revision: EligibilityRevision::new(1),
        created_at: Instant::new(0),
        updated_at: Instant::new(0),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    }
}

impl ServerHandler for LtmrsFrontend {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ltmrs", env!("CARGO_PKG_VERSION")))
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
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // The client is shared across calls; lock it for the round-trip.
        let mut client = self.client.lock().await;
        // Ensure the connection is established and handshaked. The handshake
        // validates protocol/generation and issues the retry namespace epoch.
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
        }
        let retry_epoch = client.retry_epoch().unwrap_or(0);
        let envelope = self.build_envelope(&request, retry_epoch)?;
        let resp = client
            .roundtrip(&envelope)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let text = serde_json::to_string(&resp)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        drop(client);
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(text),
        ])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(
        map: serde_json::Map<String, serde_json::Value>,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        Some(map)
    }

    #[test]
    fn routes_memory_add() {
        let mut m = serde_json::Map::new();
        m.insert("fragment".into(), json!("hello world"));
        let req = route_tool("memory_add", &args(m)).unwrap();
        match req {
            DomainRequest::AddMemory { memory } => {
                assert_eq!(memory.fragment, "hello world");
                assert_eq!(memory.title, "hello world");
            }
            _ => panic!("expected AddMemory"),
        }
    }

    #[test]
    fn memory_add_requires_fragment() {
        let req = route_tool("memory_add", &None);
        assert!(req.is_err());
    }

    #[test]
    fn routes_memory_read() {
        let req = route_tool("memory_read", &None).unwrap();
        assert!(matches!(req, DomainRequest::GetMemories { .. }));
    }

    #[test]
    fn unknown_tool_rejected() {
        let req = route_tool("nonexistent", &None);
        assert!(req.is_err());
    }

    #[test]
    fn envelope_carries_identity() {
        let id = FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        );
        let client = IpcClient::new(std::path::PathBuf::from("unused"));
        let fe = LtmrsFrontend::new(id.clone(), client);
        let params = CallToolRequestParams::new("memory_add").with_arguments(
            json!({
                "fragment": "x"
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let env = fe.build_envelope(&params, 7).unwrap();
        assert_eq!(env.frontend_id, id.frontend_id);
        assert_eq!(env.channel_id, id.channel_id);
        assert_eq!(env.protocol_version, PROTOCOL_VERSION);
        assert_eq!(env.retry_epoch, 7);
    }
}

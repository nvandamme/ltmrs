//! Frontend: rmcp stdio boundary and CLI, no duplicate domain handlers.
//!
//! The frontend terminates stdio via rmcp and routes tool calls to the daemon
//! over IPC. It holds no domain logic of its own: every tool call becomes a
//! typed IPC envelope. This module wires the rmcp `ServerHandler` to the IPC
//! client and keeps the routing logic testable.

pub mod mcp;

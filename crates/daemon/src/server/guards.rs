//! Connection guards and wall clock (moved verbatim from `server.rs`).

use std::sync::Arc;

use crate::dispatcher::Dispatcher;
use crate::limits::QuotaTracker;

/// Wall-clock millis for idle tracking (same shape as the maintenance
/// retention clock; serve has no injected clock by design).
pub(crate) fn wall_now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Tracks one live IPC connection in the frontend registry (RAII decrement
/// on drop, mirroring `ClientGuard`). Restore readiness counts live
/// connections, never persisted channel history.
pub(crate) struct LiveConnGuard {
    dispatcher: Arc<Dispatcher>,
}

impl LiveConnGuard {
    pub(crate) fn new(dispatcher: &Arc<Dispatcher>) -> Self {
        dispatcher.registry().note_live_connect();
        Self {
            dispatcher: Arc::clone(dispatcher),
        }
    }
}

impl Drop for LiveConnGuard {
    fn drop(&mut self) {
        self.dispatcher.registry().note_live_disconnect();
    }
}

/// Unregisters the connection's frontend from the quota table on drop, so a
/// disconnect frees its client slot on every return path above. Bound to the
/// handshake-authenticated ID at construction (never request-claimed IDs).
pub(crate) struct ClientGuard {
    quotas: Arc<QuotaTracker>,
    frontend: ltmrs_domain::id::FrontendId,
    channel: ltmrs_domain::id::ChannelId,
}

impl ClientGuard {
    pub(crate) fn new(
        quotas: Arc<QuotaTracker>,
        frontend: ltmrs_domain::id::FrontendId,
        channel: ltmrs_domain::id::ChannelId,
    ) -> Self {
        Self {
            quotas,
            frontend,
            channel,
        }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.quotas.unregister_client(self.frontend, self.channel);
    }
}

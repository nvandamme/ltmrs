//! Frontend channel registry and per-channel session binding (design §7.2).
//!
//! Legacy `session_start`/`session_attempt`/`session_end` operate on the
//! session **for that frontend channel**, never a daemon-global current
//! session (RV-05). Two independently identified MCP frontends stay isolated:
//! one channel's `session_end` cannot end another channel's session, even if
//! the MCP numeric request IDs collide.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use ltmrs_domain::id::{ChannelId, FrontendId, SessionHandle};
use ltmrs_domain::memory::Instant;
use ltmrs_domain::session::{Session, SessionStatus, TaskOutcome};
use uuid::Uuid;

/// A channel's lease: when it expires the channel's session may be abandoned
/// per the profile's virtual-session rules.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Lease {
    pub expires_at: u64,
}

/// Upstream virtual-session timeouts, applied per channel (never
/// daemon-global, RV-05): idle virtual sessions finalize after 120s without
/// a touch; no virtual session lives past 30 minutes from its start.
pub const VIRTUAL_IDLE_TIMEOUT_MILLIS: u64 = 120_000;
pub const VIRTUAL_LIFETIME_MILLIS: u64 = 1_800_000;

/// A registered frontend channel with its bound session and lease.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChannelBinding {
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
    /// The bound session handle (implicit legacy or explicit native).
    pub session: Option<SessionHandle>,
    /// The channel's virtual session for session-less calls (never shadows
    /// a traced session; see `ensure_virtual_session`). Defaults to none so
    /// pre-virtual snapshots load cleanly.
    #[serde(default)]
    pub virtual_session: Option<SessionHandle>,
    /// Idle deadline for the virtual session above. Separate from `lease`
    /// (which governs traced-session expiry): virtual touches must never
    /// extend a traced session's lease, and traced cycles must never wipe
    /// virtual idle tracking.
    #[serde(default)]
    pub virtual_lease: Option<Lease>,
    pub lease: Option<Lease>,
    /// Whether this channel uses an explicit native session binding.
    pub explicit: bool,
}

/// The frontend registry: per-(frontend, channel) routing state.
///
/// There is deliberately no daemon-global "current session". Every operation
/// is routed by (frontend_id, channel_id).
///
/// Durable traced-session state (sessions, attempts, outcomes, operation
/// receipts) lives in the canonical store ([`CanonicalRepository`]); this
/// registry keeps only routing and ephemeral state: channel bindings (which
/// handle a channel currently addresses), leases, live-connection counts
/// and implicit virtual sessions for session-less calls.
pub struct FrontendRegistry {
    channels: HashMap<(FrontendId, ChannelId), ChannelBinding>,
    /// Implicit per-channel virtual sessions only. Traced sessions live in
    /// the canonical store; handles here never collide with them (UUIDv7).
    virtual_sessions: HashMap<SessionHandle, Session>,
    /// Live IPC connections serving right now (incremented on connect,
    /// decremented on drop). Restore readiness counts these — never the
    /// persisted channel history, whose dead entries outlive their runs
    /// and would otherwise block every restore after a daemon restart.
    live: AtomicUsize,
}

/// Traced sessions + operation receipts recovered from a pre-migration
/// sessions.json file. The daemon imports these into the canonical store
/// once at startup (idempotent by handle/operation ID); bindings and
/// virtual sessions stay in the registry file.
pub struct LegacyPayload {
    pub sessions: Vec<Session>,
    pub receipts: Vec<(String, ltmrs_domain::session::SessionReceipt)>,
}

impl FrontendRegistry {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
            virtual_sessions: HashMap::new(),
            live: AtomicUsize::new(0),
        }
    }

    /// Bind a channel to a session handle (after the canonical store
    /// created or verified it). Preserves the channel's virtual session.
    pub fn bind_session(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        handle: SessionHandle,
        explicit: bool,
    ) {
        let k = Self::key(frontend_id, channel_id);
        let existing = self
            .channels
            .get(&k)
            .map(|b| b.channel_id)
            .unwrap_or(channel_id);
        let virtual_session = self.channels.get(&k).and_then(|b| b.virtual_session);
        let virtual_lease = self.channels.get(&k).and_then(|b| b.virtual_lease);
        self.channels.insert(
            k,
            ChannelBinding {
                frontend_id,
                channel_id: existing,
                session: Some(handle),
                virtual_session,
                lease: None,
                virtual_lease,
                explicit,
            },
        );
    }

    fn key(frontend_id: FrontendId, channel_id: ChannelId) -> (FrontendId, ChannelId) {
        (frontend_id, channel_id)
    }

    /// Bind an explicit native session handle to a channel. The session
    /// record itself lives in the canonical store; this only routes.
    pub fn bind_native_session(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        handle: SessionHandle,
    ) {
        self.bind_session(frontend_id, channel_id, handle, true);
    }

    /// The session handle a channel is bound to (None if unbound).
    /// Routing only — liveness comes from the canonical store via
    /// `Dispatcher::resolve_session`, never from this cache.
    pub fn channel_session(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
    ) -> Option<SessionHandle> {
        self.channels
            .get(&Self::key(frontend_id, channel_id))?
            .session
    }

    /// Reset every pre-restore execution context (P2-A restore semantics):
    /// a generation cut invalidates traced routes, leases, virtual routes,
    /// virtual leases AND the virtual session store — no runtime context
    /// may span the cut (a reused virtual session would mix pre- and
    /// post-restore memories in `memories_created`, and virtual state
    /// persists in sessions.json across restarts). Channels stay
    /// registered; the next call on each channel binds fresh. Returns the
    /// number of traced routes dropped (for the restore report).
    pub fn reset_execution_contexts(&mut self) -> usize {
        let mut dropped = 0;
        for binding in self.channels.values_mut() {
            if binding.session.is_some() {
                binding.session = None;
                dropped += 1;
            }
            binding.lease = None;
            binding.virtual_session = None;
            binding.virtual_lease = None;
        }
        self.virtual_sessions.clear();
        dropped
    }

    /// The channel's live virtual session, if one is bound (for tests and
    /// session-less attribution). Traced sessions are NOT returned here;
    /// use the canonical store (via the dispatcher) for those.
    pub fn virtual_session(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
    ) -> Option<SessionHandle> {
        let k = Self::key(frontend_id, channel_id);
        let handle = self.channels.get(&k)?.virtual_session?;
        let session = self.virtual_sessions.get(&handle)?;
        if session.is_virtual && !session.status.is_terminal() {
            Some(handle)
        } else {
            None
        }
    }

    /// Ensure session context for a session-less call on a channel
    /// (per-channel virtual sessions, WP-09). Returns the channel's live
    /// virtual session (lease refreshed) or creates one. Traced sessions
    /// are NOT consulted here — callers check the canonical store first
    /// (via `Dispatcher::resolve_session`) and only fall back to this
    /// when no traced session is bound.
    pub fn ensure_virtual_session(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        now_millis: u64,
    ) -> SessionHandle {
        self.sweep_virtual_sessions(now_millis);
        let k = Self::key(frontend_id, channel_id);
        if let Some(handle) = self.channels.get(&k).and_then(|b| b.virtual_session)
            && let Some(session) = self.virtual_sessions.get(&handle)
            && session.is_virtual
            && !session.status.is_terminal()
            && let Some(binding) = self.channels.get_mut(&k)
        {
            binding.virtual_lease = Some(Lease {
                expires_at: now_millis + VIRTUAL_IDLE_TIMEOUT_MILLIS,
            });
            return handle;
        }
        let handle = SessionHandle::new(Uuid::now_v7());
        let session = Session {
            handle,
            channel_id,
            project: None,
            task_type: None,
            technologies: Vec::new(),
            status: SessionStatus::Active,
            attempts: Vec::new(),
            outcome: None,
            final_approach: None,
            lessons: Vec::new(),
            initial_approach: None,
            guides_used: Vec::new(),
            memories_read: Vec::new(),
            memories_created: Vec::new(),
            refinement_attempts: 0,
            self_critique_count: 0,
            started_at: Instant::new(now_millis),
            ended_at: None,
            is_virtual: true,
        };
        self.virtual_sessions.insert(handle, session);
        let binding = self.channels.entry(k).or_insert(ChannelBinding {
            frontend_id,
            channel_id,
            session: None,
            virtual_session: None,
            lease: None,
            virtual_lease: None,
            explicit: false,
        });
        binding.virtual_session = Some(handle);
        binding.virtual_lease = Some(Lease {
            expires_at: now_millis + VIRTUAL_IDLE_TIMEOUT_MILLIS,
        });
        handle
    }

    /// Finalize virtual sessions past idle timeout or lifetime bound.
    /// Returns finalized handles. Only virtual sessions live here, so
    /// only they are ever touched.
    pub fn sweep_virtual_sessions(&mut self, now_millis: u64) -> Vec<SessionHandle> {
        let mut finalized = Vec::new();
        let keys: Vec<(FrontendId, ChannelId)> = self.channels.keys().cloned().collect();
        for k in keys {
            let handle = match self.channels.get(&k).and_then(|b| b.virtual_session) {
                Some(h) => h,
                None => continue,
            };
            let expired = match self.virtual_sessions.get(&handle) {
                Some(s) if s.is_virtual && !s.status.is_terminal() => {
                    let idle = match self.channels.get(&k).and_then(|b| b.virtual_lease) {
                        Some(lease) => now_millis >= lease.expires_at,
                        None => true,
                    };
                    let aged = now_millis.saturating_sub(s.started_at.as_millis())
                        >= VIRTUAL_LIFETIME_MILLIS;
                    idle || aged
                }
                _ => false,
            };
            if !expired {
                continue;
            }
            if let Some(s) = self.virtual_sessions.get_mut(&handle) {
                s.status = SessionStatus::Abandoned;
                s.outcome = Some(TaskOutcome::Abandoned);
                s.ended_at = Some(Instant::new(now_millis));
            }
            if let Some(binding) = self.channels.get_mut(&k)
                && binding.virtual_session == Some(handle)
            {
                binding.virtual_session = None;
                binding.virtual_lease = None;
            }
            finalized.push(handle);
        }
        finalized
    }

    /// Whether a channel uses an explicit native binding.
    pub fn is_explicit(&self, frontend_id: FrontendId, channel_id: ChannelId) -> bool {
        self.channels
            .get(&Self::key(frontend_id, channel_id))
            .map(|b| b.explicit)
            .unwrap_or(false)
    }

    /// Set a lease on a channel.
    pub fn set_lease(&mut self, frontend_id: FrontendId, channel_id: ChannelId, lease: Lease) {
        let k = Self::key(frontend_id, channel_id);
        if let Some(b) = self.channels.get_mut(&k) {
            b.lease = Some(lease);
        }
    }

    /// Number of registered channels (for health/diagnostics).
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Note a live IPC connection (paired with `note_live_disconnect`
    /// via RAII in the connection task).
    pub fn note_live_connect(&self) {
        self.live.fetch_add(1, Ordering::SeqCst);
    }

    /// Note a live IPC connection closing. Saturating: an imbalance
    /// must degrade to a possibly-stale positive (recoverable by
    /// restart), never wrap to usize::MAX (blocking restores forever).
    pub fn note_live_disconnect(&self) {
        let prev = self.live.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "live-connection count imbalance");
        if prev == 0 {
            self.live.store(0, Ordering::SeqCst);
        }
    }

    /// Live IPC connections right now. Restore readiness uses this —
    /// persisted channel bindings outlive their runs and must never
    /// block a restore after a daemon restart.
    pub fn live_connection_count(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// A virtual session record by handle, if present.
    pub fn virtual_record(&self, handle: SessionHandle) -> Option<&Session> {
        self.virtual_sessions.get(&handle)
    }

    /// Record created-memory links on a virtual session (deduped).
    /// Virtual sessions are routing-ephemeral, so this is best-effort
    /// attribution, never durability. No-op for unknown handles. (Guides
    /// used and memories read only ever attach to traced sessions, which
    /// live in the canonical store.)
    pub fn track_virtual_created(&mut self, handle: SessionHandle, ids: &[String]) {
        if let Some(session) = self.virtual_sessions.get_mut(&handle) {
            for id in ids {
                if !session.memories_created.contains(id) {
                    session.memories_created.push(id.clone());
                }
            }
        }
    }

    /// Persist routing + ephemeral state (channel bindings, leases, virtual
    /// sessions) to a JSON file. Traced sessions and operation receipts
    /// live in the canonical store, not here. Atomic tmp+rename plus file
    /// + directory synchronization, as before.
    pub fn persist(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let snapshot = RegistrySnapshot {
            channels: self
                .channels
                .iter()
                .map(|((fe, ch), b)| ChannelEntry {
                    frontend_id: *fe,
                    channel_id: *ch,
                    binding: b.clone(),
                })
                .collect(),
            virtual_sessions: self
                .virtual_sessions
                .iter()
                .map(|(h, s)| SessionEntry {
                    handle: *h,
                    session: s.clone(),
                })
                .collect(),
            // Never written back: legacy read-only shape for old files.
            sessions: Vec::new(),
            op_log: HashMap::new(),
        };
        let json = serde_json::to_vec_pretty(&snapshot)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        // Durability barrier (re-review R1): flush file data before the
        // rename makes it visible, then flush the directory entry. Every
        // step participates in the result — a discarded sync error would
        // let callers acknowledge unflushed state as durable. Windows
        // `FlushFileBuffers` requires write access, and refuses to rename
        // a file opened without FILE_SHARE_DELETE, so the handle is opened
        // write-only and closed before the rename.
        let f = std::fs::OpenOptions::new().write(true).open(&tmp)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        // Unix only: the parent-dir flush pins the rename in the directory
        // entry. Windows has no openable directory handle; the rename is
        // atomic and NTFS flushes metadata with the handle close.
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            let dir = std::fs::File::open(parent).map_err(|e| {
                std::io::Error::other(format!("cannot open parent dir for sync: {e}"))
            })?;
            dir.sync_all()
                .map_err(|e| std::io::Error::other(format!("cannot sync parent dir: {e}")))?;
        }
        Ok(())
    }

    /// Load the registry state from a JSON file. A missing file yields an
    /// empty registry (first run). Returns the registry (bindings +
    /// virtual sessions) plus any pre-migration traced sessions and
    /// operation receipts, which the daemon imports into the canonical
    /// store once at startup. Unknown snapshot fields (old `handle_gen`,
    /// `op_log`, full `sessions`) are tolerated for forward reading.
    pub fn load(path: &std::path::Path) -> Result<(Self, LegacyPayload), std::io::Error> {
        if !path.exists() {
            return Ok((
                Self::new(),
                LegacyPayload {
                    sessions: Vec::new(),
                    receipts: Vec::new(),
                },
            ));
        }
        let bytes = std::fs::read(path)?;
        let snapshot: RegistrySnapshot =
            serde_json::from_slice(&bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut channels = HashMap::new();
        for e in snapshot.channels {
            channels.insert((e.frontend_id, e.channel_id), e.binding);
        }
        let mut virtual_sessions = HashMap::new();
        let mut legacy_sessions = Vec::new();
        // Current files carry virtual sessions under their own key;
        // pre-migration files only have the shared `sessions` list, split
        // here by the virtual flag.
        for e in snapshot.virtual_sessions {
            virtual_sessions.insert(e.handle, e.session);
        }
        for e in snapshot.sessions {
            if e.session.is_virtual {
                virtual_sessions.insert(e.handle, e.session);
            } else {
                legacy_sessions.push(e.session);
            }
        }
        let legacy_receipts = snapshot
            .op_log
            .into_iter()
            .map(|(id, rec)| {
                (
                    id,
                    ltmrs_domain::session::SessionReceipt {
                        digest: rec.digest,
                        session: rec.session,
                        seq: None,
                        response: None,
                        continuity_boosted: false,
                        scope: None,
                    },
                )
            })
            .collect();
        Ok((
            Self {
                channels,
                virtual_sessions,
                // Live connections never persist: a fresh process starts at zero.
                live: AtomicUsize::new(0),
            },
            LegacyPayload {
                sessions: legacy_sessions,
                receipts: legacy_receipts,
            },
        ))
    }
}

/// A serializable snapshot of the registry for persistence across restarts.
/// Extra fields from older snapshots (`handle_gen`, full `sessions`,
/// `op_log`) are ignored on load; only bindings and virtual sessions are
/// written back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RegistrySnapshot {
    #[serde(default)]
    channels: Vec<ChannelEntry>,
    #[serde(default)]
    virtual_sessions: Vec<SessionEntry>,
    #[serde(default)]
    sessions: Vec<SessionEntry>,
    /// Pre-migration operation receipts, stored as a map exactly like the
    /// old `op_log`. Unknown entry fields are ignored; only digest and
    /// session carry over (attempt replays fall back to the imported
    /// session's attempt list for sequence numbers).
    #[serde(default)]
    op_log: HashMap<String, LegacyOpRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LegacyOpRecord {
    digest: String,
    session: SessionHandle,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ChannelEntry {
    frontend_id: FrontendId,
    channel_id: ChannelId,
    binding: ChannelBinding,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SessionEntry {
    handle: SessionHandle,
    session: Session,
}

impl Default for FrontendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fe(n: u64) -> FrontendId {
        FrontendId::new(Uuid::from_u128(n as u128))
    }
    fn ch(n: u64) -> ChannelId {
        ChannelId::new(Uuid::from_u128(n as u128))
    }
    fn hs(n: u128) -> SessionHandle {
        SessionHandle::new(Uuid::from_u128(n))
    }

    /// Bindings are per-channel routing: one channel's binding never leaks
    /// into another, and unknown channels bind nothing.
    #[test]
    fn bindings_are_isolated_per_channel() {
        let mut reg = FrontendRegistry::new();
        reg.bind_session(fe(1), ch(1), hs(11), false);
        reg.bind_session(fe(1), ch(2), hs(22), false);
        assert_eq!(reg.channel_session(fe(1), ch(1)), Some(hs(11)));
        assert_eq!(reg.channel_session(fe(1), ch(2)), Some(hs(22)));
        assert_eq!(reg.channel_session(fe(2), ch(99)), None);
        assert_eq!(reg.channel_session(fe(1), ch(3)), None);
        // Rebinding a channel moves only that channel.
        reg.bind_session(fe(1), ch(1), hs(33), false);
        assert_eq!(reg.channel_session(fe(1), ch(1)), Some(hs(33)));
        assert_eq!(reg.channel_session(fe(1), ch(2)), Some(hs(22)));
    }

    #[test]
    fn native_explicit_binding_is_tracked() {
        let mut reg = FrontendRegistry::new();
        let native = hs(999);
        reg.bind_native_session(fe(1), ch(1), native);
        assert!(reg.is_explicit(fe(1), ch(1)));
        assert_eq!(reg.channel_session(fe(1), ch(1)), Some(native));
        assert!(!reg.is_explicit(fe(1), ch(2)));
    }

    /// Generation cut resets every pre-restore execution context: traced
    /// routes, leases, virtual routes/leases AND the virtual session
    /// store. A virtual session must never span a generation boundary
    /// (its memories_created would mix pre- and post-restore memories).
    #[test]
    fn reset_execution_contexts_clears_virtual_state() {
        let mut reg = FrontendRegistry::new();
        reg.bind_session(fe(1), ch(1), hs(11), false);
        reg.set_lease(fe(1), ch(1), Lease { expires_at: 5000 });
        let v = reg.ensure_virtual_session(fe(1), ch(1), 0);
        assert!(reg.virtual_session(fe(1), ch(1)).is_some());
        let dropped = reg.reset_execution_contexts();
        assert_eq!(dropped, 1);
        assert_eq!(reg.channel_session(fe(1), ch(1)), None);
        assert_eq!(reg.virtual_session(fe(1), ch(1)), None);
        assert!(reg.virtual_record(v).is_none());
        // Channels stay registered (routes reset, channels kept).
        assert_eq!(reg.channel_count(), 1);
    }

    #[test]
    fn leases_are_settable_per_channel() {
        let mut reg = FrontendRegistry::new();
        reg.bind_session(fe(1), ch(1), hs(11), false);
        reg.set_lease(fe(1), ch(1), Lease { expires_at: 5000 });
        // Lease expiry no longer abandons sessions here (canonical store
        // owns terminal state); the lease is routing metadata only.
        assert_eq!(reg.channel_session(fe(1), ch(1)), Some(hs(11)));
        assert_eq!(reg.channel_count(), 1);
    }

    /// Bindings, leases and virtual sessions persist; traced sessions and
    /// receipts found in an old file split into the legacy payload for the
    /// canonical import (never into live registry state).
    #[test]
    fn persist_restores_bindings_and_splits_legacy_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        let mut reg = FrontendRegistry::new();
        reg.bind_session(fe(1), ch(1), hs(11), false);
        reg.bind_session(fe(1), ch(2), hs(22), false);
        reg.set_lease(fe(1), ch(1), Lease { expires_at: 5000 });
        let v = reg.ensure_virtual_session(fe(1), ch(9), 0);
        reg.persist(&path).unwrap();

        // Simulate restart: bindings + virtual restore, nothing traced.
        let (reg2, payload) = FrontendRegistry::load(&path).unwrap();
        assert_eq!(reg2.channel_session(fe(1), ch(1)), Some(hs(11)));
        assert_eq!(reg2.channel_session(fe(1), ch(2)), Some(hs(22)));
        assert_eq!(reg2.channel_session(fe(1), ch(99)), None);
        assert!(payload.sessions.is_empty());
        assert!(payload.receipts.is_empty());
        assert!(reg2.virtual_record(v).unwrap().is_virtual);
        // Live connections never persist.
        assert_eq!(reg2.live_connection_count(), 0);
        reg2.note_live_connect();
        assert_eq!(reg2.live_connection_count(), 1);
        reg2.note_live_disconnect();
        assert_eq!(reg2.live_connection_count(), 0);
    }

    /// A pre-migration snapshot (traced sessions + op receipts + counter)
    /// loads: bindings restore, traced state splits into the import
    /// payload, and unknown old fields are tolerated.
    #[test]
    fn legacy_snapshot_splits_traced_state_for_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let traced = ltmrs_domain::session::Session {
            handle: hs(77),
            channel_id: ch(1),
            project: Some("proj".into()),
            task_type: Some("debug".into()),
            technologies: Vec::new(),
            status: ltmrs_domain::session::SessionStatus::Active,
            attempts: Vec::new(),
            outcome: None,
            final_approach: None,
            lessons: Vec::new(),
            initial_approach: None,
            guides_used: Vec::new(),
            memories_read: Vec::new(),
            memories_created: Vec::new(),
            refinement_attempts: 0,
            self_critique_count: 0,
            started_at: Instant::new(0),
            ended_at: None,
            is_virtual: false,
        };
        let legacy = serde_json::json!({
            "handle_gen": 42u64,
            "channels": [
                {"frontend_id": fe(1), "channel_id": ch(1),
                 "binding": {
                    "frontend_id": fe(1), "channel_id": ch(1),
                    "session": hs(77), "virtual_session": null,
                    "virtual_lease": null, "lease": null, "explicit": false,
                 }},
            ],
            "sessions": [{"handle": hs(77), "session": traced}],
            "op_log": {"op-9": {
                "digest": "digest-9", "session": hs(77),
                "text": "text-9", "data": null, "effects": [],
            }},
        });
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let (reg, payload) = FrontendRegistry::load(&path).unwrap();
        // Binding restores (routing).
        assert_eq!(reg.channel_session(fe(1), ch(1)), Some(hs(77)));
        // Traced state splits out for the canonical import.
        assert_eq!(payload.sessions.len(), 1);
        assert_eq!(payload.sessions[0].project.as_deref(), Some("proj"));
        assert_eq!(payload.receipts.len(), 1);
        assert_eq!(payload.receipts[0].0, "op-9");
        assert_eq!(payload.receipts[0].1.digest, "digest-9");
        assert_eq!(payload.receipts[0].1.session, hs(77));
    }

    #[test]
    fn load_missing_file_yields_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let (reg, payload) = FrontendRegistry::load(&path).unwrap();
        assert_eq!(reg.channel_count(), 0);
        assert!(payload.sessions.is_empty());
        assert!(payload.receipts.is_empty());
    }

    /// Virtual sessions are stable per channel and isolated across
    /// channels. Traced shadowing lives in the dispatcher (canonical
    /// store), not here.
    #[test]
    fn virtual_session_stable_per_channel_and_isolated() {
        let mut reg = FrontendRegistry::new();
        let v1 = reg.ensure_virtual_session(fe(1), ch(1), 0);
        assert!(reg.virtual_record(v1).unwrap().is_virtual);
        // Same channel, still live: same handle.
        assert_eq!(reg.ensure_virtual_session(fe(1), ch(1), 1_000), v1);
        // Another channel gets its own virtual session.
        let v2 = reg.ensure_virtual_session(fe(1), ch(2), 1_000);
        assert_ne!(v1, v2);
    }

    /// Virtual sessions idle-finalize after 120s and die after 30min,
    /// matching the upstream timeouts per channel (never daemon-global).
    #[test]
    fn virtual_session_idle_finalize_and_lifetime_bound() {
        let mut reg = FrontendRegistry::new();
        let v1 = reg.ensure_virtual_session(fe(1), ch(1), 0);
        // Activity at 119s keeps it alive (lease refreshed).
        assert_eq!(reg.ensure_virtual_session(fe(1), ch(1), 119_999), v1);
        // Touch refreshed the deadline: still alive at 200s (would expire
        // at 120s without the refresh above).
        assert_eq!(
            reg.ensure_virtual_session(fe(1), ch(1), 200_000),
            v1,
            "touch must refresh the idle deadline"
        );
        // Idle past the refreshed deadline (200s + 120s): finalized, a
        // fresh virtual session starts.
        let v2 = reg.ensure_virtual_session(fe(1), ch(1), 321_000);
        assert_ne!(v1, v2);
        assert!(
            reg.virtual_record(v1)
                .is_none_or(|s| s.status.is_terminal())
        );
        // Lifetime bound: constant activity still retires at 30 minutes.
        let v3 = reg.ensure_virtual_session(fe(1), ch(2), 0);
        let mut t = 0u64;
        while t + 60_000 < 1_800_000 {
            t += 60_000;
            assert_eq!(reg.ensure_virtual_session(fe(1), ch(2), t), v3);
        }
        assert_eq!(reg.ensure_virtual_session(fe(1), ch(2), 1_799_999), v3);
        assert_ne!(
            v3,
            reg.ensure_virtual_session(fe(1), ch(2), 1_800_000),
            "30-minute lifetime bound must retire even active virtual sessions"
        );
    }

    #[test]
    fn reconnect_restores_only_verified_channel_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        let mut reg = FrontendRegistry::new();
        reg.bind_session(fe(1), ch(1), hs(11), false);
        reg.persist(&path).unwrap();

        // A reconnecting frontend presents (frontend_id, channel_id). The
        // daemon restores the binding only if it matches a persisted one.
        let (reg2, _) = FrontendRegistry::load(&path).unwrap();
        // Verified: the persisted channel is found.
        assert_eq!(reg2.channel_session(fe(1), ch(1)), Some(hs(11)));
        // Not verified: an unknown channel has no session (never guessed).
        assert_eq!(reg2.channel_session(fe(1), ch(99)), None);
        // Not verified: a different frontend cannot claim the session.
        assert_eq!(reg2.channel_session(fe(2), ch(1)), None);
    }
}

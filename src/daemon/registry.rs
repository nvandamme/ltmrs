//! Frontend channel registry and per-channel session binding (design §7.2).
//!
//! Legacy `session_start`/`session_attempt`/`session_end` operate on the
//! session **for that frontend channel**, never a daemon-global current
//! session (RV-05). Two independently identified MCP frontends stay isolated:
//! one channel's `session_end` cannot end another channel's session, even if
//! the MCP numeric request IDs collide.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::domain::id::{ChannelId, FrontendId, SessionHandle};
use crate::domain::memory::Instant;
use crate::domain::session::{Attempt, Session, SessionStatus, TaskOutcome};
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

/// The frontend registry: per-(frontend, channel) session state.
///
/// There is deliberately no daemon-global "current session". Every operation
/// is routed by (frontend_id, channel_id).
pub struct FrontendRegistry {
    channels: HashMap<(FrontendId, ChannelId), ChannelBinding>,
    sessions: HashMap<SessionHandle, Session>,
    handle_gen: AtomicU64,
    /// Durable operation receipts for session/tool operations (re-review
    /// R5): keyed by operation ID, bound to the request digest. A retried
    /// operation with the same ID + digest replays its recorded outcome;
    /// the same ID with a different digest is rejected as key reuse.
    /// Persisted in sessions.json alongside sessions (ephemeral leases and
    /// live counts stay out, as before).
    op_log: HashMap<String, OpRecord>,
    /// Live IPC connections serving right now (incremented on connect,
    /// decremented on drop). Restore readiness counts these — never the
    /// persisted channel history, whose dead entries outlive their runs
    /// and would otherwise block every restore after a daemon restart.
    live: AtomicUsize,
}

/// A durable receipt for one completed session/tool operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpRecord {
    /// The request digest the operation first executed with. Replays must
    /// match; mismatches are key-reuse conflicts, never silent replays.
    pub digest: String,
    /// The session this operation acted on.
    pub session: SessionHandle,
    /// Recorded response text (tool path), if any.
    pub text: Option<String>,
    /// Recorded structured data (tool path), if any.
    pub data: Option<serde_json::Value>,
    /// Effect markers for multi-step operations (e.g. session_end guide
    /// updates): effect keys applied under this operation, so a retry
    /// resumes the missing effects instead of duplicating completed ones.
    #[serde(default)]
    pub effects: Vec<String>,
}

/// The outcome of consulting the operation log before executing.
#[derive(Debug, Clone)]
pub enum OpCheck {
    /// No record: execute normally, then record the outcome.
    Fresh,
    /// Same ID + digest seen before: return the recorded outcome.
    Replay(OpRecord),
    /// Same ID, different digest: reject, never execute.
    Conflict,
}

impl FrontendRegistry {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
            sessions: HashMap::new(),
            handle_gen: AtomicU64::new(1),
            op_log: HashMap::new(),
            live: AtomicUsize::new(0),
        }
    }

    /// Consult the operation log before executing a session/tool operation.
    pub fn check_op(&self, operation_id: &str, digest: &str) -> OpCheck {
        match self.op_log.get(operation_id) {
            None => OpCheck::Fresh,
            Some(rec) if rec.digest == digest => OpCheck::Replay(rec.clone()),
            Some(_) => OpCheck::Conflict,
        }
    }

    /// Record a completed operation's outcome (durable once persisted).
    pub fn record_op(
        &mut self,
        operation_id: &str,
        digest: &str,
        session: SessionHandle,
        text: Option<String>,
        data: Option<serde_json::Value>,
    ) {
        self.op_log.insert(
            operation_id.to_string(),
            OpRecord {
                digest: digest.to_string(),
                session,
                text,
                data,
                effects: Vec::new(),
            },
        );
    }

    /// Mark one effect of a multi-step operation applied (idempotent).
    pub fn mark_effect_applied(&mut self, operation_id: &str, effect: &str) {
        if let Some(rec) = self.op_log.get_mut(operation_id)
            && !rec.effects.iter().any(|e| e == effect)
        {
            rec.effects.push(effect.to_string());
        }
    }

    /// Whether an effect key was already applied under an operation.
    pub fn effect_applied(&self, operation_id: &str, effect: &str) -> bool {
        self.op_log
            .get(operation_id)
            .is_some_and(|rec| rec.effects.iter().any(|e| e == effect))
    }

    fn next_handle(&self) -> SessionHandle {
        let n = self.handle_gen.fetch_add(1, Ordering::SeqCst);
        SessionHandle::new(Uuid::from_u128(n as u128))
    }

    fn key(frontend_id: FrontendId, channel_id: ChannelId) -> (FrontendId, ChannelId) {
        (frontend_id, channel_id)
    }

    /// Start (or resume) the legacy session for a channel. Returns the bound
    /// session handle. Idempotent per channel: a second start on the same
    /// channel resumes the existing session rather than creating a new one.
    pub fn start_session(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        project: Option<String>,
        task_type: Option<String>,
        now_millis: u64,
    ) -> SessionHandle {
        let k = Self::key(frontend_id, channel_id);
        if let Some(b) = self.channels.get(&k)
            && let Some(h) = b.session
            && let Some(s) = self.sessions.get(&h)
            && !s.status.is_terminal()
        {
            return h;
        }
        let handle = self.next_handle();
        let session = Session {
            handle,
            channel_id,
            project,
            task_type,
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
            is_virtual: false,
        };
        self.sessions.insert(handle, session);
        // The channel's virtual session (if any) survives traced cycles:
        // attribution continuity must not reset on session_start.
        let virtual_session = self.channels.get(&k).and_then(|b| b.virtual_session);
        let virtual_lease = self.channels.get(&k).and_then(|b| b.virtual_lease);
        self.channels.insert(
            k,
            ChannelBinding {
                frontend_id,
                channel_id,
                session: Some(handle),
                virtual_session,
                lease: None,
                virtual_lease,
                explicit: false,
            },
        );
        handle
    }

    /// Bind an explicit native session handle to a channel.
    pub fn bind_native_session(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        handle: SessionHandle,
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
                explicit: true,
            },
        );
        self.sessions.entry(handle).or_insert_with(|| Session {
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
            started_at: Instant::new(0),
            ended_at: None,
            is_virtual: false,
        });
    }

    /// Legacy `session_start` (WP-09): abandon the channel's existing active
    /// session (if any) and create a fresh traced session. Returns the new
    /// handle. Per-channel isolation is preserved (RV-05).
    pub fn start_legacy_session(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        task_type: String,
        technologies: Vec<String>,
        now_millis: u64,
    ) -> SessionHandle {
        let k = Self::key(frontend_id, channel_id);
        if let Some(b) = self.channels.get(&k)
            && let Some(h) = b.session
            && let Some(s) = self.sessions.get_mut(&h)
            && s.can_end()
        {
            s.status = SessionStatus::Abandoned;
            s.outcome = Some(TaskOutcome::Abandoned);
            s.ended_at = Some(Instant::new(now_millis));
        }
        let handle = self.next_handle();
        let session = Session {
            handle,
            channel_id,
            project: None,
            task_type: Some(task_type),
            technologies,
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
            is_virtual: false,
        };
        self.sessions.insert(handle, session);
        // The channel's virtual session (if any) survives traced cycles:
        // attribution continuity must not reset on session_start.
        let virtual_session = self.channels.get(&k).and_then(|b| b.virtual_session);
        let virtual_lease = self.channels.get(&k).and_then(|b| b.virtual_lease);
        self.channels.insert(
            k,
            ChannelBinding {
                frontend_id,
                channel_id,
                session: Some(handle),
                virtual_session,
                lease: None,
                virtual_lease,
                explicit: false,
            },
        );
        handle
    }

    /// Record an attempt on the channel's session. Returns the session handle.
    /// Fails if the channel has no active session. Idempotent on the
    /// deterministic attempt ID (P1 replay safety): a retried operation with
    /// the same `operation_id` reuses its UUIDv5 attempt ID, so a second
    /// record is a no-op returning the same handle — exactly one attempt and
    /// one counter increment per operation.
    pub fn record_attempt(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        attempt: Attempt,
    ) -> Option<SessionHandle> {
        let k = Self::key(frontend_id, channel_id);
        let handle = self.channels.get(&k)?.session?;
        let session = self.sessions.get_mut(&handle)?;
        if !session.can_end() {
            return Some(handle);
        }
        if session.attempts.iter().any(|a| a.id == attempt.id) {
            return Some(handle);
        }
        session.attempts.push(attempt);
        Some(handle)
    }

    /// Whether an attempt ID is already recorded on a session (replay check
    /// for counter increments that live outside `record_attempt`).
    pub fn has_attempt(
        &self,
        handle: SessionHandle,
        attempt_id: crate::domain::id::EntityId,
    ) -> bool {
        self.sessions
            .get(&handle)
            .is_some_and(|s| s.attempts.iter().any(|a| a.id == attempt_id))
    }

    /// Track a practiced guide into the channel's active session (lowercased,
    /// de-duplicated). Best-effort; no-op if no active session.
    pub fn track_guide_used(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        guide: &str,
    ) {
        let k = Self::key(frontend_id, channel_id);
        let handle = match self.channels.get(&k).and_then(|b| b.session) {
            Some(h) => h,
            None => return,
        };
        let session = match self.sessions.get_mut(&handle) {
            Some(s) => s,
            None => return,
        };
        let lower = guide.to_lowercase();
        if !session.guides_used.contains(&lower) {
            session.guides_used.push(lower);
        }
    }

    /// Track read memory IDs into the channel's active session (de-duplicated).
    pub fn track_memories_read(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        ids: &[String],
    ) {
        let k = Self::key(frontend_id, channel_id);
        let handle = match self.channels.get(&k).and_then(|b| b.session) {
            Some(h) => h,
            None => return,
        };
        let session = match self.sessions.get_mut(&handle) {
            Some(s) => s,
            None => return,
        };
        for id in ids {
            if !session.memories_read.contains(id) {
                session.memories_read.push(id.clone());
            }
        }
    }

    /// Track created memory IDs into the channel's active session.
    pub fn track_memories_created(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        ids: &[String],
    ) {
        let k = Self::key(frontend_id, channel_id);
        let handle = match self.channels.get(&k).and_then(|b| b.session) {
            Some(h) => h,
            None => return,
        };
        let session = match self.sessions.get_mut(&handle) {
            Some(s) => s,
            None => return,
        };
        for id in ids {
            if !session.memories_created.contains(id) {
                session.memories_created.push(id.clone());
            }
        }
    }

    /// Decay every attempt's confidence by `delta` (floored at 0). Called at
    /// session start so stale dead-ends lose priority over time.
    pub fn decay_attempts(&mut self, delta: f64) {
        for s in self.sessions.values_mut() {
            for a in &mut s.attempts {
                a.confidence = (a.confidence - delta).max(0.0);
            }
        }
    }

    /// Boost an attempt's confidence by `delta` (capped at 1) and bump its
    /// access counters. Best-effort; no-op if not found.
    pub fn boost_attempt(&mut self, handle: SessionHandle, seq: u32, delta: f64, now: u64) {
        if let Some(s) = self.sessions.get_mut(&handle)
            && let Some(a) = s.attempts.iter_mut().find(|a| a.seq == seq)
        {
            a.confidence = (a.confidence + delta).min(1.0);
            a.access_count += 1;
            a.last_accessed_at = Some(Instant::new(now));
        }
    }

    /// Penalize an attempt's confidence by `delta` (floored at 0) and bump its
    /// access counters. Best-effort; no-op if not found.
    pub fn penalize_attempt(&mut self, handle: SessionHandle, seq: u32, delta: f64, now: u64) {
        if let Some(s) = self.sessions.get_mut(&handle)
            && let Some(a) = s.attempts.iter_mut().find(|a| a.seq == seq)
        {
            a.confidence = (a.confidence - delta).max(0.0);
            a.access_count += 1;
            a.last_accessed_at = Some(Instant::new(now));
        }
    }

    /// All sessions as owned clones (for analytics over the canonical snapshot).
    pub fn all_sessions_owned(&self) -> Vec<crate::domain::session::Session> {
        self.sessions.values().cloned().collect()
    }

    /// End the channel's session. Only affects THIS channel's session — a
    /// different channel's session is untouched even with a colliding MCP
    /// request ID. Returns the ended session handle.
    pub fn end_session(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        outcome: TaskOutcome,
        final_approach: Option<String>,
        lessons: Vec<String>,
        now_millis: u64,
    ) -> Option<SessionHandle> {
        let k = Self::key(frontend_id, channel_id);
        let handle = self.channels.get(&k)?.session?;
        let session = self.sessions.get_mut(&handle)?;
        if session.can_end() {
            session.status = SessionStatus::Ended;
            session.outcome = Some(outcome);
            session.final_approach = final_approach;
            session.lessons = lessons;
            session.ended_at = Some(Instant::new(now_millis));
        }
        Some(handle)
    }

    /// Abandon every live session (restore path): marks non-terminal
    /// sessions Abandoned with the restore timestamp and returns the count.
    /// Backup sessions stay abandoned history (upstream parity) — reported,
    /// never resurrected into live channels.
    pub fn abandon_all_sessions(&mut self, now_millis: u64) -> usize {
        let mut abandoned = 0;
        for session in self.sessions.values_mut() {
            if session.can_end() {
                session.status = crate::domain::session::SessionStatus::Abandoned;
                session.ended_at = Some(crate::domain::memory::Instant::new(now_millis));
                abandoned += 1;
            }
        }
        abandoned
    }

    /// Resolve the active session for a channel (None if none or terminal).
    pub fn resolve_session(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
    ) -> Option<SessionHandle> {
        let k = Self::key(frontend_id, channel_id);
        let handle = self.channels.get(&k)?.session?;
        let session = self.sessions.get(&handle)?;
        if session.status.is_terminal() {
            None
        } else {
            Some(handle)
        }
    }

    /// The channel's live virtual session, if one is bound (for tests and
    /// session-less attribution). Traced sessions are NOT returned here;
    /// use `resolve_session` for those.
    pub fn virtual_session(
        &self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
    ) -> Option<SessionHandle> {
        let k = Self::key(frontend_id, channel_id);
        let handle = self.channels.get(&k)?.virtual_session?;
        let session = self.sessions.get(&handle)?;
        if session.is_virtual && !session.status.is_terminal() {
            Some(handle)
        } else {
            None
        }
    }

    /// Ensure session context for a session-less call on a channel
    /// (per-channel virtual sessions, WP-09): a traced session shadows
    /// everything and is returned as-is; otherwise the channel's live
    /// virtual session is returned (lease refreshed); otherwise a fresh
    /// virtual session is created and bound. Expired virtual sessions are
    /// swept first, so this never returns a dead handle.
    pub fn ensure_virtual_session(
        &mut self,
        frontend_id: FrontendId,
        channel_id: ChannelId,
        now_millis: u64,
    ) -> SessionHandle {
        self.sweep_virtual_sessions(now_millis);
        if let Some(handle) = self.resolve_session(frontend_id, channel_id) {
            return handle;
        }
        let k = Self::key(frontend_id, channel_id);
        if let Some(handle) = self.channels.get(&k).and_then(|b| b.virtual_session)
            && let Some(session) = self.sessions.get(&handle)
            && session.is_virtual
            && !session.status.is_terminal()
            && let Some(binding) = self.channels.get_mut(&k)
        {
            binding.virtual_lease = Some(Lease {
                expires_at: now_millis + VIRTUAL_IDLE_TIMEOUT_MILLIS,
            });
            return handle;
        }
        let handle = self.next_handle();
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
        self.sessions.insert(handle, session);
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
    /// Returns finalized handles. Only virtual sessions are touched;
    /// traced sessions follow the lease path (`expire_leases`) instead.
    pub fn sweep_virtual_sessions(&mut self, now_millis: u64) -> Vec<SessionHandle> {
        let mut finalized = Vec::new();
        let keys: Vec<(FrontendId, ChannelId)> = self.channels.keys().cloned().collect();
        for k in keys {
            let handle = match self.channels.get(&k).and_then(|b| b.virtual_session) {
                Some(h) => h,
                None => continue,
            };
            let expired = match self.sessions.get(&handle) {
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
            if let Some(s) = self.sessions.get_mut(&handle) {
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

    /// Abandon sessions whose leases have expired. Returns abandoned handles.
    pub fn expire_leases(&mut self, now_millis: u64) -> Vec<SessionHandle> {
        let keys: Vec<(FrontendId, ChannelId)> = self.channels.keys().cloned().collect();
        let mut abandoned = Vec::new();
        for k in keys {
            if let Some(b) = self.channels.get(&k)
                && let Some(lease) = b.lease
                && lease.expires_at <= now_millis
                && let Some(h) = b.session
                && let Some(s) = self.sessions.get_mut(&h)
                && s.can_end()
            {
                s.status = SessionStatus::Abandoned;
                s.ended_at = Some(Instant::new(now_millis));
                abandoned.push(h);
            }
        }
        abandoned
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

    /// Number of sessions (for health/diagnostics).
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Get a session by handle (for diagnostics/tests).
    pub fn session(&self, handle: SessionHandle) -> Option<&Session> {
        self.sessions.get(&handle)
    }

    /// Mutable access to a session by handle (for in-place updates).
    pub fn session_mut(&mut self, handle: SessionHandle) -> Option<&mut Session> {
        self.sessions.get_mut(&handle)
    }

    /// Persist the registry state (sessions, channel bindings, leases,
    /// operation receipts) to a JSON file so a daemon restart restores
    /// durable history. Atomic tmp+rename plus file + directory
    /// synchronization: a crash or power loss mid-write leaves either the
    /// previous file intact or the new file durable — never a torn
    /// sessions.json that fails the next load. (File sync is durability,
    /// not atomicity with Fjall state: cross-store operations still need
    /// their own idempotency markers, see OpRecord::effects.)
    pub fn persist(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let snapshot = RegistrySnapshot {
            handle_gen: self.handle_gen.load(Ordering::SeqCst),
            channels: self
                .channels
                .iter()
                .map(|((fe, ch), b)| ChannelEntry {
                    frontend_id: *fe,
                    channel_id: *ch,
                    binding: b.clone(),
                })
                .collect(),
            sessions: self
                .sessions
                .iter()
                .map(|(h, s)| SessionEntry {
                    handle: *h,
                    session: s.clone(),
                })
                .collect(),
            op_log: self.op_log.clone(),
        };
        let json = serde_json::to_vec_pretty(&snapshot)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        // Durability barrier (re-review R1): flush file data before the
        // rename makes it visible, then flush the directory entry.
        let f = std::fs::File::open(&tmp)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Load the registry state from a JSON file. A missing file yields an
    /// empty registry (first run).
    pub fn load(path: &std::path::Path) -> Result<Self, std::io::Error> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let bytes = std::fs::read(path)?;
        let snapshot: RegistrySnapshot =
            serde_json::from_slice(&bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut channels = HashMap::new();
        for e in snapshot.channels {
            channels.insert((e.frontend_id, e.channel_id), e.binding);
        }
        let mut sessions = HashMap::new();
        for e in snapshot.sessions {
            sessions.insert(e.handle, e.session);
        }
        Ok(Self {
            channels,
            sessions,
            handle_gen: AtomicU64::new(snapshot.handle_gen),
            op_log: snapshot.op_log,
            // Live connections never persist: a fresh process starts at zero.
            live: AtomicUsize::new(0),
        })
    }
}

/// A serializable snapshot of the registry for persistence across restarts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RegistrySnapshot {
    handle_gen: u64,
    channels: Vec<ChannelEntry>,
    sessions: Vec<SessionEntry>,
    /// Durable operation receipts (re-review R5). Old snapshots without
    /// this field load with an empty log (replays then re-execute once —
    /// attempt IDs and guide op logs still dedup the effects).
    #[serde(default)]
    op_log: HashMap<String, OpRecord>,
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

    fn attempt(id: u64, session: SessionHandle) -> Attempt {
        Attempt {
            id: crate::domain::id::EntityId::new(Uuid::from_u128(id as u128)),
            session_id: session,
            seq: 1,
            approach: "try X".into(),
            outcome: crate::domain::session::AttemptOutcome::Rejected,
            critique: None,
            rationale: None,
            related_memory_id: None,
            confidence: 1.0,
            access_count: 0,
            last_accessed_at: None,
            created_at: Instant::new(0),
        }
    }

    #[test]
    fn one_channels_session_end_cannot_end_another() {
        let mut reg = FrontendRegistry::new();
        // Two channels, same MCP numeric request ID would be reused — but we
        // route by (frontend, channel), not by request ID.
        let h_a = reg.start_session(fe(1), ch(1), None, None, 0);
        let h_b = reg.start_session(fe(1), ch(2), None, None, 0);
        assert_ne!(h_a, h_b);

        // End channel 1's session.
        let ended = reg
            .end_session(fe(1), ch(1), TaskOutcome::Success, None, vec![], 100)
            .unwrap();
        assert_eq!(ended, h_a);

        // Channel 2's session must still be active.
        assert_eq!(reg.resolve_session(fe(1), ch(2)), Some(h_b));
        // Channel 1's session is now terminal.
        assert_eq!(reg.resolve_session(fe(1), ch(1)), None);
    }

    #[test]
    fn thirty_two_channels_are_isolated() {
        let mut reg = FrontendRegistry::new();
        let mut handles = Vec::new();
        for i in 0..32 {
            let h = reg.start_session(fe(i as u64), ch(i as u64), None, None, 0);
            handles.push(h);
        }
        // All distinct.
        let unique = handles.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 32, "32 channels => 32 distinct sessions");

        // Record an attempt on each; each lands on its own session.
        for i in 0..32 {
            let h = handles[i as usize];
            reg.record_attempt(fe(i as u64), ch(i as u64), attempt(i as u64, h));
        }
        // End only channel 5; others remain active.
        reg.end_session(fe(5), ch(5), TaskOutcome::Failure, None, vec![], 100);
        assert_eq!(reg.resolve_session(fe(5), ch(5)), None);
        for i in 0..32 {
            if i != 5 {
                assert_eq!(
                    reg.resolve_session(fe(i as u64), ch(i as u64)),
                    Some(handles[i as usize])
                );
            }
        }
    }

    #[test]
    fn no_daemon_global_current_session() {
        // Starting a session on one channel must not be visible as the
        // "current" session of another channel.
        let mut reg = FrontendRegistry::new();
        let h = reg.start_session(fe(1), ch(1), None, None, 0);
        // A brand-new channel has no session.
        assert_eq!(reg.resolve_session(fe(2), ch(99)), None);
        // And the started handle is only bound to its own channel.
        assert_eq!(reg.resolve_session(fe(1), ch(1)), Some(h));
        assert_eq!(reg.resolve_session(fe(1), ch(2)), None);
    }

    #[test]
    fn lease_expiry_abandons_only_expired_channels() {
        let mut reg = FrontendRegistry::new();
        let h_a = reg.start_session(fe(1), ch(1), None, None, 0);
        let h_b = reg.start_session(fe(1), ch(2), None, None, 0);
        // Lease A expires at 100, B at 1000.
        reg.set_lease(fe(1), ch(1), Lease { expires_at: 100 });
        reg.set_lease(fe(1), ch(2), Lease { expires_at: 1000 });

        let abandoned = reg.expire_leases(500);
        assert_eq!(abandoned, vec![h_a]);
        // A abandoned, B still active.
        assert_eq!(reg.resolve_session(fe(1), ch(1)), None);
        assert_eq!(reg.resolve_session(fe(1), ch(2)), Some(h_b));
    }

    #[test]
    fn native_explicit_binding_is_tracked() {
        let mut reg = FrontendRegistry::new();
        let native = SessionHandle::new(Uuid::from_u128(999));
        reg.bind_native_session(fe(1), ch(1), native);
        assert!(reg.is_explicit(fe(1), ch(1)));
        assert_eq!(reg.resolve_session(fe(1), ch(1)), Some(native));
    }

    #[test]
    fn sessions_persist_and_restore_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        // Start sessions, record attempts, persist.
        let mut reg = FrontendRegistry::new();
        let h_a = reg.start_session(fe(1), ch(1), Some("proj".into()), Some("debug".into()), 0);
        let h_b = reg.start_session(fe(1), ch(2), None, None, 0);
        reg.record_attempt(fe(1), ch(1), attempt(1, h_a));
        reg.set_lease(fe(1), ch(1), Lease { expires_at: 5000 });
        reg.persist(&path).unwrap();

        // Simulate restart: load into a fresh registry.
        let mut reg2 = FrontendRegistry::load(&path).unwrap();
        // Active sessions restored.
        assert_eq!(reg2.resolve_session(fe(1), ch(1)), Some(h_a));
        assert_eq!(reg2.resolve_session(fe(1), ch(2)), Some(h_b));
        // Attempts and lease restored.
        assert_eq!(reg2.channel_count(), 2);
        // Live connections never persist: a fresh process starts at zero
        // even with channel history on disk (restore readiness counts
        // live connections, never this history).
        assert_eq!(reg2.live_connection_count(), 0);
        reg2.note_live_connect();
        reg2.note_live_connect();
        assert_eq!(reg2.live_connection_count(), 2);
        reg2.note_live_disconnect();
        assert_eq!(reg2.live_connection_count(), 1);
        // The restored session for ch(1) has its recorded attempt.
        let s = reg2.session(h_a).unwrap();
        assert_eq!(s.attempts.len(), 1);
        assert_eq!(s.project.as_deref(), Some("proj"));
        // The lease is restored so expiry still applies.
        let abandoned = reg2.expire_leases(6000);
        assert_eq!(abandoned, vec![h_a]);
    }

    #[test]
    fn reconnect_restores_only_verified_channel_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        let mut reg = FrontendRegistry::new();
        let h_a = reg.start_session(fe(1), ch(1), None, None, 0);
        reg.persist(&path).unwrap();

        // A reconnecting frontend presents (frontend_id, channel_id). The
        // daemon restores the binding only if it matches a persisted one.
        let reg2 = FrontendRegistry::load(&path).unwrap();
        // Verified: the persisted channel is found.
        assert_eq!(reg2.resolve_session(fe(1), ch(1)), Some(h_a));
        // Not verified: an unknown channel has no session (never guessed).
        assert_eq!(reg2.resolve_session(fe(1), ch(99)), None);
        // Not verified: a different frontend cannot claim the session.
        assert_eq!(reg2.resolve_session(fe(2), ch(1)), None);
    }

    #[test]
    fn load_missing_file_yields_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let reg = FrontendRegistry::load(&path).unwrap();
        assert_eq!(reg.channel_count(), 0);
        assert_eq!(reg.session_count(), 0);
    }

    /// Virtual sessions are stable per channel, isolated across channels,
    /// and never shadow a traced session.
    #[test]
    fn virtual_session_stable_per_channel_and_isolated() {
        let mut reg = FrontendRegistry::new();
        let v1 = reg.ensure_virtual_session(fe(1), ch(1), 0);
        assert!(reg.session(v1).unwrap().is_virtual);
        // Same channel, still live: same handle.
        assert_eq!(reg.ensure_virtual_session(fe(1), ch(1), 1_000), v1);
        // Another channel gets its own virtual session.
        let v2 = reg.ensure_virtual_session(fe(1), ch(2), 1_000);
        assert_ne!(v1, v2);
        // Traced sessions shadow virtual ones; resolve_session is untouched.
        let traced = reg.start_legacy_session(fe(1), ch(1), "debugging".to_string(), vec![], 2_000);
        assert!(!reg.session(traced).unwrap().is_virtual);
        assert_eq!(reg.ensure_virtual_session(fe(1), ch(1), 2_000), traced);
        assert_eq!(reg.resolve_session(fe(1), ch(1)), Some(traced));
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
        assert!(reg.session(v1).unwrap().status.is_terminal());
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

    /// Re-review R5: the operation log distinguishes fresh, replay and
    /// conflicting-key reuse, and effect markers are idempotent.
    #[test]
    fn op_log_fresh_replay_conflict_and_effects() {
        let mut reg = FrontendRegistry::new();
        let h = reg.start_session(fe(1), ch(1), None, None, 0);
        assert!(matches!(reg.check_op("op-1", "d1"), OpCheck::Fresh));
        reg.record_op("op-1", "d1", h, Some("done".into()), None);
        match reg.check_op("op-1", "d1") {
            OpCheck::Replay(rec) => {
                assert_eq!(rec.session, h);
                assert_eq!(rec.text.as_deref(), Some("done"));
            }
            other => panic!("expected replay, got: {other:?}"),
        }
        assert!(matches!(
            reg.check_op("op-1", "DIFFERENT"),
            OpCheck::Conflict
        ));
        assert!(!reg.effect_applied("op-1", "e1"));
        reg.mark_effect_applied("op-1", "e1");
        reg.mark_effect_applied("op-1", "e1");
        assert!(reg.effect_applied("op-1", "e1"));
        // Unknown operations have no effects.
        assert!(!reg.effect_applied("op-unknown", "e1"));
    }

    /// Re-review R1/R5: the op log persists across restarts (replays stay
    /// replays after a restart), and old snapshots without the log still
    /// load (empty log, no failure).
    #[test]
    fn op_log_persists_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut reg = FrontendRegistry::new();
        let h = reg.start_session(fe(1), ch(1), None, None, 0);
        reg.record_op("op-9", "digest-9", h, Some("text-9".into()), None);
        reg.persist(&path).unwrap();
        let reg2 = FrontendRegistry::load(&path).unwrap();
        match reg2.check_op("op-9", "digest-9") {
            OpCheck::Replay(rec) => {
                assert_eq!(rec.session, h);
                assert_eq!(rec.text.as_deref(), Some("text-9"));
            }
            other => panic!("receipt must survive restart, got: {other:?}"),
        }
        assert!(matches!(reg2.check_op("op-9", "other"), OpCheck::Conflict));
        // Hand-written old snapshot without op_log loads fine.
        let legacy = serde_json::json!({
            "handle_gen": 1u64,
            "channels": [],
            "sessions": [],
        });
        let legacy_path = dir.path().join("legacy.json");
        std::fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let reg3 = FrontendRegistry::load(&legacy_path).unwrap();
        assert!(matches!(reg3.check_op("anything", "d"), OpCheck::Fresh));
    }
}

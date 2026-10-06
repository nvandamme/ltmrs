//! Session, attempt, and feedback event types.

use crate::domain::id::{ChannelId, EntityId, SessionHandle};
use crate::domain::memory::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TaskOutcome {
    Success,
    Partial,
    Failure,
    Abandoned,
}

impl TaskOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskOutcome::Success => "success",
            TaskOutcome::Partial => "partial",
            TaskOutcome::Failure => "failure",
            TaskOutcome::Abandoned => "abandoned",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "success" => TaskOutcome::Success,
            "partial" => TaskOutcome::Partial,
            "failure" => TaskOutcome::Failure,
            "abandoned" => TaskOutcome::Abandoned,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AttemptOutcome {
    Rejected,
    Partial,
    Promising,
}

impl AttemptOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttemptOutcome::Rejected => "rejected",
            AttemptOutcome::Partial => "partial",
            AttemptOutcome::Promising => "promising",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "rejected" => AttemptOutcome::Rejected,
            "partial" => AttemptOutcome::Partial,
            "promising" => AttemptOutcome::Promising,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionStatus {
    Active,
    Ended,
    Abandoned,
}

impl SessionStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, SessionStatus::Ended | SessionStatus::Abandoned)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attempt {
    pub id: EntityId,
    pub session_id: SessionHandle,
    /// The 1-based sequence number of this attempt within its session.
    pub seq: u32,
    pub approach: String,
    pub outcome: AttemptOutcome,
    pub critique: Option<String>,
    pub rationale: Option<String>,
    pub related_memory_id: Option<EntityId>,
    /// Recall priority in [0,1]: decayed at each session start, boosted when
    /// recalled or when a derived suggestion is accepted, penalized when a
    /// derived suggestion is dismissed.
    pub confidence: f64,
    pub access_count: u32,
    pub last_accessed_at: Option<Instant>,
    pub created_at: Instant,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub handle: SessionHandle,
    pub channel_id: ChannelId,
    pub project: Option<String>,
    pub task_type: Option<String>,
    pub technologies: Vec<String>,
    pub status: SessionStatus,
    pub attempts: Vec<Attempt>,
    pub outcome: Option<TaskOutcome>,
    pub final_approach: Option<String>,
    pub lessons: Vec<String>,
    pub initial_approach: Option<String>,
    /// Lowercased guide names practiced during this session.
    pub guides_used: Vec<String>,
    /// Legacy memory IDs read during this session.
    pub memories_read: Vec<String>,
    /// Legacy memory IDs created during this session.
    pub memories_created: Vec<String>,
    pub refinement_attempts: u32,
    pub self_critique_count: u32,
    pub started_at: Instant,
    pub ended_at: Option<Instant>,
    /// True for implicit per-channel virtual sessions (session-less calls);
    /// false for traced sessions. Defaults false so pre-virtual records
    /// decode as traced, which is what they were.
    #[serde(default)]
    pub is_virtual: bool,
}

impl Session {
    pub fn can_end(&self) -> bool {
        !self.status.is_terminal()
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FeedbackEvent {
    pub id: EntityId,
    pub memory_id: EntityId,
    pub useful: bool,
    pub timestamp: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SuggestionStatus {
    Pending,
    Accepted,
    Dismissed,
}

impl SuggestionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SuggestionStatus::Pending => "pending",
            SuggestionStatus::Accepted => "accepted",
            SuggestionStatus::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => SuggestionStatus::Pending,
            "accepted" => SuggestionStatus::Accepted,
            "dismissed" => SuggestionStatus::Dismissed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Suggestion {
    pub id: u64,
    pub session_id: Option<String>,
    pub suggestion: String,
    pub status: SuggestionStatus,
    pub created_at: Instant,
    pub resolved_at: Option<Instant>,
}

/// A durable receipt for one completed session operation, stored in the
/// canonical store alongside the session it acted on (never in a sidecar
/// file): same operation ID + digest replays the recorded outcome, the same
/// ID with a different digest rejects as key reuse. Attempts additionally
/// record their sequence number so replays rebuild responses without
/// touching session state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionReceipt {
    /// The request digest the operation first executed with.
    pub digest: String,
    /// The session this operation acted on.
    pub session: SessionHandle,
    /// The attempt sequence number, for attempt operations only.
    #[serde(default)]
    pub seq: Option<u32>,
}

/// The outcome of claiming a session operation inside its transaction:
/// either it executes (or replays) to a usable result, or it rejects.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionOp<T> {
    /// Fresh execution completed.
    Applied(T),
    /// Same ID + digest seen before: the recorded result, no re-execution.
    Replayed(T),
    /// Same ID, different digest: reject, never execute.
    Conflict,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_roundtrip() {
        assert_eq!(TaskOutcome::parse("success"), Some(TaskOutcome::Success));
        assert_eq!(TaskOutcome::parse("nope"), None);
        assert_eq!(
            AttemptOutcome::parse("rejected"),
            Some(AttemptOutcome::Rejected)
        );
        assert_eq!(SuggestionStatus::Pending.as_str(), "pending");
        for v in [
            TaskOutcome::Success,
            TaskOutcome::Partial,
            TaskOutcome::Failure,
            TaskOutcome::Abandoned,
        ] {
            assert_eq!(TaskOutcome::parse(v.as_str()), Some(v));
        }
    }
}

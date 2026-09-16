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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AttemptOutcome {
    Rejected,
    Partial,
    Promising,
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
    pub approach: String,
    pub outcome: AttemptOutcome,
    pub critique: Option<String>,
    pub rationale: Option<String>,
    pub related_memory_id: Option<EntityId>,
    pub created_at: Instant,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub handle: SessionHandle,
    pub channel_id: ChannelId,
    pub project: Option<String>,
    pub task_type: Option<String>,
    pub status: SessionStatus,
    pub attempts: Vec<Attempt>,
    pub outcome: Option<TaskOutcome>,
    pub final_approach: Option<String>,
    pub lessons: Vec<String>,
    pub started_at: Instant,
    pub ended_at: Option<Instant>,
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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Suggestion {
    pub id: u64,
    pub memory_id: EntityId,
    pub suggestion: String,
    pub status: SuggestionStatus,
}

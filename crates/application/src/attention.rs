//! Bounded daemon-owned fleet attention read model.

use std::{future::Future, time::SystemTime};

use signalbox_domain::{SessionId, TurnId};

/// Maximum session summaries returned by one coherent fleet snapshot.
// numeric-bound: guard - prevents a growing fleet from projecting an unbounded snapshot response
const ATTENTION_SNAPSHOT_ITEM_CEILING: u16 = 32;
/// Maximum Unicode scalar values retained from blocked-goal need text.
// numeric-bound: guard - prevents one operator-authored goal need from carrying unbounded text into every summary
const ATTENTION_GOAL_SUMMARY_CHARACTER_CEILING: u16 = 128;
/// Maximum journal records consumed by one incremental follow read.
// numeric-bound: guard - prevents a change-journal backlog from driving an unbounded follow read and replacement batch
const ATTENTION_CHANGE_ITEM_CEILING: u16 = 32;

/// Returns the hard safety ceiling for one coherent fleet snapshot.
#[must_use]
pub const fn max_attention_snapshot_items() -> u16 {
    ATTENTION_SNAPSHOT_ITEM_CEILING
}

/// Returns the hard safety ceiling for one blocked-goal summary.
#[must_use]
pub const fn max_attention_goal_summary_characters() -> u16 {
    ATTENTION_GOAL_SUMMARY_CHARACTER_CEILING
}

/// Returns the hard safety ceiling for one incremental follow read.
#[must_use]
pub const fn max_attention_change_items() -> u16 {
    ATTENTION_CHANGE_ITEM_CEILING
}

/// Durable change-journal position; zero names the empty frontier.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AttentionCursor(u64);

impl AttentionCursor {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Closed current operator classification for one session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionState {
    Active,
    Queued,
    Blocked,
    AwaitingApproval,
    Ambiguous,
    AwaitingToolRecovery,
    AwaitingReconciliation,
    RunnerLost,
    Idle,
}

/// Exact operator action owed by the current facts, when any.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionAction {
    ProvideGoalNeed,
    DecideApproval,
    ReconcileTurn,
}

/// Typed blocked-goal reason retained without parsing goal prose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionBlockedReason {
    UserInputRequired,
    ExternalChangeRequired,
    AuthorizationRequired,
    ExecutionFailure,
}

/// Current blocked-goal evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionGoalBlock {
    pub generation: u64,
    pub reason: AttentionBlockedReason,
    /// Bounded summary; the session detail read retains the exact need text.
    pub need_summary: String,
}

/// Aggregate approval-judge outcomes for one session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttentionJudgeFacts {
    pub actionable: u64,
    pub completed: u64,
    pub escalated: u64,
    pub failed: u64,
}

/// Timestamped durable fact that last changed the summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttentionActivity {
    pub recorded_at: SystemTime,
    pub kind: AttentionActivityKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionActivityKind {
    Session,
    Turn,
    Goal,
    ApprovalJudge,
    Runner,
}

/// One lightweight browser-independent session summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionSummary {
    pub session: SessionId,
    pub current_turn: Option<TurnId>,
    pub state: AttentionState,
    pub action: Option<AttentionAction>,
    pub goal_block: Option<AttentionGoalBlock>,
    pub judge: AttentionJudgeFacts,
    pub last_activity: AttentionActivity,
}

/// One coherent fleet projection and its durable follow cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionSnapshot {
    pub cursor: AttentionCursor,
    pub summaries: Vec<AttentionSummary>,
    pub continuation_after: Option<SessionId>,
}

/// Bounded incremental projection above a known cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttentionChanges {
    Updated {
        cursor: AttentionCursor,
        summaries: Vec<AttentionSummary>,
    },
    ResyncRequired {
        cursor: AttentionCursor,
    },
}

/// Read-side infrastructure or durable-integrity failure.
pub trait AttentionReader {
    type Error;

    fn snapshot(
        &self,
        after: Option<SessionId>,
    ) -> impl Future<Output = Result<AttentionSnapshot, Self::Error>> + Send;

    fn changes_after(
        &self,
        cursor: AttentionCursor,
    ) -> impl Future<Output = Result<AttentionChanges, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_preserves_empty_frontier() {
        assert_eq!(AttentionCursor::new(0).value(), 0);
    }

    #[test]
    fn fleet_snapshot_bound_is_pinned() {
        assert_eq!(max_attention_snapshot_items(), 32);
    }

    #[test]
    fn goal_summary_bound_is_pinned() {
        assert_eq!(max_attention_goal_summary_characters(), 128);
    }

    #[test]
    fn change_batch_bound_is_pinned() {
        assert_eq!(max_attention_change_items(), 32);
    }
}

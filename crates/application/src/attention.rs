//! Bounded daemon-owned fleet attention read model.

use std::{collections::BTreeSet, fmt, future::Future, time::SystemTime};

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
/// Maximum Unicode scalar values carried from one session title.
// numeric-bound: guard - keeps the hot fleet page within its response byte bound
const ATTENTION_TITLE_CHARACTER_CEILING: u16 = 128;
/// Maximum exact tags accepted by one catalog filter.
// numeric-bound: guard - bounds query decoding and indexed tag predicates
const ATTENTION_FILTER_TAG_CEILING: u8 = 8;
/// Maximum UTF-8 bytes accepted across search text and exact tags.
// numeric-bound: guard - bounds one catalog query's decoded filter material
const ATTENTION_FILTER_UTF8_BYTE_CEILING: u16 = 1_024;

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

/// Returns the title-summary ceiling for one catalog row.
#[must_use]
pub const fn max_attention_title_characters() -> u16 {
    ATTENTION_TITLE_CHARACTER_CEILING
}

/// Returns the maximum exact tags in one catalog filter.
#[must_use]
pub const fn max_attention_filter_tags() -> u8 {
    ATTENTION_FILTER_TAG_CEILING
}

/// Returns the maximum filter material accepted by one catalog request.
#[must_use]
pub const fn max_attention_filter_utf8_bytes() -> u16 {
    ATTENTION_FILTER_UTF8_BYTE_CEILING
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

/// Stable server-owned catalog order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionSort {
    /// Most recently changed durable session first, identity as the tie-breaker.
    LastActivityDescending,
    /// Semantic session identity in ascending order.
    SessionIdentityAscending,
}

/// Exclusive keyset position in one exact catalog order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttentionContinuation {
    LastActivity {
        recorded_at: SystemTime,
        session: SessionId,
    },
    SessionIdentity(SessionId),
}

/// Validated bounded fleet/catalog query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionQuery {
    search: Option<String>,
    required_tags: BTreeSet<String>,
    include_archived: bool,
    sort: AttentionSort,
    continuation: Option<AttentionContinuation>,
}

impl AttentionQuery {
    /// Constructs the ordinary hot-session first page.
    #[must_use]
    pub fn hot_page() -> Self {
        Self {
            search: None,
            required_tags: BTreeSet::new(),
            include_archived: false,
            sort: AttentionSort::LastActivityDescending,
            continuation: None,
        }
    }

    /// Constructs one unfiltered identity-ordered attention page.
    #[must_use]
    pub fn identity_page(after: Option<SessionId>) -> Self {
        Self {
            search: None,
            required_tags: BTreeSet::new(),
            include_archived: false,
            sort: AttentionSort::SessionIdentityAscending,
            continuation: after.map(AttentionContinuation::SessionIdentity),
        }
    }

    /// Validates bounded search, exact tags, ordering, and keyset shape.
    pub fn try_new(
        search: Option<String>,
        required_tags: Vec<String>,
        include_archived: bool,
        sort: AttentionSort,
        continuation: Option<AttentionContinuation>,
    ) -> Result<Self, AttentionQueryError> {
        if required_tags.len() > usize::from(max_attention_filter_tags()) {
            return Err(AttentionQueryError::TooManyTags);
        }
        let mut filter_bytes = 0_usize;
        let mut tags = BTreeSet::new();
        for tag in required_tags {
            if tag.is_empty() || tag.contains('\0') {
                return Err(AttentionQueryError::InvalidTag);
            }
            filter_bytes = filter_bytes.saturating_add(tag.len());
            if !tags.insert(tag) {
                return Err(AttentionQueryError::DuplicateTag);
            }
        }
        if let Some(value) = search.as_deref() {
            if value.is_empty() || value.contains('\0') {
                return Err(AttentionQueryError::InvalidSearch);
            }
            filter_bytes = filter_bytes.saturating_add(value.len());
        }
        if filter_bytes > usize::from(max_attention_filter_utf8_bytes()) {
            return Err(AttentionQueryError::FilterTooLarge);
        }
        let continuation_matches = matches!(
            (&sort, &continuation),
            (_, None)
                | (
                    AttentionSort::LastActivityDescending,
                    Some(AttentionContinuation::LastActivity { .. })
                )
                | (
                    AttentionSort::SessionIdentityAscending,
                    Some(AttentionContinuation::SessionIdentity(_))
                )
        );
        if !continuation_matches {
            return Err(AttentionQueryError::ContinuationSortMismatch);
        }
        Ok(Self {
            search,
            required_tags: tags,
            include_archived,
            sort,
            continuation,
        })
    }

    pub fn search(&self) -> Option<&str> {
        self.search.as_deref()
    }

    pub fn required_tags(&self) -> impl ExactSizeIterator<Item = &str> {
        self.required_tags.iter().map(String::as_str)
    }

    pub const fn include_archived(&self) -> bool {
        self.include_archived
    }

    pub const fn sort(&self) -> AttentionSort {
        self.sort
    }

    pub const fn continuation(&self) -> Option<&AttentionContinuation> {
        self.continuation.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionQueryError {
    TooManyTags,
    InvalidTag,
    DuplicateTag,
    InvalidSearch,
    FilterTooLarge,
    ContinuationSortMismatch,
}

impl fmt::Display for AttentionQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("session catalog query is malformed or outside its hard bounds")
    }
}

impl std::error::Error for AttentionQueryError {}

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
    pub title_summary: Option<String>,
    pub title_truncated: bool,
    pub archived: bool,
    pub current_turn: Option<TurnId>,
    pub active_turn_count: u64,
    pub queued_turn_count: u64,
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
    pub total: u64,
    pub sort: AttentionSort,
    pub summaries: Vec<AttentionSummary>,
    pub continuation: Option<AttentionContinuation>,
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
        query: AttentionQuery,
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

    #[test]
    fn catalog_filter_bounds_are_pinned() {
        assert_eq!(max_attention_title_characters(), 128);
        assert_eq!(max_attention_filter_tags(), 8);
        assert_eq!(max_attention_filter_utf8_bytes(), 1_024);
    }

    #[test]
    fn continuation_must_match_the_selected_sort() {
        let continuation =
            AttentionContinuation::SessionIdentity(SessionId::from_uuid(uuid::Uuid::from_u128(7)));
        let error = AttentionQuery::try_new(
            None,
            Vec::new(),
            false,
            AttentionSort::LastActivityDescending,
            Some(continuation),
        )
        .expect_err("identity continuation cannot advance activity order");

        assert_eq!(error, AttentionQueryError::ContinuationSortMismatch);
    }
}

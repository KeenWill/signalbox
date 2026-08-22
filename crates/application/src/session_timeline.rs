//! Bounded historical session-timeline query orchestration.
//!
//! The address and window vocabulary is application-owned. Adapters preserve
//! it without exporting storage rows, while browser DTOs remain a separate
//! representation at the HTTP boundary.

use std::{fmt, future::Future, num::NonZeroU64};

use signalbox_domain::{BlobDigest, ModelCallId, ProviderModelIdentity, SessionId, TurnId};

/// Returns the hard ceiling on records in one historical window.
#[must_use]
pub const fn max_timeline_window_items() -> u16 {
    256
}

/// Returns the hard ceiling on projected structured bytes in one window.
#[must_use]
pub const fn max_timeline_window_bytes() -> u32 {
    64 * 1024
}

/// Returns the smallest accepted projected-byte budget.
#[must_use]
pub const fn min_timeline_window_bytes() -> u32 {
    256
}

/// Returns the hard ceiling on detailed records in one historical read.
#[must_use]
pub const fn max_timeline_detail_items() -> u16 {
    128
}

/// Returns the hard ceiling on projected body bytes in one detail response.
#[must_use]
pub const fn max_timeline_detail_bytes() -> u32 {
    64 * 1024
}

/// Returns the smallest accepted detail byte budget.
#[must_use]
pub const fn min_timeline_detail_bytes() -> u32 {
    256
}

/// Stable logical location of one durable session event.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TimelineAddress(NonZeroU64);

impl TimelineAddress {
    /// Constructs an address from the allocator-owned durable event sequence.
    #[must_use]
    pub const fn new(sequence: NonZeroU64) -> Self {
        Self(sequence)
    }

    /// Returns the durable event sequence carried by this address.
    #[must_use]
    pub const fn sequence(self) -> NonZeroU64 {
        self.0
    }
}

/// Logical place from which a bounded historical read starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineWindowAnchor {
    /// Earliest events in the session.
    First,
    /// Latest events in the session.
    Latest,
    /// Events immediately below this stable address.
    Before(TimelineAddress),
    /// Events immediately above this stable address.
    After(TimelineAddress),
    /// Events nearest this stable address, returned in logical order.
    Around(TimelineAddress),
}

/// Rejection of an invalid client-selected window ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineWindowLimitError {
    /// The item count is zero or exceeds the hard ceiling.
    Items,
    /// The byte budget is below the safe minimum or above the hard ceiling.
    Bytes,
}

impl fmt::Display for TimelineWindowLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Items => formatter.write_str("timeline item limit is outside its hard bounds"),
            Self::Bytes => formatter.write_str("timeline byte limit is outside its hard bounds"),
        }
    }
}

impl std::error::Error for TimelineWindowLimitError {}

/// Validated item and projected-byte ceilings for one window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineWindowLimits {
    max_items: u16,
    max_projected_bytes: u32,
}

impl TimelineWindowLimits {
    /// Validates client-selected effective limits against hard ceilings.
    pub const fn new(
        max_items: u16,
        max_projected_bytes: u32,
    ) -> Result<Self, TimelineWindowLimitError> {
        if max_items == 0 || max_items > max_timeline_window_items() {
            return Err(TimelineWindowLimitError::Items);
        }
        if max_projected_bytes < min_timeline_window_bytes()
            || max_projected_bytes > max_timeline_window_bytes()
        {
            return Err(TimelineWindowLimitError::Bytes);
        }
        Ok(Self {
            max_items,
            max_projected_bytes,
        })
    }

    /// Maximum number of returned items.
    #[must_use]
    pub const fn max_items(self) -> u16 {
        self.max_items
    }

    /// Maximum sum of returned items' projected structured bytes.
    #[must_use]
    pub const fn max_projected_bytes(self) -> u32 {
        self.max_projected_bytes
    }
}

/// Closed durable event categories exposed by the historical foundation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTimelineEventKind {
    /// A session was durably created.
    SessionCreated,
    /// Session-default model settings changed.
    SessionModelSettingsChanged,
    /// Effective model settings were resolved for a turn.
    TurnModelSettingsResolved,
    /// Input was durably accepted into the session.
    InputAccepted,
    /// A goal-bearing turn retired.
    GoalTurnRetired,
    /// A queued turn became active.
    TurnActivated,
    /// A turn reached a failed terminal state.
    TurnFailed,
    /// A model call changed durable state.
    ModelCallTransition,
    /// A tool batch changed durable state.
    ToolBatchTransition,
    /// A tool-approval decision was recorded.
    ToolApprovalDecided,
    /// Historical context was compacted.
    ContextCompacted,
    /// A turn completed successfully.
    TurnCompleted,
    /// A turn ended by refusal.
    TurnRefused,
    /// A turn ended by cancellation.
    TurnCancelled,
    /// A turn requires explicit reconciliation.
    TurnReconciliationRequired,
    /// Runner placement or execution state changed.
    RunnerStateTransition,
    /// Delegation state changed.
    DelegationUpdate,
    /// A delegation wake was recorded.
    DelegationWake,
}

/// One lightweight typed event header in a bounded historical window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTimelineItem {
    /// Immutable creation address of this durable event.
    pub address: TimelineAddress,
    /// Closed category of the durable event.
    pub kind: SessionTimelineEventKind,
    /// Bytes charged to this header's structured projection.
    pub projected_structured_bytes: u32,
}

/// Exact stable address bounds for the current durable history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionTimelineBounds {
    /// Earliest stable event address, absent only when no events exist.
    pub first: Option<TimelineAddress>,
    /// Latest stable event address, absent only when no events exist.
    pub latest: Option<TimelineAddress>,
}

/// Explicit policy inputs describing the lifetime history without loading it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionTimelineSizeFacts {
    /// Total durable event-header count.
    pub item_count: u64,
    /// Total bytes in projected human-readable text.
    pub projected_text_bytes: u64,
    /// Total bytes in projected structured event headers.
    pub projected_structured_bytes: u64,
    /// Number of referenced blobs without materializing them.
    pub referenced_blob_count: u64,
    /// Total bytes of referenced blobs without materializing them.
    pub referenced_blob_bytes: u64,
}

/// Current and queued work counts needed to interpret a historical tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionWorkFacts {
    /// Number of currently active turns.
    pub active_turn_count: u64,
    /// Number of turns queued behind active work.
    pub queued_turn_count: u64,
}

/// Lightweight authoritative description of one session read projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTimelineDescriptor {
    /// Session described by these facts.
    pub session: SessionId,
    /// Lifetime size facts maintained for bounded policy decisions.
    pub sizes: SessionTimelineSizeFacts,
    /// Earliest and latest stable addresses.
    pub bounds: SessionTimelineBounds,
    /// Current active and queued work facts.
    pub work: SessionWorkFacts,
    /// Durable global observation cursor covered by these facts.
    pub observed_through: u64,
}

/// Closed continuation state carrying the boundary needed for another read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineContinuation {
    /// The loaded window reaches the corresponding end of history.
    Exhausted,
    /// More history exists beyond the returned boundary item.
    MoreAt(TimelineAddress),
}

/// One bounded, logically ordered historical response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTimelineWindow {
    /// Session whose events were loaded.
    pub session: SessionId,
    /// Strictly ordered durable event headers in this window.
    pub items: Vec<SessionTimelineItem>,
    /// Sum of structured projection bytes for returned items.
    pub projected_structured_bytes: u32,
    /// Continuation state toward earlier addresses.
    pub continuation_before: TimelineContinuation,
    /// Continuation state toward later addresses.
    pub continuation_after: TimelineContinuation,
}

/// Rejection of an invalid selected detail ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineDetailLimitError {
    /// The item count is zero or exceeds the hard ceiling.
    Items,
    /// The byte budget is below the safe minimum or above the hard ceiling.
    Bytes,
}

impl fmt::Display for TimelineDetailLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Items => {
                formatter.write_str("timeline detail item limit is outside its hard bounds")
            }
            Self::Bytes => {
                formatter.write_str("timeline detail byte limit is outside its hard bounds")
            }
        }
    }
}

impl std::error::Error for TimelineDetailLimitError {}

/// Validated item and projected-body byte ceilings for one detail read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineDetailLimits {
    max_items: u16,
    max_projected_bytes: u32,
}

impl TimelineDetailLimits {
    /// Validates client-selected effective limits against hard ceilings.
    pub const fn new(
        max_items: u16,
        max_projected_bytes: u32,
    ) -> Result<Self, TimelineDetailLimitError> {
        if max_items == 0 || max_items > max_timeline_detail_items() {
            return Err(TimelineDetailLimitError::Items);
        }
        if max_projected_bytes < min_timeline_detail_bytes()
            || max_projected_bytes > max_timeline_detail_bytes()
        {
            return Err(TimelineDetailLimitError::Bytes);
        }
        Ok(Self {
            max_items,
            max_projected_bytes,
        })
    }

    /// Maximum number of returned detail records.
    #[must_use]
    pub const fn max_items(self) -> u16 {
        self.max_items
    }

    /// Maximum sum of returned projected body bytes.
    #[must_use]
    pub const fn max_projected_bytes(self) -> u32 {
        self.max_projected_bytes
    }
}

/// Text-bearing field within one typed detail body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineBodyField {
    /// Accepted user input text.
    InputText,
    /// Authoritative assistant response text.
    ModelResponse,
    /// One tool request's exact arguments.
    ToolArguments,
    /// One tool attempt's exact result.
    ToolResult,
    /// One tool attempt's exact failure detail.
    ToolFailure,
    /// One approval decision's rationale.
    ApprovalRationale,
    /// Goal statement, need, guidance, or report text.
    GoalText,
    /// Compaction summary text.
    CompactionSummary,
    /// Delegation message or result content.
    DelegationContent,
}

/// Exact continuation within a body too large for one selected byte budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineBodyContinuation {
    /// Stable item address whose body continues.
    pub address: TimelineAddress,
    /// Typed field continued within the body.
    pub field: TimelineBodyField,
    /// Zero-based member within a repeated body family such as tool attempts.
    pub member_index: u32,
    /// UTF-8 byte offset of the next excerpt.
    pub offset_bytes: u64,
}

/// Cursor accepted by item, turn, and region detail reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineDetailCursor {
    /// First address considered by the next read.
    pub address: TimelineAddress,
    /// Body field continued at that address, absent for the first field.
    pub field: Option<TimelineBodyField>,
    /// Zero-based repeated member selected by `field`.
    pub member_index: u32,
    /// UTF-8 byte offset within `field`.
    pub offset_bytes: u64,
}

/// Explicit next position after a bounded detail response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineDetailContinuation {
    /// The next response starts at another stable item address.
    MoreAt(TimelineAddress),
    /// The current typed body continues at an exact UTF-8 byte offset.
    MoreBody(TimelineBodyContinuation),
}

/// Bounded UTF-8 excerpt that never claims to be a complete body implicitly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineTextExcerpt {
    /// Selected UTF-8 content.
    pub text: String,
    /// UTF-8 byte offset of the excerpt's first byte.
    pub offset_bytes: u64,
    /// Total UTF-8 bytes in the authoritative field.
    pub total_bytes: u64,
    /// Exact next body position when this excerpt is incomplete.
    pub continuation: Option<TimelineBodyContinuation>,
}

/// Reference-only blob fact; bytes are never materialized by timeline detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineBlobReference {
    /// Stable blob identity.
    pub blob_id: BlobDigest,
    /// Exact referenced byte length.
    pub length_bytes: u64,
    /// Optional recorded media type.
    pub media_type: Option<String>,
}

/// Closed model-call lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineModelCallState {
    Prepared,
    InFlight,
    CancellationRequested,
    Terminal(TimelineModelCallDisposition),
}

/// Closed terminal model-call disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineModelCallDisposition {
    Completed,
    KnownFailed,
    Refused,
    Cancelled,
    Ambiguous,
}

/// Provider-reported usage, with independently optional counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

/// Closed turn lifecycle boundary shown in detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineTurnLifecycleKind {
    Activated,
    Terminalized,
}

/// Closed tool-execution lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineToolState {
    Prepared,
    InFlight,
    AwaitingChild,
    Completed,
    KnownFailed,
    Ambiguous,
}

/// One request and its optional attempt, projected one member at a time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineToolAttempt {
    pub request_id: String,
    pub attempt_id: Option<String>,
    pub tool_name: String,
    pub arguments: Option<TimelineTextExcerpt>,
    pub result: Option<TimelineTextExcerpt>,
    pub failure: Option<TimelineTextExcerpt>,
    pub approval_posture: String,
    pub approval_judge_escalated: bool,
    pub operator_required: bool,
    pub effect_posture: Option<String>,
    pub sandbox_posture: Option<String>,
    pub state: Option<TimelineToolState>,
    pub cause_code: Option<String>,
}

/// Closed tool-batch projection state exposed by timeline detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineToolBatchState {
    Proposed,
    ResultsProjected,
    RecoveryRequired,
}

/// Typed provenance of an approval decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineApprovalSource {
    Policy,
    Delegate,
    User,
}

/// Exact durable actor that decided an explicit approval request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TimelineApprovalDecider {
    User {
        command_id: String,
    },
    Delegate {
        model_selection_id: String,
        model_call_id: String,
    },
}

/// Closed runner sandbox posture exposed by timeline detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineRunnerSandboxPosture {
    Unsandboxed,
    Sandboxed,
}

/// Closed runner lifecycle state exposed by timeline detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineRunnerState {
    Pinned,
    Suspect,
    Connected,
    RunnerLostBeforePin,
    RunnerLost,
    Replaced,
    WorkingDirectoryChanged,
    Abandoned,
}

/// Closed goal-event kind exposed by timeline detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineGoalEventKind {
    Commissioned,
    Blocked,
    Resumed,
    Achieved,
    UserStopped,
    Superseded,
}

/// Closed reason carried by blocked goal events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineGoalBlockedReason {
    UserInputRequired,
    ExternalChangeRequired,
    AuthorizationRequired,
    ExecutionFailure,
}

/// Typed goal-lineage event attached to the timeline fact that caused it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineGoalEvent {
    pub generation: u64,
    pub event_kind: TimelineGoalEventKind,
    pub reason: Option<TimelineGoalBlockedReason>,
    pub text: Option<TimelineTextExcerpt>,
}

/// Closed action applied to a bound child after a parent terminal transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineBoundChildAction {
    KeepRunning,
    Stop,
    Cancel,
}

/// Parent-selected child relationship policy preserved in delegation detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineDelegationPolicy {
    Background,
    Bound {
        on_parent_stopped: TimelineBoundChildAction,
        on_parent_cancelled: TimelineBoundChildAction,
    },
}

/// Imported-frontier provenance; source bytes remain reference-only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineImportedEvidence {
    pub imported_entry_id: String,
    pub imported_position: u64,
}

/// Typed detail body, separate from storage, process-wire, and browser DTOs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTimelineDetailBody {
    /// Session creation provenance, including an imported frontier when present.
    SessionCreated {
        imported_evidence: Option<TimelineImportedEvidence>,
    },
    /// A model-settings projection changed at session or turn scope.
    ModelSettings {
        turn_id: Option<String>,
        cause_code: String,
    },
    /// Exact accepted user input and reference-only attachments.
    UserInput {
        turn_id: TurnId,
        text: TimelineTextExcerpt,
        attachments: Vec<TimelineBlobReference>,
    },
    /// One model-call checkpoint enriched with request, response, and usage facts.
    ModelCall {
        turn_id: TurnId,
        model_call_id: ModelCallId,
        state: TimelineModelCallState,
        model_identity_id: ProviderModelIdentity,
        request_context_items: u64,
        response: Option<TimelineTextExcerpt>,
        usage: TimelineModelUsage,
        cause_code: Option<String>,
    },
    /// A tool batch with one progressively selected request/attempt member.
    ToolBatch {
        turn_id: String,
        producing_model_call_id: String,
        state: TimelineToolBatchState,
        tools: Vec<TimelineToolAttempt>,
        goal_events: Vec<TimelineGoalEvent>,
    },
    /// One explicit tool approval decision and complete provenance.
    ToolApprovalDecision {
        turn_id: String,
        request_id: String,
        tool_name: String,
        decision: String,
        source: TimelineApprovalSource,
        decider: TimelineApprovalDecider,
        rationale: Option<TimelineTextExcerpt>,
        approval_judge_escalated: bool,
    },
    /// One typed goal-lineage transition.
    GoalEvent {
        turn_id: String,
        event: TimelineGoalEvent,
    },
    /// One append-only context compaction and its bounded summary.
    ContextCompaction {
        compaction_id: String,
        model_call_id: String,
        through_position: u64,
        summary_entry_id: String,
        result_frontier_id: String,
        summary: TimelineTextExcerpt,
    },
    /// Activated or terminalized turn boundary with a stable cause code.
    TurnLifecycle {
        turn_id: TurnId,
        lifecycle: TimelineTurnLifecycleKind,
        cause_code: String,
    },
    /// Automatic reconciliation facts that explain operator-required parking.
    Reconciliation {
        turn_id: String,
        operation_kind: String,
        operation_id: String,
        attempt_count: u64,
        exhausted: bool,
        operator_required: bool,
        cause_code: String,
    },
    /// Runner placement fact, including sandbox posture.
    Runner {
        runner_id: String,
        placement_revision: u64,
        sandbox_posture: TimelineRunnerSandboxPosture,
        working_directory: Option<String>,
        state: TimelineRunnerState,
    },
    /// Typed delegation update or wake with optional bounded delivered content.
    Delegation {
        event_kind: String,
        relationship_id: String,
        subject_id: Option<String>,
        policy: Option<TimelineDelegationPolicy>,
        outcome: Option<String>,
        reason: Option<String>,
        content: Option<TimelineTextExcerpt>,
    },
}

/// One detail record at the same stable address as its lightweight header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTimelineDetail {
    pub address: TimelineAddress,
    pub kind: SessionTimelineEventKind,
    pub body: SessionTimelineDetailBody,
    pub projected_body_bytes: u32,
}

/// One bounded item, turn, or contiguous-region detail response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTimelineDetailPage {
    pub session: SessionId,
    pub items: Vec<SessionTimelineDetail>,
    pub projected_body_bytes: u32,
    pub continuation: Option<TimelineDetailContinuation>,
}

/// Application-owned read port for the historical session projection.
pub trait SessionTimelineReader {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Reads explicit lifetime size, bounds, work, and cursor facts.
    fn read_descriptor(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Result<Option<SessionTimelineDescriptor>, Self::Error>> + Send;

    /// Reads one bounded window at a stable logical anchor.
    fn read_window(
        &self,
        session: SessionId,
        anchor: TimelineWindowAnchor,
        limits: TimelineWindowLimits,
    ) -> impl Future<Output = Result<Option<SessionTimelineWindow>, Self::Error>> + Send;

    /// Reads one selected item body under an explicit byte ceiling.
    fn read_item_details(
        &self,
        session: SessionId,
        address: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> impl Future<Output = Result<Option<SessionTimelineDetailPage>, Self::Error>> + Send;

    /// Reads details associated with one exact turn under item and byte ceilings.
    fn read_turn_details(
        &self,
        session: SessionId,
        turn: TurnId,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> impl Future<Output = Result<Option<SessionTimelineDetailPage>, Self::Error>> + Send;

    /// Reads one bounded contiguous inclusive address region.
    fn read_region_details(
        &self,
        session: SessionId,
        first: TimelineAddress,
        through: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> impl Future<Output = Result<Option<SessionTimelineDetailPage>, Self::Error>> + Send;
}

/// Coordinates bounded historical reads without choosing presentation modes.
#[derive(Debug)]
pub struct ReadSessionTimelineService<Reader> {
    reader: Reader,
}

impl<Reader> ReadSessionTimelineService<Reader> {
    /// Constructs a service around the application read port.
    #[must_use]
    pub const fn new(reader: Reader) -> Self {
        Self { reader }
    }
}

impl<Reader: SessionTimelineReader> ReadSessionTimelineService<Reader> {
    /// Reads the authoritative bounded descriptor for one session.
    pub async fn descriptor(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionTimelineDescriptor>, Reader::Error> {
        self.reader.read_descriptor(session).await
    }

    /// Reads one validated historical window for one session.
    pub async fn window(
        &self,
        session: SessionId,
        anchor: TimelineWindowAnchor,
        limits: TimelineWindowLimits,
    ) -> Result<Option<SessionTimelineWindow>, Reader::Error> {
        self.reader.read_window(session, anchor, limits).await
    }

    /// Reads one selected item without loading unrelated history.
    pub async fn item_details(
        &self,
        session: SessionId,
        address: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Reader::Error> {
        self.reader
            .read_item_details(session, address, cursor, limits)
            .await
    }

    /// Reads one bounded set of events associated with an exact turn.
    pub async fn turn_details(
        &self,
        session: SessionId,
        turn: TurnId,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Reader::Error> {
        self.reader
            .read_turn_details(session, turn, cursor, limits)
            .await
    }

    /// Reads one bounded inclusive region in stable address order.
    pub async fn region_details(
        &self,
        session: SessionId,
        first: TimelineAddress,
        through: TimelineAddress,
        cursor: Option<TimelineDetailCursor>,
        limits: TimelineDetailLimits,
    ) -> Result<Option<SessionTimelineDetailPage>, Reader::Error> {
        self.reader
            .read_region_details(session, first, through, cursor, limits)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_limits_reject_unbounded_requests() {
        assert_eq!(
            TimelineWindowLimits::new(max_timeline_window_items() + 1, 1024),
            Err(TimelineWindowLimitError::Items)
        );
        assert_eq!(
            TimelineWindowLimits::new(1, max_timeline_window_bytes() + 1),
            Err(TimelineWindowLimitError::Bytes)
        );
    }

    #[test]
    fn window_limits_preserve_client_selected_bounds() {
        let selected_item_limit = 37;
        let selected_byte_limit = 4096;
        let limits = TimelineWindowLimits::new(selected_item_limit, selected_byte_limit)
            .expect("fixture limits are bounded");

        assert_eq!(limits.max_items(), selected_item_limit);
        assert_eq!(limits.max_projected_bytes(), selected_byte_limit);
    }

    #[test]
    fn detail_limits_reject_unbounded_requests() {
        assert_eq!(
            TimelineDetailLimits::new(max_timeline_detail_items() + 1, 1024),
            Err(TimelineDetailLimitError::Items)
        );
        assert_eq!(
            TimelineDetailLimits::new(1, max_timeline_detail_bytes() + 1),
            Err(TimelineDetailLimitError::Bytes)
        );
    }
}

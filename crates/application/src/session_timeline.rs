//! Bounded historical session-timeline query orchestration.
//!
//! The address and window vocabulary is application-owned. Adapters preserve
//! it without exporting storage rows, while browser DTOs remain a separate
//! representation at the HTTP boundary.

use std::{fmt, future::Future, num::NonZeroU64};

use signalbox_domain::SessionId;

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
}

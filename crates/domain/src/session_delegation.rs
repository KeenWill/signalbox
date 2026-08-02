//! Typed delegated-session relation, messages, outcomes, and wait subject.

use std::num::NonZeroU64;

use crate::{
    DelegationMessageId, DurableCommandId, NonEmptyUnicodeText, SessionCreationProvenance,
    SessionId, ToolRequest, ToolRequestId, TurnId,
};

/// Shared admission bound for delegated messages and returned results.
pub const MAX_DELEGATION_CONTENT_UTF8_BYTES: usize = 1_048_576;

/// Action applied to a bound child when its parent terminalizes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BoundChildAction {
    KeepRunning,
    Stop,
    Cancel,
}

/// Durable relationship policy fixed when the child is spawned.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChildRelationshipPolicy {
    Background,
    Bound {
        on_parent_stopped: BoundChildAction,
        on_parent_cancelled: BoundChildAction,
    },
}

/// Whether awaiting a child retains the parent turn slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationWaitMode {
    Foreground,
    Background,
}

/// Explicit scope of a parent stop or cancellation command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DescendantTerminationScope {
    ParentAlone,
    ParentAndDescendants,
}

/// Exact bounded nonempty content delivered across a delegation relation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationContent(NonEmptyUnicodeText);

impl DelegationContent {
    pub fn try_new(value: String) -> Result<Self, DelegationContentError> {
        if value.len() > MAX_DELEGATION_CONTENT_UTF8_BYTES {
            return Err(DelegationContentError::Oversized {
                utf8_byte_length: value.len(),
            });
        }
        NonEmptyUnicodeText::try_new(value)
            .map(Self)
            .map_err(|error| DelegationContentError::Invalid(error.failure()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Why delegated content was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationContentError {
    Invalid(crate::NonEmptyUnicodeTextFailure),
    Oversized { utf8_byte_length: usize },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DelegationProvenanceKind {
    ToolRequest {
        source_session: SessionId,
        source_turn: TurnId,
        request: ToolRequestId,
    },
    ChildTurn {
        child: SessionId,
        turn: TurnId,
    },
    ParentCommand {
        parent: SessionId,
        command: DurableCommandId,
    },
}

/// Exact typed authority for a delegation event.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DelegationProvenance {
    kind: DelegationProvenanceKind,
}

impl DelegationProvenance {
    /// Seals request identity together with its canonical owning session/turn.
    pub fn from_tool_request(request: &ToolRequest) -> Self {
        Self {
            kind: DelegationProvenanceKind::ToolRequest {
                source_session: request.session(),
                source_turn: request.turn(),
                request: request.id(),
            },
        }
    }

    pub const fn from_child_turn(child: SessionId, turn: TurnId) -> Self {
        Self {
            kind: DelegationProvenanceKind::ChildTurn { child, turn },
        }
    }

    pub const fn from_parent_command(parent: SessionId, command: DurableCommandId) -> Self {
        Self {
            kind: DelegationProvenanceKind::ParentCommand { parent, command },
        }
    }

    pub const fn tool_request(&self) -> Option<(SessionId, TurnId, ToolRequestId)> {
        match self.kind {
            DelegationProvenanceKind::ToolRequest {
                source_session,
                source_turn,
                request,
            } => Some((source_session, source_turn, request)),
            DelegationProvenanceKind::ChildTurn { .. }
            | DelegationProvenanceKind::ParentCommand { .. } => None,
        }
    }

    pub const fn child_turn(&self) -> Option<(SessionId, TurnId)> {
        match self.kind {
            DelegationProvenanceKind::ChildTurn { child, turn } => Some((child, turn)),
            DelegationProvenanceKind::ToolRequest { .. }
            | DelegationProvenanceKind::ParentCommand { .. } => None,
        }
    }

    pub const fn parent_command(&self) -> Option<(SessionId, DurableCommandId)> {
        match self.kind {
            DelegationProvenanceKind::ParentCommand { parent, command } => Some((parent, command)),
            DelegationProvenanceKind::ToolRequest { .. }
            | DelegationProvenanceKind::ChildTurn { .. } => None,
        }
    }
}

/// Direction derived from a message's exact sender in the parent/child pair.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationMessageDirection {
    ParentToChild,
    ChildToParent,
}

/// One immutable bidirectional message whose content is authoritative.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationMessage {
    id: DelegationMessageId,
    direction: DelegationMessageDirection,
    content: DelegationContent,
    provenance: DelegationProvenance,
}

impl DelegationMessage {
    pub const fn id(&self) -> DelegationMessageId {
        self.id
    }
    pub const fn direction(&self) -> DelegationMessageDirection {
        self.direction
    }
    pub const fn content(&self) -> &DelegationContent {
        &self.content
    }
    pub const fn provenance(&self) -> DelegationProvenance {
        self.provenance
    }
}

/// Closed reason vocabulary for relation outcomes and no-change evaluations.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationOutcomeReason {
    ChildCompleted,
    ChildExecutionFailed,
    ParentStopped { scope: DescendantTerminationScope },
    ParentCancelled { scope: DescendantTerminationScope },
}

/// Every child disposition, including an explicit no-change evaluation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum DelegationOutcome {
    ResultReturned {
        content: DelegationContent,
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
    ChildFailed {
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
    ChildStopped {
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
    ChildCancelled {
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
    ContinueRunning {
        reason: DelegationOutcomeReason,
        provenance: DelegationProvenance,
    },
}

/// One positive contiguous event position per delegation relation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DelegationEventOrdinal(NonZeroU64);

impl DelegationEventOrdinal {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u64 {
        self.0.get()
    }
    const fn first() -> Self {
        Self(NonZeroU64::MIN)
    }
    fn successor(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// One immutable event in a relation's ordered history.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum DelegationEvent {
    Spawned {
        ordinal: DelegationEventOrdinal,
        provenance: DelegationProvenance,
    },
    MessageDelivered {
        ordinal: DelegationEventOrdinal,
        message: DelegationMessage,
    },
    OutcomeRecorded {
        ordinal: DelegationEventOrdinal,
        outcome: DelegationOutcome,
    },
}

impl DelegationEvent {
    pub const fn ordinal(&self) -> DelegationEventOrdinal {
        event_ordinal(self)
    }

    /// Borrows the delivered message when this is a message event.
    pub const fn message(&self) -> Option<&DelegationMessage> {
        match self {
            Self::MessageDelivered { message, .. } => Some(message),
            Self::Spawned { .. } | Self::OutcomeRecorded { .. } => None,
        }
    }

    pub const fn outcome(&self) -> Option<&DelegationOutcome> {
        match self {
            Self::OutcomeRecorded { outcome, .. } => Some(outcome),
            Self::Spawned { .. } | Self::MessageDelivered { .. } => None,
        }
    }
}

/// Scheduling-visible relation lifecycle.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegationLifecycle {
    Active,
    Terminal,
}

/// Exact subject retained by a foreground parent turn wait.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ChildWait {
    awaiting_request: ToolRequestId,
    spawning_request: ToolRequestId,
    child: SessionId,
}

impl ChildWait {
    pub const fn awaiting_request(self) -> ToolRequestId {
        self.awaiting_request
    }
    pub const fn spawning_request(self) -> ToolRequestId {
        self.spawning_request
    }
    pub const fn child(self) -> SessionId {
        self.child
    }
}

/// One exact parent delivery registration made by an await tool request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DelegationWait {
    awaiting_request: ToolRequestId,
    spawning_request: ToolRequestId,
    parent: SessionId,
    child: SessionId,
    mode: DelegationWaitMode,
}

impl DelegationWait {
    pub const fn awaiting_request(self) -> ToolRequestId {
        self.awaiting_request
    }
    pub const fn spawning_request(self) -> ToolRequestId {
        self.spawning_request
    }
    pub const fn parent(self) -> SessionId {
        self.parent
    }
    pub const fn child(self) -> SessionId {
        self.child
    }
    pub const fn mode(self) -> DelegationWaitMode {
        self.mode
    }
    pub const fn foreground_subject(self) -> Option<ChildWait> {
        match self.mode {
            DelegationWaitMode::Foreground => Some(ChildWait {
                awaiting_request: self.awaiting_request,
                spawning_request: self.spawning_request,
                child: self.child,
            }),
            DelegationWaitMode::Background => None,
        }
    }
}

/// One exact parent/child relationship keyed by its spawning request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegation {
    spawning_request: ToolRequestId,
    parent: SessionId,
    child: SessionId,
    policy: ChildRelationshipPolicy,
    lifecycle: DelegationLifecycle,
    events: Vec<DelegationEvent>,
}

impl SessionDelegation {
    pub fn spawn(
        spawning_request: &ToolRequest,
        child: SessionId,
        policy: ChildRelationshipPolicy,
    ) -> Result<Self, DelegationTransitionError> {
        let parent = spawning_request.session();
        if parent == child {
            return Err(DelegationTransitionError {
                failure: DelegationTransitionFailure::SameSession,
            });
        }
        let provenance = DelegationProvenance::from_tool_request(spawning_request);
        Ok(Self {
            spawning_request: spawning_request.id(),
            parent,
            child,
            policy,
            lifecycle: DelegationLifecycle::Active,
            events: vec![DelegationEvent::Spawned {
                ordinal: DelegationEventOrdinal::first(),
                provenance,
            }],
        })
    }

    pub const fn spawning_request(&self) -> ToolRequestId {
        self.spawning_request
    }
    pub const fn parent(&self) -> SessionId {
        self.parent
    }
    pub const fn child(&self) -> SessionId {
        self.child
    }
    pub const fn policy(&self) -> ChildRelationshipPolicy {
        self.policy
    }
    pub const fn lifecycle(&self) -> DelegationLifecycle {
        self.lifecycle
    }
    pub fn events(&self) -> &[DelegationEvent] {
        &self.events
    }
    pub const fn child_creation_provenance(&self) -> SessionCreationProvenance {
        SessionCreationProvenance::delegated(self.spawning_request)
    }
    pub fn register_wait(
        &self,
        awaiting_request: &ToolRequest,
        mode: DelegationWaitMode,
    ) -> Result<DelegationWait, DelegationTransitionError> {
        self.require_active()?;
        if awaiting_request.session() != self.parent {
            return Err(self.fail(DelegationTransitionFailure::InvalidProvenance));
        }
        Ok(DelegationWait {
            awaiting_request: awaiting_request.id(),
            spawning_request: self.spawning_request,
            parent: self.parent,
            child: self.child,
            mode,
        })
    }

    pub fn deliver_message(
        mut self,
        id: DelegationMessageId,
        content: DelegationContent,
        provenance: DelegationProvenance,
    ) -> Result<Self, DelegationTransitionError> {
        self.require_active()?;
        let direction = match provenance.kind {
            DelegationProvenanceKind::ToolRequest { source_session, .. }
                if source_session == self.parent =>
            {
                DelegationMessageDirection::ParentToChild
            }
            DelegationProvenanceKind::ToolRequest { source_session, .. }
                if source_session == self.child =>
            {
                DelegationMessageDirection::ChildToParent
            }
            DelegationProvenanceKind::ToolRequest { .. }
            | DelegationProvenanceKind::ChildTurn { .. }
            | DelegationProvenanceKind::ParentCommand { .. } => {
                return Err(self.fail(DelegationTransitionFailure::InvalidProvenance));
            }
        };
        let ordinal = self.next_ordinal()?;
        self.events.push(DelegationEvent::MessageDelivered {
            ordinal,
            message: DelegationMessage {
                id,
                direction,
                content,
                provenance,
            },
        });
        Ok(self)
    }

    pub fn record_outcome(
        mut self,
        outcome: DelegationOutcome,
    ) -> Result<Self, DelegationTransitionError> {
        self.require_active()?;
        validate_outcome(&self, &outcome).map_err(|failure| self.fail(failure))?;
        let remains_active = matches!(outcome, DelegationOutcome::ContinueRunning { .. });
        let ordinal = self.next_ordinal()?;
        self.events
            .push(DelegationEvent::OutcomeRecorded { ordinal, outcome });
        if !remains_active {
            self.lifecycle = DelegationLifecycle::Terminal;
        }
        Ok(self)
    }

    fn require_active(&self) -> Result<(), DelegationTransitionError> {
        if self.lifecycle == DelegationLifecycle::Active {
            Ok(())
        } else {
            Err(self.fail(DelegationTransitionFailure::AlreadyTerminal))
        }
    }
    fn next_ordinal(&self) -> Result<DelegationEventOrdinal, DelegationTransitionError> {
        let Some(last_event) = self.events.last() else {
            return Err(self.fail(DelegationTransitionFailure::MissingSpawnEvent));
        };
        event_ordinal(last_event)
            .successor()
            .ok_or_else(|| self.fail(DelegationTransitionFailure::EventOrdinalExhausted))
    }
    fn fail(&self, failure: DelegationTransitionFailure) -> DelegationTransitionError {
        DelegationTransitionError { failure }
    }
}

const fn event_ordinal(event: &DelegationEvent) -> DelegationEventOrdinal {
    match event {
        DelegationEvent::Spawned { ordinal, .. }
        | DelegationEvent::MessageDelivered { ordinal, .. }
        | DelegationEvent::OutcomeRecorded { ordinal, .. } => *ordinal,
    }
}

fn validate_outcome(
    relation: &SessionDelegation,
    outcome: &DelegationOutcome,
) -> Result<(), DelegationTransitionFailure> {
    let (reason, provenance) = match outcome {
        DelegationOutcome::ResultReturned {
            reason, provenance, ..
        }
        | DelegationOutcome::ChildFailed { reason, provenance }
        | DelegationOutcome::ChildStopped { reason, provenance }
        | DelegationOutcome::ChildCancelled { reason, provenance }
        | DelegationOutcome::ContinueRunning { reason, provenance } => (*reason, *provenance),
    };
    let provenance_matches = match outcome {
        DelegationOutcome::ResultReturned { .. } => matches!(
            provenance.kind,
            DelegationProvenanceKind::ToolRequest { source_session, .. }
                if source_session == relation.child
        ),
        DelegationOutcome::ChildFailed { .. } => matches!(
            provenance.kind,
            DelegationProvenanceKind::ChildTurn { child, .. } if child == relation.child
        ),
        DelegationOutcome::ChildStopped { .. }
        | DelegationOutcome::ChildCancelled { .. }
        | DelegationOutcome::ContinueRunning { .. } => parent_command_matches(relation, provenance),
    };
    if !provenance_matches {
        return Err(DelegationTransitionFailure::InvalidProvenance);
    }
    let combination_matches = match outcome {
        DelegationOutcome::ResultReturned { .. } => {
            matches!(reason, DelegationOutcomeReason::ChildCompleted)
        }
        DelegationOutcome::ChildFailed { .. } => {
            matches!(reason, DelegationOutcomeReason::ChildExecutionFailed)
        }
        DelegationOutcome::ChildStopped { .. } => {
            descendant_action(relation.policy, reason) == Some(BoundChildAction::Stop)
        }
        DelegationOutcome::ChildCancelled { .. } => {
            descendant_action(relation.policy, reason) == Some(BoundChildAction::Cancel)
        }
        DelegationOutcome::ContinueRunning { .. } => {
            descendant_action(relation.policy, reason) == Some(BoundChildAction::KeepRunning)
        }
    };
    if combination_matches {
        Ok(())
    } else {
        Err(DelegationTransitionFailure::OutcomeReasonMismatch)
    }
}

fn parent_command_matches(relation: &SessionDelegation, provenance: DelegationProvenance) -> bool {
    matches!(
        provenance.kind,
        DelegationProvenanceKind::ParentCommand { parent, .. } if parent == relation.parent
    )
}

fn descendant_action(
    policy: ChildRelationshipPolicy,
    reason: DelegationOutcomeReason,
) -> Option<BoundChildAction> {
    match (policy, reason) {
        (
            ChildRelationshipPolicy::Background,
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            }
            | DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => Some(BoundChildAction::KeepRunning),
        (
            ChildRelationshipPolicy::Bound {
                on_parent_stopped, ..
            },
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => Some(on_parent_stopped),
        (
            ChildRelationshipPolicy::Bound {
                on_parent_cancelled,
                ..
            },
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => Some(on_parent_cancelled),
        (
            ChildRelationshipPolicy::Background | ChildRelationshipPolicy::Bound { .. },
            DelegationOutcomeReason::ChildCompleted
            | DelegationOutcomeReason::ChildExecutionFailed
            | DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAlone,
            }
            | DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAlone,
            },
        ) => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationTransitionFailure {
    SameSession,
    AlreadyTerminal,
    MissingSpawnEvent,
    InvalidProvenance,
    OutcomeReasonMismatch,
    EventOrdinalExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationTransitionError {
    failure: DelegationTransitionFailure,
}
impl DelegationTransitionError {
    pub const fn failure(self) -> DelegationTransitionFailure {
        self.failure
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        NormalizedToolArguments, ToolCallProposal, ToolName, ToolRequestOrdinal,
        test_support::{command_id, model_call_id, session_id, tool_request_id, turn_id},
    };

    const TEST_TOOL_NAME: &str = "spawn_session";

    /// One canonical logical request; the session seed derives every identity.
    fn request(session: u128) -> ToolRequest {
        ToolRequest::from_model_proposal(
            tool_request_id(session + 100),
            session_id(session),
            turn_id(session + 10),
            model_call_id(session + 20),
            ToolRequestOrdinal::from_u32(0),
            ToolCallProposal::new(
                ToolName::try_new(TEST_TOOL_NAME.into()).expect("valid name"),
                NormalizedToolArguments::try_from_provider_text("{}".into())
                    .expect("valid arguments"),
            ),
        )
    }
    /// A later await request in the same session, with identities distinct from spawn.
    fn await_request(session: u128) -> ToolRequest {
        ToolRequest::from_model_proposal(
            tool_request_id(session + 200),
            session_id(session),
            turn_id(session + 11),
            model_call_id(session + 21),
            ToolRequestOrdinal::from_u32(0),
            ToolCallProposal::new(
                ToolName::try_new("await_session".into()).expect("valid name"),
                NormalizedToolArguments::try_from_provider_text("{}".into())
                    .expect("valid arguments"),
            ),
        )
    }
    fn content(value: &str) -> DelegationContent {
        DelegationContent::try_new(value.into()).expect("nonempty bounded content")
    }

    /// S18 / INV-003: delegated cause retains exact spawning work while ancestry remains explicitly none.
    #[test]
    fn s18_inv003_delegated_creation_keeps_cause_independent_from_ancestry() {
        let relation = SessionDelegation::spawn(
            &request(2),
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let provenance = relation.child_creation_provenance();
        assert_eq!(
            provenance.cause(),
            crate::SessionCreationCause::Delegated {
                spawning_request: relation.spawning_request()
            }
        );
        assert_eq!(provenance.ancestry(), crate::TranscriptAncestry::None);
    }

    /// S18 / INV-010: a foreground delivery registration yields the exact slot-retaining wait.
    #[test]
    fn s18_inv010_foreground_registration_yields_exact_child_wait() {
        let relation = SessionDelegation::spawn(
            &request(2),
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let registration = relation
            .register_wait(&await_request(2), DelegationWaitMode::Foreground)
            .expect("parent registers foreground delivery");
        let wait = registration
            .foreground_subject()
            .expect("foreground wait exists");
        assert_eq!(wait.awaiting_request(), registration.awaiting_request());
        assert_eq!(wait.spawning_request(), relation.spawning_request());
        assert_eq!(wait.child(), relation.child());
    }

    /// S18 / INV-010: a background delivery registration never retains the parent slot.
    #[test]
    fn s18_inv010_background_registration_has_no_child_wait() {
        let relation = SessionDelegation::spawn(
            &request(2),
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let registration = relation
            .register_wait(&await_request(2), DelegationWaitMode::Background)
            .expect("parent registers background delivery");
        assert_eq!(registration.foreground_subject(), None);
    }

    /// S18 / INV-010: message direction is derived from exact request ownership and unrelated senders fail closed.
    #[test]
    fn s18_inv010_messages_are_relation_directed_and_request_provenanced() {
        let parent_request = request(2);
        let child_request = request(3);
        let unrelated_request = request(6);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let relation = relation
            .deliver_message(
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(7)),
                content("parent message"),
                DelegationProvenance::from_tool_request(&parent_request),
            )
            .expect("parent sends to child");
        let relation = relation
            .deliver_message(
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(8)),
                content("child message"),
                DelegationProvenance::from_tool_request(&child_request),
            )
            .expect("child sends to parent");
        let error = relation
            .clone()
            .deliver_message(
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(9)),
                content("unrelated"),
                DelegationProvenance::from_tool_request(&unrelated_request),
            )
            .expect_err("unrelated sender rejected");
        let parent_message = relation.events()[1]
            .message()
            .expect("second event is parent message");
        let child_message = relation.events()[2]
            .message()
            .expect("third event is child message");
        assert_eq!(
            parent_message.direction(),
            DelegationMessageDirection::ParentToChild
        );
        assert_eq!(
            child_message.direction(),
            DelegationMessageDirection::ChildToParent
        );
        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S19 / INV-010: a bound keep-running policy records a typed no-change event.
    #[test]
    fn s19_inv010_bound_keep_running_records_no_change() {
        let parent_request = request(2);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::KeepRunning,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        )
        .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ContinueRunning {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                },
                provenance: DelegationProvenance::from_parent_command(session_id(2), command_id(5)),
            })
            .expect("no-change evaluation is recorded");
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(relation.events().len(), 2);
    }

    /// S18 / INV-010: returned content with exact child provenance terminalizes once.
    #[test]
    fn s18_inv010_returned_result_terminalizes_exactly_once() {
        let parent_request = request(2);
        let child_request = request(3);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ResultReturned {
                content: content("delivered result"),
                reason: DelegationOutcomeReason::ChildCompleted,
                provenance: DelegationProvenance::from_tool_request(&child_request),
            })
            .expect("child result terminalizes");
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(relation.events().len(), 2);
        let error = relation
            .record_outcome(DelegationOutcome::ChildFailed {
                reason: DelegationOutcomeReason::ChildExecutionFailed,
                provenance: DelegationProvenance::from_child_turn(session_id(3), turn_id(6)),
            })
            .expect_err("terminal relation never reopens");
        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::AlreadyTerminal
        );
    }

    /// S19 / INV-010: a background child explicitly continues when a descendant stop evaluates it.
    #[test]
    fn s19_inv010_background_child_survives_parent_stop_with_typed_outcome() {
        let parent_request = request(2);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ContinueRunning {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                },
                provenance: DelegationProvenance::from_parent_command(session_id(2), command_id(5)),
            })
            .expect("background relationship records survival");
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(relation.events().len(), 2);
    }

    /// S19 / INV-010: a background child explicitly continues when descendant cancellation evaluates it.
    #[test]
    fn s19_inv010_background_child_survives_parent_cancel_with_typed_outcome() {
        let parent_request = request(2);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ContinueRunning {
                reason: DelegationOutcomeReason::ParentCancelled {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                },
                provenance: DelegationProvenance::from_parent_command(session_id(2), command_id(5)),
            })
            .expect("background relationship records survival");
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(relation.events().len(), 2);
    }

    /// S19 / INV-010: a bound child accepts only its chosen parent-stop action.
    #[test]
    fn s19_inv010_bound_child_follows_parent_stop_policy() {
        let parent_request = request(2);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        )
        .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ChildStopped {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                },
                provenance: DelegationProvenance::from_parent_command(session_id(2), command_id(5)),
            })
            .expect("bound stop policy authorizes stopped outcome");
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
    }

    /// S19 / INV-010: parent-alone creates no descendant outcome authority.
    #[test]
    fn s19_inv010_parent_alone_cannot_fabricate_child_disposition() {
        let parent_request = request(2);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let error = relation
            .record_outcome(DelegationOutcome::ContinueRunning {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAlone,
                },
                provenance: DelegationProvenance::from_parent_command(session_id(2), command_id(5)),
            })
            .expect_err("parent-alone does not evaluate descendants");
        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::OutcomeReasonMismatch
        );
    }
}

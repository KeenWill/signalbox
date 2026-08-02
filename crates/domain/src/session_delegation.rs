//! Typed delegated-session relation, messages, outcomes, and wait subject.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

use sha2::{Digest, Sha256};

use crate::{
    DelegationMessageId, DurableCommandId, NonEmptyUnicodeText, SessionCreationProvenance,
    SessionId, ToolRequest, ToolRequestId, TurnId,
};

/// Returned-result content bound. Request-carried content must also fit its
/// complete normalized tool-argument envelope.
pub const MAX_DELEGATION_CONTENT_UTF8_BYTES: usize = 1_048_576;

const MAX_ACTIVE_DIRECT_CHILDREN: u32 = 32;

const SPAWN_SESSION_TOOL_NAME: &str = "spawn_session";
const AWAIT_SESSION_TOOL_NAME: &str = "await_session";
const SEND_SESSION_MESSAGE_TOOL_NAME: &str = "send_session_message";

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

/// Closed reason that a valid delegated spawn was refused before child creation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegatedSpawnRejection {
    /// The parent already retains the maximum active direct-child inventory.
    ActiveDirectChildLimit,
}

impl DelegatedSpawnRejection {
    /// Returns the fixed limit associated with this refusal.
    pub const fn limit(self) -> u32 {
        match self {
            Self::ActiveDirectChildLimit => MAX_ACTIVE_DIRECT_CHILDREN,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ParentTerminationKind {
    Stopped,
    Cancelled,
}

/// Exact applied parent termination authority.
///
/// Raw identities cannot construct this proof. The scheduling slice supplies
/// it only from the exact applied stop or cancellation command result; this
/// foundation slice keeps that producer sealed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ParentTerminationAuthority {
    parent: SessionId,
    turn: TurnId,
    command: DurableCommandId,
    kind: ParentTerminationKind,
    scope: DescendantTerminationScope,
}

impl ParentTerminationAuthority {
    pub const fn parent(self) -> SessionId {
        self.parent
    }

    pub const fn turn(self) -> TurnId {
        self.turn
    }

    pub const fn command(self) -> DurableCommandId {
        self.command
    }

    pub const fn scope(self) -> DescendantTerminationScope {
        self.scope
    }

    pub const fn kind(self) -> ParentTerminationKind {
        self.kind
    }
}

/// One policy-derived disposition for an evaluated relationship edge.
///
/// The disposition retains the relationship identity separately from its
/// outcome so a persistence caller can correlate an ordered plan without
/// inferring an edge from provenance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DelegationCascadeDisposition {
    spawning_request: ToolRequestId,
    parent: SessionId,
    child: SessionId,
    outcome: DelegationOutcome,
}

impl DelegationCascadeDisposition {
    pub const fn spawning_request(&self) -> ToolRequestId {
        self.spawning_request
    }

    pub const fn parent(&self) -> SessionId {
        self.parent
    }

    pub const fn child(&self) -> SessionId {
        self.child
    }

    pub const fn outcome(&self) -> &DelegationOutcome {
        &self.outcome
    }
}

/// Complete deterministic result of evaluating a parent termination cascade.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationCascadeEvaluation {
    dispositions: Box<[DelegationCascadeDisposition]>,
}

impl DelegationCascadeEvaluation {
    pub const fn dispositions(&self) -> &[DelegationCascadeDisposition] {
        &self.dispositions
    }
}

/// Why an explicitly ordered relationship inventory could not be evaluated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationCascadeEvaluationFailure {
    AuthorityScopeMismatch,
    InventoryNotStrictlyOrdered {
        earlier: ToolRequestId,
        later: ToolRequestId,
    },
    DuplicateChild {
        child: SessionId,
    },
    InvalidRelationshipTree {
        child: SessionId,
    },
    UnreachableRelationship {
        spawning_request: ToolRequestId,
    },
    RelationshipTransition {
        spawning_request: ToolRequestId,
        failure: DelegationTransitionFailure,
    },
}

impl std::fmt::Display for DelegationCascadeEvaluationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "delegation cascade evaluation failed: {self:?}")
    }
}

impl std::error::Error for DelegationCascadeEvaluationFailure {}

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

    /// Concatenates exact assistant text parts in semantic order without
    /// inserting separators, then applies the delegation content bound.
    pub fn from_assistant_text(
        parts: &[crate::AssistantText],
    ) -> Result<Self, DelegationContentError> {
        let utf8_byte_length = parts.iter().map(|part| part.as_str().len()).sum();
        if utf8_byte_length > MAX_DELEGATION_CONTENT_UTF8_BYTES {
            return Err(DelegationContentError::Oversized { utf8_byte_length });
        }
        let mut value = String::with_capacity(utf8_byte_length);
        for part in parts {
            value.push_str(part.as_str());
        }
        Self::try_new(value)
    }
}

/// Why delegated content was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationContentError {
    Invalid(crate::NonEmptyUnicodeTextFailure),
    Oversized { utf8_byte_length: usize },
}
impl std::fmt::Display for DelegationContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(failure) => write!(f, "invalid delegation content: {failure:?}"),
            Self::Oversized { utf8_byte_length } => write!(
                f,
                "delegation content is {utf8_byte_length} bytes; maximum is {MAX_DELEGATION_CONTENT_UTF8_BYTES}"
            ),
        }
    }
}
impl std::error::Error for DelegationContentError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TerminalChildTurnKind {
    Returned,
    Failed,
    Cancelled,
}

/// Exact child turn sealed by checked terminal scheduling evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TerminalChildTurn {
    session: SessionId,
    turn: TurnId,
    kind: TerminalChildTurnKind,
    reason: DelegationOutcomeReason,
    result_digest: Option<[u8; 32]>,
}
impl TerminalChildTurn {
    pub fn from_scheduling(
        value: &crate::AcceptedInputTurnSchedulingProjection,
        reason: DelegationOutcomeReason,
        assistant_text: &[crate::AssistantText],
    ) -> Option<Self> {
        let (kind, result_digest) = match (value.status(), reason) {
            (
                crate::AcceptedInputTurnSchedulingStatus::TerminalCompleted,
                DelegationOutcomeReason::ChildCompleted,
            ) => {
                let content = DelegationContent::from_assistant_text(assistant_text).ok()?;
                (
                    TerminalChildTurnKind::Returned,
                    Some(delegation_content_digest(&content)),
                )
            }
            (
                crate::AcceptedInputTurnSchedulingStatus::TerminalFailed
                | crate::AcceptedInputTurnSchedulingStatus::TerminalRefused,
                DelegationOutcomeReason::ChildExecutionFailed,
            ) => (TerminalChildTurnKind::Failed, None),
            (
                crate::AcceptedInputTurnSchedulingStatus::TerminalCompleted,
                DelegationOutcomeReason::ChildResultUnavailable,
            ) if DelegationContent::from_assistant_text(assistant_text).is_err() => {
                (TerminalChildTurnKind::Failed, None)
            }
            (
                crate::AcceptedInputTurnSchedulingStatus::TerminalCancelled,
                DelegationOutcomeReason::ChildCancelled,
            ) => (TerminalChildTurnKind::Cancelled, None),
            (
                crate::AcceptedInputTurnSchedulingStatus::Queued
                | crate::AcceptedInputTurnSchedulingStatus::Active
                | crate::AcceptedInputTurnSchedulingStatus::TerminalCompleted
                | crate::AcceptedInputTurnSchedulingStatus::TerminalFailed
                | crate::AcceptedInputTurnSchedulingStatus::TerminalRefused
                | crate::AcceptedInputTurnSchedulingStatus::TerminalCancelled
                | crate::AcceptedInputTurnSchedulingStatus::TerminalReconciliationRequired,
                _,
            ) => return None,
        };
        Some(Self {
            session: value.session(),
            turn: value.turn(),
            kind,
            reason,
            result_digest,
        })
    }
    pub const fn session(self) -> SessionId {
        self.session
    }
    pub const fn turn(self) -> TurnId {
        self.turn
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DelegationProvenanceKind {
    ToolRequest {
        source_session: SessionId,
        source_turn: TurnId,
        request: ToolRequestId,
        purpose: DelegationToolRequestPurpose,
    },
    ChildTurn {
        terminal: TerminalChildTurn,
    },
    ParentCommand {
        authority: ParentTerminationAuthority,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DelegationToolRequestPurpose {
    Spawn,
    SendMessage,
    Other,
}

/// Exact typed authority for a delegation event.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DelegationProvenance {
    kind: DelegationProvenanceKind,
}

impl DelegationProvenance {
    /// Seals request identity together with its canonical owning session/turn.
    pub fn from_tool_request(request: &ToolRequest) -> Self {
        let purpose = match request.name().as_str() {
            SPAWN_SESSION_TOOL_NAME => DelegationToolRequestPurpose::Spawn,
            SEND_SESSION_MESSAGE_TOOL_NAME => DelegationToolRequestPurpose::SendMessage,
            _ => DelegationToolRequestPurpose::Other,
        };
        Self {
            kind: DelegationProvenanceKind::ToolRequest {
                source_session: request.session(),
                source_turn: request.turn(),
                request: request.id(),
                purpose,
            },
        }
    }

    pub const fn from_terminal_child(terminal: TerminalChildTurn) -> Self {
        Self {
            kind: DelegationProvenanceKind::ChildTurn { terminal },
        }
    }

    pub const fn from_parent_termination(authority: ParentTerminationAuthority) -> Self {
        Self {
            kind: DelegationProvenanceKind::ParentCommand { authority },
        }
    }

    pub const fn tool_request(&self) -> Option<(SessionId, TurnId, ToolRequestId)> {
        match self.kind {
            DelegationProvenanceKind::ToolRequest {
                source_session,
                source_turn,
                request,
                ..
            } => Some((source_session, source_turn, request)),
            DelegationProvenanceKind::ChildTurn { .. }
            | DelegationProvenanceKind::ParentCommand { .. } => None,
        }
    }

    pub const fn child_turn(&self) -> Option<(SessionId, TurnId)> {
        match self.kind {
            DelegationProvenanceKind::ChildTurn { terminal } => {
                Some((terminal.session(), terminal.turn()))
            }
            DelegationProvenanceKind::ToolRequest { .. }
            | DelegationProvenanceKind::ParentCommand { .. } => None,
        }
    }

    pub const fn parent_command(&self) -> Option<(SessionId, TurnId, DurableCommandId)> {
        match self.kind {
            DelegationProvenanceKind::ParentCommand { authority } => {
                Some((authority.parent(), authority.turn(), authority.command()))
            }
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
    ChildResultUnavailable,
    ChildCancelled,
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
    task: DelegationContent,
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
        let task =
            spawn_task(spawning_request, policy).map_err(|failure| DelegationTransitionError {
                spawning_request: spawning_request.id(),
                failure,
            })?;
        if parent == child {
            return Err(DelegationTransitionError {
                spawning_request: spawning_request.id(),
                failure: DelegationTransitionFailure::SameSession,
            });
        }
        let provenance = DelegationProvenance::from_tool_request(spawning_request);
        Ok(Self {
            spawning_request: spawning_request.id(),
            parent,
            child,
            task,
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
    pub const fn task(&self) -> &DelegationContent {
        &self.task
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
        if awaiting_request.session() != self.parent
            || awaiting_request.id() == self.spawning_request
        {
            return Err(self.fail(DelegationTransitionFailure::InvalidProvenance));
        }
        if awaiting_request.name().as_str() != AWAIT_SESSION_TOOL_NAME
            || !request_arguments_match(
                awaiting_request,
                serde_json::json!({
                    "child_session_id": self.child.as_uuid().to_string(),
                    "mode": wait_mode_argument(mode),
                }),
            )
        {
            return Err(self.fail(DelegationTransitionFailure::InvalidToolRequestPurpose));
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
        sending_request: &ToolRequest,
        id: DelegationMessageId,
        content: DelegationContent,
    ) -> Result<(Self, DelegationEvent), DelegationTransitionError> {
        let (direction, peer) = match sending_request.session() {
            source if source == self.parent => {
                (DelegationMessageDirection::ParentToChild, self.child)
            }
            source if source == self.child => {
                (DelegationMessageDirection::ChildToParent, self.parent)
            }
            _ => return Err(self.fail(DelegationTransitionFailure::InvalidProvenance)),
        };
        if sending_request.id() == self.spawning_request {
            return Err(self.fail(DelegationTransitionFailure::InvalidProvenance));
        }
        if sending_request.name().as_str() != SEND_SESSION_MESSAGE_TOOL_NAME
            || !request_arguments_match(
                sending_request,
                serde_json::json!({
                    "content": content.as_str(), "peer_session_id": peer.as_uuid().to_string(),
                }),
            )
        {
            return Err(self.fail(DelegationTransitionFailure::InvalidToolRequestPurpose));
        }
        let provenance = DelegationProvenance::from_tool_request(sending_request);
        if let Some(existing) = self
            .events
            .iter()
            .find(|event| {
                event.message().is_some_and(|message| {
                    message
                        .provenance()
                        .tool_request()
                        .is_some_and(|(_, _, request)| request == sending_request.id())
                })
            })
            .cloned()
        {
            if existing.message().is_some_and(|message| {
                message.direction() == direction
                    && message.content() == &content
                    && message.provenance() == provenance
            }) {
                return Ok((self, existing));
            }
            return Err(self.fail(DelegationTransitionFailure::ConflictingMessageReplay));
        }
        if self
            .events
            .iter()
            .any(|event| event.message().is_some_and(|message| message.id() == id))
        {
            return Err(self.fail(DelegationTransitionFailure::DuplicateMessageIdentity));
        }
        let ordinal = self.next_ordinal()?;
        let event = DelegationEvent::MessageDelivered {
            ordinal,
            message: DelegationMessage {
                id,
                direction,
                content,
                provenance,
            },
        };
        self.events.push(event.clone());
        Ok((self, event))
    }

    pub fn record_outcome(
        mut self,
        outcome: DelegationOutcome,
    ) -> Result<Self, DelegationTransitionError> {
        let provenance = outcome_provenance(&outcome);
        if let Some(existing) = self
            .events
            .iter()
            .filter_map(DelegationEvent::outcome)
            .find(|existing| outcome_provenance(existing) == provenance)
        {
            if existing == &outcome {
                return Ok(self);
            }
            return Err(self.fail(DelegationTransitionFailure::DuplicateOutcomeAuthority));
        }
        self.require_active()?;
        validate_outcome(&self, &outcome).map_err(|failure| self.fail(failure))?;
        let lifecycle = match &outcome {
            DelegationOutcome::ContinueRunning { .. } => DelegationLifecycle::Active,
            DelegationOutcome::ResultReturned { .. }
            | DelegationOutcome::ChildFailed { .. }
            | DelegationOutcome::ChildStopped { .. }
            | DelegationOutcome::ChildCancelled { .. } => DelegationLifecycle::Terminal,
        };
        let ordinal = self.next_ordinal()?;
        self.events
            .push(DelegationEvent::OutcomeRecorded { ordinal, outcome });
        self.lifecycle = lifecycle;
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
        DelegationTransitionError {
            spawning_request: self.spawning_request,
            failure,
        }
    }
}

#[derive(Clone, Copy)]
enum PlannedCascadeAction {
    ContinueRunning,
    Stop,
    Cancel,
}

/// Evaluates one parent termination against an already locked relationship inventory.
///
/// The ordered reachable relationships must contain the complete structurally
/// reachable tree in the caller's stable spawning-request order. This function
/// does not acquire or imply locks. It returns dispositions in that supplied
/// order, omitting only descendants below an explicitly pruned relationship.
pub fn evaluate_delegation_cascade(
    ordered_reachable_relationships: &[SessionDelegation],
    scope: DescendantTerminationScope,
    authority: ParentTerminationAuthority,
) -> Result<DelegationCascadeEvaluation, DelegationCascadeEvaluationFailure> {
    if authority.scope() != scope {
        return Err(DelegationCascadeEvaluationFailure::AuthorityScopeMismatch);
    }
    if scope == DescendantTerminationScope::ParentAlone {
        return Ok(DelegationCascadeEvaluation {
            dispositions: Box::new([]),
        });
    }
    validate_cascade_inventory_order(ordered_reachable_relationships)?;
    let children_by_parent =
        validate_cascade_tree(ordered_reachable_relationships, authority.parent())?;
    let planned = plan_cascade_actions(
        ordered_reachable_relationships,
        &children_by_parent,
        authority.parent(),
        authority.kind(),
    );
    let mut dispositions = Vec::new();
    for (relation, planned) in ordered_reachable_relationships.iter().zip(planned) {
        let Some((parent_kind, action)) = planned else {
            continue;
        };
        let disposition = cascade_disposition(relation, authority, parent_kind, action)?;
        relation
            .clone()
            .record_outcome(disposition.outcome().clone())
            .map_err(
                |error| DelegationCascadeEvaluationFailure::RelationshipTransition {
                    spawning_request: relation.spawning_request(),
                    failure: error.failure(),
                },
            )?;
        dispositions.push(disposition);
    }
    Ok(DelegationCascadeEvaluation {
        dispositions: dispositions.into_boxed_slice(),
    })
}

fn validate_cascade_inventory_order(
    relationships: &[SessionDelegation],
) -> Result<(), DelegationCascadeEvaluationFailure> {
    for adjacent in relationships.windows(2) {
        let earlier = adjacent[0].spawning_request();
        let later = adjacent[1].spawning_request();
        if earlier >= later {
            return Err(
                DelegationCascadeEvaluationFailure::InventoryNotStrictlyOrdered { earlier, later },
            );
        }
    }
    Ok(())
}

fn validate_cascade_tree(
    relationships: &[SessionDelegation],
    root: SessionId,
) -> Result<BTreeMap<SessionId, Vec<usize>>, DelegationCascadeEvaluationFailure> {
    let mut parent_by_child = BTreeMap::new();
    let mut children_by_parent: BTreeMap<SessionId, Vec<usize>> = BTreeMap::new();
    for (index, relation) in relationships.iter().enumerate() {
        if parent_by_child
            .insert(relation.child(), relation.parent())
            .is_some()
        {
            return Err(DelegationCascadeEvaluationFailure::DuplicateChild {
                child: relation.child(),
            });
        }
        children_by_parent
            .entry(relation.parent())
            .or_default()
            .push(index);
    }
    let mut reachable_sessions = BTreeSet::from([root]);
    let mut pending_parents = vec![root];
    while let Some(parent) = pending_parents.pop() {
        let Some(children) = children_by_parent.get(&parent) else {
            continue;
        };
        for index in children {
            let child = relationships[*index].child();
            if !reachable_sessions.insert(child) {
                return Err(DelegationCascadeEvaluationFailure::InvalidRelationshipTree { child });
            }
            pending_parents.push(child);
        }
    }
    for relation in relationships {
        if !reachable_sessions.contains(&relation.child()) {
            return Err(
                DelegationCascadeEvaluationFailure::UnreachableRelationship {
                    spawning_request: relation.spawning_request(),
                },
            );
        }
    }
    Ok(children_by_parent)
}

fn plan_cascade_actions(
    relationships: &[SessionDelegation],
    children_by_parent: &BTreeMap<SessionId, Vec<usize>>,
    root: SessionId,
    root_kind: ParentTerminationKind,
) -> Vec<Option<(ParentTerminationKind, PlannedCascadeAction)>> {
    let mut planned = vec![None; relationships.len()];
    let mut pending_parents = vec![(root, root_kind)];
    while let Some((parent, parent_kind)) = pending_parents.pop() {
        let Some(children) = children_by_parent.get(&parent) else {
            continue;
        };
        for index in children {
            let relation = &relationships[*index];
            let action = cascade_action(relation.policy(), parent_kind);
            planned[*index] = Some((parent_kind, action));
            match action {
                PlannedCascadeAction::ContinueRunning => {}
                PlannedCascadeAction::Stop => {
                    pending_parents.push((relation.child(), ParentTerminationKind::Stopped));
                }
                PlannedCascadeAction::Cancel => {
                    pending_parents.push((relation.child(), ParentTerminationKind::Cancelled));
                }
            }
        }
    }
    planned
}

const fn cascade_action(
    policy: ChildRelationshipPolicy,
    parent_kind: ParentTerminationKind,
) -> PlannedCascadeAction {
    let action = match (policy, parent_kind) {
        (ChildRelationshipPolicy::Background, _) => BoundChildAction::KeepRunning,
        (
            ChildRelationshipPolicy::Bound {
                on_parent_stopped, ..
            },
            ParentTerminationKind::Stopped,
        ) => on_parent_stopped,
        (
            ChildRelationshipPolicy::Bound {
                on_parent_cancelled,
                ..
            },
            ParentTerminationKind::Cancelled,
        ) => on_parent_cancelled,
    };
    match action {
        BoundChildAction::KeepRunning => PlannedCascadeAction::ContinueRunning,
        BoundChildAction::Stop => PlannedCascadeAction::Stop,
        BoundChildAction::Cancel => PlannedCascadeAction::Cancel,
    }
}

fn cascade_disposition(
    relation: &SessionDelegation,
    authority: ParentTerminationAuthority,
    parent_kind: ParentTerminationKind,
    action: PlannedCascadeAction,
) -> Result<DelegationCascadeDisposition, DelegationCascadeEvaluationFailure> {
    let spawning_turn = relation.events().first().and_then(|event| match event {
        DelegationEvent::Spawned { provenance, .. } => {
            provenance.tool_request().map(|(_, turn, _)| turn)
        }
        DelegationEvent::MessageDelivered { .. } | DelegationEvent::OutcomeRecorded { .. } => None,
    });
    let Some(turn) = spawning_turn else {
        return Err(DelegationCascadeEvaluationFailure::RelationshipTransition {
            spawning_request: relation.spawning_request(),
            failure: DelegationTransitionFailure::MissingSpawnEvent,
        });
    };
    let reason = match parent_kind {
        ParentTerminationKind::Stopped => DelegationOutcomeReason::ParentStopped {
            scope: authority.scope(),
        },
        ParentTerminationKind::Cancelled => DelegationOutcomeReason::ParentCancelled {
            scope: authority.scope(),
        },
    };
    let provenance = DelegationProvenance::from_parent_termination(ParentTerminationAuthority {
        parent: relation.parent(),
        turn,
        command: authority.command(),
        kind: parent_kind,
        scope: authority.scope(),
    });
    let outcome = match action {
        PlannedCascadeAction::ContinueRunning => {
            DelegationOutcome::ContinueRunning { reason, provenance }
        }
        PlannedCascadeAction::Stop => DelegationOutcome::ChildStopped { reason, provenance },
        PlannedCascadeAction::Cancel => DelegationOutcome::ChildCancelled { reason, provenance },
    };
    Ok(DelegationCascadeDisposition {
        spawning_request: relation.spawning_request(),
        parent: relation.parent(),
        child: relation.child(),
        outcome,
    })
}

const fn event_ordinal(event: &DelegationEvent) -> DelegationEventOrdinal {
    match event {
        DelegationEvent::Spawned { ordinal, .. }
        | DelegationEvent::MessageDelivered { ordinal, .. }
        | DelegationEvent::OutcomeRecorded { ordinal, .. } => *ordinal,
    }
}

const fn outcome_provenance(outcome: &DelegationOutcome) -> DelegationProvenance {
    match outcome {
        DelegationOutcome::ResultReturned { provenance, .. }
        | DelegationOutcome::ChildFailed { provenance, .. }
        | DelegationOutcome::ChildStopped { provenance, .. }
        | DelegationOutcome::ChildCancelled { provenance, .. }
        | DelegationOutcome::ContinueRunning { provenance, .. } => *provenance,
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
        DelegationOutcome::ResultReturned { content, .. } => child_turn_matches(
            relation,
            provenance,
            reason,
            Some(content),
            TerminalChildTurnKind::Returned,
        ),
        DelegationOutcome::ChildFailed { .. } => child_turn_matches(
            relation,
            provenance,
            reason,
            None,
            TerminalChildTurnKind::Failed,
        ),
        DelegationOutcome::ChildCancelled { .. }
            if reason == DelegationOutcomeReason::ChildCancelled =>
        {
            child_turn_matches(
                relation,
                provenance,
                reason,
                None,
                TerminalChildTurnKind::Cancelled,
            )
        }
        DelegationOutcome::ChildStopped { .. }
        | DelegationOutcome::ChildCancelled { .. }
        | DelegationOutcome::ContinueRunning { .. } => {
            parent_command_matches(relation, provenance, reason)
        }
    };
    if !provenance_matches {
        return Err(DelegationTransitionFailure::InvalidProvenance);
    }
    let combination_matches = match outcome {
        DelegationOutcome::ResultReturned { .. } => match reason {
            DelegationOutcomeReason::ChildCompleted => true,
            DelegationOutcomeReason::ChildExecutionFailed
            | DelegationOutcomeReason::ChildResultUnavailable
            | DelegationOutcomeReason::ChildCancelled
            | DelegationOutcomeReason::ParentStopped { .. }
            | DelegationOutcomeReason::ParentCancelled { .. } => false,
        },
        DelegationOutcome::ChildFailed { .. } => match reason {
            DelegationOutcomeReason::ChildExecutionFailed
            | DelegationOutcomeReason::ChildResultUnavailable => true,
            DelegationOutcomeReason::ChildCompleted
            | DelegationOutcomeReason::ChildCancelled
            | DelegationOutcomeReason::ParentStopped { .. }
            | DelegationOutcomeReason::ParentCancelled { .. } => false,
        },
        DelegationOutcome::ChildStopped { .. } => {
            descendant_action(relation.policy, reason) == Some(BoundChildAction::Stop)
        }
        DelegationOutcome::ChildCancelled { .. } => {
            reason == DelegationOutcomeReason::ChildCancelled
                || descendant_action(relation.policy, reason) == Some(BoundChildAction::Cancel)
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

fn child_turn_matches(
    relation: &SessionDelegation,
    provenance: DelegationProvenance,
    reason: DelegationOutcomeReason,
    content: Option<&DelegationContent>,
    kind: TerminalChildTurnKind,
) -> bool {
    let result_digest = content.map(delegation_content_digest);
    matches!(
        provenance.kind,
        DelegationProvenanceKind::ChildTurn { terminal }
            if terminal.session() == relation.child
                && terminal.kind == kind
                && terminal.reason == reason
                && terminal.result_digest == result_digest
    )
}

fn delegation_content_digest(content: &DelegationContent) -> [u8; 32] {
    Sha256::digest(content.as_str().as_bytes()).into()
}

fn request_arguments_match(request: &ToolRequest, expected: serde_json::Value) -> bool {
    serde_json::from_str::<serde_json::Value>(request.arguments().as_str())
        .is_ok_and(|value| value == expected)
}
fn spawn_task(
    request: &ToolRequest,
    policy: ChildRelationshipPolicy,
) -> Result<DelegationContent, DelegationTransitionFailure> {
    if request.name().as_str() != SPAWN_SESSION_TOOL_NAME {
        return Err(DelegationTransitionFailure::InvalidToolRequestPurpose);
    }
    let value = serde_json::from_str::<serde_json::Value>(request.arguments().as_str())
        .map_err(|_| DelegationTransitionFailure::InvalidToolRequestPurpose)?;
    let task_text = value
        .get("task")
        .and_then(serde_json::Value::as_str)
        .ok_or(DelegationTransitionFailure::InvalidToolRequestPurpose)?;
    let task = DelegationContent::try_new(task_text.to_owned())
        .map_err(DelegationTransitionFailure::InvalidTaskContent)?;
    (value
        == serde_json::json!({
            "relationship": relationship_argument(policy),
            "task": task.as_str()
        }))
    .then_some(task)
    .ok_or(DelegationTransitionFailure::InvalidToolRequestPurpose)
}
fn relationship_argument(policy: ChildRelationshipPolicy) -> serde_json::Value {
    match policy {
        ChildRelationshipPolicy::Background => serde_json::json!({ "kind": "background" }),
        ChildRelationshipPolicy::Bound {
            on_parent_stopped,
            on_parent_cancelled,
        } => serde_json::json!({
            "kind": "bound", "on_parent_cancelled": action_argument(on_parent_cancelled),
            "on_parent_stopped": action_argument(on_parent_stopped),
        }),
    }
}
const fn action_argument(action: BoundChildAction) -> &'static str {
    match action {
        BoundChildAction::KeepRunning => "keep_running",
        BoundChildAction::Stop => "stop",
        BoundChildAction::Cancel => "cancel",
    }
}
const fn wait_mode_argument(mode: DelegationWaitMode) -> &'static str {
    match mode {
        DelegationWaitMode::Foreground => "foreground",
        DelegationWaitMode::Background => "background",
    }
}

fn parent_command_matches(
    relation: &SessionDelegation,
    provenance: DelegationProvenance,
    reason: DelegationOutcomeReason,
) -> bool {
    let expected = match reason {
        DelegationOutcomeReason::ParentStopped { scope } => {
            Some((ParentTerminationKind::Stopped, scope))
        }
        DelegationOutcomeReason::ParentCancelled { scope } => {
            Some((ParentTerminationKind::Cancelled, scope))
        }
        DelegationOutcomeReason::ChildCompleted
        | DelegationOutcomeReason::ChildExecutionFailed
        | DelegationOutcomeReason::ChildResultUnavailable
        | DelegationOutcomeReason::ChildCancelled => None,
    };
    matches!(
        (provenance.kind, expected),
        (
            DelegationProvenanceKind::ParentCommand { authority },
            Some((kind, scope)),
        ) if authority.parent == relation.parent
            && authority.kind == kind
            && authority.scope == scope
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
            | DelegationOutcomeReason::ChildResultUnavailable
            | DelegationOutcomeReason::ChildCancelled
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
    InvalidToolRequestPurpose,
    InvalidTaskContent(DelegationContentError),
    DuplicateMessageIdentity,
    ConflictingMessageReplay,
    DuplicateOutcomeAuthority,
    OutcomeReasonMismatch,
    EventOrdinalExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationTransitionError {
    spawning_request: ToolRequestId,
    failure: DelegationTransitionFailure,
}
impl DelegationTransitionError {
    pub const fn failure(self) -> DelegationTransitionFailure {
        self.failure
    }
    pub const fn spawning_request(self) -> ToolRequestId {
        self.spawning_request
    }
}
impl std::fmt::Display for DelegationTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "delegation {} transition failed: {:?}",
            self.spawning_request.as_uuid(),
            self.failure
        )
    }
}
impl std::error::Error for DelegationTransitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.failure {
            DelegationTransitionFailure::InvalidTaskContent(error) => Some(error),
            DelegationTransitionFailure::SameSession
            | DelegationTransitionFailure::AlreadyTerminal
            | DelegationTransitionFailure::MissingSpawnEvent
            | DelegationTransitionFailure::InvalidProvenance
            | DelegationTransitionFailure::InvalidToolRequestPurpose
            | DelegationTransitionFailure::DuplicateMessageIdentity
            | DelegationTransitionFailure::ConflictingMessageReplay
            | DelegationTransitionFailure::DuplicateOutcomeAuthority
            | DelegationTransitionFailure::OutcomeReasonMismatch
            | DelegationTransitionFailure::EventOrdinalExhausted => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        NormalizedToolArguments, ToolCallProposal, ToolName, ToolRequestOrdinal,
        test_support::{command_id, model_call_id, session_id, tool_request_id, turn_id},
    };

    const TEST_TASK: &str = "inspect delegated work";

    #[test]
    fn s18_spawn_capacity_rejection_carries_the_fixed_direct_child_limit() {
        assert_eq!(
            DelegatedSpawnRejection::ActiveDirectChildLimit.limit(),
            MAX_ACTIVE_DIRECT_CHILDREN
        );
    }

    fn named_request(session: u128, name: &str, arguments: serde_json::Value) -> ToolRequest {
        let identity_offset = match name {
            SPAWN_SESSION_TOOL_NAME => 100,
            AWAIT_SESSION_TOOL_NAME => 200,
            SEND_SESSION_MESSAGE_TOOL_NAME => 300,
            _ => 400,
        };
        named_request_with_id(
            session,
            name,
            arguments,
            tool_request_id(session + identity_offset),
        )
    }
    fn named_request_with_id(
        session: u128,
        name: &str,
        arguments: serde_json::Value,
        request_id: ToolRequestId,
    ) -> ToolRequest {
        ToolRequest::from_model_proposal(
            request_id,
            session_id(session),
            turn_id(session + 10),
            model_call_id(session + 20),
            ToolRequestOrdinal::from_u32(0),
            ToolCallProposal::new(
                ToolName::try_new(name.into()).expect("valid name"),
                NormalizedToolArguments::try_from_provider_text(arguments.to_string())
                    .expect("valid arguments"),
            ),
        )
    }
    fn request(session: u128) -> ToolRequest {
        request_with_policy(session, ChildRelationshipPolicy::Background)
    }
    fn request_with_policy(session: u128, policy: ChildRelationshipPolicy) -> ToolRequest {
        named_request(
            session,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": relationship_argument(policy),
                "task": TEST_TASK,
            }),
        )
    }
    fn await_request(session: u128, child: SessionId, mode: DelegationWaitMode) -> ToolRequest {
        named_request(
            session,
            AWAIT_SESSION_TOOL_NAME,
            serde_json::json!({
                "child_session_id": child.as_uuid().to_string(), "mode": wait_mode_argument(mode),
            }),
        )
    }
    fn message_request(session: u128, peer: SessionId, value: &DelegationContent) -> ToolRequest {
        named_request(
            session,
            SEND_SESSION_MESSAGE_TOOL_NAME,
            serde_json::json!({
                "content": value.as_str(), "peer_session_id": peer.as_uuid().to_string(),
            }),
        )
    }
    fn content(value: &str) -> DelegationContent {
        DelegationContent::try_new(value.into()).expect("nonempty bounded content")
    }
    fn terminal_provenance(
        session: u128,
        turn: TurnId,
        kind: TerminalChildTurnKind,
        reason: DelegationOutcomeReason,
        content: Option<&DelegationContent>,
    ) -> DelegationProvenance {
        DelegationProvenance::from_terminal_child(TerminalChildTurn {
            session: session_id(session),
            turn,
            kind,
            reason,
            result_digest: content.map(delegation_content_digest),
        })
    }
    fn parent_termination_provenance(
        parent: u128,
        turn: TurnId,
        command: DurableCommandId,
        reason: DelegationOutcomeReason,
    ) -> DelegationProvenance {
        let (kind, scope) = match reason {
            DelegationOutcomeReason::ParentStopped { scope } => {
                (ParentTerminationKind::Stopped, scope)
            }
            DelegationOutcomeReason::ParentCancelled { scope } => {
                (ParentTerminationKind::Cancelled, scope)
            }
            DelegationOutcomeReason::ChildCompleted
            | DelegationOutcomeReason::ChildExecutionFailed
            | DelegationOutcomeReason::ChildResultUnavailable
            | DelegationOutcomeReason::ChildCancelled => {
                panic!("only parent termination has parent command authority")
            }
        };
        DelegationProvenance::from_parent_termination(ParentTerminationAuthority {
            parent: session_id(parent),
            turn,
            command,
            kind,
            scope,
        })
    }

    #[derive(Clone, Copy)]
    struct CascadeRelationshipFacts {
        spawning_request: u128,
        parent: u128,
        child: u128,
        policy: ChildRelationshipPolicy,
    }

    struct CascadeRelationshipFixture {
        relation: SessionDelegation,
        spawning_turn: TurnId,
    }

    fn cascade_relationship(facts: CascadeRelationshipFacts) -> CascadeRelationshipFixture {
        let request = named_request_with_id(
            facts.parent,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": relationship_argument(facts.policy),
                "task": TEST_TASK,
            }),
            tool_request_id(facts.spawning_request),
        );
        let spawning_turn = request.turn();
        let relation = SessionDelegation::spawn(&request, session_id(facts.child), facts.policy)
            .expect("cascade relationship fixture is valid");
        CascadeRelationshipFixture {
            relation,
            spawning_turn,
        }
    }

    #[derive(Clone, Copy)]
    struct CascadeAuthorityFacts {
        parent: SessionId,
        kind: ParentTerminationKind,
        scope: DescendantTerminationScope,
    }

    fn cascade_authority(facts: CascadeAuthorityFacts) -> ParentTerminationAuthority {
        ParentTerminationAuthority {
            parent: facts.parent,
            turn: turn_id(900),
            command: command_id(901),
            kind: facts.kind,
            scope: facts.scope,
        }
    }

    #[test]
    fn assistant_text_projection_preserves_exact_part_order_without_separator() {
        let first = crate::AssistantText::try_new("first".into()).expect("first part");
        let second = crate::AssistantText::try_new("second".into()).expect("second part");
        let expected = format!("{}{}", first.as_str(), second.as_str());
        let projected = DelegationContent::from_assistant_text(&[first, second])
            .expect("ordered nonempty parts project");

        assert_eq!(projected.as_str(), expected);
    }

    #[test]
    fn empty_assistant_text_projection_is_typed_content_failure() {
        let error = DelegationContent::from_assistant_text(&[])
            .expect_err("successful empty output has no delegation content");

        assert_eq!(
            error,
            DelegationContentError::Invalid(crate::NonEmptyUnicodeTextFailure::Empty)
        );
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

    /// S18 / INV-010: spawn arguments cannot authorize another relationship policy.
    #[test]
    fn s18_inv010_spawn_rejects_mismatched_relationship_arguments() {
        let error = SessionDelegation::spawn(
            &request(2),
            session_id(3),
            ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        )
        .expect_err("background arguments cannot authorize a bound relationship");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidToolRequestPurpose
        );
    }

    /// S18 / INV-010: malformed task content keeps its precise admission failure.
    #[test]
    fn s18_inv010_spawn_preserves_invalid_task_content_failure() {
        let invalid_task = named_request(
            2,
            SPAWN_SESSION_TOOL_NAME,
            serde_json::json!({
                "relationship": relationship_argument(ChildRelationshipPolicy::Background),
                "task": "",
            }),
        );
        let error = SessionDelegation::spawn(
            &invalid_task,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect_err("empty task is not admitted");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidTaskContent(DelegationContentError::Invalid(
                crate::NonEmptyUnicodeTextFailure::Empty
            ))
        );
        assert!(std::error::Error::source(&error).is_some());
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
            .register_wait(
                &await_request(2, session_id(3), DelegationWaitMode::Foreground),
                DelegationWaitMode::Foreground,
            )
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
            .register_wait(
                &await_request(2, session_id(3), DelegationWaitMode::Background),
                DelegationWaitMode::Background,
            )
            .expect("parent registers background delivery");
        assert_eq!(registration.foreground_subject(), None);
    }

    /// S18 / INV-010: await arguments must authorize the selected delivery mode.
    #[test]
    fn s18_inv010_wait_registration_requires_exact_mode() {
        let spawning_request = request(2);
        let relation = SessionDelegation::spawn(
            &spawning_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let mismatched = await_request(2, session_id(3), DelegationWaitMode::Background);
        let error = relation
            .register_wait(&mismatched, DelegationWaitMode::Foreground)
            .expect_err("request mode cannot authorize a different wait mode");
        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidToolRequestPurpose
        );
    }

    /// S18 / INV-010: the spawning request cannot masquerade as a later await request.
    #[test]
    fn s18_inv010_wait_registration_requires_distinct_request_identity() {
        let spawning_request = request(2);
        let colliding_request = named_request_with_id(
            2,
            AWAIT_SESSION_TOOL_NAME,
            serde_json::json!({
                "child_session_id": session_id(3).as_uuid().to_string(),
                "mode": wait_mode_argument(DelegationWaitMode::Foreground),
            }),
            spawning_request.id(),
        );
        let relation = SessionDelegation::spawn(
            &spawning_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let error = relation
            .register_wait(&colliding_request, DelegationWaitMode::Foreground)
            .expect_err("spawn identity cannot be reused for a wait");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18 / INV-010: message direction is derived from exact request ownership and unrelated senders fail closed.
    #[test]
    fn s18_inv010_messages_are_relation_directed_and_request_provenanced() {
        let parent_request = request(2);
        let parent_content = content("parent message");
        let child_content = content("child message");
        let unrelated_content = content("unrelated");
        let parent_message_request = message_request(2, session_id(3), &parent_content);
        let child_request = message_request(3, session_id(2), &child_content);
        let unrelated_request = message_request(6, session_id(3), &unrelated_content);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let (relation, _) = relation
            .deliver_message(
                &parent_message_request,
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(7)),
                parent_content,
            )
            .expect("parent sends to child");
        let (relation, _) = relation
            .deliver_message(
                &child_request,
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(8)),
                child_content,
            )
            .expect("child sends to parent");
        let error = relation
            .clone()
            .deliver_message(
                &unrelated_request,
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(9)),
                unrelated_content,
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

    /// S18 / INV-010: the spawning request identity cannot authorize a later message.
    #[test]
    fn s18_inv010_message_rejects_spawning_request_identity() {
        let spawning_request = request(2);
        let message_content = content("message");
        let colliding_request = named_request_with_id(
            2,
            SEND_SESSION_MESSAGE_TOOL_NAME,
            serde_json::json!({
                "content": message_content.as_str(),
                "peer_session_id": session_id(3).as_uuid().to_string(),
            }),
            spawning_request.id(),
        );
        let relation = SessionDelegation::spawn(
            &spawning_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let error = relation
            .deliver_message(
                &colliding_request,
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(7)),
                message_content,
            )
            .expect_err("spawn identity cannot be reused for a message");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18 / INV-012: one sending request names exactly one immutable delivery.
    #[test]
    fn s18_inv012_message_request_replay_is_exact() {
        let parent_request = request(2);
        let first_content = content("first content");
        let parent_message_request = message_request(2, session_id(3), &first_content);
        let message_id = DelegationMessageId::from_uuid(uuid::Uuid::from_u128(7));
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let (relation, event) = relation
            .deliver_message(&parent_message_request, message_id, first_content.clone())
            .expect("first identity is admitted");
        let (replayed, replayed_event) = relation
            .clone()
            .deliver_message(
                &parent_message_request,
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(8)),
                first_content.clone(),
            )
            .expect("equal replay returns the persisted message identity");
        let argument_error = relation
            .clone()
            .deliver_message(
                &parent_message_request,
                message_id,
                content("conflicting content"),
            )
            .expect_err("request content cannot authorize different content");
        assert_eq!(replayed.events(), relation.events());
        assert_eq!(replayed_event, event);
        assert_eq!(
            argument_error.failure(),
            DelegationTransitionFailure::InvalidToolRequestPurpose
        );
    }

    /// S19 / INV-010: parent-alone evaluates no relationship and creates no descendant authority.
    #[test]
    fn s19_inv010_cascade_parent_alone_returns_no_dispositions() {
        let relationship = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 10,
            parent: 1,
            child: 2,
            policy: ChildRelationshipPolicy::Background,
        });
        let scope = DescendantTerminationScope::ParentAlone;
        let authority = cascade_authority(CascadeAuthorityFacts {
            parent: relationship.relation.parent(),
            kind: ParentTerminationKind::Stopped,
            scope,
        });

        let evaluation = evaluate_delegation_cascade(&[relationship.relation], scope, authority)
            .expect("parent-alone has no descendant work");

        assert_eq!(evaluation.dispositions(), []);
    }

    /// S19 / INV-010: every selected edge in a mixed tree receives one explicit typed disposition.
    #[test]
    fn s19_inv010_cascade_mixed_tree_has_no_silent_orphan_or_kill() {
        let background = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 10,
            parent: 1,
            child: 2,
            policy: ChildRelationshipPolicy::Background,
        });
        let keep_running = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 20,
            parent: 1,
            child: 3,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::KeepRunning,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        });
        let stopped = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 30,
            parent: 1,
            child: 4,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::KeepRunning,
            },
        });
        let cancelled = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 40,
            parent: 1,
            child: 5,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Cancel,
                on_parent_cancelled: BoundChildAction::KeepRunning,
            },
        });
        let inventory = vec![
            background.relation.clone(),
            keep_running.relation.clone(),
            stopped.relation.clone(),
            cancelled.relation.clone(),
        ];
        let scope = DescendantTerminationScope::ParentAndDescendants;
        let authority = cascade_authority(CascadeAuthorityFacts {
            parent: background.relation.parent(),
            kind: ParentTerminationKind::Stopped,
            scope,
        });

        let evaluation = evaluate_delegation_cascade(&inventory, scope, authority)
            .expect("mixed tree evaluates");
        let dispositions = evaluation.dispositions();

        assert_eq!(dispositions.len(), 4);
        assert_eq!(
            dispositions[0].spawning_request(),
            background.relation.spawning_request()
        );
        assert_eq!(
            dispositions[1].spawning_request(),
            keep_running.relation.spawning_request()
        );
        assert_eq!(
            dispositions[2].spawning_request(),
            stopped.relation.spawning_request()
        );
        assert_eq!(
            dispositions[3].spawning_request(),
            cancelled.relation.spawning_request()
        );
        assert!(matches!(
            dispositions[0].outcome(),
            DelegationOutcome::ContinueRunning { .. }
        ));
        assert!(matches!(
            dispositions[1].outcome(),
            DelegationOutcome::ContinueRunning { .. }
        ));
        assert!(matches!(
            dispositions[2].outcome(),
            DelegationOutcome::ChildStopped { .. }
        ));
        assert!(matches!(
            dispositions[3].outcome(),
            DelegationOutcome::ChildCancelled { .. }
        ));
    }

    /// S19 / INV-010: surviving background and bound edges prune their descendants.
    #[test]
    fn s19_inv010_cascade_background_survival_prunes_nested_work() {
        let background = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 10,
            parent: 1,
            child: 2,
            policy: ChildRelationshipPolicy::Background,
        });
        let bound_keep_running = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 20,
            parent: 1,
            child: 4,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::KeepRunning,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        });
        let nested_background = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 30,
            parent: 2,
            child: 3,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        });
        let nested_bound = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 40,
            parent: 4,
            child: 5,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        });
        let inventory = vec![
            background.relation.clone(),
            bound_keep_running.relation.clone(),
            nested_background.relation.clone(),
            nested_bound.relation.clone(),
        ];
        let scope = DescendantTerminationScope::ParentAndDescendants;
        let authority = cascade_authority(CascadeAuthorityFacts {
            parent: background.relation.parent(),
            kind: ParentTerminationKind::Stopped,
            scope,
        });

        let evaluation = evaluate_delegation_cascade(&inventory, scope, authority)
            .expect("background and bound keep-running branches survive");
        let dispositions = evaluation.dispositions();

        assert_eq!(dispositions.len(), 2);
        assert_eq!(
            dispositions[0].spawning_request(),
            background.relation.spawning_request()
        );
        assert_eq!(
            dispositions[1].spawning_request(),
            bound_keep_running.relation.spawning_request()
        );
        assert!(matches!(
            dispositions[0].outcome(),
            DelegationOutcome::ContinueRunning {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants
                },
                ..
            }
        ));
        assert!(matches!(
            dispositions[1].outcome(),
            DelegationOutcome::ContinueRunning {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants
                },
                ..
            }
        ));
    }

    /// S19 / INV-010: recursive evaluation switches to each selected stop or cancel policy arm.
    #[test]
    fn s19_inv010_cascade_nested_propagation_uses_selected_policy_arms() {
        let cancelled_by_stop = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 20,
            parent: 1,
            child: 2,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Cancel,
                on_parent_cancelled: BoundChildAction::KeepRunning,
            },
        });
        let stopped_by_cancel = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 10,
            parent: 2,
            child: 3,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::KeepRunning,
                on_parent_cancelled: BoundChildAction::Stop,
            },
        });
        let inventory = vec![
            stopped_by_cancel.relation.clone(),
            cancelled_by_stop.relation.clone(),
        ];
        let scope = DescendantTerminationScope::ParentAndDescendants;
        let authority = cascade_authority(CascadeAuthorityFacts {
            parent: cancelled_by_stop.relation.parent(),
            kind: ParentTerminationKind::Stopped,
            scope,
        });

        let evaluation = evaluate_delegation_cascade(&inventory, scope, authority)
            .expect("nested bound cascade evaluates");
        let dispositions = evaluation.dispositions();

        assert_eq!(dispositions.len(), 2);
        assert!(matches!(
            dispositions[0].outcome(),
            DelegationOutcome::ChildStopped {
                reason: DelegationOutcomeReason::ParentCancelled {
                    scope: DescendantTerminationScope::ParentAndDescendants
                },
                ..
            }
        ));
        assert!(matches!(
            dispositions[1].outcome(),
            DelegationOutcome::ChildCancelled {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants
                },
                ..
            }
        ));
        assert_eq!(
            outcome_provenance(dispositions[0].outcome()).parent_command(),
            Some((
                stopped_by_cancel.relation.parent(),
                stopped_by_cancel.spawning_turn,
                authority.command()
            ))
        );
        assert_eq!(
            outcome_provenance(dispositions[1].outcome()).parent_command(),
            Some((
                cancelled_by_stop.relation.parent(),
                cancelled_by_stop.spawning_turn,
                authority.command()
            ))
        );
    }

    /// S19 / INV-010: parent stop yields stopped outcome, reason, and exact command provenance.
    #[test]
    fn s19_inv010_cascade_stop_preserves_typed_reason_and_provenance() {
        let relationship = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 10,
            parent: 1,
            child: 2,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        });
        let scope = DescendantTerminationScope::ParentAndDescendants;
        let authority = cascade_authority(CascadeAuthorityFacts {
            parent: relationship.relation.parent(),
            kind: ParentTerminationKind::Stopped,
            scope,
        });

        let evaluation = evaluate_delegation_cascade(
            std::slice::from_ref(&relationship.relation),
            scope,
            authority,
        )
        .expect("stop cascade evaluates");
        let disposition = &evaluation.dispositions()[0];

        assert_eq!(disposition.parent(), relationship.relation.parent());
        assert_eq!(disposition.child(), relationship.relation.child());
        assert!(matches!(
            disposition.outcome(),
            DelegationOutcome::ChildStopped {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants
                },
                ..
            }
        ));
        assert_eq!(
            outcome_provenance(disposition.outcome()).parent_command(),
            Some((
                relationship.relation.parent(),
                relationship.spawning_turn,
                authority.command()
            ))
        );
    }

    /// S19 / INV-010: parent cancellation yields cancelled outcome, reason, and exact command provenance.
    #[test]
    fn s19_inv010_cascade_cancel_preserves_typed_reason_and_provenance() {
        let relationship = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 10,
            parent: 1,
            child: 2,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        });
        let scope = DescendantTerminationScope::ParentAndDescendants;
        let authority = cascade_authority(CascadeAuthorityFacts {
            parent: relationship.relation.parent(),
            kind: ParentTerminationKind::Cancelled,
            scope,
        });

        let evaluation = evaluate_delegation_cascade(
            std::slice::from_ref(&relationship.relation),
            scope,
            authority,
        )
        .expect("cancel cascade evaluates");
        let disposition = &evaluation.dispositions()[0];

        assert!(matches!(
            disposition.outcome(),
            DelegationOutcome::ChildCancelled {
                reason: DelegationOutcomeReason::ParentCancelled {
                    scope: DescendantTerminationScope::ParentAndDescendants
                },
                ..
            }
        ));
        assert_eq!(
            outcome_provenance(disposition.outcome()).parent_command(),
            Some((
                relationship.relation.parent(),
                relationship.spawning_turn,
                authority.command()
            ))
        );
    }

    /// S19 / INV-012: applying and reevaluating an equal cascade returns the same disposition.
    #[test]
    fn s19_inv012_cascade_equal_replay_is_idempotent() {
        let relationship = cascade_relationship(CascadeRelationshipFacts {
            spawning_request: 10,
            parent: 1,
            child: 2,
            policy: ChildRelationshipPolicy::Bound {
                on_parent_stopped: BoundChildAction::Stop,
                on_parent_cancelled: BoundChildAction::Cancel,
            },
        });
        let scope = DescendantTerminationScope::ParentAndDescendants;
        let authority = cascade_authority(CascadeAuthorityFacts {
            parent: relationship.relation.parent(),
            kind: ParentTerminationKind::Stopped,
            scope,
        });
        let first = evaluate_delegation_cascade(
            std::slice::from_ref(&relationship.relation),
            scope,
            authority,
        )
        .expect("first cascade evaluates");
        let recorded = relationship
            .relation
            .record_outcome(first.dispositions()[0].outcome().clone())
            .expect("first disposition records");

        let replay = evaluate_delegation_cascade(&[recorded], scope, authority)
            .expect("equal cascade replay evaluates");

        assert_eq!(replay, first, "equal cascade replay must remain stable");
    }

    /// S19 / INV-010: a bound keep-running policy records a typed no-change event.
    #[test]
    fn s19_inv010_bound_keep_running_records_no_change() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::KeepRunning,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let parent_request = request_with_policy(2, policy);
        let relation = SessionDelegation::spawn(&parent_request, session_id(3), policy)
            .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ContinueRunning {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                },
                provenance: parent_termination_provenance(
                    2,
                    parent_request.turn(),
                    command_id(5),
                    DelegationOutcomeReason::ParentStopped {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                ),
            })
            .expect("no-change evaluation is recorded");
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(relation.events().len(), 2);
    }

    /// S19 / INV-012: equal replay of one evaluated edge records no second disposition.
    #[test]
    fn s19_inv012_continue_running_replay_is_idempotent() {
        let parent_request = request(2);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let outcome = DelegationOutcome::ContinueRunning {
            reason: DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
            provenance: parent_termination_provenance(
                2,
                parent_request.turn(),
                command_id(5),
                DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                },
            ),
        };
        let relation = relation
            .record_outcome(outcome.clone())
            .expect("first disposition records");
        let replayed = relation
            .clone()
            .record_outcome(outcome)
            .expect("equal replay returns the relation");

        assert_eq!(replayed.events(), relation.events());
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
        let returned_content = content("delivered result");
        let mismatched_content = content("fabricated result");
        let returned_provenance = terminal_provenance(
            3,
            child_request.turn(),
            TerminalChildTurnKind::Returned,
            DelegationOutcomeReason::ChildCompleted,
            Some(&returned_content),
        );
        let mismatch = relation
            .clone()
            .record_outcome(DelegationOutcome::ResultReturned {
                content: mismatched_content,
                reason: DelegationOutcomeReason::ChildCompleted,
                provenance: returned_provenance,
            })
            .expect_err("terminal evidence binds exact returned content");
        let relation = relation
            .record_outcome(DelegationOutcome::ResultReturned {
                content: returned_content,
                reason: DelegationOutcomeReason::ChildCompleted,
                provenance: returned_provenance,
            })
            .expect("child result terminalizes");
        assert_eq!(
            mismatch.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(relation.events().len(), 2);
        let error = relation
            .record_outcome(DelegationOutcome::ChildFailed {
                reason: DelegationOutcomeReason::ChildExecutionFailed,
                provenance: terminal_provenance(
                    3,
                    turn_id(6),
                    TerminalChildTurnKind::Failed,
                    DelegationOutcomeReason::ChildExecutionFailed,
                    None,
                ),
            })
            .expect_err("terminal relation never reopens");
        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::AlreadyTerminal
        );
    }

    /// S18 / INV-010: child failure evidence cannot be relabeled with another reason.
    #[test]
    fn s18_inv010_child_failure_preserves_exact_evidence_reason() {
        let parent_request = request(2);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let result_unavailable_provenance = terminal_provenance(
            3,
            turn_id(6),
            TerminalChildTurnKind::Failed,
            DelegationOutcomeReason::ChildResultUnavailable,
            None,
        );
        let error = relation
            .record_outcome(DelegationOutcome::ChildFailed {
                reason: DelegationOutcomeReason::ChildExecutionFailed,
                provenance: result_unavailable_provenance,
            })
            .expect_err("failure evidence retains its exact reason");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
    }

    /// S18 / INV-010: a parent may register delivery after the child has already completed.
    #[test]
    fn s18_inv010_terminal_result_still_accepts_a_late_wait() {
        let parent_request = request(2);
        let child_request = request(3);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let returned_content = content("already delivered");
        let returned_provenance = terminal_provenance(
            3,
            child_request.turn(),
            TerminalChildTurnKind::Returned,
            DelegationOutcomeReason::ChildCompleted,
            Some(&returned_content),
        );
        let relation = relation
            .record_outcome(DelegationOutcome::ResultReturned {
                content: returned_content,
                reason: DelegationOutcomeReason::ChildCompleted,
                provenance: returned_provenance,
            })
            .expect("child result terminalizes");
        let registration = relation
            .register_wait(
                &await_request(2, session_id(3), DelegationWaitMode::Foreground),
                DelegationWaitMode::Foreground,
            )
            .expect("late wait registers against the existing result");

        assert_eq!(registration.child(), relation.child());
        assert_eq!(registration.mode(), DelegationWaitMode::Foreground);
    }

    /// S18 / INV-010: terminal child disposition does not close either messaging direction.
    #[test]
    fn s18_inv010_messages_remain_available_after_child_terminalizes() {
        let parent_request = request(2);
        let parent_content = content("parent follow-up");
        let child_content = content("child follow-up");
        let parent_message_request = message_request(2, session_id(3), &parent_content);
        let child_request = message_request(3, session_id(2), &child_content);
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ChildCancelled {
                reason: DelegationOutcomeReason::ChildCancelled,
                provenance: terminal_provenance(
                    3,
                    child_request.turn(),
                    TerminalChildTurnKind::Cancelled,
                    DelegationOutcomeReason::ChildCancelled,
                    None,
                ),
            })
            .expect("child records its own cancellation");
        let (relation, _) = relation
            .deliver_message(
                &parent_message_request,
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(7)),
                parent_content,
            )
            .expect("terminal relation remains messageable");
        let (relation, _) = relation
            .deliver_message(
                &child_request,
                DelegationMessageId::from_uuid(uuid::Uuid::from_u128(8)),
                child_content,
            )
            .expect("terminal child remains messageable");
        let message = relation.events()[2]
            .message()
            .expect("third event is the follow-up message");
        let reply = relation.events()[3]
            .message()
            .expect("fourth event is the child reply");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(
            message.direction(),
            DelegationMessageDirection::ParentToChild
        );
        assert_eq!(reply.direction(), DelegationMessageDirection::ChildToParent);
    }

    /// S19 / INV-010: a child may report its own cancellation with its exact turn.
    #[test]
    fn s19_inv010_child_cancel_records_child_turn_reason_and_provenance() {
        let parent_request = request(2);
        let child_request = request(3);
        let provenance = terminal_provenance(
            3,
            child_request.turn(),
            TerminalChildTurnKind::Cancelled,
            DelegationOutcomeReason::ChildCancelled,
            None,
        );
        let relation = SessionDelegation::spawn(
            &parent_request,
            session_id(3),
            ChildRelationshipPolicy::Background,
        )
        .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ChildCancelled {
                reason: DelegationOutcomeReason::ChildCancelled,
                provenance,
            })
            .expect("child records its own cancellation");

        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(
            provenance.child_turn(),
            Some((relation.child(), child_request.turn()))
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
                provenance: parent_termination_provenance(
                    2,
                    parent_request.turn(),
                    command_id(5),
                    DelegationOutcomeReason::ParentStopped {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                ),
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
                provenance: parent_termination_provenance(
                    2,
                    parent_request.turn(),
                    command_id(5),
                    DelegationOutcomeReason::ParentCancelled {
                        scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                ),
            })
            .expect("background relationship records survival");
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Active);
        assert_eq!(relation.events().len(), 2);
    }

    /// S19 / INV-010: a bound child accepts only its chosen parent-stop action.
    #[test]
    fn s19_inv010_bound_child_follows_parent_stop_policy() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let parent_request = request_with_policy(2, policy);
        let command = command_id(5);
        let provenance = parent_termination_provenance(
            2,
            parent_request.turn(),
            command,
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        );
        let relation = SessionDelegation::spawn(&parent_request, session_id(3), policy)
            .expect("distinct child");
        let relation = relation
            .record_outcome(DelegationOutcome::ChildStopped {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                },
                provenance,
            })
            .expect("bound stop policy authorizes stopped outcome");
        assert_eq!(relation.lifecycle(), DelegationLifecycle::Terminal);
        assert_eq!(
            provenance.parent_command(),
            Some((parent_request.session(), parent_request.turn(), command))
        );
    }

    /// S19 / INV-010: a cancellation command cannot authorize a stop disposition.
    #[test]
    fn s19_inv010_parent_termination_authority_matches_exact_command_kind() {
        let policy = ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        };
        let parent_request = request_with_policy(2, policy);
        let cancellation_authority = parent_termination_provenance(
            2,
            parent_request.turn(),
            command_id(5),
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        );
        let relation = SessionDelegation::spawn(&parent_request, session_id(3), policy)
            .expect("distinct child");
        let error = relation
            .record_outcome(DelegationOutcome::ChildStopped {
                reason: DelegationOutcomeReason::ParentStopped {
                    scope: DescendantTerminationScope::ParentAndDescendants,
                },
                provenance: cancellation_authority,
            })
            .expect_err("cancellation authority cannot prove a parent stop");

        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::InvalidProvenance
        );
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
                provenance: parent_termination_provenance(
                    2,
                    parent_request.turn(),
                    command_id(5),
                    DelegationOutcomeReason::ParentStopped {
                        scope: DescendantTerminationScope::ParentAlone,
                    },
                ),
            })
            .expect_err("parent-alone does not evaluate descendants");
        assert_eq!(
            error.failure(),
            DelegationTransitionFailure::OutcomeReasonMismatch
        );
    }
}

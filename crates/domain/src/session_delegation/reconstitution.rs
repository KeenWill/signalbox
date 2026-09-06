//! Session delegation reconstitution for `docs/spec/sessions-and-transcript.md`.

use super::event::{DelegationEvent, DelegationEventOrdinal, DelegationLifecycle};
use super::message::{DelegationMessage, DelegationMessageDirection};
use super::outcome::{DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason};
use super::provenance::{
    DelegationProvenance, DelegationProvenanceKind, DelegationToolRequestPurpose,
};
use super::relation::SessionDelegation;
use super::request::DelegatedSpawnRequest;
use super::vocabulary::{
    BoundChildAction, ChildRelationshipPolicy, DescendantTerminationScope, ParentTerminationKind,
};
use crate::{DelegationMessageId, DurableCommandId, SessionId, ToolRequest, ToolRequestId, TurnId};
use std::collections::HashSet;

/// Complete canonical facts required to reconstitute one stored relation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegationReconstitutionInput {
    pub(super) spawning_request: DelegatedSpawnRequest,
    pub(super) child: SessionId,
    pub(super) child_turn: TurnId,
    pub(super) events: Vec<DelegationEvent>,
}

impl SessionDelegationReconstitutionInput {
    pub fn new(
        spawning_request: DelegatedSpawnRequest,
        child: SessionId,
        child_turn: TurnId,
        events: Vec<DelegationEvent>,
    ) -> Self {
        Self {
            spawning_request,
            child,
            child_turn,
            events,
        }
    }

    pub const fn spawning_request(&self) -> &DelegatedSpawnRequest {
        &self.spawning_request
    }

    pub const fn child(&self) -> SessionId {
        self.child
    }

    pub const fn child_turn(&self) -> TurnId {
        self.child_turn
    }

    pub fn events(&self) -> &[DelegationEvent] {
        &self.events
    }

    /// Validates the complete ordered history without authorizing a new
    /// spawn, message, outcome, or wait effect.
    pub fn reconstitute(self) -> Result<SessionDelegation, SessionDelegationReconstitutionError> {
        SessionDelegation::reconstitute(self)
    }
}

/// Why stored relationship facts could not form one canonical aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDelegationReconstitutionFailure {
    SameSession,
    MissingSpawnEvent,
    NoncontiguousEventOrdinal,
    InvalidSpawnEvent,
    InvalidMessageProvenance,
    DuplicateMessageIdentity,
    DuplicateMessageRequest,
    DuplicateOutcomeAuthority,
    OutcomeReasonMismatch,
    EventAfterTerminal,
}

/// Rejected reconstitution retaining every canonical input fact unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegationReconstitutionError {
    input: Box<SessionDelegationReconstitutionInput>,
    failure: SessionDelegationReconstitutionFailure,
}

impl SessionDelegationReconstitutionError {
    pub const fn input(&self) -> &SessionDelegationReconstitutionInput {
        &self.input
    }

    pub const fn failure(&self) -> SessionDelegationReconstitutionFailure {
        self.failure
    }

    pub fn into_parts(
        self,
    ) -> (
        SessionDelegationReconstitutionInput,
        SessionDelegationReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

impl std::fmt::Display for SessionDelegationReconstitutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "delegation {} reconstitution failed: {:?}",
            self.input.spawning_request.request().id().as_uuid(),
            self.failure
        )
    }
}

impl std::error::Error for SessionDelegationReconstitutionError {}

pub(super) fn reconstitution_error(
    input: SessionDelegationReconstitutionInput,
    failure: SessionDelegationReconstitutionFailure,
) -> SessionDelegationReconstitutionError {
    SessionDelegationReconstitutionError {
        input: Box::new(input),
        failure,
    }
}

#[derive(Clone, Copy)]
pub(super) struct DelegationRelationIdentity {
    spawning_request: ToolRequestId,
    parent: SessionId,
    child: SessionId,
    child_turn: TurnId,
    policy: ChildRelationshipPolicy,
}

impl DelegationRelationIdentity {
    pub(super) const fn from_relation(relation: &SessionDelegation) -> Self {
        Self {
            spawning_request: relation.spawning_request,
            parent: relation.parent,
            child: relation.child,
            child_turn: relation.child_turn,
            policy: relation.policy,
        }
    }
}

pub(super) fn validate_reconstituted_history(
    input: &SessionDelegationReconstitutionInput,
) -> Result<DelegationLifecycle, SessionDelegationReconstitutionFailure> {
    let identity = DelegationRelationIdentity {
        spawning_request: input.spawning_request.request().id(),
        parent: input.spawning_request.request().session(),
        child: input.child,
        child_turn: input.child_turn,
        policy: input.spawning_request.policy(),
    };
    if identity.parent == identity.child {
        return Err(SessionDelegationReconstitutionFailure::SameSession);
    }
    let Some(first) = input.events.first() else {
        return Err(SessionDelegationReconstitutionFailure::MissingSpawnEvent);
    };
    if first
        != &(DelegationEvent::Spawned {
            ordinal: DelegationEventOrdinal::first(),
            provenance: DelegationProvenance::from_spawn(&input.spawning_request),
        })
    {
        return Err(SessionDelegationReconstitutionFailure::InvalidSpawnEvent);
    }

    let mut lifecycle = DelegationLifecycle::Active;
    let mut seen_message_ids = HashSet::new();
    let mut seen_message_requests = HashSet::new();
    let mut seen_parent_commands = HashSet::new();
    let mut seen_outcome_provenance = HashSet::new();
    let mut seen_child_terminal = false;
    for (index, event) in input.events.iter().enumerate() {
        let expected = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1));
        if expected != Some(event.ordinal().get()) {
            return Err(SessionDelegationReconstitutionFailure::NoncontiguousEventOrdinal);
        }
        match event {
            DelegationEvent::Spawned { .. } if index == 0 => {}
            DelegationEvent::Spawned { .. } => {
                return Err(SessionDelegationReconstitutionFailure::InvalidSpawnEvent);
            }
            DelegationEvent::MessageDelivered { message, .. } => {
                validate_reconstituted_message(
                    identity,
                    message,
                    &mut seen_message_ids,
                    &mut seen_message_requests,
                )?;
            }
            DelegationEvent::OutcomeRecorded { outcome, .. } => {
                lifecycle = validate_reconstituted_outcome(
                    identity,
                    lifecycle,
                    outcome,
                    &mut seen_parent_commands,
                    &mut seen_outcome_provenance,
                    &mut seen_child_terminal,
                )?;
            }
        }
    }
    Ok(lifecycle)
}

fn validate_reconstituted_message(
    relation: DelegationRelationIdentity,
    message: &DelegationMessage,
    seen_ids: &mut HashSet<DelegationMessageId>,
    seen_requests: &mut HashSet<ToolRequestId>,
) -> Result<(), SessionDelegationReconstitutionFailure> {
    let (source, request, purpose) = match message.provenance.kind {
        DelegationProvenanceKind::ToolRequest {
            source_session,
            request,
            purpose,
            ..
        } => (source_session, request, purpose),
        DelegationProvenanceKind::ChildTurn { .. }
        | DelegationProvenanceKind::ParentCommand { .. } => {
            return Err(SessionDelegationReconstitutionFailure::InvalidMessageProvenance);
        }
    };
    let endpoints_match = match message.direction {
        DelegationMessageDirection::ParentToChild => {
            source == relation.parent && message.peer == relation.child
        }
        DelegationMessageDirection::ChildToParent => {
            source == relation.child && message.peer == relation.parent
        }
    };
    if purpose != DelegationToolRequestPurpose::SendMessage
        || request == relation.spawning_request
        || !endpoints_match
    {
        return Err(SessionDelegationReconstitutionFailure::InvalidMessageProvenance);
    }
    if !seen_ids.insert(message.id()) {
        return Err(SessionDelegationReconstitutionFailure::DuplicateMessageIdentity);
    }
    if !seen_requests.insert(request) {
        return Err(SessionDelegationReconstitutionFailure::DuplicateMessageRequest);
    }
    Ok(())
}

fn validate_reconstituted_outcome(
    relation: DelegationRelationIdentity,
    lifecycle: DelegationLifecycle,
    outcome: &DelegationOutcome,
    seen_parent_commands: &mut HashSet<DurableCommandId>,
    seen_provenance: &mut HashSet<DelegationProvenance>,
    seen_child_terminal: &mut bool,
) -> Result<DelegationLifecycle, SessionDelegationReconstitutionFailure> {
    if let Some(authority) = outcome.provenance().parent_command()
        && !seen_parent_commands.insert(authority.command())
    {
        return Err(SessionDelegationReconstitutionFailure::DuplicateOutcomeAuthority);
    }
    if !seen_provenance.insert(outcome.provenance()) {
        return Err(SessionDelegationReconstitutionFailure::DuplicateOutcomeAuthority);
    }
    let records_terminal_evaluation = outcome.kind() == DelegationOutcomeKind::AlreadyTerminal;
    if lifecycle == DelegationLifecycle::Terminal
        && (!records_terminal_evaluation || !*seen_child_terminal)
    {
        return Err(SessionDelegationReconstitutionFailure::EventAfterTerminal);
    }
    if lifecycle == DelegationLifecycle::Active && records_terminal_evaluation {
        return Err(SessionDelegationReconstitutionFailure::OutcomeReasonMismatch);
    }
    if !outcome_matches_relation(relation, outcome) {
        return Err(SessionDelegationReconstitutionFailure::OutcomeReasonMismatch);
    }
    if matches!(
        outcome.kind(),
        DelegationOutcomeKind::ResultReturned
            | DelegationOutcomeKind::ChildFailed
            | DelegationOutcomeKind::ChildStopped
            | DelegationOutcomeKind::ChildCancelled
    ) {
        *seen_child_terminal = true;
    }
    match outcome.kind() {
        DelegationOutcomeKind::ContinueRunning => Ok(DelegationLifecycle::Active),
        DelegationOutcomeKind::ResultReturned
        | DelegationOutcomeKind::ChildFailed
        | DelegationOutcomeKind::ChildStopped
        | DelegationOutcomeKind::ChildCancelled
        | DelegationOutcomeKind::AlreadyTerminal => Ok(DelegationLifecycle::Terminal),
    }
}

pub(super) fn dispatch_matches(
    request: &ToolRequest,
    dispatch: &crate::ToolDispatchAuthority,
) -> bool {
    dispatch.request() == request
}

pub(super) fn has_child_terminal_outcome(relation: &SessionDelegation) -> bool {
    relation
        .events
        .iter()
        .filter_map(DelegationEvent::outcome)
        .any(|outcome| {
            matches!(
                outcome.kind(),
                DelegationOutcomeKind::ResultReturned
                    | DelegationOutcomeKind::ChildFailed
                    | DelegationOutcomeKind::ChildStopped
                    | DelegationOutcomeKind::ChildCancelled
            )
        })
}

pub(super) fn outcome_matches_relation(
    relation: DelegationRelationIdentity,
    outcome: &DelegationOutcome,
) -> bool {
    let reason = outcome.reason();
    let child_matches = || {
        outcome
            .provenance()
            .child_turn()
            .is_some_and(|(child, turn)| child == relation.child && turn == relation.child_turn)
    };
    match outcome.kind() {
        DelegationOutcomeKind::ResultReturned => {
            reason == DelegationOutcomeReason::ChildCompleted && child_matches()
        }
        DelegationOutcomeKind::ChildFailed => {
            matches!(
                reason,
                DelegationOutcomeReason::ChildExecutionFailed
                    | DelegationOutcomeReason::ChildResultUnavailable
            ) && child_matches()
        }
        DelegationOutcomeKind::ChildStopped => {
            parent_outcome_matches(relation, outcome, reason, BoundChildAction::Stop)
        }
        DelegationOutcomeKind::ChildCancelled => {
            if reason == DelegationOutcomeReason::ChildCancelled {
                child_matches()
            } else {
                parent_outcome_matches(relation, outcome, reason, BoundChildAction::Cancel)
            }
        }
        DelegationOutcomeKind::ContinueRunning => {
            parent_outcome_matches(relation, outcome, reason, BoundChildAction::KeepRunning)
        }
        DelegationOutcomeKind::AlreadyTerminal => {
            parent_evaluation_matches(relation, outcome, reason)
        }
    }
}

fn parent_outcome_matches(
    relation: DelegationRelationIdentity,
    outcome: &DelegationOutcome,
    reason: DelegationOutcomeReason,
    expected_action: BoundChildAction,
) -> bool {
    parent_evaluation_matches(relation, outcome, reason)
        && descendant_action(relation.policy, reason) == Some(expected_action)
}

fn parent_evaluation_matches(
    relation: DelegationRelationIdentity,
    outcome: &DelegationOutcome,
    reason: DelegationOutcomeReason,
) -> bool {
    match (outcome.provenance().kind, reason) {
        (
            DelegationProvenanceKind::ParentCommand { authority },
            DelegationOutcomeReason::ParentStopped {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => {
            authority.parent() == relation.parent
                && authority.kind() == ParentTerminationKind::Stopped
                && authority.scope() == DescendantTerminationScope::ParentAndDescendants
        }
        (
            DelegationProvenanceKind::ParentCommand { authority },
            DelegationOutcomeReason::ParentCancelled {
                scope: DescendantTerminationScope::ParentAndDescendants,
            },
        ) => {
            authority.parent() == relation.parent
                && authority.kind() == ParentTerminationKind::Cancelled
                && authority.scope() == DescendantTerminationScope::ParentAndDescendants
        }
        _ => false,
    }
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

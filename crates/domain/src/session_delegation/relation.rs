//! Session delegation relation for `docs/spec/sessions-and-transcript.md`.

use super::content::DelegationContent;
use super::event::{DelegationEvent, DelegationEventOrdinal, DelegationLifecycle};
use super::message::{DelegationMessage, DelegationMessageDirection};
use super::outcome::{DelegationOutcome, DelegationOutcomeKind};
use super::provenance::DelegationProvenance;
use super::reconstitution::{
    DelegationRelationIdentity, SessionDelegationReconstitutionError,
    SessionDelegationReconstitutionInput, dispatch_matches, has_child_terminal_outcome,
    outcome_matches_relation, reconstitution_error, validate_reconstituted_history,
};
use super::request::{DelegatedSpawnRequest, DelegationAwaitRequest, DelegationMessageRequest};
use super::vocabulary::{
    BoundChildAction, ChildRelationshipPolicy, DescendantTerminationScope,
    ParentTerminationAuthority, ParentTerminationKind,
};
use super::wait::DelegationWait;
use crate::{DelegationMessageId, SessionCreationProvenance, SessionId, ToolRequestId, TurnId};

/// One exact parent/child relationship keyed by its spawning request.
///
/// Construction is intentionally sealed inside this module until the
/// persistence stack can admit a spawn from the complete, locked parent
/// relationship inventory. Callers cannot substitute a count or partial slice
/// for that admission proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDelegation {
    pub(super) spawning_request: ToolRequestId,
    pub(super) parent: SessionId,
    pub(super) child: SessionId,
    pub(super) child_turn: TurnId,
    task: DelegationContent,
    pub(super) policy: ChildRelationshipPolicy,
    lifecycle: DelegationLifecycle,
    pub(super) events: Vec<DelegationEvent>,
}

impl SessionDelegation {
    pub(super) fn reconstitute(
        input: SessionDelegationReconstitutionInput,
    ) -> Result<Self, SessionDelegationReconstitutionError> {
        let lifecycle = match validate_reconstituted_history(&input) {
            Ok(lifecycle) => lifecycle,
            Err(failure) => return Err(reconstitution_error(input, failure)),
        };
        let SessionDelegationReconstitutionInput {
            spawning_request,
            child,
            child_turn,
            events,
        } = input;
        let DelegatedSpawnRequest {
            request,
            task,
            policy,
        } = spawning_request;
        Ok(Self {
            spawning_request: request.id(),
            parent: request.session(),
            child,
            child_turn,
            task,
            policy,
            lifecycle,
            events,
        })
    }

    #[cfg(test)]
    pub(super) fn spawn_fixture(
        spawning_request: DelegatedSpawnRequest,
        child: SessionId,
        child_turn: TurnId,
    ) -> Result<Self, DelegationTransitionError> {
        let parent = spawning_request.request().session();
        if parent == child {
            let request_id = spawning_request.request().id();
            return Err(DelegationTransitionError {
                spawning_request: request_id,
                failure: DelegationTransitionFailure::SameSession,
                rejected: Some(Box::new(RejectedDelegationTransition::Spawn {
                    request: spawning_request,
                    child,
                    child_turn,
                })),
            });
        }
        let provenance = DelegationProvenance::from_spawn(&spawning_request);
        Ok(Self {
            spawning_request: spawning_request.request().id(),
            parent,
            child,
            child_turn,
            task: spawning_request.task().clone(),
            policy: spawning_request.policy(),
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

    pub const fn child_turn(&self) -> TurnId {
        self.child_turn
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

    /// Returns the child session's immutable delegated/no-ancestry provenance.
    pub const fn child_creation_provenance(&self) -> SessionCreationProvenance {
        SessionCreationProvenance::delegated(self.spawning_request)
    }

    pub fn register_wait(
        &self,
        awaiting_request: &DelegationAwaitRequest,
        dispatch: &crate::ToolDispatchAuthority,
    ) -> Result<DelegationWait, DelegationTransitionError> {
        if !dispatch_matches(awaiting_request.request(), dispatch)
            || awaiting_request.request().session() != self.parent
            || awaiting_request.request().id() == self.spawning_request
            || awaiting_request.child() != self.child
        {
            return Err(self.fail(DelegationTransitionFailure::InvalidProvenance));
        }
        Ok(DelegationWait {
            awaiting_request: awaiting_request.request().id(),
            spawning_request: self.spawning_request,
            parent: self.parent,
            child: self.child,
            mode: awaiting_request.mode(),
        })
    }

    pub fn deliver_message(
        mut self,
        sending_request: DelegationMessageRequest,
        id: DelegationMessageId,
        dispatch: &crate::ToolDispatchAuthority,
    ) -> Result<(Self, DelegationEvent), DelegationTransitionError> {
        if !dispatch_matches(sending_request.request(), dispatch) {
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::InvalidProvenance,
            ));
        }
        let source = sending_request.request().session();
        let direction = if source == self.parent && sending_request.peer() == self.child {
            DelegationMessageDirection::ParentToChild
        } else if source == self.child && sending_request.peer() == self.parent {
            DelegationMessageDirection::ChildToParent
        } else {
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::InvalidProvenance,
            ));
        };
        if sending_request.request().id() == self.spawning_request {
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::InvalidProvenance,
            ));
        }
        let provenance = DelegationProvenance::from_message(&sending_request);
        if let Some(existing) = self
            .events
            .iter()
            .find(|event| {
                event.message().is_some_and(|message| {
                    message
                        .provenance()
                        .tool_request()
                        .is_some_and(|(_, _, request)| request == sending_request.request().id())
                })
            })
            .cloned()
        {
            if existing.message().is_some_and(|message| {
                message.direction() == direction
                    && message.content() == sending_request.content()
                    && message.provenance() == provenance
            }) {
                return Ok((self, existing));
            }
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::ConflictingMessageReplay,
            ));
        }
        if self
            .events
            .iter()
            .any(|event| event.message().is_some_and(|message| message.id() == id))
        {
            return Err(Self::reject_message(
                self,
                sending_request,
                id,
                DelegationTransitionFailure::DuplicateMessageIdentity,
            ));
        }
        let ordinal = match self.next_ordinal() {
            Ok(ordinal)
                if self.lifecycle == DelegationLifecycle::Terminal || ordinal.get() < u64::MAX =>
            {
                ordinal
            }
            Ok(_) => {
                return Err(Self::reject_message(
                    self,
                    sending_request,
                    id,
                    DelegationTransitionFailure::EventOrdinalExhausted,
                ));
            }
            Err(failure) => return Err(Self::reject_message(self, sending_request, id, failure)),
        };
        let event = DelegationEvent::MessageDelivered {
            ordinal,
            message: DelegationMessage {
                id,
                direction,
                peer: sending_request.peer(),
                content: sending_request.content().clone(),
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
        let provenance = outcome.provenance();
        if let Some(authority) = provenance.parent_command()
            && let Some(existing) = self
                .events
                .iter()
                .filter_map(DelegationEvent::outcome)
                .find(|existing| {
                    existing
                        .provenance()
                        .parent_command()
                        .is_some_and(|existing| existing.command() == authority.command())
                })
        {
            if existing == &outcome {
                return Ok(self);
            }
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::DuplicateOutcomeAuthority,
            ));
        }
        if let Some(existing) = self
            .events
            .iter()
            .filter_map(DelegationEvent::outcome)
            .find(|existing| existing.provenance() == provenance)
        {
            if existing == &outcome {
                return Ok(self);
            }
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::DuplicateOutcomeAuthority,
            ));
        }
        let records_terminal_evaluation = outcome.kind() == DelegationOutcomeKind::AlreadyTerminal;
        if self.lifecycle != DelegationLifecycle::Active
            && (!records_terminal_evaluation || !has_child_terminal_outcome(&self))
        {
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::AlreadyTerminal,
            ));
        }
        if self.lifecycle == DelegationLifecycle::Active && records_terminal_evaluation {
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::OutcomeReasonMismatch,
            ));
        }
        if !outcome_matches_relation(DelegationRelationIdentity::from_relation(&self), &outcome) {
            return Err(Self::reject_outcome(
                self,
                outcome,
                DelegationTransitionFailure::OutcomeReasonMismatch,
            ));
        }
        let remains_active = match outcome.kind() {
            DelegationOutcomeKind::ContinueRunning => true,
            DelegationOutcomeKind::ResultReturned
            | DelegationOutcomeKind::ChildFailed
            | DelegationOutcomeKind::ChildStopped
            | DelegationOutcomeKind::ChildCancelled
            | DelegationOutcomeKind::AlreadyTerminal => false,
        };
        let ordinal = match self.next_ordinal() {
            Ok(ordinal) => ordinal,
            Err(failure) => return Err(Self::reject_outcome(self, outcome, failure)),
        };
        self.events
            .push(DelegationEvent::OutcomeRecorded { ordinal, outcome });
        if !remains_active {
            self.lifecycle = DelegationLifecycle::Terminal;
        }
        Ok(self)
    }

    /// Applies one descendant-scoped parent termination using the immutable
    /// relationship policy and current edge lifecycle.
    pub fn record_parent_termination(
        self,
        authority: ParentTerminationAuthority,
    ) -> Result<Self, DelegationTransitionError> {
        if authority.parent() != self.parent {
            return Err(Self::reject_parent_termination(
                self,
                authority,
                DelegationTransitionFailure::InvalidProvenance,
            ));
        }
        if authority.scope() == DescendantTerminationScope::ParentAlone {
            return Err(Self::reject_parent_termination(
                self,
                authority,
                DelegationTransitionFailure::DescendantsNotSelected,
            ));
        }
        if self
            .events
            .iter()
            .filter_map(DelegationEvent::outcome)
            .any(|outcome| outcome.provenance().parent_command() == Some(authority))
        {
            return Ok(self);
        }
        let outcome = if self.lifecycle == DelegationLifecycle::Terminal {
            DelegationOutcome::from_parent_already_terminal(authority)
        } else {
            let action = match self.policy {
                ChildRelationshipPolicy::Background => BoundChildAction::KeepRunning,
                ChildRelationshipPolicy::Bound {
                    on_parent_stopped,
                    on_parent_cancelled,
                } => match authority.kind() {
                    ParentTerminationKind::Stopped => on_parent_stopped,
                    ParentTerminationKind::Cancelled => on_parent_cancelled,
                },
            };
            DelegationOutcome::from_parent_policy(authority, action)
        };
        let Some(outcome) = outcome else {
            return Err(Self::reject_parent_termination(
                self,
                authority,
                DelegationTransitionFailure::DescendantsNotSelected,
            ));
        };
        self.record_outcome(outcome)
    }

    fn next_ordinal(&self) -> Result<DelegationEventOrdinal, DelegationTransitionFailure> {
        let Some(last_event) = self.events.last() else {
            return Err(DelegationTransitionFailure::MissingSpawnEvent);
        };
        last_event
            .ordinal()
            .successor()
            .ok_or(DelegationTransitionFailure::EventOrdinalExhausted)
    }

    fn fail(&self, failure: DelegationTransitionFailure) -> DelegationTransitionError {
        DelegationTransitionError {
            spawning_request: self.spawning_request,
            failure,
            rejected: None,
        }
    }

    fn reject_message(
        relation: Self,
        request: DelegationMessageRequest,
        id: DelegationMessageId,
        failure: DelegationTransitionFailure,
    ) -> DelegationTransitionError {
        DelegationTransitionError {
            spawning_request: relation.spawning_request,
            failure,
            rejected: Some(Box::new(RejectedDelegationTransition::DeliverMessage {
                relation,
                request,
                id,
            })),
        }
    }

    fn reject_outcome(
        relation: Self,
        outcome: DelegationOutcome,
        failure: DelegationTransitionFailure,
    ) -> DelegationTransitionError {
        DelegationTransitionError {
            spawning_request: relation.spawning_request,
            failure,
            rejected: Some(Box::new(RejectedDelegationTransition::RecordOutcome {
                relation,
                outcome,
            })),
        }
    }

    fn reject_parent_termination(
        relation: Self,
        authority: ParentTerminationAuthority,
        failure: DelegationTransitionFailure,
    ) -> DelegationTransitionError {
        DelegationTransitionError {
            spawning_request: relation.spawning_request,
            failure,
            rejected: Some(Box::new(
                RejectedDelegationTransition::RecordParentTermination {
                    relation,
                    authority,
                },
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationTransitionFailure {
    SameSession,
    AlreadyTerminal,
    MissingSpawnEvent,
    InvalidProvenance,
    DescendantsNotSelected,
    DuplicateMessageIdentity,
    ConflictingMessageReplay,
    DuplicateOutcomeAuthority,
    OutcomeReasonMismatch,
    EventOrdinalExhausted,
}

/// Unchanged aggregate and exact attempted input from a rejected consuming transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RejectedDelegationTransition {
    Spawn {
        request: DelegatedSpawnRequest,
        child: SessionId,
        child_turn: TurnId,
    },
    DeliverMessage {
        relation: SessionDelegation,
        request: DelegationMessageRequest,
        id: DelegationMessageId,
    },
    RecordOutcome {
        relation: SessionDelegation,
        outcome: DelegationOutcome,
    },
    RecordParentTermination {
        relation: SessionDelegation,
        authority: ParentTerminationAuthority,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationTransitionError {
    spawning_request: ToolRequestId,
    failure: DelegationTransitionFailure,
    rejected: Option<Box<RejectedDelegationTransition>>,
}

impl DelegationTransitionError {
    pub const fn failure(&self) -> DelegationTransitionFailure {
        self.failure
    }

    pub const fn spawning_request(&self) -> ToolRequestId {
        self.spawning_request
    }

    pub fn into_rejected(self) -> Option<RejectedDelegationTransition> {
        self.rejected.map(|rejected| *rejected)
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

impl std::error::Error for DelegationTransitionError {}

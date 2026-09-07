//! Model-call prepared call for `docs/spec/model-call-execution.md`.

use super::{IssuedModelCallCorrelation, ModelCallExecution, ModelTargetResolutionError};
use crate::{
    AcceptedInputId, AcceptedInputLifecycle, CurrentModelCall, CurrentTurnAttempt,
    DangerousToolAutoApproval, ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SessionId,
    TurnAttemptId, TurnId, UserContent, ValidatedModelSettings,
};
use std::collections::BTreeMap;

/// Why a fresh prepared checkpoint could not be derived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallPreparationFailure {
    /// The frozen selection has no immutable configured target.
    TargetUnavailable,
    /// The initial call has already been durably created.
    CallAlreadyExists,
    /// The current physical attempt is no longer prepared.
    AttemptIsNotPrepared,
    /// The supplied steering entry count differs from the complete inventory.
    SteeringIdentityCountMismatch,
    /// The steering snapshot candidate is missing, unexpected, or invalid.
    SteeringFrontierIdentityMismatch,
    /// Pending steering cannot form the exact consumed semantic suffix.
    SteeringCorrelationMismatch,
}

/// Failed preparation retaining the unchanged live aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallPreparationError {
    execution: Box<ModelCallExecution>,
    failure: ModelCallPreparationFailure,
    target_resolution_error: Option<ModelTargetResolutionError>,
}

impl ModelCallPreparationError {
    pub(super) fn new(execution: ModelCallExecution, failure: ModelCallPreparationFailure) -> Self {
        Self {
            execution: Box::new(execution),
            failure,
            target_resolution_error: None,
        }
    }

    pub(super) fn target_unavailable(
        execution: ModelCallExecution,
        target_resolution_error: ModelTargetResolutionError,
    ) -> Self {
        Self {
            execution: Box::new(execution),
            failure: ModelCallPreparationFailure::TargetUnavailable,
            target_resolution_error: Some(target_resolution_error),
        }
    }

    /// Returns the failure classification.
    pub const fn failure(&self) -> ModelCallPreparationFailure {
        self.failure
    }

    /// Returns the unchanged live aggregate.
    pub const fn execution(&self) -> &ModelCallExecution {
        &self.execution
    }

    /// Returns the exact immutable-catalog miss for target unavailability.
    pub const fn target_resolution_error(&self) -> Option<ModelTargetResolutionError> {
        self.target_resolution_error
    }
}

/// A newly prepared call and the exact durable ownership facts to commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedInitialModelCall {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) attempt: TurnAttemptId,
    pub(super) call: CurrentModelCall,
    pub(super) consumed_steering: Box<[PreparedSteeringConsumption]>,
    pub(super) steering_snapshot: Option<ResolvedContextFrontierSnapshot>,
}

impl PreparedInitialModelCall {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the unchanged prepared attempt.
    pub const fn attempt(&self) -> TurnAttemptId {
        self.attempt
    }

    /// Borrows the new durable prepared call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }

    /// Returns every steering consumption in immutable acceptance order.
    pub fn consumed_steering(&self) -> &[PreparedSteeringConsumption] {
        &self.consumed_steering
    }

    /// Borrows the extended call frontier when steering created one.
    pub const fn steering_snapshot(&self) -> Option<&ResolvedContextFrontierSnapshot> {
        self.steering_snapshot.as_ref()
    }
}

/// One accepted-input disposition and semantic entry prepared atomically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedSteeringConsumption {
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) semantic_entry: SemanticTranscriptEntry,
}

impl PreparedSteeringConsumption {
    /// Borrows the consumed accepted-input lifecycle.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Borrows the semantic entry appended for this consumption.
    pub const fn semantic_entry(&self) -> &SemanticTranscriptEntry {
        &self.semantic_entry
    }
}

/// Checked request material for a previously committed prepared call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedModelCallRequest {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) attempt: TurnAttemptId,
    pub(super) dangerous_tool_auto_approval: DangerousToolAutoApproval,
    pub(super) model_settings: ValidatedModelSettings,
    pub(super) call: CurrentModelCall,
    pub(super) frontier_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) origin_contents: BTreeMap<AcceptedInputId, UserContent>,
    pub(super) attachment_blob_facts: BTreeMap<crate::BlobDigest, std::num::NonZeroU64>,
}

impl PreparedModelCallRequest {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact prepared attempt.
    pub const fn attempt(&self) -> TurnAttemptId {
        self.attempt
    }

    /// Returns the dangerous blanket-auto posture frozen into this call's turn.
    pub const fn dangerous_tool_auto_approval(&self) -> DangerousToolAutoApproval {
        self.dangerous_tool_auto_approval
    }

    /// Returns the complete validated settings frozen for this turn.
    pub const fn model_settings(&self) -> ValidatedModelSettings {
        self.model_settings
    }

    /// Borrows the exact prepared call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }

    /// Iterates over the exact ordered semantic frontier.
    pub fn frontier_entries(&self) -> impl ExactSizeIterator<Item = &SemanticTranscriptEntry> {
        self.frontier_entry_slice().iter()
    }

    /// Borrows the exact ordered semantic frontier.
    ///
    /// Rendering projects and bounds the frontier before cloning any of it,
    /// which a borrow of the stored order supports and an owning copy of the
    /// same entries would defeat by duplicating every payload's content first.
    pub const fn frontier_entry_slice(&self) -> &[SemanticTranscriptEntry] {
        &self.frontier_entries
    }

    /// Borrows the exact user content for a frontier origin.
    pub fn origin_content(&self, accepted_input: AcceptedInputId) -> Option<&UserContent> {
        self.origin_contents.get(&accepted_input)
    }

    /// Returns the immutable byte length for one referenced attachment blob.
    pub fn attachment_byte_length(
        &self,
        digest: crate::BlobDigest,
    ) -> Option<std::num::NonZeroU64> {
        self.attachment_blob_facts.get(&digest).copied()
    }
}

/// Why no prepared request can be resumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallResumeFailure {
    /// No call has been durably checkpointed.
    CallMissing,
    /// The call has already left `Prepared`.
    CallIsNotPrepared,
    /// The owning attempt has already left `Prepared`.
    AttemptIsNotPrepared,
}

/// Why send authorization could not be derived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallAuthorizationFailure {
    /// No call has been durably checkpointed.
    CallMissing,
    /// The call has already left `Prepared`.
    CallIsNotPrepared,
    /// The owning attempt has already left `Prepared`.
    AttemptIsNotPrepared,
}

/// Failed authorization retaining the unchanged aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallAuthorizationError {
    pub(super) execution: Box<ModelCallExecution>,
    pub(super) failure: ModelCallAuthorizationFailure,
}

impl ModelCallAuthorizationError {
    /// Returns the failure classification.
    pub const fn failure(&self) -> ModelCallAuthorizationFailure {
        self.failure
    }

    /// Returns the unchanged aggregate.
    pub const fn execution(&self) -> &ModelCallExecution {
        &self.execution
    }
}

/// Exact metadata authorized for one provider interaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedModelCall {
    pub(super) session: SessionId,
    pub(super) turn: TurnId,
    pub(super) attempt: CurrentTurnAttempt,
    pub(super) call: CurrentModelCall,
    pub(super) frontier_entries: Box<[SemanticTranscriptEntry]>,
    pub(super) origin_contents: BTreeMap<AcceptedInputId, UserContent>,
}

impl AuthorizedModelCall {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the now-running attempt.
    pub const fn attempt(&self) -> &CurrentTurnAttempt {
        &self.attempt
    }

    /// Borrows the now-in-flight call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }

    /// Iterates over the exact ordered semantic frontier.
    pub fn frontier_entries(&self) -> impl ExactSizeIterator<Item = &SemanticTranscriptEntry> {
        self.frontier_entries.iter()
    }

    /// Borrows the exact user content for a frontier origin.
    pub fn origin_content(&self, accepted_input: AcceptedInputId) -> Option<&UserContent> {
        self.origin_contents.get(&accepted_input)
    }

    /// Returns the sealed issued facts that bind later provider observations
    /// to this exact authorization.
    pub const fn observation_correlation(&self) -> IssuedModelCallCorrelation {
        IssuedModelCallCorrelation {
            session: self.session,
            turn: self.turn,
            attempt: self.attempt.id(),
            call: self.call.id(),
            target: self.call.target(),
            frontier: self.call.frontier().snapshot(),
        }
    }
}

//! Model-call turn aggregate with intra-turn tool yields.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md,
//! docs/spec/model-call-execution.md, docs/spec/sessions-and-transcript.md,
//! and docs/spec/persistence-protocol.md are normative. This purpose-specific
//! aggregate reconstitutes one active accepted-input turn together with its
//! current model call. It owns target resolution against immutable
//! configured definitions, the separate prepared and send-authorization
//! transitions, atomic terminal candidates, and the response-side transition
//! that yields a tool-request batch without terminalizing the turn.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputTurnStart, ActivatedTurn, ActiveTurnPhase, AppliedInterruptCommandResult,
    AppliedInterruptProof, AssistantResponsePart, AttachmentBlobFact, AttemptEnd,
    AwaitingToolRecovery, CancellationStopDisposition, ContextFrontierId, CurrentModelCall,
    CurrentModelCallState, CurrentTurnAttempt, CurrentTurnAttemptState, DangerousToolAutoApproval,
    DelegationWaitMode, EffectiveConfiguration, EndedModelCall, EndedToolAttempt, EndedTurnAttempt,
    InitialToolApproval, ModelCallDisposition, ModelCallId, ModelCallReconstitutionInput,
    NonEmptyIssuedOperationRefs, OriginConfiguration, PinnedProviderTarget,
    PinnedProviderTargetReconstitutionInput, PreparedToolResultProjection, ReconciliationMarker,
    ReconstitutedModelCall, ResolvedContextFrontierReconstitutionInput,
    ResolvedContextFrontierSnapshot, SemanticTranscriptEntry, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SessionId, SteeringBinding, SteeringReclassificationReason,
    ToolApprovalDecision, ToolApprovalResolution, ToolRequest, ToolRequestId, ToolRequestOrdinal,
    ToolUsingAssistantResponse, TurnAttemptId, TurnAttemptStopCauses, TurnDisposition, TurnId,
    UnstoppedAttemptDisposition, UserContent,
};

mod target_catalog;
pub use target_catalog::{
    ModelTargetCatalog, ModelTargetCatalogError, ModelTargetDefinition, ModelTargetResolutionError,
    ResolvedModelSelection,
};
mod origin_content;
pub use origin_content::ModelCallOriginContent;
mod prepared_call;
pub use prepared_call::{
    AuthorizedModelCall, ModelCallAuthorizationError, ModelCallAuthorizationFailure,
    ModelCallPreparationError, ModelCallPreparationFailure, ModelCallResumeFailure,
    PreparedInitialModelCall, PreparedModelCallRequest, PreparedSteeringConsumption,
};
mod provider_call;
pub use provider_call::{
    CorrelatedModelCallTerminalObservation, IssuedModelCallCorrelation,
    ModelCallTerminalObservation, ProviderModelCallFailureCause, ProviderReportedTokenUsage,
};

/// Complete domain facts for reconstituting one live model-call execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallExecutionReconstitutionInput {
    active_turn: ActivatedTurn,
    targets: ModelTargetCatalog,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    continuation_snapshot: Option<ResolvedContextFrontierReconstitutionInput>,
    call_snapshot: Option<ResolvedContextFrontierReconstitutionInput>,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    origin_contents: Vec<ModelCallOriginContent>,
    attachment_blob_facts: Vec<AttachmentBlobFact>,
    pinned_target: Option<PinnedProviderTargetReconstitutionInput>,
    calls: Vec<ModelCallReconstitutionInput>,
    tool_result_correlations: Vec<ToolResultAttemptCorrelation>,
    tool_denial_correlations: Vec<ToolApprovalResolution>,
    uncommitted_tool_result_projection: Option<PreparedToolResultProjection>,
    availability_successor: bool,
}

impl ModelCallExecutionReconstitutionInput {
    /// Supplies the complete purpose-specific active-turn projection.
    pub fn new(
        active_turn: impl Into<ActivatedTurn>,
        targets: ModelTargetCatalog,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        frontier_entries: Vec<SemanticTranscriptEntry>,
        origin_contents: Vec<ModelCallOriginContent>,
        pinned_target: Option<PinnedProviderTargetReconstitutionInput>,
        calls: Vec<ModelCallReconstitutionInput>,
    ) -> Self {
        Self {
            active_turn: active_turn.into(),
            targets,
            starting_snapshot,
            continuation_snapshot: None,
            call_snapshot: None,
            frontier_entries,
            origin_contents,
            attachment_blob_facts: Vec::new(),
            pinned_target,
            calls,
            tool_result_correlations: Vec::new(),
            tool_denial_correlations: Vec::new(),
            uncommitted_tool_result_projection: None,
            availability_successor: false,
        }
    }

    /// Supplies immutable catalog length facts for every referenced attachment.
    pub fn with_attachment_blob_facts(mut self, facts: Vec<AttachmentBlobFact>) -> Self {
        self.attachment_blob_facts = facts;
        self
    }

    /// Supplies exact request and producing-call ownership for every physical
    /// tool attempt referenced by the current frontier.
    pub fn with_tool_result_correlations(
        mut self,
        correlations: Vec<ToolResultAttemptCorrelation>,
    ) -> Self {
        self.tool_result_correlations = correlations;
        self
    }

    /// Supplies the exact durable denial resolution for every denied request
    /// referenced by the current frontier.
    pub fn with_tool_denial_correlations(
        mut self,
        correlations: Vec<ToolApprovalResolution>,
    ) -> Self {
        self.tool_denial_correlations = correlations;
        self
    }

    /// Supplies the domain-prepared result projection being consumed inside
    /// the same transaction that will insert its continuation call.
    ///
    /// A durably visible resolved frontier without a prepared call is rejected;
    /// this proof admits only the transaction-local intermediate shape.
    pub fn with_uncommitted_tool_result_projection(
        mut self,
        projection: PreparedToolResultProjection,
    ) -> Self {
        self.uncommitted_tool_result_projection = Some(projection);
        self
    }

    /// Supplies the non-starting snapshot named by a steering-consuming call.
    pub fn with_call_snapshot(
        mut self,
        call_snapshot: ResolvedContextFrontierReconstitutionInput,
    ) -> Self {
        self.call_snapshot = Some(call_snapshot);
        self
    }

    /// Supplies the all-resolved tool-result frontier that precedes a fresh
    /// continuation call.
    ///
    /// The owning persistence aggregate remains responsible for proving the
    /// tool-batch/result correlation. This seam validates complete snapshot
    /// shape, turn ownership, and preservation of the eligibility-fixed
    /// starting prefix.
    pub fn with_continuation_snapshot(
        mut self,
        continuation_snapshot: ResolvedContextFrontierReconstitutionInput,
    ) -> Self {
        self.continuation_snapshot = Some(continuation_snapshot);
        self
    }

    /// Supplies durable proof that a call-free pinned attempt is the distinct
    /// successor of an availability-failed predecessor.
    pub fn with_availability_successor(mut self) -> Self {
        self.availability_successor = true;
        self
    }

    /// Reconstructs the canonical live aggregate without effects.
    pub fn reconstitute(self) -> Result<ModelCallExecution, ModelCallExecutionReconstitutionError> {
        reconstitute(self)
    }
}

/// Stored ownership facts for one tool attempt referenced by model input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolResultAttemptCorrelation {
    attempt: crate::ToolAttemptId,
    request: crate::ToolRequestId,
    producing_call: ModelCallId,
}

impl ToolResultAttemptCorrelation {
    /// Captures one exact attempt-to-request-to-producing-call relationship.
    pub const fn new(
        attempt: crate::ToolAttemptId,
        request: crate::ToolRequestId,
        producing_call: ModelCallId,
    ) -> Self {
        Self {
            attempt,
            request,
            producing_call,
        }
    }

    /// Returns the physical attempt.
    pub const fn attempt(&self) -> crate::ToolAttemptId {
        self.attempt
    }

    /// Returns the logical request executed by the attempt.
    pub const fn request(&self) -> crate::ToolRequestId {
        self.request
    }

    /// Returns the model call that proposed the request.
    pub const fn producing_call(&self) -> ModelCallId {
        self.producing_call
    }
}

/// Why live stored execution facts cannot reconstruct the initial aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallExecutionReconstitutionFailure {
    /// The supplied phase is a durable wait, not a live execution.
    TurnIsNotRunning,
    /// The starting snapshot belongs to a different session.
    StartingSnapshotSessionMismatch,
    /// The supplied snapshot is not the turn's eligibility-fixed start.
    StartingSnapshotMismatch,
    /// A non-starting call frontier was omitted.
    CallSnapshotMissing,
    /// A continuation snapshot was supplied outside a fresh continuation.
    ContinuationSnapshotUnexpected,
    /// A continuation snapshot is malformed or does not preserve the start.
    ContinuationSnapshotMismatch,
    /// A call snapshot was supplied without a consuming call or steering.
    CallSnapshotUnexpected,
    /// The supplied call snapshot is not the call's exact prefix extension.
    CallSnapshotMismatch,
    /// The supplied frontier entries do not exactly back ordered membership.
    FrontierEntryMismatch,
    /// Tool-result attempts do not exactly belong to the referenced requests
    /// and producing model calls.
    ToolResultCorrelationMismatch,
    /// Tool-denial entries do not exactly match durable denied resolutions.
    ToolDenialCorrelationMismatch,
    /// More than one model call was supplied to the initial-call slice.
    MultipleCalls,
    /// More than one user-content fact names the same accepted input.
    DuplicateOriginContent,
    /// A frontier origin has no exact accepted user content.
    MissingOriginContent,
    /// User content was supplied for an accepted input absent from the
    /// frontier and pending steering inventory.
    UnreferencedOriginContent,
    /// Attachment catalog facts do not exactly cover referenced blob identities.
    AttachmentBlobFactMismatch,
    /// Consumed steering does not exactly match the call frontier suffix.
    ConsumedSteeringMismatch,
    /// A call belongs to a different turn or session frontier.
    CallOwnershipMismatch,
    /// A call records a different frozen selection.
    CallSelectionMismatch,
    /// A stored call target contradicts an available immutable catalog entry.
    CallTargetMismatch,
    /// A call exists without the independently stored turn-pinned target.
    PinnedTargetMissing,
    /// A pinned target exists even though no call was atomically created.
    PinnedTargetUnexpected,
    /// The stored pinned target belongs to another turn.
    PinnedTargetTurnMismatch,
    /// Stored call facts cannot reconstruct the accepted call lifecycle.
    InvalidCall,
    /// Attempt and call states do not form one accepted execution phase.
    LifecycleMismatch,
}

/// Reconstitution failure retaining the complete rejected input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallExecutionReconstitutionError {
    input: Box<ModelCallExecutionReconstitutionInput>,
    failure: ModelCallExecutionReconstitutionFailure,
}

impl ModelCallExecutionReconstitutionError {
    /// Returns the exact failure classification.
    pub const fn failure(&self) -> ModelCallExecutionReconstitutionFailure {
        self.failure
    }

    /// Returns the complete rejected input.
    pub const fn input(&self) -> &ModelCallExecutionReconstitutionInput {
        &self.input
    }

    /// Returns the rejected input and failure.
    pub fn into_parts(
        self,
    ) -> (
        ModelCallExecutionReconstitutionInput,
        ModelCallExecutionReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One checked live initial model-call execution aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallExecution {
    active_turn: ActivatedTurn,
    session: SessionId,
    turn: TurnId,
    configuration: OriginConfiguration,
    start: AcceptedInputTurnStart,
    targets: ModelTargetCatalog,
    current_attempt: CurrentTurnAttempt,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    current_snapshot: ResolvedContextFrontierSnapshot,
    frontier_entries: Box<[SemanticTranscriptEntry]>,
    origin_contents: BTreeMap<AcceptedInputId, UserContent>,
    attachment_blob_facts: BTreeMap<crate::BlobDigest, std::num::NonZeroU64>,
    pinned_target: Option<PinnedProviderTarget>,
    current_call: Option<CurrentModelCall>,
    tool_continuation_frontier: bool,
}

impl ModelCallExecution {
    /// Borrows the checked active-turn facts that establish ownership.
    pub const fn active_turn(&self) -> &ActivatedTurn {
        &self.active_turn
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the exact frozen origin configuration.
    pub const fn configuration(&self) -> &OriginConfiguration {
        &self.configuration
    }

    /// Returns the exact eligibility-fixed lineage and starting frontier.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
    }

    /// Borrows the current physical attempt.
    pub const fn current_attempt(&self) -> &CurrentTurnAttempt {
        &self.current_attempt
    }

    /// Borrows the current model call, when one has been checkpointed.
    pub const fn current_call(&self) -> Option<&CurrentModelCall> {
        self.current_call.as_ref()
    }

    /// Iterates over the exact ordered semantic frontier supplied to the call.
    pub fn frontier_entries(&self) -> impl ExactSizeIterator<Item = &SemanticTranscriptEntry> {
        self.frontier_entries.iter()
    }

    /// Borrows the exact user content for a frontier origin.
    pub fn origin_content(&self, accepted_input: AcceptedInputId) -> Option<&UserContent> {
        self.origin_contents.get(&accepted_input)
    }

    /// Derives the exact first-call request without committing or changing this aggregate.
    pub fn preview_initial_call(
        &self,
        call: ModelCallId,
    ) -> Result<PreparedModelCallRequest, ModelCallPreparationError> {
        let prepared = self.clone().prepare_initial_call(call)?;
        Ok(PreparedModelCallRequest {
            session: self.session,
            turn: self.turn,
            attempt: prepared.attempt(),
            dangerous_tool_auto_approval: self
                .configuration
                .effective()
                .dangerous_tool_auto_approval(),
            model_settings: self.configuration.effective().model_settings(),
            call: prepared.call().clone(),
            frontier_entries: self.frontier_entries.clone(),
            origin_contents: self.origin_contents.clone(),
            attachment_blob_facts: self.attachment_blob_facts.clone(),
        })
    }

    /// Creates the initial durable `Prepared` call checkpoint.
    pub fn prepare_initial_call(
        self,
        call: ModelCallId,
    ) -> Result<PreparedInitialModelCall, ModelCallPreparationError> {
        self.prepare_initial_call_consuming_steering(call, Vec::new(), None)
    }

    /// Creates the initial durable call while consuming the complete pending
    /// steering inventory.
    pub fn prepare_initial_call_consuming_steering(
        self,
        call: ModelCallId,
        steering_entries: Vec<SemanticTranscriptEntryId>,
        steering_frontier: Option<ContextFrontierId>,
    ) -> Result<PreparedInitialModelCall, ModelCallPreparationError> {
        let frozen = *self.configuration.effective().model();
        if self.current_call.is_some() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::CallAlreadyExists,
            ));
        }
        if !self.attempt_accepts_prepared_call() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::AttemptIsNotPrepared,
            ));
        }
        let pending = self.active_turn.pending_steering().to_vec();
        if pending.len() != steering_entries.len() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::SteeringIdentityCountMismatch,
            ));
        }
        if pending.is_empty() != steering_frontier.is_none() {
            return Err(ModelCallPreparationError::new(
                self,
                ModelCallPreparationFailure::SteeringFrontierIdentityMismatch,
            ));
        }
        let pinned = if let Some(pinned) = self.pinned_target {
            pinned
        } else {
            let resolution = match self.targets.resolve(frozen) {
                Ok(resolution) => resolution,
                Err(error) => {
                    return Err(ModelCallPreparationError::target_unavailable(self, error));
                }
            };
            PinnedProviderTarget::pinned(self.turn, resolution.target)
        };
        let mut distinct_entries = self
            .frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::identity)
            .collect::<BTreeSet<_>>();
        let mut consumed_steering = Vec::with_capacity(pending.len());
        let mut semantic_entries = Vec::with_capacity(pending.len());
        for (pending, entry) in pending.iter().zip(steering_entries) {
            let AcceptedInputDisposition::PendingSteering { binding } =
                pending.lifecycle().disposition()
            else {
                return Err(ModelCallPreparationError::new(
                    self,
                    ModelCallPreparationFailure::SteeringCorrelationMismatch,
                ));
            };
            if binding.source_turn() != self.turn
                || !distinct_entries.insert(entry)
                || !self.origin_contents.contains_key(&pending.accepted_input())
            {
                return Err(ModelCallPreparationError::new(
                    self,
                    ModelCallPreparationFailure::SteeringCorrelationMismatch,
                ));
            }
            let lifecycle = pending
                .lifecycle()
                .clone()
                .consume_as_steering(call)
                .map_err(|_| {
                    ModelCallPreparationError::new(
                        self.clone(),
                        ModelCallPreparationFailure::SteeringCorrelationMismatch,
                    )
                })?;
            let semantic_entry = SemanticTranscriptEntry::from_validated_parts(
                entry,
                self.session,
                SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input: pending.accepted_input(),
                    source_turn: self.turn,
                },
            );
            consumed_steering.push(PreparedSteeringConsumption {
                accepted_input: lifecycle,
                semantic_entry: semantic_entry.clone(),
            });
            semantic_entries.push(semantic_entry);
        }
        let call_snapshot = if semantic_entries.is_empty() {
            self.current_snapshot.clone()
        } else {
            self.current_snapshot
                .derive_appending_candidate(
                    match steering_frontier {
                        Some(frontier) => frontier,
                        None => {
                            return Err(ModelCallPreparationError::new(
                                self,
                                ModelCallPreparationFailure::SteeringFrontierIdentityMismatch,
                            ));
                        }
                    },
                    semantic_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| {
                    ModelCallPreparationError::new(
                        self.clone(),
                        ModelCallPreparationFailure::SteeringFrontierIdentityMismatch,
                    )
                })?
        };
        let prepared = CurrentModelCall::prepared(
            call,
            self.current_attempt.id(),
            frozen,
            pinned,
            &call_snapshot,
        );
        Ok(PreparedInitialModelCall {
            session: self.session,
            turn: self.turn,
            attempt: self.current_attempt.id(),
            call: prepared,
            consumed_steering: consumed_steering.into_boxed_slice(),
            steering_snapshot: (!semantic_entries.is_empty()).then_some(call_snapshot),
        })
    }

    /// Returns a previously committed `Prepared` call for capability setup.
    pub fn resume_prepared_call(&self) -> Result<PreparedModelCallRequest, ModelCallResumeFailure> {
        if !self.attempt_accepts_prepared_call() {
            return Err(ModelCallResumeFailure::AttemptIsNotPrepared);
        }
        let Some(call) = &self.current_call else {
            return Err(ModelCallResumeFailure::CallMissing);
        };
        if call.state() != CurrentModelCallState::Prepared {
            return Err(ModelCallResumeFailure::CallIsNotPrepared);
        }
        let origin_contents = self
            .frontier_entries
            .iter()
            .filter_map(|entry| match entry.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => self
                    .origin_contents
                    .get(accepted_input)
                    .map(|content| (*accepted_input, content.clone())),
                SemanticTranscriptEntryPayload::TurnFailed { .. }
                | SemanticTranscriptEntryPayload::DelegatedTask { .. }
                | SemanticTranscriptEntryPayload::DelegationMessage { .. }
                | SemanticTranscriptEntryPayload::DelegationResult { .. }
                | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
                | SemanticTranscriptEntryPayload::ContextSummary { .. }
                | SemanticTranscriptEntryPayload::Imported { .. }
                | SemanticTranscriptEntryPayload::AssistantText { .. }
                | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
                | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                | SemanticTranscriptEntryPayload::ToolDenied { .. }
                | SemanticTranscriptEntryPayload::ToolClosed { .. }
                | SemanticTranscriptEntryPayload::TurnCompleted { .. }
                | SemanticTranscriptEntryPayload::TurnCancelled { .. } => None,
            })
            .collect();
        Ok(PreparedModelCallRequest {
            session: self.session,
            turn: self.turn,
            attempt: self.current_attempt.id(),
            dangerous_tool_auto_approval: self
                .configuration
                .effective()
                .dangerous_tool_auto_approval(),
            model_settings: self.configuration.effective().model_settings(),
            call: call.clone(),
            frontier_entries: self.frontier_entries.clone(),
            origin_contents,
            attachment_blob_facts: self.attachment_blob_facts.clone(),
        })
    }

    /// Atomically authorizes the prepared attempt and call to cross the send
    /// boundary.
    pub fn authorize_send(self) -> Result<AuthorizedModelCall, ModelCallAuthorizationError> {
        let fail = |execution, failure| ModelCallAuthorizationError {
            execution: Box::new(execution),
            failure,
        };
        let Some(call) = self.current_call.clone() else {
            return Err(fail(self, ModelCallAuthorizationFailure::CallMissing));
        };
        if call.state() != CurrentModelCallState::Prepared {
            return Err(fail(self, ModelCallAuthorizationFailure::CallIsNotPrepared));
        }
        let attempt = match self.current_attempt.state() {
            CurrentTurnAttemptState::Prepared => {
                self.current_attempt.clone().begin_running().map_err(|_| {
                    fail(
                        self.clone(),
                        ModelCallAuthorizationFailure::AttemptIsNotPrepared,
                    )
                })?
            }
            CurrentTurnAttemptState::Running if self.is_running_tool_continuation() => {
                self.current_attempt.clone()
            }
            CurrentTurnAttemptState::Running | CurrentTurnAttemptState::StopRequested { .. } => {
                return Err(fail(
                    self,
                    ModelCallAuthorizationFailure::AttemptIsNotPrepared,
                ));
            }
        };
        let call = call.begin_in_flight().map_err(|_| {
            fail(
                self.clone(),
                ModelCallAuthorizationFailure::CallIsNotPrepared,
            )
        })?;
        Ok(AuthorizedModelCall {
            session: self.session,
            turn: self.turn,
            attempt,
            call,
            frontier_entries: self.frontier_entries,
            origin_contents: self.origin_contents,
        })
    }

    /// Reconstructs the exact issued metadata after an ambiguous
    /// authorization commit without performing another state transition.
    pub fn resume_in_flight_call(&self) -> Option<AuthorizedModelCall> {
        let call = self.current_call.clone()?;
        if self.current_attempt.state() != &CurrentTurnAttemptState::Running
            || call.state() != CurrentModelCallState::InFlight
        {
            return None;
        }
        Some(AuthorizedModelCall {
            session: self.session,
            turn: self.turn,
            attempt: self.current_attempt.clone(),
            call,
            frontier_entries: self.frontier_entries.clone(),
            origin_contents: self.origin_contents.clone(),
        })
    }

    /// Reconstructs an issued call whose exact interrupt was durably accepted
    /// before this process entered the provider.
    pub fn resume_cancellation_requested_call(&self) -> Option<StopRequestedModelCallTurn> {
        let call = self.current_call.clone()?;
        let CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
        } = self.current_attempt.state()
        else {
            return None;
        };
        if call.state() != CurrentModelCallState::CancellationRequested {
            return None;
        }
        Some(StopRequestedModelCallTurn {
            session: self.session,
            turn: self.turn,
            call,
            attempt: self.current_attempt.clone(),
            interrupt: *interrupt,
        })
    }

    /// Applies one exactly correlated interrupt to the current initial-call
    /// execution.
    pub fn apply_interrupt(
        self,
        interrupt: AppliedInterruptCommandResult,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<ModelCallInterruptOutcome, ModelCallClosureError> {
        let proof = interrupt.proof();
        if interrupt.session() != self.session
            || proof.predecessor() != self.turn
            || interrupt.successor() == self.turn
            || interrupt.successor_order().priority()
                != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                    predecessor: self.turn,
                })
        {
            return Err(ModelCallClosureError::InterruptCorrelationMismatch);
        }
        let unsent_call = matches!(
            (
                self.current_attempt.state(),
                self.current_call.as_ref().map(CurrentModelCall::state),
            ),
            (CurrentTurnAttemptState::Prepared, None)
                | (
                    CurrentTurnAttemptState::Prepared,
                    Some(CurrentModelCallState::Prepared)
                )
        ) || (self.is_running_tool_continuation()
            && matches!(
                self.current_call.as_ref().map(CurrentModelCall::state),
                None | Some(CurrentModelCallState::Prepared)
            ));
        if unsent_call {
            let reclassified_pending_steering = reclassify_pending_steering(
                &self.active_turn,
                &identities.pending_steering_reclassifications,
            )?;
            let ended_call = self
                .current_call
                .map(|call| call.end_cancelled_unsent(proof))
                .transpose()
                .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
            let cancelled = close_cancelled_turn(
                ModelCallTurnScope {
                    session: self.session,
                    turn: self.turn,
                },
                Some(self.current_attempt),
                ended_call,
                CancellationFrontierSource::new(self.current_snapshot, &[]),
                proof,
                identities,
                reclassified_pending_steering,
            )?;
            return Ok(ModelCallInterruptOutcome::Cancelled(cancelled));
        }
        match (
            self.current_attempt.state(),
            self.current_call.as_ref().map(CurrentModelCall::state),
        ) {
            (CurrentTurnAttemptState::Running, Some(CurrentModelCallState::InFlight)) => {
                let attempt = self
                    .current_attempt
                    .request_cancellation(proof)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let call = self
                    .current_call
                    .ok_or(ModelCallClosureError::CallStateMismatch)?
                    .request_cancellation()
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                Ok(ModelCallInterruptOutcome::CancellationRequested(
                    StopRequestedModelCallTurn {
                        session: self.session,
                        turn: self.turn,
                        call,
                        attempt,
                        interrupt: proof,
                    },
                ))
            }
            _ => Err(ModelCallClosureError::AttemptStateMismatch),
        }
    }

    fn attempt_accepts_prepared_call(&self) -> bool {
        self.current_attempt.state() == &CurrentTurnAttemptState::Prepared
            || self.is_running_tool_continuation()
    }

    fn is_running_tool_continuation(&self) -> bool {
        self.current_attempt.state() == &CurrentTurnAttemptState::Running
            && self.tool_continuation_frontier
    }

    /// Applies an interrupt after a tool batch has closed every logical
    /// request and no executor effect remains live.
    pub fn apply_interrupt_to_tool_batch(
        self,
        interrupt: AppliedInterruptCommandResult,
        result_projection: PreparedToolResultProjection,
        identities: CancelledModelCallTurnIdentities,
    ) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some()
            || interrupt.session() != self.session
            || interrupt.proof().predecessor() != self.turn
            || interrupt.successor() == self.turn
            || interrupt.successor_order().priority()
                != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                    predecessor: self.turn,
                })
            || result_projection.turn() != self.turn
            || result_projection.snapshot().frontier().owning_session() != self.session
            || result_projection.source_frontier() != self.current_snapshot.frontier().snapshot()
            || !self
                .current_snapshot
                .is_semantic_prefix_of(result_projection.snapshot())
            || result_projection.snapshot().entry_count()
                != self.current_snapshot.entry_count() + result_projection.entries().len()
        {
            return Err(ModelCallClosureError::InterruptCorrelationMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &identities.pending_steering_reclassifications,
        )?;
        let (result_entries, _result_snapshot) = result_projection.into_parts();
        let mut cancelled = close_cancelled_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            Some(self.current_attempt),
            None,
            CancellationFrontierSource::new(self.current_snapshot, &result_entries),
            interrupt.proof(),
            identities,
            reclassified_pending_steering,
        )?;
        cancelled.tool_result_entries = result_entries;
        Ok(cancelled)
    }

    /// Applies one provider observation to freshly reloaded issued state.
    ///
    /// docs/spec/model-call-execution.md requires the observation
    /// transaction to reconstruct current authority after the provider
    /// effect rather than retaining the earlier authorization projection
    /// across that effect.
    pub fn apply_terminal_observation(
        self,
        observation: CorrelatedModelCallTerminalObservation,
        identities: ModelCallTerminalIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
        let Some(call) = self.current_call else {
            return Err(ModelCallClosureError::CallStateMismatch);
        };
        if observation.correlation
            != (IssuedModelCallCorrelation {
                session: self.session,
                turn: self.turn,
                attempt: self.current_attempt.id(),
                call: call.id(),
                target: call.target(),
                frontier: call.frontier().snapshot(),
            })
        {
            return Err(ModelCallClosureError::ObservationCorrelationMismatch);
        }
        let lifecycle_valid = matches!(
            (self.current_attempt.state(), call.state()),
            (
                CurrentTurnAttemptState::Running,
                CurrentModelCallState::InFlight
            ) | (
                CurrentTurnAttemptState::StopRequested {
                    causes: TurnAttemptStopCauses::CancellationOnly { .. }
                },
                CurrentModelCallState::CancellationRequested
            )
        );
        if !lifecycle_valid {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let cancellation_requested = matches!(
            self.current_attempt.state(),
            CurrentTurnAttemptState::StopRequested {
                causes: TurnAttemptStopCauses::CancellationOnly { .. }
            }
        );
        let reclassified_pending_steering = if (observation.observation.is_tool_round()
            && !cancellation_requested)
            || (observation.observation.disposition() == ModelCallDisposition::Ambiguous
                && !cancellation_requested)
        {
            Box::new([])
        } else {
            reclassify_pending_steering(
                &self.active_turn,
                identities.pending_steering_reclassifications(),
            )?
        };
        let dangerous_tool_auto_approval = self
            .configuration
            .effective()
            .dangerous_tool_auto_approval();
        apply_terminal_observation(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            call,
            self.frontier_entries,
            observation.observation,
            identities,
            ModelCallTerminalContext {
                reclassified_pending_steering,
                dangerous_tool_auto_approval,
            },
        )
    }

    /// Ends one availability-failed call and prepares its distinct successor
    /// attempt without terminalizing the logical turn.
    ///
    /// The caller must first validate the call-pinned credential-pool policy
    /// and authorize either retry or rotation. This aggregate transition owns
    /// only the lifecycle proof required by
    /// `docs/spec/model-call-execution.md`: the
    /// predecessor remains `KnownFailed`, one authorization is never reused,
    /// and the successor receives a fresh physical attempt.
    pub fn apply_availability_successor(
        self,
        observation: CorrelatedModelCallTerminalObservation,
        successor_attempt: TurnAttemptId,
    ) -> Result<AvailabilitySuccessorModelCallTurn, ModelCallClosureError> {
        let Some(call) = self.current_call else {
            return Err(ModelCallClosureError::CallStateMismatch);
        };
        if observation.correlation
            != (IssuedModelCallCorrelation {
                session: self.session,
                turn: self.turn,
                attempt: self.current_attempt.id(),
                call: call.id(),
                target: call.target(),
                frontier: call.frontier().snapshot(),
            })
            || observation.observation != ModelCallTerminalObservation::KnownFailed
            || !matches!(
                observation.provider_failure_cause,
                Some(
                    ProviderModelCallFailureCause::CredentialRejected
                        | ProviderModelCallFailureCause::RateLimited
                        | ProviderModelCallFailureCause::QuotaExhausted
                        | ProviderModelCallFailureCause::Overloaded
                        | ProviderModelCallFailureCause::ProviderInternal
                )
            )
            || self.current_attempt.state() != &CurrentTurnAttemptState::Running
            || call.state() != CurrentModelCallState::InFlight
            || successor_attempt == self.current_attempt.id()
        {
            return Err(ModelCallClosureError::ObservationCorrelationMismatch);
        }
        let ended_call = call
            .end_classified(ModelCallDisposition::KnownFailed)
            .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
        let ended_attempt = self
            .current_attempt
            .end_without_stop(UnstoppedAttemptDisposition::KnownFailure)
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
        Ok(AvailabilitySuccessorModelCallTurn {
            session: self.session,
            turn: self.turn,
            predecessor_call: ended_call,
            predecessor_attempt: ended_attempt,
            successor_attempt: CurrentTurnAttempt::prepared(successor_attempt),
        })
    }

    /// Closes target-resolution failure before a model call exists.
    pub fn fail_target_resolution(
        self,
        resolution_error: ModelTargetResolutionError,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some() {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let frozen = *self.configuration.effective().model();
        if resolution_error.selection() != frozen
            || !matches!(self.targets.resolve(frozen), Err(expected) if expected == resolution_error)
        {
            return Err(ModelCallClosureError::TargetResolutionMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &identities.pending_steering_reclassifications,
        )?;
        close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            self.starting_snapshot,
            identities,
            UnstoppedAttemptDisposition::KnownFailure,
            reclassified_pending_steering,
        )
    }

    /// Closes a call-free attempt when its frozen credential pool admits no
    /// member, preserving exhaustion as a cause distinct from any member's
    /// provider failure.
    ///
    /// The selection and evidence rules are owned by
    /// `docs/spec/credential-availability.md`.
    pub fn fail_credential_pool_exhausted(
        self,
        pool_name: String,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<CredentialPoolExhaustedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some() || !self.attempt_accepts_prepared_call() {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &identities.pending_steering_reclassifications,
        )?;
        let failed = close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            self.current_snapshot,
            identities,
            UnstoppedAttemptDisposition::KnownFailure,
            reclassified_pending_steering,
        )?;
        Ok(CredentialPoolExhaustedModelCallTurn { pool_name, failed })
    }

    /// Closes a call-free attempt whose required automatic context compaction failed.
    pub fn fail_automatic_context_compaction(
        self,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some() || !self.attempt_accepts_prepared_call() {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &identities.pending_steering_reclassifications,
        )?;
        close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            self.current_snapshot,
            identities,
            UnstoppedAttemptDisposition::KnownFailure,
            reclassified_pending_steering,
        )
    }

    /// Closes a trustworthy local capability-preparation failure before send.
    pub fn fail_prepared_call(
        self,
        identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        let Some(call) = self.current_call else {
            return Err(ModelCallClosureError::CallStateMismatch);
        };
        if call.state() != CurrentModelCallState::Prepared {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &identities.pending_steering_reclassifications,
        )?;
        let ended_call = call
            .end_classified(ModelCallDisposition::KnownFailed)
            .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
        close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            Some(ended_call),
            self.current_snapshot,
            identities,
            UnstoppedAttemptDisposition::KnownFailure,
            reclassified_pending_steering,
        )
    }

    /// Applies the prior-process recovery rule in
    /// docs/spec/model-call-execution.md for a committed model call after
    /// startup has established that no provider task survived.
    pub fn recover_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
        let Some(call) = self.current_call else {
            return Err(ModelCallClosureError::CallStateMismatch);
        };
        match call.state() {
            CurrentModelCallState::Prepared => {
                let reclassified_pending_steering = reclassify_pending_steering(
                    &self.active_turn,
                    &failure_identities.pending_steering_reclassifications,
                )?;
                let call = call
                    .end_classified(ModelCallDisposition::KnownFailed)
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                close_failed_turn(
                    ModelCallTurnScope {
                        session: self.session,
                        turn: self.turn,
                    },
                    self.current_attempt,
                    Some(call),
                    self.current_snapshot,
                    failure_identities,
                    UnstoppedAttemptDisposition::Lost,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::Failed)
            }
            CurrentModelCallState::InFlight => {
                let call = call
                    .end_classified(ModelCallDisposition::Ambiguous)
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                let call_id = call.id();
                let attempt = self
                    .current_attempt
                    .end_without_stop(UnstoppedAttemptDisposition::Lost)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let ambiguous_operations = NonEmptyIssuedOperationRefs::try_from_operations([
                    crate::IssuedOperationRef::ModelCall(call_id),
                ])
                .map_err(|_| ModelCallClosureError::AmbiguityConstructionFailed)?;
                Ok(ModelCallTerminalOutcome::AwaitingRecovery(
                    AmbiguousModelCallTurn {
                        session: self.session,
                        turn: self.turn,
                        call,
                        attempt,
                        ambiguous_operations,
                    },
                ))
            }
            CurrentModelCallState::CancellationRequested => {
                let CurrentTurnAttemptState::StopRequested {
                    causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
                } = self.current_attempt.state()
                else {
                    return Err(ModelCallClosureError::AttemptStateMismatch);
                };
                let proof = *interrupt;
                let call = call
                    .end_classified(ModelCallDisposition::Ambiguous)
                    .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
                let call_id = call.id();
                let reclassified_pending_steering = reclassify_pending_steering(
                    &self.active_turn,
                    &failure_identities.pending_steering_reclassifications,
                )?;
                let terminal_snapshot = self
                    .current_snapshot
                    .derive_appending_candidate(failure_identities.terminal_frontier, Vec::new())
                    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let attempt = self
                    .current_attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::Lost)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                let ambiguous_operations = NonEmptyIssuedOperationRefs::try_from_operations([
                    crate::IssuedOperationRef::ModelCall(call_id),
                ])
                .map_err(|_| ModelCallClosureError::AmbiguityConstructionFailed)?;
                let marker =
                    ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations, proof);
                Ok(ModelCallTerminalOutcome::ReconciliationRequired(
                    ReconciliationRequiredModelCallTurn {
                        session: self.session,
                        turn: self.turn,
                        call,
                        attempt,
                        disposition: TurnDisposition::ReconciliationRequired { marker },
                        terminal_snapshot,
                        reclassified_pending_steering,
                    },
                ))
            }
        }
    }

    /// Applies evidence-free startup recovery before any model call exists.
    ///
    /// Pending steering is reclassified in the same failed-terminal commit, so
    /// a prior-process prepared attempt cannot remain live solely because no
    /// model-call checkpoint had yet been created.
    pub fn recover_evidence_free_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some() {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &failure_identities.pending_steering_reclassifications,
        )?;
        close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            self.starting_snapshot,
            failure_identities,
            UnstoppedAttemptDisposition::Lost,
            reclassified_pending_steering,
        )
    }

    /// Fails a current tool continuation after crash closure is materialized.
    ///
    /// Unlike evidence-free recovery, the failure marker extends the exact
    /// current frontier so committed assistant tool uses and their
    /// proposal-ordered closures remain provider-renderable history.
    pub fn recover_tool_crash_after_restart(
        self,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<FailedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some()
            || !frontier_contains_tool_round(&self.starting_snapshot, &self.frontier_entries)
        {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &failure_identities.pending_steering_reclassifications,
        )?;
        close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            self.current_snapshot,
            failure_identities,
            UnstoppedAttemptDisposition::Lost,
            reclassified_pending_steering,
        )
    }

    /// Closes a resolved tool continuation before another call when durable
    /// provider usage proves that call cannot retain configured output
    /// headroom without compaction.
    pub fn require_context_compaction_after_tool_results(
        self,
        producing_call: ModelCallId,
        failure_identities: FailedModelCallTurnIdentities,
    ) -> Result<ContextHeadroomExhaustedModelCallTurn, ModelCallClosureError> {
        if self.current_call.is_some()
            || !frontier_contains_tool_round(&self.starting_snapshot, &self.frontier_entries)
        {
            return Err(ModelCallClosureError::CallStateMismatch);
        }
        let reclassified_pending_steering = reclassify_pending_steering(
            &self.active_turn,
            &failure_identities.pending_steering_reclassifications,
        )?;
        let failed = close_failed_turn(
            ModelCallTurnScope {
                session: self.session,
                turn: self.turn,
            },
            self.current_attempt,
            None,
            self.current_snapshot,
            failure_identities,
            UnstoppedAttemptDisposition::KnownFailure,
            reclassified_pending_steering,
        )?;
        Ok(ContextHeadroomExhaustedModelCallTurn {
            producing_call,
            failed,
        })
    }
}

#[derive(Clone, Copy)]
struct ModelCallTurnScope {
    session: SessionId,
    turn: TurnId,
}

struct ModelCallTerminalContext {
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
}

fn apply_terminal_observation(
    scope: ModelCallTurnScope,
    attempt: CurrentTurnAttempt,
    call: CurrentModelCall,
    frontier_entries: Box<[SemanticTranscriptEntry]>,
    observation: ModelCallTerminalObservation,
    identities: ModelCallTerminalIdentities,
    context: ModelCallTerminalContext,
) -> Result<ModelCallTerminalOutcome, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    let ModelCallTerminalContext {
        reclassified_pending_steering,
        dangerous_tool_auto_approval,
    } = context;
    let cancellation_proof = match attempt.state() {
        CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::CancellationOnly { interrupt },
        } => Some(*interrupt),
        CurrentTurnAttemptState::Prepared
        | CurrentTurnAttemptState::Running
        | CurrentTurnAttemptState::StopRequested {
            causes: TurnAttemptStopCauses::FatalMismatch(_),
        } => None,
    };
    let disposition = observation.disposition();
    let source_frontier = call.frontier();
    let ended_call = call
        .end_classified(disposition)
        .map_err(|_| ModelCallClosureError::CallStateMismatch)?;
    match observation {
        ModelCallTerminalObservation::Completed { assistant_text } => {
            let ModelCallTerminalIdentities::Completed(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = match cancellation_proof {
                Some(proof) => attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::TurnCompleted),
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::TurnCompleted),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let completed = complete_turn(
                scope,
                ended_call,
                ended_attempt,
                frontier_entries.into_vec(),
                assistant_text
                    .into_iter()
                    .map(AssistantResponsePart::Text)
                    .collect(),
                identities,
                reclassified_pending_steering,
            )?;
            Ok(ModelCallTerminalOutcome::Completed(completed))
        }
        ModelCallTerminalObservation::CompletedWithProviderCompaction { response, .. } => {
            let ModelCallTerminalIdentities::Completed(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = match cancellation_proof {
                Some(proof) => attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::TurnCompleted),
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::TurnCompleted),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let completed = complete_turn(
                scope,
                ended_call,
                ended_attempt,
                frontier_entries.into_vec(),
                response,
                identities,
                reclassified_pending_steering,
            )?;
            Ok(ModelCallTerminalOutcome::Completed(completed))
        }
        ModelCallTerminalObservation::CompletedWithTools { response, .. } => {
            if let Some(proof) = cancellation_proof {
                let ModelCallTerminalIdentities::StoppedToolRound(identities) = identities else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let ended_attempt = attempt
                    .end_after_cancellation(proof, CancellationStopDisposition::Cancelled)
                    .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
                return assemble_stopped_tool_round(
                    scope,
                    ended_call,
                    ended_attempt,
                    frontier_entries.into_vec(),
                    response,
                    proof,
                    identities,
                    dangerous_tool_auto_approval,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::CancelledWithToolResponse);
            }
            let ModelCallTerminalIdentities::ToolRound(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = attempt
                .end_without_stop(UnstoppedAttemptDisposition::YieldedToDurableWait)
                .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            assemble_tool_round(
                scope,
                ended_call,
                ended_attempt,
                frontier_entries.into_vec(),
                response,
                identities,
                dangerous_tool_auto_approval,
            )
            .map(ModelCallTerminalOutcome::ToolRound)
        }
        ModelCallTerminalObservation::KnownFailed => {
            let ModelCallTerminalIdentities::Failed(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                session,
                source_frontier.snapshot(),
                frontier_entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            let failed = match cancellation_proof {
                Some(proof) => close_failed_turn_after_cancellation(
                    scope,
                    attempt,
                    ended_call,
                    source,
                    proof,
                    identities,
                    reclassified_pending_steering,
                ),
                None => close_failed_turn(
                    scope,
                    attempt,
                    Some(ended_call),
                    source,
                    identities,
                    UnstoppedAttemptDisposition::KnownFailure,
                    reclassified_pending_steering,
                ),
            }?;
            Ok(ModelCallTerminalOutcome::Failed(failed))
        }
        ModelCallTerminalObservation::Cancelled => match cancellation_proof {
            Some(proof) => {
                let ModelCallTerminalIdentities::PhysicalCancellation(identities) = identities
                else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let identities = CancelledModelCallTurnIdentities {
                    cancellation_entry: identities.terminal_entry,
                    terminal_frontier: identities.terminal_frontier,
                    pending_steering_reclassifications: identities
                        .pending_steering_reclassifications,
                };
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                close_cancelled_turn(
                    scope,
                    Some(attempt),
                    Some(ended_call),
                    CancellationFrontierSource::new(source, &[]),
                    proof,
                    identities,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::Cancelled)
            }
            None => {
                let ModelCallTerminalIdentities::PhysicalCancellation(identities) = identities
                else {
                    return Err(ModelCallClosureError::IdentityShapeMismatch);
                };
                let identities = FailedModelCallTurnIdentities {
                    failure_entry: identities.terminal_entry,
                    terminal_frontier: identities.terminal_frontier,
                    pending_steering_reclassifications: identities
                        .pending_steering_reclassifications,
                };
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                close_failed_turn(
                    scope,
                    attempt,
                    Some(ended_call),
                    source,
                    identities,
                    UnstoppedAttemptDisposition::KnownFailure,
                    reclassified_pending_steering,
                )
                .map(ModelCallTerminalOutcome::Failed)
            }
        },
        observation @ ModelCallTerminalObservation::Refused
        | observation @ ModelCallTerminalObservation::RefusedWithProviderCompaction { .. } => {
            let ModelCallTerminalIdentities::Refused(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let ended_attempt = match cancellation_proof {
                Some(proof) => {
                    attempt.end_after_cancellation(proof, CancellationStopDisposition::TurnRefused)
                }
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::TurnRefused),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                session,
                source_frontier.snapshot(),
                frontier_entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            let provider_compaction =
                if let ModelCallTerminalObservation::RefusedWithProviderCompaction {
                    provider_compaction,
                    ..
                } = observation
                {
                    provider_compaction
                } else {
                    Vec::new()
                };
            if provider_compaction.len() != identities.provider_compaction_entries.len() {
                return Err(ModelCallClosureError::AssistantIdentityCountMismatch);
            }
            let mut used = frontier_entries
                .iter()
                .map(SemanticTranscriptEntry::identity)
                .collect::<BTreeSet<_>>();
            if identities
                .provider_compaction_entries
                .iter()
                .any(|identity| !used.insert(*identity))
            {
                return Err(ModelCallClosureError::FrontierDerivationFailed);
            }
            let provider_compaction_entries = identities
                .provider_compaction_entries
                .into_iter()
                .zip(provider_compaction)
                .map(|(identity, block)| {
                    SemanticTranscriptEntry::from_validated_parts(
                        identity,
                        session,
                        SemanticTranscriptEntryPayload::ProviderCompaction {
                            producing_call: ended_call.id(),
                            block,
                        },
                    )
                })
                .collect::<Vec<_>>();
            let terminal_snapshot = source
                .derive_appending_candidate(
                    identities.terminal_frontier,
                    provider_compaction_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
            Ok(ModelCallTerminalOutcome::Refused(RefusedModelCallTurn {
                session,
                turn,
                call: ended_call,
                attempt: ended_attempt,
                disposition: TurnDisposition::Refused,
                provider_compaction_entries: provider_compaction_entries.into_boxed_slice(),
                terminal_snapshot,
                reclassified_pending_steering,
            }))
        }
        ModelCallTerminalObservation::Ambiguous => {
            let ModelCallTerminalIdentities::Ambiguous(identities) = identities else {
                return Err(ModelCallClosureError::IdentityShapeMismatch);
            };
            let call_id = ended_call.id();
            let ended_attempt = match cancellation_proof {
                Some(proof) => {
                    attempt.end_after_cancellation(proof, CancellationStopDisposition::Ambiguous)
                }
                None => attempt.end_without_stop(UnstoppedAttemptDisposition::Ambiguous),
            }
            .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
            let ambiguous_operations = NonEmptyIssuedOperationRefs::try_from_operations([
                crate::IssuedOperationRef::ModelCall(call_id),
            ])
            .map_err(|_| ModelCallClosureError::AmbiguityConstructionFailed)?;
            if let Some(proof) = cancellation_proof {
                let source = ResolvedContextFrontierSnapshot::try_from_candidate(
                    session,
                    source_frontier.snapshot(),
                    frontier_entries
                        .iter()
                        .map(SemanticTranscriptEntry::reference)
                        .collect(),
                )
                .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let terminal_snapshot = source
                    .derive_appending_candidate(identities.terminal_frontier, Vec::new())
                    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
                let marker =
                    ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations, proof);
                Ok(ModelCallTerminalOutcome::ReconciliationRequired(
                    ReconciliationRequiredModelCallTurn {
                        session,
                        turn,
                        call: ended_call,
                        attempt: ended_attempt,
                        disposition: TurnDisposition::ReconciliationRequired { marker },
                        terminal_snapshot,
                        reclassified_pending_steering,
                    },
                ))
            } else {
                Ok(ModelCallTerminalOutcome::AwaitingRecovery(
                    AmbiguousModelCallTurn {
                        session,
                        turn,
                        call: ended_call,
                        attempt: ended_attempt,
                        ambiguous_operations,
                    },
                ))
            }
        }
    }
}

/// One fresh turn identity correlated to an exact pending steering input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingSteeringReclassificationIdentity {
    accepted_input: AcceptedInputId,
    turn: TurnId,
}

impl PendingSteeringReclassificationIdentity {
    /// Associates one pending accepted input with its proposed successor turn.
    pub const fn new(accepted_input: AcceptedInputId, turn: TurnId) -> Self {
        Self {
            accepted_input,
            turn,
        }
    }

    /// Returns the pending accepted input being reclassified.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the fresh turn proposed for that input.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
}

/// Fresh identities for a successful text-only outcome transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedModelCallIdentities {
    assistant_entries: Vec<SemanticTranscriptEntryId>,
    completion_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl CompletedModelCallIdentities {
    /// Supplies one identity per text part, the final marker, and frontier.
    pub fn new(
        assistant_entries: Vec<SemanticTranscriptEntryId>,
        completion_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            assistant_entries,
            completion_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Fresh identities and initial policy for one ordered tool-response part.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResponsePartIdentity {
    /// One semantic assistant-text entry.
    Text {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic provider-compaction entry.
    ProviderCompaction {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One logical request plus its reference-only semantic entry.
    ToolCall {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
        /// Fresh logical request identity.
        request: ToolRequestId,
        /// The explicit initial approval outcome selected by application policy.
        approval: InitialToolApproval,
    },
}

impl ToolResponsePartIdentity {
    /// Constructs a text-part identity.
    pub const fn text(entry: SemanticTranscriptEntryId) -> Self {
        Self::Text { entry }
    }

    /// Constructs a provider-compaction-part identity.
    pub const fn provider_compaction(entry: SemanticTranscriptEntryId) -> Self {
        Self::ProviderCompaction { entry }
    }

    /// Constructs a tool-part identity and explicit initial policy outcome.
    pub const fn tool_call(
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        approval: InitialToolApproval,
    ) -> Self {
        Self::ToolCall {
            entry,
            request,
            approval,
        }
    }
}

/// Fresh identities for one nonterminal tool-using response commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRoundModelCallIdentities {
    response_parts: Vec<ToolResponsePartIdentity>,
    yielded_frontier: ContextFrontierId,
    continuation_attempt: Option<TurnAttemptId>,
}

impl ToolRoundModelCallIdentities {
    /// Supplies one identity shape per response part, a yielded frontier, and
    /// a continuation attempt exactly when every request is auto-approved.
    pub fn new(
        response_parts: Vec<ToolResponsePartIdentity>,
        yielded_frontier: ContextFrontierId,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Self {
        Self {
            response_parts,
            yielded_frontier,
            continuation_attempt,
        }
    }

    /// Returns ordered response-part identities.
    pub fn response_parts(&self) -> &[ToolResponsePartIdentity] {
        &self.response_parts
    }

    /// Returns the proposed yielded snapshot identity.
    pub const fn yielded_frontier(&self) -> ContextFrontierId {
        self.yielded_frontier
    }

    /// Returns the proposed continuation attempt, if the batch has no wait.
    pub const fn continuation_attempt(&self) -> Option<TurnAttemptId> {
        self.continuation_attempt
    }
}

/// Fresh identities for one response part when an applied interrupt closes
/// newly proposed tools instead of continuing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoppedToolResponsePartIdentity {
    /// One semantic assistant-text entry.
    Text {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One semantic provider-compaction entry.
    ProviderCompaction {
        /// Fresh semantic-entry identity.
        entry: SemanticTranscriptEntryId,
    },
    /// One request, tool-use entry, and turn-closed result entry.
    ToolCall {
        /// Fresh assistant tool-use entry identity.
        entry: SemanticTranscriptEntryId,
        /// Fresh logical request identity.
        request: ToolRequestId,
        /// Fresh reference-only closed-result entry identity.
        closed_result_entry: SemanticTranscriptEntryId,
        /// Frozen policy outcome for the request.
        approval: InitialToolApproval,
    },
}

impl StoppedToolResponsePartIdentity {
    /// Constructs one text identity.
    pub const fn text(entry: SemanticTranscriptEntryId) -> Self {
        Self::Text { entry }
    }

    /// Constructs one provider-compaction identity.
    pub const fn provider_compaction(entry: SemanticTranscriptEntryId) -> Self {
        Self::ProviderCompaction { entry }
    }

    /// Constructs one closed tool-proposal identity group.
    pub const fn tool_call(
        entry: SemanticTranscriptEntryId,
        request: ToolRequestId,
        closed_result_entry: SemanticTranscriptEntryId,
        approval: InitialToolApproval,
    ) -> Self {
        Self::ToolCall {
            entry,
            request,
            closed_result_entry,
            approval,
        }
    }
}

/// Fresh identities for a tool-using response closed by an applied interrupt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoppedToolRoundModelCallIdentities {
    response_parts: Vec<StoppedToolResponsePartIdentity>,
    cancellation_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl StoppedToolRoundModelCallIdentities {
    /// Supplies ordered response identities, the cancellation marker, and
    /// terminal snapshot.
    pub fn new(
        response_parts: Vec<StoppedToolResponsePartIdentity>,
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            response_parts,
            cancellation_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one successor identity per pending steering input.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Fresh identities for a failed-turn outcome transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedModelCallTurnIdentities {
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl FailedModelCallTurnIdentities {
    /// Supplies the failure marker and terminal-frontier identities.
    pub fn new(
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            failure_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }

    /// Returns the failure-marker identity this bundle mints.
    ///
    /// Exposed alongside [`Self::terminal_frontier`] because the two are
    /// minted into one collision domain: a retry after an identity collision
    /// has to refresh *both*, and a caller that can only compare whole
    /// bundles cannot tell a full refresh from one that reused half.
    pub const fn failure_entry(&self) -> SemanticTranscriptEntryId {
        self.failure_entry
    }

    /// Returns the terminal-frontier identity this bundle mints.
    pub const fn terminal_frontier(&self) -> ContextFrontierId {
        self.terminal_frontier
    }
}

/// Fresh identities for an interrupt-cancelled turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledModelCallTurnIdentities {
    cancellation_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

/// Fresh identities for a physical-cancellation observation.
///
/// The freshly reloaded attempt decides whether the terminal entry is a
/// proof-bearing cancellation marker or an ordinary failure marker. This
/// shape lets the application mint one collision domain without guessing
/// whether a concurrent interrupt committed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalCancellationModelCallTurnIdentities {
    terminal_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl PhysicalCancellationModelCallTurnIdentities {
    /// Supplies the terminal-marker and terminal-frontier identities.
    pub fn new(
        terminal_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            terminal_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

impl CancelledModelCallTurnIdentities {
    /// Supplies the cancellation marker and terminal-frontier identities.
    pub fn new(
        cancellation_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            cancellation_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }

    /// Reuses the terminal frontier and pending-steering successors when an
    /// interrupt closes an existing ambiguity wait instead of emitting a
    /// cancellation marker.
    pub fn into_ambiguous(self) -> AmbiguousModelCallTurnIdentities {
        AmbiguousModelCallTurnIdentities {
            terminal_frontier: self.terminal_frontier,
            pending_steering_reclassifications: self.pending_steering_reclassifications,
        }
    }
}

/// Fresh identity for a refusal terminal frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefusedModelCallTurnIdentities {
    provider_compaction_entries: Vec<SemanticTranscriptEntryId>,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl RefusedModelCallTurnIdentities {
    /// Supplies the new equal-content terminal frontier identity.
    pub fn new(terminal_frontier: ContextFrontierId) -> Self {
        Self {
            provider_compaction_entries: Vec::new(),
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one semantic identity per retained provider compaction block.
    pub fn with_provider_compaction_entries(
        mut self,
        identities: Vec<SemanticTranscriptEntryId>,
    ) -> Self {
        self.provider_compaction_entries = identities;
        self
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// Candidate identities matching one possible terminal observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalIdentities {
    /// Successful assistant-content and completion identities.
    Completed(CompletedModelCallIdentities),
    /// Assistant content, logical requests, and a nonterminal yielded frontier.
    ToolRound(ToolRoundModelCallIdentities),
    /// Tool response content and closed results under an applied interrupt.
    StoppedToolRound(StoppedToolRoundModelCallIdentities),
    /// Known-failure or cause-free physical-cancellation identities.
    Failed(FailedModelCallTurnIdentities),
    /// Physical-cancellation identities whose semantic meaning is selected
    /// from the freshly reloaded stop state.
    PhysicalCancellation(PhysicalCancellationModelCallTurnIdentities),
    /// Refusal terminal-frontier identity.
    Refused(RefusedModelCallTurnIdentities),
    /// Ambiguity identities used only when a stop requires terminal
    /// reconciliation; ordinary ambiguity ignores them while retaining the
    /// slot.
    Ambiguous(AmbiguousModelCallTurnIdentities),
}

/// One terminal or durable-wait result from the observation transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallTerminalOutcome {
    /// Assistant content and turn completion committed atomically.
    Completed(CompletedModelCallTurn),
    /// Assistant content and requests committed while the same turn continues.
    ToolRound(ToolRoundModelCallTurn),
    /// A tool-using response raced an interrupt and closed without execution.
    CancelledWithToolResponse(CancelledToolRoundModelCallTurn),
    /// The call and turn failed atomically.
    Failed(FailedModelCallTurn),
    /// The applied interrupt and physical evidence cancelled the turn.
    Cancelled(CancelledModelCallTurn),
    /// The provider refusal terminalized the turn.
    Refused(RefusedModelCallTurn),
    /// An applied interrupt and exact ambiguity set require reconciliation.
    ReconciliationRequired(ReconciliationRequiredModelCallTurn),
    /// Physical ambiguity ended the attempt and retained the slot.
    AwaitingRecovery(AmbiguousModelCallTurn),
}

/// Result of atomically applying one matching interrupt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallInterruptOutcome {
    /// Unsent work ended directly and released the turn slot.
    Cancelled(CancelledModelCallTurn),
    /// Issued work retained the slot while durable cancellation was requested.
    CancellationRequested(StopRequestedModelCallTurn),
    /// An existing physical-ambiguity wait closed under the applied interrupt.
    ReconciliationRequired(ReconciliationRequiredModelCallTurn),
    /// An existing tool-attempt ambiguity closed under the applied interrupt.
    ToolReconciliationRequired(ReconciliationRequiredToolTurn),
}

impl ModelCallTerminalIdentities {
    fn pending_steering_reclassifications(&self) -> &[PendingSteeringReclassificationIdentity] {
        match self {
            Self::Completed(identities) => &identities.pending_steering_reclassifications,
            Self::ToolRound(_) => &[],
            Self::StoppedToolRound(identities) => &identities.pending_steering_reclassifications,
            Self::Failed(identities) => &identities.pending_steering_reclassifications,
            Self::PhysicalCancellation(identities) => {
                &identities.pending_steering_reclassifications
            }
            Self::Refused(identities) => &identities.pending_steering_reclassifications,
            Self::Ambiguous(identities) => &identities.pending_steering_reclassifications,
        }
    }
}

/// Fresh identities needed only when ambiguity terminalizes under a stop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbiguousModelCallTurnIdentities {
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl AmbiguousModelCallTurnIdentities {
    /// Supplies the candidate terminal frontier for proof-bearing
    /// reconciliation.
    pub const fn new(terminal_frontier: ContextFrontierId) -> Self {
        Self {
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }
}

/// One pending steering input atomically reclassified when its source turn
/// terminalizes before another model-call safe point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReclassifiedPendingSteeringTurn {
    session: SessionId,
    source_turn: TurnId,
    accepted_input: AcceptedInputLifecycle,
    turn: TurnId,
    order: AcceptedInputQueueOrder,
    binding: SteeringBinding,
    effective_configuration: EffectiveConfiguration,
}

impl ReclassifiedPendingSteeringTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the terminal source turn.
    pub const fn source_turn(&self) -> TurnId {
        self.source_turn
    }

    /// Borrows the accepted input with its reclassified disposition.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the fresh queued turn originated by the input.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns ordinary queue order at the input's original position.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Returns inherited provenance binding the new origin to its source.
    pub const fn binding(&self) -> SteeringBinding {
        self.binding
    }

    /// Borrows the source turn's exact inherited effective configuration.
    pub const fn effective_configuration(&self) -> &EffectiveConfiguration {
        &self.effective_configuration
    }
}

/// One successful completed-turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    assistant_entries: Box<[SemanticTranscriptEntry]>,
    completion_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

/// One nonterminal commit candidate from a tool-using completed model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRoundModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    assistant_entries: Box<[SemanticTranscriptEntry]>,
    requests: Box<[ToolRequest]>,
    automatic_approvals: Box<[ToolApprovalResolution]>,
    yielded_snapshot: ResolvedContextFrontierSnapshot,
    next_phase: ActiveTurnPhase,
}

/// One availability-failed call and the distinct prepared attempt succeeding it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailabilitySuccessorModelCallTurn {
    session: SessionId,
    turn: TurnId,
    predecessor_call: EndedModelCall,
    predecessor_attempt: EndedTurnAttempt,
    successor_attempt: CurrentTurnAttempt,
}

impl AvailabilitySuccessorModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the continuing logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the terminal failed predecessor call.
    pub const fn predecessor_call(&self) -> &EndedModelCall {
        &self.predecessor_call
    }

    /// Borrows the terminal failed predecessor attempt.
    pub const fn predecessor_attempt(&self) -> &EndedTurnAttempt {
        &self.predecessor_attempt
    }

    /// Borrows the fresh prepared successor attempt.
    pub const fn successor_attempt(&self) -> &CurrentTurnAttempt {
        &self.successor_attempt
    }
}

impl ToolRoundModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the continuing logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the completed producing call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }

    /// Borrows the yielded producing attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }

    /// Returns ordered text/tool-use semantic entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }

    /// Returns logical requests in proposal order.
    pub fn requests(&self) -> &[ToolRequest] {
        &self.requests
    }

    /// Returns only automatic decisions, in proposal order among those selected.
    pub fn automatic_approvals(&self) -> &[ToolApprovalResolution] {
        &self.automatic_approvals
    }

    /// Borrows the source-plus-assistant yielded snapshot.
    pub const fn yielded_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.yielded_snapshot
    }

    /// Borrows the approval wait or prepared continuation attempt.
    pub const fn next_phase(&self) -> &ActiveTurnPhase {
        &self.next_phase
    }
}

/// One interrupt-cancelled turn whose racing response proposed tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledToolRoundModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    assistant_entries: Box<[SemanticTranscriptEntry]>,
    requests: Box<[ToolRequest]>,
    closed_result_entries: Box<[SemanticTranscriptEntry]>,
    cancellation_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl CancelledToolRoundModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the cancelled logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the completed producing call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }

    /// Borrows the proof-bearing ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }

    /// Borrows the cancelled disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }

    /// Returns ordered assistant response entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }

    /// Returns proposed logical requests in proposal order.
    pub fn requests(&self) -> &[ToolRequest] {
        &self.requests
    }

    /// Returns proposal-ordered closed-result entries.
    pub fn closed_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.closed_result_entries
    }

    /// Borrows the final proof-bearing cancellation marker.
    pub const fn cancellation_entry(&self) -> &SemanticTranscriptEntry {
        &self.cancellation_entry
    }

    /// Borrows the complete terminal snapshot.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }

    /// Returns successor turns for pending steering.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

impl CompletedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the completed turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the completed physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the completed turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Returns ordered assistant text entries.
    pub fn assistant_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.assistant_entries
    }
    /// Borrows the final completion marker.
    pub const fn completion_entry(&self) -> &SemanticTranscriptEntry {
        &self.completion_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One failed-turn commit candidate, with an optional physical call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: Option<EndedModelCall>,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    failure_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl FailedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the failed turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the physical call when one existed.
    pub const fn call(&self) -> Option<&EndedModelCall> {
        self.call.as_ref()
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the failed turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows the explicit failure marker.
    pub const fn failure_entry(&self) -> &SemanticTranscriptEntry {
        &self.failure_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// Typed terminal failure for a pool that admitted no credential member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialPoolExhaustedModelCallTurn {
    pool_name: String,
    failed: FailedModelCallTurn,
}

/// Typed terminal boundary that requires compaction before another model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextHeadroomExhaustedModelCallTurn {
    producing_call: ModelCallId,
    failed: FailedModelCallTurn,
}

impl ContextHeadroomExhaustedModelCallTurn {
    /// Returns the completed tool-producing call whose usage exhausted headroom.
    pub const fn producing_call(&self) -> ModelCallId {
        self.producing_call
    }

    /// Borrows the ordinary failed-turn projection committed for clients.
    pub const fn failed(&self) -> &FailedModelCallTurn {
        &self.failed
    }

    /// Consumes the typed boundary into its failed-turn persistence payload.
    pub fn into_failed(self) -> FailedModelCallTurn {
        self.failed
    }
}

impl CredentialPoolExhaustedModelCallTurn {
    /// Borrows the deployment-owned pool name whose members were unavailable.
    pub fn pool_name(&self) -> &str {
        &self.pool_name
    }

    /// Borrows the ordinary failed-turn projection committed for the client.
    pub const fn failed(&self) -> &FailedModelCallTurn {
        &self.failed
    }

    /// Consumes the typed cause into its failed-turn persistence payload.
    pub fn into_failed(self) -> FailedModelCallTurn {
        self.failed
    }
}

/// One interrupt-cancelled turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: Option<EndedModelCall>,
    attempt: Option<EndedTurnAttempt>,
    disposition: TurnDisposition,
    tool_result_entries: Box<[SemanticTranscriptEntry]>,
    cancellation_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl CancelledModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the cancelled turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the physical call when one existed.
    pub const fn call(&self) -> Option<&EndedModelCall> {
        self.call.as_ref()
    }
    /// Borrows the ended attempt when cancellation closed live execution.
    pub const fn attempt(&self) -> Option<&EndedTurnAttempt> {
        self.attempt.as_ref()
    }
    /// Borrows the proof-bearing cancelled disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows proposal-ordered tool results materialized before cancellation.
    pub fn tool_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.tool_result_entries
    }
    /// Borrows the explicit cancellation marker.
    pub const fn cancellation_entry(&self) -> &SemanticTranscriptEntry {
        &self.cancellation_entry
    }
    /// Borrows the complete terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One durable cancellation request retaining the active turn slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopRequestedModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: CurrentModelCall,
    attempt: CurrentTurnAttempt,
    interrupt: AppliedInterruptProof,
}

impl StopRequestedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the stopped active turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact cancellation-requested call.
    pub const fn call(&self) -> &CurrentModelCall {
        &self.call
    }
    /// Borrows the proof-bearing stop-requested attempt.
    pub const fn attempt(&self) -> &CurrentTurnAttempt {
        &self.attempt
    }
    /// Returns the applied interrupt authorizing cancellation.
    pub const fn interrupt(&self) -> AppliedInterruptProof {
        self.interrupt
    }

    /// Returns the issued facts binding a provider-neutral cancellation
    /// observation to this exact stopped authorization.
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

/// One refused-turn commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefusedModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    provider_compaction_entries: Box<[SemanticTranscriptEntry]>,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl RefusedModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the refused turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the refused physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the refused turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Returns provider compaction entries retained before refusal.
    pub fn provider_compaction_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.provider_compaction_entries
    }
    /// Borrows the terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One proof-bearing reconciliation-required commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationRequiredModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl ReconciliationRequiredModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the turn whose ambiguity requires reconciliation.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact ambiguous physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the proof-bearing ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact reconciliation disposition and marker.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Borrows the terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One proof-bearing tool-attempt reconciliation commit candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationRequiredToolTurn {
    session: SessionId,
    turn: TurnId,
    tool_attempt: EndedToolAttempt,
    attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    tool_result_entries: Box<[SemanticTranscriptEntry]>,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl ReconciliationRequiredToolTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the turn whose ambiguity requires reconciliation.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the exact ambiguous physical tool attempt.
    pub const fn tool_attempt(&self) -> &EndedToolAttempt {
        &self.tool_attempt
    }
    /// Borrows the proof-bearing ended turn attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact reconciliation disposition and marker.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }
    /// Returns proposal-ordered logical results closing the terminal batch.
    pub fn tool_result_entries(&self) -> &[SemanticTranscriptEntry] {
        &self.tool_result_entries
    }
    /// Borrows the prefix-extending terminal frontier.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }
    /// Returns queued turns created from every pending steering input.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }
}

/// One ambiguity wait candidate retaining immutable physical history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbiguousModelCallTurn {
    session: SessionId,
    turn: TurnId,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    ambiguous_operations: NonEmptyIssuedOperationRefs,
}

impl AmbiguousModelCallTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
    /// Returns the active turn retaining the slot.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }
    /// Borrows the ambiguous physical call.
    pub const fn call(&self) -> &EndedModelCall {
        &self.call
    }
    /// Borrows the ended attempt.
    pub const fn attempt(&self) -> &EndedTurnAttempt {
        &self.attempt
    }
    /// Borrows the exact recovery wait set.
    pub const fn ambiguous_operations(&self) -> &NonEmptyIssuedOperationRefs {
        &self.ambiguous_operations
    }
}

/// Why a guarded terminal candidate could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCallClosureError {
    /// Candidate identities do not match the observed disposition.
    IdentityShapeMismatch,
    /// The call cannot take the requested terminal transition.
    CallStateMismatch,
    /// The observation names different issued authority than fresh state.
    ObservationCorrelationMismatch,
    /// The applied interrupt does not name this exact session, predecessor,
    /// and immediate successor relation.
    InterruptCorrelationMismatch,
    /// The attempt cannot take the required terminal transition.
    AttemptStateMismatch,
    /// Claimed target-resolution failure does not match this execution's
    /// immutable catalog and frozen selection.
    TargetResolutionMismatch,
    /// Assistant text and entry identity counts differ.
    AssistantIdentityCountMismatch,
    /// Tool response parts and their identity shapes differ.
    ToolResponseIdentityMismatch,
    /// The request ordinal cannot fit the durable zero-based space.
    ToolRequestOrdinalOverflow,
    /// Initial approval provenance contradicts the frozen blanket posture.
    InitialToolApprovalMismatch,
    /// A continuation attempt was missing, unexpected, or reused the yielded attempt.
    ContinuationAttemptIdentityMismatch,
    /// Pending steering and proposed successor identities are not exact,
    /// ordered, distinct, and source-turn-safe.
    PendingSteeringReclassificationMismatch,
    /// The exact terminal frontier could not preserve its source prefix.
    FrontierDerivationFailed,
    /// The exact nonempty ambiguity set could not be constructed.
    AmbiguityConstructionFailed,
}

fn reconstitute(
    input: ModelCallExecutionReconstitutionInput,
) -> Result<ModelCallExecution, ModelCallExecutionReconstitutionError> {
    let fail = |input, failure| ModelCallExecutionReconstitutionError {
        input: Box::new(input),
        failure,
    };
    let ActiveTurnPhase::Running { current_attempt } = input.active_turn.phase() else {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::TurnIsNotRunning,
        ));
    };
    let current_attempt = current_attempt.clone();
    let session = input.active_turn.session();
    let turn = input.active_turn.turn();
    let configuration = input.active_turn.configuration().clone();
    let start = input.active_turn.start();
    if input.starting_snapshot.frontier().owning_session() != session {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::StartingSnapshotSessionMismatch,
        ));
    }
    if start.frontier() != input.starting_snapshot.frontier() {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::StartingSnapshotMismatch,
        ));
    }
    if input.calls.len() > 1 {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::MultipleCalls,
        ));
    }
    let current_snapshot = match (
        input.calls.first(),
        input.call_snapshot.as_ref(),
        input.continuation_snapshot.as_ref(),
    ) {
        (None, None, None) => input.starting_snapshot.clone(),
        (None, Some(_), _) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallSnapshotUnexpected,
            ));
        }
        (None, None, Some(stored)) => {
            let Some(current) = stored.clone().reconstitute() else {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::ContinuationSnapshotMismatch,
                ));
            };
            if current.frontier().owning_session() != session
                || current.frontier() == input.starting_snapshot.frontier()
                || !input.starting_snapshot.is_semantic_prefix_of(&current)
            {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::ContinuationSnapshotMismatch,
                ));
            }
            current
        }
        (Some(_), _, Some(_)) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ContinuationSnapshotUnexpected,
            ));
        }
        (Some(call), None, None)
            if call.frontier() == input.starting_snapshot.frontier().snapshot() =>
        {
            input.starting_snapshot.clone()
        }
        (Some(_), None, None) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallSnapshotMissing,
            ));
        }
        (Some(call), Some(stored), None) => {
            if call.frontier() == input.starting_snapshot.frontier().snapshot() {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::CallSnapshotUnexpected,
                ));
            }
            let owner = stored.owning_session();
            let snapshot = stored.snapshot();
            let Some(current) = stored.clone().reconstitute() else {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::CallSnapshotMismatch,
                ));
            };
            if owner != session
                || snapshot != call.frontier()
                || current.entry_count() == input.starting_snapshot.entry_count()
                || !input.starting_snapshot.is_semantic_prefix_of(&current)
            {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::CallSnapshotMismatch,
                ));
            }
            current
        }
    };
    if input
        .frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .ne(current_snapshot.ordered_entries())
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::FrontierEntryMismatch,
        ));
    }
    let mut origin_contents = BTreeMap::new();
    for origin in &input.origin_contents {
        if origin_contents
            .insert(origin.accepted_input, origin.content.clone())
            .is_some()
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::DuplicateOriginContent,
            ));
        }
    }
    let mut referenced_origins = BTreeSet::new();
    for entry in &input.frontier_entries {
        let accepted_input = match entry.payload() {
            SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { accepted_input, .. } => {
                Some(*accepted_input)
            }
            SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::Imported { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | SemanticTranscriptEntryPayload::ToolDenied { .. }
            | SemanticTranscriptEntryPayload::ToolClosed { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. } => None,
        };
        if let Some(accepted_input) = accepted_input {
            if !origin_contents.contains_key(&accepted_input) {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::MissingOriginContent,
                ));
            }
            referenced_origins.insert(accepted_input);
        }
    }
    let pending_inputs = input
        .active_turn
        .pending_steering()
        .iter()
        .map(crate::PendingSteeringInput::accepted_input)
        .collect::<BTreeSet<_>>();
    if pending_inputs
        .iter()
        .any(|accepted_input| !origin_contents.contains_key(accepted_input))
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::MissingOriginContent,
        ));
    }
    if origin_contents.keys().any(|accepted_input| {
        !referenced_origins.contains(accepted_input) && !pending_inputs.contains(accepted_input)
    }) {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::UnreferencedOriginContent,
        ));
    }
    let referenced_attachments = origin_contents
        .values()
        .flat_map(UserContent::parts)
        .filter_map(|part| match part {
            crate::UserContentPart::Attachment { digest, .. } => Some(*digest),
            crate::UserContentPart::Text { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    let mut attachment_blob_facts = BTreeMap::new();
    for fact in &input.attachment_blob_facts {
        if attachment_blob_facts
            .insert(fact.digest(), fact.byte_length())
            .is_some()
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::AttachmentBlobFactMismatch,
            ));
        }
    }
    if attachment_blob_facts
        .keys()
        .copied()
        .ne(referenced_attachments)
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::AttachmentBlobFactMismatch,
        ));
    }
    let consumed = input.active_turn.consumed_steering();
    let consumed_entries = input
        .frontier_entries
        .iter()
        .skip(input.starting_snapshot.entry_count())
        .filter(|entry| {
            matches!(
                entry.payload(),
                SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            )
        })
        .collect::<Vec<_>>();
    if consumed.len() != consumed_entries.len()
        || consumed
            .iter()
            .zip(consumed_entries)
            .any(|(consumed, entry)| {
                !matches!(
                    (consumed.lifecycle().disposition(), entry.payload()),
                    (
                        AcceptedInputDisposition::ConsumedAsSteering { .. },
                        SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                            accepted_input,
                            source_turn,
                        },
                    ) if *accepted_input == consumed.accepted_input()
                        && *source_turn == consumed.source_turn()
                        && *source_turn == turn
                )
            })
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::ConsumedSteeringMismatch,
        ));
    }
    let pinned_target = match (
        input.pinned_target,
        input.calls.first(),
        input.continuation_snapshot.as_ref(),
        input.availability_successor,
    ) {
        (None, None, None, false) => None,
        (None, None, Some(_), _) | (None, Some(_), _, _) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::PinnedTargetMissing,
            ));
        }
        (Some(_), None, None, false) | (None, None, None, true) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::PinnedTargetUnexpected,
            ));
        }
        (Some(stored), Some(_), None, false)
        | (Some(stored), Some(_), None, true)
        | (Some(stored), None, Some(_), false)
        | (Some(stored), None, None, true) => {
            let Some(pinned) = stored.reconstitute_for_turn(turn) else {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::PinnedTargetTurnMismatch,
                ));
            };
            Some(pinned)
        }
        (Some(_), Some(_), Some(_), _) | (Some(_), None, Some(_), true) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ContinuationSnapshotUnexpected,
            ));
        }
    };
    if let Some(pinned) = pinned_target
        && input
            .targets
            .resolve(*configuration.effective().model())
            .is_ok_and(|resolution| pinned.target() != resolution.target())
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::CallTargetMismatch,
        ));
    }
    let current_call = if let Some(call) = input.calls.first() {
        if call.turn() != turn
            || call.attempt() != current_attempt.id()
            || call.frontier() != current_snapshot.frontier().snapshot()
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallOwnershipMismatch,
            ));
        }
        if call.selection() != *configuration.effective().model() {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallSelectionMismatch,
            ));
        }
        let Some(pinned) = pinned_target else {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::PinnedTargetMissing,
            ));
        };
        if call.target() != pinned.target()
            || input
                .targets
                .resolve(call.selection())
                .is_ok_and(|resolution| pinned.target() != resolution.target())
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::CallTargetMismatch,
            ));
        }
        match call.reconstitute(&current_snapshot, pinned) {
            Ok(ReconstitutedModelCall::Current(call)) => Some(call),
            Ok(ReconstitutedModelCall::Ended(_)) | Err(_) => {
                return Err(fail(
                    input,
                    ModelCallExecutionReconstitutionFailure::InvalidCall,
                ));
            }
        }
    } else {
        None
    };
    let referenced_tool_attempts = input
        .frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => Some(*attempt),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut tool_result_correlations = BTreeMap::new();
    for correlation in &input.tool_result_correlations {
        if tool_result_correlations
            .insert(correlation.attempt(), *correlation)
            .is_some()
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ToolResultCorrelationMismatch,
            ));
        }
    }
    if referenced_tool_attempts.len() != tool_result_correlations.len()
        || referenced_tool_attempts
            .iter()
            .any(|attempt| !tool_result_correlations.contains_key(attempt))
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::ToolResultCorrelationMismatch,
        ));
    }
    let referenced_tool_denials = input
        .frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::ToolDenied { request } => Some(*request),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut tool_denial_correlations = BTreeSet::new();
    for correlation in &input.tool_denial_correlations {
        if !matches!(correlation.decision(), ToolApprovalDecision::Deny { .. })
            || !tool_denial_correlations.insert(correlation.request())
        {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ToolDenialCorrelationMismatch,
            ));
        }
    }
    if referenced_tool_denials.len() != tool_denial_correlations.len()
        || referenced_tool_denials
            .iter()
            .any(|request| !tool_denial_correlations.contains(request))
    {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::ToolDenialCorrelationMismatch,
        ));
    }
    let running_tool_round =
        frontier_contains_tool_round(&input.starting_snapshot, &input.frontier_entries);
    let running_tool_continuation = match frontier_closes_latest_tool_round(
        &input.starting_snapshot,
        &input.frontier_entries,
        &tool_result_correlations,
        &tool_denial_correlations,
    ) {
        Ok(closed) => closed,
        Err(()) => {
            return Err(fail(
                input,
                ModelCallExecutionReconstitutionFailure::ToolResultCorrelationMismatch,
            ));
        }
    };
    let uncommitted_tool_result_projection = input
        .uncommitted_tool_result_projection
        .as_ref()
        .is_some_and(|projection| {
            projection.turn() == turn
                && projection.snapshot() == &current_snapshot
                && projection.entries().len() <= current_snapshot.entry_count()
                && projection
                    .entries()
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .eq(current_snapshot
                        .ordered_entries()
                        .skip(current_snapshot.entry_count() - projection.entries().len()))
        });
    if input.uncommitted_tool_result_projection.is_some() && !uncommitted_tool_result_projection {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::ContinuationSnapshotMismatch,
        ));
    }
    let lifecycle_valid = matches!(
        (
            current_attempt.state(),
            current_call.as_ref().map(CurrentModelCall::state)
        ),
        (CurrentTurnAttemptState::Prepared, None)
            if !running_tool_round
                || (running_tool_continuation && uncommitted_tool_result_projection)
    ) || matches!(
        (
            current_attempt.state(),
            current_call.as_ref().map(CurrentModelCall::state)
        ),
        (
            CurrentTurnAttemptState::Prepared,
            Some(CurrentModelCallState::Prepared)
        ) if !running_tool_round || running_tool_continuation
    ) || matches!(
        (
            current_attempt.state(),
            current_call.as_ref().map(CurrentModelCall::state)
        ),
        (
            CurrentTurnAttemptState::Running,
            Some(CurrentModelCallState::InFlight)
        ) | (
            CurrentTurnAttemptState::StopRequested {
                causes: TurnAttemptStopCauses::CancellationOnly { .. }
            },
            Some(CurrentModelCallState::CancellationRequested)
        )
    ) || matches!(
        (
            current_attempt.state(),
            current_call.as_ref().map(CurrentModelCall::state)
        ),
        (CurrentTurnAttemptState::Running, None)
            if (running_tool_round && !running_tool_continuation)
                || (running_tool_continuation && uncommitted_tool_result_projection)
    ) || matches!(
        (
            current_attempt.state(),
            current_call.as_ref().map(CurrentModelCall::state)
        ),
        (
            CurrentTurnAttemptState::Running,
            Some(CurrentModelCallState::Prepared)
        ) if running_tool_continuation
    );
    if !lifecycle_valid {
        return Err(fail(
            input,
            ModelCallExecutionReconstitutionFailure::LifecycleMismatch,
        ));
    }
    Ok(ModelCallExecution {
        active_turn: input.active_turn,
        session,
        turn,
        configuration,
        start,
        targets: input.targets,
        current_attempt,
        starting_snapshot: input.starting_snapshot,
        current_snapshot,
        frontier_entries: input.frontier_entries.into_boxed_slice(),
        origin_contents,
        attachment_blob_facts,
        pinned_target,
        current_call,
        tool_continuation_frontier: running_tool_continuation,
    })
}

fn frontier_closes_latest_tool_round(
    starting_snapshot: &ResolvedContextFrontierSnapshot,
    frontier_entries: &[SemanticTranscriptEntry],
    tool_result_correlations: &BTreeMap<crate::ToolAttemptId, ToolResultAttemptCorrelation>,
    tool_denial_correlations: &BTreeSet<crate::ToolRequestId>,
) -> Result<bool, ()> {
    let suffix = &frontier_entries[starting_snapshot.entry_count()..];
    let Some((last_tool_use, producing_call)) =
        suffix
            .iter()
            .enumerate()
            .rev()
            .find_map(|(position, entry)| match entry.payload() {
                SemanticTranscriptEntryPayload::AssistantToolUse { producing_call, .. } => {
                    Some((position, *producing_call))
                }
                _ => None,
            })
    else {
        return Ok(false);
    };
    let response_start = (0..=last_tool_use)
        .rev()
        .take_while(|index| assistant_entry_call(&suffix[*index]) == Some(producing_call))
        .last()
        .unwrap_or(last_tool_use);
    let response_end = (last_tool_use..suffix.len())
        .take_while(|index| assistant_entry_call(&suffix[*index]) == Some(producing_call))
        .last()
        .map_or(last_tool_use + 1, |index| index + 1);
    let requests = suffix[response_start..response_end]
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. } => Some(*request),
            SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult { .. }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::Imported { .. }
            | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | SemanticTranscriptEntryPayload::ToolDenied { .. }
            | SemanticTranscriptEntryPayload::ToolClosed { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. } => None,
        })
        .collect::<Vec<_>>();
    let Some(results_end) = response_end.checked_add(requests.len()) else {
        return Ok(false);
    };
    if requests.is_empty() || results_end > suffix.len() {
        return Ok(false);
    }
    for (entry, request) in suffix[response_end..results_end].iter().zip(&requests) {
        let valid = match entry.payload() {
            SemanticTranscriptEntryPayload::ToolExecutionResult { attempt } => {
                let Some(correlation) = tool_result_correlations.get(attempt) else {
                    return Err(());
                };
                if correlation.request() != *request
                    || correlation.producing_call() != producing_call
                {
                    return Err(());
                }
                true
            }
            SemanticTranscriptEntryPayload::ToolDenied {
                request: result_request,
            } => result_request == request && tool_denial_correlations.contains(result_request),
            SemanticTranscriptEntryPayload::DelegationResult {
                awaiting_request,
                mode: DelegationWaitMode::Foreground,
                ..
            } => awaiting_request == request,
            SemanticTranscriptEntryPayload::ToolClosed { .. } => false,
            SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::DelegatedTask { .. }
            | SemanticTranscriptEntryPayload::DelegationMessage { .. }
            | SemanticTranscriptEntryPayload::DelegationResult {
                mode: DelegationWaitMode::Background,
                ..
            }
            | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
            | SemanticTranscriptEntryPayload::ContextSummary { .. }
            | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
            | SemanticTranscriptEntryPayload::TurnFailed { .. }
            | SemanticTranscriptEntryPayload::Imported { .. }
            | SemanticTranscriptEntryPayload::AssistantText { .. }
            | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
            | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
            | SemanticTranscriptEntryPayload::TurnCompleted { .. }
            | SemanticTranscriptEntryPayload::TurnCancelled { .. } => false,
        };
        if !valid {
            return Ok(false);
        }
    }
    Ok(suffix[results_end..].iter().all(|entry| {
        matches!(
            entry.payload(),
            SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
        )
    }))
}

fn assistant_entry_call(entry: &SemanticTranscriptEntry) -> Option<ModelCallId> {
    match entry.payload() {
        SemanticTranscriptEntryPayload::AssistantText { producing_call, .. }
        | SemanticTranscriptEntryPayload::ProviderCompaction { producing_call, .. }
        | SemanticTranscriptEntryPayload::AssistantToolUse { producing_call, .. } => {
            Some(*producing_call)
        }
        SemanticTranscriptEntryPayload::OriginAcceptedInput { .. }
        | SemanticTranscriptEntryPayload::DelegatedTask { .. }
        | SemanticTranscriptEntryPayload::DelegationMessage { .. }
        | SemanticTranscriptEntryPayload::DelegationResult { .. }
        | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
        | SemanticTranscriptEntryPayload::ContextSummary { .. }
        | SemanticTranscriptEntryPayload::SteeringAcceptedInput { .. }
        | SemanticTranscriptEntryPayload::TurnFailed { .. }
        | SemanticTranscriptEntryPayload::Imported { .. }
        | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
        | SemanticTranscriptEntryPayload::ToolDenied { .. }
        | SemanticTranscriptEntryPayload::ToolClosed { .. }
        | SemanticTranscriptEntryPayload::TurnCompleted { .. }
        | SemanticTranscriptEntryPayload::TurnCancelled { .. } => None,
    }
}

fn frontier_contains_tool_round(
    starting_snapshot: &ResolvedContextFrontierSnapshot,
    frontier_entries: &[SemanticTranscriptEntry],
) -> bool {
    frontier_entries
        .iter()
        .skip(starting_snapshot.entry_count())
        .any(|entry| {
            matches!(
                entry.payload(),
                SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                    | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                    | SemanticTranscriptEntryPayload::ToolDenied { .. }
                    | SemanticTranscriptEntryPayload::ToolClosed { .. }
            )
        })
}

fn reclassify_pending_steering(
    active_turn: &ActivatedTurn,
    identities: &[PendingSteeringReclassificationIdentity],
) -> Result<Box<[ReclassifiedPendingSteeringTurn]>, ModelCallClosureError> {
    reclassify_pending_steering_inputs(
        active_turn.session(),
        active_turn.turn(),
        active_turn.pending_steering(),
        identities,
        active_turn.configuration().effective(),
    )
}

pub(crate) fn reclassify_pending_steering_inputs(
    session: SessionId,
    source_turn: TurnId,
    pending: &[crate::PendingSteeringInput],
    identities: &[PendingSteeringReclassificationIdentity],
    effective_configuration: &EffectiveConfiguration,
) -> Result<Box<[ReclassifiedPendingSteeringTurn]>, ModelCallClosureError> {
    if pending.len() != identities.len() {
        return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
    }

    let mut turns = BTreeSet::new();
    let mut reclassified = Vec::with_capacity(pending.len());
    for (pending, identity) in pending.iter().zip(identities) {
        let AcceptedInputDisposition::PendingSteering { binding } =
            pending.lifecycle().disposition()
        else {
            return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
        };
        if pending.accepted_input() != identity.accepted_input
            || binding.source_turn() != source_turn
            || identity.turn == source_turn
            || !turns.insert(identity.turn)
        {
            return Err(ModelCallClosureError::PendingSteeringReclassificationMismatch);
        }
        let accepted_input = pending
            .lifecycle()
            .clone()
            .reclassify_as_turn_origin(
                identity.turn,
                SteeringReclassificationReason::NoSafePointBeforeTerminal,
            )
            .map_err(|_| ModelCallClosureError::PendingSteeringReclassificationMismatch)?;
        reclassified.push(ReclassifiedPendingSteeringTurn {
            session,
            source_turn,
            accepted_input,
            turn: identity.turn,
            order: AcceptedInputQueueOrder::ordinary(pending.acceptance_position()),
            binding: *binding,
            effective_configuration: effective_configuration.clone(),
        });
    }
    Ok(reclassified.into_boxed_slice())
}

#[allow(clippy::too_many_arguments)]
fn assemble_tool_round(
    scope: ModelCallTurnScope,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    response: ToolUsingAssistantResponse,
    identities: ToolRoundModelCallIdentities,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
) -> Result<ToolRoundModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if response.parts().len() != identities.response_parts.len() {
        return Err(ModelCallClosureError::ToolResponseIdentityMismatch);
    }
    let mut used_entries = frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    let mut used_requests = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. } => Some(*request),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut assistant_entries = Vec::with_capacity(response.parts().len());
    let mut requests = Vec::with_capacity(response.tool_count());
    let mut automatic_approvals = Vec::with_capacity(response.tool_count());
    let mut earliest_undecided = None;
    let mut tool_ordinal = 0usize;

    for (part, identity) in response.parts().iter().zip(identities.response_parts) {
        let entry = match (part, identity) {
            (AssistantResponsePart::Text(value), ToolResponsePartIdentity::Text { entry }) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantText {
                        producing_call: call.id(),
                        value: value.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ProviderCompaction(block),
                ToolResponsePartIdentity::ProviderCompaction { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call: call.id(),
                        block: block.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ToolCall(proposal),
                ToolResponsePartIdentity::ToolCall {
                    entry,
                    request,
                    approval,
                },
            ) => {
                if !used_entries.insert(entry) || !used_requests.insert(request) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                if !initial_tool_approval_matches_posture(dangerous_tool_auto_approval, approval) {
                    return Err(ModelCallClosureError::InitialToolApprovalMismatch);
                }
                let ordinal = ToolRequestOrdinal::try_from_usize(tool_ordinal)
                    .ok_or(ModelCallClosureError::ToolRequestOrdinalOverflow)?;
                tool_ordinal += 1;
                let request_record = ToolRequest::from_model_proposal(
                    request,
                    session,
                    turn,
                    call.id(),
                    ordinal,
                    proposal.clone(),
                    approval,
                );
                match approval.resolution(request) {
                    Some(resolution) => automatic_approvals.push(resolution),
                    None => {
                        earliest_undecided.get_or_insert(request);
                    }
                }
                requests.push(request_record);
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call: call.id(),
                        request,
                    },
                )
            }
            _ => return Err(ModelCallClosureError::ToolResponseIdentityMismatch),
        };
        assistant_entries.push(entry);
    }

    let next_phase = match (earliest_undecided, identities.continuation_attempt) {
        (Some(request), None) => ActiveTurnPhase::AwaitingApproval { request },
        (None, Some(continuation)) if continuation != attempt.id() => ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(continuation),
        },
        _ => return Err(ModelCallClosureError::ContinuationAttemptIdentityMismatch),
    };
    let source = ResolvedContextFrontierSnapshot::try_from_candidate(
        session,
        call.frontier().snapshot(),
        frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::reference)
            .collect(),
    )
    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    let yielded_snapshot = source
        .derive_appending_candidate(
            identities.yielded_frontier,
            assistant_entries
                .iter()
                .map(SemanticTranscriptEntry::reference)
                .collect(),
        )
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;

    Ok(ToolRoundModelCallTurn {
        session,
        turn,
        call,
        attempt,
        assistant_entries: assistant_entries.into_boxed_slice(),
        requests: requests.into_boxed_slice(),
        automatic_approvals: automatic_approvals.into_boxed_slice(),
        yielded_snapshot,
        next_phase,
    })
}

fn initial_tool_approval_matches_posture(
    posture: DangerousToolAutoApproval,
    approval: InitialToolApproval,
) -> bool {
    match (posture, approval) {
        (DangerousToolAutoApproval::ApproveAll, InitialToolApproval::Confirm)
        | (DangerousToolAutoApproval::Disabled, InitialToolApproval::SessionBlanket) => false,
        (
            DangerousToolAutoApproval::ApproveAll,
            InitialToolApproval::AlwaysConfirm
            | InitialToolApproval::SessionBlanket
            | InitialToolApproval::PolicyAuto
            | InitialToolApproval::Human
            | InitialToolApproval::Delegated
            | InitialToolApproval::RuntimeSafetyDeny
            | InitialToolApproval::UserOverride { .. },
        )
        | (
            DangerousToolAutoApproval::Disabled,
            InitialToolApproval::Confirm
            | InitialToolApproval::AlwaysConfirm
            | InitialToolApproval::PolicyAuto
            | InitialToolApproval::Human
            | InitialToolApproval::Delegated
            | InitialToolApproval::RuntimeSafetyDeny
            | InitialToolApproval::UserOverride { .. },
        ) => true,
    }
}

#[allow(clippy::too_many_arguments)]
fn assemble_stopped_tool_round(
    scope: ModelCallTurnScope,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    response: ToolUsingAssistantResponse,
    proof: AppliedInterruptProof,
    identities: StoppedToolRoundModelCallIdentities,
    dangerous_tool_auto_approval: DangerousToolAutoApproval,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<CancelledToolRoundModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if proof.predecessor() != turn || response.parts().len() != identities.response_parts.len() {
        return Err(ModelCallClosureError::ToolResponseIdentityMismatch);
    }
    let mut used_entries = frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    if !used_entries.insert(identities.cancellation_entry) {
        return Err(ModelCallClosureError::FrontierDerivationFailed);
    }
    let mut used_requests = frontier_entries
        .iter()
        .filter_map(|entry| match entry.payload() {
            SemanticTranscriptEntryPayload::AssistantToolUse { request, .. } => Some(*request),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut assistant_entries = Vec::with_capacity(response.parts().len());
    let mut requests = Vec::with_capacity(response.tool_count());
    let mut closed_result_entries = Vec::with_capacity(response.tool_count());
    let mut tool_ordinal = 0usize;

    for (part, identity) in response.parts().iter().zip(identities.response_parts) {
        let entry = match (part, identity) {
            (
                AssistantResponsePart::Text(value),
                StoppedToolResponsePartIdentity::Text { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantText {
                        producing_call: call.id(),
                        value: value.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ProviderCompaction(block),
                StoppedToolResponsePartIdentity::ProviderCompaction { entry },
            ) => {
                if !used_entries.insert(entry) {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call: call.id(),
                        block: block.clone(),
                    },
                )
            }
            (
                AssistantResponsePart::ToolCall(proposal),
                StoppedToolResponsePartIdentity::ToolCall {
                    entry,
                    request,
                    closed_result_entry,
                    approval,
                },
            ) => {
                if !used_entries.insert(entry)
                    || !used_entries.insert(closed_result_entry)
                    || !used_requests.insert(request)
                {
                    return Err(ModelCallClosureError::FrontierDerivationFailed);
                }
                if !initial_tool_approval_matches_posture(dangerous_tool_auto_approval, approval) {
                    return Err(ModelCallClosureError::InitialToolApprovalMismatch);
                }
                let ordinal = ToolRequestOrdinal::try_from_usize(tool_ordinal)
                    .ok_or(ModelCallClosureError::ToolRequestOrdinalOverflow)?;
                tool_ordinal += 1;
                requests.push(ToolRequest::from_model_proposal(
                    request,
                    session,
                    turn,
                    call.id(),
                    ordinal,
                    proposal.clone(),
                    approval,
                ));
                closed_result_entries.push(SemanticTranscriptEntry::from_validated_parts(
                    closed_result_entry,
                    session,
                    SemanticTranscriptEntryPayload::ToolClosed { request },
                ));
                SemanticTranscriptEntry::from_validated_parts(
                    entry,
                    session,
                    SemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call: call.id(),
                        request,
                    },
                )
            }
            _ => return Err(ModelCallClosureError::ToolResponseIdentityMismatch),
        };
        assistant_entries.push(entry);
    }
    let cancellation_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.cancellation_entry,
        session,
        SemanticTranscriptEntryPayload::TurnCancelled { turn },
    );
    let source = ResolvedContextFrontierSnapshot::try_from_candidate(
        session,
        call.frontier().snapshot(),
        frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::reference)
            .collect(),
    )
    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    let appended = assistant_entries
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .chain(
            closed_result_entries
                .iter()
                .map(SemanticTranscriptEntry::reference),
        )
        .chain([cancellation_entry.reference()])
        .collect();
    let terminal_snapshot = source
        .derive_appending_candidate(identities.terminal_frontier, appended)
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(CancelledToolRoundModelCallTurn {
        session,
        turn,
        call,
        attempt,
        disposition: TurnDisposition::Cancelled { cause: proof },
        assistant_entries: assistant_entries.into_boxed_slice(),
        requests: requests.into_boxed_slice(),
        closed_result_entries: closed_result_entries.into_boxed_slice(),
        cancellation_entry,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_recovery_wait(
    active_turn: ActivatedTurn,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    source_snapshot: ResolvedContextFrontierSnapshot,
    interrupt: AppliedInterruptCommandResult,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt: None,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    if interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ModelCall(call.id()))
        || call.turn() != active_turn.turn()
        || call.attempt() != attempt.id()
        || call.disposition() != ModelCallDisposition::Ambiguous
        || call.frontier() != source_snapshot.frontier()
        || source_snapshot.frontier().owning_session() != active_turn.session()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let terminal_snapshot = source_snapshot
        .derive_appending_candidate(identities.terminal_frontier, Vec::new())
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    if !matches!(
        attempt.end(),
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost,
        }
    ) {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    }
    let marker =
        ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), proof);
    Ok(ReconciliationRequiredModelCallTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        call,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_automatic_reconciliation(
    active_turn: ActivatedTurn,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    source_snapshot: ResolvedContextFrontierSnapshot,
    recovery_attempt: std::num::NonZeroU32,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredModelCallTurn, ModelCallClosureError> {
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    if ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ModelCall(call.id()))
        || call.turn() != active_turn.turn()
        || call.attempt() != attempt.id()
        || call.disposition() != ModelCallDisposition::Ambiguous
        || call.frontier() != source_snapshot.frontier()
        || source_snapshot.frontier().owning_session() != active_turn.session()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let terminal_snapshot = source_snapshot
        .derive_appending_candidate(identities.terminal_frontier, Vec::new())
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    if !matches!(
        attempt.end(),
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost,
        }
    ) {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    }
    let marker = match applied_interrupt {
        Some(proof) => {
            ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), *proof)
        }
        None => ReconciliationMarker::from_automatic_recovery(
            ambiguous_operations.clone(),
            recovery_attempt,
        ),
    };
    Ok(ReconciliationRequiredModelCallTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        call,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_runner_recovery_wait(
    active_turn: ActivatedTurn,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    source_snapshot: ResolvedContextFrontierSnapshot,
    result_projection: Option<PreparedToolResultProjection>,
    interrupt: AppliedInterruptCommandResult,
    identities: CancelledModelCallTurnIdentities,
) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    if !matches!(
        active_turn.phase(),
        ActiveTurnPhase::AwaitingRunnerRecovery {
            optional_tool_attempt: None,
            ..
        }
    ) || interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || starting_snapshot.frontier() != active_turn.start().frontier()
        || source_snapshot.frontier().owning_session() != active_turn.session()
        || !starting_snapshot.is_semantic_prefix_of(&source_snapshot)
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let tool_result_entries = match result_projection {
        Some(projection)
            if projection.turn() == active_turn.turn()
                && projection.source_frontier() == source_snapshot.frontier().snapshot()
                && projection.snapshot().frontier().owning_session() == active_turn.session()
                && source_snapshot.is_semantic_prefix_of(projection.snapshot()) =>
        {
            projection.into_parts().0
        }
        None => Vec::new().into_boxed_slice(),
        Some(_) => return Err(ModelCallClosureError::InterruptCorrelationMismatch),
    };
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let mut cancelled = close_cancelled_turn(
        ModelCallTurnScope {
            session: active_turn.session(),
            turn: active_turn.turn(),
        },
        None,
        None,
        CancellationFrontierSource::new(source_snapshot, &tool_result_entries),
        proof,
        identities,
        reclassified_pending_steering,
    )?;
    cancelled.tool_result_entries = tool_result_entries;
    Ok(cancelled)
}

pub(crate) fn apply_interrupt_to_runner_tool_recovery_wait(
    active_turn: ActivatedTurn,
    wait: AwaitingToolRecovery,
    tool_attempt: EndedToolAttempt,
    yielded_attempt: crate::TurnAttemptId,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRunnerRecovery {
        optional_tool_attempt: Some(interrupted_tool_attempt),
        ..
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let attempt = EndedTurnAttempt::reconstitute_yielded(yielded_attempt);
    if interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || *interrupted_tool_attempt != wait.attempt()
        || wait.session() != active_turn.session()
        || wait.turn() != active_turn.turn()
        || wait.producing_call() != result_projection.producing_call()
        || wait.issuing_attempt() != yielded_attempt
        || wait.attempt() != tool_attempt.attempt()
        || result_projection.turn() != active_turn.turn()
        || wait.yielded_frontier() != result_projection.source_frontier()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_attempt.request(),
                }
        })
        || tool_attempt.session() != active_turn.session()
        || tool_attempt.turn() != active_turn.turn()
        || tool_attempt.issuing_attempt() != yielded_attempt
        || tool_attempt.end() != &crate::ToolAttemptEnd::Ambiguous
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let ambiguous_operations =
        NonEmptyIssuedOperationRefs::try_from_operations([crate::IssuedOperationRef::ToolAttempt(
            wait.attempt(),
        )])
        .map_err(|_| ModelCallClosureError::InterruptCorrelationMismatch)?;
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, terminal_snapshot) = result_projection.into_parts();
    let marker = ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations, proof);
    Ok(ReconciliationRequiredToolTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        tool_attempt,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        tool_result_entries,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_retryable_runner_tool_recovery_wait(
    active_turn: ActivatedTurn,
    starting_snapshot: ResolvedContextFrontierSnapshot,
    batch: crate::ToolBatch,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: CancelledModelCallTurnIdentities,
) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRunnerRecovery {
        optional_tool_attempt: Some(interrupted_tool_attempt),
        ..
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let crate::ToolBatchPhase::Executing { turn_attempt } = batch.phase() else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let stopped_attempt =
        batch
            .requests()
            .iter()
            .find_map(|request| match batch.attempt(request.id()) {
                Some(crate::ReconstitutedToolAttempt::Ended(attempt))
                    if attempt.attempt() == *interrupted_tool_attempt =>
                {
                    Some(attempt)
                }
                Some(crate::ReconstitutedToolAttempt::Current(_))
                | Some(crate::ReconstitutedToolAttempt::Ended(_))
                | None => None,
            });
    let Some(stopped_attempt) = stopped_attempt else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    if batch.session() != active_turn.session()
        || batch.turn() != active_turn.turn()
        || stopped_attempt.session() != active_turn.session()
        || stopped_attempt.turn() != active_turn.turn()
        || stopped_attempt.issuing_attempt() != turn_attempt
        || !matches!(
            stopped_attempt.end(),
            crate::ToolAttemptEnd::KnownFailed { error }
                if error.kind() == crate::ToolExecutionErrorKind::CrashLost
                    && error.detail().is_none()
        )
        || interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || starting_snapshot.frontier() != active_turn.start().frontier()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let source_snapshot = batch.yielded_snapshot().clone();
    if !starting_snapshot.is_semantic_prefix_of(&source_snapshot)
        || result_projection.turn() != active_turn.turn()
        || result_projection.producing_call() != batch.producing_call()
        || result_projection.source_frontier() != source_snapshot.frontier().snapshot()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !source_snapshot.is_semantic_prefix_of(result_projection.snapshot())
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: *interrupted_tool_attempt,
                }
        })
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, _) = result_projection.into_parts();
    let mut cancelled = close_cancelled_turn(
        ModelCallTurnScope {
            session: active_turn.session(),
            turn: active_turn.turn(),
        },
        None,
        None,
        CancellationFrontierSource::new(source_snapshot, &tool_result_entries),
        proof,
        identities,
        reclassified_pending_steering,
    )?;
    cancelled.tool_result_entries = tool_result_entries;
    Ok(cancelled)
}

pub(crate) fn apply_interrupt_to_tool_recovery_wait(
    active_turn: ActivatedTurn,
    wait: AwaitingToolRecovery,
    tool_attempt: EndedToolAttempt,
    attempt: EndedTurnAttempt,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let attempt_end_matches = match attempt.end() {
        AttemptEnd::WithoutStop { disposition } => {
            applied_interrupt.is_none()
                && matches!(
                    disposition,
                    UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost
                )
        }
        AttemptEnd::AfterCancellation { cause, disposition } => {
            *cause == proof
                && applied_interrupt == &Some(proof)
                && matches!(
                    disposition,
                    CancellationStopDisposition::Ambiguous | CancellationStopDisposition::Lost
                )
        }
        AttemptEnd::AfterFatalMismatch { .. } => false,
    };
    if interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
        || ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ToolAttempt(wait.attempt()))
        || wait.session() != active_turn.session()
        || wait.turn() != active_turn.turn()
        || wait.producing_call() != result_projection.producing_call()
        || wait.issuing_attempt() != attempt.id()
        || wait.attempt() != tool_attempt.attempt()
        || result_projection.turn() != active_turn.turn()
        || wait.yielded_frontier() != result_projection.source_frontier()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_attempt.request(),
                }
        })
        || tool_attempt.session() != active_turn.session()
        || tool_attempt.turn() != active_turn.turn()
        || tool_attempt.issuing_attempt() != attempt.id()
        || tool_attempt.end() != &crate::ToolAttemptEnd::Ambiguous
        || !attempt_end_matches
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, terminal_snapshot) = result_projection.into_parts();
    let marker =
        ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), proof);
    Ok(ReconciliationRequiredToolTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        tool_attempt,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        tool_result_entries,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_automatic_tool_reconciliation(
    active_turn: ActivatedTurn,
    wait: AwaitingToolRecovery,
    tool_attempt: EndedToolAttempt,
    attempt: EndedTurnAttempt,
    result_projection: PreparedToolResultProjection,
    recovery_attempt: std::num::NonZeroU32,
    identities: AmbiguousModelCallTurnIdentities,
) -> Result<ReconciliationRequiredToolTurn, ModelCallClosureError> {
    let ActiveTurnPhase::AwaitingRecoveryDecision {
        ambiguous_operations,
        applied_interrupt,
    } = active_turn.phase()
    else {
        return Err(ModelCallClosureError::AttemptStateMismatch);
    };
    let attempt_end_matches = match attempt.end() {
        AttemptEnd::WithoutStop { disposition } => {
            applied_interrupt.is_none()
                && matches!(
                    disposition,
                    UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost
                )
        }
        AttemptEnd::AfterCancellation { cause, disposition } => {
            applied_interrupt == &Some(*cause)
                && matches!(
                    disposition,
                    CancellationStopDisposition::Ambiguous | CancellationStopDisposition::Lost
                )
        }
        AttemptEnd::AfterFatalMismatch { .. } => false,
    };
    if ambiguous_operations.operation_count() != 1
        || !ambiguous_operations.contains(crate::IssuedOperationRef::ToolAttempt(wait.attempt()))
        || wait.session() != active_turn.session()
        || wait.turn() != active_turn.turn()
        || wait.producing_call() != result_projection.producing_call()
        || wait.issuing_attempt() != attempt.id()
        || wait.attempt() != tool_attempt.attempt()
        || result_projection.turn() != active_turn.turn()
        || wait.yielded_frontier() != result_projection.source_frontier()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.snapshot().frontier().snapshot() != identities.terminal_frontier
        || !result_projection.entries().iter().any(|entry| {
            entry.payload()
                == &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_attempt.request(),
                }
        })
        || tool_attempt.session() != active_turn.session()
        || tool_attempt.turn() != active_turn.turn()
        || tool_attempt.issuing_attempt() != attempt.id()
        || tool_attempt.end() != &crate::ToolAttemptEnd::Ambiguous
        || !attempt_end_matches
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, terminal_snapshot) = result_projection.into_parts();
    let marker = match applied_interrupt {
        Some(proof) => {
            ReconciliationMarker::from_interrupt_ambiguity(ambiguous_operations.clone(), *proof)
        }
        None => ReconciliationMarker::from_automatic_recovery(
            ambiguous_operations.clone(),
            recovery_attempt,
        ),
    };
    Ok(ReconciliationRequiredToolTurn {
        session: active_turn.session(),
        turn: active_turn.turn(),
        tool_attempt,
        attempt,
        disposition: TurnDisposition::ReconciliationRequired { marker },
        tool_result_entries,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

pub(crate) fn apply_interrupt_to_executing_tool_batch(
    active_turn: ActivatedTurn,
    batch: crate::ToolBatch,
    result_projection: PreparedToolResultProjection,
    interrupt: AppliedInterruptCommandResult,
    identities: CancelledModelCallTurnIdentities,
) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
    let proof = interrupt.proof();
    let attempt = match (active_turn.phase(), batch.phase()) {
        (
            ActiveTurnPhase::Running { current_attempt },
            crate::ToolBatchPhase::Executing { turn_attempt },
        ) if turn_attempt == current_attempt.id() => Some(current_attempt.clone()),
        (
            ActiveTurnPhase::AwaitingChild { wait },
            crate::ToolBatchPhase::AwaitingChild {
                request,
                spawning_request,
                child,
            },
        ) if request == wait.awaiting_request()
            && spawning_request == wait.spawning_request()
            && child == wait.child() =>
        {
            None
        }
        (ActiveTurnPhase::Running { .. }, crate::ToolBatchPhase::Executing { .. }) => {
            return Err(ModelCallClosureError::InterruptCorrelationMismatch);
        }
        _ => return Err(ModelCallClosureError::AttemptStateMismatch),
    };
    if batch.session() != active_turn.session()
        || batch.turn() != active_turn.turn()
        || interrupt.session() != active_turn.session()
        || proof.predecessor() != active_turn.turn()
        || interrupt.successor() == active_turn.turn()
        || interrupt.successor_order().priority()
            != (crate::AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn.turn(),
            })
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let source_snapshot = batch.yielded_snapshot().clone();
    if result_projection.turn() != active_turn.turn()
        || result_projection.snapshot().frontier().owning_session() != active_turn.session()
        || result_projection.source_frontier() != source_snapshot.frontier().snapshot()
        || !source_snapshot.is_semantic_prefix_of(result_projection.snapshot())
        || result_projection.snapshot().entry_count()
            != source_snapshot.entry_count() + result_projection.entries().len()
    {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let reclassified_pending_steering =
        reclassify_pending_steering(&active_turn, &identities.pending_steering_reclassifications)?;
    let (tool_result_entries, _result_snapshot) = result_projection.into_parts();
    let mut cancelled = close_cancelled_turn(
        ModelCallTurnScope {
            session: active_turn.session(),
            turn: active_turn.turn(),
        },
        attempt,
        None,
        CancellationFrontierSource::new(source_snapshot, &tool_result_entries),
        proof,
        identities,
        reclassified_pending_steering,
    )?;
    cancelled.tool_result_entries = tool_result_entries;
    Ok(cancelled)
}

fn complete_turn(
    scope: ModelCallTurnScope,
    call: EndedModelCall,
    attempt: EndedTurnAttempt,
    frontier_entries: Vec<SemanticTranscriptEntry>,
    response: Vec<AssistantResponsePart>,
    identities: CompletedModelCallIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<CompletedModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if response.len() != identities.assistant_entries.len()
        || response
            .iter()
            .any(|part| matches!(part, AssistantResponsePart::ToolCall(_)))
    {
        return Err(ModelCallClosureError::AssistantIdentityCountMismatch);
    }
    let mut used = frontier_entries
        .iter()
        .map(SemanticTranscriptEntry::identity)
        .collect::<BTreeSet<_>>();
    if identities
        .assistant_entries
        .iter()
        .chain([&identities.completion_entry])
        .any(|identity| !used.insert(*identity))
    {
        return Err(ModelCallClosureError::FrontierDerivationFailed);
    }
    let assistant_entries = identities
        .assistant_entries
        .into_iter()
        .zip(response)
        .map(|(identity, part)| {
            let payload = match part {
                AssistantResponsePart::Text(value) => {
                    SemanticTranscriptEntryPayload::AssistantText {
                        producing_call: call.id(),
                        value,
                    }
                }
                AssistantResponsePart::ProviderCompaction(block) => {
                    SemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call: call.id(),
                        block,
                    }
                }
                AssistantResponsePart::ToolCall(_) => {
                    return Err(ModelCallClosureError::AssistantIdentityCountMismatch);
                }
            };
            Ok(SemanticTranscriptEntry::from_validated_parts(
                identity, session, payload,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let completion_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.completion_entry,
        session,
        SemanticTranscriptEntryPayload::TurnCompleted { turn },
    );
    let source = ResolvedContextFrontierSnapshot::try_from_candidate(
        session,
        call.frontier().snapshot(),
        frontier_entries
            .iter()
            .map(SemanticTranscriptEntry::reference)
            .collect(),
    )
    .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    let appended = assistant_entries
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .chain([completion_entry.reference()])
        .collect();
    let terminal_snapshot = source
        .derive_appending_candidate(identities.terminal_frontier, appended)
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(CompletedModelCallTurn {
        session,
        turn,
        call,
        attempt,
        disposition: TurnDisposition::Completed,
        assistant_entries: assistant_entries.into_boxed_slice(),
        completion_entry,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

fn close_failed_turn(
    scope: ModelCallTurnScope,
    attempt: CurrentTurnAttempt,
    call: Option<EndedModelCall>,
    source: ResolvedContextFrontierSnapshot,
    identities: FailedModelCallTurnIdentities,
    attempt_disposition: UnstoppedAttemptDisposition,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<FailedModelCallTurn, ModelCallClosureError> {
    let ended_attempt = attempt
        .end_without_stop(attempt_disposition)
        .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
    assemble_failed_turn(
        scope,
        ended_attempt,
        call,
        source,
        identities,
        reclassified_pending_steering,
    )
}

fn close_failed_turn_after_cancellation(
    scope: ModelCallTurnScope,
    attempt: CurrentTurnAttempt,
    call: EndedModelCall,
    source: ResolvedContextFrontierSnapshot,
    proof: AppliedInterruptProof,
    identities: FailedModelCallTurnIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<FailedModelCallTurn, ModelCallClosureError> {
    let ended_attempt = attempt
        .end_after_cancellation(proof, CancellationStopDisposition::KnownFailure)
        .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
    assemble_failed_turn(
        scope,
        ended_attempt,
        Some(call),
        source,
        identities,
        reclassified_pending_steering,
    )
}

fn assemble_failed_turn(
    scope: ModelCallTurnScope,
    ended_attempt: EndedTurnAttempt,
    call: Option<EndedModelCall>,
    source: ResolvedContextFrontierSnapshot,
    identities: FailedModelCallTurnIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<FailedModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    let failure_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.failure_entry,
        session,
        SemanticTranscriptEntryPayload::TurnFailed { turn },
    );
    let terminal_snapshot = source
        .derive_appending_candidate(
            identities.terminal_frontier,
            vec![failure_entry.reference()],
        )
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(FailedModelCallTurn {
        session,
        turn,
        call,
        attempt: ended_attempt,
        disposition: TurnDisposition::Failed,
        failure_entry,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

struct CancellationFrontierSource<'a> {
    snapshot: ResolvedContextFrontierSnapshot,
    entries_before_cancellation: &'a [SemanticTranscriptEntry],
}

impl<'a> CancellationFrontierSource<'a> {
    fn new(
        snapshot: ResolvedContextFrontierSnapshot,
        entries_before_cancellation: &'a [SemanticTranscriptEntry],
    ) -> Self {
        Self {
            snapshot,
            entries_before_cancellation,
        }
    }
}

fn close_cancelled_turn(
    scope: ModelCallTurnScope,
    attempt: Option<CurrentTurnAttempt>,
    call: Option<EndedModelCall>,
    source: CancellationFrontierSource<'_>,
    proof: AppliedInterruptProof,
    identities: CancelledModelCallTurnIdentities,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
) -> Result<CancelledModelCallTurn, ModelCallClosureError> {
    let ModelCallTurnScope { session, turn } = scope;
    if proof.predecessor() != turn {
        return Err(ModelCallClosureError::InterruptCorrelationMismatch);
    }
    let ended_attempt = attempt
        .map(|attempt| {
            attempt.end_after_cancellation(proof, CancellationStopDisposition::Cancelled)
        })
        .transpose()
        .map_err(|_| ModelCallClosureError::AttemptStateMismatch)?;
    let cancellation_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.cancellation_entry,
        session,
        SemanticTranscriptEntryPayload::TurnCancelled { turn },
    );
    let appended_entries = source
        .entries_before_cancellation
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .chain(std::iter::once(cancellation_entry.reference()))
        .collect();
    let terminal_snapshot = source
        .snapshot
        .derive_appending_candidate(identities.terminal_frontier, appended_entries)
        .map_err(|_| ModelCallClosureError::FrontierDerivationFailed)?;
    Ok(CancelledModelCallTurn {
        session,
        turn,
        call,
        attempt: ended_attempt,
        disposition: TurnDisposition::Cancelled { cause: proof },
        tool_result_entries: Box::new([]),
        cancellation_entry,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

#[cfg(test)]
pub(crate) use tests::{
    cancelled_turn_fixture, completed_turn_fixture,
    completed_turn_with_provider_compaction_fixture, failed_turn_fixture,
};

#[cfg(test)]
mod tests;

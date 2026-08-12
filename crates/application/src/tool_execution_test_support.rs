//! Shared construction for driving a provider executor through the real
//! `ToolExecutionService`.
//!
//! `ToolExecutionInvocation` has no public constructor: this crate mints one
//! only while running a batch, so a test in a provider-adapter crate cannot
//! call an executor's `execute` directly. Reaching the evidence-producing path
//! therefore means driving the real service over a reconstituted batch, and
//! every provider crate that wants to test its own evidence needs the same
//! three pieces: a prepared single-attempt batch, a transaction that serves it,
//! and a recorder that captures what the executor returned.
//!
//! Those pieces differ between providers only in tool names, fixture values,
//! and the adapter's own error type, which is why `docs/style.md` places them
//! here — in the crate every provider adapter already depends on — rather than
//! in each provider crate. Provider crates keep their catalog, transport, and
//! fixture values and build the rest from this module.
//!
//! Gated behind the `test-support` feature so it never compiles into a
//! production build of this crate.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this module is fixture construction compiled only under the `test-support` feature, where a malformed fixture or an unreachable orchestration path is a defect to report loudly rather than a condition to handle; the workspace gate remains active for every production target"
)]

use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, CorrelatedToolAttemptObservation, CurrentToolAttempt,
    DecideToolRequest, DurableCommandId, EndedToolAttempt, ModelCallId, NormalizedToolArguments,
    ResolvedContextFrontierReconstitutionInput, SemanticTranscriptEntryId, SessionId,
    ToolApprovalDecision, ToolApprovalPosture, ToolApprovalResolutionReconstitutionInput,
    ToolAttemptCrashOutcome, ToolAttemptDispatchCorrelation, ToolAttemptId,
    ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolBatch,
    ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolDispatchGeneration,
    ToolEffectClass, ToolExecutionError, ToolName, ToolRequestId, ToolRequestOrdinal,
    ToolRequestReconstitutionInput, TurnAttemptId, TurnId,
};

use crate::{
    ClassifyOperatorFailure, CorrelatedDurableChildWait, CorrelatedToolExecutorEvidence,
    PrepareToolContinuationOutcome, RetainedToolAttemptObservationStatus,
    ToolAttemptAuthorizationOutcome, ToolAttemptAuthorizationStatus, ToolContinuationIdentities,
    ToolCrashClosureIdentities, ToolExecutionInvocation, ToolExecutionTransaction, ToolExecutor,
    ToolExecutorDisposition, ToolExecutorEvidence, ToolPreauthorization,
};

use std::sync::{Arc, Mutex};

/// The identities one prepared single-attempt batch is reconstituted with.
///
/// Grouped rather than passed as a run of same-typed parameters: every member
/// is an opaque UUID newtype, so a transposed pair would reconstitute a
/// coherent batch describing the wrong thing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreparedAttemptIdentities {
    /// Session owning the batch.
    pub session: SessionId,
    /// Turn owning the batch.
    pub turn: TurnId,
    /// Model call that proposed the batch.
    pub producing_call: ModelCallId,
    /// The single proposed tool request.
    pub request: ToolRequestId,
    /// The single prepared physical attempt.
    pub attempt: ToolAttemptId,
    /// Turn attempt that issued the batch and is executing it.
    pub issuing_turn_attempt: TurnAttemptId,
    /// The batch's resolved context frontier.
    pub frontier: ContextFrontierId,
}

/// What the single request in a prepared batch proposes.
#[derive(Clone, Debug)]
pub struct PreparedAttemptProposal {
    /// Checked model-facing tool name, as the catalog declares it.
    pub name: ToolName,
    /// Exact normalized arguments the model proposed.
    pub arguments: NormalizedToolArguments,
    /// Effect class the catalog declared for `name`.
    pub effect_class: ToolEffectClass,
    /// How this attempt became admissible to dispatch.
    pub approval: PreparedAttemptApproval,
}

/// The source that admitted a prepared attempt to dispatch.
///
/// Stated by the caller rather than assumed, because it has to agree with the
/// permission default its catalog declares: a tool declared
/// `ToolPermissionDefault::Confirm` never reaches dispatch on policy alone, so
/// a fixture pairing it with a policy approval drives a batch the application
/// cannot produce.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedAttemptApproval {
    /// The catalog declares the tool auto-approved and policy admitted it.
    PolicyAuto,
    /// A user decision admitted it, as a confirm-by-default tool requires.
    UserConfirmation {
        /// Exact durable command carrying the decision's provenance.
        command: DurableCommandId,
    },
}

/// Reconstitutes one approved, prepared, executing single-attempt batch.
///
/// This is the state a provider executor is actually invoked from: one
/// proposal, the approval that admitted it, and one prepared attempt awaiting
/// dispatch.
///
/// # Panics
///
/// Panics when the supplied identities and proposal do not reconstitute a
/// coherent batch, which is a defect in the calling fixture rather than a
/// condition under test.
#[must_use]
pub fn prepared_single_attempt_batch(
    identities: PreparedAttemptIdentities,
    proposal: PreparedAttemptProposal,
) -> ToolBatch {
    let request = ToolRequestReconstitutionInput::new(
        identities.request,
        identities.session,
        identities.turn,
        identities.producing_call,
        ToolRequestOrdinal::from_u32(0),
        proposal.name,
        proposal.arguments,
    )
    // The stored posture is part of what made the attempt admissible, so it
    // follows the approval rather than the reconstitution default: a
    // policy-auto approval on a `Human`-posture request is a pairing the
    // application never records, and reconstitution admitting it would let an
    // auto-approved adapter test pass against speculative authority.
    .with_approval_posture(match proposal.approval {
        PreparedAttemptApproval::PolicyAuto => ToolApprovalPosture::Auto,
        PreparedAttemptApproval::UserConfirmation { .. } => ToolApprovalPosture::Human,
    })
    .into_request();
    let approval = match proposal.approval {
        PreparedAttemptApproval::PolicyAuto => {
            ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
        }
        PreparedAttemptApproval::UserConfirmation { command } => {
            ToolApprovalResolutionReconstitutionInput::user_command(
                DecideToolRequest::try_new(command, request.id(), ToolApprovalDecision::Approve)
                    .expect("fixture command identity is admitted")
                    .prepare_applied(&request)
                    .expect("the command names the exact request"),
            )
        }
    }
    .reconstitute()
    .expect("approval fixture is valid");
    let attempt = ToolAttemptReconstitutionInput::new(
        identities.attempt,
        request.id(),
        identities.session,
        identities.turn,
        identities.issuing_turn_attempt,
        proposal.effect_class,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Prepared,
    )
    .reconstitute()
    .expect("prepared attempt fixture is valid");
    let frontier = ResolvedContextFrontierReconstitutionInput::new(
        identities.session,
        identities.frontier,
        Vec::new(),
    )
    .reconstitute()
    .expect("empty frontier fixture is valid");

    ToolBatchReconstitutionInput::new(
        identities.session,
        identities.turn,
        identities.producing_call,
        frontier,
        vec![request],
        vec![approval],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::Executing {
            turn_attempt: identities.issuing_turn_attempt,
        },
    )
    .reconstitute()
    .expect("single-attempt batch fixture is valid")
}

/// The two failures a fixture transaction returns, in the caller's error type.
///
/// Grouped rather than passed as two same-typed parameters: they mean opposite
/// things, and a transposed pair would leave a fixture asserting on the wrong
/// classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureTransactionFailures<Error> {
    /// Returned when the domain refuses a transition the fixture drove, which
    /// is a defect in the fixture rather than a condition under test.
    pub domain_rejection: Error,
    /// Returned instead of durably classifying an in-flight attempt after the
    /// executor failed. Declining is the point: it surfaces the executor's own
    /// failure to the caller rather than resolving it.
    pub declined_crash_classification: Error,
}

/// Serves one prepared batch and applies whatever the executor returns.
///
/// Every method a single-attempt fixture cannot reach panics rather than
/// returning a plausible value, so a service change that starts routing
/// through one of them is reported instead of silently exercised.
#[derive(Clone, Debug)]
pub struct FixtureToolExecutionTransaction<Error> {
    batch: ToolBatch,
    failures: FixtureTransactionFailures<Error>,
}

impl<Error> FixtureToolExecutionTransaction<Error> {
    /// Binds one prepared batch to the failures this fixture may return.
    #[must_use]
    pub const fn new(batch: ToolBatch, failures: FixtureTransactionFailures<Error>) -> Self {
        Self { batch, failures }
    }

    /// Borrows the batch this transaction serves.
    #[must_use]
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }
}

impl<Error> ToolExecutionTransaction for FixtureToolExecutionTransaction<Error>
where
    Error: ClassifyOperatorFailure + Clone + Send,
{
    type Error = Error;

    async fn resume_child_wait(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: TurnAttemptId,
    ) -> Result<bool, Self::Error> {
        panic!("a prepared single-attempt fixture never contains a delegated child wait")
    }

    async fn reread_durable_child_wait(
        &mut self,
        _wait: CorrelatedDurableChildWait,
    ) -> Result<bool, Self::Error> {
        panic!("a prepared single-attempt fixture never parks on a delegated child wait")
    }

    async fn reread_durable_completion(
        &mut self,
        _correlation: ToolAttemptDispatchCorrelation,
    ) -> Result<bool, Self::Error> {
        panic!("a prepared single-attempt fixture never reports a durable completion")
    }

    async fn load_active_batch(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
    ) -> Result<Option<ToolBatch>, Self::Error> {
        Ok(Some(self.batch.clone()))
    }

    async fn prepare_next_attempt(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: ToolAttemptId,
        _effect_class: ToolEffectClass,
    ) -> Result<Option<CurrentToolAttempt>, Self::Error> {
        panic!("a prepared single-attempt fixture begins with its attempt already prepared")
    }

    async fn authorize_attempt(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        attempt: ToolAttemptId,
        _preauthorization: ToolPreauthorization,
    ) -> Result<ToolAttemptAuthorizationOutcome, Self::Error> {
        self.batch
            .authorize_dispatch(attempt)
            .map(Box::new)
            .map(ToolAttemptAuthorizationOutcome::Authorized)
            .map_err(|_| self.failures.domain_rejection.clone())
    }

    async fn reread_ambiguous_authorization(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: ToolAttemptId,
    ) -> Result<ToolAttemptAuthorizationStatus, Self::Error> {
        panic!("a prepared single-attempt fixture authorizes unambiguously")
    }

    async fn commit_preflight_error(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: ToolAttemptId,
        _error: ToolExecutionError,
    ) -> Result<EndedToolAttempt, Self::Error> {
        panic!("a prepared single-attempt fixture supplies arguments that pass preflight")
    }

    async fn commit_observation(
        &mut self,
        observation: CorrelatedToolAttemptObservation,
    ) -> Result<EndedToolAttempt, Self::Error> {
        self.batch
            .authorize_attempt(observation.correlation().attempt())
            .map_err(|_| self.failures.domain_rejection.clone())?
            .into_parts()
            .0
            .apply_terminal_observation(observation)
            .map_err(|_| self.failures.domain_rejection.clone())
    }

    async fn reread_observation(
        &mut self,
        _observation: &CorrelatedToolAttemptObservation,
    ) -> Result<RetainedToolAttemptObservationStatus, Self::Error> {
        Ok(RetainedToolAttemptObservationStatus::Pending)
    }

    async fn classify_crash_loss<NextTurn>(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _attempt: ToolAttemptId,
        _identities: ToolCrashClosureIdentities,
        _next_turn: NextTurn,
    ) -> Result<ToolAttemptCrashOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        // Reached, not dead: an executor error the service cannot resolve into
        // evidence routes the attempt here, because an external effect that may
        // already have happened must be classified rather than assumed. The
        // fixture declines, which surfaces the executor's own error to the
        // caller as `ExecutorCrashClassification`.
        Err(self.failures.declined_crash_classification.clone())
    }

    async fn prepare_continuation<NextSteering>(
        &mut self,
        _session: SessionId,
        _turn: TurnId,
        _producing_call: ModelCallId,
        _identities: ToolContinuationIdentities,
        _next_steering: NextSteering,
    ) -> Result<PrepareToolContinuationOutcome, Self::Error>
    where
        NextSteering: FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send,
    {
        panic!("a prepared single-attempt fixture has no second round to continue into")
    }
}

/// Captures an executor's evidence on its way back to the service.
///
/// The evidence is the executor's own return value, captured before the
/// service converts it into a durable observation, so a test can assert the
/// exact `ToolExecutorEvidence` variant and detail rather than whatever the
/// commit path happened to persist.
#[derive(Debug)]
pub struct RecordingToolExecutor<Executor> {
    inner: Executor,
    recorded: Arc<Mutex<Option<ToolExecutorEvidence>>>,
}

impl<Executor> RecordingToolExecutor<Executor> {
    /// Wraps `inner`, recording into a handle the caller retains.
    #[must_use]
    pub fn new(inner: Executor) -> (Self, RecordedEvidence) {
        let recorded = Arc::new(Mutex::new(None));
        let handle = RecordedEvidence {
            recorded: Arc::clone(&recorded),
        };
        (Self { inner, recorded }, handle)
    }

    /// Replaces whatever the previous invocation recorded.
    fn store(&self, evidence: Option<ToolExecutorEvidence>) {
        *self
            .recorded
            .lock()
            .expect("recorded evidence lock is available") = evidence;
    }
}

impl<Executor> ToolExecutor for RecordingToolExecutor<Executor>
where
    Executor: ToolExecutor + Send,
{
    type Error = Executor::Error;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let result = self.inner.execute(invocation).await;
        // Replaced on *every* invocation, including errors. Leaving the slot
        // untouched on failure would let a reused recorder report the previous
        // invocation's evidence as belonging to the one that bound none.
        self.store(
            result
                .as_ref()
                .ok()
                .map(|correlated| correlated.evidence().clone()),
        );
        result
    }

    async fn execute_with_scheduling(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<ToolExecutorDisposition, Self::Error>
    where
        Self: Send,
    {
        // Forwarded rather than left to the trait default, which would call
        // `inner.execute` and silently discard the wrapped executor's own
        // scheduling-aware override — changing the behaviour under test, and
        // panicking outright for an executor whose plain `execute` is
        // unreachable by construction.
        let result = self.inner.execute_with_scheduling(invocation).await;
        // Only `Completed` carries executor evidence; the durable dispositions
        // committed their own effect and have none to record.
        self.store(match &result {
            Ok(ToolExecutorDisposition::Completed(correlated)) => {
                Some(correlated.evidence().clone())
            }
            Ok(
                ToolExecutorDisposition::DurableCompletion(_)
                | ToolExecutorDisposition::DurableChildWait(_),
            )
            | Err(_) => None,
        });
        result
    }
}

/// A handle to whatever evidence the recorded executor last returned.
#[derive(Clone, Debug)]
pub struct RecordedEvidence {
    recorded: Arc<Mutex<Option<ToolExecutorEvidence>>>,
}

impl RecordedEvidence {
    /// Removes and returns the recorded evidence, leaving the slot empty.
    ///
    /// Consuming rather than cloning: a caller that reads twice is asking
    /// about two different invocations, and returning the same value again
    /// would attribute the first one's evidence to the second.
    ///
    /// # Panics
    ///
    /// Panics when the recording lock was poisoned by an earlier panic in the
    /// driven executor.
    #[must_use]
    pub fn take(&self) -> Option<ToolExecutorEvidence> {
        self.recorded
            .lock()
            .expect("recorded evidence lock is available")
            .take()
    }
}

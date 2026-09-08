use super::{
    AvailabilitySuccessorOutcome, ClassifyOperatorFailure, CorrelatedModelCallTerminalObservation,
    CredentialPoolExhaustedOutcome, Duration, FailedModelCallTurn, ModelCallId,
    ModelCallTerminalOutcome, ModelFrontierRenderingError, OperatorFailureClass,
};

/// Completed stage of one service invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCallExecutionOutcome {
    /// The scheduling hint no longer identifies runnable work.
    NoWork,
    /// Durable retry backoff remains before the successor may be prepared.
    RetryBackoff(Duration),
    /// The pool admitted no member; this is not a member provider failure.
    PoolExhausted(Box<CredentialPoolExhaustedOutcome>),
    /// A new prepared checkpoint committed and requires a later invocation.
    Checkpointed(ModelCallId),
    /// Target resolution failed before call creation.
    TargetUnavailable(Box<FailedModelCallTurn>),
    /// A trustworthy local capability failure closed the prepared call.
    CapabilityKnownFailure(Box<FailedModelCallTurn>),
    /// Attachment verification was unavailable; the call remains `Prepared`.
    AttachmentUnavailable,
    /// A retained prepared failure's earlier commit was proven to have landed.
    CapabilityFailureAlreadyCommitted(ModelCallId),
    /// The automatic tool-round limit closed the prepared call and turn.
    ToolRoundLimitReached(Box<FailedModelCallTurn>),
    /// A retained tool-round-limit closure was proven to have landed.
    ToolRoundLimitAlreadyCommitted(ModelCallId),
    /// The provider observation committed its authoritative result.
    ObservationCommitted(Box<ModelCallTerminalOutcome>),
    /// An availability failure committed and left the turn on a fresh attempt.
    AvailabilitySuccessor(Box<AvailabilitySuccessorOutcome>),
    /// A retained observation's earlier commit was proven to have landed.
    ObservationAlreadyCommitted(ModelCallId),
}

#[derive(signalbox_derive::OperatorError)]
/// Failure annotated with the exact orchestration stage that failed.
#[derive(Debug)]
pub enum ModelCallExecutionError<
    PrepareError,
    FailureError,
    AuthorizationError,
    ProviderError,
    ObservationError,
> {
    #[error("model-call prepare stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_prepare")]
    /// The prepare-call transaction failed.
    Prepare(PrepareError),
    #[error("model-call render stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_render")]
    /// Provider-neutral request rendering failed closed.
    Render(ModelFrontierRenderingError),
    #[error("model-call capability stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_capability_preparation")]
    /// Credential lookup or capability preparation failed as an operator error.
    CapabilityPreparation(ProviderError),
    #[error("model-call prepared-failure commit failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_prepared_failure_commit")]
    /// The guarded prepared-call failure transaction failed.
    PreparedFailureCommit(FailureError),
    #[error("model-call prepared-failure reread failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_prepared_failure_reread")]
    /// Authoritative reread of a retained prepared-call failure failed.
    PreparedFailureReread(FailureError),
    #[error("model-call authorization stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_authorization")]
    /// Durable send authorization failed.
    Authorization(AuthorizationError),
    #[error("model-call authorization reread failed: {reread_error}")]
    #[operator(delegate = reread_error, code = "model_call_authorization_reread")]
    /// Authoritative reread after an ambiguous authorization also failed.
    AuthorizationReread {
        /// The original commit-ambiguous authorization failure.
        authorization_error: AuthorizationError,
        /// The failure to establish whether authorization committed.
        reread_error: AuthorizationError,
    },
    #[error("model-call authorization reconciliation failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_authorization_reconciliation")]
    /// A later pass still could not reconcile retained non-consumption proof.
    AuthorizationReconciliation(AuthorizationError),
    #[error("model-call provider stage failed: {field_0}")]
    #[operator(delegate = 0, code = "model_call_provider")]
    /// Provider work produced no trustworthy observation.
    Provider(ProviderError),
    #[error("model-call observation commit failed: {error}")]
    #[operator(delegate = error, code = "model_call_observation_commit")]
    /// The terminal-observation transaction failed.
    ObservationCommit {
        /// The failed observation transaction or authoritative reread.
        error: ObservationError,
        /// The unchanged provider observation retained for a later pass.
        retained_observation: CorrelatedModelCallTerminalObservation,
    },
}

//! Physical tool-attempt authorization, fencing, and terminal evidence.
//!
//! `docs/spec/tool-loop.md` is normative. The hub prepares a durable attempt,
//! authorizes it only after reloading current aggregate state, then applies
//! evidence through the exact dispatch correlation. The executor never owns a
//! durable transition.

use crate::{
    SessionId, ToolApprovalDecision, ToolApprovalResolution, ToolAttemptId, ToolEffectClass,
    ToolRequest, ToolRequestId, ToolResultContent, TurnAttemptId, TurnId,
};

const MAX_TOOL_ERROR_DETAIL_BYTES: usize = 4096;

/// A positive dispatch generation in one physical attempt's fence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolDispatchGeneration(u64);

impl ToolDispatchGeneration {
    /// Reconstitutes a positive generation.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the initial dispatch generation.
    pub const fn first() -> Self {
        Self(1)
    }

    /// Returns the next generation when representable.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the positive durable ordinal.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// One approved request proven by exact request/decision correlation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovedToolRequest {
    request: ToolRequest,
    approval: ToolApprovalResolution,
}

impl ApprovedToolRequest {
    /// Correlates one approval with its exact immutable request.
    pub fn try_from_resolution(
        request: ToolRequest,
        approval: ToolApprovalResolution,
    ) -> Result<Self, ApprovedToolRequestError> {
        if request.id() != approval.request()
            || !matches!(approval.decision(), ToolApprovalDecision::Approve)
        {
            return Err(ApprovedToolRequestError {
                request: Box::new(request),
                approval,
            });
        }
        Ok(Self { request, approval })
    }

    /// Borrows the immutable request content authority.
    pub const fn request(&self) -> &ToolRequest {
        &self.request
    }

    /// Borrows the exact approval and provenance.
    pub const fn approval(&self) -> &ToolApprovalResolution {
        &self.approval
    }

    /// Prepares one first-generation physical attempt.
    pub fn prepare_attempt(
        &self,
        attempt: ToolAttemptId,
        issuing_attempt: TurnAttemptId,
        effect_class: ToolEffectClass,
    ) -> CurrentToolAttempt {
        CurrentToolAttempt {
            attempt,
            request: self.request.id(),
            session: self.request.session(),
            turn: self.request.turn(),
            issuing_attempt,
            effect_class,
            generation: ToolDispatchGeneration::first(),
            state: CurrentToolAttemptState::Prepared,
        }
    }
}

/// Rejected request/approval correlation retaining both inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovedToolRequestError {
    request: Box<ToolRequest>,
    approval: ToolApprovalResolution,
}

impl ApprovedToolRequestError {
    /// Borrows the unchanged request.
    pub fn request(&self) -> &ToolRequest {
        self.request.as_ref()
    }

    /// Borrows the unchanged resolution.
    pub const fn approval(&self) -> &ToolApprovalResolution {
        &self.approval
    }

    /// Returns both unchanged values.
    pub fn into_parts(self) -> (ToolRequest, ToolApprovalResolution) {
        (*self.request, self.approval)
    }
}

/// Closed typed tool-execution error kinds.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolExecutionErrorKind {
    /// No current catalog declaration matched the request name.
    UnknownTool,
    /// Arguments were undecodable or outside the selected tool's schema.
    InvalidArguments,
    /// The executor reported a definitive failure.
    ExecutionFailed,
    /// Successful content exceeded its admission bound.
    ResultTooLarge,
    /// Restart lost a prepared or effect-free attempt.
    CrashLost,
}

/// One optional bounded sanitized tool-error detail.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolExecutionErrorDetail(String);

impl ToolExecutionErrorDetail {
    /// Checks a nonempty, trimmed, control-free detail.
    pub fn try_new(value: String) -> Result<Self, ToolExecutionErrorDetailError> {
        let failure = if value.is_empty() {
            Some(ToolExecutionErrorDetailFailure::Empty)
        } else if value.len() > MAX_TOOL_ERROR_DETAIL_BYTES {
            Some(ToolExecutionErrorDetailFailure::TooLong { bytes: value.len() })
        } else if value.trim() != value {
            Some(ToolExecutionErrorDetailFailure::SurroundingWhitespace)
        } else {
            value
                .chars()
                .any(char::is_control)
                .then_some(ToolExecutionErrorDetailFailure::ContainsControl)
        };
        match failure {
            Some(failure) => Err(ToolExecutionErrorDetailError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact checked detail.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked detail.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why an execution-error detail was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolExecutionErrorDetailFailure {
    /// A present detail cannot be empty.
    Empty,
    /// The detail exceeded its bound.
    TooLong {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// Leading or trailing Unicode whitespace was present.
    SurroundingWhitespace,
    /// At least one Unicode control scalar was present.
    ContainsControl,
}

/// Failed execution-error-detail construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolExecutionErrorDetailError {
    value: String,
    failure: ToolExecutionErrorDetailFailure,
}

impl ToolExecutionErrorDetailError {
    /// Borrows the rejected detail.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the validation failure.
    pub const fn failure(&self) -> ToolExecutionErrorDetailFailure {
        self.failure
    }

    /// Returns the rejected detail and failure.
    pub fn into_parts(self) -> (String, ToolExecutionErrorDetailFailure) {
        (self.value, self.failure)
    }
}

/// One durable typed execution error.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolExecutionError {
    kind: ToolExecutionErrorKind,
    detail: Option<ToolExecutionErrorDetail>,
}

impl ToolExecutionError {
    /// Constructs one typed error with optional bounded detail.
    pub const fn new(
        kind: ToolExecutionErrorKind,
        detail: Option<ToolExecutionErrorDetail>,
    ) -> Self {
        Self { kind, detail }
    }

    /// Returns the closed error kind.
    pub const fn kind(&self) -> ToolExecutionErrorKind {
        self.kind
    }

    /// Borrows optional sanitized detail.
    pub const fn detail(&self) -> Option<&ToolExecutionErrorDetail> {
        self.detail.as_ref()
    }
}

/// The nonterminal local state of one physical attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CurrentToolAttemptState {
    /// Durable authorization facts exist but no executor effect is authorized.
    Prepared,
    /// The exact dispatch fence has been authorized.
    InFlight,
}

/// Honest terminal evidence for one physical attempt.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolAttemptEnd {
    /// The executor returned admitted result content.
    Completed {
        /// The sole durable content authority.
        result: ToolResultContent,
    },
    /// Definitive typed error evidence exists.
    KnownFailed {
        /// The sole durable error authority.
        error: ToolExecutionError,
    },
    /// An external effect may have occurred without definitive evidence.
    Ambiguous,
}

impl ToolAttemptEnd {
    /// Returns the storage-facing terminal classification.
    pub const fn disposition(&self) -> ToolAttemptDisposition {
        match self {
            Self::Completed { .. } => ToolAttemptDisposition::Completed,
            Self::KnownFailed { .. } => ToolAttemptDisposition::KnownFailed,
            Self::Ambiguous => ToolAttemptDisposition::Ambiguous,
        }
    }
}

/// Storage-facing terminal attempt classification.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolAttemptDisposition {
    /// Admitted success content exists.
    Completed,
    /// Definitive typed failure evidence exists.
    KnownFailed,
    /// External-effect outcome remains unresolved.
    Ambiguous,
}

/// One exact executor observation before durable application.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolAttemptObservation {
    /// Admitted success content.
    Completed {
        /// Exact bounded result content.
        result: ToolResultContent,
    },
    /// Definitive execution failure.
    KnownFailed {
        /// Exact typed error evidence.
        error: ToolExecutionError,
    },
    /// The external-effect outcome cannot be established.
    Ambiguous,
}

/// The complete fence carried to and returned by an executor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ToolAttemptDispatchCorrelation {
    session: SessionId,
    turn: TurnId,
    issuing_attempt: TurnAttemptId,
    request: ToolRequestId,
    attempt: ToolAttemptId,
    generation: ToolDispatchGeneration,
}

impl ToolAttemptDispatchCorrelation {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the turn attempt that authorized this physical effort.
    pub const fn issuing_attempt(&self) -> TurnAttemptId {
        self.issuing_attempt
    }

    /// Returns the logical request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    /// Returns the physical attempt.
    pub const fn attempt(&self) -> ToolAttemptId {
        self.attempt
    }

    /// Returns the exact dispatch generation.
    pub const fn generation(&self) -> ToolDispatchGeneration {
        self.generation
    }

    /// Binds one executor observation to this exact authorization.
    pub const fn bind(
        self,
        observation: ToolAttemptObservation,
    ) -> CorrelatedToolAttemptObservation {
        CorrelatedToolAttemptObservation {
            correlation: self,
            observation,
        }
    }
}

/// One executor observation bound to exact issued authority.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CorrelatedToolAttemptObservation {
    correlation: ToolAttemptDispatchCorrelation,
    observation: ToolAttemptObservation,
}

impl CorrelatedToolAttemptObservation {
    /// Borrows the exact dispatch fence.
    pub const fn correlation(&self) -> &ToolAttemptDispatchCorrelation {
        &self.correlation
    }

    /// Borrows executor-returned evidence.
    pub const fn observation(&self) -> &ToolAttemptObservation {
        &self.observation
    }
}

/// One current physical tool attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentToolAttempt {
    attempt: ToolAttemptId,
    request: ToolRequestId,
    session: SessionId,
    turn: TurnId,
    issuing_attempt: TurnAttemptId,
    effect_class: ToolEffectClass,
    generation: ToolDispatchGeneration,
    state: CurrentToolAttemptState,
}

impl CurrentToolAttempt {
    /// Returns the physical attempt identity.
    pub const fn attempt(&self) -> ToolAttemptId {
        self.attempt
    }

    /// Returns the logical request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the authorizing turn attempt.
    pub const fn issuing_attempt(&self) -> TurnAttemptId {
        self.issuing_attempt
    }

    /// Returns crash-relevant effect classification.
    pub const fn effect_class(&self) -> ToolEffectClass {
        self.effect_class
    }

    /// Returns the current dispatch generation.
    pub const fn generation(&self) -> ToolDispatchGeneration {
        self.generation
    }

    /// Returns the current local stage.
    pub const fn state(&self) -> CurrentToolAttemptState {
        self.state
    }

    /// Authorizes exactly one executor dispatch from `Prepared`.
    pub fn authorize(self) -> Result<AuthorizedToolAttempt, ToolAttemptTransitionError> {
        if self.state != CurrentToolAttemptState::Prepared {
            return Err(ToolAttemptTransitionError {
                attempt: self,
                failure: ToolAttemptTransitionFailure::InvalidState,
            });
        }
        let correlation = self.correlation();
        Ok(AuthorizedToolAttempt {
            attempt: Self {
                state: CurrentToolAttemptState::InFlight,
                ..self
            },
            correlation,
        })
    }

    /// Reconstitutes exact dispatch authority after an ambiguous commit
    /// acknowledgement only from the durable in-flight stage.
    pub fn resume_in_flight(self) -> Result<AuthorizedToolAttempt, ToolAttemptTransitionError> {
        if self.state != CurrentToolAttemptState::InFlight {
            return Err(ToolAttemptTransitionError {
                attempt: self,
                failure: ToolAttemptTransitionFailure::InvalidState,
            });
        }
        let correlation = self.correlation();
        Ok(AuthorizedToolAttempt {
            attempt: self,
            correlation,
        })
    }

    /// Applies pre-execution lookup or argument failure without authorizing an
    /// executor effect.
    pub fn end_preflight_error(
        self,
        error: ToolExecutionError,
    ) -> Result<EndedToolAttempt, ToolAttemptTransitionError> {
        if self.state != CurrentToolAttemptState::Prepared {
            return Err(ToolAttemptTransitionError {
                attempt: self,
                failure: ToolAttemptTransitionFailure::InvalidState,
            });
        }
        if !matches!(
            error.kind(),
            ToolExecutionErrorKind::UnknownTool | ToolExecutionErrorKind::InvalidArguments
        ) {
            return Err(ToolAttemptTransitionError {
                attempt: self,
                failure: ToolAttemptTransitionFailure::InvalidPreflightError,
            });
        }
        Ok(self.end(ToolAttemptEnd::KnownFailed { error }))
    }

    /// Applies executor evidence through a freshly reloaded exact fence.
    pub fn apply_terminal_observation(
        self,
        observation: CorrelatedToolAttemptObservation,
    ) -> Result<EndedToolAttempt, ToolAttemptTransitionError> {
        if self.state != CurrentToolAttemptState::InFlight {
            return Err(ToolAttemptTransitionError {
                attempt: self,
                failure: ToolAttemptTransitionFailure::InvalidState,
            });
        }
        if observation.correlation != self.correlation() {
            return Err(ToolAttemptTransitionError {
                attempt: self,
                failure: ToolAttemptTransitionFailure::CorrelationMismatch,
            });
        }
        let end = match observation.observation {
            ToolAttemptObservation::Completed { result } => ToolAttemptEnd::Completed { result },
            ToolAttemptObservation::KnownFailed { error }
                if matches!(
                    error.kind(),
                    ToolExecutionErrorKind::ExecutionFailed
                        | ToolExecutionErrorKind::ResultTooLarge
                ) =>
            {
                ToolAttemptEnd::KnownFailed { error }
            }
            ToolAttemptObservation::KnownFailed { .. } => {
                return Err(ToolAttemptTransitionError {
                    attempt: self,
                    failure: ToolAttemptTransitionFailure::InvalidObservationError,
                });
            }
            ToolAttemptObservation::Ambiguous
                if self.effect_class == ToolEffectClass::ExternalEffect =>
            {
                ToolAttemptEnd::Ambiguous
            }
            ToolAttemptObservation::Ambiguous => {
                return Err(ToolAttemptTransitionError {
                    attempt: self,
                    failure: ToolAttemptTransitionFailure::EffectFreeCannotBeAmbiguous,
                });
            }
        };
        Ok(self.end(end))
    }

    /// Classifies prior-process loss without retrying.
    pub fn classify_crash_loss(self) -> ToolAttemptCrashOutcome {
        match (self.state, self.effect_class) {
            (CurrentToolAttemptState::InFlight, ToolEffectClass::ExternalEffect) => {
                ToolAttemptCrashOutcome::Ambiguous(self.end(ToolAttemptEnd::Ambiguous))
            }
            (
                CurrentToolAttemptState::Prepared | CurrentToolAttemptState::InFlight,
                ToolEffectClass::EffectFree,
            )
            | (CurrentToolAttemptState::Prepared, ToolEffectClass::ExternalEffect) => {
                ToolAttemptCrashOutcome::KnownFailed(self.end(ToolAttemptEnd::KnownFailed {
                    error: ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
                }))
            }
        }
    }

    const fn correlation(&self) -> ToolAttemptDispatchCorrelation {
        ToolAttemptDispatchCorrelation {
            session: self.session,
            turn: self.turn,
            issuing_attempt: self.issuing_attempt,
            request: self.request,
            attempt: self.attempt,
            generation: self.generation,
        }
    }

    fn end(self, end: ToolAttemptEnd) -> EndedToolAttempt {
        EndedToolAttempt {
            attempt: self.attempt,
            request: self.request,
            session: self.session,
            turn: self.turn,
            issuing_attempt: self.issuing_attempt,
            effect_class: self.effect_class,
            generation: self.generation,
            end,
        }
    }
}

/// An in-flight attempt paired with the only valid executor fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedToolAttempt {
    attempt: CurrentToolAttempt,
    correlation: ToolAttemptDispatchCorrelation,
}

impl AuthorizedToolAttempt {
    /// Borrows the in-flight attempt.
    pub const fn attempt(&self) -> &CurrentToolAttempt {
        &self.attempt
    }

    /// Returns the executor correlation.
    pub const fn correlation(&self) -> ToolAttemptDispatchCorrelation {
        self.correlation
    }

    /// Returns both values for the authorization transaction and effect.
    pub fn into_parts(self) -> (CurrentToolAttempt, ToolAttemptDispatchCorrelation) {
        (self.attempt, self.correlation)
    }
}

/// Immutable terminal physical-attempt history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndedToolAttempt {
    attempt: ToolAttemptId,
    request: ToolRequestId,
    session: SessionId,
    turn: TurnId,
    issuing_attempt: TurnAttemptId,
    effect_class: ToolEffectClass,
    generation: ToolDispatchGeneration,
    end: ToolAttemptEnd,
}

impl EndedToolAttempt {
    /// Returns the physical attempt identity.
    pub const fn attempt(&self) -> ToolAttemptId {
        self.attempt
    }

    /// Returns the logical request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the authorizing turn attempt.
    pub const fn issuing_attempt(&self) -> TurnAttemptId {
        self.issuing_attempt
    }

    /// Returns crash-relevant effect classification.
    pub const fn effect_class(&self) -> ToolEffectClass {
        self.effect_class
    }

    /// Returns the final dispatch generation.
    pub const fn generation(&self) -> ToolDispatchGeneration {
        self.generation
    }

    /// Borrows terminal evidence.
    pub const fn end(&self) -> &ToolAttemptEnd {
        &self.end
    }
}

/// Restart classification selected solely from durable stage and effect class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAttemptCrashOutcome {
    /// No external effect ambiguity exists; the turn fails without retry.
    KnownFailed(EndedToolAttempt),
    /// An external effect may have occurred and requires reconciliation.
    Ambiguous(EndedToolAttempt),
}

/// Why a local attempt transition was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolAttemptTransitionFailure {
    /// The predecessor stage did not admit this transition.
    InvalidState,
    /// Returned evidence did not carry the exact issued fence.
    CorrelationMismatch,
    /// A preflight path attempted to record a non-preflight error kind.
    InvalidPreflightError,
    /// An executor observation claimed a preflight- or restart-only error kind.
    InvalidObservationError,
    /// Effect-free work cannot produce external-effect ambiguity.
    EffectFreeCannotBeAmbiguous,
}

/// Rejected transition retaining the unchanged current attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolAttemptTransitionError {
    attempt: CurrentToolAttempt,
    failure: ToolAttemptTransitionFailure,
}

impl ToolAttemptTransitionError {
    /// Borrows the unchanged attempt.
    pub const fn attempt(&self) -> &CurrentToolAttempt {
        &self.attempt
    }

    /// Returns the transition failure.
    pub const fn failure(&self) -> ToolAttemptTransitionFailure {
        self.failure
    }

    /// Returns the unchanged attempt and failure.
    pub fn into_parts(self) -> (CurrentToolAttempt, ToolAttemptTransitionFailure) {
        (self.attempt, self.failure)
    }
}

/// Stored local stage for complete attempt reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAttemptReconstitutionState {
    /// No executor effect was authorized.
    Prepared,
    /// The exact generation was authorized and remains live.
    InFlight,
    /// Terminal evidence is immutable.
    Ended(ToolAttemptEnd),
}

/// Complete independently stored physical-attempt facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolAttemptReconstitutionInput {
    attempt: ToolAttemptId,
    request: ToolRequestId,
    session: SessionId,
    turn: TurnId,
    issuing_attempt: TurnAttemptId,
    effect_class: ToolEffectClass,
    generation: ToolDispatchGeneration,
    state: ToolAttemptReconstitutionState,
}

impl ToolAttemptReconstitutionInput {
    /// Supplies every typed stored authorization and lifecycle fact.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        attempt: ToolAttemptId,
        request: ToolRequestId,
        session: SessionId,
        turn: TurnId,
        issuing_attempt: TurnAttemptId,
        effect_class: ToolEffectClass,
        generation: ToolDispatchGeneration,
        state: ToolAttemptReconstitutionState,
    ) -> Self {
        Self {
            attempt,
            request,
            session,
            turn,
            issuing_attempt,
            effect_class,
            generation,
            state,
        }
    }

    /// Reconstitutes local immutable/current shape without claiming aggregate
    /// ordering, uniqueness, or approval correlation.
    pub fn reconstitute(self) -> ReconstitutedToolAttempt {
        let Self {
            attempt,
            request,
            session,
            turn,
            issuing_attempt,
            effect_class,
            generation,
            state,
        } = self;
        match state {
            ToolAttemptReconstitutionState::Prepared => {
                ReconstitutedToolAttempt::Current(CurrentToolAttempt {
                    attempt,
                    request,
                    session,
                    turn,
                    issuing_attempt,
                    effect_class,
                    generation,
                    state: CurrentToolAttemptState::Prepared,
                })
            }
            ToolAttemptReconstitutionState::InFlight => {
                ReconstitutedToolAttempt::Current(CurrentToolAttempt {
                    attempt,
                    request,
                    session,
                    turn,
                    issuing_attempt,
                    effect_class,
                    generation,
                    state: CurrentToolAttemptState::InFlight,
                })
            }
            ToolAttemptReconstitutionState::Ended(end) => {
                ReconstitutedToolAttempt::Ended(EndedToolAttempt {
                    attempt,
                    request,
                    session,
                    turn,
                    issuing_attempt,
                    effect_class,
                    generation,
                    end,
                })
            }
        }
    }
}

/// Local result of typed physical-attempt reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconstitutedToolAttempt {
    /// One nonterminal attempt.
    Current(CurrentToolAttempt),
    /// One terminal immutable attempt.
    Ended(EndedToolAttempt),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DecideToolRequest, DurableCommandId, NormalizedToolArguments, ToolApprovalDecision,
        ToolName, ToolRequestOrdinal, ToolRequestReconstitutionInput, ToolResultText,
        test_support::{
            model_call_id, session_id, tool_attempt_id, tool_request_id, turn_attempt_id, turn_id,
        },
    };

    fn approved_request() -> ApprovedToolRequest {
        let request = ToolRequestReconstitutionInput::new(
            tool_request_id(1),
            session_id(2),
            turn_id(3),
            model_call_id(4),
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("current_time")).expect("canonical name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("canonical arguments are valid"),
        )
        .into_request();
        let command = DecideToolRequest::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(5)),
            request.id(),
            ToolApprovalDecision::Approve,
        );
        let prepared = command
            .prepare_applied(&request)
            .expect("the request and command correlate");
        let crate::DecideToolRequestResult::Applied(applied) = prepared.result() else {
            panic!("matching request prepares application");
        };
        ApprovedToolRequest::try_from_resolution(request, applied.resolution().clone())
            .expect("the resolution approves the same request")
    }

    fn prepared(effect_class: ToolEffectClass) -> CurrentToolAttempt {
        approved_request().prepare_attempt(tool_attempt_id(6), turn_attempt_id(7), effect_class)
    }

    /// S12 / INV-004 / INV-011 / INV-021: result application requires the
    /// exact request/turn/attempt/generation fence.
    #[test]
    fn s12_inv004_inv011_inv021_result_rejects_a_stale_fence_unchanged() {
        let authorized = prepared(ToolEffectClass::ExternalEffect)
            .authorize()
            .expect("prepared work can be authorized");
        let (in_flight, correlation) = authorized.into_parts();
        let stale = ToolAttemptDispatchCorrelation {
            generation: ToolDispatchGeneration::try_from_u64(2)
                .expect("two is a positive generation"),
            ..correlation
        };
        let error = in_flight
            .clone()
            .apply_terminal_observation(stale.bind(ToolAttemptObservation::Ambiguous))
            .expect_err("a stale generation cannot advance the attempt");

        assert_eq!(
            error.failure(),
            ToolAttemptTransitionFailure::CorrelationMismatch
        );
        assert_eq!(error.attempt(), &in_flight);
    }

    /// S15 / INV-024: executor success is evidence only; the hub transition
    /// creates terminal content authority.
    #[test]
    fn s15_inv024_authorized_success_commits_exact_result_evidence() {
        let authorized = prepared(ToolEffectClass::EffectFree)
            .authorize()
            .expect("prepared work can be authorized");
        let (in_flight, correlation) = authorized.into_parts();
        let text = ToolResultText::try_new(String::from(r#"{"timezone":"UTC"}"#))
            .expect("bounded result is valid");
        let ended = in_flight
            .apply_terminal_observation(correlation.bind(ToolAttemptObservation::Completed {
                result: ToolResultContent::Text(text.clone()),
            }))
            .expect("matching executor evidence can terminalize");

        assert!(matches!(
            ended.end(),
            ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(actual),
            } if actual == &text
        ));
    }

    /// S12 / INV-011 / INV-024: ambiguous commit recovery can reconstruct
    /// dispatch authority only from the exact durable in-flight checkpoint.
    #[test]
    fn s12_inv011_inv024_in_flight_reread_restores_exact_dispatch_authority() {
        let authorized = prepared(ToolEffectClass::ExternalEffect)
            .authorize()
            .expect("prepared work can be authorized");
        let (in_flight, expected) = authorized.into_parts();
        let resumed = in_flight
            .resume_in_flight()
            .expect("the durable in-flight stage restores authority");

        assert_eq!(resumed.correlation(), expected);
        assert_eq!(
            prepared(ToolEffectClass::ExternalEffect)
                .resume_in_flight()
                .expect_err("prepared work has not crossed authorization")
                .failure(),
            ToolAttemptTransitionFailure::InvalidState
        );
    }

    /// S15 / INV-024: executor observations cannot claim error kinds reserved
    /// for preflight validation or restart classification.
    #[test]
    fn s15_inv024_executor_cannot_claim_preflight_or_crash_errors() {
        for kind in [
            ToolExecutionErrorKind::UnknownTool,
            ToolExecutionErrorKind::InvalidArguments,
            ToolExecutionErrorKind::CrashLost,
        ] {
            let authorized = prepared(ToolEffectClass::ExternalEffect)
                .authorize()
                .expect("prepared work can be authorized");
            let (in_flight, correlation) = authorized.into_parts();
            let error = in_flight
                .apply_terminal_observation(correlation.bind(ToolAttemptObservation::KnownFailed {
                    error: ToolExecutionError::new(kind, None),
                }))
                .expect_err("the executor cannot manufacture reserved evidence");

            assert_eq!(
                error.failure(),
                ToolAttemptTransitionFailure::InvalidObservationError
            );
        }
    }

    /// S05 / INV-024 / INV-026: crash loss of effect-free issued work is a
    /// known failure and never an automatic retry.
    #[test]
    fn s05_inv024_inv026_effect_free_crash_loss_is_known_failed() {
        let in_flight = prepared(ToolEffectClass::EffectFree)
            .authorize()
            .expect("prepared work can be authorized")
            .into_parts()
            .0;
        let ToolAttemptCrashOutcome::KnownFailed(ended) = in_flight.classify_crash_loss() else {
            panic!("effect-free loss is definitive");
        };

        assert!(matches!(
            ended.end(),
            ToolAttemptEnd::KnownFailed { error }
                if error.kind() == ToolExecutionErrorKind::CrashLost
        ));
    }

    /// S06 / INV-025 / INV-026: crash loss of in-flight external-effect work
    /// is ambiguous and retains the exact attempt.
    #[test]
    fn s06_inv025_inv026_external_effect_crash_loss_is_ambiguous() {
        let in_flight = prepared(ToolEffectClass::ExternalEffect)
            .authorize()
            .expect("prepared work can be authorized")
            .into_parts()
            .0;
        let attempt = in_flight.attempt();
        let ToolAttemptCrashOutcome::Ambiguous(ended) = in_flight.classify_crash_loss() else {
            panic!("external-effect loss is ambiguous");
        };

        assert_eq!(ended.attempt(), attempt);
        assert_eq!(ended.end(), &ToolAttemptEnd::Ambiguous);
    }

    /// S10 / INV-027: a denial cannot produce approved-request authority.
    #[test]
    fn s10_inv027_denial_cannot_authorize_an_attempt() {
        let request = ToolRequestReconstitutionInput::new(
            tool_request_id(10),
            session_id(2),
            turn_id(3),
            model_call_id(4),
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("risky_tool")).expect("canonical name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("canonical arguments are valid"),
        )
        .into_request();
        let command = DecideToolRequest::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(11)),
            request.id(),
            ToolApprovalDecision::Deny { reason: None },
        );
        let prepared = command
            .prepare_applied(&request)
            .expect("the request and command correlate");
        let crate::DecideToolRequestResult::Applied(applied) = prepared.result() else {
            panic!("matching request prepares application");
        };
        let error = ApprovedToolRequest::try_from_resolution(request, applied.resolution().clone())
            .expect_err("denial is not execution authority");

        assert!(matches!(
            error.approval().decision(),
            ToolApprovalDecision::Deny { .. }
        ));
    }
}

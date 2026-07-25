//! Application-owned transaction shapes implemented by durable tool storage.
//!
//! These ports keep storage adapters dependent on application contracts while
//! leaving tool selection, execution, and retry orchestration in the
//! application layer.

use std::future::Future;

use signalbox_domain::{
    AcceptedInputId, AuthorizedToolAttempt, CorrelatedToolAttemptObservation, CurrentToolAttempt,
    DecideToolRequest, EndedToolAttempt, FailedModelCallTurn, FailedModelCallTurnIdentities,
    ModelCallId, PreparedDecideToolRequest, SemanticTranscriptEntryId, SemanticTranscriptEntryRef,
    SessionId, ToolApprovalResolution, ToolAttemptCrashOutcome, ToolAttemptId, ToolBatch,
    ToolEffectClass, ToolExecutionError, ToolRequest, TurnAttemptId, TurnId,
};

use crate::ClassifyOperatorFailure;

/// Storage-resolved authority for one tool-related semantic entry.
///
/// The prepared-model renderer correlates this evidence against the exact
/// reference-only semantic payload before exposing provider-neutral messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedToolConversationEntry {
    /// The request record referenced by one assistant tool-use entry.
    AssistantToolUse {
        /// Source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// Immutable request content authority.
        request: ToolRequest,
    },
    /// The terminal attempt and request referenced by an execution-result entry.
    ExecutionResult {
        /// Source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// Immutable request content authority.
        request: ToolRequest,
        /// Terminal physical result authority.
        attempt: EndedToolAttempt,
    },
    /// The owner decision and request referenced by one denial entry.
    Denied {
        /// Source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// Immutable request content authority.
        request: ToolRequest,
        /// Exact durable denial and provenance.
        approval: ToolApprovalResolution,
    },
    /// The request referenced by one closed-by-turn-end entry.
    Closed {
        /// Source-qualified semantic entry.
        source: SemanticTranscriptEntryRef,
        /// Immutable request content authority.
        request: ToolRequest,
    },
}

impl ResolvedToolConversationEntry {
    /// Returns the semantic entry whose references this evidence resolves.
    pub const fn source(&self) -> SemanticTranscriptEntryRef {
        match self {
            Self::AssistantToolUse { source, .. }
            | Self::ExecutionResult { source, .. }
            | Self::Denied { source, .. }
            | Self::Closed { source, .. } => *source,
        }
    }
}

/// Authoritative reread after an ambiguous attempt-authorization commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAttemptAuthorizationStatus {
    /// Authorization did not commit; the exact attempt remains prepared.
    Prepared(CurrentToolAttempt),
    /// Authorization committed; this exact fence may enter the executor.
    InFlight(AuthorizedToolAttempt),
}

/// Transaction consuming one owner decision and advancing the exact wait.
pub trait DecideToolRequestTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Applies a replay-safe command, consuming a fresh attempt candidate only
    /// when the final decision opens execution.
    fn decide<NextAttempt>(
        &mut self,
        command: DecideToolRequest,
        next_attempt: NextAttempt,
    ) -> impl Future<Output = Result<PreparedDecideToolRequest, Self::Error>> + Send
    where
        NextAttempt: FnMut() -> TurnAttemptId + Send;
}

/// Fresh identities for one all-resolved continuation transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolContinuationIdentities {
    result_entries: Box<[SemanticTranscriptEntryId]>,
    result_frontier: signalbox_domain::ContextFrontierId,
    call: ModelCallId,
    target_failure: FailedModelCallTurnIdentities,
    steering_frontier: signalbox_domain::ContextFrontierId,
}

impl ToolContinuationIdentities {
    /// Supplies exact proposal-order result identities and staged-call candidates.
    pub fn new(
        result_entries: Vec<SemanticTranscriptEntryId>,
        result_frontier: signalbox_domain::ContextFrontierId,
        call: ModelCallId,
        target_failure: FailedModelCallTurnIdentities,
        steering_frontier: signalbox_domain::ContextFrontierId,
    ) -> Self {
        Self {
            result_entries: result_entries.into_boxed_slice(),
            result_frontier,
            call,
            target_failure,
            steering_frontier,
        }
    }

    /// Returns result-entry identities in request order.
    pub fn result_entries(&self) -> &[SemanticTranscriptEntryId] {
        &self.result_entries
    }

    /// Returns the yielded-plus-results frontier candidate.
    pub const fn result_frontier(&self) -> signalbox_domain::ContextFrontierId {
        self.result_frontier
    }

    /// Returns the next model-call candidate.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Borrows target-failure closure candidates.
    pub const fn target_failure(&self) -> &FailedModelCallTurnIdentities {
        &self.target_failure
    }

    /// Returns the pending-steering frontier candidate.
    pub const fn steering_frontier(&self) -> signalbox_domain::ContextFrontierId {
        self.steering_frontier
    }
}

/// Fresh identities for proposal-ordered closure before a known crash failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCrashClosureIdentities {
    result_entries: Box<[SemanticTranscriptEntryId]>,
    result_frontier: signalbox_domain::ContextFrontierId,
    failure: FailedModelCallTurnIdentities,
}

impl ToolCrashClosureIdentities {
    /// Supplies one closure identity per request plus the terminal failure pair.
    pub fn new(
        result_entries: Vec<SemanticTranscriptEntryId>,
        result_frontier: signalbox_domain::ContextFrontierId,
        failure: FailedModelCallTurnIdentities,
    ) -> Self {
        Self {
            result_entries: result_entries.into_boxed_slice(),
            result_frontier,
            failure,
        }
    }

    /// Returns closure-entry identities in proposal order.
    pub fn result_entries(&self) -> &[SemanticTranscriptEntryId] {
        &self.result_entries
    }

    /// Returns the yielded-plus-closures frontier candidate.
    pub const fn result_frontier(&self) -> signalbox_domain::ContextFrontierId {
        self.result_frontier
    }

    /// Borrows the subsequent `TurnFailed` identity pair.
    pub const fn failure(&self) -> &FailedModelCallTurnIdentities {
        &self.failure
    }
}

/// Atomic continuation outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareToolContinuationOutcome {
    /// The scheduling hint no longer identifies an all-resolved active batch.
    NoWork,
    /// Results, steering, and the next Prepared call committed together.
    Checkpointed(ModelCallId),
    /// Target resolution failed and the turn closed in the same transaction.
    TargetUnavailable(Box<FailedModelCallTurn>),
}

/// Authoritative status of one unchanged in-memory executor observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedToolAttemptObservationStatus {
    /// The exact in-flight attempt still awaits this observation.
    Pending,
    /// The exact observation is already represented durably.
    AlreadyCommitted,
}

/// Authoritative tool execution and continuation transactions.
pub trait ToolExecutionTransaction {
    /// Adapter-specific classified failure.
    type Error: ClassifyOperatorFailure;

    /// Reloads one active batch without granting mutation authority.
    fn load_active_batch(
        &mut self,
        session: SessionId,
        turn: TurnId,
    ) -> impl Future<Output = Result<Option<ToolBatch>, Self::Error>> + Send;

    /// Commits the next proposal-order Prepared attempt.
    fn prepare_next_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> impl Future<Output = Result<Option<signalbox_domain::CurrentToolAttempt>, Self::Error>> + Send;

    /// Authorizes one exact Prepared attempt.
    fn authorize_attempt(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> impl Future<Output = Result<AuthorizedToolAttempt, Self::Error>> + Send;

    /// Rereads whether an ambiguously acknowledged authorization committed.
    fn reread_ambiguous_authorization(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
    ) -> impl Future<Output = Result<ToolAttemptAuthorizationStatus, Self::Error>> + Send;

    /// Commits a catalog/decode failure without authorizing an executor effect.
    fn commit_preflight_error(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        error: ToolExecutionError,
    ) -> impl Future<Output = Result<EndedToolAttempt, Self::Error>> + Send;

    /// Commits exact executor evidence through its durable fence.
    fn commit_observation(
        &mut self,
        observation: CorrelatedToolAttemptObservation,
    ) -> impl Future<Output = Result<EndedToolAttempt, Self::Error>> + Send;

    /// Rereads whether one retained executor observation committed.
    fn reread_observation(
        &mut self,
        observation: &CorrelatedToolAttemptObservation,
    ) -> impl Future<Output = Result<RetainedToolAttemptObservationStatus, Self::Error>> + Send;

    /// Classifies one prior-process live attempt without retrying it.
    fn classify_crash_loss<NextTurn>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        identities: ToolCrashClosureIdentities,
        next_turn: NextTurn,
    ) -> impl Future<Output = Result<ToolAttemptCrashOutcome, Self::Error>> + Send
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send;

    /// Atomically projects all results, consumes steering, and checkpoints the
    /// next model call.
    fn prepare_continuation<NextSteering>(
        &mut self,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        identities: ToolContinuationIdentities,
        next_steering: NextSteering,
    ) -> impl Future<Output = Result<PrepareToolContinuationOutcome, Self::Error>> + Send
    where
        NextSteering: FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send;
}

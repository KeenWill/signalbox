//! Application orchestration boundary for Signalbox.
//!
//! This crate coordinates domain decisions and external effects while
//! depending inward on `signalbox-domain`.

mod create_session;
mod load_session;
mod model_execution;
mod operator_failure;
mod replace_session_defaults;
mod scheduler;
mod start_eligible_turn;
mod startup_scan;
mod submit_input;
mod tool_dispatch_gate;
mod tool_loop;
mod tool_loop_ports;

pub use create_session::{
    CreateSessionError, CreateSessionOutcome, CreateSessionRequest, CreateSessionService,
    CreateSessionTransaction, InvalidDurableCommandId, SessionIdGenerator,
    UuidV7SessionIdGenerator,
};
pub use load_session::{LoadSessionService, SessionReader};
pub use model_execution::{
    AttemptDispatchGate, AuthorizeModelCallOutcome, AuthorizeModelCallTransaction,
    CommitModelCallObservationTransaction, FailPreparedModelCallTransaction,
    InProcessAttemptDispatchGate, InProcessAttemptDispatchPermit, ModelCallAuthorizationReread,
    ModelCallCapabilityPreparation, ModelCallCredentialReference, ModelCallExecutionError,
    ModelCallExecutionIdGenerator, ModelCallExecutionOutcome, ModelCallExecutionService,
    ModelCallProvider, ModelCallTerminalIdentityCandidates, ModelConversationMessage,
    ModelFrontierRenderingError, ModelToolResultContent, PrepareModelCallOutcome,
    PrepareModelCallTransaction, PreparedModelOperation, RetainedCapabilityFailureStatus,
    RetainedModelCallExecutionState, RetainedModelCallObservationStatus,
    ScriptedModelCallCapability, ScriptedModelCallError, ScriptedModelCallProvider,
    ScriptedModelCallStep, UuidV7ModelCallExecutionIdGenerator,
};
pub use operator_failure::{ClassifyOperatorFailure, OperatorFailureClass};
pub use replace_session_defaults::{
    ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsRequest, ReplaceSessionDefaultsService,
    ReplaceSessionDefaultsTransaction,
};
pub use scheduler::{
    EligibilityNudge, EligibilityNudgeOutcome, EligibilityPass, EligibilitySweep,
    EligibilitySweepBatch, EligibilityWorkSource, InProcessEligibilityNudge,
    InProcessEligibilityWorkSource, InvalidReconciliationSweepInterval,
    ReconciliationSweepInterval, SchedulerLoop, SchedulerLoopExit,
};
pub use start_eligible_turn::{
    StartEligibleTurnIdGenerator, StartEligibleTurnOutcome, StartEligibleTurnService,
    StartEligibleTurnTransaction, UuidV7StartEligibleTurnIdGenerator,
};
pub use startup_scan::{
    StartupScanError, StartupScanIdGenerator, StartupScanOutcome, StartupScanRepository,
    StartupScanService, StartupScanSessionOutcome, UuidV7StartupScanIdGenerator,
};
pub use submit_input::{
    SubmitInputIdGenerator, SubmitInputOutcome, SubmitInputRequest, SubmitInputRequestError,
    SubmitInputService, SubmitInputTransaction, UuidV7SubmitInputIdGenerator,
};
pub use tool_dispatch_gate::{InProcessToolDispatchGate, InProcessToolDispatchPermit};
pub use tool_loop::{
    CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence, DecideToolRequestService,
    DuplicateToolDefinition, InProcessToolDecisionWake, NoToolCatalog, RetainedToolExecutionState,
    ToolApprovalIdGenerator, ToolArgumentValidator, ToolCatalog, ToolCatalogValidationFailure,
    ToolDefinition, ToolExecutionIdGenerator, ToolExecutionInvocation, ToolExecutionService,
    ToolExecutionServiceError, ToolExecutionServiceOutcome, ToolExecutor, ToolExecutorEvidence,
    ToolInputSchema, ToolInputSchemaError, ToolInputSchemaFailure, UuidV7ToolLoopIdGenerator,
};
pub use tool_loop_ports::{
    DecideToolRequestTransaction, PrepareToolContinuationOutcome, ResolvedToolConversationEntry,
    RetainedToolAttemptObservationStatus, ToolAttemptAuthorizationStatus,
    ToolContinuationIdentities, ToolExecutionTransaction,
};

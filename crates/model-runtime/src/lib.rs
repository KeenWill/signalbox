//! Provider-neutral typed model-runtime core (ADR-0047 Layer 1).
//!
//! This crate is the shared vocabulary of the model-runtime layer: the typed
//! operation a caller authorizes, the observation stream an adapter emits
//! while executing it, and the terminal evidence the caller classifies
//! afterwards. Provider adapters (one crate per provider) translate exactly
//! one [`ModelOperation`] into at most one provider interaction and report
//! typed facts; they never decide lifecycle outcomes.
//!
//! Despite the name, this layer is unrelated to the hub's asynchronous
//! runtime (ADR-0044): a *model runtime* here is a library that executes one
//! explicitly authorized model operation against a provider and reports
//! evidence about what happened.
//!
//! # Boundary rules (ADR-0047, binding)
//!
//! - This crate depends on no Signalbox domain, application, persistence, or
//!   hub crate, and none of those crates may depend on it. Caller identity
//!   enters as the opaque correlation parameter `C` threaded through
//!   [`ModelOperation`], every [`Observation`], and the final
//!   [`TerminalReport`]; no domain identifier type is imported or redefined
//!   here.
//! - One operation, one interaction (ADR-0005): nothing in this layer
//!   retries, falls back, or repeats a request after the provider could have
//!   accepted it. There is no retry machinery to disable.
//! - Evidence, not classification (ADR-0043): adapters report what provably
//!   happened — prepared, possibly accepted, definitive response, incomplete
//!   stream — and the caller classifies dispositions. See [`TerminalEvidence`]
//!   for the intended mapping onto ADR-0043's vocabulary.
//! - Structured-output parsing and tool-call decoding are pure functions.
//!   Parsing never performs a model call; a repair call is a new, explicitly
//!   authorized operation owned by the caller.
//!
//! Every trait and signature in this crate is draft scaffolding under
//! ADR-0047: the application-side model-execution port is owned by the
//! orchestration ADR process, and this crate is rewritten to conform when
//! that port lands.

mod credential;
mod evidence;
mod message;
mod observation;
mod operation;
mod output;
mod runtime;
mod scripted;
mod settings;
mod sse;
mod target;
mod tool;
mod usage;

pub use credential::{
    CredentialAccess, CredentialAccessError, CredentialAccessFailure, CredentialReference,
    CredentialValue,
};
pub use evidence::{
    BoundaryLossEvidence, CancellationConfirmedEvidence, CompletionEvidence, CompletionFinish,
    ExchangeFacts, FinishReason, LossCause, NativeErrorFacts, PreparationFailure,
    ProvenUnsentEvidence, ProviderErrorEvidence, ProviderErrorKind, ProviderMessageId,
    ProviderRequestId, RefusalEvidence, StreamInterruption, TerminalEvidence, TerminalReport,
    TransportFacts, UnsentCause,
};
pub use message::{
    AssistantPart, ConversationMessage, ConversationRole, MessagePart, ToolResultRecord,
};
pub use observation::{Observation, ObservationFact, ObservationSink};
pub use operation::{DeliveryMode, ModelOperation, ModelOperationValidationError, ToolChoice};
pub use output::{
    DomainValidator, NoDomainConstraints, StructuredDecodeFailure, StructuredOutputContract,
    decode_structured, decode_structured_json,
};
pub use runtime::{CancellationSignal, ModelRuntime};
pub use scripted::{Script, ScriptedModel};
pub use settings::ModelSettings;
pub use sse::{SseFraming, SseFramingError, SsePushOutcome, SseRecord, SseTermination};
pub use target::{ProviderReportedModel, RequestedTarget, ResolvedTarget};
pub use tool::{
    ToolCallId, ToolCallProposal, ToolDecodeFailure, ToolDefinition, ToolName,
    decode_tool_arguments,
};
pub use usage::TokenUsage;

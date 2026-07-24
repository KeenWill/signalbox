//! Provider-neutral typed model-runtime core (Layer 1).
//!
//! This crate is the shared vocabulary of the model-runtime layer: the typed
//! operation a caller authorizes, the observation stream an adapter emits
//! while executing it, and the terminal evidence the caller classifies
//! afterwards. Provider adapters (one crate per provider) translate exactly
//! one [`ModelOperation`] into at most one provider interaction and report
//! typed facts; they never decide lifecycle outcomes. The two-stage boundary
//! in docs/spec/runtime-substrate.md is represented directly: request
//! preparation yields an opaque one-shot capability, and execution consumes
//! it only after the caller has durably authorized the provider interaction.
//!
//! Despite the name, this layer is unrelated to the hub's asynchronous
//! runtime: per docs/spec/runtime-substrate.md, a *model runtime* here is a
//! library that executes one explicitly authorized model operation against a
//! provider and reports evidence about what happened.
//!
//! # Boundary rules (docs/spec/runtime-substrate.md, binding)
//!
//! - This crate depends on no Signalbox domain, application, persistence, or
//!   hub crate, and none of those crates may depend on it. Caller identity
//!   enters as the opaque correlation parameter `C` threaded through
//!   [`ModelOperation`], every [`Observation`], and the final
//!   [`TerminalReport`]; no domain identifier type is imported or redefined
//!   here.
//! - One operation, one interaction: nothing in this layer retries, falls
//!   back, or repeats a request after the provider could have accepted it.
//!   There is no retry machinery to disable.
//! - Evidence, not classification: adapters report what provably
//!   happened — possibly accepted, definitive response, incomplete
//!   stream — and the caller classifies dispositions. See [`TerminalEvidence`]
//!   for the intended mapping onto the disposition vocabulary in
//!   docs/spec/model-call-execution.md.
//! - Structured-output parsing and tool-call decoding are pure functions.
//!   Parsing never performs a model call; a repair call is a new, explicitly
//!   authorized operation owned by the caller.
//!
//! The two-stage [`ModelRuntime`] interface conforms to the
//! provider-interaction boundary in docs/spec/runtime-substrate.md. It
//! remains a Layer-1 interface: application and domain crates neither import
//! these types nor delegate lifecycle policy to them.

mod credential;
mod evidence;
mod message;
mod observation;
mod operation;
mod output;
mod preparation;
mod provider_json;
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
    ExchangeFacts, FinishReason, LossCause, NativeErrorFacts, ProvenUnsentEvidence,
    ProviderErrorEvidence, ProviderErrorKind, ProviderMessageId, ProviderRequestId,
    RefusalEvidence, StreamInterruption, TerminalEvidence, TerminalReport, TransportFacts,
    UnsentCause,
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
pub use preparation::{PreparationDefect, PreparationFailure, PreparationOutcome};
pub use provider_json::{
    PROVIDER_JSON_NESTING_LIMIT, ProviderJsonNestingExceeded, ProviderJsonNestingValidator,
    validate_provider_json_nesting,
};
pub use runtime::{CancellationSignal, ModelRuntime};
pub use scripted::{Script, ScriptedModel, ScriptedPrepared};
pub use settings::ModelSettings;
pub use sse::{SseFraming, SseFramingError, SsePushOutcome, SseRecord, SseTermination};
pub use target::{ProviderReportedModel, RequestedTarget, ResolvedTarget};
pub use tool::{
    ToolCallId, ToolCallProposal, ToolDecodeFailure, ToolDefinition, ToolName,
    decode_tool_arguments,
};
pub use usage::TokenUsage;

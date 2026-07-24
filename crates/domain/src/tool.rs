//! Logical tool requests, approval provenance, and result values.
//!
//! `docs/spec/tool-loop.md` is normative. This module owns bounded,
//! provider-neutral request content and the approval algebra. Physical
//! execution lives in `tool_attempt`; persistence, registry lookup, and
//! executor selection remain outside the domain boundary.

use serde::{Deserialize, Serialize, de::IgnoredAny};

use crate::{
    AssistantText, DurableCommandId, ModelCallId, SessionId, ToolAttemptId, ToolRequestId, TurnId,
};

const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_TOOL_DENIAL_REASON_BYTES: usize = 1024;
const MAX_TOOL_RESULT_TEXT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_REQUESTS_PER_RESPONSE: usize = 32;

/// One checked model-facing tool name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolName(String);

impl ToolName {
    /// Checks the closed baseline spelling without rewriting it.
    pub fn try_new(value: String) -> Result<Self, ToolNameError> {
        let failure = if value.is_empty() {
            Some(ToolNameFailure::Empty)
        } else if value.len() > MAX_TOOL_NAME_BYTES {
            Some(ToolNameFailure::TooLong { bytes: value.len() })
        } else {
            value
                .char_indices()
                .find(|(_, character)| {
                    !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
                })
                .map(
                    |(byte_index, character)| ToolNameFailure::InvalidCharacter {
                        byte_index,
                        character,
                    },
                )
        };

        match failure {
            Some(failure) => Err(ToolNameError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact checked spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked spelling.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why a proposed tool name is outside the baseline spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolNameFailure {
    /// No name was supplied.
    Empty,
    /// The UTF-8 spelling exceeds the baseline bound.
    TooLong {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// One scalar is outside ASCII alphanumeric, underscore, and hyphen.
    InvalidCharacter {
        /// Its UTF-8 byte offset.
        byte_index: usize,
        /// The rejected scalar.
        character: char,
    },
}

/// Failed tool-name construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolNameError {
    value: String,
    failure: ToolNameFailure,
}

impl ToolNameError {
    /// Borrows the rejected spelling.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the exact validation failure.
    pub const fn failure(&self) -> ToolNameFailure {
        self.failure
    }

    /// Returns the rejected spelling and failure.
    pub fn into_parts(self) -> (String, ToolNameFailure) {
        (self.value, self.failure)
    }
}

/// Which bounded representation normalized tool arguments carry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolArgumentsKind {
    /// Compact JSON with recursively lexical object keys.
    Json,
    /// Exact provider text that did not decode as JSON.
    Undecodable,
}

/// One bounded normalized tool-argument value.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NormalizedToolArguments {
    kind: ToolArgumentsKind,
    value: String,
}

impl NormalizedToolArguments {
    /// Normalizes exact provider-supplied UTF-8 text.
    ///
    /// Syntactically valid JSON is compacted with recursively lexical object
    /// keys. Invalid JSON remains exact and is tagged `Undecodable`.
    pub fn try_from_provider_text(value: String) -> Result<Self, ToolArgumentsError> {
        if value.len() > MAX_TOOL_ARGUMENT_BYTES {
            return Err(ToolArgumentsError {
                failure: ToolArgumentsFailure::TooLarge { bytes: value.len() },
                value,
            });
        }

        if !is_complete_json(&value) {
            return Ok(Self {
                kind: ToolArgumentsKind::Undecodable,
                value,
            });
        }
        let mut deserializer = serde_json::Deserializer::from_str(&value);
        deserializer.disable_recursion_limit();
        let deserializer = serde_stacker::Deserializer::new(&mut deserializer);
        let decoded = match serde_json::Value::deserialize(deserializer) {
            Ok(decoded) => decoded,
            Err(_) => {
                return Err(ToolArgumentsError {
                    value,
                    failure: ToolArgumentsFailure::CanonicalizationFailed,
                });
            }
        };
        let mut canonical = Vec::with_capacity(value.len());
        let mut serializer = serde_json::Serializer::new(&mut canonical);
        let serializer = serde_stacker::Serializer::new(&mut serializer);
        let serialization = decoded.serialize(serializer);
        drop_json_value_iteratively(decoded);
        serialization.map_err(|_| ToolArgumentsError {
            value: value.clone(),
            failure: ToolArgumentsFailure::CanonicalizationFailed,
        })?;
        let value = String::from_utf8(canonical).map_err(|_| ToolArgumentsError {
            value,
            failure: ToolArgumentsFailure::CanonicalizationFailed,
        })?;
        if value.len() > MAX_TOOL_ARGUMENT_BYTES {
            return Err(ToolArgumentsError {
                failure: ToolArgumentsFailure::CanonicalTooLarge { bytes: value.len() },
                value,
            });
        }
        Ok(Self {
            kind: ToolArgumentsKind::Json,
            value,
        })
    }

    /// Reconstitutes one stored normalized value, rejecting representation
    /// drift or a false kind tag.
    pub fn try_from_stored(
        kind: ToolArgumentsKind,
        value: String,
    ) -> Result<Self, ToolArgumentsError> {
        let normalized = Self::try_from_provider_text(value.clone())?;
        if normalized.kind != kind {
            return Err(ToolArgumentsError {
                value,
                failure: ToolArgumentsFailure::StoredKindMismatch,
            });
        }
        if normalized.value != value {
            return Err(ToolArgumentsError {
                value,
                failure: ToolArgumentsFailure::StoredJsonNotCanonical,
            });
        }
        Ok(normalized)
    }

    /// Returns the closed representation tag.
    pub const fn kind(&self) -> ToolArgumentsKind {
        self.kind
    }

    /// Borrows the canonical JSON or exact undecodable text.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Returns the tag and stored text.
    pub fn into_parts(self) -> (ToolArgumentsKind, String) {
        (self.kind, self.value)
    }
}

fn is_complete_json(value: &str) -> bool {
    let mut deserializer = serde_json::Deserializer::from_str(value);
    deserializer.disable_recursion_limit();
    let decoded = {
        let deserializer = serde_stacker::Deserializer::new(&mut deserializer);
        IgnoredAny::deserialize(deserializer)
    };
    decoded.is_ok() && deserializer.end().is_ok()
}

fn drop_json_value_iteratively(value: serde_json::Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Array(mut values) => pending.append(&mut values),
            serde_json::Value::Object(values) => pending.extend(values.into_values()),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }
}

/// Why tool-argument normalization or reconstitution failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolArgumentsFailure {
    /// Provider text exceeded the admission bound.
    TooLarge {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// Canonical JSON exceeded the admission bound.
    CanonicalTooLarge {
        /// The canonical UTF-8 byte count.
        bytes: usize,
    },
    /// Serialization of an already-decoded JSON value unexpectedly failed.
    CanonicalizationFailed,
    /// The stored tag disagreed with whether the text decodes as JSON.
    StoredKindMismatch,
    /// Stored JSON was not in its canonical compact representation.
    StoredJsonNotCanonical,
}

/// Failed argument construction retaining the rejected text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolArgumentsError {
    value: String,
    failure: ToolArgumentsFailure,
}

impl ToolArgumentsError {
    /// Borrows the rejected text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the exact normalization failure.
    pub const fn failure(&self) -> ToolArgumentsFailure {
        self.failure
    }

    /// Returns the rejected text and failure.
    pub fn into_parts(self) -> (String, ToolArgumentsFailure) {
        (self.value, self.failure)
    }
}

/// Zero-based proposal order among tool calls in one model response.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolRequestOrdinal(u32);

impl ToolRequestOrdinal {
    /// Checks that one in-memory index fits the durable ordinal space.
    pub fn try_from_usize(value: usize) -> Option<Self> {
        u32::try_from(value).ok().map(Self)
    }

    /// Reconstitutes one stored zero-based ordinal.
    pub const fn from_u32(value: u32) -> Self {
        Self(value)
    }

    /// Returns the zero-based ordinal.
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// One normalized logical proposal from a completed model response.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolCallProposal {
    name: ToolName,
    arguments: NormalizedToolArguments,
}

impl ToolCallProposal {
    /// Assembles already-checked provider-neutral content.
    pub const fn new(name: ToolName, arguments: NormalizedToolArguments) -> Self {
        Self { name, arguments }
    }

    /// Borrows the checked tool name.
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Borrows normalized arguments.
    pub const fn arguments(&self) -> &NormalizedToolArguments {
        &self.arguments
    }
}

/// One ordered assistant response part admitted by the tool-loop slice.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum AssistantResponsePart {
    /// Exact assistant text.
    Text(AssistantText),
    /// One normalized logical tool proposal.
    ToolCall(ToolCallProposal),
}

/// A completed response proven to contain at least one tool proposal.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolUsingAssistantResponse {
    parts: Box<[AssistantResponsePart]>,
    tool_count: usize,
}

impl ToolUsingAssistantResponse {
    /// Checks the positive bounded tool-count requirement while preserving
    /// part order.
    pub fn try_from_parts(
        parts: Vec<AssistantResponsePart>,
    ) -> Result<Self, ToolUsingAssistantResponseError> {
        let tool_count = parts
            .iter()
            .filter(|part| matches!(part, AssistantResponsePart::ToolCall(_)))
            .count();
        if tool_count == 0 || tool_count > MAX_TOOL_REQUESTS_PER_RESPONSE {
            return Err(ToolUsingAssistantResponseError { parts });
        }
        Ok(Self {
            parts: parts.into_boxed_slice(),
            tool_count,
        })
    }

    /// Returns every response part in provider order.
    pub fn parts(&self) -> &[AssistantResponsePart] {
        &self.parts
    }

    /// Returns the positive number of tool proposals.
    pub const fn tool_count(&self) -> usize {
        self.tool_count
    }
}

/// A response rejected because its tool-proposal count was zero or exceeded
/// the per-response bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolUsingAssistantResponseError {
    parts: Vec<AssistantResponsePart>,
}

impl ToolUsingAssistantResponseError {
    /// Returns the unchanged response parts.
    pub fn into_parts(self) -> Vec<AssistantResponsePart> {
        self.parts
    }
}

/// One immutable content-authoritative logical tool request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolRequest {
    id: ToolRequestId,
    session: SessionId,
    turn: TurnId,
    producing_call: ModelCallId,
    ordinal: ToolRequestOrdinal,
    name: ToolName,
    arguments: NormalizedToolArguments,
}

impl ToolRequest {
    pub(crate) fn from_model_proposal(
        id: ToolRequestId,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        ordinal: ToolRequestOrdinal,
        proposal: ToolCallProposal,
    ) -> Self {
        Self {
            id,
            session,
            turn,
            producing_call,
            ordinal,
            name: proposal.name,
            arguments: proposal.arguments,
        }
    }

    /// Returns the logical request identity.
    pub const fn id(&self) -> ToolRequestId {
        self.id
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the definitive model call that proposed this request.
    pub const fn producing_call(&self) -> ModelCallId {
        self.producing_call
    }

    /// Returns proposal order among tool calls from the producing call.
    pub const fn ordinal(&self) -> ToolRequestOrdinal {
        self.ordinal
    }

    /// Borrows the checked request name.
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Borrows the normalized request arguments.
    pub const fn arguments(&self) -> &NormalizedToolArguments {
        &self.arguments
    }
}

/// Complete independently stored facts for one logical request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRequestReconstitutionInput {
    request: ToolRequest,
}

impl ToolRequestReconstitutionInput {
    /// Supplies all typed stored facts without claiming batch correlation.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        id: ToolRequestId,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        ordinal: ToolRequestOrdinal,
        name: ToolName,
        arguments: NormalizedToolArguments,
    ) -> Self {
        Self {
            request: ToolRequest {
                id,
                session,
                turn,
                producing_call,
                ordinal,
                name,
                arguments,
            },
        }
    }

    /// Returns the inert typed request for complete aggregate validation.
    pub fn into_request(self) -> ToolRequest {
        self.request
    }
}

/// The dangerous blanket-auto posture frozen into one turn.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DangerousToolAutoApproval {
    /// Registry defaults and fail-closed confirmation remain authoritative.
    Disabled,
    /// Every proposal is automatically approved under explicit blanket provenance.
    ApproveAll,
}

/// Registry permission behavior for one declared tool.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolPermissionDefault {
    /// Policy automatically approves the request.
    Auto,
    /// An owner decision is required.
    Confirm,
}

/// Crash-relevant physical effect classification.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolEffectClass {
    /// Crash loss is known not to have caused an external effect.
    EffectFree,
    /// Crash loss may have caused an externally visible effect.
    ExternalEffect,
}

/// Closed additive provenance for one approval decision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolDecisionSource {
    /// An applied owner-global durable command.
    OwnerCommand,
    /// Registry policy selected automatic approval.
    PolicyAuto,
    /// The frozen dangerous session blanket selected automatic approval.
    SessionBlanket,
    /// Reserved for a future exact per-tool session override.
    SessionOverride,
    /// Reserved for a future advisory judge producer.
    JudgeRecommendation,
}

/// One checked optional denial explanation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolDenialReason(String);

impl ToolDenialReason {
    /// Checks length, surrounding POSIX whitespace, and control characters.
    pub fn try_new(value: String) -> Result<Self, ToolDenialReasonError> {
        let failure = if value.is_empty() {
            Some(ToolDenialReasonFailure::Empty)
        } else if value.len() > MAX_TOOL_DENIAL_REASON_BYTES {
            Some(ToolDenialReasonFailure::TooLong { bytes: value.len() })
        } else if has_surrounding_posix_whitespace(&value) {
            Some(ToolDenialReasonFailure::SurroundingWhitespace)
        } else {
            value
                .chars()
                .any(char::is_control)
                .then_some(ToolDenialReasonFailure::ContainsControl)
        };
        match failure {
            Some(failure) => Err(ToolDenialReasonError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows the exact checked reason.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact checked reason.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why a denial reason is unsafe or outside its bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolDenialReasonFailure {
    /// A present reason cannot be empty.
    Empty,
    /// The reason exceeds the admission bound.
    TooLong {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// Leading or trailing POSIX whitespace was present.
    SurroundingWhitespace,
    /// At least one Unicode control scalar was present.
    ContainsControl,
}

/// Failed denial-reason construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDenialReasonError {
    value: String,
    failure: ToolDenialReasonFailure,
}

fn has_surrounding_posix_whitespace(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        || value
            .as_bytes()
            .last()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
}

impl ToolDenialReasonError {
    /// Borrows the rejected value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the validation failure.
    pub const fn failure(&self) -> ToolDenialReasonFailure {
        self.failure
    }

    /// Returns the rejected value and failure.
    pub fn into_parts(self) -> (String, ToolDenialReasonFailure) {
        (self.value, self.failure)
    }
}

/// One durable logical approval decision.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalDecision {
    /// Execution is permitted subject to current aggregate guards.
    Approve,
    /// Execution is permanently prohibited for this request.
    Deny {
        /// Optional bounded owner explanation rendered to the model.
        reason: Option<ToolDenialReason>,
    },
}

/// One request-bound approval resolution with explicit provenance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolApprovalResolution {
    request: ToolRequestId,
    decision: ToolApprovalDecision,
    source: ToolDecisionSource,
}

impl ToolApprovalResolution {
    pub(crate) const fn policy_auto(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Approve,
            source: ToolDecisionSource::PolicyAuto,
        }
    }

    pub(crate) const fn session_blanket(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Approve,
            source: ToolDecisionSource::SessionBlanket,
        }
    }

    const fn owner(request: ToolRequestId, decision: ToolApprovalDecision) -> Self {
        Self {
            request,
            decision,
            source: ToolDecisionSource::OwnerCommand,
        }
    }

    /// Returns the resolved request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    /// Borrows the exact decision.
    pub const fn decision(&self) -> &ToolApprovalDecision {
        &self.decision
    }

    /// Returns the provenance that made the decision.
    pub const fn source(&self) -> ToolDecisionSource {
        self.source
    }

    /// Returns whether this resolution permits an attempt.
    pub const fn is_approved(&self) -> bool {
        matches!(self.decision, ToolApprovalDecision::Approve)
    }
}

/// Independently stored approval evidence supplied for checked
/// reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolApprovalResolutionReconstitutionInput {
    evidence: StoredToolApprovalEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StoredToolApprovalEvidence {
    OwnerCommand(PreparedDecideToolRequest),
    PolicyAuto(ToolRequestId),
    SessionBlanket {
        request: ToolRequestId,
        frozen_posture: DangerousToolAutoApproval,
    },
}

impl ToolApprovalResolutionReconstitutionInput {
    /// Supplies the exact applied owner command that owns one stored decision.
    pub const fn owner_command(command: PreparedDecideToolRequest) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::OwnerCommand(command),
        }
    }

    /// Supplies one request-bound registry-policy approval.
    pub const fn policy_auto(request: ToolRequestId) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::PolicyAuto(request),
        }
    }

    /// Supplies one request-bound session-blanket approval and the exact
    /// dangerous posture frozen for its turn.
    pub const fn session_blanket(
        request: ToolRequestId,
        frozen_posture: DangerousToolAutoApproval,
    ) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::SessionBlanket {
                request,
                frozen_posture,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn owner_fixture(request: ToolRequestId, decision: ToolApprovalDecision) -> Self {
        let command = DecideToolRequest::try_new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(1)),
            request,
            decision.clone(),
        )
        .expect("the fixture command identity is admitted");
        Self::owner_command(PreparedDecideToolRequest {
            command,
            result: DecideToolRequestResult::Applied(DecideToolRequestAppliedResult {
                resolution: ToolApprovalResolution::owner(request, decision),
            }),
        })
    }

    /// Checks source-specific evidence before restoring execution authority.
    pub fn reconstitute(
        self,
    ) -> Result<ToolApprovalResolution, ToolApprovalResolutionReconstitutionError> {
        let resolution = match &self.evidence {
            StoredToolApprovalEvidence::OwnerCommand(command) => match command.result() {
                DecideToolRequestResult::Applied(applied)
                    if command.command().request() == applied.resolution().request()
                        && applied.resolution().source() == ToolDecisionSource::OwnerCommand =>
                {
                    Some(applied.resolution().clone())
                }
                DecideToolRequestResult::Applied(_) | DecideToolRequestResult::Rejected(_) => None,
            },
            StoredToolApprovalEvidence::PolicyAuto(request) => {
                Some(ToolApprovalResolution::policy_auto(*request))
            }
            StoredToolApprovalEvidence::SessionBlanket {
                request,
                frozen_posture: DangerousToolAutoApproval::ApproveAll,
            } => Some(ToolApprovalResolution::session_blanket(*request)),
            StoredToolApprovalEvidence::SessionBlanket {
                frozen_posture: DangerousToolAutoApproval::Disabled,
                ..
            } => None,
        };
        match resolution {
            Some(resolution) => Ok(resolution),
            None => Err(ToolApprovalResolutionReconstitutionError { input: self }),
        }
    }
}

/// Stored approval facts outside the implemented producer vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolApprovalResolutionReconstitutionError {
    input: ToolApprovalResolutionReconstitutionInput,
}

impl ToolApprovalResolutionReconstitutionError {
    /// Borrows the unchanged stored facts.
    pub const fn input(&self) -> &ToolApprovalResolutionReconstitutionInput {
        &self.input
    }

    /// Returns the unchanged stored facts.
    pub fn into_input(self) -> ToolApprovalResolutionReconstitutionInput {
        self.input
    }
}

/// One initial policy outcome for a newly proposed request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InitialToolApproval {
    /// Leave the request undecided and fail closed.
    Confirm,
    /// Record automatic approval from registry policy.
    PolicyAuto,
    /// Record automatic approval from the frozen dangerous blanket.
    SessionBlanket,
}

impl InitialToolApproval {
    pub(crate) const fn resolution(self, request: ToolRequestId) -> Option<ToolApprovalResolution> {
        match self {
            Self::Confirm => None,
            Self::PolicyAuto => Some(ToolApprovalResolution::policy_auto(request)),
            Self::SessionBlanket => Some(ToolApprovalResolution::session_blanket(request)),
        }
    }
}

/// The canonical owner command for one pending tool request.
#[derive(Clone, Debug)]
pub struct DecideToolRequest {
    command_id: DurableCommandId,
    request: ToolRequestId,
    decision: ToolApprovalDecision,
}

impl DecideToolRequest {
    /// Constructs the complete canonical caller payload after rejecting the
    /// owner-global nil and max command sentinels.
    pub fn try_new(
        command_id: DurableCommandId,
        request: ToolRequestId,
        decision: ToolApprovalDecision,
    ) -> Result<Self, DecideToolRequestConstructionError> {
        if command_id.as_uuid().is_nil() || command_id.as_uuid().is_max() {
            return Err(DecideToolRequestConstructionError { command_id });
        }
        Ok(Self {
            command_id,
            request,
            decision,
        })
    }

    #[cfg(test)]
    pub(crate) fn new(
        command_id: DurableCommandId,
        request: ToolRequestId,
        decision: ToolApprovalDecision,
    ) -> Self {
        Self::try_new(command_id, request, decision)
            .expect("the fixture command identity is admitted")
    }

    /// Returns the owner-global command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the exact logical request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    /// Borrows the requested approval decision.
    pub const fn decision(&self) -> &ToolApprovalDecision {
        &self.decision
    }

    /// Prepares owner-sourced resolution against the exact request record.
    pub fn prepare_applied(
        self,
        request: &ToolRequest,
    ) -> Result<PreparedDecideToolRequest, DecideToolRequestPreparationError> {
        if request.id != self.request {
            return Err(DecideToolRequestPreparationError {
                command: self,
                provided_request: request.id,
            });
        }
        let resolution = ToolApprovalResolution::owner(self.request, self.decision.clone());
        Ok(PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Applied(DecideToolRequestAppliedResult { resolution }),
        })
    }

    /// Prepares an authoritative missing-request rejection.
    pub const fn prepare_request_not_found(self) -> PreparedDecideToolRequest {
        let request = self.request;
        PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Rejected(
                DecideToolRequestRejectedResult::RequestNotFound { request },
            ),
        }
    }

    /// Prepares an authoritative already-resolved rejection.
    pub const fn prepare_already_resolved(self) -> PreparedDecideToolRequest {
        let request = self.request;
        PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Rejected(
                DecideToolRequestRejectedResult::AlreadyResolved { request },
            ),
        }
    }

    /// Prepares an authoritative proposal-order rejection.
    pub const fn prepare_not_earliest(self, earliest: ToolRequestId) -> PreparedDecideToolRequest {
        let request = self.request;
        PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Rejected(
                DecideToolRequestRejectedResult::NotEarliestUndecided { request, earliest },
            ),
        }
    }
}

/// A tool-decision command used a reserved owner-global identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecideToolRequestConstructionError {
    command_id: DurableCommandId,
}

impl DecideToolRequestConstructionError {
    /// Returns the rejected command identity.
    pub const fn command_id(self) -> DurableCommandId {
        self.command_id
    }
}

impl PartialEq for DecideToolRequest {
    fn eq(&self, other: &Self) -> bool {
        self.request == other.request && self.decision == other.decision
    }
}

impl Eq for DecideToolRequest {}

impl std::hash::Hash for DecideToolRequest {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.request.hash(state);
        self.decision.hash(state);
    }
}

/// Terminal typed result for one tool-decision command.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum DecideToolRequestResult {
    /// The owner decision was recorded.
    Applied(DecideToolRequestAppliedResult),
    /// Authoritative current state rejected the command.
    Rejected(DecideToolRequestRejectedResult),
}

/// The applied owner decision and its non-forgeable source tag.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DecideToolRequestAppliedResult {
    resolution: ToolApprovalResolution,
}

impl DecideToolRequestAppliedResult {
    /// Borrows the exact owner-sourced resolution.
    pub const fn resolution(&self) -> &ToolApprovalResolution {
        &self.resolution
    }
}

/// Closed authoritative rejection vocabulary for tool decisions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DecideToolRequestRejectedResult {
    /// No logical request had this identity.
    RequestNotFound {
        /// The absent request.
        request: ToolRequestId,
    },
    /// The request already had a terminal approval resolution.
    AlreadyResolved {
        /// The already-resolved request.
        request: ToolRequestId,
    },
    /// An earlier request in the same batch still awaited decision.
    NotEarliestUndecided {
        /// The out-of-order requested subject.
        request: ToolRequestId,
        /// The exact request that must be decided first.
        earliest: ToolRequestId,
    },
}

/// A pre-commit tool-decision candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedDecideToolRequest {
    command: DecideToolRequest,
    result: DecideToolRequestResult,
}

impl PreparedDecideToolRequest {
    /// Borrows the canonical command.
    pub const fn command(&self) -> &DecideToolRequest {
        &self.command
    }

    /// Borrows the terminal typed result.
    pub const fn result(&self) -> &DecideToolRequestResult {
        &self.result
    }

    /// Returns the command and result for one transaction.
    pub fn into_parts(self) -> (DecideToolRequest, DecideToolRequestResult) {
        (self.command, self.result)
    }
}

/// A command/request adapter correlation error, not a recorded rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecideToolRequestPreparationError {
    command: DecideToolRequest,
    provided_request: ToolRequestId,
}

impl DecideToolRequestPreparationError {
    /// Borrows the unchanged command.
    pub const fn command(&self) -> &DecideToolRequest {
        &self.command
    }

    /// Returns the mismatched request record identity.
    pub const fn provided_request(&self) -> ToolRequestId {
        self.provided_request
    }

    /// Returns both unchanged values.
    pub fn into_parts(self) -> (DecideToolRequest, ToolRequestId) {
        (self.command, self.provided_request)
    }
}

/// The implemented result-content algebra for one terminal tool attempt.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolResultContent {
    /// Exact bounded UTF-8 text, including the empty value.
    Text(ToolResultText),
}

/// Exact bounded tool-result text.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolResultText(String);

impl ToolResultText {
    /// Checks the admission bound and rejects U+0000 without rewriting.
    pub fn try_new(value: String) -> Result<Self, ToolResultTextError> {
        let failure = if value.len() > MAX_TOOL_RESULT_TEXT_BYTES {
            Some(ToolResultTextFailure::TooLarge { bytes: value.len() })
        } else if value.contains('\0') {
            Some(ToolResultTextFailure::ContainsNull)
        } else {
            None
        };
        match failure {
            Some(failure) => Err(ToolResultTextError { value, failure }),
            None => Ok(Self(value)),
        }
    }

    /// Borrows exact admitted text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns exact admitted text.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Why tool-result text was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResultTextFailure {
    /// The text exceeded the result bound.
    TooLarge {
        /// The observed UTF-8 byte count.
        bytes: usize,
    },
    /// The text contained U+0000.
    ContainsNull,
}

/// Failed result-text construction retaining the rejected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResultTextError {
    value: String,
    failure: ToolResultTextFailure,
}

impl ToolResultTextError {
    /// Borrows the rejected text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the admission failure.
    pub const fn failure(&self) -> ToolResultTextFailure {
        self.failure
    }

    /// Returns the rejected text and failure.
    pub fn into_parts(self) -> (String, ToolResultTextFailure) {
        (self.value, self.failure)
    }
}

/// One durable logical resolution referenced by semantic history.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolRequestResolution {
    /// Execution evidence lives on the exact attempt.
    Executed {
        /// The terminal physical attempt.
        attempt: ToolAttemptId,
    },
    /// Approval evidence lives on the request-bound decision.
    Denied {
        /// The denied logical request.
        request: ToolRequestId,
    },
    /// The turn ended while the request remained undecided.
    ClosedByTurnEnd {
        /// The closed logical request.
        request: ToolRequestId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{command_id, model_call_id, session_id, tool_request_id, turn_id};

    fn request(id: u128) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            tool_request_id(id),
            session_id(1),
            turn_id(2),
            model_call_id(3),
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("current_time")).expect("canonical tool name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("canonical arguments are valid"),
        )
        .into_request()
    }

    fn tool_response_parts(count: usize) -> Vec<AssistantResponsePart> {
        (0..count)
            .map(|_| {
                AssistantResponsePart::ToolCall(ToolCallProposal::new(
                    ToolName::try_new(String::from("current_time"))
                        .expect("canonical tool name is valid"),
                    NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                        .expect("canonical arguments are valid"),
                ))
            })
            .collect()
    }

    /// S10 / INV-019: request names are exact and restricted to the recorded
    /// ASCII spelling.
    #[test]
    fn s10_inv019_tool_name_rejects_empty_long_and_unsafe_spelling() {
        assert_eq!(
            ToolName::try_new(String::new())
                .expect_err("empty names are invalid")
                .failure(),
            ToolNameFailure::Empty
        );
        assert_eq!(
            ToolName::try_new("x".repeat(65))
                .expect_err("overlong names are invalid")
                .failure(),
            ToolNameFailure::TooLong { bytes: 65 }
        );
        assert_eq!(
            ToolName::try_new(String::from("current/time"))
                .expect_err("slash is outside the spelling")
                .failure(),
            ToolNameFailure::InvalidCharacter {
                byte_index: 7,
                character: '/',
            }
        );
    }

    /// S10 / INV-005 / INV-019: valid JSON is canonicalized recursively,
    /// while malformed provider text remains exact bounded evidence.
    #[test]
    fn s10_inv005_inv019_arguments_are_canonical_or_exactly_undecodable() {
        let json = NormalizedToolArguments::try_from_provider_text(String::from(
            r#"{ "z": [{"b": 2, "a": 1}], "a": true }"#,
        ))
        .expect("bounded JSON is valid");
        let malformed_text = String::from("{\"timezone\":");
        let malformed = NormalizedToolArguments::try_from_provider_text(malformed_text.clone())
            .expect("bounded malformed text remains evidence");

        assert_eq!(json.kind(), ToolArgumentsKind::Json);
        assert_eq!(json.as_str(), r#"{"a":true,"z":[{"a":1,"b":2}]}"#);
        assert_eq!(malformed.kind(), ToolArgumentsKind::Undecodable);
        assert_eq!(malformed.as_str(), malformed_text);
    }

    /// S10 / INV-005: a complete JSON prefix followed by any non-whitespace
    /// provider text remains exact undecodable evidence.
    #[test]
    fn s10_inv005_arguments_reject_trailing_non_whitespace() {
        let provider_text = String::from(r#"{"timezone":"UTC"} trailing"#);
        let normalized = NormalizedToolArguments::try_from_provider_text(provider_text.clone())
            .expect("bounded non-JSON text remains admissible evidence");

        assert_eq!(normalized.kind(), ToolArgumentsKind::Undecodable);
        assert_eq!(normalized.as_str(), provider_text);
    }

    /// S10 / INV-005: reconstitution rejects a competing noncanonical JSON
    /// representation.
    #[test]
    fn s10_inv005_stored_json_must_be_canonical() {
        let error = NormalizedToolArguments::try_from_stored(
            ToolArgumentsKind::Json,
            String::from(r#"{ "b": 2, "a": 1 }"#),
        )
        .expect_err("stored JSON must already be canonical");

        assert_eq!(
            error.failure(),
            ToolArgumentsFailure::StoredJsonNotCanonical
        );
    }

    /// S10 / INV-005: canonicalization preserves JSON numeric values outside
    /// the native integer and floating-point ranges without rounding.
    #[test]
    fn s10_inv005_arguments_preserve_arbitrary_precision_numbers() {
        let normalized = NormalizedToolArguments::try_from_provider_text(String::from(
            r#"{"wide":18446744073709551617,"exponent":1e400}"#,
        ))
        .expect("valid JSON numbers remain decodable");

        assert_eq!(normalized.kind(), ToolArgumentsKind::Json);
        assert_eq!(
            normalized.as_str(),
            r#"{"exponent":1e+400,"wide":18446744073709551617}"#
        );
    }

    /// S10 / INV-005: the byte bound, rather than serde's default recursion
    /// cutoff, governs syntactically valid nested JSON.
    #[test]
    fn s10_inv005_deeply_nested_arguments_remain_json() {
        let depth = 512;
        let value = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
        let normalized = NormalizedToolArguments::try_from_provider_text(value.clone())
            .expect("deep bounded JSON remains admissible");

        assert_eq!(normalized.kind(), ToolArgumentsKind::Json);
        assert_eq!(normalized.as_str(), value);
    }

    /// S10 / INV-005: malformed input is classified before any recursively
    /// owned JSON tree exists, even after a deeply nested complete child.
    #[test]
    fn s10_inv005_deep_partial_json_is_dropped_stack_safely() {
        let depth = 100_000;
        let value = format!("[{}null{},!]", "[".repeat(depth), "]".repeat(depth));
        let normalized = NormalizedToolArguments::try_from_provider_text(value.clone())
            .expect("bounded malformed text remains exact evidence");

        assert_eq!(normalized.kind(), ToolArgumentsKind::Undecodable);
        assert_eq!(normalized.as_str(), value);
    }

    /// S10 / INV-020: a restored session-blanket approval requires the
    /// approve-all posture frozen for that turn.
    #[test]
    fn s10_inv020_session_blanket_reconstitution_requires_frozen_authority() {
        let request = tool_request_id(4);
        let restored = ToolApprovalResolutionReconstitutionInput::session_blanket(
            request,
            DangerousToolAutoApproval::ApproveAll,
        )
        .reconstitute()
        .expect("the exact frozen approve-all posture restores blanket authority");
        let rejected = ToolApprovalResolutionReconstitutionInput::session_blanket(
            request,
            DangerousToolAutoApproval::Disabled,
        )
        .reconstitute()
        .expect_err("a disabled frozen posture cannot restore blanket authority");

        assert_eq!(restored.request(), request);
        assert_eq!(restored.source(), ToolDecisionSource::SessionBlanket);
        assert_eq!(
            rejected.input(),
            &ToolApprovalResolutionReconstitutionInput::session_blanket(
                request,
                DangerousToolAutoApproval::Disabled,
            )
        );
    }

    /// S10 / INV-020: only the owner-command preparation path can construct
    /// owner-sourced approval.
    #[test]
    fn s10_inv020_owner_command_preparation_preserves_agency() {
        let request = request(4);
        let command =
            DecideToolRequest::new(command_id(5), request.id(), ToolApprovalDecision::Approve);
        let prepared = command
            .prepare_applied(&request)
            .expect("the exact pending request is correlated");
        let DecideToolRequestResult::Applied(applied) = prepared.result() else {
            panic!("the exact request should produce an applied candidate");
        };

        assert_eq!(applied.resolution().request(), request.id());
        assert_eq!(
            applied.resolution().source(),
            ToolDecisionSource::OwnerCommand
        );
        assert!(applied.resolution().is_approved());
    }

    /// S10 / INV-019: one provider response admits at most the recorded 32
    /// logical tool requests without accepting a partial prefix.
    #[test]
    fn s10_inv019_tool_response_request_count_is_bounded() {
        let admitted = ToolUsingAssistantResponse::try_from_parts(tool_response_parts(32))
            .expect("the exact per-response limit is admitted");
        let rejected = ToolUsingAssistantResponse::try_from_parts(tool_response_parts(33))
            .expect_err("the first response above the limit is rejected whole");

        assert_eq!(admitted.tool_count(), 32);
        assert_eq!(rejected.into_parts().len(), 33);
    }

    /// INV-012: owner-global command sentinels never enter the canonical
    /// tool-decision command space.
    #[test]
    fn inv012_tool_decision_rejects_reserved_command_identities() {
        for value in [uuid::Uuid::nil(), uuid::Uuid::max()] {
            let command_id = DurableCommandId::from_uuid(value);
            let error = DecideToolRequest::try_new(
                command_id,
                tool_request_id(1),
                ToolApprovalDecision::Approve,
            )
            .expect_err("reserved command identities are rejected");

            assert_eq!(error.command_id(), command_id);
        }
    }

    /// S10 / INV-020: only an applied owner command can restore
    /// owner-command approval authority.
    #[test]
    fn s10_inv020_rejected_owner_command_cannot_restore_approval() {
        let command = DecideToolRequest::new(
            command_id(5),
            tool_request_id(4),
            ToolApprovalDecision::Approve,
        )
        .prepare_request_not_found();
        let input = ToolApprovalResolutionReconstitutionInput::owner_command(command);

        assert!(
            input
                .clone()
                .reconstitute()
                .expect_err("a rejected command carries no approval authority")
                .input()
                == &input
        );
    }

    /// S10: denial admission follows the persisted POSIX-whitespace contract
    /// without silently broadening it to every Unicode space scalar.
    #[test]
    fn s10_denial_reason_rejects_posix_edges_and_preserves_nonbreaking_space() {
        for value in [" denied", "denied\n", "\tdenied", "denied\u{000c}"] {
            assert_eq!(
                ToolDenialReason::try_new(String::from(value))
                    .expect_err("POSIX edge whitespace is rejected")
                    .failure(),
                ToolDenialReasonFailure::SurroundingWhitespace
            );
        }

        let admitted = ToolDenialReason::try_new(String::from("\u{00a0}denied\u{00a0}"))
            .expect("nonbreaking space is not POSIX whitespace");
        assert_eq!(admitted.as_str(), "\u{00a0}denied\u{00a0}");
    }

    /// INV-012: durable-command comparison equality excludes only command
    /// identity and retains the exact decision payload.
    #[test]
    fn inv012_decision_command_equality_excludes_only_command_identity() {
        let request = tool_request_id(1);
        let approve = DecideToolRequest::new(command_id(2), request, ToolApprovalDecision::Approve);
        let replay = DecideToolRequest::new(command_id(3), request, ToolApprovalDecision::Approve);
        let deny = DecideToolRequest::new(
            command_id(2),
            request,
            ToolApprovalDecision::Deny { reason: None },
        );

        assert_eq!(approve, replay);
        assert_ne!(approve, deny);
    }

    /// S11 / INV-027: denials remain request-bound logical resolutions and
    /// cannot name a physical attempt.
    #[test]
    fn s11_inv027_denial_resolution_names_only_the_request() {
        let request = tool_request_id(9);

        assert_eq!(
            ToolRequestResolution::Denied { request },
            ToolRequestResolution::Denied { request }
        );
        assert_ne!(
            ToolRequestResolution::Denied { request },
            ToolRequestResolution::ClosedByTurnEnd { request }
        );
    }
}

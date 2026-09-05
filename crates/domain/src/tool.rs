//! Logical tool requests, approval provenance, and result values.
//!
//! `docs/spec/tool-loop.md` is normative. This module owns bounded,
//! provider-neutral request content and the approval algebra. Physical
//! execution lives in `tool_attempt`; persistence, registry lookup, and
//! executor selection remain outside the domain boundary.

use serde::{
    Deserialize, Serialize, Serializer, de::IgnoredAny, ser::SerializeMap, ser::SerializeSeq,
};

use crate::{
    AssistantText, DurableCommandId, ModelCallId, SessionId, ToolAttemptId, ToolRequestId, TurnId,
};

const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_TOOL_RESULT_TEXT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TOOL_REQUESTS_PER_RESPONSE: usize = 32;
const SUPPRESSED_TOOL_ARGUMENTS: &str = r#"{"redacted":"[redacted]"}"#;
const SUPPRESSED_TOOL_DENIAL_REASON: &str =
    "Tool arguments were suppressed by the credential boundary";

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
        if value.contains('\0') {
            return Err(ToolArgumentsError {
                failure: ToolArgumentsFailure::ContainsNull,
                value,
            });
        }

        // The byte cap does not bound JSON depth. Disabling serde_json's
        // recursion limit is safe here only because serde_stacker grows the
        // parse and serialization stacks and iterative destruction below keeps
        // a deeply nested Value from overflowing the thread stack during Drop.
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
        let serialization = LexicallyOrderedJson(&decoded).serialize(serializer);
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

/// A stacker-compatible view that restores the lexical object ordering which
/// canonical tool JSON promises independently of serde_json's map backend.
struct LexicallyOrderedJson<'a>(&'a serde_json::Value);

impl Serialize for LexicallyOrderedJson<'_> {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        match self.0 {
            serde_json::Value::Null => serializer.serialize_unit(),
            serde_json::Value::Bool(value) => serializer.serialize_bool(*value),
            serde_json::Value::Number(value) => value.serialize(serializer),
            serde_json::Value::String(value) => serializer.serialize_str(value),
            serde_json::Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&Self(value))?;
                }
                sequence.end()
            }
            serde_json::Value::Object(object) => {
                let mut entries = object.iter().collect::<Vec<_>>();
                entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, &Self(value))?;
                }
                map.end()
            }
        }
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
    /// Provider text contained U+0000, which cannot enter durable text.
    ContainsNull,
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
    suppressed: bool,
}

impl ToolCallProposal {
    /// Assembles already-checked provider-neutral content.
    pub const fn new(name: ToolName, arguments: NormalizedToolArguments) -> Self {
        Self {
            name,
            arguments,
            suppressed: false,
        }
    }

    /// Constructs the inert projection of a proposal whose arguments were
    /// suppressed by a provider credential boundary.
    pub fn suppressed(name: ToolName) -> Self {
        Self {
            name,
            arguments: NormalizedToolArguments {
                kind: ToolArgumentsKind::Json,
                value: String::from(SUPPRESSED_TOOL_ARGUMENTS),
            },
            suppressed: true,
        }
    }

    /// Borrows the checked tool name.
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Borrows normalized arguments.
    pub const fn arguments(&self) -> &NormalizedToolArguments {
        &self.arguments
    }

    /// Reports whether the proposal is an inert credential-boundary projection.
    pub const fn is_suppressed(&self) -> bool {
        self.suppressed
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
    approval_posture: ToolApprovalPosture,
}

impl ToolRequest {
    pub(crate) fn from_model_proposal(
        id: ToolRequestId,
        session: SessionId,
        turn: TurnId,
        producing_call: ModelCallId,
        ordinal: ToolRequestOrdinal,
        proposal: ToolCallProposal,
        approval: InitialToolApproval,
    ) -> Self {
        Self {
            id,
            session,
            turn,
            producing_call,
            ordinal,
            name: proposal.name,
            arguments: proposal.arguments,
            approval_posture: approval.posture(),
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

    /// Returns the exact per-request posture frozen when the proposal landed.
    pub const fn approval_posture(&self) -> ToolApprovalPosture {
        self.approval_posture
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
                approval_posture: ToolApprovalPosture::Human,
            },
        }
    }

    /// Supplies the exact stored posture selected when this request landed.
    pub const fn with_approval_posture(mut self, posture: ToolApprovalPosture) -> Self {
        self.request.approval_posture = posture;
        self
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
    /// A user decision is required.
    Confirm,
    /// A user decision is required even under blanket automatic approval.
    AlwaysConfirm,
}

/// Deployment-selected approval authority for one exact tool.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToolApprovalPosture {
    /// Policy may approve without a decision event.
    Auto,
    /// A delegate model may decide or escalate to the user.
    Delegated,
    /// Only the user may approve or deny.
    Human,
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
    /// An applied user-global durable command.
    UserCommand,
    /// Registry policy selected automatic approval.
    PolicyAuto,
    /// The frozen dangerous session blanket selected automatic approval.
    SessionBlanket,
    /// Reserved for a future exact per-tool session override.
    SessionOverride,
    /// A checked delegate-model decision.
    Delegate,
    /// The provider credential boundary suppressed executable arguments.
    RuntimeSafety,
    /// A committed session closure denied a parked request before interrupting
    /// the live turn.
    LifecycleClosure,
    /// A user-recorded one-shot override of a delegate denial supplied approval
    /// when the session re-proposed the denied command.
    UserOverride,
}

impl ToolDecisionSource {
    pub(crate) const fn requires_ordered_prefix(self) -> bool {
        match self {
            Self::UserCommand | Self::Delegate => true,
            Self::PolicyAuto
            | Self::SessionBlanket
            | Self::SessionOverride
            | Self::RuntimeSafety
            | Self::LifecycleClosure
            | Self::UserOverride => false,
        }
    }
}

/// Who made one explicit approval decision.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalDecider {
    /// The user acted through the named durable command.
    User {
        /// Exact command provenance.
        command: DurableCommandId,
    },
    /// The named configured model acted through the recorded call.
    Delegate {
        /// Exact direct model selection used by the judge.
        model: crate::DirectModelSelection,
        /// Dedicated recorded judge call.
        call: ModelCallId,
    },
    /// The user pre-approved the re-proposed command by overriding one exact
    /// delegate denial through the named durable command.
    UserOverride {
        /// Exact override-command provenance.
        command: DurableCommandId,
        /// The delegate-denied request whose recorded override was consumed.
        denied_request: ToolRequestId,
    },
}

/// One checked delegate rationale retained verbatim.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolDecisionRationale(String);

impl ToolDecisionRationale {
    /// Maximum admitted UTF-8 byte length.
    pub const MAX_UTF8_BYTES: usize = 4096;

    /// Admits nonempty bounded text without U+0000.
    pub fn try_new(value: String) -> Result<Self, ToolDecisionRationaleError> {
        if value.is_empty() || value.len() > Self::MAX_UTF8_BYTES || value.contains('\0') {
            Err(ToolDecisionRationaleError { value })
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows the exact rationale.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the exact rationale.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// A delegate rationale was empty, oversized, or contained U+0000.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDecisionRationaleError {
    value: String,
}

impl ToolDecisionRationaleError {
    /// Borrows the rejected value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the rejected value.
    pub fn into_value(self) -> String {
        self.value
    }
}

impl std::fmt::Display for ToolDecisionRationaleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tool decision rationale must be nonempty, at most {} bytes, and contain no U+0000",
            ToolDecisionRationale::MAX_UTF8_BYTES
        )
    }
}

impl std::error::Error for ToolDecisionRationaleError {}

/// Closed result vocabulary emitted by an approval judge.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DelegateApprovalRecommendation {
    /// Permit this exact request.
    Approve,
    /// Permanently deny this exact request.
    Deny,
    /// Leave the request parked for the user.
    EscalateToHuman,
}

/// One authority-checked delegate result with complete model provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegateToolApproval {
    request: ToolRequestId,
    posture: ToolApprovalPosture,
    model: crate::DirectModelSelection,
    call: ModelCallId,
    recommendation: DelegateApprovalRecommendation,
    rationale: ToolDecisionRationale,
}

impl DelegateToolApproval {
    /// Checks the recommendation against the request's frozen posture.
    pub fn try_new(
        request: &ToolRequest,
        model: crate::DirectModelSelection,
        call: ModelCallId,
        recommendation: DelegateApprovalRecommendation,
        rationale: ToolDecisionRationale,
    ) -> Result<Self, DelegateToolApprovalError> {
        let permitted = match request.approval_posture() {
            ToolApprovalPosture::Delegated => true,
            ToolApprovalPosture::Human => {
                recommendation == DelegateApprovalRecommendation::EscalateToHuman
            }
            ToolApprovalPosture::Auto => false,
        };
        if !permitted {
            return Err(DelegateToolApprovalError {
                posture: request.approval_posture(),
                recommendation,
            });
        }
        Ok(Self {
            request: request.id(),
            posture: request.approval_posture(),
            model,
            call,
            recommendation,
            rationale,
        })
    }

    /// Returns the exact request judged.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }

    pub(crate) const fn posture(&self) -> ToolApprovalPosture {
        self.posture
    }

    /// Returns the direct model selection used by the judge.
    pub const fn model(&self) -> crate::DirectModelSelection {
        self.model
    }

    /// Returns the dedicated judge call.
    pub const fn call(&self) -> ModelCallId {
        self.call
    }

    /// Returns the checked recommendation.
    pub const fn recommendation(&self) -> DelegateApprovalRecommendation {
        self.recommendation
    }

    /// Borrows the exact judge rationale.
    pub const fn rationale(&self) -> &ToolDecisionRationale {
        &self.rationale
    }
}

/// A delegate recommendation exceeded the request's frozen authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegateToolApprovalError {
    posture: ToolApprovalPosture,
    recommendation: DelegateApprovalRecommendation,
}

impl DelegateToolApprovalError {
    /// Returns the frozen posture that rejected the recommendation.
    pub const fn posture(self) -> ToolApprovalPosture {
        self.posture
    }

    /// Returns the rejected recommendation.
    pub const fn recommendation(self) -> DelegateApprovalRecommendation {
        self.recommendation
    }
}

impl std::fmt::Display for DelegateToolApprovalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "delegate recommendation {:?} exceeds {:?} approval-posture authority",
            self.recommendation, self.posture
        )
    }
}

impl std::error::Error for DelegateToolApprovalError {}

/// One checked optional denial explanation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolDenialReason(String);

impl ToolDenialReason {
    /// Maximum admitted UTF-8 byte length.
    pub const MAX_UTF8_BYTES: usize = 1024;

    /// Checks length, surrounding POSIX whitespace, and control characters.
    pub fn try_new(value: String) -> Result<Self, ToolDenialReasonError> {
        let failure = if value.is_empty() {
            Some(ToolDenialReasonFailure::Empty)
        } else if value.len() > Self::MAX_UTF8_BYTES {
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

    /// Derives the deterministic denial reason carried by a delegate denial.
    ///
    /// A rationale admits control characters and up to
    /// [`ToolDecisionRationale::MAX_UTF8_BYTES`] bytes, so this conversion is
    /// lossy exactly where the two bounds disagree: control characters become
    /// spaces, edge characters the reason validator forbids are trimmed, and
    /// the text is cut to [`Self::MAX_UTF8_BYTES`] on a character boundary.
    /// After control mapping the only forbidden edge character left is the
    /// space itself, so admissible non-POSIX edge whitespace such as NBSP is
    /// preserved verbatim. A rationale that is entirely control characters
    /// and spaces derives no reason.
    pub fn from_rationale(rationale: &ToolDecisionRationale) -> Option<Self> {
        let sanitized = rationale
            .as_str()
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        let mut trimmed = sanitized.trim_matches(' ');
        while trimmed.len() > Self::MAX_UTF8_BYTES {
            let mut cut = Self::MAX_UTF8_BYTES;
            while !trimmed.is_char_boundary(cut) {
                cut -= 1;
            }
            trimmed = trimmed[..cut].trim_end_matches(' ');
        }
        (!trimmed.is_empty()).then(|| Self(String::from(trimmed)))
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
        /// Optional bounded denial explanation rendered to the model; its
        /// author — user or judge — follows from the decision source.
        reason: Option<ToolDenialReason>,
    },
}

/// One request-bound approval resolution with explicit provenance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolApprovalResolution {
    request: ToolRequestId,
    decision: ToolApprovalDecision,
    source: ToolDecisionSource,
    decider: Option<ToolApprovalDecider>,
    rationale: Option<ToolDecisionRationale>,
}

impl ToolApprovalResolution {
    pub(crate) const fn policy_auto(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Approve,
            source: ToolDecisionSource::PolicyAuto,
            decider: None,
            rationale: None,
        }
    }

    pub(crate) const fn session_blanket(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Approve,
            source: ToolDecisionSource::SessionBlanket,
            decider: None,
            rationale: None,
        }
    }

    pub(crate) fn runtime_safety(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Deny {
                reason: Some(ToolDenialReason(String::from(
                    SUPPRESSED_TOOL_DENIAL_REASON,
                ))),
            },
            source: ToolDecisionSource::RuntimeSafety,
            decider: None,
            rationale: None,
        }
    }

    pub(crate) const fn lifecycle_closure(request: ToolRequestId) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Deny { reason: None },
            source: ToolDecisionSource::LifecycleClosure,
            decider: None,
            rationale: None,
        }
    }

    fn user(
        command: DurableCommandId,
        request: ToolRequestId,
        decision: ToolApprovalDecision,
    ) -> Self {
        Self {
            request,
            decision,
            source: ToolDecisionSource::UserCommand,
            decider: Some(ToolApprovalDecider::User { command }),
            rationale: None,
        }
    }

    pub(crate) const fn user_override(
        request: ToolRequestId,
        command: DurableCommandId,
        denied_request: ToolRequestId,
    ) -> Self {
        Self {
            request,
            decision: ToolApprovalDecision::Approve,
            source: ToolDecisionSource::UserOverride,
            decider: Some(ToolApprovalDecider::UserOverride {
                command,
                denied_request,
            }),
            rationale: None,
        }
    }

    pub(crate) fn delegate(approval: &DelegateToolApproval) -> Option<Self> {
        let decision = match approval.recommendation {
            DelegateApprovalRecommendation::Approve => ToolApprovalDecision::Approve,
            DelegateApprovalRecommendation::Deny => ToolApprovalDecision::Deny {
                reason: ToolDenialReason::from_rationale(&approval.rationale),
            },
            DelegateApprovalRecommendation::EscalateToHuman => return None,
        };
        Some(Self {
            request: approval.request,
            decision,
            source: ToolDecisionSource::Delegate,
            decider: Some(ToolApprovalDecider::Delegate {
                model: approval.model,
                call: approval.call,
            }),
            rationale: Some(approval.rationale.clone()),
        })
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

    /// Returns the explicit decider, absent only for automatic policy.
    pub const fn decider(&self) -> Option<&ToolApprovalDecider> {
        self.decider.as_ref()
    }

    /// Returns the delegate rationale, when a delegate decided.
    pub const fn rationale(&self) -> Option<&ToolDecisionRationale> {
        self.rationale.as_ref()
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
    UserCommand(PreparedDecideToolRequest),
    Delegate {
        approval: Box<DelegateToolApproval>,
        stored_denial_reason: Option<ToolDenialReason>,
    },
    PolicyAuto(ToolRequestId),
    SessionBlanket {
        request: ToolRequestId,
        frozen_posture: DangerousToolAutoApproval,
    },
    RuntimeSafety(ToolRequestId),
    LifecycleClosure(ToolRequestId),
    UserOverride {
        request: ToolRequestId,
        command: DurableCommandId,
        denied_request: ToolRequestId,
        frozen_posture: ToolApprovalPosture,
    },
}

impl ToolApprovalResolutionReconstitutionInput {
    /// Supplies the exact applied user command that owns one stored decision.
    pub const fn user_command(command: PreparedDecideToolRequest) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::UserCommand(command),
        }
    }

    /// Supplies one authority-checked delegate decision, its recorded call,
    /// and the denial reason exactly as stored beside the decision.
    ///
    /// Reconstitution requires the stored reason to equal the derivation
    /// from the recorded rationale — null exactly when the rationale
    /// derives nothing — so a row missing its current evidence fails closed
    /// instead of restoring as an unexplained denial.
    pub fn delegate(
        approval: DelegateToolApproval,
        stored_denial_reason: Option<ToolDenialReason>,
    ) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::Delegate {
                approval: Box::new(approval),
                stored_denial_reason,
            },
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

    /// Supplies one credential-boundary safety denial.
    pub const fn runtime_safety(request: ToolRequestId) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::RuntimeSafety(request),
        }
    }

    /// Supplies one denial caused by a committed session closure.
    pub const fn lifecycle_closure(request: ToolRequestId) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::LifecycleClosure(request),
        }
    }

    /// Supplies one request-bound consumed user override, its recorded
    /// command and overridden request, and the exact approval posture frozen
    /// on the approved request.
    pub const fn user_override(
        request: ToolRequestId,
        command: DurableCommandId,
        denied_request: ToolRequestId,
        frozen_posture: ToolApprovalPosture,
    ) -> Self {
        Self {
            evidence: StoredToolApprovalEvidence::UserOverride {
                request,
                command,
                denied_request,
                frozen_posture,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn user_fixture(request: ToolRequestId, decision: ToolApprovalDecision) -> Self {
        const USER_COMMAND_SEED: u128 = 1;

        let command_id = DurableCommandId::from_uuid(uuid::Uuid::from_u128(USER_COMMAND_SEED));
        let command = DecideToolRequest::try_new(command_id, request, decision.clone())
            .expect("the fixture command identity is admitted");
        Self::user_command(PreparedDecideToolRequest {
            command,
            result: DecideToolRequestResult::Applied(DecideToolRequestAppliedResult {
                resolution: ToolApprovalResolution::user(command_id, request, decision),
            }),
        })
    }

    /// Checks source-specific evidence before restoring execution authority.
    pub fn reconstitute(
        self,
    ) -> Result<ToolApprovalResolution, ToolApprovalResolutionReconstitutionError> {
        let resolution = match &self.evidence {
            StoredToolApprovalEvidence::UserCommand(command) => match command.result() {
                DecideToolRequestResult::Applied(applied)
                    if command.command().request() == applied.resolution().request()
                        && applied.resolution().source() == ToolDecisionSource::UserCommand =>
                {
                    Some(applied.resolution().clone())
                }
                DecideToolRequestResult::Applied(_) | DecideToolRequestResult::Rejected(_) => None,
            },
            StoredToolApprovalEvidence::Delegate {
                approval,
                stored_denial_reason,
            } => ToolApprovalResolution::delegate(approval).and_then(|resolution| {
                // The stored reason must equal the derivation exactly — a
                // null admitted only when the rationale derives nothing — so
                // a row missing its current evidence is corruption, never an
                // unexplained denial.
                match &resolution.decision {
                    ToolApprovalDecision::Approve => {
                        stored_denial_reason.is_none().then_some(resolution)
                    }
                    ToolApprovalDecision::Deny { reason } => {
                        (reason == stored_denial_reason).then_some(resolution)
                    }
                }
            }),
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
            StoredToolApprovalEvidence::RuntimeSafety(request) => {
                Some(ToolApprovalResolution::runtime_safety(*request))
            }
            StoredToolApprovalEvidence::LifecycleClosure(request) => {
                Some(ToolApprovalResolution::lifecycle_closure(*request))
            }
            StoredToolApprovalEvidence::UserOverride {
                request,
                command,
                denied_request,
                frozen_posture: ToolApprovalPosture::Delegated,
            } => Some(ToolApprovalResolution::user_override(
                *request,
                *command,
                *denied_request,
            )),
            StoredToolApprovalEvidence::UserOverride {
                frozen_posture: ToolApprovalPosture::Auto | ToolApprovalPosture::Human,
                ..
            } => None,
        };
        match resolution {
            Some(resolution) => Ok(resolution),
            None => Err(ToolApprovalResolutionReconstitutionError {
                input: Box::new(self),
            }),
        }
    }
}

/// Stored approval facts outside the implemented producer vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolApprovalResolutionReconstitutionError {
    input: Box<ToolApprovalResolutionReconstitutionInput>,
}

impl ToolApprovalResolutionReconstitutionError {
    /// Borrows the unchanged stored facts.
    pub const fn input(&self) -> &ToolApprovalResolutionReconstitutionInput {
        &self.input
    }

    /// Returns the unchanged stored facts.
    pub fn into_input(self) -> ToolApprovalResolutionReconstitutionInput {
        *self.input
    }
}

impl std::fmt::Display for ToolApprovalResolutionReconstitutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stored tool approval evidence cannot be reconstituted")
    }
}

impl std::error::Error for ToolApprovalResolutionReconstitutionError {}

/// One initial policy outcome for a newly proposed request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InitialToolApproval {
    /// Leave the request undecided and fail closed.
    Confirm,
    /// Leave an `AlwaysConfirm` request undecided despite blanket posture.
    AlwaysConfirm,
    /// Leave the request parked for an explicitly human-only decision.
    Human,
    /// Leave the request parked for a delegate judge.
    Delegated,
    /// Record automatic approval from registry policy.
    PolicyAuto,
    /// Record automatic approval from the frozen dangerous blanket.
    SessionBlanket,
    /// Record an automatic denial for credential-suppressed arguments.
    RuntimeSafetyDeny,
    /// Record approval consumed from a user-recorded override of one exact
    /// delegate denial instead of parking for the judge again.
    UserOverride {
        /// The applied durable override command.
        command: DurableCommandId,
        /// The delegate-denied request whose recorded override this proposal
        /// consumes.
        denied_request: ToolRequestId,
    },
}

impl InitialToolApproval {
    pub(crate) fn resolution(self, request: ToolRequestId) -> Option<ToolApprovalResolution> {
        match self {
            Self::Confirm | Self::AlwaysConfirm | Self::Human | Self::Delegated => None,
            Self::PolicyAuto => Some(ToolApprovalResolution::policy_auto(request)),
            Self::SessionBlanket => Some(ToolApprovalResolution::session_blanket(request)),
            Self::RuntimeSafetyDeny => Some(ToolApprovalResolution::runtime_safety(request)),
            Self::UserOverride {
                command,
                denied_request,
            } => Some(ToolApprovalResolution::user_override(
                request,
                command,
                denied_request,
            )),
        }
    }

    pub(crate) const fn posture(self) -> ToolApprovalPosture {
        match self {
            Self::Confirm | Self::AlwaysConfirm | Self::Human => ToolApprovalPosture::Human,
            Self::Delegated | Self::UserOverride { .. } => ToolApprovalPosture::Delegated,
            Self::PolicyAuto | Self::SessionBlanket | Self::RuntimeSafetyDeny => {
                ToolApprovalPosture::Auto
            }
        }
    }

    /// Returns whether this outcome leaves an explicit decision outstanding.
    pub const fn requires_decision(self) -> bool {
        match self {
            Self::Confirm | Self::AlwaysConfirm | Self::Human | Self::Delegated => true,
            Self::PolicyAuto
            | Self::SessionBlanket
            | Self::RuntimeSafetyDeny
            | Self::UserOverride { .. } => false,
        }
    }
}

/// The canonical user command for one pending tool request.
#[derive(Clone, Debug)]
pub struct DecideToolRequest {
    command_id: DurableCommandId,
    request: ToolRequestId,
    decision: ToolApprovalDecision,
}

impl DecideToolRequest {
    /// Constructs the complete canonical caller payload after rejecting the
    /// user-global nil and max command sentinels.
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

    /// Returns the user-global command identity.
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

    /// Prepares user-sourced resolution against the exact request record.
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
        let resolution =
            ToolApprovalResolution::user(self.command_id, self.request, self.decision.clone());
        Ok(PreparedDecideToolRequest {
            command: self,
            result: DecideToolRequestResult::Applied(DecideToolRequestAppliedResult { resolution }),
        })
    }

    /// Prepares a closure-sourced denial against the exact request record.
    pub fn prepare_lifecycle_closure_applied(
        self,
        request: &ToolRequest,
    ) -> Result<PreparedDecideToolRequest, DecideToolRequestPreparationError> {
        if request.id != self.request
            || self.decision != (ToolApprovalDecision::Deny { reason: None })
        {
            return Err(DecideToolRequestPreparationError {
                command: self,
                provided_request: request.id,
            });
        }
        let resolution = ToolApprovalResolution::lifecycle_closure(self.request);
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

/// A tool-decision command used a reserved user-global identity.
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
    /// The user decision was recorded.
    Applied(DecideToolRequestAppliedResult),
    /// Authoritative current state rejected the command.
    Rejected(DecideToolRequestRejectedResult),
}

/// The applied user decision and its non-forgeable source tag.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DecideToolRequestAppliedResult {
    resolution: ToolApprovalResolution,
}

impl DecideToolRequestAppliedResult {
    /// Borrows the exact user-sourced resolution.
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

/// One recorded, not-yet-consumed user override of a delegate denial.
///
/// The override pre-approves exactly one future proposal in the owning
/// session: the first one whose tool name and normalized arguments equal the
/// denied request's. It links the denied request, the judge call that denied
/// it, and the user command that recorded the override, so the full audit chain
/// stays queryable from any of the three.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RecordedUserOverride {
    command: DurableCommandId,
    session: SessionId,
    denied_request: ToolRequestId,
    judge_call: ModelCallId,
    tool: ToolName,
    arguments: NormalizedToolArguments,
}

impl RecordedUserOverride {
    /// Supplies all typed stored facts of one recorded override.
    pub const fn new(
        command: DurableCommandId,
        session: SessionId,
        denied_request: ToolRequestId,
        judge_call: ModelCallId,
        tool: ToolName,
        arguments: NormalizedToolArguments,
    ) -> Self {
        Self {
            command,
            session,
            denied_request,
            judge_call,
            tool,
            arguments,
        }
    }

    /// Returns the applied durable override command.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }

    /// Returns the session whose future proposal may consume this override.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the delegate-denied request the override names.
    pub const fn denied_request(&self) -> ToolRequestId {
        self.denied_request
    }

    /// Returns the completed judge call that denied the request.
    pub const fn judge_call(&self) -> ModelCallId {
        self.judge_call
    }

    /// Borrows the denied request's checked tool name.
    pub const fn tool(&self) -> &ToolName {
        &self.tool
    }

    /// Borrows the denied request's normalized arguments.
    pub const fn arguments(&self) -> &NormalizedToolArguments {
        &self.arguments
    }

    /// Returns whether the proposal re-proposes the exact denied command:
    /// equal tool name and equal normalized arguments.
    pub fn matches_proposal(&self, proposal: &ToolCallProposal) -> bool {
        self.tool == *proposal.name() && self.arguments == *proposal.arguments()
    }
}

/// The canonical user command overriding one delegate-denied tool request.
///
/// Applying the command records one one-shot pre-approval in the named session:
/// the next proposal of the exact denied command is approved under
/// user-override provenance instead of parking for the judge again. Unlike
/// [`DecideToolRequest`], the session is part of the canonical payload,
/// because the recorded override is a session-scoped standing fact consumed by a
/// later proposal rather than a decision on an already-parked request.
#[derive(Clone, Debug)]
pub struct OverrideDeniedToolRequest {
    command_id: DurableCommandId,
    session: SessionId,
    denied_request: ToolRequestId,
}

impl OverrideDeniedToolRequest {
    /// Constructs the complete canonical caller payload after rejecting the
    /// user-global nil and max command sentinels.
    pub fn try_new(
        command_id: DurableCommandId,
        session: SessionId,
        denied_request: ToolRequestId,
    ) -> Result<Self, OverrideDeniedToolRequestConstructionError> {
        if command_id.as_uuid().is_nil() || command_id.as_uuid().is_max() {
            return Err(OverrideDeniedToolRequestConstructionError { command_id });
        }
        Ok(Self {
            command_id,
            session,
            denied_request,
        })
    }

    /// Returns the user-global command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the session the override covers.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact delegate-denied request named by the command.
    pub const fn denied_request(&self) -> ToolRequestId {
        self.denied_request
    }

    /// Verifies the named request admits a user override and prepares the
    /// terminal typed result.
    ///
    /// This is the override verification predicate. Recording requires every
    /// conjunct, each with its own typed rejection:
    ///
    /// - the recorded approval is a delegate denial, so a user denial, any
    ///   approval, or an undecided request cannot be overridden;
    /// - the denial is terminal — its denied-result entry is materialized —
    ///   so a denial whose round is still resolving cannot be overridden;
    /// - the request belongs to the command's session, so an override can
    ///   never pre-approve a proposal in another session; and
    /// - no override is already recorded for the request, so each denial admits
    ///   at most one override ever.
    pub fn prepare(
        self,
        request: &ToolRequest,
        approval: Option<&ToolApprovalResolution>,
        terminal_resolution: Option<ToolRequestResolution>,
        existing_override_command: Option<DurableCommandId>,
    ) -> Result<PreparedOverrideDeniedToolRequest, OverrideDeniedToolRequestPreparationError> {
        if request.id() != self.denied_request {
            return Err(OverrideDeniedToolRequestPreparationError {
                command: self,
                provided_request: request.id(),
            });
        }
        if let Some(approval) = approval
            && approval.request() != self.denied_request
        {
            let provided_request = approval.request();
            return Err(OverrideDeniedToolRequestPreparationError {
                command: self,
                provided_request,
            });
        }
        if request.session() != self.session {
            return Ok(self.prepare_request_not_in_session());
        }
        // Delegate-denied: the decision is a denial and its decider is the
        // judge. The sealed resolution producers make a delegate decider
        // equivalent to the delegate source, so the decider is the checked
        // fact and also supplies the judge call the recorded override links.
        let judge_call = match approval {
            Some(approval) if matches!(approval.decision(), ToolApprovalDecision::Deny { .. }) => {
                match approval.decider() {
                    Some(ToolApprovalDecider::Delegate { call, .. }) => Some(*call),
                    Some(
                        ToolApprovalDecider::User { .. } | ToolApprovalDecider::UserOverride { .. },
                    )
                    | None => None,
                }
            }
            Some(_) | None => None,
        };
        let Some(judge_call) = judge_call else {
            return Ok(self.prepare_not_delegate_denied());
        };
        let terminally_denied = matches!(
            terminal_resolution,
            Some(ToolRequestResolution::Denied { request }) if request == self.denied_request
        );
        if !terminally_denied {
            return Ok(self.prepare_not_terminally_denied());
        }
        if existing_override_command.is_some() {
            return Ok(self.prepare_already_overridden());
        }
        let recorded = RecordedUserOverride::new(
            self.command_id,
            self.session,
            self.denied_request,
            judge_call,
            request.name().clone(),
            request.arguments().clone(),
        );
        Ok(PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Applied(
                OverrideDeniedToolRequestAppliedResult { recorded },
            ),
        })
    }

    /// Restores the exact recorded applied receipt from its durable recorded
    /// row, rejecting a row that does not correlate with this command.
    pub fn reconstitute_applied(
        self,
        recorded: RecordedUserOverride,
    ) -> Result<PreparedOverrideDeniedToolRequest, OverrideDeniedToolRequestPreparationError> {
        if recorded.command() != self.command_id
            || recorded.session() != self.session
            || recorded.denied_request() != self.denied_request
        {
            let provided_request = recorded.denied_request();
            return Err(OverrideDeniedToolRequestPreparationError {
                command: self,
                provided_request,
            });
        }
        Ok(PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Applied(
                OverrideDeniedToolRequestAppliedResult { recorded },
            ),
        })
    }

    /// Prepares an authoritative missing-request rejection.
    pub const fn prepare_request_not_found(self) -> PreparedOverrideDeniedToolRequest {
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::RequestNotFound { denied_request },
            ),
        }
    }

    /// Prepares an authoritative other-session rejection.
    pub const fn prepare_request_not_in_session(self) -> PreparedOverrideDeniedToolRequest {
        let session = self.session;
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::RequestNotInSession {
                    session,
                    denied_request,
                },
            ),
        }
    }

    /// Prepares an authoritative not-delegate-denied rejection.
    pub const fn prepare_not_delegate_denied(self) -> PreparedOverrideDeniedToolRequest {
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotDelegateDenied { denied_request },
            ),
        }
    }

    /// Prepares an authoritative still-resolving rejection.
    pub const fn prepare_not_terminally_denied(self) -> PreparedOverrideDeniedToolRequest {
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied { denied_request },
            ),
        }
    }

    /// Prepares an authoritative already-overridden rejection.
    pub const fn prepare_already_overridden(self) -> PreparedOverrideDeniedToolRequest {
        let denied_request = self.denied_request;
        PreparedOverrideDeniedToolRequest {
            command: self,
            result: OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::AlreadyOverridden { denied_request },
            ),
        }
    }
}

impl PartialEq for OverrideDeniedToolRequest {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session && self.denied_request == other.denied_request
    }
}

impl Eq for OverrideDeniedToolRequest {}

impl std::hash::Hash for OverrideDeniedToolRequest {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.session.hash(state);
        self.denied_request.hash(state);
    }
}

/// An override command used a reserved user-global identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverrideDeniedToolRequestConstructionError {
    command_id: DurableCommandId,
}

impl OverrideDeniedToolRequestConstructionError {
    /// Returns the rejected command identity.
    pub const fn command_id(self) -> DurableCommandId {
        self.command_id
    }
}

/// Terminal typed result for one override command.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum OverrideDeniedToolRequestResult {
    /// The override was recorded.
    Applied(OverrideDeniedToolRequestAppliedResult),
    /// Authoritative current state rejected the command.
    Rejected(OverrideDeniedToolRequestRejectedResult),
}

/// The recorded override and its complete linked provenance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OverrideDeniedToolRequestAppliedResult {
    recorded: RecordedUserOverride,
}

impl OverrideDeniedToolRequestAppliedResult {
    /// Borrows the exact recorded override.
    pub const fn recorded(&self) -> &RecordedUserOverride {
        &self.recorded
    }
}

/// Closed authoritative rejection vocabulary for override commands.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OverrideDeniedToolRequestRejectedResult {
    /// No logical request had this identity.
    RequestNotFound {
        /// The absent request.
        denied_request: ToolRequestId,
    },
    /// The named request belongs to another session.
    RequestNotInSession {
        /// The session the command named.
        session: SessionId,
        /// The request owned elsewhere.
        denied_request: ToolRequestId,
    },
    /// The request's recorded approval is not a delegate denial.
    NotDelegateDenied {
        /// The request without a delegate denial.
        denied_request: ToolRequestId,
    },
    /// The delegate denial has not reached its terminal denied result.
    NotTerminallyDenied {
        /// The request whose denial is still resolving.
        denied_request: ToolRequestId,
    },
    /// An override is already recorded for this denial.
    AlreadyOverridden {
        /// The already-overridden request.
        denied_request: ToolRequestId,
    },
}

/// A pre-commit override-command candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedOverrideDeniedToolRequest {
    command: OverrideDeniedToolRequest,
    result: OverrideDeniedToolRequestResult,
}

impl PreparedOverrideDeniedToolRequest {
    /// Borrows the canonical command.
    pub const fn command(&self) -> &OverrideDeniedToolRequest {
        &self.command
    }

    /// Borrows the terminal typed result.
    pub const fn result(&self) -> &OverrideDeniedToolRequestResult {
        &self.result
    }

    /// Returns the command and result for one transaction.
    pub fn into_parts(self) -> (OverrideDeniedToolRequest, OverrideDeniedToolRequestResult) {
        (self.command, self.result)
    }
}

/// A command/request adapter correlation error, not a recorded rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverrideDeniedToolRequestPreparationError {
    command: OverrideDeniedToolRequest,
    provided_request: ToolRequestId,
}

impl OverrideDeniedToolRequestPreparationError {
    /// Borrows the unchanged command.
    pub const fn command(&self) -> &OverrideDeniedToolRequest {
        &self.command
    }

    /// Returns the mismatched supplied-evidence request identity.
    pub const fn provided_request(&self) -> ToolRequestId {
        self.provided_request
    }

    /// Returns both unchanged values.
    pub fn into_parts(self) -> (OverrideDeniedToolRequest, ToolRequestId) {
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
    use crate::{
        DirectModelSelection,
        test_support::{command_id, model_call_id, session_id, tool_request_id, turn_id},
    };

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

    /// S10: request names are exact and restricted to the recorded
    /// ASCII spelling.
    #[test]
    fn s10_tool_name_rejects_empty_long_and_unsafe_spelling() {
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

    /// S10: valid JSON is canonicalized recursively,
    /// while malformed provider text remains exact bounded evidence.
    #[test]
    fn s10_arguments_are_canonical_or_exactly_undecodable() {
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

    /// S10: a complete JSON prefix followed by any non-whitespace
    /// provider text remains exact undecodable evidence.
    #[test]
    fn s10_arguments_reject_trailing_non_whitespace() {
        let provider_text = String::from(r#"{"timezone":"UTC"} trailing"#);
        let normalized = NormalizedToolArguments::try_from_provider_text(provider_text.clone())
            .expect("bounded non-JSON text remains admissible evidence");

        assert_eq!(normalized.kind(), ToolArgumentsKind::Undecodable);
        assert_eq!(normalized.as_str(), provider_text);
    }

    /// S10: literal U+0000 cannot enter the durable text
    /// vocabulary even when the remaining provider text is undecodable JSON.
    #[test]
    fn s10_arguments_reject_literal_null() {
        let value = String::from("{\"timezone\":\0");
        let error = NormalizedToolArguments::try_from_provider_text(value.clone())
            .expect_err("PostgreSQL text cannot preserve a literal null");

        assert_eq!(error.value(), value);
        assert_eq!(error.failure(), ToolArgumentsFailure::ContainsNull);
    }

    /// S10: reconstitution rejects a competing noncanonical JSON
    /// representation.
    #[test]
    fn s10_stored_json_must_be_canonical() {
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

    /// S10: canonicalization preserves JSON numeric values outside
    /// the native integer and floating-point ranges without rounding.
    #[test]
    fn s10_arguments_preserve_arbitrary_precision_numbers() {
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

    /// S10: the byte bound, rather than serde's default recursion
    /// cutoff, governs syntactically valid nested JSON.
    #[test]
    fn s10_deeply_nested_arguments_remain_json() {
        let depth = 512;
        let value = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
        let normalized = NormalizedToolArguments::try_from_provider_text(value.clone())
            .expect("deep bounded JSON remains admissible");

        assert_eq!(normalized.kind(), ToolArgumentsKind::Json);
        assert_eq!(normalized.as_str(), value);
    }

    /// S10: malformed input is classified before any recursively
    /// owned JSON tree exists, even after a deeply nested complete child.
    #[test]
    fn s10_deep_partial_json_is_dropped_stack_safely() {
        let depth = 100_000;
        let value = format!("[{}null{},!]", "[".repeat(depth), "]".repeat(depth));
        let normalized = NormalizedToolArguments::try_from_provider_text(value.clone())
            .expect("bounded malformed text remains exact evidence");

        assert_eq!(normalized.kind(), ToolArgumentsKind::Undecodable);
        assert_eq!(normalized.as_str(), value);
    }

    /// delegated approval narrows authority and can never approve or deny a
    /// request frozen as human-only.
    #[test]
    fn delegate_narrows_and_never_widens_human_authority() {
        const HUMAN_ONLY_REQUEST_SEED: u128 = 40;
        const JUDGE_MODEL_SEED: u128 = 41;
        const APPROVAL_CALL_SEED: u128 = 42;
        const DENIAL_CALL_SEED: u128 = 43;
        const ESCALATION_CALL_SEED: u128 = 44;
        const HUMAN_AUTHORITY_RATIONALE: &str = "needs user authority";

        let request = request(HUMAN_ONLY_REQUEST_SEED);
        let model = DirectModelSelection::from_uuid(uuid::Uuid::from_u128(JUDGE_MODEL_SEED));
        let rationale = ToolDecisionRationale::try_new(String::from(HUMAN_AUTHORITY_RATIONALE))
            .expect("fixture rationale is admitted");
        let rejected = DelegateToolApproval::try_new(
            &request,
            model,
            model_call_id(APPROVAL_CALL_SEED),
            DelegateApprovalRecommendation::Approve,
            rationale.clone(),
        )
        .expect_err("a delegate cannot approve a human-only request");
        let rejected_denial = DelegateToolApproval::try_new(
            &request,
            model,
            model_call_id(DENIAL_CALL_SEED),
            DelegateApprovalRecommendation::Deny,
            rationale.clone(),
        )
        .expect_err("a delegate cannot deny a human-only request");
        let escalated = DelegateToolApproval::try_new(
            &request,
            model,
            model_call_id(ESCALATION_CALL_SEED),
            DelegateApprovalRecommendation::EscalateToHuman,
            rationale,
        )
        .expect("escalation preserves human authority");

        assert_eq!(rejected.posture(), ToolApprovalPosture::Human);
        assert_eq!(
            rejected.recommendation(),
            DelegateApprovalRecommendation::Approve
        );
        assert_eq!(rejected_denial.posture(), ToolApprovalPosture::Human);
        assert_eq!(
            rejected_denial.recommendation(),
            DelegateApprovalRecommendation::Deny
        );
        assert_eq!(
            escalated.recommendation(),
            DelegateApprovalRecommendation::EscalateToHuman
        );
    }

    #[test]
    fn delegate_resolution_preserves_model_call_and_rationale() {
        const SUBJECT_REQUEST_SEED: u128 = 50;
        const SUBJECT_SESSION_SEED: u128 = 1;
        const SUBJECT_TURN_SEED: u128 = 2;
        const ISSUING_CALL_SEED: u128 = 3;
        const SUBJECT_ORDINAL: u32 = 0;
        const SUBJECT_TOOL_NAME: &str = "current_time";
        const SUBJECT_ARGUMENTS: &str = "{}";
        const JUDGE_MODEL_SEED: u128 = 51;
        const JUDGE_CALL_SEED: u128 = 52;
        const JUDGE_RATIONALE: &str = "bounded request";

        let request = ToolRequestReconstitutionInput::new(
            tool_request_id(SUBJECT_REQUEST_SEED),
            session_id(SUBJECT_SESSION_SEED),
            turn_id(SUBJECT_TURN_SEED),
            model_call_id(ISSUING_CALL_SEED),
            ToolRequestOrdinal::from_u32(SUBJECT_ORDINAL),
            ToolName::try_new(String::from(SUBJECT_TOOL_NAME)).expect("fixture name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from(SUBJECT_ARGUMENTS))
                .expect("fixture arguments are valid"),
        )
        .with_approval_posture(ToolApprovalPosture::Delegated)
        .into_request();
        let model = DirectModelSelection::from_uuid(uuid::Uuid::from_u128(JUDGE_MODEL_SEED));
        let call = model_call_id(JUDGE_CALL_SEED);
        let rationale = ToolDecisionRationale::try_new(String::from(JUDGE_RATIONALE))
            .expect("fixture rationale is admitted");
        let approval = DelegateToolApproval::try_new(
            &request,
            model,
            call,
            DelegateApprovalRecommendation::Deny,
            rationale.clone(),
        )
        .expect("delegated authority may deny");
        let stored_reason = ToolDenialReason::try_new(String::from(JUDGE_RATIONALE))
            .expect("fixture rationale is an admitted reason");
        let resolution = ToolApprovalResolutionReconstitutionInput::delegate(
            approval,
            Some(stored_reason.clone()),
        )
        .reconstitute()
        .expect("checked delegate evidence restores its decision");

        assert_eq!(
            resolution.decider(),
            Some(&ToolApprovalDecider::Delegate { model, call })
        );
        assert_eq!(resolution.rationale(), Some(&rationale));
        assert_eq!(
            resolution.decision(),
            &ToolApprovalDecision::Deny {
                reason: Some(stored_reason)
            }
        );
    }

    /// One delegate denial whose recorded rationale is "scope exceeded".
    fn denied_delegate_fixture() -> DelegateToolApproval {
        const SUBJECT_REQUEST_SEED: u128 = 60;
        const SUBJECT_SESSION_SEED: u128 = 1;
        const SUBJECT_TURN_SEED: u128 = 2;
        const ISSUING_CALL_SEED: u128 = 3;
        const SUBJECT_TOOL_NAME: &str = "current_time";
        const SUBJECT_ARGUMENTS: &str = "{}";
        const JUDGE_MODEL_SEED: u128 = 61;
        const JUDGE_CALL_SEED: u128 = 62;
        const JUDGE_RATIONALE: &str = "scope exceeded";

        let request = ToolRequestReconstitutionInput::new(
            tool_request_id(SUBJECT_REQUEST_SEED),
            session_id(SUBJECT_SESSION_SEED),
            turn_id(SUBJECT_TURN_SEED),
            model_call_id(ISSUING_CALL_SEED),
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from(SUBJECT_TOOL_NAME)).expect("fixture name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from(SUBJECT_ARGUMENTS))
                .expect("fixture arguments are valid"),
        )
        .with_approval_posture(ToolApprovalPosture::Delegated)
        .into_request();
        DelegateToolApproval::try_new(
            &request,
            DirectModelSelection::from_uuid(uuid::Uuid::from_u128(JUDGE_MODEL_SEED)),
            model_call_id(JUDGE_CALL_SEED),
            DelegateApprovalRecommendation::Deny,
            ToolDecisionRationale::try_new(String::from(JUDGE_RATIONALE))
                .expect("fixture rationale is admitted"),
        )
        .expect("delegated authority may deny")
    }

    /// A stored null reason is admitted exactly when the rationale derives
    /// nothing; a null beside a deriving rationale is missing evidence and
    /// fails closed.
    #[test]
    fn delegate_reconstitution_admits_null_only_for_empty_derivation() {
        let denial = denied_delegate_fixture();
        let missing_evidence = ToolApprovalResolutionReconstitutionInput::delegate(denial, None)
            .reconstitute()
            .expect_err("a null reason beside a deriving rationale is rejected");
        drop(missing_evidence);
    }

    /// A stored delegate denial reason the recorded rationale cannot derive
    /// is corruption, not a decision to restore.
    #[test]
    fn delegate_reconstitution_rejects_an_unrelated_stored_reason() {
        let mismatched = ToolApprovalResolutionReconstitutionInput::delegate(
            denied_delegate_fixture(),
            Some(
                ToolDenialReason::try_new(String::from("unrelated stored text"))
                    .expect("fixture reason is admitted"),
            ),
        )
        .reconstitute()
        .expect_err("a stored reason the rationale cannot derive is rejected");
        drop(mismatched);
    }

    fn admitted_rationale(value: &str) -> ToolDecisionRationale {
        ToolDecisionRationale::try_new(String::from(value)).expect("fixture rationale is admitted")
    }

    /// A rationale already inside the reason bounds derives verbatim.
    #[test]
    fn denial_reason_derivation_preserves_admissible_text_verbatim() {
        assert_eq!(
            ToolDenialReason::from_rationale(&admitted_rationale("scope exceeded"))
                .map(ToolDenialReason::into_string),
            Some(String::from("scope exceeded"))
        );
    }

    /// Control characters become spaces and forbidden edge spaces trim.
    #[test]
    fn denial_reason_derivation_maps_control_characters_and_trims_edges() {
        assert_eq!(
            ToolDenialReason::from_rationale(&admitted_rationale("  first\nsecond\tthird  "))
                .map(ToolDenialReason::into_string),
            Some(String::from("first second third"))
        );
    }

    /// A rationale of only control characters and spaces derives nothing.
    #[test]
    fn denial_reason_derivation_of_whitespace_only_text_is_empty() {
        assert_eq!(
            ToolDenialReason::from_rationale(&admitted_rationale(" \n \t ")),
            None
        );
    }

    /// Admitted non-POSIX edge whitespace such as NBSP is preserved.
    #[test]
    fn denial_reason_derivation_preserves_admitted_edge_whitespace() {
        assert_eq!(
            ToolDenialReason::from_rationale(&admitted_rationale("\u{00a0}denied\u{00a0}"))
                .map(ToolDenialReason::into_string),
            Some(String::from("\u{00a0}denied\u{00a0}"))
        );
    }

    /// Oversized text cuts to the reason bound on a character boundary.
    #[test]
    fn denial_reason_derivation_truncates_on_a_character_boundary() {
        let truncation_prefix = "a".repeat(1023);
        let oversized = ToolDecisionRationale::try_new(format!("{truncation_prefix}é"))
            .expect("fixture rationale is admitted");
        let truncated =
            ToolDenialReason::from_rationale(&oversized).expect("nonempty text derives a reason");
        assert_eq!(truncated.as_str(), truncation_prefix);
    }

    /// Every derived reason re-admits through the reason validator.
    #[test]
    fn denial_reason_derivation_output_is_always_admissible() {
        let derived =
            ToolDenialReason::from_rationale(&admitted_rationale("  first\nsecond\tthird  "))
                .expect("nonempty text derives a reason");
        assert!(ToolDenialReason::try_new(derived.into_string()).is_ok());
    }

    /// S10: a restored session-blanket approval requires the
    /// approve-all posture frozen for that turn.
    #[test]
    fn s10_session_blanket_reconstitution_requires_frozen_authority() {
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

    /// S10: credential-boundary suppression constructs an
    /// inert proposal and restores only the fixed automatic denial provenance.
    #[test]
    fn s10_runtime_safety_denial_is_non_executable() {
        let request = tool_request_id(4);
        let proposal = ToolCallProposal::suppressed(
            ToolName::try_new(String::from("sandboxed_exec")).expect("fixture tool name is valid"),
        );
        let restored = ToolApprovalResolutionReconstitutionInput::runtime_safety(request)
            .reconstitute()
            .expect("runtime safety evidence is self-authenticating");

        assert!(proposal.is_suppressed());
        assert_eq!(proposal.arguments().as_str(), SUPPRESSED_TOOL_ARGUMENTS);
        assert_eq!(restored.request(), request);
        assert_eq!(restored.source(), ToolDecisionSource::RuntimeSafety);
        assert_eq!(
            restored.decision(),
            &ToolApprovalDecision::Deny {
                reason: Some(
                    ToolDenialReason::try_new(String::from(SUPPRESSED_TOOL_DENIAL_REASON))
                        .expect("fixed denial reason is valid"),
                ),
            }
        );
        assert!(!restored.is_approved());
    }

    /// S10: only the user-command preparation path can construct
    /// user-sourced approval.
    #[test]
    fn s10_user_command_preparation_preserves_agency() {
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
            ToolDecisionSource::UserCommand
        );
        assert_eq!(
            applied.resolution().decider(),
            Some(&ToolApprovalDecider::User {
                command: prepared.command().command_id(),
            })
        );
        assert_eq!(applied.resolution().rationale(), None);
        assert!(applied.resolution().is_approved());
    }

    /// S10: one provider response admits at most the recorded 32
    /// logical tool requests without accepting a partial prefix.
    #[test]
    fn s10_tool_response_request_count_is_bounded() {
        let admitted = ToolUsingAssistantResponse::try_from_parts(tool_response_parts(32))
            .expect("the exact per-response limit is admitted");
        let rejected = ToolUsingAssistantResponse::try_from_parts(tool_response_parts(33))
            .expect_err("the first response above the limit is rejected whole");

        assert_eq!(admitted.tool_count(), 32);
        assert_eq!(rejected.into_parts().len(), 33);
    }

    /// user-global command sentinels never enter the canonical
    /// tool-decision command space.
    #[test]
    fn tool_decision_rejects_reserved_command_identities() {
        let nil_command_id = DurableCommandId::from_uuid(uuid::Uuid::nil());
        let nil_error = DecideToolRequest::try_new(
            nil_command_id,
            tool_request_id(1),
            ToolApprovalDecision::Approve,
        )
        .expect_err("the nil command identity is rejected");
        assert_eq!(nil_error.command_id(), nil_command_id);

        let max_command_id = DurableCommandId::from_uuid(uuid::Uuid::max());
        let max_error = DecideToolRequest::try_new(
            max_command_id,
            tool_request_id(1),
            ToolApprovalDecision::Approve,
        )
        .expect_err("the max command identity is rejected");
        assert_eq!(max_error.command_id(), max_command_id);
    }

    /// S10: only an applied user command can restore
    /// user-command approval authority.
    #[test]
    fn s10_rejected_user_command_cannot_restore_approval() {
        let command = DecideToolRequest::new(
            command_id(5),
            tool_request_id(4),
            ToolApprovalDecision::Approve,
        )
        .prepare_request_not_found();
        let input = ToolApprovalResolutionReconstitutionInput::user_command(command);

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

    /// S15: the admission bound is inclusive, so a result of exactly the
    /// bounded size is admitted exactly.
    #[test]
    fn s15_result_text_admits_exactly_the_bounded_size() {
        let at_bound = "r".repeat(MAX_TOOL_RESULT_TEXT_BYTES);

        let admitted = ToolResultText::try_new(at_bound.clone())
            .expect("the bound itself is an admissible result size");
        assert_eq!(admitted.as_str(), at_bound);
    }

    /// S15: one byte past the bound is refused, and the refusal reports the
    /// observed size while retaining the rejected text without rewriting it.
    #[test]
    fn s15_result_text_rejects_one_byte_past_the_bound() {
        let past_bound = "r".repeat(MAX_TOOL_RESULT_TEXT_BYTES + 1);

        let error = ToolResultText::try_new(past_bound.clone())
            .expect_err("one byte past the bound is not an admissible result");

        assert_eq!(
            error.failure(),
            ToolResultTextFailure::TooLarge {
                bytes: past_bound.len(),
            }
        );
        assert_eq!(error.value(), past_bound);
    }

    /// S15: literal U+0000 cannot enter the durable result vocabulary, and the
    /// refusal retains the rejected text without rewriting it.
    #[test]
    fn s15_result_text_rejects_a_literal_null() {
        let value = String::from("head\0tail");

        let error = ToolResultText::try_new(value.clone())
            .expect_err("PostgreSQL text cannot preserve a literal null");

        assert_eq!(error.failure(), ToolResultTextFailure::ContainsNull);
        assert_eq!(error.value(), value);
    }

    /// durable-command comparison equality excludes only command
    /// identity and retains the exact decision payload.
    #[test]
    fn decision_command_equality_excludes_only_command_identity() {
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

    /// S11: denials remain request-bound logical resolutions and
    /// cannot name a physical attempt.
    #[test]
    fn s11_denial_resolution_names_only_the_request() {
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

    /// The request-identity seed of the canonical delegate-denied fixture; the
    /// judge model and call seeds derive from it, decorrelated per testing
    /// rule 4.
    const DENIED_REQUEST_SEED: u128 = 70;
    /// The seed of the fixture override command overriding that denial.
    const OVERRIDE_COMMAND_SEED: u128 = 71;

    /// One request frozen `Delegated` in the canonical fixture session.
    fn delegated_request(seed: u128) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            tool_request_id(seed),
            session_id(1),
            turn_id(2),
            model_call_id(3),
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("current_time")).expect("fixture name is valid"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are valid"),
        )
        .with_approval_posture(ToolApprovalPosture::Delegated)
        .into_request()
    }

    /// The judge model and call seeds of the canonical delegate denial;
    /// arbitrary — they only need to exist as one recorded judge.
    const DENYING_JUDGE_MODEL_SEED: u128 = 200;
    const DENYING_JUDGE_CALL_SEED: u128 = 201;

    /// The delegate denial recorded against one delegated request by the
    /// canonical fixture judge.
    fn delegate_denial(request: &ToolRequest) -> ToolApprovalResolution {
        let denial = DelegateToolApproval::try_new(
            request,
            DirectModelSelection::from_uuid(uuid::Uuid::from_u128(DENYING_JUDGE_MODEL_SEED)),
            model_call_id(DENYING_JUDGE_CALL_SEED),
            DelegateApprovalRecommendation::Deny,
            ToolDecisionRationale::try_new(String::from("scope exceeded"))
                .expect("fixture rationale is admitted"),
        )
        .expect("delegated authority may deny");
        ToolApprovalResolution::delegate(&denial)
            .expect("a delegate denial resolves the delegated request")
    }

    /// The canonical override command naming the fixture denial in its own
    /// session.
    fn override_command() -> OverrideDeniedToolRequest {
        OverrideDeniedToolRequest::try_new(
            command_id(OVERRIDE_COMMAND_SEED),
            session_id(1),
            tool_request_id(DENIED_REQUEST_SEED),
        )
        .expect("the fixture command identity is admitted")
    }

    /// The override verification predicate records exactly the denied command:
    /// every conjunct holds, and the recorded override links the command, the
    /// session, the denied request, and the denying judge call.
    #[test]
    fn override_prepare_records_the_exact_denied_command() {
        let request = delegated_request(DENIED_REQUEST_SEED);
        let denial = delegate_denial(&request);
        let prepared = override_command()
            .prepare(
                &request,
                Some(&denial),
                Some(ToolRequestResolution::Denied {
                    request: request.id(),
                }),
                None,
            )
            .expect("correlated evidence prepares a terminal result");

        let OverrideDeniedToolRequestResult::Applied(applied) = prepared.result() else {
            panic!("a terminal delegate denial admits the override");
        };
        let recorded = applied.recorded();
        let Some(ToolApprovalDecider::Delegate {
            call: denying_call, ..
        }) = denial.decider()
        else {
            panic!("the fixture denial carries delegate provenance");
        };
        assert_eq!(recorded.command(), prepared.command().command_id());
        assert_eq!(recorded.session(), request.session());
        assert_eq!(recorded.denied_request(), request.id());
        assert_eq!(recorded.judge_call(), *denying_call);
        assert_eq!(recorded.tool(), request.name());
        assert_eq!(recorded.arguments(), request.arguments());
    }

    /// An recorded override matches only the exact denied command: equal tool
    /// name and equal normalized arguments.
    #[test]
    fn recorded_override_matches_only_the_exact_denied_command() {
        let request = delegated_request(DENIED_REQUEST_SEED);
        let denial = delegate_denial(&request);
        let prepared = override_command()
            .prepare(
                &request,
                Some(&denial),
                Some(ToolRequestResolution::Denied {
                    request: request.id(),
                }),
                None,
            )
            .expect("correlated evidence prepares a terminal result");
        let OverrideDeniedToolRequestResult::Applied(applied) = prepared.result() else {
            panic!("a terminal delegate denial admits the override");
        };
        let recorded = applied.recorded();

        let same_command =
            ToolCallProposal::new(request.name().clone(), request.arguments().clone());
        let other_arguments = ToolCallProposal::new(
            request.name().clone(),
            NormalizedToolArguments::try_from_provider_text(String::from(r#"{"timezone":"UTC"}"#))
                .expect("fixture arguments are valid"),
        );
        let other_tool = ToolCallProposal::new(
            ToolName::try_new(String::from("another_tool")).expect("fixture name is valid"),
            request.arguments().clone(),
        );
        assert!(recorded.matches_proposal(&same_command));
        assert!(!recorded.matches_proposal(&other_arguments));
        assert!(!recorded.matches_proposal(&other_tool));
    }

    /// Predicate conjunct: the request must belong to the command's session.
    #[test]
    fn override_prepare_rejects_another_sessions_request() {
        const OTHER_SESSION_SEED: u128 = 9;

        let request = delegated_request(DENIED_REQUEST_SEED);
        let denial = delegate_denial(&request);
        let command = OverrideDeniedToolRequest::try_new(
            command_id(OVERRIDE_COMMAND_SEED),
            session_id(OTHER_SESSION_SEED),
            request.id(),
        )
        .expect("the fixture command identity is admitted");
        let prepared = command
            .prepare(
                &request,
                Some(&denial),
                Some(ToolRequestResolution::Denied {
                    request: request.id(),
                }),
                None,
            )
            .expect("correlated evidence prepares a terminal result");

        assert_eq!(
            prepared.result(),
            &OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::RequestNotInSession {
                    session: session_id(OTHER_SESSION_SEED),
                    denied_request: request.id(),
                }
            )
        );
    }

    /// Predicate conjunct: an undecided request has no delegate denial to
    /// override.
    #[test]
    fn override_prepare_rejects_an_undecided_request() {
        let request = delegated_request(DENIED_REQUEST_SEED);
        let prepared = override_command()
            .prepare(&request, None, None, None)
            .expect("correlated evidence prepares a terminal result");

        assert_eq!(
            prepared.result(),
            &OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotDelegateDenied {
                    denied_request: request.id(),
                }
            )
        );
    }

    /// Predicate conjunct: a user denial is not a judge denial; the override
    /// can never reverse the user's own decision.
    #[test]
    fn override_prepare_rejects_a_user_denial() {
        const USER_DENIAL_COMMAND_SEED: u128 = 8;

        let request = delegated_request(DENIED_REQUEST_SEED);
        let user_denial = ToolApprovalResolution::user(
            command_id(USER_DENIAL_COMMAND_SEED),
            request.id(),
            ToolApprovalDecision::Deny { reason: None },
        );
        let prepared = override_command()
            .prepare(
                &request,
                Some(&user_denial),
                Some(ToolRequestResolution::Denied {
                    request: request.id(),
                }),
                None,
            )
            .expect("correlated evidence prepares a terminal result");

        assert_eq!(
            prepared.result(),
            &OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotDelegateDenied {
                    denied_request: request.id(),
                }
            )
        );
    }

    /// Predicate conjunct: a delegate approval is not a denial; there is
    /// nothing to override.
    #[test]
    fn override_prepare_rejects_a_delegate_approval() {
        const APPROVING_JUDGE_CALL_SEED: u128 = 12;

        let request = delegated_request(DENIED_REQUEST_SEED);
        let approval = DelegateToolApproval::try_new(
            &request,
            DirectModelSelection::from_uuid(uuid::Uuid::from_u128(DENYING_JUDGE_MODEL_SEED)),
            model_call_id(APPROVING_JUDGE_CALL_SEED),
            DelegateApprovalRecommendation::Approve,
            ToolDecisionRationale::try_new(String::from("bounded request"))
                .expect("fixture rationale is admitted"),
        )
        .expect("delegated authority may approve");
        let approval = ToolApprovalResolution::delegate(&approval)
            .expect("a delegate approval resolves the delegated request");
        let prepared = override_command()
            .prepare(
                &request,
                Some(&approval),
                Some(ToolRequestResolution::Denied {
                    request: request.id(),
                }),
                None,
            )
            .expect("correlated evidence prepares a terminal result");

        assert_eq!(
            prepared.result(),
            &OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotDelegateDenied {
                    denied_request: request.id(),
                }
            )
        );
    }

    /// Predicate conjunct: a delegate denial whose denied result is not yet
    /// materialized is still resolving and cannot be overridden.
    #[test]
    fn override_prepare_rejects_a_denial_still_resolving() {
        let request = delegated_request(DENIED_REQUEST_SEED);
        let denial = delegate_denial(&request);
        let prepared = override_command()
            .prepare(&request, Some(&denial), None, None)
            .expect("correlated evidence prepares a terminal result");

        assert_eq!(
            prepared.result(),
            &OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied {
                    denied_request: request.id(),
                }
            )
        );
    }

    /// Predicate conjunct: the terminal resolution must be this exact
    /// request's denial, so mismatched terminal evidence fails closed.
    #[test]
    fn override_prepare_rejects_a_foreign_terminal_denial() {
        const FOREIGN_REQUEST_SEED: u128 = 6;

        let request = delegated_request(DENIED_REQUEST_SEED);
        let denial = delegate_denial(&request);
        let prepared = override_command()
            .prepare(
                &request,
                Some(&denial),
                Some(ToolRequestResolution::Denied {
                    request: tool_request_id(FOREIGN_REQUEST_SEED),
                }),
                None,
            )
            .expect("correlated evidence prepares a terminal result");

        assert_eq!(
            prepared.result(),
            &OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::NotTerminallyDenied {
                    denied_request: request.id(),
                }
            )
        );
    }

    /// Predicate conjunct: each denial admits at most one override ever.
    #[test]
    fn override_prepare_rejects_an_already_overridden_denial() {
        const EARLIER_OVERRIDE_COMMAND_SEED: u128 = 7;

        let request = delegated_request(DENIED_REQUEST_SEED);
        let denial = delegate_denial(&request);
        let prepared = override_command()
            .prepare(
                &request,
                Some(&denial),
                Some(ToolRequestResolution::Denied {
                    request: request.id(),
                }),
                Some(command_id(EARLIER_OVERRIDE_COMMAND_SEED)),
            )
            .expect("correlated evidence prepares a terminal result");

        assert_eq!(
            prepared.result(),
            &OverrideDeniedToolRequestResult::Rejected(
                OverrideDeniedToolRequestRejectedResult::AlreadyOverridden {
                    denied_request: request.id(),
                }
            )
        );
    }

    /// Evidence for another request is an adapter correlation error, never a
    /// recorded rejection.
    #[test]
    fn override_prepare_correlates_supplied_evidence() {
        const UNCORRELATED_REQUEST_SEED: u128 = 5;

        let uncorrelated = delegated_request(UNCORRELATED_REQUEST_SEED);
        let error = override_command()
            .prepare(&uncorrelated, None, None, None)
            .expect_err("mismatched request evidence must fail as a preparation error");

        assert_eq!(error.provided_request(), uncorrelated.id());
        assert_eq!(error.command(), &override_command());
    }

    /// The recorded applied receipt restores from its durable recorded row and
    /// rejects a row that does not correlate with the command.
    #[test]
    fn override_reconstitute_applied_restores_the_recorded_receipt() {
        const FOREIGN_OVERRIDE_REQUEST_SEED: u128 = 4;
        const FIXTURE_JUDGE_CALL_SEED: u128 = 930;

        let request = delegated_request(DENIED_REQUEST_SEED);
        let recorded = RecordedUserOverride::new(
            command_id(OVERRIDE_COMMAND_SEED),
            request.session(),
            request.id(),
            model_call_id(FIXTURE_JUDGE_CALL_SEED),
            request.name().clone(),
            request.arguments().clone(),
        );
        let restored = override_command()
            .reconstitute_applied(recorded.clone())
            .expect("the correlated recorded row restores the applied receipt");
        let OverrideDeniedToolRequestResult::Applied(applied) = restored.result() else {
            panic!("the recorded row restores an applied result");
        };
        assert_eq!(applied.recorded(), &recorded);

        let foreign = RecordedUserOverride::new(
            command_id(OVERRIDE_COMMAND_SEED),
            request.session(),
            tool_request_id(FOREIGN_OVERRIDE_REQUEST_SEED),
            model_call_id(FIXTURE_JUDGE_CALL_SEED),
            request.name().clone(),
            request.arguments().clone(),
        );
        let error = override_command()
            .reconstitute_applied(foreign)
            .expect_err("an uncorrelated recorded row must fail closed");
        assert_eq!(
            error.provided_request(),
            tool_request_id(FOREIGN_OVERRIDE_REQUEST_SEED)
        );
    }

    /// the reserved user-global nil and max command sentinels cannot
    /// claim override commands.
    #[test]
    fn override_command_identity_rejects_reserved_sentinels() {
        let nil = OverrideDeniedToolRequest::try_new(
            DurableCommandId::from_uuid(uuid::Uuid::nil()),
            session_id(1),
            tool_request_id(DENIED_REQUEST_SEED),
        )
        .expect_err("the nil sentinel is reserved");
        let max = OverrideDeniedToolRequest::try_new(
            DurableCommandId::from_uuid(uuid::Uuid::max()),
            session_id(1),
            tool_request_id(DENIED_REQUEST_SEED),
        )
        .expect_err("the max sentinel is reserved");

        assert_eq!(
            nil.command_id(),
            DurableCommandId::from_uuid(uuid::Uuid::nil())
        );
        assert_eq!(
            max.command_id(),
            DurableCommandId::from_uuid(uuid::Uuid::max())
        );
    }

    /// override-command comparison equality excludes only command
    /// identity and retains the session and the denied request.
    #[test]
    fn override_command_equality_excludes_only_command_identity() {
        const REPLAY_COMMAND_SEED: u128 = 72;
        const OTHER_SESSION_SEED: u128 = 9;

        let replay = OverrideDeniedToolRequest::try_new(
            command_id(REPLAY_COMMAND_SEED),
            session_id(1),
            tool_request_id(DENIED_REQUEST_SEED),
        )
        .expect("the fixture command identity is admitted");
        let other_session = OverrideDeniedToolRequest::try_new(
            command_id(OVERRIDE_COMMAND_SEED),
            session_id(OTHER_SESSION_SEED),
            tool_request_id(DENIED_REQUEST_SEED),
        )
        .expect("the fixture command identity is admitted");

        assert_eq!(override_command(), replay);
        assert_ne!(override_command(), other_session);
    }

    /// A consumed user override records approval under override provenance:
    /// the override source, the override command, and the overridden denial.
    #[test]
    fn user_override_initial_approval_records_override_provenance() {
        const CONSUMING_REQUEST_SEED: u128 = 73;

        let approval = InitialToolApproval::UserOverride {
            command: command_id(OVERRIDE_COMMAND_SEED),
            denied_request: tool_request_id(DENIED_REQUEST_SEED),
        };
        let resolution = approval
            .resolution(tool_request_id(CONSUMING_REQUEST_SEED))
            .expect("a consumed override records its approval at proposal time");

        assert_eq!(
            resolution.request(),
            tool_request_id(CONSUMING_REQUEST_SEED)
        );
        assert_eq!(resolution.source(), ToolDecisionSource::UserOverride);
        assert_eq!(
            resolution.decider(),
            Some(&ToolApprovalDecider::UserOverride {
                command: command_id(OVERRIDE_COMMAND_SEED),
                denied_request: tool_request_id(DENIED_REQUEST_SEED),
            })
        );
        assert_eq!(resolution.decision(), &ToolApprovalDecision::Approve);
        assert_eq!(resolution.rationale(), None);
        assert_eq!(approval.posture(), ToolApprovalPosture::Delegated);
        assert!(!approval.requires_decision());
    }

    /// S10: a restored user-override approval requires the
    /// delegated posture frozen on its request — the posture the judge would
    /// otherwise decide.
    #[test]
    fn s10_user_override_reconstitution_requires_delegated_posture() {
        const CONSUMING_REQUEST_SEED: u128 = 73;

        let restored = ToolApprovalResolutionReconstitutionInput::user_override(
            tool_request_id(CONSUMING_REQUEST_SEED),
            command_id(OVERRIDE_COMMAND_SEED),
            tool_request_id(DENIED_REQUEST_SEED),
            ToolApprovalPosture::Delegated,
        )
        .reconstitute()
        .expect("the frozen delegated posture restores override authority");
        assert_eq!(restored.source(), ToolDecisionSource::UserOverride);
        assert_eq!(restored.request(), tool_request_id(CONSUMING_REQUEST_SEED));

        let human = ToolApprovalResolutionReconstitutionInput::user_override(
            tool_request_id(CONSUMING_REQUEST_SEED),
            command_id(OVERRIDE_COMMAND_SEED),
            tool_request_id(DENIED_REQUEST_SEED),
            ToolApprovalPosture::Human,
        )
        .reconstitute()
        .expect_err("a human-frozen request cannot restore override authority");
        drop(human);
        let auto = ToolApprovalResolutionReconstitutionInput::user_override(
            tool_request_id(CONSUMING_REQUEST_SEED),
            command_id(OVERRIDE_COMMAND_SEED),
            tool_request_id(DENIED_REQUEST_SEED),
            ToolApprovalPosture::Auto,
        )
        .reconstitute()
        .expect_err("an auto-frozen request cannot restore override authority");
        drop(auto);
    }
}

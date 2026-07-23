//! Closed version-one JSON-lines process protocol.
//!
//! This crate owns wire representations and frame validation only. Domain,
//! persistence, and client presentation values remain distinct mappings
//! (docs/spec/process-protocol.md).

use std::{collections::HashSet, error::Error, fmt};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{MapAccess, Visitor},
};
use serde_json::value::RawValue;
use uuid::Uuid;

/// The only protocol version accepted by this crate.
pub const PROTOCOL_VERSION: u64 = 1;

/// Maximum encoded frame size, including its final newline.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Maximum UTF-8 bytes in one transcript content fragment.
pub const MAX_CONTENT_FRAGMENT_BYTES: usize = 1024 * 1024;

/// A lowercase hyphenated UUID at the process boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalUuid(Uuid);

impl CanonicalUuid {
    /// Constructs the canonical wire value from a UUID.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Returns the underlying UUID for an explicit adapter mapping.
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }

    fn parse(value: &str) -> Result<Self, CanonicalValueError> {
        let parsed = Uuid::parse_str(value).map_err(|_| CanonicalValueError::Uuid)?;
        if parsed.hyphenated().to_string() != value {
            return Err(CanonicalValueError::Uuid);
        }
        Ok(Self(parsed))
    }
}

impl fmt::Display for CanonicalUuid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl Serialize for CanonicalUuid {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CanonicalUuid {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// A non-sentinel durable command UUID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CommandId(CanonicalUuid);

impl CommandId {
    /// Validates the nil and all-ones sentinels reserved by command handling.
    pub fn try_from_uuid(value: Uuid) -> Result<Self, CanonicalValueError> {
        if value.is_nil() || value.as_u128() == u128::MAX {
            return Err(CanonicalValueError::CommandId);
        }
        Ok(Self(CanonicalUuid::from_uuid(value)))
    }

    /// Returns the UUID for explicit application-boundary mapping.
    pub const fn into_uuid(self) -> Uuid {
        self.0.into_uuid()
    }
}

impl Serialize for CommandId {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CommandId {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let value = CanonicalUuid::deserialize(deserializer)?;
        Self::try_from_uuid(value.into_uuid()).map_err(serde::de::Error::custom)
    }
}

/// A full-range unsigned 64-bit value encoded as its shortest decimal string.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalU64(u64);

impl CanonicalU64 {
    /// Wraps an unsigned value for precision-safe wire encoding.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric value after canonical decoding.
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl TryFrom<String> for CanonicalU64 {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        parse_decimal_u64(&value).map(Self)
    }
}

impl From<CanonicalU64> for String {
    fn from(value: CanonicalU64) -> Self {
        value.0.to_string()
    }
}

/// Request correlation identity. Zero is reserved for uncorrelated errors.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(u64);

impl RequestId {
    /// Constructs a client-usable nonzero request identity.
    pub fn try_new(value: u64) -> Result<Self, CanonicalValueError> {
        if value == 0 {
            Err(CanonicalValueError::RequestId)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the reserved identity for a frame that cannot be correlated.
    pub const fn uncorrelated() -> Self {
        Self(0)
    }

    /// Returns the numeric identity after canonical decoding.
    pub const fn value(self) -> u64 {
        self.0
    }

    const fn is_correlated(self) -> bool {
        self.0 != 0
    }
}

impl TryFrom<String> for RequestId {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        parse_decimal_u64(&value).map(Self)
    }
}

impl From<RequestId> for String {
    fn from(value: RequestId) -> Self {
        value.0.to_string()
    }
}

/// Exact owner input content carried to the application admission boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputContent(String);

impl InputContent {
    /// Wraps decoded content without applying application admission policy.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Borrows exact decoded text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One bounded transcript-content fragment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentFragment(String);

impl ContentFragment {
    /// Applies the per-fragment UTF-8 byte bound.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        if value.len() > MAX_CONTENT_FRAGMENT_BYTES {
            Err(CanonicalValueError::Content)
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows exact fragment text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContentFragment {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<ContentFragment> for String {
    fn from(value: ContentFragment) -> Self {
        value.0
    }
}

/// Iterates exact text as bounded fragments split only at UTF-8 boundaries.
pub fn content_fragments(value: &str) -> ContentFragments<'_> {
    ContentFragments {
        remaining: value,
        emitted_empty: false,
    }
}

/// Borrowed iterator returned by [`content_fragments`].
#[derive(Clone, Debug)]
pub struct ContentFragments<'a> {
    remaining: &'a str,
    emitted_empty: bool,
}

impl Iterator for ContentFragments<'_> {
    type Item = ContentFragment;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            if self.emitted_empty {
                return None;
            }
            self.emitted_empty = true;
            return Some(ContentFragment(String::new()));
        }
        let mut end = self.remaining.len().min(MAX_CONTENT_FRAGMENT_BYTES);
        while !self.remaining.is_char_boundary(end) {
            end -= 1;
        }
        let (fragment, remaining) = self.remaining.split_at(end);
        self.remaining = remaining;
        self.emitted_empty = true;
        Some(ContentFragment(fragment.to_owned()))
    }
}

/// Invalid canonical scalar at the wire boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalValueError {
    /// UUID was not lowercase canonical hyphenated text.
    Uuid,
    /// Command UUID used a reserved sentinel.
    CommandId,
    /// Decimal text was not the shortest full-range unsigned spelling.
    Decimal,
    /// Client request identity was zero.
    RequestId,
    /// A transcript fragment exceeded its UTF-8 byte bound.
    Content,
}

impl fmt::Display for CanonicalValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Uuid => "UUID is not canonical lowercase hyphenated text",
            Self::CommandId => "command identity is a reserved sentinel",
            Self::Decimal => "unsigned integer is not canonical decimal text",
            Self::RequestId => "client request identity must be nonzero",
            Self::Content => "content fragment exceeds the version-one UTF-8 byte bound",
        })
    }
}

impl Error for CanonicalValueError {}

fn parse_decimal_u64(value: &str) -> Result<u64, CanonicalValueError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(CanonicalValueError::Decimal);
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|_| CanonicalValueError::Decimal)?;
    if parsed.to_string() != value {
        return Err(CanonicalValueError::Decimal);
    }
    Ok(parsed)
}

/// Direct or alias model-selection request at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSelection {
    /// Stable direct selection key.
    Direct {
        /// Exact configured direct-selection identity.
        selection_id: CanonicalUuid,
    },
    /// Stable alias key resolved by the hub.
    Alias {
        /// Exact configured alias identity.
        alias_id: CanonicalUuid,
    },
}

/// Closed version-one request family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Create an owner-initiated session.
    CreateSession {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Initial session model-selection defaults.
        initial_model_selection: ModelSelection,
    },
    /// List current sessions.
    ListSessions {},
    /// Submit sequential owner input.
    SubmitInput {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact owner text.
        content: InputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
    },
    /// Read one durable transcript snapshot.
    ReadTranscript {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Read a snapshot and follow later durable updates.
    FollowSession {
        /// Target session.
        session_id: CanonicalUuid,
    },
}

/// One validated client frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFrame {
    version: u64,
    request_id: RequestId,
    request: ClientRequest,
}

impl ClientFrame {
    /// Constructs a version-one frame with a correlated request identity.
    pub fn try_new(
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version: PROTOCOL_VERSION,
            request_id,
            request,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Borrows the closed request.
    pub const fn request(&self) -> &ClientRequest {
        &self.request
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if self.version != PROTOCOL_VERSION {
            return Err(FrameValidationError::UnsupportedVersion);
        }
        if !self.request_id.is_correlated() {
            return Err(FrameValidationError::UncorrelatedClientRequest);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClientFrame {
    version: u64,
    request_id: RequestId,
    request: ClientRequest,
}

impl<'de> Deserialize<'de> for ClientFrame {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawClientFrame::deserialize(deserializer)?;
        let frame = Self {
            version: raw.version,
            request_id: raw.request_id,
            request: raw.request,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

/// Stable server error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// JSON, UTF-8, framing, field, or size validation failed.
    MalformedFrame,
    /// Client version is not one.
    UnsupportedVersion,
    /// A boundary value cannot construct the application input.
    InvalidRequest,
    /// A read target does not exist.
    NotFound,
    /// A durable identity already names different intent.
    ConflictingReuse,
    /// Canonical command handling recorded a typed rejection.
    Rejected,
    /// A follower fell behind bounded fan-out.
    ResyncRequired,
    /// Infrastructure prevented completion.
    Unavailable,
    /// Fail-closed corruption or a hub defect stopped the request.
    Internal,
}

/// Typed durable submit rejection details.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RejectionDetail {
    /// The target session did not exist at command handling.
    SessionNotFound {
        /// Absent target.
        session_id: CanonicalUuid,
    },
    /// A turn already held the session slot.
    ActiveTurnPresent {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// The caller observed stale defaults.
    DefaultsVersionMismatch {
        /// Target session.
        session_id: CanonicalUuid,
        /// Caller version.
        expected: CanonicalU64,
        /// Current authoritative version.
        current: CanonicalU64,
    },
    /// The selected alias had no current definition.
    UnknownModelAlias {
        /// Target session.
        session_id: CanonicalUuid,
        /// Unknown alias.
        alias_id: CanonicalUuid,
    },
    /// The session acceptance ordinal was exhausted.
    AcceptancePositionExhausted {
        /// Target session.
        session_id: CanonicalUuid,
        /// Last representable position.
        last: CanonicalU64,
    },
}

/// Presence-checked rejection detail on an error message.
///
/// An absent value omits the JSON member. A present JSON `null` is rejected
/// rather than being treated as absence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ErrorDetail(Option<RejectionDetail>);

impl ErrorDetail {
    /// Omits rejection detail from a non-rejection error.
    pub const fn none() -> Self {
        Self(None)
    }

    /// Includes exact durable-rejection detail.
    pub const fn rejected(detail: RejectionDetail) -> Self {
        Self(Some(detail))
    }

    /// Returns the typed rejection detail when present.
    pub const fn value(self) -> Option<RejectionDetail> {
        self.0
    }

    const fn is_absent(&self) -> bool {
        self.0.is_none()
    }
}

impl Serialize for ErrorDetail {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        match self.0 {
            Some(detail) => detail.serialize(serializer),
            None => serializer.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for ErrorDetail {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        RejectionDetail::deserialize(deserializer).map(Self::rejected)
    }
}

/// Durable nonterminal model-call state carried by a transcript snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CurrentModelCallState {
    /// Call is prepared but unsent.
    Prepared {},
    /// Call crossed the send boundary.
    InFlight {},
}

/// Current model call attached to one running turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentModelCall {
    model_call_id: CanonicalUuid,
    state: CurrentModelCallState,
}

impl CurrentModelCall {
    /// Constructs one exact current-call projection.
    pub const fn new(model_call_id: CanonicalUuid, state: CurrentModelCallState) -> Self {
        Self {
            model_call_id,
            state,
        }
    }

    /// Returns the current model-call identity.
    pub const fn model_call_id(&self) -> CanonicalUuid {
        self.model_call_id
    }

    /// Returns the exact durable nonterminal state.
    pub const fn state(&self) -> CurrentModelCallState {
        self.state
    }
}

/// Authoritative turn state carried by a transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnState {
    /// Accepted work has not activated.
    Queued {
        /// Accepted input that created the queued turn.
        accepted_input_id: CanonicalUuid,
        /// Exact accepted owner text.
        content: InputContent,
    },
    /// The turn is running its current attempt.
    ActiveRunning {
        /// Current live attempt.
        current_attempt_id: CanonicalUuid,
        /// Current provider call, or null before one is prepared.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        current_model_call: Option<CurrentModelCall>,
    },
    /// The turn is parked on an ambiguous model call.
    ActiveAwaitingModelCallRecovery {
        /// Ended attempt that issued the call.
        ended_attempt_id: CanonicalUuid,
        /// Ambiguous call awaiting recovery.
        recovery_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized as failed.
    Failed {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// The turn terminalized as completed.
    Completed {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Outcome-authoritative call.
        terminal_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized as refused.
    Refused {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Outcome-authoritative call.
        terminal_model_call_id: CanonicalUuid,
    },
}

/// Non-text semantic transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptEntry {
    /// Explicit completed-turn marker.
    TurnCompleted {
        /// Completed turn.
        turn_id: CanonicalUuid,
    },
    /// Explicit failed-turn marker.
    TurnFailed {
        /// Failed turn.
        turn_id: CanonicalUuid,
    },
}

/// Metadata for a text-bearing semantic transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptTextEntry {
    /// Owner input text.
    User {
        /// Exact accepted input.
        accepted_input_id: CanonicalUuid,
        /// Origin turn.
        turn_id: CanonicalUuid,
    },
    /// Committed assistant text.
    Assistant {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
    },
}

/// Durable model-call terminal disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallDisposition {
    /// Provider call completed.
    Completed,
    /// Call failed with definitive evidence.
    KnownFailed,
    /// Provider refused.
    Refused,
    /// Call was cancelled.
    Cancelled,
    /// External outcome is ambiguous.
    Ambiguous,
}

/// Durable model-call state carried by a session event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelCallState {
    /// Call is prepared but unsent.
    Prepared {},
    /// Call crossed the send boundary.
    InFlight {},
    /// Call reached a terminal disposition.
    Terminal {
        /// Exact terminal disposition.
        disposition: ModelCallDisposition,
    },
}

/// Closed durable update event family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEvent {
    /// Session creation committed.
    SessionCreated {},
    /// Owner input acceptance and its queued turn committed.
    InputAccepted {
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Queued origin turn.
        turn_id: CanonicalUuid,
        /// Immutable session acceptance position.
        acceptance_position: CanonicalU64,
        /// Exact accepted owner text.
        content: InputContent,
    },
    /// A queued turn became active.
    TurnActivated {
        /// Activated turn.
        turn_id: CanonicalUuid,
        /// Initial current attempt.
        current_attempt_id: CanonicalUuid,
    },
    /// Model call advanced.
    ModelCallTransition {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Advancing call.
        model_call_id: CanonicalUuid,
        /// Exact committed state.
        state: ModelCallState,
    },
    /// Turn completed.
    TurnCompleted {
        /// Completed turn.
        turn_id: CanonicalUuid,
        /// Outcome-authoritative call.
        model_call_id: CanonicalUuid,
        /// Final completion marker.
        completion_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn failed.
    TurnFailed {
        /// Failed turn.
        turn_id: CanonicalUuid,
        /// Failure marker.
        failure_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn was refused.
    TurnRefused {
        /// Refused turn.
        turn_id: CanonicalUuid,
        /// Outcome-authoritative call.
        model_call_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
}

/// Closed version-one server message family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    /// Session creation receipt.
    SessionCreated {
        /// Created session.
        session_id: CanonicalUuid,
    },
    /// Input acceptance receipt.
    InputSubmitted {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Immutable acceptance position.
        acceptance_position: CanonicalU64,
        /// Created origin turn.
        turn_id: CanonicalUuid,
    },
    /// Begins a session-summary sequence.
    SessionsStart {},
    /// One current session summary.
    SessionSummary {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Current defaults version.
        defaults_version: CanonicalU64,
        /// Current model-selection request.
        model_selection: ModelSelection,
    },
    /// Completes a session-summary sequence.
    SessionsEnd {
        /// Number of preceding summaries.
        session_count: CanonicalU64,
    },
    /// Begins one transcript snapshot sequence.
    TranscriptSnapshotStart {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Snapshot outbox cursor.
        cursor: CanonicalU64,
    },
    /// One authoritative turn projection.
    TranscriptTurn {
        /// Immutable turn identity.
        turn_id: CanonicalUuid,
        /// Immutable acceptance order.
        acceptance_position: CanonicalU64,
        /// Exact lifecycle state.
        state: TurnState,
    },
    /// One non-text frontier member.
    TranscriptEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact marker payload.
        entry: TranscriptEntry,
    },
    /// Begins one text-bearing frontier member.
    TranscriptTextEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact text-entry metadata.
        entry: TranscriptTextEntry,
    },
    /// One bounded text fragment.
    TranscriptContent {
        /// Frontier member index.
        entry_index: CanonicalU64,
        /// Zero-based fragment index.
        fragment_index: CanonicalU64,
        /// Whether this is the entry's final fragment.
        final_fragment: bool,
        /// Exact content fragment.
        content_fragment: ContentFragment,
    },
    /// Completes one transcript snapshot.
    TranscriptSnapshotEnd {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Snapshot outbox cursor.
        cursor: CanonicalU64,
        /// Number of preceding turn messages.
        turn_count: CanonicalU64,
        /// Number of complete semantic entries.
        entry_count: CanonicalU64,
    },
    /// One committed update after a follow snapshot.
    SessionEvent {
        /// Global durable cursor.
        cursor: CanonicalU64,
        /// Owning session.
        session_id: CanonicalUuid,
        /// Exact typed update.
        event: SessionEvent,
    },
    /// Stable, sanitized failure.
    Error {
        /// Stable error code.
        code: ErrorCode,
        /// Non-sensitive human diagnostic.
        message: String,
        /// Required only for a durable command rejection.
        #[serde(default, skip_serializing_if = "ErrorDetail::is_absent")]
        detail: ErrorDetail,
    },
}

fn deserialize_required_nullable<'de, DeserializerT, ValueT>(
    deserializer: DeserializerT,
) -> Result<Option<ValueT>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
    ValueT: Deserialize<'de>,
{
    Option::<ValueT>::deserialize(deserializer)
}

/// One validated server frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFrame {
    version: u64,
    request_id: RequestId,
    message: ServerMessage,
}

impl ServerFrame {
    /// Constructs a version-one response frame.
    pub fn try_new(
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version: PROTOCOL_VERSION,
            request_id,
            message,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the request correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Borrows the closed server message.
    pub const fn message(&self) -> &ServerMessage {
        &self.message
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if self.version != PROTOCOL_VERSION {
            return Err(FrameValidationError::UnsupportedVersion);
        }
        match &self.message {
            ServerMessage::Error { code, detail, .. } => {
                if !self.request_id.is_correlated()
                    && !matches!(
                        code,
                        ErrorCode::MalformedFrame | ErrorCode::UnsupportedVersion
                    )
                {
                    return Err(FrameValidationError::UncorrelatedApplicationError);
                }
                if (*code == ErrorCode::Rejected) != detail.value().is_some() {
                    return Err(FrameValidationError::ErrorDetailShape);
                }
            }
            _ if !self.request_id.is_correlated() => {
                return Err(FrameValidationError::UncorrelatedSuccess);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerFrame {
    version: u64,
    request_id: RequestId,
    message: ServerMessage,
}

impl<'de> Deserialize<'de> for ServerFrame {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawServerFrame::deserialize(deserializer)?;
        let frame = Self {
            version: raw.version,
            request_id: raw.request_id,
            message: raw.message,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

/// A structurally invalid frame value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameValidationError {
    /// In-memory frame used another version.
    UnsupportedVersion,
    /// A client request used reserved correlation identity zero.
    UncorrelatedClientRequest,
    /// A success response used reserved correlation identity zero.
    UncorrelatedSuccess,
    /// A non-framing error used reserved correlation identity zero.
    UncorrelatedApplicationError,
    /// Rejection detail did not match the error code.
    ErrorDetailShape,
}

impl fmt::Display for FrameValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedVersion => "frame version is unsupported",
            Self::UncorrelatedClientRequest => "client request identity is uncorrelated",
            Self::UncorrelatedSuccess => "successful server message is uncorrelated",
            Self::UncorrelatedApplicationError => "application server error is uncorrelated",
            Self::ErrorDetailShape => "server error detail does not match its code",
        })
    }
}

impl Error for FrameValidationError {}

/// Stable classification of an incoming line failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameDecodeErrorKind {
    /// Frame exceeded the inclusive byte cap.
    OversizedFrame,
    /// Framing, JSON, field, or canonical scalar validation failed.
    MalformedFrame,
    /// Frame named another integer version.
    UnsupportedVersion,
}

/// Incoming-line failure with the recoverable request identity, or zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameDecodeError {
    kind: FrameDecodeErrorKind,
    request_id: RequestId,
}

impl FrameDecodeError {
    /// Returns the stable failure classification.
    pub const fn kind(&self) -> FrameDecodeErrorKind {
        self.kind
    }

    /// Returns the recovered request identity or reserved zero.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    const fn malformed(request_id: RequestId) -> Self {
        Self {
            kind: FrameDecodeErrorKind::MalformedFrame,
            request_id,
        }
    }
}

impl fmt::Display for FrameDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            FrameDecodeErrorKind::OversizedFrame => {
                formatter.write_str("process-protocol frame is oversized")
            }
            FrameDecodeErrorKind::MalformedFrame => {
                formatter.write_str("process-protocol frame is malformed")
            }
            FrameDecodeErrorKind::UnsupportedVersion => formatter
                .write_str("process-protocol version is unsupported; the supported version is 1"),
        }
    }
}

impl Error for FrameDecodeError {}

/// Outgoing frame could not be encoded within the version-one boundary.
#[derive(Debug)]
pub enum FrameEncodeError {
    /// In-memory value violated its closed frame shape.
    Validation(FrameValidationError),
    /// JSON serialization failed.
    Json(serde_json::Error),
    /// Encoded frame exceeded the inclusive byte cap.
    OversizedFrame,
}

impl fmt::Display for FrameEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => write!(formatter, "invalid process-protocol frame: {error}"),
            Self::Json(_) => formatter.write_str("process-protocol frame serialization failed"),
            Self::OversizedFrame => formatter.write_str("process-protocol frame is oversized"),
        }
    }
}

impl Error for FrameEncodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Validation(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::OversizedFrame => None,
        }
    }
}

impl From<FrameValidationError> for FrameEncodeError {
    fn from(error: FrameValidationError) -> Self {
        Self::Validation(error)
    }
}

impl From<serde_json::Error> for FrameEncodeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Decodes and validates one complete client line including its final newline.
pub fn decode_client_line(line: &[u8]) -> Result<ClientFrame, FrameDecodeError> {
    let content = checked_line_content(line, false)?;
    let header = probe_header(content, "request", false)?;
    let frame: ClientFrame = serde_json::from_slice(content)
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    frame
        .validate()
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    Ok(frame)
}

/// Decodes and validates one complete server line including its final newline.
pub fn decode_server_line(line: &[u8]) -> Result<ServerFrame, FrameDecodeError> {
    let content = checked_line_content(line, true)?;
    let header = probe_header(content, "message", true)?;
    let frame: ServerFrame = serde_json::from_slice(content)
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    frame
        .validate()
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    Ok(frame)
}

/// Encodes one validated client frame with its final newline.
pub fn encode_client_line(frame: &ClientFrame) -> Result<Vec<u8>, FrameEncodeError> {
    frame.validate()?;
    encode_line(frame)
}

/// Encodes one validated server frame with its final newline.
pub fn encode_server_line(frame: &ServerFrame) -> Result<Vec<u8>, FrameEncodeError> {
    frame.validate()?;
    encode_line(frame)
}

fn encode_line<T: Serialize>(frame: &T) -> Result<Vec<u8>, FrameEncodeError> {
    let mut encoded = serde_json::to_vec(frame)?;
    encoded.push(b'\n');
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(FrameEncodeError::OversizedFrame);
    }
    Ok(encoded)
}

fn checked_line_content(line: &[u8], allow_uncorrelated: bool) -> Result<&[u8], FrameDecodeError> {
    if line.len() > MAX_FRAME_BYTES {
        return Err(FrameDecodeError {
            kind: FrameDecodeErrorKind::OversizedFrame,
            request_id: RequestId::uncorrelated(),
        });
    }
    let Some(content) = line.strip_suffix(b"\n") else {
        return Err(FrameDecodeError::malformed(recover_request_id(
            line,
            allow_uncorrelated,
        )));
    };
    if content.is_empty() || content.ends_with(b"\r") || content.contains(&b'\n') {
        return Err(FrameDecodeError::malformed(recover_request_id(
            content,
            allow_uncorrelated,
        )));
    }
    Ok(content)
}

struct ProbedHeader {
    request_id: RequestId,
}

struct RawHeaderProbe<'a> {
    members: HashSet<String>,
    duplicate_member: bool,
    duplicate_request_id: bool,
    version: Option<&'a RawValue>,
    request_id: Option<&'a RawValue>,
}

struct RawHeaderProbeVisitor;

impl<'de> Visitor<'de> for RawHeaderProbeVisitor {
    type Value = RawHeaderProbe<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a process-protocol frame object")
    }

    fn visit_map<AccessT>(self, mut map: AccessT) -> Result<Self::Value, AccessT::Error>
    where
        AccessT: MapAccess<'de>,
    {
        let mut members = HashSet::new();
        let mut duplicate_member = false;
        let mut duplicate_request_id = false;
        let mut version = None;
        let mut request_id = None;

        while let Some(member) = map.next_key::<String>()? {
            let value = map.next_value::<&'de RawValue>()?;
            if !members.insert(member.clone()) {
                duplicate_member = true;
                duplicate_request_id |= member == "request_id";
                continue;
            }
            match member.as_str() {
                "version" => version = Some(value),
                "request_id" => request_id = Some(value),
                _ => {}
            }
        }

        Ok(RawHeaderProbe {
            members,
            duplicate_member,
            duplicate_request_id,
            version,
            request_id,
        })
    }
}

impl<'de> Deserialize<'de> for RawHeaderProbe<'de> {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        deserializer.deserialize_map(RawHeaderProbeVisitor)
    }
}

fn probe_header(
    content: &[u8],
    payload_member: &str,
    allow_uncorrelated: bool,
) -> Result<ProbedHeader, FrameDecodeError> {
    let probe = deserialize_header_probe(content)
        .map_err(|_| FrameDecodeError::malformed(RequestId::uncorrelated()))?;
    let request_id = request_id_from_probe(&probe, allow_uncorrelated);
    if probe.duplicate_member {
        return Err(FrameDecodeError::malformed(request_id));
    }
    let Some(version) = probe.version else {
        return Err(FrameDecodeError::malformed(request_id));
    };
    let version_spelling = version.get();
    let integer_spelling = version_spelling
        .strip_prefix('-')
        .unwrap_or(version_spelling);
    if integer_spelling.is_empty() || !integer_spelling.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(FrameDecodeError::malformed(request_id));
    }
    if version_spelling != "1" {
        return Err(FrameDecodeError {
            kind: FrameDecodeErrorKind::UnsupportedVersion,
            request_id,
        });
    }
    if probe.members.len() != 3
        || !probe.members.contains("version")
        || !probe.members.contains("request_id")
        || !probe.members.contains(payload_member)
    {
        return Err(FrameDecodeError::malformed(request_id));
    }
    Ok(ProbedHeader { request_id })
}

fn deserialize_header_probe(content: &[u8]) -> Result<RawHeaderProbe<'_>, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(content);
    RawHeaderProbe::deserialize(&mut deserializer).and_then(|probe| {
        deserializer.end()?;
        Ok(probe)
    })
}

fn request_id_from_probe(probe: &RawHeaderProbe<'_>, allow_uncorrelated: bool) -> RequestId {
    if probe.duplicate_request_id {
        RequestId::uncorrelated()
    } else {
        probe
            .request_id
            .and_then(|value| serde_json::from_str::<String>(value.get()).ok())
            .and_then(|value| RequestId::try_from(value).ok())
            .filter(|value| allow_uncorrelated || value.is_correlated())
            .unwrap_or_else(RequestId::uncorrelated)
    }
}

fn recover_request_id(content: &[u8], allow_uncorrelated: bool) -> RequestId {
    deserialize_header_probe(content)
        .map(|probe| request_id_from_probe(&probe, allow_uncorrelated))
        .unwrap_or_else(|_| RequestId::uncorrelated())
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, CommandId, ContentFragment,
        CurrentModelCall, CurrentModelCallState, ErrorCode, ErrorDetail, FrameDecodeErrorKind,
        FrameEncodeError, InputContent, MAX_CONTENT_FRAGMENT_BYTES, ModelCallDisposition,
        ModelCallState, ModelSelection, PROTOCOL_VERSION, RejectionDetail, RequestId, ServerFrame,
        ServerMessage, SessionEvent, TranscriptEntry, TranscriptTextEntry, TurnState,
        decode_client_line, decode_server_line, encode_client_line, encode_server_line,
    };
    use uuid::Uuid;

    fn command(value: u128) -> Result<CommandId, Box<dyn std::error::Error>> {
        Ok(CommandId::try_from_uuid(Uuid::from_u128(value))?)
    }

    fn request(value: u64) -> Result<RequestId, Box<dyn std::error::Error>> {
        Ok(RequestId::try_new(value)?)
    }

    fn uuid(value: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(value))
    }

    fn line(json: &str) -> Vec<u8> {
        let mut bytes = json.as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    }

    #[track_caller]
    fn assert_client_malformed(json: &str) {
        let error = decode_client_line(&line(json)).expect_err("client frame must be malformed");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
    }

    #[track_caller]
    fn assert_server_malformed(json: &str) {
        let error = decode_server_line(&line(json)).expect_err("server frame must be malformed");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
    }

    #[track_caller]
    fn assert_unsupported_version(version: &str) {
        let json = format!(
            "{{\"version\":{version},\"request_id\":\"9\",\"request\":{{\"type\":\"future_request\",\"anything\":true}}}}"
        );
        let error = decode_client_line(&line(&json)).expect_err("version must be unsupported");
        assert_eq!(error.kind(), FrameDecodeErrorKind::UnsupportedVersion);
        assert_eq!(error.request_id().value(), 9);
    }

    #[track_caller]
    fn assert_command_sentinel_rejected(command_id: &str) {
        let json = format!(
            "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"create_session\",\"command_id\":\"{command_id}\",\"initial_model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000001\"}}}}}}"
        );
        assert_client_malformed(&json);
    }

    #[track_caller]
    fn assert_client_request_version_one(
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new(request_id, request)?;
        let encoded = String::from_utf8(encode_client_line(&frame)?)?;
        assert!(encoded.starts_with(&format!("{{\"version\":{PROTOCOL_VERSION},")));
        Ok(())
    }

    #[track_caller]
    fn assert_server_message_round_trip(
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new(request_id, message)?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_client_round_trip_preserves_closed_request_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new(
            request(u64::MAX)?,
            ClientRequest::SubmitInput {
                command_id: command(1)?,
                session_id: uuid(2),
                content: InputContent::new("hello".to_owned()),
                expected_defaults_version: CanonicalU64::new(u64::MAX),
            },
        )?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(decode_client_line(&encoded)?, frame);
        assert!(String::from_utf8(encoded)?.contains("\"request_id\":\"18446744073709551615\""));
        Ok(())
    }

    #[test]
    fn inv033_unknown_missing_and_wrong_typed_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_sessions","extra":true}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"read_transcript"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":1,"request":{"type":"list_sessions"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"future_request"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_sessions"},"extra":true}"#,
        );
    }

    #[test]
    fn inv033_unsupported_version_precedes_payload_decoding() {
        assert_unsupported_version("-1");
        assert_unsupported_version("2");
        assert_unsupported_version("18446744073709551616");
        assert_client_malformed(
            r#"{"version":1.0,"request_id":"9","request":{"type":"list_sessions"}}"#,
        );
        let nested_payload = format!("{}0{}", "[".repeat(200), "]".repeat(200));
        let future = format!(
            "{{\"version\":2,\"request_id\":\"9\",\"request\":{nested_payload},\"new_v2_field\":true}}"
        );
        let error =
            decode_client_line(&line(&future)).expect_err("future version must be rejected first");
        assert_eq!(error.kind(), FrameDecodeErrorKind::UnsupportedVersion);
        assert_eq!(error.request_id().value(), 9);
    }

    #[test]
    fn inv033_duplicate_top_level_members_are_malformed_before_classification() {
        let duplicate_version = decode_client_line(&line(
            r#"{"version":1,"version":2,"request_id":"9","request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("a duplicate version is malformed");
        assert_eq!(
            duplicate_version.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(duplicate_version.request_id().value(), 9);

        let reversed_version = decode_client_line(&line(
            r#"{"version":2,"version":1,"request_id":"9","request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("version order cannot alter duplicate classification");
        assert_eq!(
            reversed_version.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(reversed_version.request_id().value(), 9);

        let duplicate_request = decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request_id":"2","request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("a duplicate request identity is malformed");
        assert_eq!(
            duplicate_request.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(duplicate_request.request_id().value(), 0);

        let duplicate_payload = decode_client_line(&line(
            r#"{"version":1,"request_id":"9","request":{"type":"list_sessions"},"request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("a duplicate payload is malformed");
        assert_eq!(
            duplicate_payload.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(duplicate_payload.request_id().value(), 9);
    }

    #[test]
    fn inv033_nested_unit_shapes_reject_unknown_members() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"sessions_start","extra":true}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":"queued","extra":true}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"session_created","extra":true}}}"#,
        );
    }

    #[test]
    fn inv033_active_running_requires_current_model_call_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000002"}}}"#,
        );
    }

    #[test]
    fn inv033_canonical_decimal_and_uuid_spellings_are_required() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"01","request":{"type":"list_sessions"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"+1","request":{"type":"list_sessions"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"read_transcript","session_id":"00000000-0000-0000-0000-00000000000A"}}"#,
        );
    }

    #[test]
    fn inv012_command_sentinels_and_zero_client_request_id_are_rejected() {
        assert_command_sentinel_rejected("00000000-0000-0000-0000-000000000000");
        assert_command_sentinel_rejected("ffffffff-ffff-ffff-ffff-ffffffffffff");
        assert!(
            decode_client_line(&line(
                r#"{"version":1,"request_id":"0","request":{"type":"list_sessions"}}"#
            ))
            .is_err()
        );
    }

    #[test]
    fn inv033_rejection_detail_shape_is_closed_and_code_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!(
            ServerFrame::try_new(
                request(1)?,
                ServerMessage::Error {
                    code: ErrorCode::Rejected,
                    message: "rejected".to_owned(),
                    detail: ErrorDetail::none(),
                },
            )
            .is_err()
        );
        assert!(
            ServerFrame::try_new(
                request(1)?,
                ServerMessage::Error {
                    code: ErrorCode::Internal,
                    message: "failed".to_owned(),
                    detail: ErrorDetail::rejected(RejectionDetail::SessionNotFound {
                        session_id: uuid(2),
                    }),
                },
            )
            .is_err()
        );
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: "rejected".to_owned(),
                detail: ErrorDetail::rejected(RejectionDetail::SessionNotFound {
                    session_id: uuid(2),
                }),
            },
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
        assert!(decode_server_line(&line(
            r#"{"version":1,"request_id":"1","message":{"type":"error","code":"internal","message":"failed","detail":null}}"#
        ))
        .is_err());
        Ok(())
    }

    #[test]
    fn inv033_uncorrelated_identity_is_reserved_for_server_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let error = ServerFrame::try_new(
            RequestId::uncorrelated(),
            ServerMessage::Error {
                code: ErrorCode::MalformedFrame,
                message: "malformed".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&error)?)?, error);
        let version_error = ServerFrame::try_new(
            RequestId::uncorrelated(),
            ServerMessage::Error {
                code: ErrorCode::UnsupportedVersion,
                message: "unsupported version".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        assert_eq!(
            decode_server_line(&encode_server_line(&version_error)?)?,
            version_error
        );
        assert!(
            ServerFrame::try_new(
                RequestId::uncorrelated(),
                ServerMessage::Error {
                    code: ErrorCode::NotFound,
                    message: "not found".to_owned(),
                    detail: ErrorDetail::none(),
                },
            )
            .is_err()
        );
        assert!(
            ServerFrame::try_new(RequestId::uncorrelated(), ServerMessage::SessionsStart {},)
                .is_err()
        );
        assert!(
            serde_json::from_str::<ClientFrame>(
                r#"{"version":1,"request_id":"0","request":{"type":"list_sessions"}}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ServerFrame>(
                r#"{"version":1,"request_id":"0","message":{"type":"sessions_start"}}"#
            )
            .is_err()
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"0","message":{"type":"error","code":"not_found","message":"not found"}}"#,
        );
        Ok(())
    }

    #[test]
    fn inv033_fragment_bound_keeps_worst_case_json_below_frame_cap()
    -> Result<(), Box<dyn std::error::Error>> {
        let fragment = ContentFragment::try_new("\u{1}".repeat(MAX_CONTENT_FRAGMENT_BYTES))?;
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(u64::MAX),
                fragment_index: CanonicalU64::new(u64::MAX),
                final_fragment: true,
                content_fragment: fragment,
            },
        )?;
        let encoded = encode_server_line(&frame)?;
        assert!(encoded.len() < super::MAX_FRAME_BYTES);
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn s24_content_fragmentation_preserves_empty_and_multibyte_text_exactly() {
        let empty = super::content_fragments("").collect::<Vec<_>>();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].as_str(), "");

        let text = format!(
            "{}\u{1f980}tail",
            "a".repeat(MAX_CONTENT_FRAGMENT_BYTES - 1)
        );
        let fragments = super::content_fragments(&text).collect::<Vec<_>>();
        assert_eq!(fragments.len(), 2);
        assert!(
            fragments
                .iter()
                .all(|fragment| fragment.as_str().len() <= MAX_CONTENT_FRAGMENT_BYTES)
        );
        assert_eq!(
            fragments
                .iter()
                .map(|fragment| fragment.as_str())
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn inv033_oversized_outgoing_frame_fails_explicitly() -> Result<(), Box<dyn std::error::Error>>
    {
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::Internal,
                message: "x".repeat(super::MAX_FRAME_BYTES),
                detail: ErrorDetail::none(),
            },
        )?;
        assert!(matches!(
            encode_server_line(&frame),
            Err(FrameEncodeError::OversizedFrame)
        ));
        Ok(())
    }

    #[test]
    fn inv033_exact_newline_framing_and_size_are_enforced() -> Result<(), Box<dyn std::error::Error>>
    {
        let frame = ClientFrame::try_new(request(1)?, ClientRequest::ListSessions {})?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(encoded.last(), Some(&b'\n'));
        let missing_newline = decode_client_line(&encoded[..encoded.len() - 1])
            .expect_err("missing newline must remain a malformed frame");
        assert_eq!(missing_newline.kind(), FrameDecodeErrorKind::MalformedFrame);
        assert_eq!(missing_newline.request_id().value(), 1);
        let mut carriage_return = encoded[..encoded.len() - 1].to_vec();
        carriage_return.extend_from_slice(b"\r\n");
        let carriage_return =
            decode_client_line(&carriage_return).expect_err("CRLF must remain malformed");
        assert_eq!(carriage_return.kind(), FrameDecodeErrorKind::MalformedFrame);
        assert_eq!(carriage_return.request_id().value(), 1);
        let mut multiline = encoded.clone();
        multiline.insert(1, b'\n');
        let multiline = decode_client_line(&multiline).expect_err("embedded LF must be malformed");
        assert_eq!(multiline.kind(), FrameDecodeErrorKind::MalformedFrame);
        assert_eq!(multiline.request_id().value(), 1);
        assert!(decode_client_line(&vec![b' '; super::MAX_FRAME_BYTES + 1]).is_err());
        Ok(())
    }

    #[test]
    fn inv033_all_client_request_variants_encode_with_version_one()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = ModelSelection::Direct {
            selection_id: uuid(3),
        };
        assert_client_request_version_one(
            request(1)?,
            ClientRequest::CreateSession {
                command_id: command(4)?,
                initial_model_selection: model,
            },
        )?;
        assert_client_request_version_one(request(2)?, ClientRequest::ListSessions {})?;
        assert_client_request_version_one(
            request(3)?,
            ClientRequest::SubmitInput {
                command_id: command(5)?,
                session_id: uuid(6),
                content: InputContent::new(String::new()),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )?;
        assert_client_request_version_one(
            request(4)?,
            ClientRequest::ReadTranscript {
                session_id: uuid(6),
            },
        )?;
        assert_client_request_version_one(
            request(5)?,
            ClientRequest::FollowSession {
                session_id: uuid(6),
            },
        )?;
        Ok(())
    }

    #[test]
    fn submit_content_is_admitted_by_the_application_not_wire_decoding()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = "x".repeat(MAX_CONTENT_FRAGMENT_BYTES + 1);
        let frame = ClientFrame::try_new(
            request(1)?,
            ClientRequest::SubmitInput {
                command_id: command(5)?,
                session_id: uuid(6),
                content: InputContent::new(content),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )?;
        let encoded = encode_client_line(&frame)?;
        assert!(encoded.len() < super::MAX_FRAME_BYTES);
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_server_message_family_round_trips_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::SessionCreated {
                session_id: uuid(1),
            },
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::InputSubmitted {
                session_id: uuid(1),
                accepted_input_id: uuid(2),
                acceptance_position: CanonicalU64::new(1),
                turn_id: uuid(3),
            },
        )?;
        assert_server_message_round_trip(request(3)?, ServerMessage::SessionsStart {})?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::SessionSummary {
                session_id: uuid(1),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Alias { alias_id: uuid(4) },
            },
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::SessionsEnd {
                session_count: CanonicalU64::new(1),
            },
        )?;
        assert_server_message_round_trip(
            request(6)?,
            ServerMessage::TranscriptSnapshotStart {
                session_id: uuid(1),
                cursor: CanonicalU64::new(5),
            },
        )?;
        assert_server_message_round_trip(
            request(7)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Refused {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: uuid(8),
                },
            },
        )?;
        assert_server_message_round_trip(
            request(14)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Queued {
                    accepted_input_id: uuid(2),
                    content: InputContent::new("queued request".to_owned()),
                },
            },
        )?;
        assert_server_message_round_trip(
            request(15)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::ActiveRunning {
                    current_attempt_id: uuid(7),
                    current_model_call: None,
                },
            },
        )?;
        assert_server_message_round_trip(
            request(16)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::ActiveRunning {
                    current_attempt_id: uuid(7),
                    current_model_call: Some(CurrentModelCall::new(
                        uuid(8),
                        CurrentModelCallState::Prepared {},
                    )),
                },
            },
        )?;
        assert_server_message_round_trip(
            request(17)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::ActiveRunning {
                    current_attempt_id: uuid(7),
                    current_model_call: Some(CurrentModelCall::new(
                        uuid(8),
                        CurrentModelCallState::InFlight {},
                    )),
                },
            },
        )?;
        assert_server_message_round_trip(
            request(8)?,
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: uuid(1),
                entry_id: uuid(9),
                entry: TranscriptEntry::TurnCompleted { turn_id: uuid(3) },
            },
        )?;
        assert_server_message_round_trip(
            request(9)?,
            ServerMessage::TranscriptTextEntry {
                entry_index: CanonicalU64::new(1),
                source_session_id: uuid(1),
                entry_id: uuid(10),
                entry: TranscriptTextEntry::Assistant {
                    turn_id: uuid(3),
                    model_call_id: uuid(8),
                },
            },
        )?;
        assert_server_message_round_trip(
            request(10)?,
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(1),
                fragment_index: CanonicalU64::new(0),
                final_fragment: true,
                content_fragment: ContentFragment::try_new("reply".to_owned())?,
            },
        )?;
        assert_server_message_round_trip(
            request(11)?,
            ServerMessage::TranscriptSnapshotEnd {
                session_id: uuid(1),
                cursor: CanonicalU64::new(5),
                turn_count: CanonicalU64::new(1),
                entry_count: CanonicalU64::new(2),
            },
        )?;
        assert_server_message_round_trip(
            request(12)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(6),
                session_id: uuid(1),
                event: SessionEvent::ModelCallTransition {
                    turn_id: uuid(3),
                    model_call_id: uuid(8),
                    state: ModelCallState::Terminal {
                        disposition: ModelCallDisposition::Refused,
                    },
                },
            },
        )?;
        assert_server_message_round_trip(
            request(18)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(2),
                session_id: uuid(1),
                event: SessionEvent::InputAccepted {
                    accepted_input_id: uuid(2),
                    turn_id: uuid(3),
                    acceptance_position: CanonicalU64::new(1),
                    content: InputContent::new("accepted request".to_owned()),
                },
            },
        )?;
        assert_server_message_round_trip(
            request(19)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(3),
                session_id: uuid(1),
                event: SessionEvent::TurnActivated {
                    turn_id: uuid(3),
                    current_attempt_id: uuid(7),
                },
            },
        )?;
        assert_server_message_round_trip(
            request(13)?,
            ServerMessage::Error {
                code: ErrorCode::NotFound,
                message: "not found".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        Ok(())
    }
}

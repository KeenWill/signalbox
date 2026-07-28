//! Closed versioned JSON-lines process protocol.
//!
//! This crate owns wire representations and frame validation only. Domain,
//! persistence, and client presentation values remain distinct mappings
//! (docs/spec/process-protocol.md).

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    error::Error,
    fmt,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as STANDARD_BASE64};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use serde_json::value::RawValue;
use uuid::Uuid;

/// The baseline protocol version retained for existing clients.
pub const PROTOCOL_VERSION: u64 = 1;

/// The imported-transcript snapshot protocol version.
pub const IMPORTED_TRANSCRIPT_PROTOCOL_VERSION: u64 = 2;

/// The tool-bearing projection protocol version.
pub const TOOL_LOOP_PROTOCOL_VERSION: u64 = 3;

/// The session-metadata protocol version.
pub const SESSION_METADATA_PROTOCOL_VERSION: u64 = 4;

/// The one-source conversation-import protocol version.
pub const CONVERSATION_IMPORT_PROTOCOL_VERSION: u64 = 5;

/// The forward-only session-defaults replacement protocol version.
pub const MID_SESSION_MODEL_SELECTION_PROTOCOL_VERSION: u64 = 6;

/// The owner turn-reconciliation protocol version.
pub const TURN_RECONCILIATION_PROTOCOL_VERSION: u64 = 7;

/// The client turn-control protocol version.
pub const TURN_CONTROL_PROTOCOL_VERSION: u64 = 8;

/// The session system-prompt protocol version.
pub const SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION: u64 = 9;

/// The imported-frontier session-creation protocol version.
pub const IMPORTED_SESSION_CONTINUATION_PROTOCOL_VERSION: u64 = 10;

/// The review-workflow protocol version.
pub const REVIEW_WORKFLOW_PROTOCOL_VERSION: u64 = 11;

/// The ephemeral provider-text streaming protocol version.
pub const PROVIDER_TEXT_STREAMING_PROTOCOL_VERSION: u64 = 12;

/// The unified conversation-listing protocol version.
///
/// Versions thirteen and fourteen were reserved by concurrent protocol work,
/// and fifteen by the imported-conversation inspection stack, when this
/// version was selected.
pub const UNIFIED_CONVERSATION_LISTING_PROTOCOL_VERSION: u64 = 16;

/// One admitted process-protocol version.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProtocolVersion {
    /// Baseline native-only transcript vocabulary.
    One,
    /// Conservative imported-transcript snapshot vocabulary.
    Two,
    /// Tool-bearing projection vocabulary.
    Three,
    /// Session-metadata reads, pages, and replacement vocabulary.
    Four,
    /// One-source conversation-import vocabulary.
    Five,
    /// Forward-only session-defaults replacement vocabulary.
    Six,
    /// Owner turn-reconciliation vocabulary.
    Seven,
    /// Client turn-control vocabulary.
    Eight,
    /// Session system-prompt vocabulary.
    Nine,
    /// Imported-frontier session-creation vocabulary.
    Ten,
    /// Review-workflow command and read vocabulary.
    Eleven,
    /// Ephemeral provider-text presentation events on follow streams.
    Twelve,
    /// Unified conversation-listing vocabulary.
    Sixteen,
}

impl ProtocolVersion {
    /// Returns the integer carried in every wire frame.
    pub const fn as_u64(self) -> u64 {
        match self {
            Self::One => PROTOCOL_VERSION,
            Self::Two => IMPORTED_TRANSCRIPT_PROTOCOL_VERSION,
            Self::Three => TOOL_LOOP_PROTOCOL_VERSION,
            Self::Four => SESSION_METADATA_PROTOCOL_VERSION,
            Self::Five => CONVERSATION_IMPORT_PROTOCOL_VERSION,
            Self::Six => MID_SESSION_MODEL_SELECTION_PROTOCOL_VERSION,
            Self::Seven => TURN_RECONCILIATION_PROTOCOL_VERSION,
            Self::Eight => TURN_CONTROL_PROTOCOL_VERSION,
            Self::Nine => SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION,
            Self::Ten => IMPORTED_SESSION_CONTINUATION_PROTOCOL_VERSION,
            Self::Eleven => REVIEW_WORKFLOW_PROTOCOL_VERSION,
            Self::Twelve => PROVIDER_TEXT_STREAMING_PROTOCOL_VERSION,
            Self::Sixteen => UNIFIED_CONVERSATION_LISTING_PROTOCOL_VERSION,
        }
    }

    const fn from_u64(value: u64) -> Option<Self> {
        match value {
            PROTOCOL_VERSION => Some(Self::One),
            IMPORTED_TRANSCRIPT_PROTOCOL_VERSION => Some(Self::Two),
            TOOL_LOOP_PROTOCOL_VERSION => Some(Self::Three),
            SESSION_METADATA_PROTOCOL_VERSION => Some(Self::Four),
            CONVERSATION_IMPORT_PROTOCOL_VERSION => Some(Self::Five),
            MID_SESSION_MODEL_SELECTION_PROTOCOL_VERSION => Some(Self::Six),
            TURN_RECONCILIATION_PROTOCOL_VERSION => Some(Self::Seven),
            TURN_CONTROL_PROTOCOL_VERSION => Some(Self::Eight),
            SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION => Some(Self::Nine),
            IMPORTED_SESSION_CONTINUATION_PROTOCOL_VERSION => Some(Self::Ten),
            REVIEW_WORKFLOW_PROTOCOL_VERSION => Some(Self::Eleven),
            PROVIDER_TEXT_STREAMING_PROTOCOL_VERSION => Some(Self::Twelve),
            UNIFIED_CONVERSATION_LISTING_PROTOCOL_VERSION => Some(Self::Sixteen),
            _ => None,
        }
    }
}

impl Serialize for ProtocolVersion {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        serializer.serialize_u64(self.as_u64())
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::from_u64(value)
            .ok_or_else(|| serde::de::Error::custom("frame version is unsupported"))
    }
}

/// Maximum encoded frame size, including its final newline.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of simultaneously open JSON objects and arrays in one frame.
pub const MAX_JSON_CONTAINER_DEPTH: usize = 127;

/// Maximum UTF-8 bytes in one transcript content fragment.
pub const MAX_CONTENT_FRAGMENT_BYTES: usize = 1024 * 1024;

/// Maximum total UTF-8 bytes in one complete metadata object or filter.
pub const MAX_SESSION_METADATA_TOTAL_UTF8_BYTES: usize = 262_144;

/// Maximum UTF-8 bytes in one indexed metadata tag or attribute key.
pub const MAX_SESSION_METADATA_INDEXED_UTF8_BYTES: usize = 1_024;

/// Maximum exact tags in one complete metadata object.
pub const MAX_SESSION_METADATA_TAGS: usize = 256;

/// Maximum exact attributes in one complete metadata object.
pub const MAX_SESSION_METADATA_ATTRIBUTES: usize = 256;

/// Maximum exact required tags in one metadata-list filter.
pub const MAX_SESSION_METADATA_REQUIRED_TAGS: usize = 256;

/// Maximum UTF-8 bytes in one session system prompt.
pub const MAX_SYSTEM_PROMPT_UTF8_BYTES: usize = 1_048_576;

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

    /// Transfers ownership of the exact decoded text.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// One closed review target subject at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewTargetSubject {
    /// A change request frozen at exact head and base revisions.
    ChangeRequest {
        /// Positive provider-local change-request number.
        number: CanonicalU64,
    },
    /// One immutable commit revision.
    Commit {},
}

/// One immutable review target snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTargetSnapshot {
    /// Stable target identity.
    pub target_id: CanonicalUuid,
    /// Opaque canonical provider key.
    pub provider: String,
    /// Opaque canonical repository key.
    pub repository: String,
    /// Exact subject kind.
    pub subject: ReviewTargetSubject,
    /// Frozen head revision.
    pub head_revision: String,
    /// Frozen comparison revision when the subject has one.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub base_revision: Option<String>,
    /// Immediate stack parent snapshot when present.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub stack_parent_target_id: Option<CanonicalUuid>,
}

/// One admitted review workflow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewWorkflow {
    /// Import provider-side review context.
    ImportExternalContext,
    /// Produce findings without mutation.
    ReadOnlyReview,
    /// Judge proposed findings.
    JudgeFindings,
    /// Deduplicate proposed findings.
    DedupeFindings,
    /// Publish findings to the provider.
    PublishReview,
    /// Repair accepted findings.
    FixFindings,
    /// Propagate one reviewed stack edge.
    PropagateStack,
}

/// One review pass purpose.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPassKind {
    /// Import provider-side context.
    ImportExternalContext,
    /// Produce read-only findings.
    ReadOnlyReview,
    /// Judge findings.
    Judge,
    /// Deduplicate findings.
    Dedupe,
    /// Publish findings.
    Publish,
    /// Repair findings.
    Fix,
    /// Propagate one stack edge.
    PropagateStack,
}

/// One projected run lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRunLifecycle {
    /// Waiting for its pass turn.
    Queued,
    /// Its pass turn is active.
    Running,
    /// Its pass completed successfully.
    Succeeded,
    /// Its pass failed.
    Failed,
    /// Its pass needs external resolution.
    Blocked,
    /// It was cancelled.
    Cancelled,
}

/// One projected pass lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPassLifecycle {
    /// Accepted input exists but its turn is not active.
    Queued,
    /// The pass turn is active.
    Running,
    /// The pass turn completed successfully.
    Succeeded,
    /// The pass turn failed.
    Failed,
    /// The pass needs external resolution.
    Blocked,
    /// The pass was cancelled.
    Cancelled,
}

/// One complete review-run read projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRunSnapshot {
    /// Owning target.
    pub target_id: CanonicalUuid,
    /// Stable run identity.
    pub run_id: CanonicalUuid,
    /// Frozen workflow.
    pub workflow: ReviewWorkflow,
    /// Frozen policy version.
    pub policy_version: CanonicalU64,
    /// Minimum judgment confidence in basis points.
    pub minimum_judge_confidence: CanonicalU64,
    /// Minimum publication confidence in basis points.
    pub minimum_publication_confidence: CanonicalU64,
    /// Current lifecycle projection.
    pub state: ReviewRunLifecycle,
    /// The run's sole pass when admitted.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pass_id: Option<CanonicalUuid>,
}

/// One complete review-pass read projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPassSnapshot {
    /// Stable pass identity.
    pub pass_id: CanonicalUuid,
    /// Owning run.
    pub run_id: CanonicalUuid,
    /// Owning target.
    pub target_id: CanonicalUuid,
    /// Exact pass purpose.
    pub kind: ReviewPassKind,
    /// Bound session.
    pub session_id: CanonicalUuid,
    /// Bound accepted input.
    pub accepted_input_id: CanonicalUuid,
    /// Bound origin turn.
    pub origin_turn_id: CanonicalUuid,
    /// Current lifecycle projection.
    pub state: ReviewPassLifecycle,
    /// Exact active or terminal turn when present.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub turn_id: Option<CanonicalUuid>,
    /// Exact successful output frontier when present.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub output_frontier_id: Option<CanonicalUuid>,
}

/// Finding location side relative to a frozen comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDiffSide {
    /// Frozen base side.
    Left,
    /// Frozen head side.
    Right,
}

/// Finding severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    /// Informational observation.
    Info,
    /// Low-severity defect.
    Low,
    /// Medium-severity defect.
    Medium,
    /// High-severity defect.
    High,
    /// Critical defect.
    Critical,
}

/// Immutable finding content admitted with one read-only pass result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingInput {
    /// Stable finding identity.
    pub finding_id: CanonicalUuid,
    /// Exact repository-relative file path.
    pub file_path: String,
    /// Optional positive first line.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub line_start: Option<CanonicalU64>,
    /// Optional positive final line.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub line_end: Option<CanonicalU64>,
    /// Optional frozen diff side.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub diff_side: Option<ReviewDiffSide>,
    /// Short exact title.
    pub title: String,
    /// Exact explanatory body.
    pub body: String,
    /// Severity classification.
    pub severity: ReviewSeverity,
    /// Producer confidence in basis points.
    pub confidence: CanonicalU64,
    /// Opaque canonical category key.
    pub category: String,
    /// Optional exact recommended repair.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub recommended_fix: Option<String>,
}

/// Current finding lifecycle status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingStatus {
    /// Proposed and not yet judged.
    Open,
    /// Accepted by judgment.
    Accepted,
    /// Rejected by judgment.
    Rejected,
    /// Classified as a duplicate.
    Duplicate,
    /// Replaced by a later finding.
    Superseded,
    /// No longer applies.
    Stale,
    /// Published externally.
    Posted,
    /// Repaired.
    Fixed,
    /// Publication or repair was blocked.
    BlockedWithReason,
}

/// One complete finding read projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingSnapshot {
    /// Owning target.
    pub target_id: CanonicalUuid,
    /// Owning run.
    pub run_id: CanonicalUuid,
    /// Producing read-only pass.
    pub producing_pass_id: CanonicalUuid,
    /// Immutable content.
    pub finding: ReviewFindingInput,
    /// Current derived lifecycle status.
    pub status: ReviewFindingStatus,
    /// Number of committed lifecycle events.
    pub event_count: CanonicalU64,
}

/// A basic judgment disposition recorded by one judge pass.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewFindingDisposition {
    /// Accept the finding.
    Accepted {},
    /// Reject with an exact nonempty reason.
    Rejected { reason: String },
    /// Mark stale against the frozen target.
    Stale {},
}

/// Provider object kind reserved for one review aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewExternalObjectKind {
    /// Provider review object.
    Review,
    /// Provider review thread.
    ReviewThread,
    /// Provider inline review comment.
    ReviewComment,
    /// Provider change-request comment.
    ChangeRequestComment,
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

/// One exact bounded session system prompt on the wire.
///
/// A present prompt is nonempty, rejects U+0000, and carries at most
/// [`MAX_SYSTEM_PROMPT_UTF8_BYTES`] UTF-8 bytes; absence is JSON null on the
/// owning member, never empty text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SystemPromptText(String);

impl SystemPromptText {
    /// Applies the nonempty, U+0000-free, bounded-bytes admission rules.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        if value.is_empty() || value.len() > MAX_SYSTEM_PROMPT_UTF8_BYTES || value.contains('\0') {
            Err(CanonicalValueError::SystemPrompt)
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows the exact admitted prompt text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Transfers ownership of the exact admitted prompt text.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for SystemPromptText {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<SystemPromptText> for String {
    fn from(value: SystemPromptText) -> Self {
        value.0
    }
}

/// Presence-checked optional system-prompt member.
///
/// Versions nine and above require the member: JSON null states explicitly
/// that the complete defaults carry no prompt, and a string carries the exact
/// bounded prompt. Versions one through eight omit the member entirely; frame
/// validation rejects a present member below version nine and an absent
/// member at version nine and above.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemPromptMember(Option<Option<SystemPromptText>>);

impl SystemPromptMember {
    /// Omits the member for a frame at versions one through eight.
    pub const fn absent() -> Self {
        Self(None)
    }

    /// Carries the explicit null-or-text member required at versions nine and
    /// above.
    pub const fn present(value: Option<SystemPromptText>) -> Self {
        Self(Some(value))
    }

    /// Returns the explicit member when it was present.
    pub const fn value(&self) -> Option<&Option<SystemPromptText>> {
        self.0.as_ref()
    }

    const fn is_absent(&self) -> bool {
        self.0.is_none()
    }
}

impl Serialize for SystemPromptMember {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        match &self.0 {
            Some(Some(text)) => text.serialize(serializer),
            Some(None) | None => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for SystemPromptMember {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        Option::<SystemPromptText>::deserialize(deserializer).map(Self::present)
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
    /// Session metadata violated its exact string, set, map, or page bound.
    Metadata,
    /// A session system prompt was empty, contained U+0000, or exceeded its
    /// UTF-8 byte bound.
    SystemPrompt,
}

impl fmt::Display for CanonicalValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Uuid => "UUID is not canonical lowercase hyphenated text",
            Self::CommandId => "command identity is a reserved sentinel",
            Self::Decimal => "unsigned integer is not canonical decimal text",
            Self::RequestId => "client request identity must be nonzero",
            Self::Content => "content fragment exceeds the version-one UTF-8 byte bound",
            Self::Metadata => "session metadata value is invalid",
            Self::SystemPrompt => "session system prompt is empty, oversized, or contains U+0000",
        })
    }
}

impl Error for CanonicalValueError {}

/// Exact source format selected for one conversation import.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationImportFormat {
    /// Claude Code session JSONL under Signalbox converter version two.
    ClaudeCodeSessionJsonlV2,
    /// Codex rollout JSONL under Signalbox converter version one.
    CodexRolloutJsonlV1,
}

/// Exact caller-supplied source bytes carried as canonical padded base64.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationImportSource(Vec<u8>);

impl ConversationImportSource {
    /// Wraps one complete source snapshot without interpreting it.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Borrows the exact decoded source snapshot.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Transfers ownership of the exact decoded source snapshot.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl Serialize for ConversationImportSource {
    fn serialize<SerializerT>(
        &self,
        serializer: SerializerT,
    ) -> Result<SerializerT::Ok, SerializerT::Error>
    where
        SerializerT: Serializer,
    {
        serializer.serialize_str(&STANDARD_BASE64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for ConversationImportSource {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        struct ConversationImportSourceVisitor;

        impl Visitor<'_> for ConversationImportSourceVisitor {
            type Value = ConversationImportSource;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("canonical padded base64")
            }

            fn visit_str<ErrorT>(self, encoded: &str) -> Result<Self::Value, ErrorT>
            where
                ErrorT: serde::de::Error,
            {
                let decoded = STANDARD_BASE64.decode(encoded.as_bytes()).map_err(|_| {
                    serde::de::Error::custom("import source is not canonical base64")
                })?;
                Ok(ConversationImportSource(decoded))
            }
        }

        deserializer.deserialize_str(ConversationImportSourceVisitor)
    }
}

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

/// One exact complete session-metadata object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetadata {
    title: Option<String>,
    tags: Vec<String>,
    attributes: MetadataAttributes,
    archived: bool,
}

impl SessionMetadata {
    /// Validates and canonicalizes one complete metadata object.
    pub fn try_new(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
    ) -> Result<Self, CanonicalValueError> {
        let mut total_utf8_bytes = 0usize;
        if let Some(title) = title.as_deref() {
            validate_nonempty_metadata_text(title)?;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, title)?;
        }
        let tags = canonical_metadata_tags(tags, MAX_SESSION_METADATA_TAGS)?;
        for tag in &tags {
            add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)?;
        }
        let attributes = MetadataAttributes::try_new(attributes)?;
        for (key, value) in &attributes.0 {
            add_metadata_utf8_bytes(&mut total_utf8_bytes, key)?;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, value)?;
        }
        Ok(Self {
            title,
            tags,
            attributes,
            archived,
        })
    }

    /// Constructs the unwritten empty, non-archived object.
    pub fn empty() -> Self {
        Self {
            title: None,
            tags: Vec::new(),
            attributes: MetadataAttributes(BTreeMap::new()),
            archived: false,
        }
    }

    /// Borrows the optional exact title.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Iterates tags in exact scalar order.
    pub fn tags(&self) -> impl ExactSizeIterator<Item = &str> {
        self.tags.iter().map(String::as_str)
    }

    /// Iterates attributes in exact key scalar order.
    pub fn attributes(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.attributes
            .0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Returns whether the session is archived.
    pub const fn archived(&self) -> bool {
        self.archived
    }

    fn is_initial(&self) -> bool {
        self.title.is_none()
            && self.tags.is_empty()
            && self.attributes.0.is_empty()
            && !self.archived
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSessionMetadata {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    title: Option<String>,
    #[serde(deserialize_with = "deserialize_session_metadata_tags")]
    tags: Vec<String>,
    attributes: MetadataAttributes,
    archived: bool,
}

impl<'de> Deserialize<'de> for SessionMetadata {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawSessionMetadata::deserialize(deserializer)?;
        Self::try_new(
            raw.title,
            raw.tags,
            raw.attributes.0.into_iter().collect(),
            raw.archived,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
struct MetadataAttributes(BTreeMap<String, String>);

impl MetadataAttributes {
    fn try_new(values: Vec<(String, String)>) -> Result<Self, CanonicalValueError> {
        if values.len() > MAX_SESSION_METADATA_ATTRIBUTES {
            return Err(CanonicalValueError::Metadata);
        }
        let mut attributes = BTreeMap::new();
        for (key, value) in values {
            validate_nonempty_metadata_text(&key)?;
            validate_indexed_metadata_text(&key)?;
            validate_metadata_text(&value)?;
            if attributes.insert(key, value).is_some() {
                return Err(CanonicalValueError::Metadata);
            }
        }
        Ok(Self(attributes))
    }
}

struct MetadataAttributesVisitor;

impl<'de> Visitor<'de> for MetadataAttributesVisitor {
    type Value = MetadataAttributes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an exact session metadata attribute object")
    }

    fn visit_map<AccessT>(self, mut map: AccessT) -> Result<Self::Value, AccessT::Error>
    where
        AccessT: MapAccess<'de>,
    {
        let mut attributes = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, String>()? {
            if attributes.len() == MAX_SESSION_METADATA_ATTRIBUTES {
                return Err(serde::de::Error::custom(
                    "too many session metadata attributes",
                ));
            }
            validate_nonempty_metadata_text(&key).map_err(serde::de::Error::custom)?;
            validate_indexed_metadata_text(&key).map_err(serde::de::Error::custom)?;
            validate_metadata_text(&value).map_err(serde::de::Error::custom)?;
            if attributes.insert(key, value).is_some() {
                return Err(serde::de::Error::custom(
                    "duplicate session metadata attribute key",
                ));
            }
        }
        Ok(MetadataAttributes(attributes))
    }
}

impl<'de> Deserialize<'de> for MetadataAttributes {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        deserializer.deserialize_map(MetadataAttributesVisitor)
    }
}

fn validate_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.contains('\0') {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

fn validate_nonempty_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.is_empty() {
        Err(CanonicalValueError::Metadata)
    } else {
        validate_metadata_text(value)
    }
}

fn validate_indexed_metadata_text(value: &str) -> Result<(), CanonicalValueError> {
    if value.len() > MAX_SESSION_METADATA_INDEXED_UTF8_BYTES {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

fn add_metadata_utf8_bytes(total: &mut usize, value: &str) -> Result<(), CanonicalValueError> {
    *total = total.saturating_add(value.len());
    if *total > MAX_SESSION_METADATA_TOTAL_UTF8_BYTES {
        Err(CanonicalValueError::Metadata)
    } else {
        Ok(())
    }
}

fn canonical_metadata_tags(
    values: Vec<String>,
    maximum: usize,
) -> Result<Vec<String>, CanonicalValueError> {
    if values.len() > maximum {
        return Err(CanonicalValueError::Metadata);
    }
    let mut tags = BTreeSet::new();
    for tag in values {
        validate_nonempty_metadata_text(&tag)?;
        validate_indexed_metadata_text(&tag)?;
        if !tags.insert(tag) {
            return Err(CanonicalValueError::Metadata);
        }
    }
    Ok(tags.into_iter().collect())
}

struct MetadataTagsVisitor<const MAXIMUM: usize>;

impl<'de, const MAXIMUM: usize> Visitor<'de> for MetadataTagsVisitor<MAXIMUM> {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "at most {MAXIMUM} exact session metadata tag strings"
        )
    }

    fn visit_seq<AccessT>(self, mut sequence: AccessT) -> Result<Self::Value, AccessT::Error>
    where
        AccessT: SeqAccess<'de>,
    {
        let mut tags = Vec::with_capacity(sequence.size_hint().unwrap_or_default().min(MAXIMUM));
        while tags.len() < MAXIMUM {
            match sequence.next_element::<String>()? {
                Some(tag) => tags.push(tag),
                None => return Ok(tags),
            }
        }
        if sequence.next_element::<IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom("too many session metadata tags"));
        }
        Ok(tags)
    }
}

fn deserialize_bounded_metadata_tags<'de, DeserializerT, const MAXIMUM: usize>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    deserializer.deserialize_seq(MetadataTagsVisitor::<MAXIMUM>)
}

fn deserialize_session_metadata_tags<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    deserialize_bounded_metadata_tags::<DeserializerT, MAX_SESSION_METADATA_TAGS>(deserializer)
}

fn deserialize_required_metadata_tags<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    deserialize_bounded_metadata_tags::<DeserializerT, MAX_SESSION_METADATA_REQUIRED_TAGS>(
        deserializer,
    )
}

/// Closed actor provenance carried by a metadata last-writer stamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataActor {
    /// The owner wrote the snapshot.
    Owner {},
}

/// The post-lock database statement time and actor of the latest replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataLastWriter {
    updated_at_unix_micros: CanonicalU64,
    actor: MetadataActor,
}

/// How a new live session relates to one selected imported frontier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedSessionRelationship {
    /// Continue from the selected imported boundary.
    Resume,
    /// Branch from the selected imported boundary.
    Fork,
}

impl MetadataLastWriter {
    /// Constructs one exact last-writer stamp.
    pub const fn new(updated_at_unix_micros: CanonicalU64, actor: MetadataActor) -> Self {
        Self {
            updated_at_unix_micros,
            actor,
        }
    }

    /// Returns the nonnegative Unix-microsecond transaction timestamp.
    pub const fn updated_at_unix_micros(self) -> CanonicalU64 {
        self.updated_at_unix_micros
    }

    /// Returns the closed actor provenance.
    pub const fn actor(self) -> MetadataActor {
        self.actor
    }
}

/// Maximum Unicode scalars in one imported-conversation display title,
/// restating the domain derivation bound on the wire.
pub const MAX_IMPORTED_CONVERSATION_DISPLAY_TITLE_SCALARS: usize = 256;

/// One closed conversation origin class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationOrigin {
    /// A native session.
    NativeSession,
    /// An immutable imported conversation.
    ImportedConversation,
}

/// Which conversation origin classes one unified list request selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationOriginFilter {
    /// Native sessions only.
    Native,
    /// Imported conversations only.
    Imported,
    /// Both origin classes.
    All,
}

/// One exclusive unified keyset cursor naming the last listed conversation.
///
/// The unified page order is by conversation identity UUID value, native
/// before imported for a theoretical equal identity, so the cursor names one
/// total position across both origin classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationCursor {
    /// Origin class of the cursor position.
    origin: ConversationOrigin,
    /// Conversation identity at the cursor position.
    conversation_id: CanonicalUuid,
}

impl ConversationCursor {
    /// Names one exact unified cursor position.
    pub const fn new(origin: ConversationOrigin, conversation_id: CanonicalUuid) -> Self {
        Self {
            origin,
            conversation_id,
        }
    }

    /// Returns the origin class of the cursor position.
    pub const fn origin(self) -> ConversationOrigin {
        self.origin
    }

    /// Returns the conversation identity at the cursor position.
    pub const fn conversation_id(self) -> CanonicalUuid {
        self.conversation_id
    }
}

/// One exact stored imported source format and converter version.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedConversationSourceFormat {
    /// Claude Code session JSONL interpreted by converter version 1.
    ClaudeCodeSessionJsonlV1,
    /// Claude Code session JSONL interpreted by converter version 2.
    ClaudeCodeSessionJsonlV2,
    /// Codex rollout JSONL interpreted by converter version 1.
    CodexRolloutJsonlV1,
}

/// One closed per-origin unified conversation summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationSummary {
    /// One native session with its current organizational facts.
    NativeSession {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Exact optional metadata title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Whether the session is archived.
        archived: bool,
        /// Current defaults version.
        defaults_version: CanonicalU64,
    },
    /// One immutable imported conversation snapshot.
    ImportedConversation {
        /// Imported conversation identity.
        imported_conversation_id: CanonicalUuid,
        /// Exact optional source-derived display title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Total normalized entry count; the greatest position a
        /// continuation may select.
        entry_count: CanonicalU64,
        /// Exact stored source format and converter version.
        source_format: ImportedConversationSourceFormat,
    },
}

impl ConversationSummary {
    /// Returns the unified cursor position this summary occupies.
    pub const fn cursor(&self) -> ConversationCursor {
        match self {
            Self::NativeSession { session_id, .. } => {
                ConversationCursor::new(ConversationOrigin::NativeSession, *session_id)
            }
            Self::ImportedConversation {
                imported_conversation_id,
                ..
            } => ConversationCursor::new(
                ConversationOrigin::ImportedConversation,
                *imported_conversation_id,
            ),
        }
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        match self {
            Self::NativeSession {
                title,
                defaults_version,
                ..
            } => {
                if let Some(title) = title {
                    validate_nonempty_metadata_text(title)
                        .map_err(|_| FrameValidationError::ConversationListShape)?;
                    let mut total_utf8_bytes = 0usize;
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, title)
                        .map_err(|_| FrameValidationError::ConversationListShape)?;
                }
                if defaults_version.value() == 0 {
                    return Err(FrameValidationError::ConversationListShape);
                }
                Ok(())
            }
            Self::ImportedConversation {
                title, entry_count, ..
            } => {
                if let Some(title) = title {
                    validate_imported_display_title(title)?;
                }
                if entry_count.value() == 0 {
                    return Err(FrameValidationError::ConversationListShape);
                }
                Ok(())
            }
        }
    }
}

/// Validates the wire restatement of the derived display-title shape:
/// nonempty single-line text without U+0000, at most
/// [`MAX_IMPORTED_CONVERSATION_DISPLAY_TITLE_SCALARS`] Unicode scalars, and
/// no leading or trailing ASCII space or tab.
fn validate_imported_display_title(title: &str) -> Result<(), FrameValidationError> {
    if title.is_empty()
        || title.contains(['\0', '\n', '\r'])
        || title.chars().count() > MAX_IMPORTED_CONVERSATION_DISPLAY_TITLE_SCALARS
        || title.starts_with([' ', '\t'])
        || title.ends_with([' ', '\t'])
    {
        return Err(FrameValidationError::ConversationListShape);
    }
    Ok(())
}

/// Closed versioned request family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Create an owner-initiated session.
    CreateSession {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Initial session model-selection defaults.
        initial_model_selection: ModelSelection,
        /// Optional initial system prompt; required null-or-text member at
        /// version nine, absent below it.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
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
    /// Read one filtered bounded metadata-summary page.
    ListSessionMetadata {
        /// Exact tags every result must carry.
        #[serde(deserialize_with = "deserialize_required_metadata_tags")]
        required_tags: Vec<String>,
        /// Optional exact case-sensitive title substring.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title_contains: Option<String>,
        /// Whether archived sessions participate.
        include_archived: bool,
        /// Inclusive result bound from one through one hundred.
        page_size: CanonicalU64,
        /// Exclusive session-identity cursor.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        after_session_id: Option<CanonicalUuid>,
    },
    /// Read one filtered bounded unified conversation-summary page.
    ListConversations {
        /// Optional exact case-sensitive title substring.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title_contains: Option<String>,
        /// Which origin classes participate.
        origin: ConversationOriginFilter,
        /// Whether archived native sessions participate.
        include_archived: bool,
        /// Inclusive result bound from one through one hundred.
        page_size: CanonicalU64,
        /// Exclusive unified keyset cursor.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        after: Option<ConversationCursor>,
    },
    /// Read one complete current metadata snapshot.
    ReadSessionMetadata {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Durably replace one complete metadata snapshot.
    ReplaceSessionMetadata {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Complete replacement object.
        metadata: SessionMetadata,
    },
    /// Replace one session's complete defaults with a new immutable epoch.
    ReplaceSessionDefaults {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact caller-observed current epoch.
        expected_defaults_version: CanonicalU64,
        /// Complete replacement model selection.
        model_selection: ModelSelection,
        /// Complete replacement dangerous-tool blanket-auto posture.
        dangerous_tool_auto_approval: bool,
        /// Complete replacement system prompt; required null-or-text member
        /// at version nine, absent below it.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
    },
    /// Read one session's complete current or named immutable defaults epoch.
    ReadSessionDefaults {
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact immutable epoch to read, or null for the current epoch.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        defaults_version: Option<CanonicalU64>,
    },
    /// Import one complete external conversation snapshot.
    ImportConversation {
        /// Explicit format-versioned converter selection.
        format: ConversationImportFormat,
        /// Exact complete source bytes.
        source: ConversationImportSource,
    },
    /// Create a live session from one inclusive imported entry boundary.
    CreateSessionFromImportedFrontier {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Immutable imported conversation to continue.
        imported_conversation_id: CanonicalUuid,
        /// Inclusive one-based imported entry position.
        through_position: CanonicalU64,
        /// Creation-time resume or fork intent.
        relationship: ImportedSessionRelationship,
        /// Initial session model-selection defaults.
        initial_model_selection: ModelSelection,
    },
    /// Reconcile the exact active turn parked on an ambiguous model call.
    ///
    /// The named turn must be the session's active turn and must be parked in
    /// the model-call recovery wait. The request supplies the owner interrupt
    /// authority that turn's terminal disposition requires and carries the
    /// successor input the session continues with.
    ReconcileTurn {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// The turn the caller observed parked awaiting reconciliation.
        expected_active_turn_id: CanonicalUuid,
        /// Exact owner text for the immediate successor turn.
        content: InputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
    },
    /// Register one immutable external review target snapshot.
    CreateReviewTarget {
        command_id: CommandId,
        target_id: CanonicalUuid,
        provider: String,
        repository: String,
        subject: ReviewTargetSubject,
        head_revision: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        base_revision: Option<String>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        stack_parent_target_id: Option<CanonicalUuid>,
    },
    /// Admit one run and its sole session-backed pass.
    StartReviewRun {
        command_id: CommandId,
        target_id: CanonicalUuid,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        workflow: ReviewWorkflow,
        session_id: CanonicalUuid,
        accepted_input_id: CanonicalUuid,
    },
    /// Atomically bind one queued run and pass to their already-active turn.
    ActivateReviewPass {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Owning run.
        run_id: CanonicalUuid,
        /// Pass to activate.
        pass_id: CanonicalUuid,
        /// Canonical active turn created from the pass's accepted input.
        turn_id: CanonicalUuid,
    },
    RecordReviewFindings {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        findings: Vec<ReviewFindingInput>,
    },
    /// Atomically conclude a judge pass and append a finding disposition.
    RecordReviewFindingDisposition {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        finding_id: CanonicalUuid,
        /// Exact contiguous event ordinal the appended disposition occupies.
        event_ordinal: CanonicalU64,
        disposition: ReviewFindingDisposition,
    },
    /// Reserve one provider object identity before an external write.
    ReserveReviewExternalLink {
        command_id: CommandId,
        external_link_id: CanonicalUuid,
        finding_id: CanonicalUuid,
        provider: String,
        object_kind: ReviewExternalObjectKind,
    },
    /// Attach a provider identity through an exact publish-pass result.
    AttachReviewExternalLink {
        command_id: CommandId,
        external_link_id: CanonicalUuid,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        external_object: String,
        event_ordinal: CanonicalU64,
    },
    /// Read one immutable target snapshot.
    ReadReviewTarget { target_id: CanonicalUuid },
    /// Read one run and its sole pass projection.
    ReadReviewRun { run_id: CanonicalUuid },
    /// Read one complete finding aggregate projection.
    ReadReviewFinding { finding_id: CanonicalUuid },
    /// List findings produced by one exact run in identity order.
    ListReviewFindings { run_id: CanonicalUuid },
    /// Stop the exact active turn through the accepted interrupt treatment.
    ///
    /// The request applies the `Interrupt` delivery to the named active turn:
    /// its stop is durably requested and terminalization flows through the
    /// existing lifecycle, while `content` becomes the immediate-successor
    /// origin the session continues with. No standalone cancellation command
    /// exists; this verb is the interrupt treatment on the wire (INV-029).
    StopTurn {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// The turn the caller observed active in the session.
        expected_active_turn_id: CanonicalUuid,
        /// Exact owner text for the immediate successor turn.
        content: InputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
    },
    /// Supply the owner decision for one pending tool request.
    DecideToolRequest {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Session the caller expects to own the request.
        session_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact closed approval decision.
        decision: ToolDecision,
    },
}

/// One closed wire approval decision for a pending tool request.
///
/// The wire surface requires a denial reason; the daemon validates it against
/// the domain's denial-reason contract before command construction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolDecision {
    /// Execution is permitted subject to current aggregate guards.
    Approve {},
    /// Execution is permanently prohibited for this request.
    Deny {
        /// Exact owner explanation rendered to the model.
        reason: String,
    },
}

impl ClientRequest {
    const fn minimum_protocol_version(&self) -> u64 {
        match self {
            Self::ListSessionMetadata { .. }
            | Self::ReadSessionMetadata { .. }
            | Self::ReplaceSessionMetadata { .. } => SESSION_METADATA_PROTOCOL_VERSION,
            Self::ImportConversation { .. } => CONVERSATION_IMPORT_PROTOCOL_VERSION,
            Self::ReplaceSessionDefaults { .. } => MID_SESSION_MODEL_SELECTION_PROTOCOL_VERSION,
            Self::ReconcileTurn { .. } => TURN_RECONCILIATION_PROTOCOL_VERSION,
            Self::CreateReviewTarget { .. }
            | Self::StartReviewRun { .. }
            | Self::RecordReviewFindings { .. }
            | Self::ActivateReviewPass { .. }
            | Self::RecordReviewFindingDisposition { .. }
            | Self::ReserveReviewExternalLink { .. }
            | Self::AttachReviewExternalLink { .. }
            | Self::ReadReviewTarget { .. }
            | Self::ReadReviewRun { .. }
            | Self::ReadReviewFinding { .. }
            | Self::ListReviewFindings { .. } => REVIEW_WORKFLOW_PROTOCOL_VERSION,
            Self::StopTurn { .. } | Self::DecideToolRequest { .. } => TURN_CONTROL_PROTOCOL_VERSION,
            Self::ListConversations { .. } => UNIFIED_CONVERSATION_LISTING_PROTOCOL_VERSION,
            Self::ReadSessionDefaults { .. } => SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION,
            Self::CreateSessionFromImportedFrontier { .. } => {
                IMPORTED_SESSION_CONTINUATION_PROTOCOL_VERSION
            }
            Self::CreateSession { .. }
            | Self::ListSessions {}
            | Self::SubmitInput { .. }
            | Self::ReadTranscript { .. }
            | Self::FollowSession { .. } => PROTOCOL_VERSION,
        }
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if let Self::CreateSessionFromImportedFrontier {
            through_position, ..
        } = self
            && through_position.value() == 0
        {
            return Err(FrameValidationError::ImportedFrontierShape);
        }
        if let Self::ListSessionMetadata {
            required_tags,
            title_contains,
            page_size,
            ..
        } = self
        {
            let canonical_tags =
                canonical_metadata_tags(required_tags.clone(), MAX_SESSION_METADATA_REQUIRED_TAGS)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
            let mut total_utf8_bytes = 0usize;
            for tag in &canonical_tags {
                add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
            }
            if let Some(query) = title_contains {
                validate_nonempty_metadata_text(query)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
                add_metadata_utf8_bytes(&mut total_utf8_bytes, query)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
            }
            if !(1..=100).contains(&page_size.value()) {
                return Err(FrameValidationError::MetadataShape);
            }
        }
        if let Self::ListConversations {
            title_contains,
            page_size,
            ..
        } = self
        {
            if let Some(query) = title_contains {
                validate_nonempty_metadata_text(query)
                    .map_err(|_| FrameValidationError::ConversationListShape)?;
                let mut total_utf8_bytes = 0usize;
                add_metadata_utf8_bytes(&mut total_utf8_bytes, query)
                    .map_err(|_| FrameValidationError::ConversationListShape)?;
            }
            if !(1..=100).contains(&page_size.value()) {
                return Err(FrameValidationError::ConversationListShape);
            }
        }
        Ok(())
    }
}

/// One validated client frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
}

impl ClientFrame {
    /// Constructs a current-version frame with a correlated request identity.
    pub fn try_new(
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        Self::try_new_for_version(ProtocolVersion::One, request_id, request)
    }

    /// Constructs a frame in one admitted protocol version.
    pub fn try_new_for_version(
        version: ProtocolVersion,
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version,
            request_id,
            request,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the admitted protocol version.
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Borrows the closed request.
    pub const fn request(&self) -> &ClientRequest {
        &self.request
    }

    /// Transfers the admitted version, correlation identity, and closed
    /// request out of the frame.
    pub fn into_parts(self) -> (ProtocolVersion, RequestId, ClientRequest) {
        (self.version, self.request_id, self.request)
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if !self.request_id.is_correlated() {
            return Err(FrameValidationError::UncorrelatedClientRequest);
        }
        if self.version.as_u64() < self.request.minimum_protocol_version() {
            return Err(FrameValidationError::RequestRequiresNewerVersion);
        }
        if let ClientRequest::CreateSession { system_prompt, .. }
        | ClientRequest::ReplaceSessionDefaults { system_prompt, .. } = &self.request
        {
            validate_system_prompt_member(
                self.version,
                system_prompt,
                FrameValidationError::RequestRequiresNewerVersion,
            )?;
        }
        self.request.validate()
    }
}

/// Requires the presence-checked system-prompt member exactly at version nine
/// and above: an absent member there is a missing required field, while a
/// present member below version nine belongs to a newer closed vocabulary.
fn validate_system_prompt_member(
    version: ProtocolVersion,
    member: &SystemPromptMember,
    requires_newer: FrameValidationError,
) -> Result<(), FrameValidationError> {
    if version.as_u64() >= SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION {
        if member.is_absent() {
            return Err(FrameValidationError::SystemPromptShape);
        }
    } else if !member.is_absent() {
        return Err(requires_newer);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClientFrame {
    version: ProtocolVersion,
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
    /// Client version is not retained by this implementation.
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
    /// Infrastructure obscured whether a requested mutation committed.
    CommitAmbiguous,
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
    /// The caller named a turn that no longer holds the session slot.
    ActiveTurnMismatch {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller expected to be active.
        expected_active_turn_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// No turn held the session slot when the caller named one.
    NoActiveTurn {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller expected to be active.
        expected_active_turn_id: CanonicalUuid,
    },
    /// The named turn is not parked on the model-call recovery wait, so no
    /// reconciliation decision is owed for it.
    ///
    /// This precondition is refused before a durable command is recorded; a
    /// caller that races the authoritative state instead receives one of the
    /// recorded rejections above.
    TurnNotAwaitingReconciliation {
        /// Target session.
        session_id: CanonicalUuid,
        /// Turn the caller named.
        turn_id: CanonicalUuid,
    },
    /// A distinct earlier stop was already applied to the active turn.
    InterruptAlreadyApplied {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
        /// Command whose applied result already carries the stop proof.
        existing_command_id: CanonicalUuid,
    },
    /// The active turn is parked on a tool-approval wait, which a stop can
    /// neither decide nor bypass; the caller denies the pending request first.
    InterruptUnavailableWhileAwaitingApproval {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// No logical tool request had the named identity.
    ToolRequestNotFound {
        /// Absent logical tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The named tool request already had a terminal approval resolution.
    ToolRequestAlreadyResolved {
        /// Resolved logical tool request.
        tool_request_id: CanonicalUuid,
    },
    /// An earlier request in the same batch still awaited its decision.
    ToolRequestNotEarliestUndecided {
        /// Named logical tool request.
        tool_request_id: CanonicalUuid,
        /// Earliest undecided request owed a decision first.
        earliest_tool_request_id: CanonicalUuid,
    },
    /// The named tool request is not owned by the named session, so no
    /// decision is admitted for it.
    ///
    /// This precondition is refused before a durable command is recorded; a
    /// correctly correlated request instead reaches the canonical decision
    /// command and its recorded rejections above.
    ToolRequestNotInSession {
        /// Session the caller named.
        session_id: CanonicalUuid,
        /// Tool request the caller named.
        tool_request_id: CanonicalUuid,
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
    /// The session defaults epoch ordinal was exhausted.
    DefaultsVersionExhausted {
        /// Target session.
        session_id: CanonicalUuid,
        /// Last representable epoch.
        current: CanonicalU64,
    },
}

impl RejectionDetail {
    const fn minimum_protocol_version(self) -> u64 {
        match self {
            Self::DefaultsVersionExhausted { .. } => MID_SESSION_MODEL_SELECTION_PROTOCOL_VERSION,
            Self::ActiveTurnMismatch { .. }
            | Self::NoActiveTurn { .. }
            | Self::TurnNotAwaitingReconciliation { .. } => TURN_RECONCILIATION_PROTOCOL_VERSION,
            Self::InterruptAlreadyApplied { .. }
            | Self::InterruptUnavailableWhileAwaitingApproval { .. }
            | Self::ToolRequestNotFound { .. }
            | Self::ToolRequestAlreadyResolved { .. }
            | Self::ToolRequestNotEarliestUndecided { .. }
            | Self::ToolRequestNotInSession { .. } => TURN_CONTROL_PROTOCOL_VERSION,
            Self::SessionNotFound { .. }
            | Self::ActiveTurnPresent { .. }
            | Self::DefaultsVersionMismatch { .. }
            | Self::UnknownModelAlias { .. }
            | Self::AcceptancePositionExhausted { .. } => 1,
        }
    }
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

    const fn minimum_protocol_version(self) -> u64 {
        match self.0 {
            Some(detail) => detail.minimum_protocol_version(),
            None => 1,
        }
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
    /// Cancellation was durably requested for the issued call.
    CancellationRequested {},
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

/// Terminal model-call dispositions admitted by a failed transcript turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedModelCallDisposition {
    /// The provider interaction failed with definitive evidence.
    KnownFailed,
    /// The provider call was cancelled without terminalizing the turn as
    /// cancelled.
    Cancelled,
}

/// Optional terminal call evidence carried by a failed transcript turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedTerminalModelCall {
    model_call_id: CanonicalUuid,
    disposition: FailedModelCallDisposition,
}

impl FailedTerminalModelCall {
    /// Constructs one exact failed-turn terminal-call projection.
    pub const fn new(
        model_call_id: CanonicalUuid,
        disposition: FailedModelCallDisposition,
    ) -> Self {
        Self {
            model_call_id,
            disposition,
        }
    }

    /// Returns the terminal model-call identity.
    pub const fn model_call_id(&self) -> CanonicalUuid {
        self.model_call_id
    }

    /// Returns the exact terminal call disposition.
    pub const fn disposition(&self) -> FailedModelCallDisposition {
        self.disposition
    }
}

/// Authoritative turn state carried by a transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
    /// The turn is parked on an owner decision for a tool request.
    ActiveAwaitingToolApproval {
        /// Earliest undecided tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The turn is parked on an ambiguous tool attempt.
    ActiveAwaitingToolRecovery {
        /// Ended turn attempt that issued the tool effect.
        ended_attempt_id: CanonicalUuid,
        /// Ambiguous tool attempt awaiting recovery.
        recovery_tool_attempt_id: CanonicalUuid,
    },
    /// The turn terminalized as failed.
    Failed {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Terminal physical attempt, or null for an evidence-free recovery
        /// failure.
        terminal_attempt_id: Option<CanonicalUuid>,
        /// Terminal call evidence, or null when no call existed.
        terminal_model_call: Option<FailedTerminalModelCall>,
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
    /// The turn terminalized after confirmed cancellation.
    Cancelled {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Terminal call, or null when cancellation preceded preparation.
        terminal_model_call_id: Option<CanonicalUuid>,
    },
    /// The turn terminalized on an ambiguous model call.
    ReconciliationRequired {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Exact ambiguous terminal model call.
        terminal_model_call_id: CanonicalUuid,
    },
    /// The turn terminalized on an ambiguous tool attempt.
    ToolReconciliationRequired {
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
        /// Authoritative terminal turn attempt.
        terminal_attempt_id: CanonicalUuid,
        /// Exact terminal tool attempt.
        terminal_tool_attempt_id: CanonicalUuid,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawTurnState {
    Queued {
        accepted_input_id: CanonicalUuid,
        content: InputContent,
    },
    ActiveRunning {
        current_attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        current_model_call: Option<CurrentModelCall>,
    },
    ActiveAwaitingModelCallRecovery {
        ended_attempt_id: CanonicalUuid,
        recovery_model_call_id: CanonicalUuid,
    },
    ActiveAwaitingToolApproval {
        tool_request_id: CanonicalUuid,
    },
    ActiveAwaitingToolRecovery {
        ended_attempt_id: CanonicalUuid,
        recovery_tool_attempt_id: CanonicalUuid,
    },
    Failed {
        terminal_frontier_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_attempt_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_model_call: Option<FailedTerminalModelCall>,
    },
    Completed {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    Refused {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    Cancelled {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_model_call_id: Option<CanonicalUuid>,
    },
    ReconciliationRequired {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_model_call_id: CanonicalUuid,
    },
    ToolReconciliationRequired {
        terminal_frontier_id: CanonicalUuid,
        terminal_attempt_id: CanonicalUuid,
        terminal_tool_attempt_id: CanonicalUuid,
    },
}

impl<'de> Deserialize<'de> for TurnState {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let state = match RawTurnState::deserialize(deserializer)? {
            RawTurnState::Queued {
                accepted_input_id,
                content,
            } => Self::Queued {
                accepted_input_id,
                content,
            },
            RawTurnState::ActiveRunning {
                current_attempt_id,
                current_model_call,
            } => Self::ActiveRunning {
                current_attempt_id,
                current_model_call,
            },
            RawTurnState::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
            } => Self::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
            },
            RawTurnState::ActiveAwaitingToolApproval { tool_request_id } => {
                Self::ActiveAwaitingToolApproval { tool_request_id }
            }
            RawTurnState::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
            } => Self::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
            },
            RawTurnState::Failed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call,
            } => {
                if terminal_model_call.is_some() && terminal_attempt_id.is_none() {
                    return Err(serde::de::Error::custom(
                        "failed terminal call requires a terminal attempt",
                    ));
                }
                Self::Failed {
                    terminal_frontier_id,
                    terminal_attempt_id,
                    terminal_model_call,
                }
            }
            RawTurnState::Completed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Completed {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::Refused {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Refused {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::Cancelled {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::Cancelled {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::ReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            } => Self::ReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_model_call_id,
            },
            RawTurnState::ToolReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_tool_attempt_id,
            } => Self::ToolReconciliationRequired {
                terminal_frontier_id,
                terminal_attempt_id,
                terminal_tool_attempt_id,
            },
        };
        Ok(state)
    }
}

impl TurnState {
    const fn minimum_protocol_version(&self) -> u64 {
        match self {
            Self::ActiveAwaitingToolApproval { .. }
            | Self::ActiveAwaitingToolRecovery { .. }
            | Self::ToolReconciliationRequired { .. } => 3,
            Self::Queued { .. }
            | Self::ActiveRunning { .. }
            | Self::ActiveAwaitingModelCallRecovery { .. }
            | Self::Failed { .. }
            | Self::Completed { .. }
            | Self::Refused { .. }
            | Self::Cancelled { .. }
            | Self::ReconciliationRequired { .. } => 1,
        }
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if let Self::Failed {
            terminal_attempt_id: None,
            terminal_model_call: Some(_),
            ..
        } = self
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        Ok(())
    }
}

/// Source speaker admitted by an imported transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedSpeaker {
    /// The source identified the entry as user-authored.
    User,
    /// The source identified the entry as assistant-authored.
    Assistant,
}

/// Exact source attestation for an imported entry's speaker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportedSourceSpeaker {
    /// The source omitted the speaker field.
    NotAttested {},
    /// The source explicitly supplied no speaker.
    AttestedAbsent {},
    /// The source supplied one admitted speaker.
    Attested {
        /// Exact source-supplied speaker.
        speaker: ImportedSpeaker,
    },
}

/// Conservative kind projection for imported content without rendered text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedContentKind {
    /// One source-defined event.
    SourceEvent,
    /// One source-defined message block.
    SourceMessageBlock,
    /// Text whose value was absent or unattested.
    Text,
    /// One imported tool call.
    ToolCall,
    /// One imported tool result.
    ToolResult,
    /// One imported thinking block.
    Thinking,
    /// One imported redacted-thinking block.
    RedactedThinking,
    /// One imported document block.
    Document,
    /// A typed absence for message content.
    MessageContentAbsent,
}

/// Non-text semantic transcript entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptEntry {
    /// Injected boundary declaring the model identity newly in force.
    ModelIdentityChanged {
        /// Turn whose start first observes the new model identity.
        turn_id: CanonicalUuid,
        /// Immutable defaults epoch bound by the turn.
        defaults_version: CanonicalU64,
        /// Exact direct model identity frozen for the turn.
        selected_model_id: CanonicalUuid,
    },
    /// Assistant proposed one durable tool request.
    AssistantToolUse {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact checked tool name.
        tool_name: String,
        /// Exact normalized or scrubbed-undecodable arguments.
        arguments: String,
    },
    /// One physical tool attempt produced the logical result.
    ToolExecutionResult {
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact physical tool attempt.
        tool_attempt_id: CanonicalUuid,
        /// Exact provider-visible result content.
        content: String,
    },
    /// One logical tool request was denied.
    ToolDenied {
        /// Exact denied tool request.
        tool_request_id: CanonicalUuid,
        /// Exact provider-visible denial content.
        content: String,
    },
    /// One logical tool request closed because its turn ended.
    ToolClosed {
        /// Exact closed tool request.
        tool_request_id: CanonicalUuid,
        /// Exact provider-visible terminal-closure content.
        content: String,
    },
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
    /// Explicit cancelled-turn marker.
    TurnCancelled {
        /// Cancelled turn.
        turn_id: CanonicalUuid,
    },
    /// Conservative imported entry without rendered text.
    Imported {
        /// Owning imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Exact imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
        /// Conservative normalized content kind.
        content_kind: ImportedContentKind,
    },
}

impl TranscriptEntry {
    const fn minimum_protocol_version(&self) -> u64 {
        match self {
            Self::ModelIdentityChanged { .. } => MID_SESSION_MODEL_SELECTION_PROTOCOL_VERSION,
            Self::AssistantToolUse { .. }
            | Self::ToolExecutionResult { .. }
            | Self::ToolDenied { .. }
            | Self::ToolClosed { .. } => TOOL_LOOP_PROTOCOL_VERSION,
            Self::Imported { .. } => IMPORTED_TRANSCRIPT_PROTOCOL_VERSION,
            Self::TurnCompleted { .. } | Self::TurnFailed { .. } | Self::TurnCancelled { .. } => 1,
        }
    }
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
    /// Imported text whose exact value was source-attested.
    Imported {
        /// Owning imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Exact imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
    },
}

impl TranscriptTextEntry {
    const fn minimum_protocol_version(&self) -> u64 {
        match self {
            Self::Imported { .. } => IMPORTED_TRANSCRIPT_PROTOCOL_VERSION,
            Self::User { .. } | Self::Assistant { .. } => 1,
        }
    }
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
    /// Cancellation was durably requested for the issued call.
    CancellationRequested {},
    /// Call reached a terminal disposition.
    Terminal {
        /// Exact terminal disposition.
        disposition: ModelCallDisposition,
    },
}

/// Exact durable state of one tool batch presentation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolBatchState {
    /// Assistant tool proposals committed.
    Proposed {
        /// Exact frontier containing the assistant tool-use entries.
        frontier_id: CanonicalUuid,
    },
    /// Proposal-ordered logical results committed.
    ResultsProjected {
        /// Exact frontier containing the result suffix.
        frontier_id: CanonicalUuid,
    },
    /// One ambiguous physical attempt requires owner recovery.
    RecoveryRequired {
        /// Exact ambiguous tool attempt.
        tool_attempt_id: CanonicalUuid,
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
    /// A tool batch crossed one durable presentation boundary.
    ToolBatchTransition {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Model call that proposed the batch.
        model_call_id: CanonicalUuid,
        /// Exact committed batch state.
        state: ToolBatchState,
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
    /// Turn was cancelled.
    TurnCancelled {
        /// Cancelled turn.
        turn_id: CanonicalUuid,
        /// Semantic cancellation marker.
        cancellation_entry_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn stopped with an ambiguous model call requiring reconciliation.
    TurnReconciliationRequired {
        /// Reconciliation-required turn.
        turn_id: CanonicalUuid,
        /// Exact ambiguous terminal model call.
        model_call_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
    /// Turn stopped with an ambiguous tool attempt requiring reconciliation.
    TurnToolReconciliationRequired {
        /// Reconciliation-required turn.
        turn_id: CanonicalUuid,
        /// Exact ambiguous terminal tool attempt.
        tool_attempt_id: CanonicalUuid,
        /// Exact terminal frontier.
        terminal_frontier_id: CanonicalUuid,
    },
}

impl SessionEvent {
    const fn minimum_protocol_version(&self) -> u64 {
        match self {
            Self::ToolBatchTransition { .. } | Self::TurnToolReconciliationRequired { .. } => 3,
            Self::SessionCreated {}
            | Self::InputAccepted { .. }
            | Self::TurnActivated { .. }
            | Self::ModelCallTransition { .. }
            | Self::TurnCompleted { .. }
            | Self::TurnFailed { .. }
            | Self::TurnRefused { .. }
            | Self::TurnCancelled { .. }
            | Self::TurnReconciliationRequired { .. } => 1,
        }
    }
}

/// Closed versioned server message family.
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
    /// Begins one bounded metadata-summary page.
    SessionMetadataPageStart {},
    /// One current session metadata summary.
    SessionMetadataSummary {
        /// Session identity.
        session_id: CanonicalUuid,
        /// Current defaults version.
        defaults_version: CanonicalU64,
        /// Current model-selection request.
        model_selection: ModelSelection,
        /// Whether the current defaults blanket-approve dangerous tools.
        dangerous_tool_auto_approval: bool,
        /// Optional exact title.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        title: Option<String>,
        /// Exact sorted flat tags.
        #[serde(deserialize_with = "deserialize_session_metadata_tags")]
        tags: Vec<String>,
        /// Whether the session is archived.
        archived: bool,
        /// Last replacement writer, absent only before the first write.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        last_writer: Option<MetadataLastWriter>,
    },
    /// Completes one bounded metadata-summary page.
    SessionMetadataPageEnd {
        /// Number of preceding summaries.
        session_count: CanonicalU64,
        /// Exclusive cursor for another page, or null when no later match
        /// existed in this page snapshot.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        next_after_session_id: Option<CanonicalUuid>,
    },
    /// Begins one bounded unified conversation-summary page.
    ConversationPageStart {},
    /// One unified conversation summary.
    ConversationSummary {
        /// Closed per-origin summary.
        conversation: ConversationSummary,
    },
    /// Completes one bounded unified conversation-summary page.
    ConversationPageEnd {
        /// Number of preceding summaries.
        conversation_count: CanonicalU64,
        /// Exclusive cursor for another page, or null when no later match
        /// existed in this page snapshot.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        next_after: Option<ConversationCursor>,
    },
    /// One complete current metadata read.
    SessionMetadata {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Complete current metadata object.
        metadata: SessionMetadata,
        /// Last replacement writer, absent only before the first write.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        last_writer: Option<MetadataLastWriter>,
    },
    /// One successful complete metadata replacement receipt.
    SessionMetadataReplaced {
        /// Updated session.
        session_id: CanonicalUuid,
        /// Complete committed metadata object.
        metadata: SessionMetadata,
        /// Non-null last replacement writer.
        last_writer: MetadataLastWriter,
    },
    /// One successful forward-only session-defaults replacement receipt.
    SessionDefaultsReplaced {
        /// Updated session.
        session_id: CanonicalUuid,
        /// Newly installed immutable defaults epoch.
        defaults_version: CanonicalU64,
        /// Complete committed model selection.
        model_selection: ModelSelection,
        /// Complete committed dangerous-tool blanket-auto posture.
        dangerous_tool_auto_approval: bool,
        /// Complete committed system prompt; required null-or-text member at
        /// version nine, absent below it.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
    },
    /// One complete current or named immutable session-defaults epoch.
    SessionDefaults {
        /// Selected session.
        session_id: CanonicalUuid,
        /// The read immutable defaults epoch.
        defaults_version: CanonicalU64,
        /// Complete model selection on that epoch.
        model_selection: ModelSelection,
        /// Complete dangerous-tool blanket-auto posture on that epoch.
        dangerous_tool_auto_approval: bool,
        /// Exact optional system prompt on that epoch.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        system_prompt: Option<SystemPromptText>,
    },
    /// One recorded owner tool-decision receipt.
    ///
    /// The receipt mirrors the recorded applied result exactly; an equal
    /// command replay returns this same projection.
    ToolRequestDecided {
        /// Decided logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact recorded decision.
        decision: ToolDecision,
    },
    /// One new immutable imported conversation was inserted.
    ConversationImportInserted {
        /// Newly durable imported-conversation identity.
        imported_conversation_id: CanonicalUuid,
    },
    /// The exact imported snapshot was already durable.
    ConversationImportAlreadyImported {
        /// Existing durable imported-conversation identity.
        imported_conversation_id: CanonicalUuid,
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
    /// One cursorless, process-local provider text fragment.
    ProviderTextDelta {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Active turn receiving the provider response.
        turn_id: CanonicalUuid,
        /// Correlated model call producing the response.
        model_call_id: CanonicalUuid,
        /// Provider part position this fragment extends.
        part_index: CanonicalU64,
        /// One bounded fragment of already-redacted provider text.
        content: ContentFragment,
    },
    /// One immutable target registration was recorded or equally replayed.
    ReviewTargetCreated {
        /// Registered target.
        target_id: CanonicalUuid,
    },
    /// One run and its sole pass were admitted or equally replayed.
    ReviewRunStarted {
        /// Admitted run.
        run_id: CanonicalUuid,
        /// Admitted pass.
        pass_id: CanonicalUuid,
    },
    /// One queued run and pass were atomically activated or equally replayed.
    ReviewPassActivated {
        /// Activated run.
        run_id: CanonicalUuid,
        /// Activated pass.
        pass_id: CanonicalUuid,
    },
    /// One read-only result and complete finding inventory were committed.
    ReviewFindingsRecorded {
        /// Concluding run.
        run_id: CanonicalUuid,
        /// Concluding pass.
        pass_id: CanonicalUuid,
        /// Exact committed finding count.
        finding_count: CanonicalU64,
    },
    /// One finding disposition was committed.
    ReviewFindingDispositionRecorded {
        /// Updated finding.
        finding_id: CanonicalUuid,
        /// Current derived status.
        status: ReviewFindingStatus,
    },
    /// One pre-effect external-link reservation was recorded.
    ReviewExternalLinkReserved {
        /// Stable reservation identity.
        external_link_id: CanonicalUuid,
    },
    /// One provider object identity was attached.
    ReviewExternalLinkAttached {
        /// Consumed reservation identity.
        external_link_id: CanonicalUuid,
        /// Canonical provider object key.
        external_object: String,
    },
    /// One immutable target read.
    ReviewTarget {
        /// Complete target snapshot.
        target: ReviewTargetSnapshot,
    },
    /// One run and its optional pass read.
    ReviewRun {
        /// Complete run snapshot.
        run: ReviewRunSnapshot,
        /// Complete pass snapshot after admission.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass: Option<ReviewPassSnapshot>,
    },
    /// One complete finding read.
    ReviewFinding {
        /// Complete finding snapshot.
        finding: ReviewFindingSnapshot,
    },
    /// Begins one finding list sequence.
    ReviewFindingsStart {
        /// Selected run.
        run_id: CanonicalUuid,
    },
    /// One finding in identity order.
    ReviewFindingItem {
        /// Complete finding snapshot.
        finding: ReviewFindingSnapshot,
    },
    /// Completes one finding list sequence.
    ReviewFindingsEnd {
        /// Number of preceding items.
        finding_count: CanonicalU64,
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

impl ServerMessage {
    /// Returns the oldest protocol version whose closed vocabulary contains
    /// this exact message.
    pub const fn minimum_protocol_version(&self) -> u64 {
        match self {
            Self::TranscriptTurn { state, .. } => state.minimum_protocol_version(),
            Self::TranscriptEntry { entry, .. } => entry.minimum_protocol_version(),
            Self::TranscriptTextEntry { entry, .. } => entry.minimum_protocol_version(),
            Self::SessionEvent { event, .. } => event.minimum_protocol_version(),
            Self::ProviderTextDelta { .. } => PROVIDER_TEXT_STREAMING_PROTOCOL_VERSION,
            Self::SessionMetadataPageStart {}
            | Self::SessionMetadataSummary { .. }
            | Self::SessionMetadataPageEnd { .. }
            | Self::SessionMetadata { .. }
            | Self::SessionMetadataReplaced { .. } => SESSION_METADATA_PROTOCOL_VERSION,
            Self::ConversationImportInserted { .. }
            | Self::ConversationImportAlreadyImported { .. } => {
                CONVERSATION_IMPORT_PROTOCOL_VERSION
            }
            Self::SessionDefaultsReplaced { .. } => MID_SESSION_MODEL_SELECTION_PROTOCOL_VERSION,
            Self::ReviewTargetCreated { .. }
            | Self::ReviewRunStarted { .. }
            | Self::ReviewFindingsRecorded { .. }
            | Self::ReviewFindingDispositionRecorded { .. }
            | Self::ReviewExternalLinkReserved { .. }
            | Self::ReviewPassActivated { .. }
            | Self::ReviewExternalLinkAttached { .. }
            | Self::ReviewTarget { .. }
            | Self::ReviewRun { .. }
            | Self::ReviewFinding { .. }
            | Self::ReviewFindingsStart { .. }
            | Self::ReviewFindingItem { .. }
            | Self::ReviewFindingsEnd { .. } => REVIEW_WORKFLOW_PROTOCOL_VERSION,
            Self::ToolRequestDecided { .. } => TURN_CONTROL_PROTOCOL_VERSION,
            Self::ConversationPageStart {}
            | Self::ConversationSummary { .. }
            | Self::ConversationPageEnd { .. } => UNIFIED_CONVERSATION_LISTING_PROTOCOL_VERSION,
            Self::SessionDefaults { .. } => SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION,
            Self::Error { detail, .. } => detail.minimum_protocol_version(),
            Self::SessionCreated { .. }
            | Self::InputSubmitted { .. }
            | Self::SessionsStart {}
            | Self::SessionSummary { .. }
            | Self::SessionsEnd { .. }
            | Self::TranscriptSnapshotStart { .. }
            | Self::TranscriptContent { .. }
            | Self::TranscriptSnapshotEnd { .. } => 1,
        }
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        match self {
            Self::SessionMetadataSummary {
                title,
                tags,
                archived,
                last_writer,
                ..
            } => {
                let mut total_utf8_bytes = 0usize;
                if let Some(title) = title {
                    validate_nonempty_metadata_text(title)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, title)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                }
                let canonical = canonical_metadata_tags(tags.clone(), MAX_SESSION_METADATA_TAGS)
                    .map_err(|_| FrameValidationError::MetadataShape)?;
                if canonical != *tags {
                    return Err(FrameValidationError::MetadataShape);
                }
                for tag in tags {
                    add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)
                        .map_err(|_| FrameValidationError::MetadataShape)?;
                }
                if last_writer.is_none() && (title.is_some() || !tags.is_empty() || *archived) {
                    return Err(FrameValidationError::MetadataShape);
                }
            }
            Self::SessionMetadataPageEnd {
                session_count,
                next_after_session_id,
            } => {
                if session_count.value() > 100
                    || (next_after_session_id.is_some() && session_count.value() == 0)
                {
                    return Err(FrameValidationError::MetadataShape);
                }
            }
            Self::ConversationSummary { conversation } => conversation.validate()?,
            Self::ConversationPageEnd {
                conversation_count,
                next_after,
            } => {
                if conversation_count.value() > 100
                    || (next_after.is_some() && conversation_count.value() == 0)
                {
                    return Err(FrameValidationError::ConversationListShape);
                }
            }
            Self::SessionMetadata {
                metadata,
                last_writer,
                ..
            } if last_writer.is_none() && !metadata.is_initial() => {
                return Err(FrameValidationError::MetadataShape);
            }
            _ => {}
        }
        Ok(())
    }
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
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
}

impl ServerFrame {
    /// Constructs a current-version response frame.
    pub fn try_new(
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        Self::try_new_for_version(ProtocolVersion::One, request_id, message)
    }

    /// Constructs one response in an admitted protocol version.
    pub fn try_new_for_version(
        version: ProtocolVersion,
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version,
            request_id,
            message,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the admitted protocol version.
    pub const fn version(&self) -> ProtocolVersion {
        self.version
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
        if self.version.as_u64() < self.message.minimum_protocol_version() {
            return Err(FrameValidationError::MessageRequiresNewerVersion);
        }
        if let ServerMessage::TranscriptTurn { state, .. } = &self.message {
            state.validate()?;
        }
        if let ServerMessage::SessionDefaultsReplaced { system_prompt, .. } = &self.message {
            validate_system_prompt_member(
                self.version,
                system_prompt,
                FrameValidationError::MessageRequiresNewerVersion,
            )?;
        }
        self.message.validate()?;
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
    version: ProtocolVersion,
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
    /// A retained old version cannot encode a newer closed message variant.
    MessageRequiresNewerVersion,
    /// A retained old version cannot carry a newer closed request variant.
    RequestRequiresNewerVersion,
    /// A client request used reserved correlation identity zero.
    UncorrelatedClientRequest,
    /// A success response used reserved correlation identity zero.
    UncorrelatedSuccess,
    /// A non-framing error used reserved correlation identity zero.
    UncorrelatedApplicationError,
    /// Rejection detail did not match the error code.
    ErrorDetailShape,
    /// A transcript turn carried an impossible correlated state shape.
    TurnStateShape,
    /// A metadata request or response carried an invalid correlated shape.
    MetadataShape,
    /// A unified conversation-listing frame carried an invalid shape.
    ConversationListShape,
    /// A version-nine frame omitted its required system-prompt member.
    SystemPromptShape,
    /// An imported-frontier request carried a nonpositive position.
    ImportedFrontierShape,
}

impl fmt::Display for FrameValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedVersion => "frame version is unsupported",
            Self::MessageRequiresNewerVersion => "server message requires a newer protocol version",
            Self::RequestRequiresNewerVersion => "client request requires a newer protocol version",
            Self::UncorrelatedClientRequest => "client request identity is uncorrelated",
            Self::UncorrelatedSuccess => "successful server message is uncorrelated",
            Self::UncorrelatedApplicationError => "application server error is uncorrelated",
            Self::ErrorDetailShape => "server error detail does not match its code",
            Self::TurnStateShape => "transcript turn state is inconsistent",
            Self::MetadataShape => "session metadata frame shape is inconsistent",
            Self::ConversationListShape => {
                "unified conversation-listing frame shape is inconsistent"
            }
            Self::SystemPromptShape => "version-nine frame omits its required system-prompt member",
            Self::ImportedFrontierShape => "imported frontier position is not positive",
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
            FrameDecodeErrorKind::UnsupportedVersion => formatter.write_str(
                "process-protocol version is unsupported; supported versions are 1 through 12, and 16",
            ),
        }
    }
}

impl Error for FrameDecodeError {}

/// Outgoing frame could not be encoded within the admitted version boundary.
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
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        return Err(FrameDecodeError {
            kind: FrameDecodeErrorKind::OversizedFrame,
            request_id: recover_request_id(content, allow_uncorrelated),
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
    if contains_duplicate_object_member(content)
        .map_err(|_| FrameDecodeError::malformed(request_id))?
    {
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
    if !matches!(
        version_spelling,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "10" | "11" | "12" | "16"
    ) {
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

enum DuplicateMemberScanError {
    InvalidMemberName,
    NestingLimitExceeded,
}

fn contains_duplicate_object_member(content: &[u8]) -> Result<bool, DuplicateMemberScanError> {
    let mut containers: Vec<Option<HashSet<String>>> = Vec::new();
    let mut index = 0;
    while index < content.len() {
        match content[index] {
            b'{' => {
                if containers.len() == MAX_JSON_CONTAINER_DEPTH {
                    return Err(DuplicateMemberScanError::NestingLimitExceeded);
                }
                containers.push(Some(HashSet::new()));
                index += 1;
            }
            b'[' => {
                if containers.len() == MAX_JSON_CONTAINER_DEPTH {
                    return Err(DuplicateMemberScanError::NestingLimitExceeded);
                }
                containers.push(None);
                index += 1;
            }
            b'}' | b']' => {
                containers.pop();
                index += 1;
            }
            b'"' => {
                let start = index;
                index += 1;
                while index < content.len() {
                    match content[index] {
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                let mut following = index;
                while following < content.len() && content[following].is_ascii_whitespace() {
                    following += 1;
                }
                if content.get(following) == Some(&b':')
                    && let Some(Some(members)) = containers.last_mut()
                {
                    let member = serde_json::from_slice::<String>(&content[start..index])
                        .map_err(|_| DuplicateMemberScanError::InvalidMemberName)?;
                    if !members.insert(member) {
                        return Ok(true);
                    }
                }
            }
            _ => index += 1,
        }
    }
    Ok(false)
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

fn protocol_version_from_probe(probe: &RawHeaderProbe<'_>) -> Option<ProtocolVersion> {
    if probe.duplicate_member {
        return None;
    }
    match probe.version?.get() {
        "1" => Some(ProtocolVersion::One),
        "2" => Some(ProtocolVersion::Two),
        "3" => Some(ProtocolVersion::Three),
        "4" => Some(ProtocolVersion::Four),
        "5" => Some(ProtocolVersion::Five),
        "6" => Some(ProtocolVersion::Six),
        "7" => Some(ProtocolVersion::Seven),
        "8" => Some(ProtocolVersion::Eight),
        "9" => Some(ProtocolVersion::Nine),
        "10" => Some(ProtocolVersion::Ten),
        "11" => Some(ProtocolVersion::Eleven),
        "12" => Some(ProtocolVersion::Twelve),
        "16" => Some(ProtocolVersion::Sixteen),
        _ => None,
    }
}

fn recover_request_id(content: &[u8], allow_uncorrelated: bool) -> RequestId {
    // Recovery is best effort and must not parse an arbitrarily large rejected
    // line. A complete minimally oversized line can still fit this content cap.
    if content.len() > MAX_FRAME_BYTES {
        return RequestId::uncorrelated();
    }
    deserialize_header_probe(content)
        .map(|probe| request_id_from_probe(&probe, allow_uncorrelated))
        .unwrap_or_else(|_| RequestId::uncorrelated())
}

/// Recovers a correlated client request identity from bounded complete frame
/// content.
///
/// This is the server reader's best-effort correlation path when a final
/// newline is the one byte that takes an otherwise complete JSON object over
/// the frame limit. Input beyond the content bound is never parsed.
pub fn recover_bounded_client_request_id(content: &[u8]) -> RequestId {
    recover_request_id(content, false)
}

/// Recovers an admitted version from bounded complete client-frame content.
///
/// A duplicate top-level member or any unsupported spelling admits no version.
pub fn recover_bounded_client_protocol_version(content: &[u8]) -> Option<ProtocolVersion> {
    if content.len() > MAX_FRAME_BYTES {
        return None;
    }
    deserialize_header_probe(content)
        .ok()
        .and_then(|probe| protocol_version_from_probe(&probe))
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, CommandId, ContentFragment,
        ConversationCursor, ConversationImportFormat, ConversationImportSource, ConversationOrigin,
        ConversationOriginFilter, ConversationSummary, CurrentModelCall, CurrentModelCallState,
        ErrorCode, ErrorDetail, FailedModelCallDisposition, FailedTerminalModelCall,
        FrameDecodeErrorKind, FrameEncodeError, FrameValidationError, ImportedContentKind,
        ImportedConversationSourceFormat, ImportedSessionRelationship, ImportedSourceSpeaker,
        ImportedSpeaker, InputContent, MAX_CONTENT_FRAGMENT_BYTES,
        MAX_IMPORTED_CONVERSATION_DISPLAY_TITLE_SCALARS, MAX_JSON_CONTAINER_DEPTH,
        MAX_SESSION_METADATA_ATTRIBUTES, MAX_SESSION_METADATA_INDEXED_UTF8_BYTES,
        MAX_SESSION_METADATA_REQUIRED_TAGS, MAX_SESSION_METADATA_TAGS,
        MAX_SESSION_METADATA_TOTAL_UTF8_BYTES, MAX_SYSTEM_PROMPT_UTF8_BYTES, MetadataActor,
        MetadataLastWriter, ModelCallDisposition, ModelCallState, ModelSelection, PROTOCOL_VERSION,
        ProtocolVersion, RejectionDetail, RequestId, ReviewTargetSubject,
        SESSION_METADATA_PROTOCOL_VERSION, SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION, ServerFrame,
        ServerMessage, SessionEvent, SessionMetadata, SystemPromptMember, SystemPromptText,
        ToolBatchState, ToolDecision, TranscriptEntry, TranscriptTextEntry, TurnState,
        UNIFIED_CONVERSATION_LISTING_PROTOCOL_VERSION, decode_client_line, decode_server_line,
        encode_client_line, encode_server_line,
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

    fn metadata(archived: bool) -> Result<SessionMetadata, Box<dyn std::error::Error>> {
        Ok(SessionMetadata::try_new(
            Some(String::from("Planning")),
            vec![String::from("work"), String::from("daily")],
            vec![
                (String::from("run"), String::from("17")),
                (String::from("trigger"), String::new()),
            ],
            archived,
        )?)
    }

    fn numbered_metadata_strings(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("value-{index}")).collect()
    }

    fn numbered_metadata_attributes(count: usize) -> Vec<(String, String)> {
        numbered_metadata_strings(count)
            .into_iter()
            .map(|key| (key, String::new()))
            .collect()
    }

    fn line(json: &str) -> Vec<u8> {
        let mut bytes = json.as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    }

    fn padded_oversized_client_frame(request_members: &str, content_len: usize) -> Vec<u8> {
        let mut bytes = format!(
            r#"{{"version":1,{request_members},"request":{{"type":"list_sessions","padding":""#
        )
        .into_bytes();
        let suffix = b"\"}}";
        assert!(content_len >= bytes.len() + suffix.len());
        bytes.resize(content_len - suffix.len(), b'x');
        bytes.extend_from_slice(suffix);
        bytes.push(b'\n');
        assert_eq!(bytes.len(), content_len + 1);
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
        assert!(
            error
                .to_string()
                .contains("supported versions are 1 through 12, and 16")
        );
    }

    fn unsupported_version_with_nested_object_payload(payload_depth: usize) -> String {
        let payload = format!(
            "{}0{}",
            r#"{"future":"#.repeat(payload_depth),
            "}".repeat(payload_depth)
        );
        format!("{{\"version\":13,\"request_id\":\"9\",\"request\":{payload}}}")
    }

    #[track_caller]
    fn assert_command_sentinel_rejected(command_id: &str) {
        let json = format!(
            "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"create_session\",\"command_id\":\"{command_id}\",\"initial_model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000001\"}}}}}}"
        );
        assert_client_malformed(&json);
    }

    #[track_caller]
    fn assert_client_request_current_version(
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
        expected_message_json: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let request_id_value = request_id.value();
        let minimum = message.minimum_protocol_version();
        let version = ProtocolVersion::from_u64(minimum)
            .expect("every closed message names one admitted minimum version");
        let frame = ServerFrame::try_new_for_version(version, request_id, message)?;
        let encoded = encode_server_line(&frame)?;
        let expected = format!(
            "{{\"version\":{minimum},\"request_id\":\"{request_id_value}\",\"message\":{}}}\n",
            expected_message_json
        );
        assert_eq!(String::from_utf8(encoded.clone())?, expected);
        assert_eq!(decode_server_line(&encoded)?, frame);
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
        let decoded = decode_client_line(&encoded)?;
        assert_eq!(decoded, frame);
        let (decoded_version, decoded_request_id, decoded_request) = decoded.into_parts();
        assert_eq!(decoded_version, ProtocolVersion::One);
        assert_eq!(decoded_request_id, request(u64::MAX)?);
        let ClientRequest::SubmitInput { content, .. } = decoded_request else {
            return Err("decoded request changed variant".into());
        };
        assert_eq!(content.into_string(), "hello");
        assert!(String::from_utf8(encoded)?.contains("\"request_id\":\"18446744073709551615\""));
        Ok(())
    }

    #[test]
    fn inv033_version_two_reuses_the_closed_request_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Two,
            request(7)?,
            ClientRequest::ReadTranscript {
                session_id: uuid(1),
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert_eq!(frame.version(), ProtocolVersion::Two);
        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":2,"));
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_imported_text_entries_exist_only_in_version_two()
    -> Result<(), Box<dyn std::error::Error>> {
        let imported_text = ServerMessage::TranscriptTextEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(1),
            entry_id: uuid(2),
            entry: TranscriptTextEntry::Imported {
                imported_conversation_id: uuid(3),
                imported_entry_id: uuid(4),
                source_speaker: ImportedSourceSpeaker::Attested {
                    speaker: ImportedSpeaker::User,
                },
            },
        };
        assert_eq!(
            ServerFrame::try_new(request(8)?, imported_text.clone())
                .expect_err("version one must retain its native-only vocabulary"),
            FrameValidationError::MessageRequiresNewerVersion
        );
        let text_frame =
            ServerFrame::try_new_for_version(ProtocolVersion::Two, request(8)?, imported_text)?;
        let encoded_text = encode_server_line(&text_frame)?;
        assert!(String::from_utf8(encoded_text.clone())?.contains(
            r#""entry":{"type":"imported","imported_conversation_id":"00000000-0000-0000-0000-000000000003","imported_entry_id":"00000000-0000-0000-0000-000000000004","source_speaker":{"type":"attested","speaker":"user"}}"#
        ));
        assert_eq!(decode_server_line(&encoded_text)?, text_frame);
        Ok(())
    }

    #[test]
    fn inv033_imported_conservative_entries_exist_only_in_version_two()
    -> Result<(), Box<dyn std::error::Error>> {
        let imported_conservative = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(1),
            source_session_id: uuid(1),
            entry_id: uuid(5),
            entry: TranscriptEntry::Imported {
                imported_conversation_id: uuid(3),
                imported_entry_id: uuid(6),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::ToolResult,
            },
        };
        assert_eq!(
            ServerFrame::try_new(request(9)?, imported_conservative.clone())
                .expect_err("version one must retain its native-only vocabulary"),
            FrameValidationError::MessageRequiresNewerVersion
        );
        let conservative = ServerFrame::try_new_for_version(
            ProtocolVersion::Two,
            request(9)?,
            imported_conservative,
        )?;
        let encoded_conservative = encode_server_line(&conservative)?;
        assert!(
            String::from_utf8(encoded_conservative.clone())?.contains(
                r#""source_speaker":{"type":"not_attested"},"content_kind":"tool_result""#
            )
        );
        assert_eq!(decode_server_line(&encoded_conservative)?, conservative);
        Ok(())
    }

    #[test]
    fn inv033_unknown_request_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_sessions","extra":true}}"#,
        );
    }

    #[test]
    fn inv033_missing_required_request_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"read_transcript"}}"#,
        );
    }

    #[test]
    fn inv033_wrong_typed_request_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":1,"request":{"type":"list_sessions"}}"#,
        );
    }

    #[test]
    fn inv033_unknown_tagged_request_variants_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"future_request"}}"#,
        );
    }

    #[test]
    fn inv033_unknown_top_level_fields_fail_explicitly() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_sessions"},"extra":true}"#,
        );
    }

    #[test]
    fn inv033_unsupported_version_precedes_payload_decoding() {
        assert_unsupported_version("-1");
        assert_unsupported_version("13");
        assert_unsupported_version("18446744073709551616");
        assert_client_malformed(
            r#"{"version":1.0,"request_id":"9","request":{"type":"list_sessions"}}"#,
        );
    }

    #[test]
    fn inv033_unsupported_version_is_classified_at_the_container_depth_limit() {
        let future = unsupported_version_with_nested_object_payload(MAX_JSON_CONTAINER_DEPTH - 1);
        let error = decode_client_line(&line(&future))
            .expect_err("the maximum admitted depth reaches version classification");
        assert_eq!(error.kind(), FrameDecodeErrorKind::UnsupportedVersion);
        assert_eq!(error.request_id().value(), 9);
    }

    #[test]
    fn inv033_container_depth_beyond_the_limit_is_malformed() {
        let future = unsupported_version_with_nested_object_payload(MAX_JSON_CONTAINER_DEPTH);
        let error =
            decode_client_line(&line(&future)).expect_err("excessive nesting must be rejected");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
        assert_eq!(error.request_id().value(), 9);
    }

    #[test]
    fn inv033_nested_duplicates_are_malformed_before_unsupported_version() {
        let error = decode_client_line(&line(
            r#"{"version":3,"request_id":"9","request":{"future":1,"future":2}}"#,
        ))
        .expect_err("nested duplicate members are malformed for every version");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
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
    fn inv033_failed_terminal_shape_requires_nullable_attempt_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_model_call":null}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_shape_requires_nullable_call_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":null}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_call_requires_an_attempt() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":null,"terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000003","disposition":"known_failed"}}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_call_accepts_only_failure_dispositions() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"completed"}}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_call_rejects_unknown_members() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","extra":true}}}}"#,
        );
    }

    #[test]
    fn inv033_cancelled_terminal_shape_requires_nullable_call_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003"}}}"#,
        );
    }

    #[test]
    fn inv033_turn_cancelled_event_rejects_unknown_members() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"turn_cancelled","turn_id":"00000000-0000-0000-0000-000000000002","cancellation_entry_id":"00000000-0000-0000-0000-000000000003","terminal_frontier_id":"00000000-0000-0000-0000-000000000004","extra":true}}}"#,
        );
    }

    #[test]
    fn inv033_cancellation_requested_state_rejects_unknown_members() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"model_call_transition","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","state":{"type":"cancellation_requested","extra":true}}}}"#,
        );
    }

    #[test]
    fn inv033_nested_terminal_duplicate_members_are_rejected() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":null,"terminal_model_call":null}}}"#,
        );
    }

    #[test]
    fn inv033_in_memory_failed_terminal_call_requires_an_attempt()
    -> Result<(), Box<dyn std::error::Error>> {
        let invalid = ServerFrame::try_new(
            request(1)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(1),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(2),
                    terminal_attempt_id: None,
                    terminal_model_call: Some(FailedTerminalModelCall::new(
                        uuid(3),
                        FailedModelCallDisposition::KnownFailed,
                    )),
                },
            },
        )
        .expect_err("an in-memory failed call without its attempt must be rejected");
        assert_eq!(invalid, FrameValidationError::TurnStateShape);
        Ok(())
    }

    #[test]
    fn inv033_canonical_decimal_spellings_are_required() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"01","request":{"type":"list_sessions"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"+1","request":{"type":"list_sessions"}}"#,
        );
    }

    #[test]
    fn inv033_canonical_uuid_spellings_are_required() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"read_transcript","session_id":"00000000-0000-0000-0000-00000000000A"}}"#,
        );
    }

    #[test]
    fn inv012_command_sentinels_are_rejected() {
        assert_command_sentinel_rejected("00000000-0000-0000-0000-000000000000");
        assert_command_sentinel_rejected("ffffffff-ffff-ffff-ffff-ffffffffffff");
    }

    #[test]
    fn inv033_zero_client_request_id_is_rejected() {
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
    fn inv033_commit_ambiguity_has_one_stable_error_code() -> Result<(), Box<dyn std::error::Error>>
    {
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::CommitAmbiguous,
                message: "ambiguous commit".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        let encoded = encode_server_line(&frame)?;
        assert!(
            encoded
                .windows(br#""code":"commit_ambiguous""#.len())
                .any(|window| window == br#""code":"commit_ambiguous""#)
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
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
            ServerFrame::try_new(
                RequestId::uncorrelated(),
                ServerMessage::TranscriptTurn {
                    turn_id: uuid(1),
                    acceptance_position: CanonicalU64::new(1),
                    state: TurnState::Failed {
                        terminal_frontier_id: uuid(2),
                        terminal_attempt_id: None,
                        terminal_model_call: None,
                    },
                },
            )
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
    fn s24_content_fragmentation_preserves_empty_text_exactly() {
        let empty = super::content_fragments("").collect::<Vec<_>>();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].as_str(), "");
    }

    #[test]
    fn s24_content_fragmentation_preserves_multibyte_boundaries_exactly() {
        let text = format!(
            "{}\u{1f980}tail",
            "a".repeat(MAX_CONTENT_FRAGMENT_BYTES - 1)
        );
        let fragments = super::content_fragments(&text).collect::<Vec<_>>();
        assert_eq!(fragments.len(), 2);
        assert_eq!(
            fragments[0].as_str(),
            "a".repeat(MAX_CONTENT_FRAGMENT_BYTES - 1)
        );
        assert_eq!(fragments[1].as_str(), "\u{1f980}tail");
        assert_eq!(
            format!("{}{}", fragments[0].as_str(), fragments[1].as_str()),
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
    fn inv033_exact_newline_framing_is_enforced() -> Result<(), Box<dyn std::error::Error>> {
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
        Ok(())
    }

    #[test]
    fn inv033_oversized_complete_frame_preserves_recoverable_request_id() {
        let oversized =
            padded_oversized_client_frame(r#""request_id":"9""#, super::MAX_FRAME_BYTES);
        let error = decode_client_line(&oversized)
            .expect_err("a complete frame over the byte cap must be rejected");

        assert_eq!(error.kind(), FrameDecodeErrorKind::OversizedFrame);
        assert_eq!(error.request_id().value(), 9);
    }

    #[test]
    fn inv033_oversized_duplicate_request_identity_is_uncorrelated() {
        let oversized = padded_oversized_client_frame(
            r#""request_id":"9","request_id":"10""#,
            super::MAX_FRAME_BYTES,
        );
        let error =
            decode_client_line(&oversized).expect_err("a duplicate request identity must fail");

        assert_eq!(error.kind(), FrameDecodeErrorKind::OversizedFrame);
        assert_eq!(error.request_id().value(), 0);
    }

    #[test]
    fn inv033_oversized_noncanonical_request_identity_is_uncorrelated() {
        let oversized =
            padded_oversized_client_frame(r#""request_id":"09""#, super::MAX_FRAME_BYTES);
        let error =
            decode_client_line(&oversized).expect_err("a noncanonical request identity must fail");

        assert_eq!(error.kind(), FrameDecodeErrorKind::OversizedFrame);
        assert_eq!(error.request_id().value(), 0);
    }

    #[test]
    fn inv033_request_identity_recovery_stops_at_the_frame_cap() {
        let far_oversized =
            padded_oversized_client_frame(r#""request_id":"9""#, super::MAX_FRAME_BYTES * 2);
        let error = decode_client_line(&far_oversized)
            .expect_err("a frame beyond the recovery budget must be rejected");

        assert_eq!(error.kind(), FrameDecodeErrorKind::OversizedFrame);
        assert_eq!(error.request_id().value(), 0);
    }

    #[test]
    fn inv033_bounded_client_request_identity_recovery_matches_oversized_decode() {
        let oversized =
            padded_oversized_client_frame(r#""request_id":"9""#, super::MAX_FRAME_BYTES);
        let content = &oversized[..oversized.len() - 1];

        assert_eq!(super::recover_bounded_client_request_id(content).value(), 9);
        assert_eq!(
            super::recover_bounded_client_request_id(&oversized).value(),
            0
        );
    }

    #[test]
    fn inv033_all_client_request_variants_encode_with_current_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = ModelSelection::Direct {
            selection_id: uuid(3),
        };
        assert_client_request_current_version(
            request(1)?,
            ClientRequest::CreateSession {
                command_id: command(4)?,
                initial_model_selection: model,
                system_prompt: SystemPromptMember::absent(),
            },
        )?;
        assert_client_request_current_version(request(2)?, ClientRequest::ListSessions {})?;
        assert_client_request_current_version(
            request(3)?,
            ClientRequest::SubmitInput {
                command_id: command(5)?,
                session_id: uuid(6),
                content: InputContent::new(String::new()),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )?;
        assert_client_request_current_version(
            request(4)?,
            ClientRequest::ReadTranscript {
                session_id: uuid(6),
            },
        )?;
        assert_client_request_current_version(
            request(5)?,
            ClientRequest::FollowSession {
                session_id: uuid(6),
            },
        )?;
        Ok(())
    }

    #[test]
    fn inv033_version_four_metadata_requests_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let cases = [
            (
                ClientRequest::ListSessionMetadata {
                    required_tags: vec![String::from("daily"), String::from("work")],
                    title_contains: Some(String::from("Plan")),
                    include_archived: true,
                    page_size: CanonicalU64::new(25),
                    after_session_id: Some(uuid(6)),
                },
                r#"{"type":"list_session_metadata","required_tags":["daily","work"],"title_contains":"Plan","include_archived":true,"page_size":"25","after_session_id":"00000000-0000-0000-0000-000000000006"}"#,
            ),
            (
                ClientRequest::ReadSessionMetadata {
                    session_id: uuid(6),
                },
                r#"{"type":"read_session_metadata","session_id":"00000000-0000-0000-0000-000000000006"}"#,
            ),
            (
                ClientRequest::ReplaceSessionMetadata {
                    command_id: command(5)?,
                    session_id: uuid(6),
                    metadata: metadata(true)?,
                },
                r#"{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":"Planning","tags":["daily","work"],"attributes":{"run":"17","trigger":""},"archived":true}}"#,
            ),
        ];

        for (index, (request_value, request_json)) in cases.into_iter().enumerate() {
            for retained in [
                ProtocolVersion::One,
                ProtocolVersion::Two,
                ProtocolVersion::Three,
            ] {
                assert_eq!(
                    ClientFrame::try_new_for_version(
                        retained,
                        request(u64::try_from(index + 1)?)?,
                        request_value.clone(),
                    ),
                    Err(FrameValidationError::RequestRequiresNewerVersion)
                );
            }
            let request_id = request(u64::try_from(index + 1)?)?;
            let frame =
                ClientFrame::try_new_for_version(ProtocolVersion::Four, request_id, request_value)?;
            let encoded = encode_client_line(&frame)?;
            let expected = format!(
                "{{\"version\":{SESSION_METADATA_PROTOCOL_VERSION},\"request_id\":\"{}\",\"request\":{request_json}}}\n",
                request_id.value()
            );
            assert_eq!(String::from_utf8(encoded.clone())?, expected);
            assert_eq!(decode_client_line(&encoded)?, frame);
        }
        Ok(())
    }

    /// INV-033: the unified listing request is admitted only from version
    /// sixteen and keeps its exact closed shape across one round trip.
    #[test]
    fn inv033_version_sixteen_list_conversations_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::ListConversations {
            title_contains: Some(String::from("Plan")),
            origin: ConversationOriginFilter::All,
            include_archived: true,
            page_size: CanonicalU64::new(25),
            after: Some(ConversationCursor::new(
                ConversationOrigin::ImportedConversation,
                uuid(6),
            )),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::Twelve,
                request_id,
                request_value.clone(),
            ),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Sixteen, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            format!(
                "{{\"version\":{UNIFIED_CONVERSATION_LISTING_PROTOCOL_VERSION},\"request_id\":\"1\",\
                 \"request\":{{\"type\":\"list_conversations\",\"title_contains\":\"Plan\",\
                 \"origin\":\"all\",\"include_archived\":true,\"page_size\":\"25\",\
                 \"after\":{{\"origin\":\"imported_conversation\",\
                 \"conversation_id\":\"00000000-0000-0000-0000-000000000006\"}}}}}}\n"
            )
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: the nullable filter and cursor members are required, the
    /// origin filter is a closed set, and the cursor rejects unknown members.
    #[test]
    fn inv033_list_conversations_members_are_required_and_closed() {
        assert_client_malformed(
            r#"{"version":16,"request_id":"1","request":{"type":"list_conversations","origin":"all","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":16,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"50"}}"#,
        );
        assert_client_malformed(
            r#"{"version":16,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"everything","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":16,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"50","after":{"origin":"native_session","conversation_id":"00000000-0000-0000-0000-000000000006","extra":true}}}"#,
        );
    }

    /// INV-033: the empty-title, U+0000, and page-size bounds reject a
    /// listing request before application construction.
    #[test]
    fn inv033_list_conversations_validates_title_and_page_size() {
        assert_client_malformed(
            r#"{"version":16,"request_id":"1","request":{"type":"list_conversations","title_contains":"","origin":"all","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            "{\"version\":16,\"request_id\":\"1\",\"request\":{\"type\":\"list_conversations\",\"title_contains\":\"a\\u0000b\",\"origin\":\"all\",\"include_archived\":false,\"page_size\":\"50\",\"after\":null}}",
        );
        assert_client_malformed(
            r#"{"version":16,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"0","after":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":16,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"101","after":null}}"#,
        );
    }

    /// INV-033: a listing request carried under any retained version one
    /// through twelve is malformed rather than reinterpreted.
    #[test]
    fn inv033_list_conversations_is_rejected_below_version_sixteen() {
        assert_client_malformed(
            r#"{"version":12,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"50","after":null}}"#,
        );
    }

    /// INV-033: the three unified page messages exist only from version
    /// sixteen and keep their exact closed shapes across round trips.
    #[test]
    fn inv033_version_sixteen_conversation_page_messages_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::ConversationPageStart {},
            r#"{"type":"conversation_page_start"}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::ConversationSummary {
                conversation: ConversationSummary::NativeSession {
                    session_id: uuid(1),
                    title: Some(String::from("Planning")),
                    archived: false,
                    defaults_version: CanonicalU64::new(2),
                },
            },
            r#"{"type":"conversation_summary","conversation":{"origin":"native_session","session_id":"00000000-0000-0000-0000-000000000001","title":"Planning","archived":false,"defaults_version":"2"}}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::ConversationSummary {
                conversation: ConversationSummary::ImportedConversation {
                    imported_conversation_id: uuid(4),
                    title: Some(String::from("Imported plan")),
                    entry_count: CanonicalU64::new(7),
                    source_format: ImportedConversationSourceFormat::CodexRolloutJsonlV1,
                },
            },
            r#"{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":"Imported plan","entry_count":"7","source_format":"codex_rollout_jsonl_v1"}}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::ConversationSummary {
                conversation: ConversationSummary::ImportedConversation {
                    imported_conversation_id: uuid(4),
                    title: None,
                    entry_count: CanonicalU64::new(1),
                    source_format: ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV1,
                },
            },
            r#"{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":null,"entry_count":"1","source_format":"claude_code_session_jsonl_v1"}}"#,
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::ConversationPageEnd {
                conversation_count: CanonicalU64::new(2),
                next_after: Some(ConversationCursor::new(
                    ConversationOrigin::NativeSession,
                    uuid(1),
                )),
            },
            r#"{"type":"conversation_page_end","conversation_count":"2","next_after":{"origin":"native_session","conversation_id":"00000000-0000-0000-0000-000000000001"}}"#,
        )?;
        assert_server_message_round_trip(
            request(6)?,
            ServerMessage::ConversationPageEnd {
                conversation_count: CanonicalU64::new(0),
                next_after: None,
            },
            r#"{"type":"conversation_page_end","conversation_count":"0","next_after":null}"#,
        )?;
        Ok(())
    }

    /// INV-033: a page end never names a cursor for an empty page or a count
    /// beyond the page bound.
    #[test]
    fn inv033_conversation_page_end_shape_is_bounded() {
        assert_server_malformed(
            r#"{"version":16,"request_id":"1","message":{"type":"conversation_page_end","conversation_count":"0","next_after":{"origin":"native_session","conversation_id":"00000000-0000-0000-0000-000000000001"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":16,"request_id":"1","message":{"type":"conversation_page_end","conversation_count":"101","next_after":null}}"#,
        );
    }

    /// INV-033: a native summary title follows the metadata title rules and
    /// an imported summary restates the derived display-title shape.
    #[test]
    fn inv033_conversation_summary_shapes_are_validated() {
        assert_server_malformed(
            r#"{"version":16,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"native_session","session_id":"00000000-0000-0000-0000-000000000001","title":"","archived":false,"defaults_version":"1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":16,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":"line\nbreak","entry_count":"1","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":16,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":" padded","entry_count":"1","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":16,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":null,"entry_count":"0","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":16,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"native_session","session_id":"00000000-0000-0000-0000-000000000001","title":null,"archived":false,"defaults_version":"0"}}}"#,
        );
    }

    /// INV-033: an in-memory imported summary title beyond the scalar bound
    /// fails encoding rather than crossing the wire.
    #[test]
    fn inv033_conversation_summary_rejects_an_oversized_imported_title()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::ConversationSummary {
            conversation: ConversationSummary::ImportedConversation {
                imported_conversation_id: uuid(4),
                title: Some("x".repeat(MAX_IMPORTED_CONVERSATION_DISPLAY_TITLE_SCALARS + 1)),
                entry_count: CanonicalU64::new(1),
                source_format: ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV2,
            },
        };
        assert_eq!(
            ServerFrame::try_new_for_version(ProtocolVersion::Sixteen, request(1)?, message)
                .expect_err("oversized imported title must fail validation"),
            FrameValidationError::ConversationListShape
        );
        Ok(())
    }

    /// INV-033: the unified page messages are rejected under every retained
    /// version below sixteen.
    #[test]
    fn inv033_conversation_page_messages_require_version_sixteen()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::Twelve,
                request(1)?,
                ServerMessage::ConversationPageStart {},
            )
            .expect_err("a retained version must reject the newer message"),
            FrameValidationError::MessageRequiresNewerVersion
        );
        assert_server_malformed(
            r#"{"version":12,"request_id":"1","message":{"type":"conversation_page_start"}}"#,
        );
        Ok(())
    }

    #[test]
    fn inv033_version_five_import_request_preserves_exact_bytes_and_format()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::ImportConversation {
            format: ConversationImportFormat::ClaudeCodeSessionJsonlV2,
            source: ConversationImportSource::new(vec![0, 255]),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::Four,
                request_id,
                request_value.clone(),
            ),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Five, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":5,\"request_id\":\"1\",\"request\":{\"type\":\"import_conversation\",\
             \"format\":\"claude_code_session_jsonl_v2\",\"source\":\"AP8=\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: version ten admits imported-frontier session creation with its
    /// exact closed request shape while the reserved gap remains unsupported.
    #[test]
    fn inv033_version_ten_imported_frontier_creation_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::CreateSessionFromImportedFrontier {
            command_id: command(4)?,
            imported_conversation_id: uuid(5),
            through_position: CanonicalU64::new(2),
            relationship: ImportedSessionRelationship::Resume,
            initial_model_selection: ModelSelection::Direct {
                selection_id: uuid(6),
            },
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::Seven,
                request_id,
                request_value.clone(),
            ),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Ten, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":10,\"request_id\":\"1\",\"request\":{\"type\":\"create_session_from_imported_frontier\",\"command_id\":\"00000000-0000-0000-0000-000000000004\",\"imported_conversation_id\":\"00000000-0000-0000-0000-000000000005\",\"through_position\":\"2\",\"relationship\":\"resume\",\"initial_model_selection\":{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000006\"}}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: version eleven retains version ten's imported-frontier request.
    #[test]
    fn inv033_version_eleven_retains_version_ten_imported_frontier_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Eleven,
            request(1)?,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command(4)?,
                imported_conversation_id: uuid(5),
                through_position: CanonicalU64::new(2),
                relationship: ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: uuid(6),
                },
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert_eq!(frame.version(), ProtocolVersion::Eleven);
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: a version-ten imported-frontier request rejects position zero.
    #[test]
    fn inv033_version_ten_imported_frontier_rejects_zero_position()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Ten,
            request(2)?,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command(4)?,
                imported_conversation_id: uuid(5),
                through_position: CanonicalU64::new(0),
                relationship: ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: uuid(6),
                },
            },
        );

        assert_eq!(frame, Err(FrameValidationError::ImportedFrontierShape));
        Ok(())
    }

    /// INV-033: version ten retains every earlier request unchanged.
    #[test]
    fn inv033_version_ten_retains_the_earlier_request_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Ten,
            request(1)?,
            ClientRequest::SubmitInput {
                command_id: command(4)?,
                session_id: uuid(6),
                content: InputContent::new(String::from("ordinary work")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":10,"));
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: version ten retains version eight's turn-control vocabulary.
    #[test]
    fn inv033_version_ten_retains_version_eight_turn_control_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Ten,
            request(1)?,
            ClientRequest::StopTurn {
                command_id: command(4)?,
                session_id: uuid(6),
                expected_active_turn_id: uuid(7),
                content: InputContent::new(String::from("continue after the stop")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":10,"));
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: the reconciliation request is admitted only by version seven
    /// and keeps its exact closed shape across one encode/decode round trip.
    #[test]
    fn inv033_version_seven_reconcile_turn_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::ReconcileTurn {
            command_id: command(4)?,
            session_id: uuid(6),
            expected_active_turn_id: uuid(7),
            content: InputContent::new(String::from("continue after reconciliation")),
            expected_defaults_version: CanonicalU64::new(1),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::Six,
                request_id,
                request_value.clone(),
            ),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Seven, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":7,\"request_id\":\"1\",\"request\":{\"type\":\"reconcile_turn\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"expected_active_turn_id\":\"00000000-0000-0000-0000-000000000007\",\
             \"content\":\"continue after reconciliation\",\
             \"expected_defaults_version\":\"1\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: version seven retains every earlier request unchanged.
    #[test]
    fn inv033_version_seven_retains_the_earlier_request_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Seven,
            request(1)?,
            ClientRequest::SubmitInput {
                command_id: command(4)?,
                session_id: uuid(6),
                content: InputContent::new(String::from("ordinary work")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":7,"));
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: the reconciliation refusal and the stale-target rejection carry
    /// their exact closed wire shapes.
    #[test]
    fn inv033_reconciliation_rejection_details_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the named turn is not awaiting reconciliation"),
                detail: ErrorDetail::rejected(RejectionDetail::TurnNotAwaitingReconciliation {
                    session_id: uuid(6),
                    turn_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"the named turn is not awaiting reconciliation","detail":{"type":"turn_not_awaiting_reconciliation","session_id":"00000000-0000-0000-0000-000000000006","turn_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the expected active turn is stale"),
                detail: ErrorDetail::rejected(RejectionDetail::ActiveTurnMismatch {
                    session_id: uuid(6),
                    expected_active_turn_id: uuid(7),
                    active_turn_id: uuid(8),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"the expected active turn is stale","detail":{"type":"active_turn_mismatch","session_id":"00000000-0000-0000-0000-000000000006","expected_active_turn_id":"00000000-0000-0000-0000-000000000007","active_turn_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("a racing decision already released the slot"),
                detail: ErrorDetail::rejected(RejectionDetail::NoActiveTurn {
                    session_id: uuid(6),
                    expected_active_turn_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"a racing decision already released the slot","detail":{"type":"no_active_turn","session_id":"00000000-0000-0000-0000-000000000006","expected_active_turn_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )
    }

    #[test]
    fn inv033_version_five_import_source_requires_canonical_padded_base64() {
        assert_client_malformed(
            r#"{"version":5,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v1","source":"AA"}}"#,
        );
        assert_client_malformed(
            r#"{"version":5,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v1","source":"AB=="}}"#,
        );
        assert_client_malformed(
            r#"{"version":5,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v1","source":"AA==="}}"#,
        );
    }

    #[test]
    fn inv033_version_eleven_retains_baseline_submit_exchange()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Eleven,
            request_id,
            ClientRequest::SubmitInput {
                command_id: command(2)?,
                session_id: uuid(3),
                content: InputContent::new(String::from("content")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )?;
        let encoded_request = encode_client_line(&request_frame)?;
        assert_eq!(decode_client_line(&encoded_request)?, request_frame);
        let response_frame = ServerFrame::try_new_for_version(
            ProtocolVersion::Eleven,
            request_id,
            ServerMessage::InputSubmitted {
                session_id: uuid(3),
                accepted_input_id: uuid(4),
                acceptance_position: CanonicalU64::new(1),
                turn_id: uuid(5),
            },
        )?;
        let encoded_response = encode_server_line(&response_frame)?;
        assert_eq!(decode_server_line(&encoded_response)?, response_frame);
        Ok(())
    }

    /// INV-033: version twelve retains version eleven's request vocabulary and
    /// admits the cursorless provider-text message only at its new boundary.
    #[test]
    fn inv033_version_twelve_adds_only_the_provider_text_message()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let retained_request = ClientFrame::try_new_for_version(
            ProtocolVersion::Twelve,
            request_id,
            ClientRequest::ReadReviewTarget { target_id: uuid(2) },
        )?;
        let encoded_request = encode_client_line(&retained_request)?;
        let delta = ServerMessage::ProviderTextDelta {
            session_id: uuid(3),
            turn_id: uuid(4),
            model_call_id: uuid(5),
            part_index: CanonicalU64::new(6),
            content: ContentFragment::try_new(String::from("already [redacted]"))?,
        };

        assert_eq!(decode_client_line(&encoded_request)?, retained_request);
        assert_eq!(
            ServerFrame::try_new_for_version(ProtocolVersion::Eleven, request_id, delta.clone(),),
            Err(FrameValidationError::MessageRequiresNewerVersion)
        );
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::Twelve, request_id, delta)?;
        let encoded_delta = encode_server_line(&frame)?;
        assert_eq!(decode_server_line(&encoded_delta)?, frame);
        Ok(())
    }

    /// INV-033: review target registration enters the closed vocabulary only
    /// at version eleven and retains its exact nullable snapshot shape.
    #[test]
    fn inv033_version_eleven_review_target_exchange_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::CreateReviewTarget {
            command_id: command(2)?,
            target_id: uuid(3),
            provider: String::from("example-host"),
            repository: String::from("owner/repository"),
            subject: ReviewTargetSubject::ChangeRequest {
                number: CanonicalU64::new(42),
            },
            head_revision: String::from("head-revision"),
            base_revision: Some(String::from("base-revision")),
            stack_parent_target_id: None,
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::Ten,
                request_id,
                request_value.clone(),
            ),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Eleven, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":11,\"request_id\":\"1\",\"request\":{\"type\":\"create_review_target\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"target_id\":\"00000000-0000-0000-0000-000000000003\",\
             \"provider\":\"example-host\",\"repository\":\"owner/repository\",\
             \"subject\":{\"kind\":\"change_request\",\"number\":\"42\"},\
             \"head_revision\":\"head-revision\",\"base_revision\":\"base-revision\",\
             \"stack_parent_target_id\":null}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        assert_server_message_round_trip(
            request_id,
            ServerMessage::ReviewTargetCreated { target_id: uuid(3) },
            r#"{"type":"review_target_created","target_id":"00000000-0000-0000-0000-000000000003"}"#,
        )
    }

    #[test]
    fn inv033_inv046_version_six_adds_forward_only_defaults_replacement()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_malformed(
            r#"{"version":5,"request_id":"6","request":{"type":"replace_session_defaults","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","expected_defaults_version":"3","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}}"#,
        );
        assert_server_malformed(
            r#"{"version":5,"request_id":"7","message":{"type":"session_defaults_replaced","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"4","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}}"#,
        );
        assert_server_malformed(
            r#"{"version":5,"request_id":"8","message":{"type":"transcript_entry","entry_index":"5","source_session_id":"00000000-0000-0000-0000-000000000002","entry_id":"00000000-0000-0000-0000-000000000006","entry":{"type":"model_identity_changed","turn_id":"00000000-0000-0000-0000-000000000007","defaults_version":"4","selected_model_id":"00000000-0000-0000-0000-000000000004"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":5,"request_id":"9","message":{"type":"error","code":"rejected","message":"defaults version exhausted","detail":{"type":"defaults_version_exhausted","session_id":"00000000-0000-0000-0000-000000000002","current":"18446744073709551615"}}}"#,
        );
        let request_id = request(6)?;
        let request_value = ClientRequest::ReplaceSessionDefaults {
            command_id: command(1)?,
            session_id: uuid(2),
            expected_defaults_version: CanonicalU64::new(3),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            dangerous_tool_auto_approval: true,
            system_prompt: SystemPromptMember::absent(),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::Five,
                request_id,
                request_value.clone(),
            ),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Six, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":6,\"request_id\":\"6\",\"request\":{\"type\":\"replace_session_defaults\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"expected_defaults_version\":\"3\",\"model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"dangerous_tool_auto_approval\":true}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let replacement_receipt = ServerMessage::SessionDefaultsReplaced {
            session_id: uuid(2),
            defaults_version: CanonicalU64::new(4),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            dangerous_tool_auto_approval: true,
            system_prompt: SystemPromptMember::absent(),
        };
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::Five,
                request(7)?,
                replacement_receipt.clone(),
            ),
            Err(FrameValidationError::MessageRequiresNewerVersion)
        );
        assert_server_message_round_trip(
            request(7)?,
            replacement_receipt,
            r#"{"type":"session_defaults_replaced","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"4","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}"#,
        )?;
        let model_identity_entry = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(5),
            source_session_id: uuid(2),
            entry_id: uuid(6),
            entry: TranscriptEntry::ModelIdentityChanged {
                turn_id: uuid(7),
                defaults_version: CanonicalU64::new(4),
                selected_model_id: uuid(4),
            },
        };
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::Five,
                request(8)?,
                model_identity_entry.clone(),
            ),
            Err(FrameValidationError::MessageRequiresNewerVersion)
        );
        assert_server_message_round_trip(
            request(8)?,
            model_identity_entry,
            r#"{"type":"transcript_entry","entry_index":"5","source_session_id":"00000000-0000-0000-0000-000000000002","entry_id":"00000000-0000-0000-0000-000000000006","entry":{"type":"model_identity_changed","turn_id":"00000000-0000-0000-0000-000000000007","defaults_version":"4","selected_model_id":"00000000-0000-0000-0000-000000000004"}}"#,
        )?;
        let exhaustion = ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("defaults version exhausted"),
            detail: ErrorDetail::rejected(RejectionDetail::DefaultsVersionExhausted {
                session_id: uuid(2),
                current: CanonicalU64::new(u64::MAX),
            }),
        };
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::Five,
                request(9)?,
                exhaustion.clone(),
            ),
            Err(FrameValidationError::MessageRequiresNewerVersion)
        );
        assert_server_message_round_trip(
            request(9)?,
            exhaustion,
            r#"{"type":"error","code":"rejected","message":"defaults version exhausted","detail":{"type":"defaults_version_exhausted","session_id":"00000000-0000-0000-0000-000000000002","current":"18446744073709551615"}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_inv046_version_nine_adds_the_bounded_session_system_prompt()
    -> Result<(), Box<dyn std::error::Error>> {
        // The member is not admitted below version nine.
        assert_client_malformed(
            r#"{"version":6,"request_id":"1","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"system_prompt":"exact prompt text"}}"#,
        );
        assert_client_malformed(
            r#"{"version":6,"request_id":"2","request":{"type":"replace_session_defaults","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","expected_defaults_version":"3","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true,"system_prompt":null}}"#,
        );
        // A version-nine frame must carry the member explicitly.
        assert_client_malformed(
            r#"{"version":9,"request_id":"3","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"}}}"#,
        );
        assert_client_malformed(
            r#"{"version":9,"request_id":"4","request":{"type":"replace_session_defaults","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","expected_defaults_version":"3","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}}"#,
        );
        // The defaults read is not admitted below version nine, and its
        // version member is required nullable.
        assert_client_malformed(
            r#"{"version":6,"request_id":"5","request":{"type":"read_session_defaults","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":9,"request_id":"5","request":{"type":"read_session_defaults","session_id":"00000000-0000-0000-0000-000000000002"}}"#,
        );
        // A present prompt is nonempty and rejects U+0000.
        assert_client_malformed(
            r#"{"version":9,"request_id":"6","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"system_prompt":""}}"#,
        );
        assert_client_malformed(
            "{\"version\":9,\"request_id\":\"7\",\"request\":{\"type\":\"create_session\",\"command_id\":\"00000000-0000-0000-0000-000000000001\",\"initial_model_selection\":{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\"system_prompt\":\"a\\u0000b\"}}",
        );

        let request_id = request(8)?;
        let create = ClientRequest::CreateSession {
            command_id: command(1)?,
            initial_model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
                "exact prompt text".to_owned(),
            )?)),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::Six, request_id, create.clone()),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );
        let frame = ClientFrame::try_new_for_version(ProtocolVersion::Nine, request_id, create)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":9,\"request_id\":\"8\",\"request\":{\"type\":\"create_session\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"initial_model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"system_prompt\":\"exact prompt text\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let promptless_create = ClientRequest::CreateSession {
            command_id: command(1)?,
            initial_model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            system_prompt: SystemPromptMember::present(None),
        };
        let promptless_frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Nine, request_id, promptless_create)?;
        let promptless_encoded = encode_client_line(&promptless_frame)?;
        assert_eq!(
            String::from_utf8(promptless_encoded.clone())?,
            "{\"version\":9,\"request_id\":\"8\",\"request\":{\"type\":\"create_session\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"initial_model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"system_prompt\":null}}\n"
        );
        assert_eq!(decode_client_line(&promptless_encoded)?, promptless_frame);
        let decoded_null = decode_client_line(&line(
            r#"{"version":9,"request_id":"8","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"system_prompt":null}}"#,
        ))?;
        let ClientRequest::CreateSession { system_prompt, .. } = decoded_null.request() else {
            panic!("decoded frame must be a create request");
        };
        assert_eq!(system_prompt.value(), Some(&None));

        let replace = ClientRequest::ReplaceSessionDefaults {
            command_id: command(1)?,
            session_id: uuid(2),
            expected_defaults_version: CanonicalU64::new(3),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            dangerous_tool_auto_approval: false,
            system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
                "exact prompt text".to_owned(),
            )?)),
        };
        let frame = ClientFrame::try_new_for_version(ProtocolVersion::Nine, request(9)?, replace)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":9,\"request_id\":\"9\",\"request\":{\"type\":\"replace_session_defaults\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"expected_defaults_version\":\"3\",\"model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"dangerous_tool_auto_approval\":false,\
             \"system_prompt\":\"exact prompt text\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let read_current = ClientRequest::ReadSessionDefaults {
            session_id: uuid(2),
            defaults_version: None,
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::Six,
                request(10)?,
                read_current.clone()
            ),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Nine, request(10)?, read_current)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":9,\"request_id\":\"10\",\"request\":{\"type\":\"read_session_defaults\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"defaults_version\":null}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let read_named = ClientRequest::ReadSessionDefaults {
            session_id: uuid(2),
            defaults_version: Some(CanonicalU64::new(3)),
        };
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Nine, request(11)?, read_named)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":9,\"request_id\":\"11\",\"request\":{\"type\":\"read_session_defaults\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"defaults_version\":\"3\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let receipt = ServerMessage::SessionDefaultsReplaced {
            session_id: uuid(2),
            defaults_version: CanonicalU64::new(4),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            dangerous_tool_auto_approval: true,
            system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
                "exact prompt text".to_owned(),
            )?)),
        };
        assert_eq!(
            ServerFrame::try_new_for_version(ProtocolVersion::Six, request(12)?, receipt.clone()),
            Err(FrameValidationError::MessageRequiresNewerVersion)
        );
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::Nine, request(12)?, receipt)?;
        let encoded = encode_server_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":9,\"request_id\":\"12\",\"message\":{\"type\":\"session_defaults_replaced\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"defaults_version\":\"4\",\"model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"dangerous_tool_auto_approval\":true,\
             \"system_prompt\":\"exact prompt text\"}}\n"
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
        // A version-nine receipt without the member is rejected.
        assert_server_malformed(
            r#"{"version":9,"request_id":"12","message":{"type":"session_defaults_replaced","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"4","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}}"#,
        );

        let defaults_read = ServerMessage::SessionDefaults {
            session_id: uuid(2),
            defaults_version: CanonicalU64::new(4),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            dangerous_tool_auto_approval: false,
            system_prompt: Some(SystemPromptText::try_new("exact prompt text".to_owned())?),
        };
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::Six,
                request(13)?,
                defaults_read.clone(),
            ),
            Err(FrameValidationError::MessageRequiresNewerVersion)
        );
        assert_server_message_round_trip(
            request(13)?,
            defaults_read,
            r#"{"type":"session_defaults","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"4","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":false,"system_prompt":"exact prompt text"}"#,
        )?;
        assert_server_message_round_trip(
            request(14)?,
            ServerMessage::SessionDefaults {
                session_id: uuid(2),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                dangerous_tool_auto_approval: false,
                system_prompt: None,
            },
            r#"{"type":"session_defaults","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"1","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":false,"system_prompt":null}"#,
        )?;
        Ok(())
    }

    /// INV-033: the wire prompt admits exactly the 1,048,576-byte bound,
    /// splitting at a multibyte scalar so the byte measure is what binds.
    #[test]
    fn inv033_system_prompt_text_binds_at_the_exact_utf8_byte_bound() {
        let exact = "y".repeat(MAX_SYSTEM_PROMPT_UTF8_BYTES - '√'.len_utf8()) + "√";
        assert_eq!(exact.len(), MAX_SYSTEM_PROMPT_UTF8_BYTES);
        let admitted = SystemPromptText::try_new(exact.clone()).expect("exact cap is admitted");
        assert_eq!(admitted.as_str(), exact);

        let oversized = exact + "y";
        assert!(SystemPromptText::try_new(oversized).is_err());
        assert!(SystemPromptText::try_new(String::new()).is_err());
        assert!(SystemPromptText::try_new("a\u{0}b".to_owned()).is_err());
    }

    /// INV-033: the admitted version set is closed exactly at one through
    /// twelve plus sixteen; thirteen, fourteen, and fifteen remain reserved
    /// by concurrent protocol stacks and are not admitted here.
    #[test]
    fn inv033_version_sixteen_completes_the_admitted_set() {
        assert_eq!(
            ProtocolVersion::Nine.as_u64(),
            SESSION_SYSTEM_PROMPT_PROTOCOL_VERSION
        );
        assert_eq!(
            ProtocolVersion::Sixteen.as_u64(),
            UNIFIED_CONVERSATION_LISTING_PROTOCOL_VERSION
        );
        assert_eq!(ProtocolVersion::from_u64(8), Some(ProtocolVersion::Eight));
        assert_eq!(ProtocolVersion::from_u64(9), Some(ProtocolVersion::Nine));
        assert_eq!(ProtocolVersion::from_u64(10), Some(ProtocolVersion::Ten));
        assert_eq!(ProtocolVersion::from_u64(11), Some(ProtocolVersion::Eleven));
        assert_eq!(ProtocolVersion::from_u64(12), Some(ProtocolVersion::Twelve));
        assert_eq!(ProtocolVersion::from_u64(13), None);
        assert_eq!(ProtocolVersion::from_u64(14), None);
        assert_eq!(ProtocolVersion::from_u64(15), None);
        assert_eq!(
            ProtocolVersion::from_u64(16),
            Some(ProtocolVersion::Sixteen)
        );
        assert_eq!(ProtocolVersion::from_u64(17), None);
    }

    #[test]
    fn inv033_version_five_import_outcomes_have_distinct_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::ConversationImportInserted {
                imported_conversation_id: uuid(2),
            },
            r#"{"type":"conversation_import_inserted","imported_conversation_id":"00000000-0000-0000-0000-000000000002"}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::ConversationImportAlreadyImported {
                imported_conversation_id: uuid(3),
            },
            r#"{"type":"conversation_import_already_imported","imported_conversation_id":"00000000-0000-0000-0000-000000000003"}"#,
        )?;
        Ok(())
    }

    /// INV-033: the stop request is admitted only by version eight and keeps
    /// its exact closed shape across one encode/decode round trip.
    #[test]
    fn inv033_version_eight_stop_turn_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::StopTurn {
            command_id: command(4)?,
            session_id: uuid(6),
            expected_active_turn_id: uuid(7),
            content: InputContent::new(String::from("continue after the stop")),
            expected_defaults_version: CanonicalU64::new(1),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::Six,
                request_id,
                request_value.clone(),
            ),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Eight, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":8,\"request_id\":\"1\",\"request\":{\"type\":\"stop_turn\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"expected_active_turn_id\":\"00000000-0000-0000-0000-000000000007\",\
             \"content\":\"continue after the stop\",\
             \"expected_defaults_version\":\"1\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: both closed decision shapes are admitted only by version eight
    /// and keep their exact wire forms across one encode/decode round trip.
    #[test]
    fn inv033_version_eight_decide_tool_request_has_exact_closed_decision_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let approval = ClientRequest::DecideToolRequest {
            command_id: command(4)?,
            session_id: uuid(6),
            tool_request_id: uuid(7),
            decision: ToolDecision::Approve {},
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::Six, request(1)?, approval.clone()),
            Err(FrameValidationError::RequestRequiresNewerVersion)
        );
        let approval_frame =
            ClientFrame::try_new_for_version(ProtocolVersion::Eight, request(1)?, approval)?;
        let encoded_approval = encode_client_line(&approval_frame)?;
        assert_eq!(
            String::from_utf8(encoded_approval.clone())?,
            "{\"version\":8,\"request_id\":\"1\",\"request\":{\"type\":\"decide_tool_request\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"tool_request_id\":\"00000000-0000-0000-0000-000000000007\",\
             \"decision\":{\"type\":\"approve\"}}}\n"
        );
        assert_eq!(decode_client_line(&encoded_approval)?, approval_frame);

        let denial_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Eight,
            request(2)?,
            ClientRequest::DecideToolRequest {
                command_id: command(5)?,
                session_id: uuid(6),
                tool_request_id: uuid(7),
                decision: ToolDecision::Deny {
                    reason: String::from("writes outside the workspace"),
                },
            },
        )?;
        let encoded_denial = encode_client_line(&denial_frame)?;
        assert_eq!(
            String::from_utf8(encoded_denial.clone())?,
            "{\"version\":8,\"request_id\":\"2\",\"request\":{\"type\":\"decide_tool_request\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000005\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"tool_request_id\":\"00000000-0000-0000-0000-000000000007\",\
             \"decision\":{\"type\":\"deny\",\"reason\":\"writes outside the workspace\"}}}\n"
        );
        assert_eq!(decode_client_line(&encoded_denial)?, denial_frame);

        assert_client_malformed(
            r#"{"version":8,"request_id":"3","request":{"type":"decide_tool_request","command_id":"00000000-0000-0000-0000-000000000004","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"approve","reason":"approve carries no reason"}}}"#,
        );
        assert_client_malformed(
            r#"{"version":8,"request_id":"4","request":{"type":"decide_tool_request","command_id":"00000000-0000-0000-0000-000000000004","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"deny"}}}"#,
        );
        Ok(())
    }

    /// INV-033: version eight retains every earlier request unchanged.
    #[test]
    fn inv033_version_eight_retains_the_earlier_request_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::Eight,
            request(1)?,
            ClientRequest::SubmitInput {
                command_id: command(4)?,
                session_id: uuid(6),
                content: InputContent::new(String::from("ordinary work")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":8,"));
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: every stop rejection carries its exact closed wire shape.
    #[test]
    fn inv033_stop_rejection_details_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("no turn held the session slot"),
                detail: ErrorDetail::rejected(RejectionDetail::NoActiveTurn {
                    session_id: uuid(6),
                    expected_active_turn_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"no turn held the session slot","detail":{"type":"no_active_turn","session_id":"00000000-0000-0000-0000-000000000006","expected_active_turn_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the expected active turn is stale"),
                detail: ErrorDetail::rejected(RejectionDetail::ActiveTurnMismatch {
                    session_id: uuid(6),
                    expected_active_turn_id: uuid(7),
                    active_turn_id: uuid(8),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"the expected active turn is stale","detail":{"type":"active_turn_mismatch","session_id":"00000000-0000-0000-0000-000000000006","expected_active_turn_id":"00000000-0000-0000-0000-000000000007","active_turn_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("a stop was already applied"),
                detail: ErrorDetail::rejected(RejectionDetail::InterruptAlreadyApplied {
                    session_id: uuid(6),
                    active_turn_id: uuid(7),
                    existing_command_id: uuid(9),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"a stop was already applied","detail":{"type":"interrupt_already_applied","session_id":"00000000-0000-0000-0000-000000000006","active_turn_id":"00000000-0000-0000-0000-000000000007","existing_command_id":"00000000-0000-0000-0000-000000000009"}}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the active turn awaits a tool decision"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::InterruptUnavailableWhileAwaitingApproval {
                        session_id: uuid(6),
                        active_turn_id: uuid(7),
                    },
                ),
            },
            r#"{"type":"error","code":"rejected","message":"the active turn awaits a tool decision","detail":{"type":"interrupt_unavailable_while_awaiting_approval","session_id":"00000000-0000-0000-0000-000000000006","active_turn_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )
    }

    /// INV-033: the decision receipt and every decision rejection carry their
    /// exact closed wire shapes, admitted only by version eight.
    #[test]
    fn inv033_tool_decision_responses_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let approval_receipt = ServerMessage::ToolRequestDecided {
            tool_request_id: uuid(7),
            decision: ToolDecision::Approve {},
        };
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::Six,
                request(1)?,
                approval_receipt.clone(),
            ),
            Err(FrameValidationError::MessageRequiresNewerVersion)
        );
        assert_server_message_round_trip(
            request(1)?,
            approval_receipt,
            r#"{"type":"tool_request_decided","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"approve"}}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::ToolRequestDecided {
                tool_request_id: uuid(7),
                decision: ToolDecision::Deny {
                    reason: String::from("writes outside the workspace"),
                },
            },
            r#"{"type":"tool_request_decided","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"deny","reason":"writes outside the workspace"}}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("no logical request had this identity"),
                detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotFound {
                    tool_request_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"no logical request had this identity","detail":{"type":"tool_request_not_found","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the request already has a resolution"),
                detail: ErrorDetail::rejected(RejectionDetail::ToolRequestAlreadyResolved {
                    tool_request_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"the request already has a resolution","detail":{"type":"tool_request_already_resolved","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("an earlier request awaits its decision"),
                detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotEarliestUndecided {
                    tool_request_id: uuid(7),
                    earliest_tool_request_id: uuid(8),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"an earlier request awaits its decision","detail":{"type":"tool_request_not_earliest_undecided","tool_request_id":"00000000-0000-0000-0000-000000000007","earliest_tool_request_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(6)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the request is not owned by the session"),
                detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotInSession {
                    session_id: uuid(6),
                    tool_request_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"the request is not owned by the session","detail":{"type":"tool_request_not_in_session","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )
    }

    #[test]
    fn inv033_metadata_list_requires_title_query_member() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_list_requires_cursor_member() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"50"}}"#,
        );
    }

    #[test]
    fn inv033_metadata_list_rejects_duplicate_required_tags() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"list_session_metadata","required_tags":["same","same"],"title_contains":null,"include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_list_rejects_empty_title_query() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":"","include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_list_rejects_zero_page_size() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"0","after_session_id":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_list_rejects_page_size_over_bound() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"101","after_session_id":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_replacement_rejects_empty_title() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":"","tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_replacement_requires_title_member() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_replacement_rejects_duplicate_tags() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":null,"tags":["same","same"],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_duplicate_metadata_attribute_member_is_malformed() {
        assert_client_malformed(
            r#"{"version":4,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":null,"tags":[],"attributes":{"same":"first","\u0073ame":"second"},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_required_tag_deserializer_rejects_member_beyond_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let required_tags = serde_json::to_string(&numbered_metadata_strings(
            MAX_SESSION_METADATA_REQUIRED_TAGS + 1,
        ))?;
        let json = format!(
            r#"{{"version":4,"request_id":"1","request":{{"type":"list_session_metadata","required_tags":{required_tags},"title_contains":null,"include_archived":false,"page_size":"50","after_session_id":null}}}}"#
        );
        assert_client_malformed(&json);
        Ok(())
    }

    #[test]
    fn inv033_metadata_required_tag_deserializer_accepts_exact_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let required_tags = serde_json::to_string(&numbered_metadata_strings(
            MAX_SESSION_METADATA_REQUIRED_TAGS,
        ))?;
        let json = format!(
            r#"{{"version":4,"request_id":"1","request":{{"type":"list_session_metadata","required_tags":{required_tags},"title_contains":null,"include_archived":false,"page_size":"50","after_session_id":null}}}}"#
        );
        decode_client_line(&line(&json))?;
        Ok(())
    }

    #[test]
    fn inv033_metadata_summary_tag_deserializer_rejects_member_beyond_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let tags =
            serde_json::to_string(&numbered_metadata_strings(MAX_SESSION_METADATA_TAGS + 1))?;
        let json = format!(
            r#"{{"version":4,"request_id":"1","message":{{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"}},"dangerous_tool_auto_approval":false,"title":null,"tags":{tags},"archived":false,"last_writer":{{"updated_at_unix_micros":"1","actor":{{"type":"owner"}}}}}}}}"#
        );
        assert_server_malformed(&json);
        Ok(())
    }

    #[test]
    fn inv033_metadata_summary_tag_deserializer_accepts_exact_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut exact_tags = numbered_metadata_strings(MAX_SESSION_METADATA_TAGS);
        exact_tags.sort();
        let tags = serde_json::to_string(&exact_tags)?;
        let json = format!(
            r#"{{"version":4,"request_id":"1","message":{{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"}},"dangerous_tool_auto_approval":false,"title":null,"tags":{tags},"archived":false,"last_writer":{{"updated_at_unix_micros":"1","actor":{{"type":"owner"}}}}}}}}"#
        );
        decode_server_line(&line(&json))?;
        Ok(())
    }

    #[test]
    fn inv033_metadata_writer_actor_is_owner_only() {
        assert_server_malformed(
            r#"{"version":4,"request_id":"1","message":{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":{"updated_at_unix_micros":"1","actor":{"type":"model","turn_id":"00000000-0000-0000-0000-000000000002"}}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_capacity_matches_domain_and_frame_headroom()
    -> Result<(), Box<dyn std::error::Error>> {
        let exact = SessionMetadata::try_new(
            Some("\u{1}".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES)),
            Vec::new(),
            Vec::new(),
            false,
        )?;
        assert!(
            SessionMetadata::try_new(
                Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES + 1)),
                Vec::new(),
                Vec::new(),
                false,
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new(
                None,
                vec!["x".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1)],
                Vec::new(),
                false,
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new(
                None,
                numbered_metadata_strings(MAX_SESSION_METADATA_TAGS + 1),
                Vec::new(),
                false,
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new(
                None,
                Vec::new(),
                numbered_metadata_attributes(MAX_SESSION_METADATA_ATTRIBUTES + 1),
                false,
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new(
                None,
                Vec::new(),
                vec![(
                    "x".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1),
                    String::new(),
                )],
                false,
            )
            .is_err()
        );

        let encoded = encode_server_line(&ServerFrame::try_new_for_version(
            ProtocolVersion::Four,
            request(1)?,
            ServerMessage::SessionMetadataReplaced {
                session_id: uuid(1),
                metadata: exact,
                last_writer: MetadataLastWriter::new(CanonicalU64::new(1), MetadataActor::Owner {}),
            },
        )?)?;
        assert!(encoded.len() < super::MAX_FRAME_BYTES);
        Ok(())
    }

    #[test]
    fn inv033_metadata_filter_capacity_is_enforced_before_mapping()
    -> Result<(), Box<dyn std::error::Error>> {
        let exact = ClientRequest::ListSessionMetadata {
            required_tags: Vec::new(),
            title_contains: Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES)),
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after_session_id: None,
        };
        ClientFrame::try_new_for_version(ProtocolVersion::Four, request(1)?, exact)?;

        let over_total = ClientRequest::ListSessionMetadata {
            required_tags: Vec::new(),
            title_contains: Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES + 1)),
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after_session_id: None,
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::Four, request(1)?, over_total),
            Err(FrameValidationError::MetadataShape)
        );

        let over_indexed = ClientRequest::ListSessionMetadata {
            required_tags: vec!["x".repeat(MAX_SESSION_METADATA_INDEXED_UTF8_BYTES + 1)],
            title_contains: None,
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after_session_id: None,
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::Four, request(1)?, over_indexed),
            Err(FrameValidationError::MetadataShape)
        );

        let over_cardinality = ClientRequest::ListSessionMetadata {
            required_tags: numbered_metadata_strings(MAX_SESSION_METADATA_REQUIRED_TAGS + 1),
            title_contains: None,
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after_session_id: None,
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::Four, request(1)?, over_cardinality,),
            Err(FrameValidationError::MetadataShape)
        );
        Ok(())
    }

    #[test]
    fn inv033_metadata_summary_enforces_aggregate_utf8_capacity()
    -> Result<(), Box<dyn std::error::Error>> {
        let individually_valid_but_oversized = ServerMessage::SessionMetadataSummary {
            session_id: uuid(1),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(2),
            },
            dangerous_tool_auto_approval: false,
            title: Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES)),
            tags: vec![String::from("tag")],
            archived: false,
            last_writer: Some(MetadataLastWriter::new(
                CanonicalU64::new(1),
                MetadataActor::Owner {},
            )),
        };

        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::Four,
                request(1)?,
                individually_valid_but_oversized,
            ),
            Err(FrameValidationError::MetadataShape)
        );
        Ok(())
    }

    #[test]
    fn inv033_metadata_summary_requires_nullable_title_member() {
        assert_server_malformed(
            r#"{"version":4,"request_id":"1","message":{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"},"dangerous_tool_auto_approval":false,"tags":[],"archived":false,"last_writer":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_summary_requires_nullable_last_writer_member() {
        assert_server_malformed(
            r#"{"version":4,"request_id":"1","message":{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"},"dangerous_tool_auto_approval":false,"title":null,"tags":[],"archived":false}}"#,
        );
    }

    #[test]
    fn inv033_metadata_page_end_requires_nullable_cursor_member() {
        assert_server_malformed(
            r#"{"version":4,"request_id":"1","message":{"type":"session_metadata_page_end","session_count":"0"}}"#,
        );
    }

    #[test]
    fn inv033_metadata_point_read_requires_nullable_last_writer_member() {
        assert_server_malformed(
            r#"{"version":4,"request_id":"1","message":{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_point_read_requires_nullable_title_member() {
        assert_server_malformed(
            r#"{"version":4,"request_id":"1","message":{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"tags":[],"attributes":{},"archived":false},"last_writer":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_response_shapes_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
        let summary = ServerMessage::SessionMetadataSummary {
            session_id: uuid(1),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(2),
            },
            dangerous_tool_auto_approval: false,
            title: None,
            tags: Vec::new(),
            archived: false,
            last_writer: None,
        };
        for retained in [
            ProtocolVersion::One,
            ProtocolVersion::Two,
            ProtocolVersion::Three,
        ] {
            assert_eq!(
                ServerFrame::try_new_for_version(retained, request(1)?, summary.clone()),
                Err(FrameValidationError::MessageRequiresNewerVersion)
            );
        }
        let invalid = [
            ServerMessage::SessionMetadataSummary {
                session_id: uuid(1),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: uuid(2),
                },
                dangerous_tool_auto_approval: false,
                title: None,
                tags: vec![String::from("z"), String::from("a")],
                archived: false,
                last_writer: Some(MetadataLastWriter::new(
                    CanonicalU64::new(1),
                    MetadataActor::Owner {},
                )),
            },
            ServerMessage::SessionMetadataSummary {
                session_id: uuid(1),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: uuid(2),
                },
                dangerous_tool_auto_approval: false,
                title: Some(String::from("unwritten")),
                tags: Vec::new(),
                archived: false,
                last_writer: None,
            },
            ServerMessage::SessionMetadata {
                session_id: uuid(1),
                metadata: metadata(false)?,
                last_writer: None,
            },
            ServerMessage::SessionMetadataPageEnd {
                session_count: CanonicalU64::new(0),
                next_after_session_id: Some(uuid(1)),
            },
            ServerMessage::SessionMetadataPageEnd {
                session_count: CanonicalU64::new(101),
                next_after_session_id: None,
            },
        ];
        for message in invalid {
            assert_eq!(
                ServerFrame::try_new_for_version(ProtocolVersion::Four, request(1)?, message,),
                Err(FrameValidationError::MetadataShape)
            );
        }
        Ok(())
    }

    #[test]
    fn inv033_retained_versions_keep_model_reconciliation_and_gate_tool_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let model_reconciliation = ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            state: TurnState::ReconciliationRequired {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_model_call_id: uuid(8),
            },
        };
        for version in [ProtocolVersion::One, ProtocolVersion::Two] {
            let frame = ServerFrame::try_new_for_version(
                version,
                request(1)?,
                model_reconciliation.clone(),
            )?;
            let encoded = encode_server_line(&frame)?;
            assert!(
                String::from_utf8(encoded.clone())?
                    .starts_with(&format!("{{\"version\":{},", version.as_u64()))
            );
            assert_eq!(decode_server_line(&encoded)?, frame);
        }

        let tool_reconciliation = ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            state: TurnState::ToolReconciliationRequired {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_tool_attempt_id: uuid(9),
            },
        };
        for version in [ProtocolVersion::One, ProtocolVersion::Two] {
            assert_eq!(
                ServerFrame::try_new_for_version(version, request(2)?, tool_reconciliation.clone(),),
                Err(FrameValidationError::MessageRequiresNewerVersion)
            );
        }
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::Three,
            request(2)?,
            tool_reconciliation,
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_version_four_inherits_imported_transcript_and_tool_event_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let imported = ServerMessage::TranscriptTextEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(1),
            entry_id: uuid(2),
            entry: TranscriptTextEntry::Imported {
                imported_conversation_id: uuid(3),
                imported_entry_id: uuid(4),
                source_speaker: ImportedSourceSpeaker::Attested {
                    speaker: ImportedSpeaker::User,
                },
            },
        };
        let imported_frame =
            ServerFrame::try_new_for_version(ProtocolVersion::Four, request(1)?, imported)?;
        assert_eq!(
            decode_server_line(&encode_server_line(&imported_frame)?)?,
            imported_frame
        );

        let tool_event = ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::TurnToolReconciliationRequired {
                turn_id: uuid(2),
                tool_attempt_id: uuid(3),
                terminal_frontier_id: uuid(4),
            },
        };
        let tool_frame =
            ServerFrame::try_new_for_version(ProtocolVersion::Four, request(2)?, tool_event)?;
        assert_eq!(
            decode_server_line(&encode_server_line(&tool_frame)?)?,
            tool_frame
        );
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
    fn inv033_server_message_family_has_exact_closed_wire_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::SessionCreated {
                session_id: uuid(1),
            },
            r#"{"type":"session_created","session_id":"00000000-0000-0000-0000-000000000001"}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::InputSubmitted {
                session_id: uuid(1),
                accepted_input_id: uuid(2),
                acceptance_position: CanonicalU64::new(1),
                turn_id: uuid(3),
            },
            r#"{"type":"input_submitted","session_id":"00000000-0000-0000-0000-000000000001","accepted_input_id":"00000000-0000-0000-0000-000000000002","acceptance_position":"1","turn_id":"00000000-0000-0000-0000-000000000003"}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::SessionsStart {},
            r#"{"type":"sessions_start"}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::SessionSummary {
                session_id: uuid(1),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Alias { alias_id: uuid(4) },
            },
            r#"{"type":"session_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"alias","alias_id":"00000000-0000-0000-0000-000000000004"}}"#,
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::SessionsEnd {
                session_count: CanonicalU64::new(1),
            },
            r#"{"type":"sessions_end","session_count":"1"}"#,
        )?;
        let writer = MetadataLastWriter::new(CanonicalU64::new(17), MetadataActor::Owner {});
        assert_server_message_round_trip(
            request(32)?,
            ServerMessage::SessionMetadataPageStart {},
            r#"{"type":"session_metadata_page_start"}"#,
        )?;
        assert_server_message_round_trip(
            request(33)?,
            ServerMessage::SessionMetadataSummary {
                session_id: uuid(1),
                defaults_version: CanonicalU64::new(2),
                model_selection: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                dangerous_tool_auto_approval: false,
                title: Some(String::from("Planning")),
                tags: vec![String::from("daily"), String::from("work")],
                archived: true,
                last_writer: Some(writer),
            },
            r#"{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"2","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":false,"title":"Planning","tags":["daily","work"],"archived":true,"last_writer":{"updated_at_unix_micros":"17","actor":{"type":"owner"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(34)?,
            ServerMessage::SessionMetadataPageEnd {
                session_count: CanonicalU64::new(1),
                next_after_session_id: Some(uuid(1)),
            },
            r#"{"type":"session_metadata_page_end","session_count":"1","next_after_session_id":"00000000-0000-0000-0000-000000000001"}"#,
        )?;
        assert_server_message_round_trip(
            request(35)?,
            ServerMessage::SessionMetadata {
                session_id: uuid(1),
                metadata: SessionMetadata::empty(),
                last_writer: None,
            },
            r#"{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":null}"#,
        )?;
        assert_server_message_round_trip(
            request(36)?,
            ServerMessage::SessionMetadataReplaced {
                session_id: uuid(1),
                metadata: metadata(true)?,
                last_writer: writer,
            },
            r#"{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":"Planning","tags":["daily","work"],"attributes":{"run":"17","trigger":""},"archived":true},"last_writer":{"updated_at_unix_micros":"17","actor":{"type":"owner"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(6)?,
            ServerMessage::TranscriptSnapshotStart {
                session_id: uuid(1),
                cursor: CanonicalU64::new(5),
            },
            r#"{"type":"transcript_snapshot_start","session_id":"00000000-0000-0000-0000-000000000001","cursor":"5"}"#,
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
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"refused","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
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
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":"queued request"}}"#,
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
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":null}}"#,
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
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"prepared"}}}}"#,
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
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"in_flight"}}}}"#,
        )?;
        assert_server_message_round_trip(
            request(20)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::ActiveRunning {
                    current_attempt_id: uuid(7),
                    current_model_call: Some(CurrentModelCall::new(
                        uuid(8),
                        CurrentModelCallState::CancellationRequested {},
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"cancellation_requested"}}}}"#,
        )?;
        assert_server_message_round_trip(
            request(21)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: None,
                    terminal_model_call: None,
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":null,"terminal_model_call":null}}"#,
        )?;
        assert_server_message_round_trip(
            request(22)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: Some(uuid(7)),
                    terminal_model_call: None,
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":null}}"#,
        )?;
        assert_server_message_round_trip(
            request(23)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: Some(uuid(7)),
                    terminal_model_call: Some(FailedTerminalModelCall::new(
                        uuid(8),
                        FailedModelCallDisposition::KnownFailed,
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","disposition":"known_failed"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(24)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: Some(uuid(7)),
                    terminal_model_call: Some(FailedTerminalModelCall::new(
                        uuid(8),
                        FailedModelCallDisposition::Cancelled,
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","disposition":"cancelled"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(25)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Cancelled {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: None,
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":null}}"#,
        )?;
        assert_server_message_round_trip(
            request(26)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::Cancelled {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: Some(uuid(8)),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(27)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                state: TurnState::ReconciliationRequired {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: uuid(8),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","state":{"type":"reconciliation_required","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(8)?,
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: uuid(1),
                entry_id: uuid(9),
                entry: TranscriptEntry::TurnCompleted { turn_id: uuid(3) },
            },
            r#"{"type":"transcript_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000009","entry":{"type":"turn_completed","turn_id":"00000000-0000-0000-0000-000000000003"}}"#,
        )?;
        assert_server_message_round_trip(
            request(28)?,
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: uuid(1),
                entry_id: uuid(9),
                entry: TranscriptEntry::TurnCancelled { turn_id: uuid(3) },
            },
            r#"{"type":"transcript_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000009","entry":{"type":"turn_cancelled","turn_id":"00000000-0000-0000-0000-000000000003"}}"#,
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
            r#"{"type":"transcript_text_entry","entry_index":"1","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-00000000000a","entry":{"type":"assistant","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(10)?,
            ServerMessage::TranscriptContent {
                entry_index: CanonicalU64::new(1),
                fragment_index: CanonicalU64::new(0),
                final_fragment: true,
                content_fragment: ContentFragment::try_new("reply".to_owned())?,
            },
            r#"{"type":"transcript_content","entry_index":"1","fragment_index":"0","final_fragment":true,"content_fragment":"reply"}"#,
        )?;
        assert_server_message_round_trip(
            request(11)?,
            ServerMessage::TranscriptSnapshotEnd {
                session_id: uuid(1),
                cursor: CanonicalU64::new(5),
                turn_count: CanonicalU64::new(1),
                entry_count: CanonicalU64::new(2),
            },
            r#"{"type":"transcript_snapshot_end","session_id":"00000000-0000-0000-0000-000000000001","cursor":"5","turn_count":"1","entry_count":"2"}"#,
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
            r#"{"type":"session_event","cursor":"6","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"model_call_transition","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"terminal","disposition":"refused"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(38)?,
            ServerMessage::ProviderTextDelta {
                session_id: uuid(1),
                turn_id: uuid(3),
                model_call_id: uuid(8),
                part_index: CanonicalU64::new(2),
                content: ContentFragment::try_new(String::from("already [redacted]"))?,
            },
            r#"{"type":"provider_text_delta","session_id":"00000000-0000-0000-0000-000000000001","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","part_index":"2","content":"already [redacted]"}"#,
        )?;
        assert_server_message_round_trip(
            request(29)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(6),
                session_id: uuid(1),
                event: SessionEvent::ModelCallTransition {
                    turn_id: uuid(3),
                    model_call_id: uuid(8),
                    state: ModelCallState::CancellationRequested {},
                },
            },
            r#"{"type":"session_event","cursor":"6","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"model_call_transition","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"cancellation_requested"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(31)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(7),
                session_id: uuid(1),
                event: SessionEvent::ToolBatchTransition {
                    turn_id: uuid(3),
                    model_call_id: uuid(8),
                    state: ToolBatchState::ResultsProjected {
                        frontier_id: uuid(6),
                    },
                },
            },
            r#"{"type":"session_event","cursor":"7","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"tool_batch_transition","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"results_projected","frontier_id":"00000000-0000-0000-0000-000000000006"}}}"#,
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
            r#"{"type":"session_event","cursor":"2","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"input_accepted","accepted_input_id":"00000000-0000-0000-0000-000000000002","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","content":"accepted request"}}"#,
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
            r#"{"type":"session_event","cursor":"3","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"turn_activated","turn_id":"00000000-0000-0000-0000-000000000003","current_attempt_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )?;
        assert_server_message_round_trip(
            request(30)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(4),
                session_id: uuid(1),
                event: SessionEvent::TurnCancelled {
                    turn_id: uuid(3),
                    cancellation_entry_id: uuid(9),
                    terminal_frontier_id: uuid(6),
                },
            },
            r#"{"type":"session_event","cursor":"4","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"turn_cancelled","turn_id":"00000000-0000-0000-0000-000000000003","cancellation_entry_id":"00000000-0000-0000-0000-000000000009","terminal_frontier_id":"00000000-0000-0000-0000-000000000006"}}"#,
        )?;
        assert_server_message_round_trip(
            request(31)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(5),
                session_id: uuid(1),
                event: SessionEvent::TurnReconciliationRequired {
                    turn_id: uuid(3),
                    model_call_id: uuid(8),
                    terminal_frontier_id: uuid(6),
                },
            },
            r#"{"type":"session_event","cursor":"5","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"turn_reconciliation_required","turn_id":"00000000-0000-0000-0000-000000000003","model_call_id":"00000000-0000-0000-0000-000000000008","terminal_frontier_id":"00000000-0000-0000-0000-000000000006"}}"#,
        )?;
        assert_server_message_round_trip(
            request(13)?,
            ServerMessage::Error {
                code: ErrorCode::NotFound,
                message: "not found".to_owned(),
                detail: ErrorDetail::none(),
            },
            r#"{"type":"error","code":"not_found","message":"not found"}"#,
        )?;
        Ok(())
    }
}

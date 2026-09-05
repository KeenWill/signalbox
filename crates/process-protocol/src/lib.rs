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
    ser::SerializeSeq,
};
use serde_json::value::RawValue;
use signalbox_domain::{
    BlobDigest, BlobDigestParseError, CredentialProfileName as DomainCredentialProfileName,
    RunnerCapabilityClass as DomainRunnerCapabilityClass,
    RunnerWorkingDirectory as DomainRunnerWorkingDirectory, ToolDecisionRationale,
    ToolDenialReason, WorkspaceRepositoryKey as DomainWorkspaceRepositoryKey,
};
use uuid::Uuid;

/// The single admitted process-protocol version.
pub const PROTOCOL_VERSION: u64 = 1;

/// One admitted process-protocol version.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProtocolVersion {
    /// The complete process-protocol vocabulary.
    One,
}

impl ProtocolVersion {
    const fn from_u64(value: u64) -> Option<Self> {
        match value {
            PROTOCOL_VERSION => Some(Self::One),
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
        serializer.serialize_u64(PROTOCOL_VERSION)
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
// numeric-bound: guard - protects process memory from oversized wire frames
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Maximum decoded source bytes carried by one conversation-import append.
///
/// The half-frame raw-byte bound leaves fixed headroom for canonical padded
/// base64, the request envelope, and the maximum-width correlation identity.
// numeric-bound: derived guard from MAX_FRAME_BYTES
pub const MAX_CONVERSATION_IMPORT_CHUNK_BYTES: usize = MAX_FRAME_BYTES / 2;

/// Maximum decoded bytes carried by one immutable-blob append.
// numeric-bound: derived guard from MAX_FRAME_BYTES
pub const MAX_BLOB_CHUNK_BYTES: usize = MAX_FRAME_BYTES / 2;

/// Maximum decoded bytes returned by one direct blob-range request.
// numeric-bound: derived guard from MAX_FRAME_BYTES
pub const MAX_BLOB_READ_BYTES: usize = MAX_FRAME_BYTES / 2;

/// Maximum number of simultaneously open JSON objects and arrays in one frame.
// numeric-bound: guard - protects parser stack and latency from pathological nesting
pub const MAX_JSON_CONTAINER_DEPTH: usize = 127;

/// Maximum UTF-8 bytes in one transcript content fragment.
// numeric-bound: guard - protects frame memory from pathological content fragmentation
pub const MAX_CONTENT_FRAGMENT_BYTES: usize = 1024 * 1024;

/// Maximum total UTF-8 bytes in one complete metadata object or filter.
// numeric-bound: guard - protects metadata row and frame memory from pathological text volume
pub const MAX_SESSION_METADATA_TOTAL_UTF8_BYTES: usize = 262_144;

/// Maximum UTF-8 bytes in one indexed metadata tag or attribute key.
// numeric-bound: guard - protects database index keys from oversized values
pub const MAX_SESSION_METADATA_INDEXED_UTF8_BYTES: usize = 1_024;

/// Maximum entries in one deployment model-alias catalog.
// numeric-bound: guard - protects model-alias catalog frame memory and wire size
pub const MAX_MODEL_ALIAS_CATALOG_ENTRIES: usize = 10_000;

/// Maximum entries in one deployment model-capability catalog.
// numeric-bound: guard - protects model-capability catalog frame memory and wire size
pub const MAX_MODEL_CAPABILITY_CATALOG_ENTRIES: usize = 10_000;

/// Maximum canonical decimal USD amount text.
// numeric-bound: not-a-bound - the longest canonical rust_decimal spelling
pub const MAX_DOLLAR_AMOUNT_BYTES: usize = 30;

/// Maximum UTF-8 bytes in one deployment-owned billing rate version.
// numeric-bound: guard - preserves the advertised billing-rate wire grammar
pub const MAX_RATE_VERSION_UTF8_BYTES: usize = 128;

/// Maximum finding-indexed members in one review-orchestration request.
// numeric-bound: guard - protects review-request memory and wire size
pub const MAX_REVIEW_ORCHESTRATION_MEMBERS: usize = 1_024;

/// Maximum UTF-8 bytes in one operator-status repository slug.
///
/// A slug is `owner/name`, and the provider admits 100 bytes on each side.
// numeric-bound: guard - the operator-status wire grammar advertises accepting repository slugs only to this length
pub const MAX_OPERATOR_STATUS_REPOSITORY_UTF8_BYTES: usize = 201;

/// Maximum UTF-8 bytes in one operator-status repository-watch rule identity.
// numeric-bound: guard - the operator-status wire grammar advertises accepting rule identities only to this length
pub const MAX_OPERATOR_STATUS_RULE_ID_UTF8_BYTES: usize = 128;

/// Maximum UTF-8 bytes in one operator-status branch name.
///
/// Covers a held slot's branch origin and a convergence row's base branch.
// numeric-bound: guard - the operator-status wire grammar advertises accepting branch names only to this length
pub const MAX_OPERATOR_STATUS_BRANCH_UTF8_BYTES: usize = 255;

/// Maximum sessions named by one operator-status dispatch inventory.
///
/// Bounds both a held slot's own sessions and the sessions occupying a queued
/// obligation, which name the same dispatch-action inventory.
// numeric-bound: guard - protects decoded frame memory from a runaway dispatch session fan-out
pub const MAX_OPERATOR_STATUS_DISPATCH_SESSIONS: usize = 32;

/// Maximum independently failing release clauses on one held slot.
// numeric-bound: not-a-bound - the closed blocker enum's exact variant count, which one slot cannot repeat
pub const MAX_OPERATOR_STATUS_HELD_SLOT_BLOCKERS: usize = 4;

/// Maximum unresolved review threads counted by one convergence assessment.
// numeric-bound: guard - refuses a thread count no durable assessment can have produced
pub const MAX_OPERATOR_STATUS_UNRESOLVED_THREADS: u64 = 10_000;

/// Maximum gating checks counted by one convergence assessment.
///
/// Persistence admits the same inventory, so a divergence here would reject an
/// otherwise valid projection and fail the whole snapshot.
// numeric-bound: guard - bounds the non-green check names one convergence frame can carry
pub const MAX_OPERATOR_STATUS_GATING_CHECKS: u64 = 10_000;

/// Maximum UTF-8 bytes in one operator-status gating-check name.
// numeric-bound: guard - the operator-status wire grammar advertises accepting check names only to this length
pub const MAX_OPERATOR_STATUS_CHECK_NAME_UTF8_BYTES: usize = 256;

/// Maximum UTF-8 bytes in one operator-status review node identity.
// numeric-bound: guard - the operator-status wire grammar advertises accepting review node identities only to this length
pub const MAX_OPERATOR_STATUS_REVIEW_NODE_ID_UTF8_BYTES: usize = 256;

/// Maximum UTF-8 bytes in one operator-status reviewer login.
// numeric-bound: guard - the operator-status wire grammar advertises accepting reviewer logins only to this length
pub const MAX_OPERATOR_STATUS_REVIEWER_UTF8_BYTES: usize = 44;

/// Maximum UTF-8 bytes in one operator-status reviewer login's base, the
/// spelling left once the optional App-bot suffix is set aside.
// numeric-bound: guard - the operator-status wire grammar advertises accepting a login base only to this length
pub const MAX_OPERATOR_STATUS_REVIEWER_BASE_UTF8_BYTES: usize = 39;

/// Literal suffix an App-bot reviewer login carries after its base.
pub const OPERATOR_STATUS_BOT_LOGIN_SUFFIX: &str = "[bot]";

/// The one base branch a merge-ready convergence verdict is settled against.
///
/// The durable assessment keys both converged verdicts to this spelling: a
/// merge-ready row's base branch is exactly this branch, and an
/// internally-converged row's base branch is any other.
pub const OPERATOR_STATUS_TRUNK_BASE_BRANCH: &str = "main";

/// Exact hexadecimal characters in one operator-status commit revision.
// numeric-bound: not-a-bound - the fixed width of a git SHA-1 object name
pub const OPERATOR_STATUS_COMMIT_SHA_LENGTH: usize = 40;

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

impl fmt::Display for CommandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
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

/// A positive unsigned 64-bit value encoded as its shortest decimal string.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PositiveCanonicalU64(u64);

impl PositiveCanonicalU64 {
    /// Checks that the represented wire integer is positive.
    pub const fn try_new(value: u64) -> Result<Self, CanonicalValueError> {
        if value == 0 {
            return Err(CanonicalValueError::Decimal);
        }
        Ok(Self(value))
    }

    /// Returns the positive numeric value.
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl TryFrom<String> for PositiveCanonicalU64 {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(parse_decimal_u64(&value)?)
    }
}

impl From<PositiveCanonicalU64> for String {
    fn from(value: PositiveCanonicalU64) -> Self {
        value.0.to_string()
    }
}

impl From<signalbox_domain::RunnerGeneration> for PositiveCanonicalU64 {
    fn from(value: signalbox_domain::RunnerGeneration) -> Self {
        Self(value.get())
    }
}

/// A lowercase 32-byte digest encoded as exactly 64 hexadecimal characters.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalDigest(String);

impl CanonicalDigest {
    /// Checks the exact lowercase hexadecimal digest spelling.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        let mut decoded = [0_u8; 32];
        if hex::decode_to_slice(&value, &mut decoded).is_err() || hex::encode(decoded) != value {
            return Err(CanonicalValueError::Digest);
        }
        Ok(Self(value))
    }

    /// Borrows the exact canonical hexadecimal spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Transfers the exact canonical hexadecimal spelling.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for CanonicalDigest {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<CanonicalDigest> for String {
    fn from(value: CanonicalDigest) -> Self {
        value.0
    }
}

/// Exact external blob identity including its fixed SHA-256 algorithm tag.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalBlobDigest(BlobDigest);

impl CanonicalBlobDigest {
    /// Constructs the exact SHA-256 identity from an already-computed digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(BlobDigest::from_bytes(bytes))
    }

    /// Wraps one validated domain digest for the process boundary.
    pub const fn from_digest(value: BlobDigest) -> Self {
        Self(value)
    }

    /// Returns the validated digest for an explicit adapter mapping.
    pub const fn into_digest(self) -> BlobDigest {
        self.0
    }
}

impl std::str::FromStr for CanonicalBlobDigest {
    type Err = BlobDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

impl fmt::Display for CanonicalBlobDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for CanonicalBlobDigest {
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

impl<'de> Deserialize<'de> for CanonicalBlobDigest {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value
            .parse::<BlobDigest>()
            .map(Self)
            .map_err(serde::de::Error::custom)
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

/// Exact user input content carried to the application admission boundary.
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

/// Maximum number of ordered parts in one process-protocol user input.
// numeric-bound: guard - prevents one submitted input from fragmenting into unbounded decoded parts
pub const MAX_USER_INPUT_PARTS: usize = signalbox_domain::UserContent::MAX_PARTS;
/// Maximum aggregate UTF-8 bytes across process-protocol text parts.
// numeric-bound: guard - prevents one submitted input from exhausting wire-frame memory
pub const MAX_USER_INPUT_TEXT_BYTES: usize = signalbox_domain::UserContent::MAX_TEXT_BYTES;
/// Maximum encoded bytes in one process-protocol attachment media type.
// numeric-bound: guard - the user-input wire grammar advertises accepting media types only to this length
pub const MAX_USER_INPUT_MEDIA_TYPE_BYTES: usize = signalbox_domain::DeclaredMediaType::MAX_BYTES;
/// Maximum encoded bytes in one process-protocol attachment display filename.
// numeric-bound: guard - the user-input wire grammar advertises accepting display filenames only to this length
pub const MAX_USER_INPUT_DISPLAY_FILENAME_BYTES: usize =
    signalbox_domain::AttachmentDisplayFilename::MAX_BYTES;

/// Closed semantic kind declared for one user attachment on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAttachmentKind {
    /// Image content.
    Image,
    /// Page- or document-oriented content.
    Document,
    /// Other file content.
    File,
}

/// One exact part in canonical ordered user input.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserInputPart {
    /// Exact decoded text.
    Text {
        /// Nonempty text containing no U+0000.
        text: String,
    },
    /// Immutable blob reference and caller-declared metadata.
    Attachment {
        /// Canonical global blob identity.
        digest: CanonicalBlobDigest,
        /// Closed semantic attachment kind.
        kind: UserAttachmentKind,
        /// Exact visible-ASCII media-type declaration.
        media_type: String,
        /// Optional display basename, explicitly null when absent.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        display_filename: Option<String>,
    },
}

impl fmt::Debug for UserInputPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { .. } => formatter
                .debug_struct("Text")
                .field("text", &"<redacted>")
                .finish(),
            Self::Attachment {
                digest,
                kind,
                media_type,
                display_filename,
            } => formatter
                .debug_struct("Attachment")
                .field("digest", digest)
                .field("kind", kind)
                .field("media_type", media_type)
                .field(
                    "display_filename",
                    &display_filename.as_ref().map(|_| "<redacted>"),
                )
                .finish(),
        }
    }
}

/// Canonical nonempty ordered user-input parts array.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct UserInputContent(Vec<UserInputPart>);

struct UserInputContentVisitor;

impl<'de> Visitor<'de> for UserInputContentVisitor {
    type Value = UserInputContent;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "at most {MAX_USER_INPUT_PARTS} ordered user-input parts"
        )
    }

    fn visit_seq<AccessT>(self, mut sequence: AccessT) -> Result<Self::Value, AccessT::Error>
    where
        AccessT: SeqAccess<'de>,
    {
        let mut parts = Vec::with_capacity(
            sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_USER_INPUT_PARTS),
        );
        while parts.len() < MAX_USER_INPUT_PARTS {
            match sequence.next_element::<UserInputPart>()? {
                Some(part) => parts.push(part),
                None => return Ok(UserInputContent(parts)),
            }
        }
        if sequence.next_element::<IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom("too many user-input parts"));
        }
        Ok(UserInputContent(parts))
    }
}

impl<'de> Deserialize<'de> for UserInputContent {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        deserializer.deserialize_seq(UserInputContentVisitor)
    }
}

impl UserInputContent {
    /// Wraps one text part for text-only clients.
    pub fn text(value: String) -> Self {
        Self(vec![UserInputPart::Text { text: value }])
    }

    /// Wraps a complete parts array for structural validation at frame encode.
    pub fn from_parts(parts: Vec<UserInputPart>) -> Self {
        Self(parts)
    }

    /// Borrows the exact ordered parts.
    pub fn parts(&self) -> &[UserInputPart] {
        &self.0
    }

    /// Borrows text when this is exactly one text part.
    pub fn single_text(&self) -> Option<&str> {
        match self.0.as_slice() {
            [UserInputPart::Text { text }] => Some(text),
            _ => None,
        }
    }

    /// Transfers ownership of the exact ordered parts.
    pub fn into_parts(self) -> Vec<UserInputPart> {
        self.0
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if self.0.is_empty() || self.0.len() > MAX_USER_INPUT_PARTS {
            return Err(FrameValidationError::UserContentShape);
        }

        let mut text_bytes = 0_usize;
        let mut previous_was_text = false;
        for part in &self.0 {
            match part {
                UserInputPart::Text { text } => {
                    if previous_was_text || text.is_empty() || text.contains('\0') {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    text_bytes = text_bytes
                        .checked_add(text.len())
                        .ok_or(FrameValidationError::UserContentShape)?;
                    if text_bytes > MAX_USER_INPUT_TEXT_BYTES {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    previous_was_text = true;
                }
                UserInputPart::Attachment {
                    media_type,
                    display_filename,
                    ..
                } => {
                    if media_type.is_empty()
                        || media_type.len() > MAX_USER_INPUT_MEDIA_TYPE_BYTES
                        || !media_type.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                    {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    if display_filename.as_ref().is_some_and(|filename| {
                        filename.is_empty()
                            || filename.len() > MAX_USER_INPUT_DISPLAY_FILENAME_BYTES
                            || filename == "."
                            || filename == ".."
                            || filename.contains('/')
                            || filename.contains('\\')
                            || filename.contains('\0')
                    }) {
                        return Err(FrameValidationError::UserContentShape);
                    }
                    previous_was_text = false;
                }
            }
        }
        Ok(())
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
    /// Producer confidence that the issue is real, in basis points.
    pub is_real_confidence: CanonicalU64,
    /// Producer confidence that the severity label is correct, in basis points.
    pub severity_label_confidence: CanonicalU64,
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

/// One immutable finding-machine event recorded by a result-bearing pass.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewFindingEvent {
    Accepted {},
    Rejected {
        reason: String,
    },
    Duplicate {
        canonical_finding_id: CanonicalUuid,
    },
    Superseded {
        successor_finding_id: CanonicalUuid,
    },
    Stale {},
    Fixed {},
    BlockedWithReason {
        reason: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        external_link_id: Option<CanonicalUuid>,
    },
}

/// Terminal outcome for a pass that does not otherwise carry typed result data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPassTerminalOutcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

/// One concern entry in a new frozen orchestration attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationConcernInput {
    pub key: String,
    pub template_name: String,
}

/// Terminal imported-context stage outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewImportTerminalOutcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

/// Terminal concern-member outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewConcernTerminalOutcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

/// One closed disposition in an immutable judgment plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewJudgmentDisposition {
    Accepted {},
    Rejected { reason: String },
    Duplicate { canonical_finding_id: CanonicalUuid },
    Superseded { successor_finding_id: CanonicalUuid },
    Stale {},
}

/// One finding member in a complete judgment plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewJudgmentPlanMember {
    pub finding_id: CanonicalUuid,
    pub disposition: ReviewJudgmentDisposition,
}

/// Terminal result of applying one judgment-plan member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewJudgmentEffectTerminalOutcome {
    Applied,
    Failed,
    Blocked,
    Cancelled,
}

/// Terminal result of one repair member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRepairTerminalOutcome {
    Fixed,
    Failed,
    Blocked,
    Cancelled,
}

/// One finding-indexed repair result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRepairOutcome {
    pub finding_id: CanonicalUuid,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub event_pass_id: Option<CanonicalUuid>,
    pub outcome: ReviewRepairTerminalOutcome,
}

/// Terminal result of one publication member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPublicationTerminalOutcome {
    Published,
    Failed,
    Blocked,
    Cancelled,
}

/// One finding-indexed publication result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPublicationOutcome {
    pub finding_id: CanonicalUuid,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub external_link_id: Option<CanonicalUuid>,
    pub outcome: ReviewPublicationTerminalOutcome,
}

/// Durable stage of one client-driven review-orchestration attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOrchestrationState {
    AwaitingImport,
    ImportIncomplete,
    AwaitingConcerns,
    FanoutIncomplete,
    AwaitingJudgment,
    AwaitingJudgmentEffects,
    JudgmentIncomplete,
    AwaitingRepair,
    RepairIncomplete,
    AwaitingPublication,
    PublicationIncomplete,
    Complete,
}

/// Durable progress of one frozen concern member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOrchestrationConcernStatus {
    Pending,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
    Superseded,
}

/// Resolved non-concern templates frozen into one attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationStageTemplateDigests {
    pub import: CanonicalDigest,
    pub judgment: CanonicalDigest,
    pub repair: CanonicalDigest,
    pub publication: CanonicalDigest,
}

/// One frozen concern and its durable progress.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationConcernSnapshot {
    pub key: String,
    pub template_digest: CanonicalDigest,
    pub status: ReviewOrchestrationConcernStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pass_id: Option<CanonicalUuid>,
}

/// Progress counts needed to observe one orchestration attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationCounts {
    pub finding_count: CanonicalU64,
    pub judgment_member_count: CanonicalU64,
    pub judgment_effect_applied_count: CanonicalU64,
    pub repair_fixed_count: CanonicalU64,
    pub publication_published_count: CanonicalU64,
}

/// Complete read projection of one review-orchestration attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOrchestrationSnapshot {
    pub attempt_id: CanonicalUuid,
    pub target_id: CanonicalUuid,
    pub state: ReviewOrchestrationState,
    pub concern_set_version: String,
    pub stage_template_digests: ReviewOrchestrationStageTemplateDigests,
    pub concerns: Vec<ReviewOrchestrationConcernSnapshot>,
    pub counts: ReviewOrchestrationCounts,
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

/// Independently nullable token fields for one terminal model call.
///
/// Every field is required on the wire but independently nullable. A null is
/// absent evidence; a present zero is encoded as the canonical string
/// `"0"`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCallTokenUsage {
    /// Input-token count from the call's named provenance.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub input_tokens: Option<CanonicalU64>,
    /// Output-token count from the call's named provenance.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub output_tokens: Option<CanonicalU64>,
    /// Cache-creation input-token count from the call's named provenance.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub cache_creation_input_tokens: Option<CanonicalU64>,
    /// Cache-read input-token count from the call's named provenance.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub cache_read_input_tokens: Option<CanonicalU64>,
}

/// Closed provenance of one terminal model call's token fields.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProvenance {
    /// Counts reported by the provider or adapter stream.
    Reported,
    /// Counts produced by an explicit estimator.
    Estimated,
}

/// How a derived token-rate dollar figure must be labeled.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallCostLabel {
    /// The serving credential profile is directly API-metered.
    Real,
    /// The serving credential profile is subscription-backed.
    MeteredEquivalent,
}

/// Canonical nonnegative decimal USD text with no exponent or redundant zeroes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalDollarAmount(String);

impl CanonicalDollarAmount {
    /// Validates one shortest nonnegative base-ten decimal spelling.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        // numeric-bound: not-a-bound - fixed rust_decimal coefficient representation
        const MAX_DECIMAL_COEFFICIENT: u128 = 79_228_162_514_264_337_593_543_950_335;

        let (integer, fraction) = value
            .split_once('.')
            .map_or((value.as_str(), None), |parts| (parts.0, Some(parts.1)));
        let integer_is_canonical = integer == "0"
            || (!integer.starts_with('0')
                && integer.as_bytes().first().is_some_and(u8::is_ascii_digit)
                && integer.bytes().all(|byte| byte.is_ascii_digit()));
        let fraction_is_canonical = fraction.is_none_or(|fraction| {
            !fraction.is_empty()
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
                && !fraction.ends_with('0')
        });
        let coefficient =
            value
                .bytes()
                .filter(|byte| *byte != b'.')
                .try_fold(0_u128, |coefficient, digit| {
                    coefficient
                        .checked_mul(10)?
                        .checked_add(u128::from(digit.checked_sub(b'0')?))
                });
        if value.is_empty()
            || value.len() > MAX_DOLLAR_AMOUNT_BYTES
            || !integer_is_canonical
            || !fraction_is_canonical
            || fraction.is_some_and(|fraction| fraction.len() > 28)
            || coefficient.is_none_or(|coefficient| coefficient > MAX_DECIMAL_COEFFICIENT)
        {
            Err(CanonicalValueError::DollarAmount)
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows the canonical decimal spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CanonicalDollarAmount {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<CanonicalDollarAmount> for String {
    fn from(value: CanonicalDollarAmount) -> Self {
        value.0
    }
}

/// One bounded deployment-owned rate version carried as cost provenance.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BillingRateVersion(String);

impl BillingRateVersion {
    /// Validates a nonempty, unpadded, NUL-free version spelling.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        if value.is_empty()
            || value.len() > MAX_RATE_VERSION_UTF8_BYTES
            || value.trim() != value
            || value.contains('\0')
        {
            Err(CanonicalValueError::RateVersion)
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows the exact rate version.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BillingRateVersion {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<BillingRateVersion> for String {
    fn from(value: BillingRateVersion) -> Self {
        value.0
    }
}

/// One read-time dollar figure derived from usage and named configured rates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCallDollarCost {
    /// Dollar amount attributable to the token axes that were present.
    pub amount_usd: CanonicalDollarAmount,
    /// Exact configured rate version used for derivation.
    pub rate_version: BillingRateVersion,
    /// Real or metered-equivalent label from the pinned credential profile.
    pub label: ModelCallCostLabel,
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

/// One exact session system prompt on the wire.
///
/// A present prompt is nonempty and rejects U+0000; absence is JSON null on
/// the owning member, never empty text. The daemon applies deployment policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SystemPromptText(String);

impl SystemPromptText {
    /// Applies the structural nonempty and U+0000-free admission rules.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        if value.is_empty() || value.contains('\0') {
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

/// Presence-checked required system-prompt member.
///
/// JSON null states explicitly that no prompt is configured and a JSON string
/// states the complete checked bounded prompt.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemPromptMember(Option<Option<SystemPromptText>>);

impl SystemPromptMember {
    /// Marks a missing member during deserialization.
    pub const fn absent() -> Self {
        Self(None)
    }

    /// Carries the explicit required null-or-text member.
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
    /// Digest was not exactly 64 lowercase hexadecimal characters.
    Digest,
    /// A session system prompt was empty, contained U+0000, or exceeded its
    /// UTF-8 byte bound.
    SystemPrompt,
    /// A dotted session placement or root-global-read decision was invalid.
    Placement,
    /// Runner working-directory text was empty, NUL-bearing, or oversized.
    RunnerWorkingDirectory,
    /// Runner capability, credential-profile, or repository name was invalid.
    RunnerCatalogName,
    /// Runner projection state and exact-runner evidence were inconsistent.
    RunnerProjection,
    /// Dollar amount was not canonical bounded nonnegative decimal text.
    DollarAmount,
    /// Billing rate version was empty, padded, NUL-bearing, or oversized.
    RateVersion,
}

impl fmt::Display for CanonicalValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Uuid => "UUID is not canonical lowercase hyphenated text",
            Self::CommandId => "command identity is a reserved sentinel",
            Self::Decimal => "unsigned integer is not canonical decimal text",
            Self::RequestId => "client request identity must be nonzero",
            Self::Content => "content fragment exceeds the process-protocol UTF-8 byte bound",
            Self::Metadata => "session metadata value is invalid",
            Self::Digest => "digest is not canonical lowercase 64-character hexadecimal text",
            Self::SystemPrompt => "session system prompt is empty, oversized, or contains U+0000",
            Self::Placement => "session placement is invalid",
            Self::RunnerWorkingDirectory => "runner working directory is invalid",
            Self::RunnerCatalogName => "runner catalog name is invalid",
            Self::RunnerProjection => "runner projection state is invalid",
            Self::DollarAmount => "dollar amount is not canonical nonnegative decimal text",
            Self::RateVersion => "billing rate version is invalid",
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

/// Content-silent reason an imported-conversation converter rejected source.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationImportRejectionClass {
    /// The supplied source contained no JSONL record.
    EmptySource,
    /// One physical JSONL record was empty.
    BlankLine,
    /// One physical record was not valid UTF-8.
    InvalidUtf8,
    /// One physical record was not valid JSON.
    InvalidJson,
    /// One physical record exceeded the JSON container-depth bound.
    JsonDepthExceeded,
    /// One physical record's top-level JSON value was not an object.
    TopLevelNotObject,
    /// A modeled record discriminator had an unsupported value shape.
    InvalidRecordType,
    /// Modeled source metadata had an unsupported value shape.
    InvalidSourceMetadata,
    /// A modeled message or response-item envelope had an unsupported shape.
    InvalidMessageEnvelope,
    /// A modeled message role had an unsupported value shape.
    InvalidMessageRole,
    /// A nested message role contradicted its enclosing source speaker.
    MessageRoleMismatch,
    /// Modeled message content had an unsupported value shape.
    InvalidMessageContent,
    /// A modeled message content block had an unsupported value shape.
    InvalidContentBlock,
    /// A modeled tool-result block had an unsupported value shape.
    InvalidToolResultBlock,
    /// A modeled reasoning item or block had an unsupported value shape.
    InvalidReasoning,
    /// A modeled tool call had an unsupported value shape.
    InvalidToolCall,
    /// A modeled tool result had an unsupported value shape.
    InvalidToolResult,
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
                if STANDARD_BASE64.encode(&decoded) != encoded {
                    return Err(serde::de::Error::custom(
                        "import source is not canonical base64",
                    ));
                }
                Ok(ConversationImportSource(decoded))
            }
        }

        deserializer.deserialize_str(ConversationImportSourceVisitor)
    }
}

/// One bounded immutable-blob upload chunk encoded as canonical padded base64.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobChunk(ConversationImportSource);

impl BlobChunk {
    /// Wraps exact decoded bytes without interpreting them.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(ConversationImportSource::new(bytes))
    }

    /// Borrows the exact decoded bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Transfers ownership of the exact decoded bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }
}

fn parse_decimal_u64(value: &str) -> Result<u64, CanonicalValueError> {
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

/// Provider-neutral reasoning effort at the process boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
    Ultra,
}

/// Whether fast serving is selected.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FastMode {
    Disabled,
    Enabled,
}

/// Anthropic Messages service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicServiceTier {
    Auto,
    StandardOnly,
}

/// OpenAI service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiServiceTier {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
    Fast,
}

/// Codex CLI service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexCliServiceTier {
    Default,
    Priority,
    Flex,
}

/// Provider-tagged service tier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(
    tag = "provider",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ServiceTier {
    Anthropic(AnthropicServiceTier),
    OpenAi(OpenAiServiceTier),
    CodexCli(CodexCliServiceTier),
}

/// One precedence-layer contribution for a setting.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingOverlay<ValueT> {
    Inherit,
    ProviderDefault,
    Value(ValueT),
}

/// One fast-mode contribution at a precedence layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FastModeOverlay {
    Inherit,
    Value(FastMode),
}

/// Three provenance-preserving setting contributions at one layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettingsOverlay {
    pub reasoning_level: SettingOverlay<ReasoningLevel>,
    pub fast_mode: FastModeOverlay,
    pub service_tier: SettingOverlay<ServiceTier>,
}

impl ModelSettingsOverlay {
    /// Constructs an overlay that inherits every setting.
    pub const fn inherit_all() -> Self {
        Self {
            reasoning_level: SettingOverlay::Inherit,
            fast_mode: FastModeOverlay::Inherit,
            service_tier: SettingOverlay::Inherit,
        }
    }
}

impl Default for ModelSettingsOverlay {
    fn default() -> Self {
        Self::inherit_all()
    }
}

/// Complete effective setting values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveModelSettings {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reasoning_level: Option<ReasoningLevel>,
    pub fast_mode: FastMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub service_tier: Option<ServiceTier>,
}

/// Precedence layer that supplied one effective value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSettingSource {
    PerCall,
    Session,
    Profile,
    GlobalDefault,
}

/// Exact four-layer setting contributions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettingsPrecedence {
    pub per_call: ModelSettingsOverlay,
    pub session: ModelSettingsOverlay,
    pub profile: ModelSettingsOverlay,
    pub global_default: ModelSettingsOverlay,
}

/// Complete resolved settings and their validation provenance.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettingsSnapshot {
    pub precedence: ModelSettingsPrecedence,
    pub effective: EffectiveModelSettings,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reasoning_source: Option<ModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub fast_mode_source: Option<ModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub service_tier_source: Option<ModelSettingSource>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub validated_for_selection_id: Option<CanonicalUuid>,
}

/// Complete frozen settings evidence for one transcript turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnModelSettingsSnapshot {
    /// Turn that owns this frozen settings evidence.
    pub turn_id: CanonicalUuid,
    /// Accepted input that originated the turn.
    pub accepted_input_id: CanonicalUuid,
    /// Session-defaults epoch resolved for the origin.
    pub defaults_version: CanonicalU64,
    /// Model request before alias freezing.
    pub requested_model: ModelSelection,
    /// Direct model selected for execution.
    pub selected_direct_id: CanonicalUuid,
    /// Exact per-call settings contribution.
    pub per_call_override: ModelSettingsOverlay,
    /// Complete validated settings frozen for execution.
    pub settings: ModelSettingsSnapshot,
    /// Prior direct selection adjusted by a model change, or null.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub adjusted_from_selection_id: Option<CanonicalUuid>,
    /// Ordered automatic model-change adjustments.
    pub adjustments: Vec<ModelChangeAdjustment>,
}

impl ModelSettingsSnapshot {
    fn validate(&self) -> Result<(), FrameValidationError> {
        let resolved = resolve_wire_settings(self.precedence);
        if resolved.effective != self.effective
            || resolved.reasoning_source != self.reasoning_source
            || resolved.fast_mode_source != self.fast_mode_source
            || resolved.service_tier_source != self.service_tier_source
            || (self.validated_for_selection_id.is_none()
                && !self.is_model_independent_provider_defaults())
        {
            return Err(FrameValidationError::ModelSettingsShape);
        }
        Ok(())
    }

    fn validate_defaults(&self) -> Result<(), FrameValidationError> {
        self.validate()?;
        if self.precedence.per_call != ModelSettingsOverlay::inherit_all() {
            return Err(FrameValidationError::ModelSettingsShape);
        }
        Ok(())
    }

    /// Reports whether this snapshot can belong to the supplied model selection.
    pub fn matches_model(&self, model: &ModelSelection) -> bool {
        snapshot_matches_model(model, self)
    }

    fn is_model_independent_provider_defaults(&self) -> bool {
        self.precedence
            == (ModelSettingsPrecedence {
                per_call: ModelSettingsOverlay::inherit_all(),
                session: ModelSettingsOverlay::inherit_all(),
                profile: ModelSettingsOverlay::inherit_all(),
                global_default: ModelSettingsOverlay::inherit_all(),
            })
            && self.effective
                == (EffectiveModelSettings {
                    reasoning_level: None,
                    fast_mode: FastMode::Disabled,
                    service_tier: None,
                })
            && self.reasoning_source.is_none()
            && self.fast_mode_source.is_none()
            && self.service_tier_source.is_none()
    }
}

impl TurnModelSettingsSnapshot {
    fn validate(&self) -> Result<(), FrameValidationError> {
        validate_turn_settings_payload(
            self.defaults_version,
            &self.requested_model,
            self.selected_direct_id,
            self.per_call_override,
            &self.settings,
            self.adjusted_from_selection_id,
            &self.adjustments,
        )
    }
}

#[derive(Clone, Copy)]
struct WireResolvedModelSettings {
    effective: EffectiveModelSettings,
    reasoning_source: Option<ModelSettingSource>,
    fast_mode_source: Option<ModelSettingSource>,
    service_tier_source: Option<ModelSettingSource>,
}

fn resolve_wire_settings(precedence: ModelSettingsPrecedence) -> WireResolvedModelSettings {
    let layers = [
        (ModelSettingSource::PerCall, precedence.per_call),
        (ModelSettingSource::Session, precedence.session),
        (ModelSettingSource::Profile, precedence.profile),
        (ModelSettingSource::GlobalDefault, precedence.global_default),
    ];
    let (reasoning_level, reasoning_source) =
        resolve_wire_nullable(layers.map(|(source, settings)| (source, settings.reasoning_level)));
    let (fast_mode, fast_mode_source) =
        resolve_wire_fast(layers.map(|(source, settings)| (source, settings.fast_mode)));
    let (service_tier, service_tier_source) =
        resolve_wire_nullable(layers.map(|(source, settings)| (source, settings.service_tier)));
    WireResolvedModelSettings {
        effective: EffectiveModelSettings {
            reasoning_level,
            fast_mode,
            service_tier,
        },
        reasoning_source,
        fast_mode_source,
        service_tier_source,
    }
}

fn resolve_wire_nullable<ValueT: Copy>(
    layers: impl IntoIterator<Item = (ModelSettingSource, SettingOverlay<ValueT>)>,
) -> (Option<ValueT>, Option<ModelSettingSource>) {
    for (source, setting) in layers {
        match setting {
            SettingOverlay::Inherit => {}
            SettingOverlay::ProviderDefault => return (None, Some(source)),
            SettingOverlay::Value(value) => return (Some(value), Some(source)),
        }
    }
    (None, None)
}

fn resolve_wire_fast(
    layers: impl IntoIterator<Item = (ModelSettingSource, FastModeOverlay)>,
) -> (FastMode, Option<ModelSettingSource>) {
    for (source, setting) in layers {
        match setting {
            FastModeOverlay::Inherit => {}
            FastModeOverlay::Value(value) => return (value, Some(source)),
        }
    }
    (FastMode::Disabled, None)
}

fn overlay_inheriting_from(
    overlay: ModelSettingsOverlay,
    prior: ModelSettingsOverlay,
) -> ModelSettingsOverlay {
    ModelSettingsOverlay {
        reasoning_level: match overlay.reasoning_level {
            SettingOverlay::Inherit => prior.reasoning_level,
            SettingOverlay::ProviderDefault | SettingOverlay::Value(_) => overlay.reasoning_level,
        },
        fast_mode: match overlay.fast_mode {
            FastModeOverlay::Inherit => prior.fast_mode,
            FastModeOverlay::Value(_) => overlay.fast_mode,
        },
        service_tier: match overlay.service_tier {
            SettingOverlay::Inherit => prior.service_tier,
            SettingOverlay::ProviderDefault | SettingOverlay::Value(_) => overlay.service_tier,
        },
    }
}

fn with_wire_effective_adjustment(
    mut precedence: ModelSettingsPrecedence,
    prior: WireResolvedModelSettings,
    adjusted: EffectiveModelSettings,
) -> ModelSettingsPrecedence {
    if prior.effective.reasoning_level != adjusted.reasoning_level {
        let value = match adjusted.reasoning_level {
            Some(value) => SettingOverlay::Value(value),
            None => SettingOverlay::ProviderDefault,
        };
        match prior.reasoning_source {
            Some(ModelSettingSource::PerCall) => precedence.per_call.reasoning_level = value,
            Some(ModelSettingSource::Session) => precedence.session.reasoning_level = value,
            Some(ModelSettingSource::Profile) => precedence.profile.reasoning_level = value,
            Some(ModelSettingSource::GlobalDefault) => {
                precedence.global_default.reasoning_level = value;
            }
            None => {}
        }
    }
    if prior.effective.fast_mode != adjusted.fast_mode {
        let value = FastModeOverlay::Value(adjusted.fast_mode);
        match prior.fast_mode_source {
            Some(ModelSettingSource::PerCall) => precedence.per_call.fast_mode = value,
            Some(ModelSettingSource::Session) => precedence.session.fast_mode = value,
            Some(ModelSettingSource::Profile) => precedence.profile.fast_mode = value,
            Some(ModelSettingSource::GlobalDefault) => precedence.global_default.fast_mode = value,
            None => {}
        }
    }
    if prior.effective.service_tier != adjusted.service_tier {
        let value = match adjusted.service_tier {
            Some(value) => SettingOverlay::Value(value),
            None => SettingOverlay::ProviderDefault,
        };
        match prior.service_tier_source {
            Some(ModelSettingSource::PerCall) => precedence.per_call.service_tier = value,
            Some(ModelSettingSource::Session) => precedence.session.service_tier = value,
            Some(ModelSettingSource::Profile) => precedence.profile.service_tier = value,
            Some(ModelSettingSource::GlobalDefault) => {
                precedence.global_default.service_tier = value;
            }
            None => {}
        }
    }
    precedence
}

fn apply_wire_adjustments(
    precedence: ModelSettingsPrecedence,
    adjustments: &[ModelChangeAdjustment],
) -> Option<ModelSettingsPrecedence> {
    validate_adjustments(adjustments).ok()?;
    let prior = resolve_wire_settings(precedence);
    let mut effective = prior.effective;
    for adjustment in adjustments {
        effective = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { from, to }
                if prior.reasoning_source != Some(ModelSettingSource::PerCall)
                    && effective.reasoning_level == Some(*from)
                    && from != to =>
            {
                EffectiveModelSettings {
                    reasoning_level: Some(*to),
                    ..effective
                }
            }
            ModelChangeAdjustment::ReasoningLevelCleared { from }
                if prior.reasoning_source != Some(ModelSettingSource::PerCall)
                    && effective.reasoning_level == Some(*from) =>
            {
                EffectiveModelSettings {
                    reasoning_level: None,
                    ..effective
                }
            }
            ModelChangeAdjustment::FastModeDisabled {}
                if prior.fast_mode_source != Some(ModelSettingSource::PerCall)
                    && effective.fast_mode == FastMode::Enabled =>
            {
                EffectiveModelSettings {
                    fast_mode: FastMode::Disabled,
                    ..effective
                }
            }
            ModelChangeAdjustment::ServiceTierCleared { from }
                if prior.service_tier_source != Some(ModelSettingSource::PerCall)
                    && effective.service_tier == Some(*from) =>
            {
                EffectiveModelSettings {
                    service_tier: None,
                    ..effective
                }
            }
            ModelChangeAdjustment::ReasoningLevelClamped { .. }
            | ModelChangeAdjustment::ReasoningLevelCleared { .. }
            | ModelChangeAdjustment::FastModeDisabled {}
            | ModelChangeAdjustment::ServiceTierCleared { .. } => return None,
        };
    }
    Some(with_wire_effective_adjustment(precedence, prior, effective))
}

fn unapply_wire_adjustments(
    settings: &ModelSettingsSnapshot,
    adjustments: &[ModelChangeAdjustment],
) -> Option<ModelSettingsPrecedence> {
    let settled = resolve_wire_settings(settings.precedence);
    let mut prior = settled.effective;
    for adjustment in adjustments {
        prior = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { from, to }
                if settled.reasoning_source != Some(ModelSettingSource::PerCall)
                    && settled.effective.reasoning_level == Some(*to) =>
            {
                EffectiveModelSettings {
                    reasoning_level: Some(*from),
                    ..prior
                }
            }
            ModelChangeAdjustment::ReasoningLevelCleared { from }
                if settled.reasoning_source != Some(ModelSettingSource::PerCall)
                    && settled.effective.reasoning_level.is_none() =>
            {
                EffectiveModelSettings {
                    reasoning_level: Some(*from),
                    ..prior
                }
            }
            ModelChangeAdjustment::FastModeDisabled {}
                if settled.fast_mode_source != Some(ModelSettingSource::PerCall)
                    && settled.effective.fast_mode == FastMode::Disabled =>
            {
                EffectiveModelSettings {
                    fast_mode: FastMode::Enabled,
                    ..prior
                }
            }
            ModelChangeAdjustment::ServiceTierCleared { from }
                if settled.service_tier_source != Some(ModelSettingSource::PerCall)
                    && settled.effective.service_tier.is_none() =>
            {
                EffectiveModelSettings {
                    service_tier: Some(*from),
                    ..prior
                }
            }
            ModelChangeAdjustment::ReasoningLevelClamped { .. }
            | ModelChangeAdjustment::ReasoningLevelCleared { .. }
            | ModelChangeAdjustment::FastModeDisabled {}
            | ModelChangeAdjustment::ServiceTierCleared { .. } => return None,
        };
    }
    Some(with_wire_effective_adjustment(
        settings.precedence,
        settled,
        prior,
    ))
}

fn snapshot_matches_model(model: &ModelSelection, settings: &ModelSettingsSnapshot) -> bool {
    match (model, settings.validated_for_selection_id) {
        (ModelSelection::Direct { selection_id }, Some(validated)) => *selection_id == validated,
        (ModelSelection::Direct { .. }, None) | (ModelSelection::Alias { .. }, _) => true,
    }
}

/// One automatic compatibility adjustment caused by a model change.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelChangeAdjustment {
    ReasoningLevelClamped {
        from: ReasoningLevel,
        to: ReasoningLevel,
    },
    ReasoningLevelCleared {
        from: ReasoningLevel,
    },
    FastModeDisabled {},
    ServiceTierCleared {
        from: ServiceTier,
    },
}

/// Client-visible exact capabilities for one direct selection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    pub reasoning_levels: Vec<ReasoningLevel>,
    pub fast_mode_supported: bool,
    pub service_tiers: Vec<ServiceTier>,
}

impl ModelCapabilities {
    fn validate(&self) -> Result<(), FrameValidationError> {
        let reasoning = self
            .reasoning_levels
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let tiers = self.service_tiers.iter().copied().collect::<BTreeSet<_>>();
        if reasoning.len() != self.reasoning_levels.len()
            || tiers.len() != self.service_tiers.len()
            || !self.reasoning_levels.is_sorted()
            || !self.service_tiers.is_sorted()
        {
            return Err(FrameValidationError::ModelSettingsShape);
        }
        Ok(())
    }
}

/// Explicit acknowledgement carried by a root-placement creation or update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootPlacementGlobalReadIntent {
    /// The caller explicitly accepts that root placement grants global read.
    Acknowledged,
}

/// One session's opt-in dotted placement decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionPlacement {
    /// Preserve legacy unrestricted conversation-read behavior.
    Pathless {},
    /// Place below root; the parent directory's subtree is readable.
    Scoped { path: String },
    /// Place at root with loud acknowledgement that this grants global read.
    RootGlobalRead {
        path: String,
        intent: RootPlacementGlobalReadIntent,
    },
}

impl SessionPlacement {
    fn is_pathless(&self) -> bool {
        matches!(self, Self::Pathless {})
    }
    /// Constructs and validates a non-root placement.
    pub fn try_scoped(path: String) -> Result<Self, CanonicalValueError> {
        let placement = Self::Scoped { path };
        validate_session_placement_shape(&placement).map_err(|_| CanonicalValueError::Placement)?;
        Ok(placement)
    }

    /// Constructs and validates the loud root-global-read decision.
    pub fn try_root_global_read(path: String) -> Result<Self, CanonicalValueError> {
        let placement = Self::RootGlobalRead {
            path,
            intent: RootPlacementGlobalReadIntent::Acknowledged,
        };
        validate_session_placement_shape(&placement).map_err(|_| CanonicalValueError::Placement)?;
        Ok(placement)
    }
}

impl Default for SessionPlacement {
    fn default() -> Self {
        Self::Pathless {}
    }
}

fn validate_session_placement_shape(
    placement: &SessionPlacement,
) -> Result<(), FrameValidationError> {
    let (path, root) = match placement {
        SessionPlacement::Pathless {} => return Ok(()),
        SessionPlacement::Scoped { path } => (path, false),
        SessionPlacement::RootGlobalRead { path, .. } => (path, true),
    };
    if path.len() > 64 * 64 + 63 {
        return Err(FrameValidationError::PlacementShape);
    }
    let segment_count = path.split('.').try_fold(0_usize, |count, segment| {
        let next_count = count + 1;
        (next_count <= 64
            && !segment.is_empty()
            && segment.len() <= 64
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        .then_some(next_count)
    });
    let shape_valid = segment_count.is_some_and(|count| (count == 1) == root);
    if shape_valid {
        Ok(())
    } else {
        Err(FrameValidationError::PlacementShape)
    }
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
        Self::try_new_with_count_limits(title, tags, attributes, archived, None, None)
    }

    /// Validates canonical metadata and deployment tag and attribute policies.
    pub fn try_new_with_count_limits(
        title: Option<String>,
        tags: Vec<String>,
        attributes: Vec<(String, String)>,
        archived: bool,
        max_tags: Option<usize>,
        max_attributes: Option<usize>,
    ) -> Result<Self, CanonicalValueError> {
        let mut total_utf8_bytes = 0usize;
        if let Some(title) = title.as_deref() {
            validate_nonempty_metadata_text(title)?;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, title)?;
        }
        let tags = canonical_metadata_tags(tags, max_tags)?;
        for tag in &tags {
            add_metadata_utf8_bytes(&mut total_utf8_bytes, tag)?;
        }
        let attributes = MetadataAttributes::try_new(attributes, max_attributes)?;
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
    fn try_new(
        values: Vec<(String, String)>,
        maximum: Option<usize>,
    ) -> Result<Self, CanonicalValueError> {
        if maximum.is_some_and(|maximum| values.len() > maximum) {
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
    maximum: Option<usize>,
) -> Result<Vec<String>, CanonicalValueError> {
    if maximum.is_some_and(|maximum| values.len() > maximum) {
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

fn deserialize_session_metadata_tags<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)
}

fn deserialize_required_metadata_tags<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)
}

/// Closed actor provenance carried by a metadata last-writer stamp.
///
/// The variants mirror the domain actor inventory exactly, because durable
/// metadata already records every one of them: the tool-facing replacement
/// constructor stamps a tool writer, and a narrower wire enum would leave a
/// readable durable snapshot with no wire projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataActor {
    /// The user wrote the snapshot.
    User {},
    /// Daemon core wrote the snapshot without delegated agency.
    Core {},
    /// Model output from one exact turn wrote the snapshot.
    Model {
        /// The turn whose model output acted.
        turn_id: CanonicalUuid,
    },
    /// The startup recovery scan wrote the snapshot.
    Recovery {},
    /// Execution of one exact tool request wrote the snapshot.
    Tool {
        /// The tool request whose execution acted.
        tool_request_id: CanonicalUuid,
    },
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

/// One closed client-selected treatment for submitted input.
///
/// Omitting this value from `submit_input` preserves the baseline
/// start-when-idle treatment. Steering and queueing carry the exact active turn
/// the client observed so the domain can reject a stale target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputDelivery {
    /// Start new work only while the session slot is idle.
    StartWhenIdle {},
    /// Bind the input to the active turn's next safe point.
    Steer {
        /// Exact active turn observed by the client.
        expected_active_turn_id: CanonicalUuid,
    },
    /// Queue new work behind the active turn.
    Queue {
        /// Exact active turn observed by the client.
        expected_active_turn_id: CanonicalUuid,
    },
}

fn deserialize_present_input_delivery<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Option<InputDelivery>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    InputDelivery::deserialize(deserializer).map(Some)
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

fn validate_tool_approval_event_shape(
    decision: &ToolApprovalEventDecision,
    decider: &ToolApprovalEventDecider,
    rationale: &Option<String>,
) -> Result<(), FrameValidationError> {
    let shape_matches = match decider {
        ToolApprovalEventDecider::User { .. } => match decision {
            ToolApprovalEventDecision::Approve {}
            | ToolApprovalEventDecision::Deny { reason: None } => rationale.is_none(),
            ToolApprovalEventDecision::Deny {
                reason: Some(reason),
            } => rationale.is_none() && ToolDenialReason::try_new(reason.clone()).is_ok(),
        },
        ToolApprovalEventDecider::Delegate { .. } => match decision {
            ToolApprovalEventDecision::Approve {} => rationale
                .as_ref()
                .is_some_and(|rationale| ToolDecisionRationale::try_new(rationale.clone()).is_ok()),
            // A delegate denial's reason is exactly the derivation from its
            // rationale: absent only when the rationale derives nothing.
            ToolApprovalEventDecision::Deny { reason } => {
                rationale.as_ref().is_some_and(|rationale| {
                    ToolDecisionRationale::try_new(rationale.clone()).is_ok_and(|rationale| {
                        ToolDenialReason::from_rationale(&rationale)
                            .as_ref()
                            .map(ToolDenialReason::as_str)
                            == reason.as_deref()
                    })
                })
            }
        },
        ToolApprovalEventDecider::UserOverride { .. } => {
            matches!(decision, ToolApprovalEventDecision::Approve {}) && rationale.is_none()
        }
    };
    if !shape_matches {
        return Err(FrameValidationError::ToolApprovalShape);
    }
    Ok(())
}

/// Validates the structural display-title shape: nonempty single-line text
/// without U+0000 and no leading or trailing ASCII space or tab.
fn validate_imported_display_title(title: &str) -> Result<(), FrameValidationError> {
    if title.is_empty()
        || title.contains(['\0', '\n', '\r'])
        || title.starts_with([' ', '\t'])
        || title.ends_with([' ', '\t'])
    {
        return Err(FrameValidationError::ConversationListShape);
    }
    Ok(())
}

fn validate_session_template_name(value: &str) -> Result<(), FrameValidationError> {
    let first_is_admitted = value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    if value.len() > 128
        || !first_is_admitted
        || value.bytes().any(|byte| {
            !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && !b"._-".contains(&byte)
        })
    {
        return Err(FrameValidationError::TemplateShape);
    }
    Ok(())
}

fn validate_review_key(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > 1_024 || value.contains('\0') {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}

fn validate_review_text(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > 65_536 || value.contains('\0') {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}

fn validate_review_judgment_disposition(
    disposition: &ReviewJudgmentDisposition,
) -> Result<(), FrameValidationError> {
    if let ReviewJudgmentDisposition::Rejected { reason } = disposition {
        validate_review_text(reason)?;
    }
    Ok(())
}

fn validate_review_finding_event(event: &ReviewFindingEvent) -> Result<(), FrameValidationError> {
    match event {
        ReviewFindingEvent::Rejected { reason }
        | ReviewFindingEvent::BlockedWithReason { reason, .. } => validate_review_text(reason),
        ReviewFindingEvent::Accepted {}
        | ReviewFindingEvent::Duplicate { .. }
        | ReviewFindingEvent::Superseded { .. }
        | ReviewFindingEvent::Stale {}
        | ReviewFindingEvent::Fixed {} => Ok(()),
    }
}

fn validate_review_orchestration_snapshot(
    snapshot: &ReviewOrchestrationSnapshot,
) -> Result<(), FrameValidationError> {
    validate_review_key(&snapshot.concern_set_version)?;
    if snapshot.concerns.is_empty() || snapshot.concerns.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
        return Err(FrameValidationError::ReviewShape);
    }
    let mut keys = HashSet::new();
    for concern in &snapshot.concerns {
        validate_review_key(&concern.key)?;
        if !keys.insert(&concern.key) {
            return Err(FrameValidationError::ReviewShape);
        }
        let valid_pass = match concern.status {
            ReviewOrchestrationConcernStatus::Pending => concern.pass_id.is_none(),
            ReviewOrchestrationConcernStatus::Succeeded
            | ReviewOrchestrationConcernStatus::Failed
            | ReviewOrchestrationConcernStatus::Blocked
            | ReviewOrchestrationConcernStatus::Superseded => concern.pass_id.is_some(),
            ReviewOrchestrationConcernStatus::Cancelled => true,
        };
        if !valid_pass {
            return Err(FrameValidationError::ReviewShape);
        }
    }
    let pending_concern_count = snapshot
        .concerns
        .iter()
        .filter(|concern| concern.status == ReviewOrchestrationConcernStatus::Pending)
        .count();
    let all_concerns_succeeded = snapshot
        .concerns
        .iter()
        .all(|concern| concern.status == ReviewOrchestrationConcernStatus::Succeeded);
    let counts = snapshot.counts;
    let no_judgment_or_terminal_counts = counts.judgment_member_count.value() == 0
        && counts.judgment_effect_applied_count.value() == 0
        && counts.repair_fixed_count.value() == 0
        && counts.publication_published_count.value() == 0;
    let judgment_is_complete =
        counts.judgment_effect_applied_count.value() == counts.judgment_member_count.value();
    let judgment_is_incomplete =
        counts.judgment_member_count.value() > counts.judgment_effect_applied_count.value();
    let state_matches_facts = match snapshot.state {
        ReviewOrchestrationState::AwaitingImport | ReviewOrchestrationState::ImportIncomplete => {
            pending_concern_count == snapshot.concerns.len()
                && counts.finding_count.value() == 0
                && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingConcerns => {
            pending_concern_count > 0 && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::FanoutIncomplete => {
            pending_concern_count == 0 && !all_concerns_succeeded && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingJudgment => {
            all_concerns_succeeded && no_judgment_or_terminal_counts
        }
        ReviewOrchestrationState::AwaitingJudgmentEffects
        | ReviewOrchestrationState::JudgmentIncomplete => {
            all_concerns_succeeded
                && judgment_is_incomplete
                && counts.repair_fixed_count.value() == 0
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::AwaitingRepair => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.repair_fixed_count.value() == 0
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::RepairIncomplete => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::AwaitingPublication => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() == 0
        }
        ReviewOrchestrationState::PublicationIncomplete => {
            all_concerns_succeeded
                && judgment_is_complete
                && counts.publication_published_count.value() < counts.judgment_member_count.value()
        }
        ReviewOrchestrationState::Complete => all_concerns_succeeded && judgment_is_complete,
    };
    if !state_matches_facts {
        return Err(FrameValidationError::ReviewShape);
    }
    if counts.finding_count.value() > MAX_REVIEW_ORCHESTRATION_MEMBERS as u64
        || counts.judgment_member_count.value() > counts.finding_count.value()
        || counts.judgment_effect_applied_count.value() > counts.judgment_member_count.value()
        || counts.repair_fixed_count.value() > counts.judgment_member_count.value()
        || counts.publication_published_count.value() > counts.judgment_member_count.value()
        || counts
            .repair_fixed_count
            .value()
            .checked_add(counts.publication_published_count.value())
            .is_none_or(|terminal_count| terminal_count > counts.judgment_member_count.value())
    {
        return Err(FrameValidationError::ReviewShape);
    }
    Ok(())
}

/// Closed durable goal-command rejection vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalCommandRejection {
    /// The target session does not exist.
    SessionNotFound,
    /// The session's closure is pending; the closure settles the goal.
    SessionClosing,
    /// A goal is already pursuing or blocked.
    GoalAlreadyAttached,
    /// The session has no goal lineage.
    GoalNotAttached,
    /// The session's selected model alias is absent from daemon configuration.
    UnknownModelAlias,
    /// The session accepted-input position cannot advance beyond `u64::MAX`.
    AcceptancePositionExhausted,
    /// Resume requires a blocked current generation.
    RequiresBlocked,
    /// Stop or supersede requires a pursuing or blocked generation.
    RequiresPursuingOrBlocked,
    /// No successor generation can be represented.
    GenerationExhausted,
    /// No successor event position can be represented.
    EventOrdinalExhausted,
}

/// Closed blocked-reason vocabulary at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalBlockedReason {
    /// Progress requires information or a decision from the user.
    UserInputRequired,
    /// Progress requires an external state change.
    ExternalChangeRequired,
    /// Progress requires authority the session does not hold.
    AuthorizationRequired,
    /// The preceding goal turn failed and was not retried.
    ExecutionFailure,
    FinishCheckFailed,
}

/// Provenance for one blocked event at the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalBlockedProvenance {
    /// The model declared the blocked transition through its correlated tool.
    Model {
        /// Exact invoking turn.
        turn_id: CanonicalUuid,
        /// Exact invoking tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The scheduler observed one unsuccessfully terminalized goal turn.
    ExecutionFailure {
        /// Exact failed turn.
        turn_id: CanonicalUuid,
    },
}

/// One generation's derived lifecycle state at the process boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalLifecycleState {
    /// Autonomous scheduling continues.
    Pursuing {},
    /// Autonomous scheduling pauses pending an explicit user transition.
    Blocked {
        /// Closed blocked reason.
        reason: GoalBlockedReason,
        /// Exact statement of what is needed.
        need: String,
    },
    /// The model declared completion.
    Achieved {
        /// Turn containing the final-report declaration.
        turn_id: CanonicalUuid,
        /// Tool request immediately preceded by the final-report transcript part.
        tool_request_id: CanonicalUuid,
    },
    /// The user explicitly ended this generation.
    UserStopped {},
    /// Another immutable statement replaced this generation.
    Superseded {
        /// Successor generation commissioned by the same event.
        by_generation: CanonicalU64,
    },
    /// The session closed beneath this generation.
    SessionClosed {
        /// Closed session outcome that settled it.
        outcome: SessionClosureOutcome,
    },
}

/// The closed session outcomes that settle a live goal generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionClosureOutcome {
    /// Closed with a retryable cause standing.
    FailedRetryable,
    /// Closed with a structural cause standing.
    FailedStructural,
    /// Closed with no classified cause.
    FailedUnknown,
    /// A human or rule stopped the session.
    Stopped,
    /// A newer session owns the work, or the work is gone.
    Superseded,
    /// An operator wrote the session off.
    Abandoned,
    /// The session never did the work and never will.
    Retired,
}

/// The closed actor classification recorded with a lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleActorClass {
    /// Daemon core.
    Core,
    /// The single user's authority.
    Operator,
    /// A module, without saying which: the classification is what the
    /// boundary carries, and the durable goal event keeps the exact module.
    Module,
    /// The recovery scan or liveness watchdog.
    Watchdog,
}

/// One append-only goal event payload at the process boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalHistoryEvent {
    /// The user commissioned an immutable statement.
    Commissioned {
        /// Exact immutable statement.
        statement: String,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// Pursuit paused with a typed reason and exact need.
    Blocked {
        /// Closed reason.
        reason: GoalBlockedReason,
        /// Exact statement of what is needed.
        need: String,
        /// Typed transition provenance.
        provenance: GoalBlockedProvenance,
    },
    /// The user resumed blocked pursuit.
    Resumed {
        /// Optional exact next-turn guidance.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        guidance: Option<String>,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// The model declared achievement with its final report.
    Achieved {
        /// Exact final report.
        report: String,
        /// Invoking turn.
        turn_id: CanonicalUuid,
        /// Invoking tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The user explicitly ended the generation.
    UserStopped {
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// The user atomically replaced the active statement.
    Superseded {
        /// Newly commissioned immutable statement.
        replacement_statement: String,
        /// Durable user command provenance.
        command_id: CommandId,
    },
    /// The session closed, settling this generation.
    SessionClosed {
        /// Closed session outcome that settled it.
        outcome: SessionClosureOutcome,
        /// Classified actor that closed the session.
        actor: LifecycleActorClass,
    },
}

fn validate_goal_text(value: &str) -> Result<(), FrameValidationError> {
    if value.is_empty() || value.len() > MAX_CONTENT_FRAGMENT_BYTES || value.contains('\0') {
        return Err(FrameValidationError::GoalShape);
    }
    Ok(())
}

fn validate_goal_state(state: &GoalLifecycleState) -> Result<(), FrameValidationError> {
    match state {
        GoalLifecycleState::Blocked { need, .. } => validate_goal_text(need),
        GoalLifecycleState::Superseded { by_generation } if by_generation.value() == 0 => {
            Err(FrameValidationError::GoalShape)
        }
        GoalLifecycleState::Pursuing {}
        | GoalLifecycleState::Achieved { .. }
        | GoalLifecycleState::UserStopped {}
        | GoalLifecycleState::Superseded { .. }
        | GoalLifecycleState::SessionClosed { .. } => Ok(()),
    }
}

fn validate_goal_event(event: &GoalHistoryEvent) -> Result<(), FrameValidationError> {
    match event {
        GoalHistoryEvent::Commissioned { statement, .. } => validate_goal_text(statement),
        GoalHistoryEvent::Blocked {
            reason,
            need,
            provenance,
        } => {
            validate_goal_text(need)?;
            let scheduler_reason = match reason {
                GoalBlockedReason::UserInputRequired
                | GoalBlockedReason::ExternalChangeRequired
                | GoalBlockedReason::AuthorizationRequired
                | GoalBlockedReason::FinishCheckFailed => false,
                GoalBlockedReason::ExecutionFailure => true,
            };
            let scheduler_provenance = match provenance {
                GoalBlockedProvenance::Model { .. } => false,
                GoalBlockedProvenance::ExecutionFailure { .. } => true,
            };
            if scheduler_reason != scheduler_provenance {
                return Err(FrameValidationError::GoalShape);
            }
            Ok(())
        }
        GoalHistoryEvent::Resumed {
            guidance: Some(guidance),
            ..
        } => validate_goal_text(guidance),
        GoalHistoryEvent::Achieved { report, .. } => validate_goal_text(report),
        GoalHistoryEvent::Superseded {
            replacement_statement,
            ..
        } => validate_goal_text(replacement_statement),
        GoalHistoryEvent::Resumed { guidance: None, .. }
        | GoalHistoryEvent::UserStopped { .. }
        | GoalHistoryEvent::SessionClosed { .. } => Ok(()),
    }
}

/// Explicit delegated-child scope selected by a parent termination request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescendantTerminationScope {
    /// Apply the stop only to the named parent session.
    ParentAlone,
    /// Evaluate every reachable delegated-child relationship.
    ParentAndDescendants,
}

/// Singleton key class shown by repository-watch operator status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusSingletonScope {
    PullRequest,
    Stack,
    Rule,
    Repo,
}

/// One independently failing held-slot release clause.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusHeldSlotBlocker {
    UndeliveredAction,
    DeliveryTurnRuntimeRelevant,
    LiveRuntimeTurn,
    PursuingGoal,
}

/// Current provider mergeability shown by repository-watch operator status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusMergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

/// Current provider review decision shown by repository-watch operator status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusReviewDecision {
    None,
    Approved,
    ReviewRequired,
    ChangesRequested,
}

/// Latest repository-watch convergence verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusConvergenceVerdict {
    NotConverged,
    InternallyConverged,
    MergeReady,
}

/// Durable convergence seal attached to the latest assessment, when any.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusConvergenceSeal {
    InternallyConverged,
    MergeReady,
}

/// Immutable authority fence a commissioned-session request records.
///
/// The shapes mirror the repository-watch dispatch fence: a pull-request fence
/// names the pull request, its exact head commit, the repository and branch
/// holding that head, and the base branch; a branch fence names the repository
/// and branch alone. Field admission (slug, commit, and branch grammar) is the
/// daemon's, at command construction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommissionedSessionFence {
    /// Exact pull-request authority for the commissioned session.
    PullRequest {
        /// Repository whose pull request the session is commissioned against.
        repository: String,
        /// Positive pull-request number within the repository.
        pull_request: CanonicalU64,
        /// Exact head commit authorized at commissioning time.
        head_sha: String,
        /// Repository containing the authorized head branch.
        head_repository: String,
        /// Authorized head branch.
        head_branch: String,
        /// Authorized base branch.
        base_branch: String,
    },
    /// Exact branch authority for the commissioned session.
    Branch {
        /// Repository whose branch the session is commissioned against.
        repository: String,
        /// Authorized branch.
        branch: String,
    },
}

/// Whether a creation holds its start gate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartGate {
    /// The session may dispatch as soon as it has input.
    #[default]
    Open,
    /// The session stays `created` until `release_start` or gate expiry.
    Held,
}

/// Whether the daemon holds a liveness obligation for the session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOwnership {
    /// The daemon drives the session to a declared terminal outcome.
    Owned,
    /// A conversation the daemon does not drive.
    #[default]
    Unmonitored,
}

/// Closed finish condition an owned session owes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinishCondition {
    /// Completion is declared outside the session.
    ExternalGate,
    /// Completion is checked against exact declared text.
    Declared {
        /// Exact statement the finish check evaluates.
        statement: String,
    },
}

/// The lifecycle members of a creation: omission means an open gate, an
/// unmonitored conversation, and no finish condition.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLifecycleMembers {
    /// Whether the creation holds its start gate.
    #[serde(default)]
    pub start_gate: StartGate,
    /// The ownership the creation establishes.
    #[serde(default)]
    pub ownership: SessionOwnership,
    /// The finish condition an owned session owes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_condition: Option<FinishCondition>,
}

impl SessionLifecycleMembers {
    /// Whether every member holds its omission value.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The standing failure cause a parked session closes with.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFailureCause {
    ProviderTransient,
    ProviderQuotaExhausted,
    ProviderOverloaded,
    InfrastructureFailure,
    RetryBudgetExhausted,
    ContextCompactionWall,
    ContextHeadroomExhausted,
    BrokenToolchain,
    ModerationBlock,
}

/// Closed session-lifecycle command rejection vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleCommandRejection {
    SessionNotFound,
    TransitionNotAdmitted,
    RequiresParked,
    ReleaseWhileParked,
    OwnershipUnchanged,
    FinishConditionAlreadyDeclared,
    StandingCauseMismatch,
    SuccessorNotFound,
    SuccessorIsSelf,
    GoalResumeRequired,
    GoalOutcomeMismatch,
    PendingTerminalConflict,
}

/// What an applied lifecycle command did.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionLifecycleEffect {
    /// The held start gate opened.
    StartReleased {},
    /// The session recorded terminal.
    Closed {},
    /// The outcome is committed; the named live turn settles first.
    ClosurePending {
        /// The turn the committed interrupt machinery settles.
        live_turn_id: CanonicalUuid,
    },
    /// The park lifted.
    Resumed {},
    /// The ownership bit flipped.
    OwnershipChanged {},
}

/// Closed versioned request family.

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    /// Create a user-initiated session.
    CreateSession {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Initial session model-selection defaults.
        initial_model_selection: ModelSelection,
        /// Initial session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
        /// Optional initial system prompt; required null-or-text member.
        #[serde(default, skip_serializing_if = "SystemPromptMember::is_absent")]
        system_prompt: SystemPromptMember,
        /// Explicit opt-in placement, defaulting to legacy pathless behavior.
        #[serde(default, skip_serializing_if = "SessionPlacement::is_pathless")]
        placement: SessionPlacement,
        /// Start gate, ownership, and finish condition.
        #[serde(default, skip_serializing_if = "SessionLifecycleMembers::is_default")]
        lifecycle: SessionLifecycleMembers,
    },
    /// Create a user-initiated session from one daemon-held template.
    CreateSessionFromTemplate {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Validated static template name.
        template_name: String,
        /// Explicit opt-in placement, defaulting to legacy pathless behavior.
        #[serde(default, skip_serializing_if = "SessionPlacement::is_pathless")]
        placement: SessionPlacement,
        /// Start gate, ownership, and finish condition.
        #[serde(default, skip_serializing_if = "SessionLifecycleMembers::is_default")]
        lifecycle: SessionLifecycleMembers,
    },
    /// Atomically commission one session from a daemon-held template: create
    /// it under a recorded immutable authority fence, attach its goal, and
    /// submit its first input through the start-when-idle path.
    CommissionSession {
        /// Durable mutation identity for the whole composite.
        command_id: CommandId,
        /// Validated static template name.
        template_name: String,
        /// Immutable authority fence recorded for the created session.
        fence: CommissionedSessionFence,
        /// Exact immutable goal statement.
        statement: String,
        /// Exact first-input text carried to the created session.
        content: InputContent,
    },
    /// List available static templates by name and version.
    ListTemplates {},
    /// Read client-relevant deployment policy for this connection.
    ReadDeploymentLimits {},
    /// List current sessions.
    ListSessions {},
    /// Read one coherent repository-watch operator-status snapshot.
    ReadOperatorStatus {},
    /// Append one explicit immutable session-placement update event.
    UpdateSessionPlacement {
        command_id: CommandId,
        session_id: CanonicalUuid,
        expected_placement_version: CanonicalU64,
        replacement: SessionPlacement,
    },
    /// Attach one immutable commissioned goal statement.
    AttachGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact immutable statement.
        statement: String,
    },
    /// Read the current goal projection and complete ordered event history.
    ReadGoal {
        /// Target session.
        session_id: CanonicalUuid,
    },
    /// Resume a blocked goal with optional next-turn guidance.
    ResumeGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Optional exact next-turn guidance.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        guidance: Option<String>,
    },
    /// Explicitly stop a pursuing or blocked goal.
    StopGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
    },
    /// Atomically replace the active immutable statement.
    SupersedeGoal {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Newly commissioned immutable statement.
        statement: String,
    },
    /// Close a session `stopped{sticky}` from any non-terminal state.
    StopSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        /// Whether re-dispatch stays suppressed until the source is updated.
        sticky: bool,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
    },
    /// Close a session `superseded{by}` in favour of its successor.
    SupersedeSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        /// The session that takes the work.
        successor_session_id: CanonicalUuid,
    },
    /// Write off a parked session as `abandoned`.
    AbandonSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Close a parked session as failed; null closes with its standing cause.
    CloseSessionFailed {
        command_id: CommandId,
        session_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        cause: Option<SessionFailureCause>,
    },
    /// Return a parked session whose goal is not blocked to its mapped state.
    ResumeSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Take the liveness obligation, optionally supplying a finish condition.
    AdoptSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        finish_condition: Option<FinishCondition>,
    },
    /// Drop the liveness obligation.
    ReleaseSession {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Open a held start gate so queued admission work may dispatch.
    ReleaseStart {
        command_id: CommandId,
        session_id: CanonicalUuid,
    },
    /// Submit user input with an admitted delivery treatment.
    SubmitInput {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Exact ordered user parts.
        content: UserInputContent,
        /// Caller-observed defaults version, or null for configuration-free
        /// steering.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        expected_defaults_version: Option<CanonicalU64>,
        /// Per-call settings contribution; steering must inherit every knob.
        model_settings: ModelSettingsOverlay,
        /// Optional delivery treatment; absence selects the start-when-idle default.
        #[serde(
            default,
            deserialize_with = "deserialize_present_input_delivery",
            skip_serializing_if = "Option::is_none"
        )]
        delivery: Option<InputDelivery>,
    },
    /// Compact one session's model-visible history without rewriting it.
    CompactSession {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// Optional one-based semantic position to summarize through; null
        /// selects the latest safe boundary.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        through_position: Option<CanonicalU64>,
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
    /// Execute one exact already-issued delegated-session spawn request.
    SpawnSession {
        /// Invoking parent session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical spawn tool request.
        tool_request_id: CanonicalUuid,
        /// Exact bounded child task.
        task: String,
        /// Parent-chosen lifecycle relationship.
        relationship: DelegationPolicy,
    },
    /// Register delivery for one related child.
    AwaitSession {
        /// Invoking parent session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical await tool request.
        tool_request_id: CanonicalUuid,
        /// Related child whose result is awaited.
        child_session_id: CanonicalUuid,
        /// Foreground or background delivery mode.
        mode: DelegationWaitMode,
    },
    /// Send one bounded message across an existing delegation relationship.
    SendSessionMessage {
        /// Invoking session.
        session_id: CanonicalUuid,
        /// Turn that issued the tool request.
        turn_id: CanonicalUuid,
        /// Exact logical message tool request.
        tool_request_id: CanonicalUuid,
        /// Related peer receiving the message.
        peer_session_id: CanonicalUuid,
        /// Exact bounded message content.
        content: String,
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
    /// Read the deployment's complete configured model-alias catalog.
    ListModelAliases {},
    /// Read the deployment's complete per-model capability catalog.
    ListModelCapabilities {},
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
        /// Complete replacement session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
        /// Complete replacement dangerous-tool blanket-auto posture.
        dangerous_tool_auto_approval: bool,
        /// Complete replacement system prompt; required null-or-text member.
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
    /// Begin one per-connection chunked conversation import.
    BeginConversationImport {
        /// Explicit format-versioned converter selection.
        format: ConversationImportFormat,
        /// Exact total source size the caller will append.
        declared_size_bytes: CanonicalU64,
    },
    /// Append one source chunk to the connection's in-progress import.
    AppendConversationImport {
        /// Next exact source bytes in physical order.
        chunk: ConversationImportSource,
    },
    /// Convert and store the connection's completely appended source.
    CommitConversationImport {},
    /// Discard the connection's in-progress import without conversion.
    AbortConversationImport {},
    /// Begin one connection-local immutable user-attachment upload.
    BeginBlobUpload {
        /// Exact content identity the caller computed before upload.
        expected_digest: CanonicalBlobDigest,
        /// Exact positive byte length the caller will append.
        expected_length_bytes: CanonicalU64,
    },
    /// Append one bounded chunk to the connection's active blob upload.
    AppendBlobUpload {
        /// Next exact bytes in physical order.
        chunk: BlobChunk,
    },
    /// Verify, publish, and catalogue the active blob upload.
    CommitBlobUpload {},
    /// Discard the connection's active blob upload.
    AbortBlobUpload {},
    /// Read bounded catalog metadata for one immutable blob.
    ReadBlobMetadata { digest: CanonicalBlobDigest },
    /// Read one exact bounded range after full replica verification.
    ReadBlobChunk {
        digest: CanonicalBlobDigest,
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
    },
    /// Read one immutable imported conversation's complete entry inventory.
    ///
    /// The read exposes the ordinals `create_session_from_imported_frontier`
    /// consumes; it creates nothing and seeds nothing.
    ReadImportedConversation {
        /// Immutable imported conversation to inspect.
        imported_conversation_id: CanonicalUuid,
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
        /// Initial session-layer settings contribution.
        model_settings: ModelSettingsOverlay,
    },
    /// Reconcile the exact active turn parked on an ambiguous model call.
    ///
    /// The named turn must be the session's active turn and must be parked in
    /// the model-call recovery wait. The request supplies the user interrupt
    /// authority that turn's terminal disposition requires and carries the
    /// successor input the session continues with.
    ReconcileTurn {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Target session.
        session_id: CanonicalUuid,
        /// The turn the caller observed parked awaiting reconciliation.
        expected_active_turn_id: CanonicalUuid,
        /// Exact ordered user parts for the immediate successor turn.
        content: UserInputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
        /// Per-call settings contribution for the immediate successor origin.
        model_settings: ModelSettingsOverlay,
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
    /// Conclude one pass that carries no other typed result payload.
    CompleteReviewPass {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        turn_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        output_frontier_id: Option<CanonicalUuid>,
        outcome: ReviewPassTerminalOutcome,
    },
    RecordReviewFindings {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        output_frontier_id: CanonicalUuid,
        findings: Vec<ReviewFindingInput>,
    },
    /// Atomically conclude a result-bearing pass and append one finding event.
    RecordReviewFindingEvent {
        command_id: CommandId,
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        output_frontier_id: Option<CanonicalUuid>,
        finding_id: CanonicalUuid,
        /// Exact contiguous event ordinal the appended disposition occupies.
        event_ordinal: CanonicalU64,
        event: ReviewFindingEvent,
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
    /// Start one immutable client-driven orchestration attempt.
    StartReviewOrchestration {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        target_id: CanonicalUuid,
        concern_set_version: String,
        import_template_name: String,
        judgment_template_name: String,
        repair_template_name: String,
        publication_template_name: String,
        concerns: Vec<ReviewOrchestrationConcernInput>,
    },
    /// Seal the import stage outcome.
    RecordReviewImportOutcome {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        external_link_id: Option<CanonicalUuid>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        context_digest: Option<CanonicalDigest>,
        outcome: ReviewImportTerminalOutcome,
    },
    /// Seal one frozen concern member outcome.
    RecordReviewConcernOutcome {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        concern: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        pass_id: Option<CanonicalUuid>,
        outcome: ReviewConcernTerminalOutcome,
    },
    /// Seal the complete judgment plan over a succeeded fan-out.
    RecordReviewJudgmentPlan {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        analysis_pass_id: CanonicalUuid,
        members: Vec<ReviewJudgmentPlanMember>,
    },
    /// Seal the result of applying one judgment-plan member.
    RecordReviewJudgmentEffect {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        finding_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        event_pass_id: Option<CanonicalUuid>,
        outcome: ReviewJudgmentEffectTerminalOutcome,
    },
    /// Seal the complete repair-stage member inventory.
    RecordReviewRepairOutcomes {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        outcomes: Vec<ReviewRepairOutcome>,
    },
    /// Seal the complete publication-stage member inventory.
    RecordReviewPublicationOutcomes {
        command_id: CommandId,
        attempt_id: CanonicalUuid,
        outcomes: Vec<ReviewPublicationOutcome>,
    },
    /// Read one complete orchestration attempt projection.
    ReadReviewOrchestration { attempt_id: CanonicalUuid },
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
        /// Exact ordered user parts for the immediate successor turn.
        content: UserInputContent,
        /// Caller-observed defaults version.
        expected_defaults_version: CanonicalU64,
        /// Explicit delegated-child scope.
        descendant_scope: DescendantTerminationScope,
        /// Per-call settings contribution for the immediate successor origin.
        model_settings: ModelSettingsOverlay,
    },
    /// Supply the user decision for one pending tool request.
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
    /// Record one one-shot user override of a delegate-denied tool request.
    OverrideDeniedToolRequest {
        /// Durable mutation identity.
        command_id: CommandId,
        /// Session the override covers; part of the canonical payload.
        session_id: CanonicalUuid,
        /// Exact delegate-denied logical tool request.
        tool_request_id: CanonicalUuid,
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
        /// Exact user explanation rendered to the model.
        reason: String,
    },
}

impl ClientRequest {
    fn validate(&self) -> Result<(), FrameValidationError> {
        match self {
            Self::AttachGoal { statement, .. }
            | Self::SupersedeGoal { statement, .. }
            | Self::CommissionSession { statement, .. } => {
                validate_goal_text(statement)?;
            }
            Self::ResumeGoal {
                guidance: Some(guidance),
                ..
            } => validate_goal_text(guidance)?,
            Self::AdoptSession {
                finish_condition: Some(FinishCondition::Declared { statement }),
                ..
            } => validate_goal_text(statement)?,
            Self::CreateSession {
                lifecycle:
                    SessionLifecycleMembers {
                        finish_condition: Some(FinishCondition::Declared { statement }),
                        ..
                    },
                ..
            }
            | Self::CreateSessionFromTemplate {
                lifecycle:
                    SessionLifecycleMembers {
                        finish_condition: Some(FinishCondition::Declared { statement }),
                        ..
                    },
                ..
            } => validate_goal_text(statement)?,
            Self::CreateSession { .. }
            | Self::CreateSessionFromTemplate { .. }
            | Self::ListTemplates {}
            | Self::ReadDeploymentLimits {}
            | Self::ListSessions {}
            | Self::ReadOperatorStatus {}
            | Self::UpdateSessionPlacement { .. }
            | Self::ReadGoal { .. }
            | Self::ResumeGoal { guidance: None, .. }
            | Self::StopGoal { .. }
            | Self::StopSession { .. }
            | Self::SupersedeSession { .. }
            | Self::AbandonSession { .. }
            | Self::CloseSessionFailed { .. }
            | Self::ResumeSession { .. }
            | Self::AdoptSession {
                finish_condition: None,
                ..
            }
            | Self::AdoptSession {
                finish_condition: Some(FinishCondition::ExternalGate),
                ..
            }
            | Self::ReleaseSession { .. }
            | Self::ReleaseStart { .. }
            | Self::SubmitInput { .. }
            | Self::CompactSession { .. }
            | Self::ReadTranscript { .. }
            | Self::FollowSession { .. }
            | Self::SpawnSession { .. }
            | Self::AwaitSession { .. }
            | Self::SendSessionMessage { .. }
            | Self::ListSessionMetadata { .. }
            | Self::ListConversations { .. }
            | Self::ListModelAliases {}
            | Self::ListModelCapabilities {}
            | Self::ReadSessionMetadata { .. }
            | Self::ReplaceSessionMetadata { .. }
            | Self::ReplaceSessionDefaults { .. }
            | Self::ReadSessionDefaults { .. }
            | Self::ImportConversation { .. }
            | Self::BeginConversationImport { .. }
            | Self::AppendConversationImport { .. }
            | Self::CommitConversationImport {}
            | Self::AbortConversationImport {}
            | Self::BeginBlobUpload { .. }
            | Self::AppendBlobUpload { .. }
            | Self::CommitBlobUpload {}
            | Self::AbortBlobUpload {}
            | Self::ReadBlobMetadata { .. }
            | Self::ReadBlobChunk { .. }
            | Self::ReadImportedConversation { .. }
            | Self::CreateSessionFromImportedFrontier { .. }
            | Self::ReconcileTurn { .. }
            | Self::CreateReviewTarget { .. }
            | Self::StartReviewRun { .. }
            | Self::ActivateReviewPass { .. }
            | Self::CompleteReviewPass { .. }
            | Self::RecordReviewFindings { .. }
            | Self::RecordReviewFindingEvent { .. }
            | Self::ReserveReviewExternalLink { .. }
            | Self::AttachReviewExternalLink { .. }
            | Self::ReadReviewTarget { .. }
            | Self::ReadReviewRun { .. }
            | Self::ReadReviewFinding { .. }
            | Self::ListReviewFindings { .. }
            | Self::StartReviewOrchestration { .. }
            | Self::RecordReviewImportOutcome { .. }
            | Self::RecordReviewConcernOutcome { .. }
            | Self::RecordReviewJudgmentPlan { .. }
            | Self::RecordReviewJudgmentEffect { .. }
            | Self::RecordReviewRepairOutcomes { .. }
            | Self::RecordReviewPublicationOutcomes { .. }
            | Self::ReadReviewOrchestration { .. }
            | Self::StopTurn { .. }
            | Self::DecideToolRequest { .. }
            | Self::OverrideDeniedToolRequest { .. } => {}
        }
        match self {
            Self::CreateSession { placement, .. }
            | Self::CreateSessionFromTemplate { placement, .. }
            | Self::UpdateSessionPlacement {
                replacement: placement,
                ..
            } => validate_session_placement_shape(placement)?,
            _ => {}
        }
        if let Self::UpdateSessionPlacement {
            expected_placement_version,
            ..
        } = self
            && expected_placement_version.value() == 0
        {
            return Err(FrameValidationError::PlacementShape);
        }
        if let Self::CommissionSession {
            fence:
                CommissionedSessionFence::PullRequest {
                    pull_request: number,
                    ..
                },
            ..
        } = self
            && number.value() == 0
        {
            return Err(FrameValidationError::DispatchFenceShape);
        }
        if let Self::SubmitInput {
            expected_defaults_version,
            delivery,
            model_settings,
            content,
            ..
        } = self
        {
            content.validate()?;
            let valid = matches!(
                (delivery, expected_defaults_version),
                (None | Some(InputDelivery::StartWhenIdle {}), Some(_))
                    | (Some(InputDelivery::Steer { .. }), None)
                    | (Some(InputDelivery::Queue { .. }), Some(_))
            );
            if !valid {
                return Err(FrameValidationError::InputDeliveryShape);
            }
            if matches!(delivery, Some(InputDelivery::Steer { .. }))
                && *model_settings != ModelSettingsOverlay::inherit_all()
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
        }
        if let Self::ReconcileTurn { content, .. } | Self::StopTurn { content, .. } = self {
            content.validate()?;
        }
        if let Self::AppendConversationImport { chunk } = self
            && (chunk.as_bytes().is_empty()
                || chunk.as_bytes().len() > MAX_CONVERSATION_IMPORT_CHUNK_BYTES)
        {
            return Err(FrameValidationError::ConversationImportShape);
        }
        if let Self::AppendBlobUpload { chunk } = self
            && (chunk.as_bytes().is_empty() || chunk.as_bytes().len() > MAX_BLOB_CHUNK_BYTES)
        {
            return Err(FrameValidationError::BlobUploadShape);
        }
        if let Self::CreateSessionFromImportedFrontier {
            through_position, ..
        } = self
            && through_position.value() == 0
        {
            return Err(FrameValidationError::ImportedFrontierShape);
        }
        if let Self::CompactSession {
            through_position: Some(position),
            ..
        } = self
            && position.value() == 0
        {
            return Err(FrameValidationError::ContextCompactionShape);
        }
        if let Self::ListSessionMetadata {
            required_tags,
            title_contains,
            ..
        } = self
        {
            let canonical_tags = canonical_metadata_tags(required_tags.clone(), None)
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
        }
        if let Self::ListConversations {
            title_contains: Some(query),
            ..
        } = self
        {
            validate_nonempty_metadata_text(query)
                .map_err(|_| FrameValidationError::ConversationListShape)?;
            let mut total_utf8_bytes = 0usize;
            add_metadata_utf8_bytes(&mut total_utf8_bytes, query)
                .map_err(|_| FrameValidationError::ConversationListShape)?;
        }
        if let Self::CreateSessionFromTemplate { template_name, .. } = self {
            validate_session_template_name(template_name)?;
        }
        if let Self::CommissionSession { template_name, .. } = self {
            validate_session_template_name(template_name)?;
        }
        if let Self::CompleteReviewPass {
            turn_id,
            output_frontier_id,
            outcome,
            ..
        } = self
        {
            let valid = matches!(
                (outcome, turn_id, output_frontier_id),
                (ReviewPassTerminalOutcome::Succeeded, Some(_), Some(_))
                    | (
                        ReviewPassTerminalOutcome::Failed | ReviewPassTerminalOutcome::Blocked,
                        Some(_),
                        None
                    )
                    | (ReviewPassTerminalOutcome::Cancelled, _, None)
            );
            if !valid {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewFindings { findings, .. } = self
            && findings.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS
        {
            return Err(FrameValidationError::ReviewShape);
        }
        if let Self::RecordReviewFindingEvent {
            finding_id,
            output_frontier_id,
            event,
            ..
        } = self
        {
            validate_review_finding_event(event)?;
            let blocked = matches!(event, ReviewFindingEvent::BlockedWithReason { .. });
            if blocked == output_frontier_id.is_some() {
                return Err(FrameValidationError::ReviewShape);
            }

            let self_reference = match event {
                ReviewFindingEvent::Duplicate {
                    canonical_finding_id,
                } => *canonical_finding_id == *finding_id,
                ReviewFindingEvent::Superseded {
                    successor_finding_id,
                } => *successor_finding_id == *finding_id,
                _ => false,
            };
            if self_reference {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::StartReviewOrchestration {
            concern_set_version,
            import_template_name,
            judgment_template_name,
            repair_template_name,
            publication_template_name,
            concerns,
            ..
        } = self
        {
            validate_review_key(concern_set_version)?;
            validate_session_template_name(import_template_name)?;
            validate_session_template_name(judgment_template_name)?;
            validate_session_template_name(repair_template_name)?;
            validate_session_template_name(publication_template_name)?;
            if concerns.is_empty() || concerns.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut keys = HashSet::new();
            let mut templates = HashSet::new();
            for concern in concerns {
                validate_review_key(&concern.key)?;
                validate_session_template_name(&concern.template_name)?;
                if !keys.insert(&concern.key) || !templates.insert(&concern.template_name) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewImportOutcome {
            pass_id,
            external_link_id,
            context_digest,
            outcome,
            ..
        } = self
        {
            let valid = match outcome {
                ReviewImportTerminalOutcome::Succeeded => {
                    pass_id.is_some() && context_digest.is_some()
                }
                ReviewImportTerminalOutcome::Failed | ReviewImportTerminalOutcome::Blocked => {
                    pass_id.is_some() && external_link_id.is_none() && context_digest.is_none()
                }
                ReviewImportTerminalOutcome::Cancelled => {
                    external_link_id.is_none() && context_digest.is_none()
                }
            };
            if !valid
                || (*outcome != ReviewImportTerminalOutcome::Succeeded
                    && external_link_id.is_some())
            {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewConcernOutcome {
            concern,
            pass_id,
            outcome,
            ..
        } = self
        {
            validate_review_key(concern)?;
            if *outcome != ReviewConcernTerminalOutcome::Cancelled && pass_id.is_none() {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewJudgmentPlan { members, .. } = self {
            if members.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for member in members {
                validate_review_judgment_disposition(&member.disposition)?;
                if !findings.insert(member.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewJudgmentEffect {
            event_pass_id,
            outcome,
            ..
        } = self
        {
            let valid = (*outcome == ReviewJudgmentEffectTerminalOutcome::Applied)
                == event_pass_id.is_some();
            if !valid {
                return Err(FrameValidationError::ReviewShape);
            }
        }
        if let Self::RecordReviewRepairOutcomes { outcomes, .. } = self {
            if outcomes.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for outcome in outcomes {
                let valid = (outcome.outcome == ReviewRepairTerminalOutcome::Fixed)
                    == outcome.event_pass_id.is_some();
                if !valid || !findings.insert(outcome.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
            }
        }
        if let Self::RecordReviewPublicationOutcomes { outcomes, .. } = self {
            if outcomes.len() > MAX_REVIEW_ORCHESTRATION_MEMBERS {
                return Err(FrameValidationError::ReviewShape);
            }
            let mut findings = HashSet::new();
            for outcome in outcomes {
                let valid = (outcome.outcome == ReviewPublicationTerminalOutcome::Published)
                    == outcome.external_link_id.is_some();
                if !valid || !findings.insert(outcome.finding_id) {
                    return Err(FrameValidationError::ReviewShape);
                }
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
    /// Constructs a single-version frame with a correlated request identity.
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
        if let ClientRequest::CreateSession { system_prompt, .. }
        | ClientRequest::ReplaceSessionDefaults { system_prompt, .. } = &self.request
        {
            validate_system_prompt_member(system_prompt)?;
        }
        self.request.validate()
    }
}

/// Requires the presence-checked system-prompt member.
fn validate_system_prompt_member(member: &SystemPromptMember) -> Result<(), FrameValidationError> {
    if member.is_absent() {
        return Err(FrameValidationError::SystemPromptShape);
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
    /// Frame version is not admitted by this implementation.
    UnsupportedVersion,
    /// A boundary value cannot construct the application input.
    InvalidRequest,
    /// A read target does not exist.
    NotFound,
    /// Every recorded replica was proven absent.
    BlobMissing,
    /// Every usable recorded replica failed content verification.
    BlobCorrupt,
    /// A durable identity already names different intent.
    ConflictingReuse,
    /// Canonical command handling recorded a typed rejection.
    Rejected,
    /// A follower fell behind bounded fan-out.
    ResyncRequired,
    /// Infrastructure prevented completion.
    Unavailable,
    /// A remote store may have accepted a deterministic publication.
    PublicationAmbiguous,
    /// Infrastructure obscured whether a requested mutation committed.
    CommitAmbiguous,
    /// Fail-closed corruption or a hub defect stopped the request.
    Internal,
}

/// Closed connection-local holder of the process-wide bulk-ingest permit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkIngestKind {
    ConversationImport,
    BlobUpload,
}

impl BulkIngestKind {
    /// Returns the exact lowercase wire token for terminal diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConversationImport => "conversation_import",
            Self::BlobUpload => "blob_upload",
        }
    }
}

/// Typed durable submit rejection details.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RejectionDetail {
    /// Another chunked bulk-ingest kind already owns this connection.
    BulkIngestAlreadyInProgress { active_kind: BulkIngestKind },
    /// An explicit reasoning value is unsupported by the selected model.
    UnsupportedReasoningLevel {
        selection_id: CanonicalUuid,
        requested: ReasoningLevel,
    },
    /// Enabled fast mode is unsupported by the selected model.
    UnsupportedFastMode { selection_id: CanonicalUuid },
    /// An explicit service tier is unsupported by the selected model.
    UnsupportedServiceTier {
        selection_id: CanonicalUuid,
        requested: ServiceTier,
    },
    /// The target session did not exist at command handling.
    SessionNotFound {
        /// Absent target.
        session_id: CanonicalUuid,
    },
    /// An attachment digest had no catalogued verified replica.
    AttachmentBlobNotFound {
        /// The unavailable immutable byte identity.
        digest: CanonicalBlobDigest,
    },
    /// Distinct attachment bytes exceeded the deployment admission ceiling.
    AttachmentByteBudgetExceeded {
        /// Configured maximum aggregate byte count.
        maximum_bytes: PositiveCanonicalU64,
    },
    /// The placement head advanced beyond the caller-observed version.
    SessionPlacementCurrentVersionMismatch {
        session_id: CanonicalUuid,
        expected_placement_version: CanonicalU64,
        current_placement_version: CanonicalU64,
    },
    /// The positive placement-version space was exhausted.
    SessionPlacementVersionExhausted {
        session_id: CanonicalUuid,
        current_placement_version: CanonicalU64,
    },
    /// A durable goal command was rejected by current goal state.
    GoalCommandRejected {
        /// Target session.
        session_id: CanonicalUuid,
        /// Closed goal-specific reason.
        reason: GoalCommandRejection,
    },
    /// A turn already held the session slot.
    ActiveTurnPresent {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative active turn.
        active_turn_id: CanonicalUuid,
    },
    /// A commissioned target already has a live session.
    CommissionTargetBusy {
        /// Authoritative live session currently owning the target.
        session_id: CanonicalUuid,
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
    /// A next-safe-point input targeted a turn that is already stopping.
    SafePointUnavailableWhileStopping {
        /// Target session.
        session_id: CanonicalUuid,
        /// Authoritative stopping turn.
        active_turn_id: CanonicalUuid,
        /// Command whose applied result already carries the stop proof.
        existing_command_id: CanonicalUuid,
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
    /// The named tool request carries no delegate denial, so no override is
    /// admitted for it.
    ToolRequestNotDelegateDenied {
        /// Tool request without a delegate denial.
        tool_request_id: CanonicalUuid,
    },
    /// The named delegate denial has not reached its terminal denied result.
    ToolRequestNotTerminallyDenied {
        /// Tool request whose denial is still resolving.
        tool_request_id: CanonicalUuid,
    },
    /// An override is already recorded for the named delegate denial.
    ToolDenialAlreadyOverridden {
        /// Already-overridden tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The named delegation request belongs to another turn.
    DelegationRequestNotInTurn {
        /// Session the caller named.
        session_id: CanonicalUuid,
        /// Turn the caller named.
        turn_id: CanonicalUuid,
        /// Delegation request owned by another turn.
        tool_request_id: CanonicalUuid,
    },
    /// A first execution named a request without executable attempt authority.
    DelegationToolRequestNotExecutable {
        /// Logical delegation tool request.
        tool_request_id: CanonicalUuid,
        /// Exact durable state that prevented first execution.
        state: DelegationToolRequestState,
    },
    /// A spawn request replay changed its immutable arguments.
    DelegationSpawnConflict {
        /// Conflicting logical spawn request.
        tool_request_id: CanonicalUuid,
    },
    /// A generated child identity was already occupied.
    DelegatedChildIdentityCollision {
        /// Colliding child identity.
        child_session_id: CanonicalUuid,
    },
    /// No delegation relationship joined the named session and peer.
    DelegationRelationNotFound {
        /// Invoking session.
        session_id: CanonicalUuid,
        /// Named related peer.
        peer_session_id: CanonicalUuid,
    },
    /// An await request replay changed its immutable arguments.
    DelegationAwaitConflict {
        /// Conflicting logical await request.
        tool_request_id: CanonicalUuid,
    },
    /// A message request replay changed its immutable arguments.
    DelegationMessageConflict {
        /// Conflicting logical message request.
        tool_request_id: CanonicalUuid,
    },
    /// A daemon-minted message identity was already claimed.
    DelegationMessageIdentityCollision {
        /// Colliding message identity.
        message_id: CanonicalUuid,
    },
    /// A relationship cannot allocate another positive event ordinal.
    DelegationEventOrdinalExhausted {
        /// Relationship's spawning request identity.
        spawning_request_id: CanonicalUuid,
        /// Last representable event ordinal.
        last: CanonicalU64,
    },
    /// A recipient cannot allocate another positive delivery sequence.
    DelegationDeliverySequenceExhausted {
        /// Recipient whose delivery sequence is exhausted.
        recipient_session_id: CanonicalUuid,
        /// Last representable delivery sequence.
        last: CanonicalU64,
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
    /// No imported conversation had the named identity.
    ///
    /// The absent target is an imported conversation, never a session: an
    /// imported conversation is durable record and creates no session.
    ImportedConversationNotFound {
        /// Absent imported conversation.
        imported_conversation_id: CanonicalUuid,
    },
    /// The named imported conversation exists but has no such position.
    ///
    /// Imported positions are the one-based contiguous sequence
    /// `1..=last_position`; the identity was valid and only the ordinal was
    /// outside it.
    ImportedFrontierPositionOutOfRange {
        /// Imported conversation whose positions bound the request.
        imported_conversation_id: CanonicalUuid,
        /// Exact position the caller named.
        requested_position: CanonicalU64,
        /// Greatest selectable position on that conversation.
        last_position: CanonicalU64,
    },
    /// This connection already has one in-progress conversation import.
    ConversationImportAlreadyInProgress {},
    /// This connection has no in-progress conversation import.
    ConversationImportNotInProgress {},
    /// The declared or observed source size exceeds the configured total bound.
    ConversationImportSourceTooLarge {
        /// Configured maximum assembled source size.
        limit_bytes: CanonicalU64,
        /// Exact total source size declared at begin.
        declared_size_bytes: CanonicalU64,
        /// Exact observed size at append or commit, or null at begin.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        actual_size_bytes: Option<CanonicalU64>,
    },
    /// The observed source size did not equal the size declared at begin.
    ConversationImportSourceSizeMismatch {
        /// Exact total source size declared at begin.
        declared_size_bytes: CanonicalU64,
        /// Exact number of source bytes observed across append requests.
        actual_size_bytes: CanonicalU64,
    },
    /// A converter rejected the complete source with content-silent evidence.
    ConversationImportConversionFailed {
        /// Closed converter failure class.
        class: ConversationImportRejectionClass,
        /// One-based offending physical record, or null when not applicable.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        record_ordinal: Option<CanonicalU64>,
    },
    /// This connection already has one in-progress blob upload.
    BlobUploadAlreadyInProgress {},
    /// This connection has no in-progress blob upload.
    BlobUploadNotInProgress {},
    /// The declared blob length fell outside the configured inclusive range.
    BlobUploadLengthOutOfRange {
        min_length_bytes: CanonicalU64,
        max_length_bytes: CanonicalU64,
        declared_length_bytes: CanonicalU64,
    },
    /// Appending the chunk would exceed the length declared at begin.
    BlobUploadSizeExceeded {
        expected_length_bytes: CanonicalU64,
        actual_length_bytes: CanonicalU64,
    },
    /// The appended byte count differed from the length declared at begin.
    BlobUploadLengthMismatch {
        expected_length_bytes: CanonicalU64,
        actual_length_bytes: CanonicalU64,
    },
    /// The assembled bytes differed from the digest declared at begin.
    BlobUploadDigestMismatch {
        expected_digest: CanonicalBlobDigest,
        actual_digest: CanonicalBlobDigest,
    },
    /// The requested direct-read length fell outside the inclusive wire bound.
    BlobReadLengthOutOfRange {
        min_length_bytes: CanonicalU64,
        max_length_bytes: CanonicalU64,
        requested_length_bytes: CanonicalU64,
    },
    /// The requested exact half-open range is not contained by the blob.
    BlobReadRangeOutOfBounds {
        offset_bytes: CanonicalU64,
        length_bytes: CanonicalU64,
        blob_length_bytes: CanonicalU64,
    },
    /// A durable session-lifecycle command was rejected by current state.
    SessionLifecycleCommandRejected {
        /// Target session.
        session_id: CanonicalUuid,
        /// Closed reason.
        reason: SessionLifecycleCommandRejection,
    },
}

impl RejectionDetail {
    const fn is_bulk_ingest(self) -> bool {
        matches!(self, Self::BulkIngestAlreadyInProgress { .. })
    }

    const fn is_blob_upload(self) -> bool {
        matches!(
            self,
            Self::BlobUploadAlreadyInProgress {}
                | Self::BlobUploadNotInProgress {}
                | Self::BlobUploadLengthOutOfRange { .. }
                | Self::BlobUploadSizeExceeded { .. }
                | Self::BlobUploadLengthMismatch { .. }
                | Self::BlobUploadDigestMismatch { .. }
        )
    }

    const fn is_blob_read(self) -> bool {
        matches!(
            self,
            Self::BlobReadLengthOutOfRange { .. } | Self::BlobReadRangeOutOfBounds { .. }
        )
    }

    const fn is_conversation_import(self) -> bool {
        match self {
            Self::ConversationImportAlreadyInProgress {}
            | Self::ConversationImportNotInProgress {}
            | Self::ConversationImportSourceTooLarge { .. }
            | Self::ConversationImportSourceSizeMismatch { .. }
            | Self::ConversationImportConversionFailed { .. } => true,
            Self::BlobUploadAlreadyInProgress {}
            | Self::BlobUploadNotInProgress {}
            | Self::BlobUploadLengthOutOfRange { .. }
            | Self::BlobUploadSizeExceeded { .. }
            | Self::BlobUploadLengthMismatch { .. }
            | Self::BlobUploadDigestMismatch { .. }
            | Self::BlobReadLengthOutOfRange { .. }
            | Self::BlobReadRangeOutOfBounds { .. }
            | Self::BulkIngestAlreadyInProgress { .. }
            | Self::SessionNotFound { .. }
            | Self::AttachmentBlobNotFound { .. }
            | Self::AttachmentByteBudgetExceeded { .. }
            | Self::UnsupportedReasoningLevel { .. }
            | Self::UnsupportedFastMode { .. }
            | Self::UnsupportedServiceTier { .. }
            | Self::SessionPlacementCurrentVersionMismatch { .. }
            | Self::SessionPlacementVersionExhausted { .. }
            | Self::GoalCommandRejected { .. }
            | Self::SessionLifecycleCommandRejected { .. }
            | Self::ActiveTurnPresent { .. }
            | Self::CommissionTargetBusy { .. }
            | Self::ActiveTurnMismatch { .. }
            | Self::NoActiveTurn { .. }
            | Self::TurnNotAwaitingReconciliation { .. }
            | Self::InterruptAlreadyApplied { .. }
            | Self::InterruptUnavailableWhileAwaitingApproval { .. }
            | Self::SafePointUnavailableWhileStopping { .. }
            | Self::ToolRequestNotFound { .. }
            | Self::ToolRequestAlreadyResolved { .. }
            | Self::ToolRequestNotEarliestUndecided { .. }
            | Self::ToolRequestNotInSession { .. }
            | Self::ToolRequestNotDelegateDenied { .. }
            | Self::ToolRequestNotTerminallyDenied { .. }
            | Self::ToolDenialAlreadyOverridden { .. }
            | Self::DelegationRequestNotInTurn { .. }
            | Self::DelegationToolRequestNotExecutable { .. }
            | Self::DelegationSpawnConflict { .. }
            | Self::DelegatedChildIdentityCollision { .. }
            | Self::DelegationRelationNotFound { .. }
            | Self::DelegationAwaitConflict { .. }
            | Self::DelegationMessageConflict { .. }
            | Self::DelegationMessageIdentityCollision { .. }
            | Self::DelegationEventOrdinalExhausted { .. }
            | Self::DelegationDeliverySequenceExhausted { .. }
            | Self::DefaultsVersionMismatch { .. }
            | Self::UnknownModelAlias { .. }
            | Self::AcceptancePositionExhausted { .. }
            | Self::DefaultsVersionExhausted { .. }
            | Self::ImportedConversationNotFound { .. }
            | Self::ImportedFrontierPositionOutOfRange { .. } => false,
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

    /// Includes typed import evidence on an invalid request.
    pub const fn invalid_request(detail: RejectionDetail) -> Self {
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

/// Closed terminal model-call failure classifications exposed to clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedModelCallCause {
    /// Distinct rendered attachments exceeded the deployment verification bound.
    AttachmentTooLarge,
    /// No recorded replica contained a required rendered attachment.
    AttachmentMissing,
    /// Recorded replicas failed attachment identity verification.
    AttachmentCorrupt,
    /// The provider rejected the request credential.
    CredentialRejected,
    /// The credential lacked permission.
    PermissionDenied,
    /// The provider judged the request invalid.
    InvalidRequest,
    /// The requested model or resource was not found.
    TargetNotFound,
    /// The request exceeded a provider size limit.
    RequestTooLarge,
    /// The provider applied a transient rate limit.
    RateLimited,
    /// The account's available quota was exhausted.
    QuotaExhausted,
    /// The provider reported overload.
    Overloaded,
    /// The provider reported an internal error.
    ProviderInternal,
    /// The adapter did not recognize the definitive provider error.
    Unrecognized,
}

/// Optional terminal call evidence carried by a failed transcript turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FailedTerminalModelCall {
    model_call_id: CanonicalUuid,
    disposition: FailedModelCallDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<FailedModelCallCause>,
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
            cause: None,
        }
    }

    /// Constructs one known-failed call with its closed failure classification.
    pub const fn known_failed_with_cause(
        model_call_id: CanonicalUuid,
        cause: FailedModelCallCause,
    ) -> Self {
        Self {
            model_call_id,
            disposition: FailedModelCallDisposition::KnownFailed,
            cause: Some(cause),
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

    /// Returns the closed failure classification when retained.
    pub const fn cause(&self) -> Option<FailedModelCallCause> {
        self.cause
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFailedTerminalModelCall {
    model_call_id: CanonicalUuid,
    disposition: FailedModelCallDisposition,
    #[serde(
        default,
        deserialize_with = "deserialize_present_failed_model_call_cause"
    )]
    cause: Option<FailedModelCallCause>,
}

// Field default handles omission; invoking this decoder means the member was
// present, so a JSON null must fail instead of collapsing into `None`.
fn deserialize_present_failed_model_call_cause<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Option<FailedModelCallCause>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    FailedModelCallCause::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for FailedTerminalModelCall {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawFailedTerminalModelCall::deserialize(deserializer)?;
        if raw.cause.is_some() && raw.disposition != FailedModelCallDisposition::KnownFailed {
            return Err(serde::de::Error::custom(
                "failure cause requires a known-failed disposition",
            ));
        }
        Ok(Self {
            model_call_id: raw.model_call_id,
            disposition: raw.disposition,
            cause: raw.cause,
        })
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
        /// Exact ordered accepted user parts.
        content: UserInputContent,
    },
    /// Delegated work has not activated.
    QueuedDelegated {
        /// Tool request that spawned the delegated session.
        spawning_request_id: CanonicalUuid,
        /// Parent session that issued the spawn request.
        parent_session_id: CanonicalUuid,
        /// Parent turn that issued the spawn request.
        parent_turn_id: CanonicalUuid,
        /// Exact delegated task text.
        content: InputContent,
    },
    /// Delivered delegation content is queued to wake an idle recipient.
    QueuedDelegationWake {
        /// First recipient-wide delivery sequence included by the wake.
        first_delivery_sequence: CanonicalU64,
        /// Last recipient-wide delivery sequence included by the wake.
        through_delivery_sequence: CanonicalU64,
    },
    /// A parent command logically terminalized delegated work while retained
    /// physical execution evidence remains inert.
    DelegationTerminated {
        /// Tool request that spawned the child.
        spawning_request_id: CanonicalUuid,
        /// Typed stopped or cancelled outcome.
        outcome: DelegationOutcome,
        /// Exact parent terminal reason.
        reason: DelegationReason,
        /// Exact parent-command provenance.
        provenance: DelegationProvenance,
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
        /// Durable automatic reconciliation attempts already claimed.
        automatic_reconciliation_attempts: CanonicalU64,
        /// True only when the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The turn is parked on a user decision for a tool request.
    ActiveAwaitingToolApproval {
        /// Earliest undecided tool request.
        tool_request_id: CanonicalUuid,
    },
    /// The turn is parked on a foreground delegated-child result.
    ActiveAwaitingChild {
        /// Tool request that issued the await.
        await_request_id: CanonicalUuid,
        /// Spawn request naming the relationship.
        spawning_request_id: CanonicalUuid,
        /// Exact child whose result releases the turn.
        child_session_id: CanonicalUuid,
    },
    /// The turn is parked on an ambiguous tool attempt.
    ActiveAwaitingToolRecovery {
        /// Ended turn attempt that issued the tool effect.
        ended_attempt_id: CanonicalUuid,
        /// Ambiguous tool attempt awaiting recovery.
        recovery_tool_attempt_id: CanonicalUuid,
        /// Durable automatic reconciliation attempts already claimed.
        automatic_reconciliation_attempts: CanonicalU64,
        /// True only when the automatic attempt budget is exhausted.
        operator_action_required: bool,
    },
    /// The turn is parked on replacement of one exact lost runner placement.
    ActiveAwaitingRunnerRecovery {
        /// Runner whose durable loss owns this wait.
        runner_id: CanonicalUuid,
        /// Positive placement revision against which loss was projected.
        placement_revision: PositiveCanonicalU64,
        /// Physical tool attempt interrupted by loss, or null when none exists.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        tool_attempt_id: Option<CanonicalUuid>,
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
        content: UserInputContent,
    },
    QueuedDelegated {
        spawning_request_id: CanonicalUuid,
        parent_session_id: CanonicalUuid,
        parent_turn_id: CanonicalUuid,
        content: InputContent,
    },
    QueuedDelegationWake {
        first_delivery_sequence: CanonicalU64,
        through_delivery_sequence: CanonicalU64,
    },
    DelegationTerminated {
        spawning_request_id: CanonicalUuid,
        outcome: DelegationOutcome,
        reason: DelegationReason,
        provenance: DelegationProvenance,
    },
    ActiveRunning {
        current_attempt_id: CanonicalUuid,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        current_model_call: Option<CurrentModelCall>,
    },
    ActiveAwaitingModelCallRecovery {
        ended_attempt_id: CanonicalUuid,
        recovery_model_call_id: CanonicalUuid,
        automatic_reconciliation_attempts: CanonicalU64,
        operator_action_required: bool,
    },
    ActiveAwaitingToolApproval {
        tool_request_id: CanonicalUuid,
    },
    ActiveAwaitingChild {
        await_request_id: CanonicalUuid,
        spawning_request_id: CanonicalUuid,
        child_session_id: CanonicalUuid,
    },
    ActiveAwaitingToolRecovery {
        ended_attempt_id: CanonicalUuid,
        recovery_tool_attempt_id: CanonicalUuid,
        automatic_reconciliation_attempts: CanonicalU64,
        operator_action_required: bool,
    },
    ActiveAwaitingRunnerRecovery {
        runner_id: CanonicalUuid,
        placement_revision: CanonicalU64,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        tool_attempt_id: Option<CanonicalUuid>,
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
            } => {
                content.validate().map_err(serde::de::Error::custom)?;
                Self::Queued {
                    accepted_input_id,
                    content,
                }
            }
            RawTurnState::QueuedDelegated {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            } => Self::QueuedDelegated {
                spawning_request_id,
                parent_session_id,
                parent_turn_id,
                content,
            },
            RawTurnState::QueuedDelegationWake {
                first_delivery_sequence,
                through_delivery_sequence,
            } => {
                if first_delivery_sequence.value() == 0
                    || first_delivery_sequence > through_delivery_sequence
                {
                    return Err(serde::de::Error::custom(
                        "delegation wake requires a positive ordered delivery range",
                    ));
                }
                Self::QueuedDelegationWake {
                    first_delivery_sequence,
                    through_delivery_sequence,
                }
            }
            RawTurnState::DelegationTerminated {
                spawning_request_id,
                outcome,
                reason,
                provenance,
            } => {
                if !delegation_terminal_outcome_reason_is_admissible(outcome, reason)
                    || !parent_delegation_provenance_has_cascade(&provenance)
                {
                    return Err(serde::de::Error::custom(
                        "delegation terminal requires parent cascade authority",
                    ));
                }
                Self::DelegationTerminated {
                    spawning_request_id,
                    outcome,
                    reason,
                    provenance,
                }
            }
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
                automatic_reconciliation_attempts,
                operator_action_required,
            } => Self::ActiveAwaitingModelCallRecovery {
                ended_attempt_id,
                recovery_model_call_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            },
            RawTurnState::ActiveAwaitingToolApproval { tool_request_id } => {
                Self::ActiveAwaitingToolApproval { tool_request_id }
            }
            RawTurnState::ActiveAwaitingChild {
                await_request_id,
                spawning_request_id,
                child_session_id,
            } => Self::ActiveAwaitingChild {
                await_request_id,
                spawning_request_id,
                child_session_id,
            },
            RawTurnState::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            } => Self::ActiveAwaitingToolRecovery {
                ended_attempt_id,
                recovery_tool_attempt_id,
                automatic_reconciliation_attempts,
                operator_action_required,
            },
            RawTurnState::ActiveAwaitingRunnerRecovery {
                runner_id,
                placement_revision,
                tool_attempt_id,
            } => Self::ActiveAwaitingRunnerRecovery {
                runner_id,
                placement_revision: PositiveCanonicalU64::try_new(placement_revision.value())
                    .map_err(|_| {
                        serde::de::Error::custom(
                            "runner recovery requires a positive placement revision",
                        )
                    })?,
                tool_attempt_id,
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
    fn validate(&self) -> Result<(), FrameValidationError> {
        if let Self::QueuedDelegationWake {
            first_delivery_sequence,
            through_delivery_sequence,
        } = self
            && (first_delivery_sequence.value() == 0
                || first_delivery_sequence > through_delivery_sequence)
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        if let Self::Failed {
            terminal_attempt_id: None,
            terminal_model_call: Some(_),
            ..
        } = self
        {
            return Err(FrameValidationError::TurnStateShape);
        }
        if let Self::DelegationTerminated {
            outcome,
            reason,
            provenance,
            ..
        } = self
            && (!delegation_terminal_outcome_reason_is_admissible(*outcome, *reason)
                || !parent_delegation_provenance_has_cascade(provenance))
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

/// Closed discriminator naming one imported entry's normalized content
/// variant.
///
/// The transcript snapshot reaches the `Text` arm only for absent or
/// unattested text, because attested text takes the separate text-entry
/// message there. An imported-conversation inspection row has no such split
/// and uses `Text` for every `Text` content, carrying attestation in its
/// preview member instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedContentKind {
    /// One source-defined event.
    SourceEvent,
    /// One source-defined message block.
    SourceMessageBlock,
    /// Imported text content.
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

/// A leading excerpt of one imported entry's exact attested text.
///
/// The preview is the entry's exact leading Unicode scalar sequence cut at a
/// scalar boundary, never a summary, replacement, or re-encoding. It is a
/// recognition aid for choosing a position; the immutable imported aggregate
/// remains the authority for complete content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawImportedTextPreview")]
pub struct ImportedTextPreview {
    /// Exact leading scalars within structural wire-text memory.
    preview: String,
    /// Whether exact text remains beyond the emitted scalars.
    truncated: bool,
}

/// The undecoded wire shape of a preview, before its bound and truncation
/// marker are checked.
///
/// Deserializing through this raw shape keeps the checked type unconstructible
/// from an invalid frame, so a direct `ImportedTextPreview` deserialization
/// cannot bypass the validation an embedded one performs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImportedTextPreview {
    preview: String,
    truncated: bool,
}

impl TryFrom<RawImportedTextPreview> for ImportedTextPreview {
    type Error = FrameValidationError;

    fn try_from(raw: RawImportedTextPreview) -> Result<Self, Self::Error> {
        let preview = Self {
            preview: raw.preview,
            truncated: raw.truncated,
        };
        preview.validate()?;
        Ok(preview)
    }
}

impl ImportedTextPreview {
    /// Constructs a structurally bounded preview of one exact attested text.
    ///
    /// The cut lands on a Unicode scalar boundary, so the preview is always a
    /// prefix of the source text rather than a truncated encoding.
    pub fn of_exact_text(text: &str) -> Self {
        Self::of_exact_text_with_limit(text, None)
    }

    /// Constructs a preview under the deployment's optional retained-detail policy.
    pub fn of_exact_text_with_limit(text: &str, limit: Option<usize>) -> Self {
        let effective_limit = limit
            .unwrap_or(MAX_CONTENT_FRAGMENT_BYTES)
            .min(MAX_CONTENT_FRAGMENT_BYTES);
        let mut end = effective_limit.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            preview: text[..end].to_owned(),
            truncated: end < text.len(),
        }
    }

    /// Returns the exact emitted leading scalars.
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Returns whether exact text remains beyond the emitted scalars.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if self.preview.len() > MAX_CONTENT_FRAGMENT_BYTES {
            return Err(FrameValidationError::ImportedTextPreviewShape);
        }
        // Every nonempty text yields at least one scalar inside the bound, so
        // an empty preview cannot be the cut prefix of a longer text.
        if self.truncated && self.preview.is_empty() {
            return Err(FrameValidationError::ImportedTextPreviewShape);
        }
        Ok(())
    }
}

/// Non-text semantic transcript entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptEntry {
    /// Exact delegated task that opened one child session.
    DelegatedTask {
        /// Tool request that spawned the child.
        spawning_request_id: CanonicalUuid,
        /// Parent session that issued the spawn request.
        parent_session_id: CanonicalUuid,
        /// Parent turn that issued the spawn request.
        parent_turn_id: CanonicalUuid,
        /// Exact delegated task text.
        content: String,
    },
    /// Exact bidirectional delegation message delivered to this frontier.
    DelegationMessage {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Immutable message identity.
        message_id: CanonicalUuid,
        /// Sending session.
        sender_session_id: CanonicalUuid,
        /// Receiving session.
        recipient_session_id: CanonicalUuid,
        /// Relationship-local message ordinal.
        ordinal: CanonicalU64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
        /// Exact delivered content.
        content: String,
    },
    /// Exact child result delivered through one registered wait.
    DelegationResult {
        /// Await request receiving this result.
        await_request_id: CanonicalUuid,
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Terminal child session.
        child_session_id: CanonicalUuid,
        /// Foreground or background delivery mode.
        mode: DelegationWaitMode,
        /// Recipient-wide position for background delivery only.
        delivery_sequence: Option<CanonicalU64>,
        /// Typed terminal result outcome.
        outcome: DelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed lifecycle reason.
        reason: DelegationReason,
        /// Exact child-turn or parent-command proof.
        provenance: DelegationProvenance,
    },
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
        /// Explicit decision provenance, absent while pending and for automatic policy.
        #[serde(
            default,
            deserialize_with = "deserialize_optional_non_null",
            skip_serializing_if = "Option::is_none"
        )]
        approval: Option<TranscriptToolApproval>,
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

/// Metadata for a text-bearing semantic transcript entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptTextEntry {
    /// Committed assistant text.
    Assistant {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Producing model call.
        model_call_id: CanonicalUuid,
    },
    /// Model-produced summary of one exact earlier semantic range.
    ContextSummary {
        /// Dedicated model call that produced the summary.
        model_call_id: CanonicalUuid,
        /// Source session of the inclusive range's first entry.
        first_source_session_id: CanonicalUuid,
        /// Identity of the inclusive range's first entry.
        first_entry_id: CanonicalUuid,
        /// Source session of the inclusive range's final entry.
        through_source_session_id: CanonicalUuid,
        /// Identity of the inclusive range's final entry.
        through_entry_id: CanonicalUuid,
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
    /// One ambiguous physical attempt requires user recovery.
    RecoveryRequired {
        /// Exact ambiguous tool attempt.
        tool_attempt_id: CanonicalUuid,
    },
}

/// Sandbox profile selected by one runner placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RunnerSandboxProfile {
    /// Supervised execution with the invoking user's ambient filesystem and network access.
    #[serde(rename = "ambient")]
    Ambient,
    /// Execution restricted to the placement-owned writable root.
    #[serde(rename = "workspace-restricted")]
    WorkspaceRestricted,
}

/// Checked runner capability-class name carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerCapabilityClass(String);

impl RunnerCapabilityClass {
    /// Applies the runner domain's portable catalog-name validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainRunnerCapabilityClass::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }

    /// Borrows the validated capability-class name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunnerCapabilityClass {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerCapabilityClass> for String {
    fn from(value: RunnerCapabilityClass) -> Self {
        value.0
    }
}

/// Checked runner credential-profile name carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerCredentialProfileName(String);

impl RunnerCredentialProfileName {
    /// Applies the runner domain's portable catalog-name validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainCredentialProfileName::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }

    /// Borrows the validated credential-profile name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunnerCredentialProfileName {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerCredentialProfileName> for String {
    fn from(value: RunnerCredentialProfileName) -> Self {
        value.0
    }
}

/// Checked runner repository key carried by a session projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerRepositoryKey(String);

impl RunnerRepositoryKey {
    /// Applies the runner domain's portable repository-key validation.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainWorkspaceRepositoryKey::try_new(value.clone())
            .map(|_| Self(value))
            .map_err(|_| CanonicalValueError::RunnerCatalogName)
    }

    /// Borrows the validated repository key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunnerRepositoryKey {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerRepositoryKey> for String {
    fn from(value: RunnerRepositoryKey) -> Self {
        value.0
    }
}

/// Complete selector carried by an authoritative runner projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerProjectionSelector {
    /// Selects one exact runner identity.
    Runner { runner_id: CanonicalUuid },
    /// Selects a runner advertising one exact capability class.
    CapabilityClass { name: RunnerCapabilityClass },
}

/// Closed current connection health carried for a pinned runner.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerConnectionHealth {
    /// The runner connection is currently healthy.
    Connected,
    /// The connection missed a heartbeat and remains within its recovery window.
    Suspect,
    /// The connection closed through an orderly daemon or runner shutdown.
    Shutdown,
    /// The connection reached a terminal loss transition.
    Lost,
}

/// Closed current state carried by an authoritative runner projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerProjectionState {
    /// No runner has been pinned yet.
    Unpinned,
    /// The current placement is pinned.
    Pinned,
    /// The exact selected runner was lost before pinning.
    RunnerLostBeforePin,
    /// The pinned runner was lost.
    RunnerLost,
    /// The lost placement was explicitly abandoned.
    RunnerAbandoned,
}

/// Authoritative current runner placement projected in a transcript snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawRunnerProjection")]
pub struct RunnerProjection {
    /// Immutable selector requested by this placement revision.
    selector: RunnerProjectionSelector,
    /// Current or lost exact runner when the state names one.
    runner_id: Option<CanonicalUuid>,
    /// Positive current placement revision.
    placement_revision: RunnerPlacementRevision,
    /// Explicit sandbox profile selected by the placement.
    sandbox_profile: RunnerSandboxProfile,
    /// Independently nullable requested credential profile.
    credential_profile: Option<RunnerCredentialProfileName>,
    /// Independently nullable requested repository key.
    repository: Option<RunnerRepositoryKey>,
    /// Independently nullable exact requested working directory.
    working_directory: Option<RunnerWorkingDirectory>,
    /// Current connection health, present exactly while the placement is pinned.
    connection_health: Option<RunnerConnectionHealth>,
    /// Exact current placement state.
    state: RunnerProjectionState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRunnerProjection {
    selector: RunnerProjectionSelector,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    runner_id: Option<CanonicalUuid>,
    placement_revision: RunnerPlacementRevision,
    sandbox_profile: RunnerSandboxProfile,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    credential_profile: Option<RunnerCredentialProfileName>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    repository: Option<RunnerRepositoryKey>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    working_directory: Option<RunnerWorkingDirectory>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    connection_health: Option<RunnerConnectionHealth>,
    state: RunnerProjectionState,
}

impl RunnerProjection {
    /// Constructs one complete internally coherent current placement projection.
    #[expect(
        clippy::too_many_arguments,
        reason = "the constructor names every independent session-composition axis"
    )]
    pub fn try_new(
        selector: RunnerProjectionSelector,
        runner_id: Option<CanonicalUuid>,
        placement_revision: RunnerPlacementRevision,
        sandbox_profile: RunnerSandboxProfile,
        credential_profile: Option<RunnerCredentialProfileName>,
        repository: Option<RunnerRepositoryKey>,
        working_directory: Option<RunnerWorkingDirectory>,
        connection_health: Option<RunnerConnectionHealth>,
        state: RunnerProjectionState,
    ) -> Result<Self, CanonicalValueError> {
        let runner_shape_valid =
            matches!(state, RunnerProjectionState::Unpinned) == runner_id.is_none();
        let selector_valid = match (&selector, runner_id, state) {
            (
                RunnerProjectionSelector::Runner {
                    runner_id: selected,
                },
                Some(current),
                _,
            ) => *selected == current,
            (RunnerProjectionSelector::Runner { .. }, None, RunnerProjectionState::Unpinned)
            | (
                RunnerProjectionSelector::CapabilityClass { .. },
                _,
                RunnerProjectionState::Unpinned
                | RunnerProjectionState::Pinned
                | RunnerProjectionState::RunnerLost
                | RunnerProjectionState::RunnerAbandoned,
            ) => true,
            (RunnerProjectionSelector::Runner { .. }, None, _)
            | (
                RunnerProjectionSelector::CapabilityClass { .. },
                _,
                RunnerProjectionState::RunnerLostBeforePin,
            ) => false,
        };
        let connection_shape_valid =
            matches!(state, RunnerProjectionState::Pinned) == connection_health.is_some();
        if !runner_shape_valid || !selector_valid || !connection_shape_valid {
            return Err(CanonicalValueError::RunnerProjection);
        }
        Ok(Self {
            selector,
            runner_id,
            placement_revision,
            sandbox_profile,
            credential_profile,
            repository,
            working_directory,
            connection_health,
            state,
        })
    }

    /// Borrows the immutable requested selector.
    pub const fn selector(&self) -> &RunnerProjectionSelector {
        &self.selector
    }

    /// Returns the current or lost exact runner when the state names one.
    pub const fn runner_id(&self) -> Option<CanonicalUuid> {
        self.runner_id
    }

    /// Returns the positive current placement revision.
    pub const fn placement_revision(&self) -> RunnerPlacementRevision {
        self.placement_revision
    }

    /// Returns the explicitly selected sandbox profile.
    pub const fn sandbox_profile(&self) -> RunnerSandboxProfile {
        self.sandbox_profile
    }

    /// Borrows the independently nullable requested credential profile.
    pub const fn credential_profile(&self) -> Option<&RunnerCredentialProfileName> {
        self.credential_profile.as_ref()
    }

    /// Borrows the independently nullable requested repository key.
    pub const fn repository(&self) -> Option<&RunnerRepositoryKey> {
        self.repository.as_ref()
    }

    /// Borrows the independently nullable exact requested working directory.
    pub const fn working_directory(&self) -> Option<&RunnerWorkingDirectory> {
        self.working_directory.as_ref()
    }

    /// Returns current connection health exactly while the placement is pinned.
    pub const fn connection_health(&self) -> Option<RunnerConnectionHealth> {
        self.connection_health
    }

    /// Returns the exact current placement state.
    pub const fn state(&self) -> RunnerProjectionState {
        self.state
    }
}

impl TryFrom<RawRunnerProjection> for RunnerProjection {
    type Error = CanonicalValueError;

    fn try_from(raw: RawRunnerProjection) -> Result<Self, Self::Error> {
        Self::try_new(
            raw.selector,
            raw.runner_id,
            raw.placement_revision,
            raw.sandbox_profile,
            raw.credential_profile,
            raw.repository,
            raw.working_directory,
            raw.connection_health,
            raw.state,
        )
    }
}

/// Exact bounded runner working-directory text carried on the process wire.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunnerWorkingDirectory(String);

impl RunnerWorkingDirectory {
    /// Maximum UTF-8 bytes admitted by the runner domain and process wire.
    // numeric-bound: guard - mirrors the domain's exact runner-value wire grammar
    pub const MAX_UTF8_BYTES: usize = DomainRunnerWorkingDirectory::MAX_BYTES;

    /// Admits nonempty, NUL-free text within the exact byte bound.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        DomainRunnerWorkingDirectory::try_new(value.clone())
            .map_err(|_| CanonicalValueError::RunnerWorkingDirectory)?;
        Ok(Self(value))
    }

    /// Borrows the exact validated directory text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunnerWorkingDirectory {
    type Error = CanonicalValueError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RunnerWorkingDirectory> for String {
    fn from(value: RunnerWorkingDirectory) -> Self {
        value.0
    }
}

/// Positive runner placement revision carried by follower-visible wire facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RunnerPlacementRevision(CanonicalU64);

impl RunnerPlacementRevision {
    /// Admits one positive placement revision.
    pub const fn try_new(value: u64) -> Option<Self> {
        if value == 0 {
            None
        } else {
            Some(Self(CanonicalU64::new(value)))
        }
    }

    /// Returns the positive integer carried by this placement revision.
    pub const fn value(self) -> u64 {
        self.0.value()
    }
}

impl<'de> Deserialize<'de> for RunnerPlacementRevision {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let value = CanonicalU64::deserialize(deserializer)?;
        Self::try_new(value.value())
            .ok_or_else(|| serde::de::Error::custom("runner placement revision must be positive"))
    }
}

/// Closed runner state carried by one session update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerStateTransitionState {
    /// Initial dispatch pinned the selected runner.
    Pinned,
    /// The current runner connection missed its first heartbeat.
    Suspect,
    /// A heartbeat acknowledgement recovered that same suspect connection.
    Connected,
    /// An exact runner selection was lost before initial pinning.
    RunnerLostBeforePin,
    /// A pinned runner became unavailable.
    RunnerLost,
    /// A checked successor runner replaced the prior placement.
    Replaced,
    /// Checked recovery retained the runner but changed the selected directory.
    WorkingDirectoryChanged,
    /// The user abandoned a lost runner placement.
    Abandoned,
}

/// Action chosen for one bound child when its parent reaches a terminal state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundChildAction {
    /// Leave the child running.
    KeepRunning,
    /// Stop the child with typed parent-policy provenance.
    Stop,
    /// Cancel the child with typed parent-policy provenance.
    Cancel,
}

/// Parent-chosen lifecycle policy carried by a child-spawned update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationPolicy {
    /// The child keeps working independently of parent state.
    Background {},
    /// The child follows the two explicit parent-state actions.
    Bound {
        /// Action when the parent stops.
        on_parent_stopped: BoundChildAction,
        /// Action when the parent is cancelled.
        on_parent_cancelled: BoundChildAction,
    },
}

/// Delivery behavior chosen by one await request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationWaitMode {
    /// Keep the current parent turn open until delivery.
    Foreground,
    /// Return registration and deliver through a later wake.
    Background,
}

/// Direction of one message within its parent-child relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMessageDirection {
    /// The relationship parent sent to its child.
    ParentToChild,
    /// The relationship child sent to its parent.
    ChildToParent,
}

/// Durable non-executable state of one delegation tool request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationToolRequestState {
    /// The request still requires an approval decision.
    AwaitingApproval,
    /// Approval was denied.
    Denied,
    /// Approval succeeded, but proposal-ordered execution has not prepared an attempt.
    Approved,
    /// A physical attempt exists but has not been authorized for execution.
    Prepared,
    /// The logical request already closed without executable work.
    Closed,
    /// Its current physical attempt already ended.
    AttemptEnded,
}

impl DelegationToolRequestState {
    /// Returns the stable wire spelling used by diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingApproval => "awaiting_approval",
            Self::Denied => "denied",
            Self::Approved => "approved",
            Self::Prepared => "prepared",
            Self::Closed => "closed",
            Self::AttemptEnded => "attempt_ended",
        }
    }
}

/// Closed relationship outcome carried by delegation updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationOutcome {
    /// Child content is available.
    Returned,
    /// Child execution failed or returned unusable content.
    Failed,
    /// Parent policy stopped the child.
    Stopped,
    /// Child or parent policy cancelled the child.
    Cancelled,
    /// Relationship policy left the child running.
    ContinueRunning,
    /// Parent policy reached an already-terminal child.
    AlreadyTerminal,
}

/// Exact reason carried alongside a delegation outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationReason {
    /// Child completed with delivered content.
    ChildCompleted,
    /// Child execution failed.
    ChildExecutionFailed,
    /// Completed child content could not form a result.
    ChildResultUnavailable,
    /// Child cancelled independently.
    ChildCancelled,
    /// A parent stop selected descendants.
    ParentStopped,
    /// A parent cancellation selected descendants.
    ParentCancelled,
}

/// Proof source retained by one lifecycle or result update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationProvenance {
    /// Exact terminal child turn.
    ChildTurn {
        /// Child session.
        child_session_id: CanonicalUuid,
        /// Terminal delegated turn.
        child_turn_id: CanonicalUuid,
    },
    /// Exact parent turn command.
    ParentTurnCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// Parent turn named by the command.
        parent_turn_id: CanonicalUuid,
        /// Durable stop or interrupt command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
    /// Exact parent goal-generation command.
    ParentGoalCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// One-based goal generation.
        goal_generation: CanonicalU64,
        /// Durable goal stop command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
    /// Exact parent lifecycle command.
    ParentLifecycleCommand {
        /// Parent session.
        parent_session_id: CanonicalUuid,
        /// Durable lifecycle stop command.
        command_id: CanonicalUuid,
        /// Explicit kill-time descendant choice.
        descendant_scope: DescendantTerminationScope,
    },
}

/// Exact decision recorded for one explicit tool approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolApprovalEventDecision {
    /// Execution is permitted subject to current aggregate guards.
    Approve {},
    /// Execution is permanently prohibited for this request.
    Deny {
        /// Exact user explanation, absent for a delegate denial.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        reason: Option<String>,
    },
}

/// Exact actor provenance for one explicit tool approval decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolApprovalEventDecider {
    /// The user acted through the named durable command.
    User {
        /// Exact durable command provenance.
        command_id: CanonicalUuid,
    },
    /// A configured model acted through the named dedicated judge call.
    Delegate {
        /// Exact direct model selection used by the judge.
        model_selection_id: CanonicalUuid,
        /// Exact recorded judge model call.
        model_call_id: CanonicalUuid,
    },
    /// The user pre-approved the re-proposed command by overriding one exact
    /// delegate denial through the named durable command.
    UserOverride {
        /// Exact durable override-command provenance.
        command_id: CanonicalUuid,
        /// The delegate-denied request whose recorded override was consumed.
        overridden_tool_request_id: CanonicalUuid,
    },
}

/// One explicit approval decision retained in an authoritative transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptToolApproval {
    /// Exact recorded decision.
    pub decision: ToolApprovalEventDecision,
    /// Exact user or delegate provenance.
    pub decider: ToolApprovalEventDecider,
    /// Exact judge rationale, absent for a user decision.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub rationale: Option<String>,
}

/// Closed durable update event family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEvent {
    /// Session creation committed.
    SessionCreated {},
    /// One defaults replacement changed model selection or settings.
    SessionModelSettingsChanged {
        command_id: CommandId,
        prior_defaults_version: CanonicalU64,
        installed_defaults_version: CanonicalU64,
        prior_model: ModelSelection,
        installed_model: ModelSelection,
        prior_settings: ModelSettingsSnapshot,
        installed_settings: ModelSettingsSnapshot,
        caller_override: ModelSettingsOverlay,
        adjustments: Vec<ModelChangeAdjustment>,
    },
    /// One accepted origin turn froze complete model settings.
    TurnModelSettingsResolved {
        accepted_input_id: CanonicalUuid,
        turn_id: CanonicalUuid,
        defaults_version: CanonicalU64,
        requested_model: ModelSelection,
        selected_direct_id: CanonicalUuid,
        per_call_override: ModelSettingsOverlay,
        settings: ModelSettingsSnapshot,
        adjusted_from_selection_id: Option<CanonicalUuid>,
        adjustments: Vec<ModelChangeAdjustment>,
    },
    /// User input acceptance and its queued turn committed.
    InputAccepted {
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Queued origin turn.
        turn_id: CanonicalUuid,
        /// Immutable session acceptance position.
        acceptance_position: CanonicalU64,
        /// Exact ordered accepted user parts.
        content: UserInputContent,
    },
    /// A queued goal turn became intentionally ineligible.
    GoalTurnRetired {
        /// Exact immutable queued turn retired by a goal transition.
        turn_id: CanonicalUuid,
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
    /// A runner placement or its exact connection changed follower-visible state.
    RunnerStateTransition {
        /// Exact runner named by the transition.
        runner_id: CanonicalUuid,
        /// Positive placement revision whose immutable facts are projected.
        placement_revision: RunnerPlacementRevision,
        /// Placement-selected sandbox profile.
        sandbox_profile: RunnerSandboxProfile,
        /// Caller-selected directory, null when the runner default was selected.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        working_directory: Option<RunnerWorkingDirectory>,
        /// Exact closed transition state.
        state: RunnerStateTransitionState,
    },
    /// One explicit tool approval decision committed with full provenance.
    ToolApprovalDecided {
        /// Owning turn.
        turn_id: CanonicalUuid,
        /// Exact logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact recorded decision.
        decision: ToolApprovalEventDecision,
        /// Exact user or delegate decider.
        decider: ToolApprovalEventDecider,
        /// Exact judge rationale, absent for a user decision.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        rationale: Option<String>,
    },
    /// One append-only context compaction committed.
    ContextCompacted {
        /// Exact compaction provenance record.
        context_compaction_id: CanonicalUuid,
        /// Dedicated producing model call.
        model_call_id: CanonicalUuid,
        /// One-based final summarized position.
        through_position: CanonicalU64,
        /// Appended semantic summary entry.
        summary_entry_id: CanonicalUuid,
        /// Complete result frontier.
        result_frontier_id: CanonicalUuid,
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
    /// A parent committed one child relationship and lifecycle policy.
    ChildSpawned {
        /// Exact spawning tool request and relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Spawned child session.
        child_session_id: CanonicalUuid,
        /// Parent-chosen relationship lifecycle policy.
        relationship: DelegationPolicy,
    },
    /// A parent registered one foreground or background wait.
    ChildWaiting {
        /// Exact await tool request.
        await_request_id: CanonicalUuid,
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Child being awaited.
        child_session_id: CanonicalUuid,
        /// Wait delivery mode.
        mode: DelegationWaitMode,
    },
    /// One bidirectional relationship message became durable for its recipient.
    SessionMessage {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Message identity.
        message_id: CanonicalUuid,
        /// Sending session.
        sender_session_id: CanonicalUuid,
        /// Receiving session.
        recipient_session_id: CanonicalUuid,
        /// Relationship-local message ordinal.
        ordinal: CanonicalU64,
        /// Recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
        /// Exact delivered content.
        content: String,
    },
    /// A terminal child result became durable for its parent.
    ChildResult {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Terminal child.
        child_session_id: CanonicalUuid,
        /// Typed terminal result outcome.
        outcome: DelegationOutcome,
        /// Delivered content for a successful result only.
        content: Option<String>,
        /// Typed reason for the terminal result.
        reason: DelegationReason,
        /// Exact child-turn or parent-command provenance.
        provenance: DelegationProvenance,
    },
    /// Parent termination evaluated one relationship edge.
    ChildLifecycleDisposition {
        /// Relationship identity.
        spawning_request_id: CanonicalUuid,
        /// Evaluated child.
        child_session_id: CanonicalUuid,
        /// Typed relationship outcome.
        outcome: DelegationOutcome,
        /// Typed reason for evaluating this relationship edge.
        reason: DelegationReason,
        /// Exact parent command provenance.
        provenance: DelegationProvenance,
    },
}

fn validate_delegation_session_event(
    session_id: CanonicalUuid,
    event: &SessionEvent,
) -> Result<(), FrameValidationError> {
    let valid = match event {
        SessionEvent::ChildSpawned {
            child_session_id, ..
        }
        | SessionEvent::ChildWaiting {
            child_session_id, ..
        } => *child_session_id != session_id,
        SessionEvent::SessionMessage {
            sender_session_id,
            recipient_session_id,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => {
            *recipient_session_id == session_id
                && sender_session_id != recipient_session_id
                && ordinal.value() > 0
                && delivery_sequence.value() > 0
                && delegation_content_is_valid(content)
        }
        SessionEvent::ChildResult {
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => {
            *child_session_id != session_id
                && child_result_shape_is_valid(
                    session_id,
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                )
        }
        SessionEvent::ChildLifecycleDisposition {
            child_session_id,
            outcome,
            reason,
            provenance,
            ..
        } => {
            matches!(
                reason,
                DelegationReason::ParentStopped | DelegationReason::ParentCancelled
            ) && if *child_session_id == session_id {
                // A descendant cascade also addresses the terminalization to
                // the child itself so that live child followers observe it.
                // That row carries the parent's cascade provenance, so the
                // provenance parent is a different session than this header.
                matches!(
                    outcome,
                    DelegationOutcome::Stopped | DelegationOutcome::Cancelled
                ) && delegation_provenance_parent(provenance)
                    .is_some_and(|parent| parent != session_id)
                    && parent_delegation_provenance_has_cascade(provenance)
            } else {
                matches!(
                    outcome,
                    DelegationOutcome::Stopped
                        | DelegationOutcome::Cancelled
                        | DelegationOutcome::AlreadyTerminal
                        | DelegationOutcome::ContinueRunning
                ) && parent_delegation_provenance_is_cascade(session_id, provenance)
            }
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::DelegationShape)
    }
}

fn child_result_shape_is_valid(
    parent_session_id: CanonicalUuid,
    child_session_id: CanonicalUuid,
    outcome: DelegationOutcome,
    content: &Option<String>,
    reason: DelegationReason,
    provenance: &DelegationProvenance,
) -> bool {
    match (outcome, reason, provenance, content) {
        (
            DelegationOutcome::Returned,
            DelegationReason::ChildCompleted,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            Some(content),
        ) => *provenance_child == child_session_id && delegation_content_is_valid(content),
        (
            DelegationOutcome::Failed,
            DelegationReason::ChildExecutionFailed | DelegationReason::ChildResultUnavailable,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            None,
        )
        | (
            DelegationOutcome::Cancelled,
            DelegationReason::ChildCancelled,
            DelegationProvenance::ChildTurn {
                child_session_id: provenance_child,
                ..
            },
            None,
        ) => *provenance_child == child_session_id,
        (
            DelegationOutcome::Stopped | DelegationOutcome::Cancelled,
            DelegationReason::ParentStopped | DelegationReason::ParentCancelled,
            provenance,
            None,
        ) => parent_delegation_provenance_is_cascade(parent_session_id, provenance),
        _ => false,
    }
}

fn direct_child_result_shape_is_valid(
    child_session_id: CanonicalUuid,
    outcome: DelegationOutcome,
    content: &Option<String>,
    reason: DelegationReason,
    provenance: &DelegationProvenance,
) -> bool {
    match provenance {
        DelegationProvenance::ChildTurn { .. } => child_result_shape_is_valid(
            child_session_id,
            child_session_id,
            outcome,
            content,
            reason,
            provenance,
        ),
        DelegationProvenance::ParentTurnCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentGoalCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentLifecycleCommand {
            parent_session_id, ..
        } => {
            *parent_session_id != child_session_id
                && child_result_shape_is_valid(
                    *parent_session_id,
                    child_session_id,
                    outcome,
                    content,
                    reason,
                    provenance,
                )
        }
    }
}

fn parent_delegation_provenance_is_cascade(
    parent_session_id: CanonicalUuid,
    provenance: &DelegationProvenance,
) -> bool {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            parent_session_id: provenance_parent,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && parent_delegation_provenance_has_cascade(provenance)
        }
        DelegationProvenance::ParentGoalCommand {
            parent_session_id: provenance_parent,
            goal_generation,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && goal_generation.value() > 0
                && parent_delegation_provenance_has_cascade(provenance)
        }
        DelegationProvenance::ParentLifecycleCommand {
            parent_session_id: provenance_parent,
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => {
            *provenance_parent == parent_session_id
                && parent_delegation_provenance_has_cascade(provenance)
        }
        _ => false,
    }
}

/// Reads the commanding parent session out of a cascade provenance.
fn delegation_provenance_parent(provenance: &DelegationProvenance) -> Option<CanonicalUuid> {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentGoalCommand {
            parent_session_id, ..
        }
        | DelegationProvenance::ParentLifecycleCommand {
            parent_session_id, ..
        } => Some(*parent_session_id),
        _ => None,
    }
}

fn parent_delegation_provenance_has_cascade(provenance: &DelegationProvenance) -> bool {
    match provenance {
        DelegationProvenance::ParentTurnCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => true,
        DelegationProvenance::ParentGoalCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            goal_generation,
            ..
        } => goal_generation.value() > 0,
        DelegationProvenance::ParentLifecycleCommand {
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            ..
        } => true,
        _ => false,
    }
}

/// Admits every terminal outcome a parent cascade can impose on a child.
///
/// A bound relationship carries its own termination policy, so the child
/// outcome is not required to match the parent reason: a parent cancellation
/// may map to a child `stop`, and a parent stop may map to a child `cancel`.
/// All four crossed pairs are therefore valid, exactly as `process_read`
/// projects them.
fn delegation_terminal_outcome_reason_is_admissible(
    outcome: DelegationOutcome,
    reason: DelegationReason,
) -> bool {
    matches!(
        outcome,
        DelegationOutcome::Stopped | DelegationOutcome::Cancelled
    ) && matches!(
        reason,
        DelegationReason::ParentStopped | DelegationReason::ParentCancelled
    )
}

fn delegation_content_is_valid(content: &str) -> bool {
    !content.is_empty() && content.len() <= MAX_CONTENT_FRAGMENT_BYTES && !content.contains('\0')
}

fn validate_delegation_transcript_entry(
    source_session_id: CanonicalUuid,
    entry: &TranscriptEntry,
) -> Result<(), FrameValidationError> {
    let valid = match entry {
        TranscriptEntry::DelegatedTask {
            parent_session_id,
            content,
            ..
        } => *parent_session_id != source_session_id && delegation_content_is_valid(content),
        TranscriptEntry::DelegationMessage {
            sender_session_id,
            recipient_session_id,
            ordinal,
            delivery_sequence,
            content,
            ..
        } => {
            *recipient_session_id == source_session_id
                && *sender_session_id != *recipient_session_id
                && ordinal.value() > 0
                && delivery_sequence.value() > 0
                && delegation_content_is_valid(content)
        }
        TranscriptEntry::DelegationResult {
            child_session_id,
            mode,
            delivery_sequence,
            outcome,
            content,
            reason,
            provenance,
            ..
        } => {
            *child_session_id != source_session_id
                && match mode {
                    DelegationWaitMode::Foreground => delivery_sequence.is_none(),
                    DelegationWaitMode::Background => {
                        delivery_sequence.is_some_and(|sequence| sequence.value() > 0)
                    }
                }
                && child_result_shape_is_valid(
                    source_session_id,
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                )
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::DelegationShape)
    }
}

fn validate_turn_settings_payload(
    defaults_version: CanonicalU64,
    requested_model: &ModelSelection,
    selected_direct_id: CanonicalUuid,
    per_call_override: ModelSettingsOverlay,
    settings: &ModelSettingsSnapshot,
    adjusted_from_selection_id: Option<CanonicalUuid>,
    adjustments: &[ModelChangeAdjustment],
) -> Result<(), FrameValidationError> {
    settings.validate()?;
    validate_adjustments(adjustments)?;
    let direct_selection_mismatch = matches!(
        requested_model,
        ModelSelection::Direct { selection_id } if *selection_id != selected_direct_id
    );
    let validation_mismatch = match settings.validated_for_selection_id {
        Some(selection_id) => selection_id != selected_direct_id,
        None => !settings.is_model_independent_provider_defaults(),
    };
    let adjustment_provenance_mismatch = unapply_wire_adjustments(settings, adjustments)
        .and_then(|unadjusted| apply_wire_adjustments(unadjusted, adjustments))
        != Some(settings.precedence);
    if defaults_version.value() == 0
        || direct_selection_mismatch
        || validation_mismatch
        || settings.precedence.per_call != per_call_override
        || match adjustments.is_empty() {
            true => adjusted_from_selection_id.is_some(),
            false => adjusted_from_selection_id.is_none_or(|prior| prior == selected_direct_id),
        }
        || adjustment_provenance_mismatch
    {
        return Err(FrameValidationError::ModelSettingsShape);
    }
    Ok(())
}

fn validate_settings_event(event: &SessionEvent) -> Result<(), FrameValidationError> {
    match event {
        SessionEvent::SessionModelSettingsChanged {
            prior_defaults_version,
            installed_defaults_version,
            prior_model,
            installed_model,
            prior_settings,
            installed_settings,
            caller_override,
            adjustments,
            ..
        } => {
            prior_settings.validate_defaults()?;
            installed_settings.validate_defaults()?;
            validate_adjustments(adjustments)?;
            let validation_changed = matches!(
                (
                    prior_settings.validated_for_selection_id,
                    installed_settings.validated_for_selection_id,
                ),
                (Some(prior), Some(installed)) if prior != installed
            );
            let copied_precedence = ModelSettingsPrecedence {
                per_call: prior_settings.precedence.per_call,
                session: prior_settings.precedence.session,
                profile: installed_settings.precedence.profile,
                global_default: installed_settings.precedence.global_default,
            };
            let unadjusted_precedence = ModelSettingsPrecedence {
                session: overlay_inheriting_from(
                    *caller_override,
                    prior_settings.precedence.session,
                ),
                ..copied_precedence
            };
            let provenance_matches = apply_wire_adjustments(unadjusted_precedence, adjustments)
                .is_some_and(|expected| expected == installed_settings.precedence);
            if prior_defaults_version.value() == 0
                || prior_defaults_version.value().checked_add(1)
                    != Some(installed_defaults_version.value())
                || (prior_model == installed_model && prior_settings == installed_settings)
                || !snapshot_matches_model(prior_model, prior_settings)
                || !snapshot_matches_model(installed_model, installed_settings)
                || !provenance_matches
                || (!adjustments.is_empty() && !validation_changed)
                || adjustments_target_explicit_overlay(*caller_override, adjustments)
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
        }
        SessionEvent::TurnModelSettingsResolved {
            defaults_version,
            requested_model,
            selected_direct_id,
            per_call_override,
            settings,
            adjusted_from_selection_id,
            adjustments,
            ..
        } => validate_turn_settings_payload(
            *defaults_version,
            requested_model,
            *selected_direct_id,
            *per_call_override,
            settings,
            *adjusted_from_selection_id,
            adjustments,
        )?,
        SessionEvent::ToolApprovalDecided {
            decision,
            decider,
            rationale,
            ..
        } => validate_tool_approval_event_shape(decision, decider, rationale)?,
        SessionEvent::InputAccepted { content, .. } => content.validate()?,
        SessionEvent::SessionCreated {}
        | SessionEvent::GoalTurnRetired { .. }
        | SessionEvent::TurnActivated { .. }
        | SessionEvent::ModelCallTransition { .. }
        | SessionEvent::ToolBatchTransition { .. }
        | SessionEvent::RunnerStateTransition { .. }
        | SessionEvent::ContextCompacted { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::TurnFailed { .. }
        | SessionEvent::TurnRefused { .. }
        | SessionEvent::TurnCancelled { .. }
        | SessionEvent::TurnReconciliationRequired { .. }
        | SessionEvent::TurnToolReconciliationRequired { .. }
        | SessionEvent::ChildSpawned { .. }
        | SessionEvent::ChildWaiting { .. }
        | SessionEvent::SessionMessage { .. }
        | SessionEvent::ChildResult { .. }
        | SessionEvent::ChildLifecycleDisposition { .. } => {}
    }
    Ok(())
}

fn adjustments_target_explicit_overlay(
    overlay: ModelSettingsOverlay,
    adjustments: &[ModelChangeAdjustment],
) -> bool {
    adjustments.iter().any(|adjustment| match adjustment {
        ModelChangeAdjustment::ReasoningLevelClamped { .. }
        | ModelChangeAdjustment::ReasoningLevelCleared { .. } => {
            overlay.reasoning_level != SettingOverlay::Inherit
        }
        ModelChangeAdjustment::FastModeDisabled {} => overlay.fast_mode != FastModeOverlay::Inherit,
        ModelChangeAdjustment::ServiceTierCleared { .. } => {
            overlay.service_tier != SettingOverlay::Inherit
        }
    })
}

fn validate_adjustments(adjustments: &[ModelChangeAdjustment]) -> Result<(), FrameValidationError> {
    if adjustments.len() > 3 {
        return Err(FrameValidationError::ModelSettingsShape);
    }
    let mut minimum_rank = 0;
    for adjustment in adjustments {
        let rank = match adjustment {
            ModelChangeAdjustment::ReasoningLevelClamped { .. }
            | ModelChangeAdjustment::ReasoningLevelCleared { .. } => 0,
            ModelChangeAdjustment::FastModeDisabled {} => 1,
            ModelChangeAdjustment::ServiceTierCleared { .. } => 2,
        };
        if rank < minimum_rank {
            return Err(FrameValidationError::ModelSettingsShape);
        }
        minimum_rank = rank + 1;
    }
    Ok(())
}

/// Origin fact whose dispatch holds one repository-watch singleton slot.
///
/// A rule matching branch workflow-run completion under `Rule` or `Repo`
/// singleton scope holds a slot from a branch fact, which names no pull
/// request; every other admitted origin names one. The two are exclusive, so
/// the shape is a tagged choice rather than a pair of nullable numbers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorStatusHeldSlotOrigin {
    /// A pull-request fact, named by its number.
    PullRequest { pull_request_number: CanonicalU64 },
    /// A branch workflow-run fact, named by its branch.
    Branch { branch: String },
}

/// Payload for one active repository-watch dispatch slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusHeldSlotMessage {
    pub dispatch_id: CanonicalUuid,
    pub repository: String,
    pub origin: OperatorStatusHeldSlotOrigin,
    pub rule_id: String,
    pub rule_version: CanonicalU64,
    pub singleton_scope: OperatorStatusSingletonScope,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_repository: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_pull_request_number: Option<CanonicalU64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_stack_root_pull_request_number: Option<CanonicalU64>,
    pub held_for_seconds: CanonicalU64,
    pub session_ids: Vec<CanonicalUuid>,
    pub blockers: Vec<OperatorStatusHeldSlotBlocker>,
}

/// Payload for one owed repository-watch dispatch waiting for admission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusQueuedObligationMessage {
    pub obligation_id: CanonicalUuid,
    pub repository: String,
    pub rule_id: String,
    pub rule_version: CanonicalU64,
    pub singleton_scope: OperatorStatusSingletonScope,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_repository: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_pull_request_number: Option<CanonicalU64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub singleton_stack_root_pull_request_number: Option<CanonicalU64>,
    pub first_event_id: CanonicalUuid,
    pub latest_event_id: CanonicalUuid,
    pub matched_event_count: CanonicalU64,
    pub waiting_for_seconds: CanonicalU64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub occupying_dispatch_id: Option<CanonicalUuid>,
    pub occupying_session_ids: Vec<CanonicalUuid>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub cooldown_remaining_seconds: Option<CanonicalU64>,
    pub cooldown_never_eligible: bool,
    pub ready: bool,
}

/// Payload for one latest pull-request convergence assessment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusPullRequestConvergenceMessage {
    pub repository: String,
    pub pull_request_number: CanonicalU64,
    pub head_sha: String,
    pub base_branch: String,
    pub base_revision: String,
    pub mergeable_state: OperatorStatusMergeableState,
    pub review_decision: OperatorStatusReviewDecision,
    pub unresolved_thread_count: CanonicalU64,
    pub gating_check_count: CanonicalU64,
    #[serde(
        serialize_with = "serialize_operator_status_check_names",
        deserialize_with = "deserialize_operator_status_check_names"
    )]
    pub non_green_gating_checks: Vec<String>,
    pub verdict: OperatorStatusConvergenceVerdict,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub seal: Option<OperatorStatusConvergenceSeal>,
    pub assessed_seconds_ago: CanonicalU64,
}

/// Payload for one stale blocking review whose planned clearance is unsettled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusPendingStaleReviewClearanceMessage {
    pub repository: String,
    pub pull_request_number: CanonicalU64,
    pub current_head_sha: String,
    pub review_node_id: String,
    pub reviewer: String,
    pub reviewed_head_sha: String,
    pub pending_for_seconds: CanonicalU64,
}

/// One non-terminal session state a deadline violation can be reported under.
///
/// `terminal` is absent by construction: a terminal session owes no deadline,
/// so a violation naming one would contradict the invariant it reports on.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorStatusLifecycleState {
    Created,
    Dispatched,
    Active,
    Waiting,
    Recovering,
    Blocked,
    Parked,
}

/// One calendar week's session-lifecycle metrics.
///
/// Every rate travels as its exact numerator and denominator rather than as a
/// ratio, so a week with an empty population reports no rate at all instead of
/// a zero the durable columns do not claim, and a reader compares exact counts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusLifecycleWeekMessage {
    /// The UTC start of the calendar week, as an ISO-8601 calendar date.
    pub week_start_date: String,
    /// Sessions counted as completion failures.
    pub completion_failure_numerator: CanonicalU64,
    /// The trimmed weekly terminal cohort the headline is over.
    pub completion_failure_denominator: CanonicalU64,
    /// `failed_unknown` closures inside that numerator.
    pub failed_unknown_count: CanonicalU64,
    /// Sessions recording context-headroom exhaustion on any turn.
    pub overflow_numerator: CanonicalU64,
    /// The untrimmed weekly terminal cohort, before the stopped and
    /// superseded trim.
    pub overflow_denominator: CanonicalU64,
    /// Overflow sessions whose outcome was `achieved_verified`.
    pub finish_given_overflow_numerator: CanonicalU64,
    /// Dispatch-cohort sessions recording a compaction wall.
    pub wall_numerator: CanonicalU64,
    /// The week's dispatch cohort.
    pub wall_denominator: CanonicalU64,
    /// Walls recorded in this week, whatever cohort they belong to.
    pub wall_occurrence_count: CanonicalU64,
    /// Terminal turns carrying a cause outside the catch-all set.
    pub classified_terminal_turn_count: CanonicalU64,
    /// Terminal turns recorded in this week.
    pub terminal_turn_count: CanonicalU64,
    /// `known_failed` calls carrying a cause outside the catch-all set.
    pub classified_known_failed_call_count: CanonicalU64,
    /// `known_failed` model calls recorded in this week.
    pub known_failed_call_count: CanonicalU64,
}

/// One owned non-terminal session violating the armed-deadline invariant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusLifecycleDeadlineViolationMessage {
    pub session_id: CanonicalUuid,
    pub state: OperatorStatusLifecycleState,
    /// Whether the session holds no armed deadline record at all.
    pub deadline_missing: bool,
    /// How long the armed expiry has been past, absent for a missing record.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub expired_for_seconds: Option<CanonicalU64>,
}

/// Terminal counts for one coherent repository-watch operator-status snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorStatusEndMessage {
    pub held_slot_count: CanonicalU64,
    pub queued_obligation_count: CanonicalU64,
    pub pull_request_convergence_count: CanonicalU64,
    pub pending_stale_review_clearance_count: CanonicalU64,
    pub lifecycle_week_count: CanonicalU64,
    /// The `nonterminal_past_deadline` alarm value, target zero.
    pub lifecycle_deadline_violation_count: CanonicalU64,
}

/// One member of a coherent repository-watch operator-status snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorStatusMessage {
    /// Begins the snapshot.
    Start {},
    /// One active repository-watch dispatch slot.
    HeldSlot(Box<OperatorStatusHeldSlotMessage>),
    /// One owed repository-watch dispatch waiting for admission.
    QueuedObligation(Box<OperatorStatusQueuedObligationMessage>),
    /// One latest pull-request convergence assessment.
    PullRequestConvergence(Box<OperatorStatusPullRequestConvergenceMessage>),
    /// One stale blocking review whose planned clearance is not yet settled.
    PendingStaleReviewClearance(Box<OperatorStatusPendingStaleReviewClearanceMessage>),
    /// One calendar week of session-lifecycle metrics.
    LifecycleWeek(Box<OperatorStatusLifecycleWeekMessage>),
    /// One owned non-terminal session past its armed-deadline obligation.
    LifecycleDeadlineViolation(Box<OperatorStatusLifecycleDeadlineViolationMessage>),
    /// Completes the snapshot with its section counts.
    End(Box<OperatorStatusEndMessage>),
}

/// Closed versioned server message family.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    /// Session creation receipt.
    SessionCreated {
        /// Created session.
        session_id: CanonicalUuid,
        /// Complete settings snapshot installed as defaults version one.
        model_settings: ModelSettingsSnapshot,
    },
    /// Commissioned-session receipt: the composite committed or replayed.
    SessionCommissioned {
        /// Created session.
        session_id: CanonicalUuid,
        /// Append-only commissioned-dispatch record carrying the fence.
        dispatch_id: CanonicalUuid,
    },
    /// A durable session-lifecycle command applied.
    SessionLifecycleCommandApplied {
        /// Target session.
        session_id: CanonicalUuid,
        /// What the command did.
        effect: SessionLifecycleEffect,
    },
    /// One delegated child spawn was recorded or equally replayed.
    SessionSpawned {
        /// Exact logical spawn tool request.
        tool_request_id: CanonicalUuid,
        /// Created child identity.
        child_session_id: CanonicalUuid,
        /// Exact immutable relationship policy.
        relationship: DelegationPolicy,
    },
    /// One child-delivery registration was recorded or equally replayed.
    SessionAwaitRegistered {
        /// Exact logical await tool request.
        tool_request_id: CanonicalUuid,
        /// Related child identity.
        child_session_id: CanonicalUuid,
        /// Exact registered delivery mode.
        mode: DelegationWaitMode,
    },
    /// One child outcome was delivered directly to a foreground await.
    ChildResult {
        /// Exact logical await tool request receiving the result.
        await_request_id: CanonicalUuid,
        /// Logical tool request that created the relationship.
        spawning_request_id: CanonicalUuid,
        /// Child whose terminal result was delivered.
        child_session_id: CanonicalUuid,
        /// Closed result outcome.
        outcome: DelegationOutcome,
        /// Exact returned content only for `returned`.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        content: Option<String>,
        /// Closed reason correlated with the outcome.
        reason: DelegationReason,
        /// Exact child-turn or parent-command authority.
        provenance: DelegationProvenance,
    },
    /// One relationship message was recorded or equally replayed.
    SessionMessageSent {
        /// Exact logical message tool request.
        tool_request_id: CanonicalUuid,
        /// Immutable message identity.
        message_id: CanonicalUuid,
        /// Exact relationship direction.
        direction: DelegationMessageDirection,
        /// Positive contiguous relationship event ordinal.
        ordinal: CanonicalU64,
        /// Positive recipient-wide delivery sequence.
        delivery_sequence: CanonicalU64,
    },
    /// One immutable placement update was appended or equally replayed.
    SessionPlacementUpdated {
        session_id: CanonicalUuid,
        placement_version: CanonicalU64,
        placement: SessionPlacement,
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
        /// Complete settings snapshot frozen for the origin turn.
        model_settings: ModelSettingsSnapshot,
    },
    /// Configuration-free steering acceptance receipt.
    SteeringSubmitted {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Accepted input.
        accepted_input_id: CanonicalUuid,
        /// Immutable acceptance position.
        acceptance_position: CanonicalU64,
        /// Exact active turn the steering is bound to.
        source_turn_id: CanonicalUuid,
    },
    /// A durable user goal command appended one event.
    GoalTransitionApplied {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Appended event position.
        event_ordinal: CanonicalU64,
        /// Generation acted on by the event.
        generation: CanonicalU64,
    },
    /// Begins one complete goal-history snapshot.
    GoalHistoryStart {
        /// Owning session.
        session_id: CanonicalUuid,
        /// Current immutable statement generation.
        current_generation: CanonicalU64,
        /// Current immutable statement.
        current_statement: String,
    },
    /// Carries the current lifecycle state in a frame bounded independently from text.
    GoalHistoryState {
        /// Current derived lifecycle state.
        current_state: GoalLifecycleState,
    },
    /// One ordered event in a goal-history snapshot.
    GoalHistoryItem {
        /// Positive contiguous event position.
        event_ordinal: CanonicalU64,
        /// Statement generation acted on by the event.
        generation: CanonicalU64,
        /// Exact event payload and provenance.
        event: GoalHistoryEvent,
    },
    /// Completes one goal-history snapshot.
    GoalHistoryEnd {
        /// Number of preceding history items.
        event_count: CanonicalU64,
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
        /// Current immutable placement-history version.
        placement_version: CanonicalU64,
        /// Current opt-in placement decision.
        placement: SessionPlacement,
        /// Complete current runner projection, null for daemon-only sessions.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        runner: Option<RunnerProjection>,
    },
    /// Completes a session-summary sequence.
    SessionsEnd {
        /// Number of preceding summaries.
        session_count: CanonicalU64,
    },
    /// One member of a coherent repository-watch operator-status snapshot.
    OperatorStatus(Box<OperatorStatusMessage>),
    /// Begins the available-template sequence.
    TemplatesStart {},
    /// One available static template summary.
    TemplateSummary {
        /// Validated template name.
        name: String,
        /// Positive operator-assigned bundle version.
        version: CanonicalU64,
    },
    /// Completes the available-template sequence.
    TemplatesEnd {
        /// Number of preceding summaries.
        template_count: CanonicalU64,
    },
    /// Client-relevant deployment policy, with null denoting unbounded.
    DeploymentLimits {
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_message_utf8_bytes: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_system_prompt_utf8_bytes: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        terminal_input_channel_capacity: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        min_metadata_page_size: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_metadata_page_size: Option<CanonicalU64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        max_review_findings_per_run: Option<CanonicalU64>,
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
    /// Begins the configured model-alias sequence.
    ModelAliasesStart {},
    /// One configured alias and the direct selection it currently names.
    ModelAliasSummary {
        /// Stable alias identity selectable by creation commands.
        alias_id: CanonicalUuid,
        /// Current deployment-owned direct selection target.
        selection_id: CanonicalUuid,
    },
    /// Completes the configured model-alias sequence.
    ModelAliasesEnd {
        /// Number of preceding alias summaries.
        alias_count: CanonicalU64,
    },
    /// Begins the configured model-capability sequence.
    ModelCapabilitiesStart {},
    /// One direct selection and its exact client-visible capabilities.
    ModelCapabilityItem {
        selection_id: CanonicalUuid,
        capabilities: ModelCapabilities,
    },
    /// Completes the configured model-capability sequence.
    ModelCapabilitiesEnd { capability_count: CanonicalU64 },
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
        /// Complete settings snapshot installed on the new epoch.
        model_settings: ModelSettingsSnapshot,
        /// Complete committed dangerous-tool blanket-auto posture.
        dangerous_tool_auto_approval: bool,
        /// Complete committed system prompt; required null-or-text member.
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
        /// Complete settings snapshot stored on the selected epoch.
        model_settings: ModelSettingsSnapshot,
        /// Complete dangerous-tool blanket-auto posture on that epoch.
        dangerous_tool_auto_approval: bool,
        /// Exact optional system prompt on that epoch.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        system_prompt: Option<SystemPromptText>,
    },
    /// One recorded user tool-decision receipt.
    ///
    /// The receipt mirrors the recorded applied result exactly; an equal
    /// command replay returns this same projection.
    ToolRequestDecided {
        /// Decided logical tool request.
        tool_request_id: CanonicalUuid,
        /// Exact recorded decision.
        decision: ToolDecision,
    },
    /// One recorded recorded-override receipt.
    ///
    /// The receipt mirrors the recorded applied result exactly; an equal
    /// command replay returns this same projection.
    ToolDenialOverridden {
        /// Overridden delegate-denied tool request.
        tool_request_id: CanonicalUuid,
    },
    /// One completed append-only context-compaction receipt.
    SessionCompacted {
        /// Compacted session.
        session_id: CanonicalUuid,
        /// Immutable compaction identity.
        context_compaction_id: CanonicalUuid,
        /// Dedicated producing model call.
        model_call_id: CanonicalUuid,
        /// One-based exact through position in the source frontier.
        through_position: CanonicalU64,
        /// Appended summary semantic entry.
        summary_entry_id: CanonicalUuid,
        /// Complete source-plus-summary result frontier.
        result_frontier_id: CanonicalUuid,
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
    /// One per-connection chunked import was initialized.
    ConversationImportBegun {
        /// Exact total source size admitted from the begin request.
        declared_size_bytes: CanonicalU64,
    },
    /// One source chunk was appended to the in-progress import.
    ConversationImportAppended {
        /// Exact total source bytes observed after this append.
        assembled_size_bytes: CanonicalU64,
    },
    /// One per-connection chunked import was discarded.
    ConversationImportAborted {},
    /// One connection-local immutable-blob upload was initialized.
    BlobUploadBegun {
        expected_digest: CanonicalBlobDigest,
        expected_length_bytes: CanonicalU64,
    },
    /// The routed store already held a verified replica, so no chunks are owed.
    BlobUploadAlreadyPresent {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    /// One bounded chunk was appended to the connection-local spool.
    BlobUploadAppended {
        assembled_length_bytes: CanonicalU64,
    },
    /// The exact assembled bytes were published and catalogued.
    BlobUploadCommitted {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
    },
    /// One connection-local immutable-blob upload was discarded.
    BlobUploadAborted {},
    /// Bounded catalog facts for one immutable identity.
    BlobMetadata {
        digest: CanonicalBlobDigest,
        byte_length: CanonicalU64,
        replica_count: CanonicalU64,
    },
    /// One exact verified byte range.
    #[serde(rename = "blob_chunk")]
    BlobChunkRead {
        digest: CanonicalBlobDigest,
        offset_bytes: CanonicalU64,
        bytes: BlobChunk,
    },
    /// Begins one imported-conversation entry sequence.
    ImportedConversationStart {
        /// Inspected imported conversation.
        imported_conversation_id: CanonicalUuid,
    },
    /// One imported entry as the inspection projection presents it.
    ImportedConversationEntry {
        /// One-based imported position, exactly the ordinal
        /// `create_session_from_imported_frontier` consumes.
        position: CanonicalU64,
        /// Immutable imported entry identity.
        imported_entry_id: CanonicalUuid,
        /// Exact source-speaker attestation.
        source_speaker: ImportedSourceSpeaker,
        /// Normalized content variant.
        content_kind: ImportedContentKind,
        /// Bounded preview of exact attested text, or null when this entry
        /// carries no exact attested text.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        text_preview: Option<ImportedTextPreview>,
    },
    /// Completes one imported-conversation entry sequence.
    ImportedConversationEnd {
        /// Inspected imported conversation.
        imported_conversation_id: CanonicalUuid,
        /// Number of preceding entries, equal to the greatest selectable
        /// position.
        entry_count: CanonicalU64,
    },
    /// Begins one transcript snapshot sequence.
    TranscriptSnapshotStart {
        /// Selected session.
        session_id: CanonicalUuid,
        /// Snapshot outbox cursor.
        cursor: CanonicalU64,
        /// Complete current runner placement, or null for a daemon-only session.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        runner: Option<RunnerProjection>,
    },
    /// One authoritative turn projection.
    TranscriptTurn {
        /// Immutable turn identity.
        turn_id: CanonicalUuid,
        /// Immutable acceptance order.
        acceptance_position: CanonicalU64,
        /// Complete frozen settings for a settings-aware turn, or null for a
        /// turn committed before settings evidence existed.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        model_settings: Option<TurnModelSettingsSnapshot>,
        /// Exact lifecycle state.
        state: TurnState,
    },
    /// Exact independently nullable token fields for one terminal model call.
    TranscriptModelCallUsage {
        /// Zero-based model-call evidence index in this snapshot.
        model_call_index: CanonicalU64,
        /// Turn that owns the terminal model call.
        turn_id: CanonicalUuid,
        /// Immutable model-call identity.
        model_call_id: CanonicalUuid,
        /// Closed source vocabulary for the independently nullable counts.
        usage_provenance: UsageProvenance,
        /// Exact independently nullable fields from the named provenance.
        usage: ModelCallTokenUsage,
        /// Read-time configured-rate derivation, required null when unavailable.
        #[serde(deserialize_with = "deserialize_required_nullable")]
        cost: Option<ModelCallDollarCost>,
    },
    /// Completes the model-call evidence section of one transcript snapshot.
    TranscriptModelCallsEnd {
        /// Number of preceding model-call usage messages.
        model_call_count: CanonicalU64,
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
    /// One atomic native user entry with exact ordered multipart content.
    TranscriptUserEntry {
        /// Zero-based frontier member index.
        entry_index: CanonicalU64,
        /// Entry source session.
        source_session_id: CanonicalUuid,
        /// Semantic entry identity.
        entry_id: CanonicalUuid,
        /// Exact accepted input.
        accepted_input_id: CanonicalUuid,
        /// Origin turn.
        turn_id: CanonicalUuid,
        /// Canonical ordered user content.
        content: UserInputContent,
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
    /// One pass without another typed result was terminalized.
    ReviewPassCompleted {
        run_id: CanonicalUuid,
        pass_id: CanonicalUuid,
        state: ReviewPassLifecycle,
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
    ReviewFindingEventRecorded {
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
    /// One orchestration attempt was admitted or equally replayed.
    ReviewOrchestrationStarted { attempt_id: CanonicalUuid },
    /// One orchestration attempt advanced or equally replayed.
    ReviewOrchestrationAdvanced {
        attempt_id: CanonicalUuid,
        state: ReviewOrchestrationState,
    },
    /// One complete orchestration attempt read.
    ReviewOrchestration {
        snapshot: ReviewOrchestrationSnapshot,
    },
    /// Stable, sanitized failure.
    Error {
        /// Stable error code.
        code: ErrorCode,
        /// Non-sensitive human diagnostic.
        message: String,
        /// Typed durable-rejection or conversation-import failure evidence.
        #[serde(default, skip_serializing_if = "ErrorDetail::is_absent")]
        detail: ErrorDetail,
    },
}

impl ServerMessage {
    fn validate(&self) -> Result<(), FrameValidationError> {
        validate_operator_status_message(self)?;
        match self {
            Self::SessionCreated { model_settings, .. } => model_settings.validate_defaults()?,
            Self::SessionAwaitRegistered {
                mode: DelegationWaitMode::Foreground,
                ..
            } => {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::ChildResult {
                await_request_id,
                spawning_request_id,
                child_session_id,
                outcome,
                content,
                reason,
                provenance,
            } if await_request_id == spawning_request_id
                || !direct_child_result_shape_is_valid(
                    *child_session_id,
                    *outcome,
                    content,
                    *reason,
                    provenance,
                ) =>
            {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::SessionMessageSent {
                ordinal,
                delivery_sequence,
                ..
            } if ordinal.value() < 2 || delivery_sequence.value() == 0 => {
                return Err(FrameValidationError::DelegationShape);
            }
            Self::SessionAwaitRegistered {
                mode: DelegationWaitMode::Background,
                ..
            }
            | Self::ChildResult { .. }
            | Self::SessionMessageSent { .. } => {}
            Self::InputSubmitted { model_settings, .. } => model_settings.validate()?,
            Self::SessionDefaultsReplaced {
                model_selection,
                model_settings,
                ..
            }
            | Self::SessionDefaults {
                model_selection,
                model_settings,
                ..
            } => {
                model_settings.validate_defaults()?;
                if !snapshot_matches_model(model_selection, model_settings) {
                    return Err(FrameValidationError::ModelSettingsShape);
                }
            }
            Self::SessionEvent {
                session_id, event, ..
            } => {
                validate_settings_event(event)?;
                validate_delegation_session_event(*session_id, event)?;
            }
            Self::TranscriptTurn {
                turn_id,
                model_settings,
                state,
                ..
            } => {
                if let TurnState::Queued { content, .. } = state {
                    content.validate()?;
                }
                if let Some(settings) = model_settings {
                    settings.validate()?;
                    if settings.turn_id != *turn_id
                        || (matches!(
                            state,
                            TurnState::Queued {
                                accepted_input_id,
                                ..
                            } if settings.accepted_input_id != *accepted_input_id
                        ))
                    {
                        return Err(FrameValidationError::ModelSettingsShape);
                    }
                }
            }
            Self::TranscriptEntry {
                entry:
                    TranscriptEntry::AssistantToolUse {
                        approval: Some(approval),
                        ..
                    },
                ..
            } => validate_tool_approval_event_shape(
                &approval.decision,
                &approval.decider,
                &approval.rationale,
            )?,
            Self::TranscriptUserEntry { content, .. } => content.validate()?,
            Self::GoalTransitionApplied {
                event_ordinal,
                generation,
                ..
            }
            | Self::GoalHistoryItem {
                event_ordinal,
                generation,
                ..
            } if event_ordinal.value() == 0 || generation.value() == 0 => {
                return Err(FrameValidationError::GoalShape);
            }
            Self::GoalHistoryStart {
                current_generation,
                current_statement,
                ..
            } => {
                if current_generation.value() == 0 {
                    return Err(FrameValidationError::GoalShape);
                }
                validate_goal_text(current_statement)?;
            }
            Self::GoalHistoryState { current_state } => validate_goal_state(current_state)?,
            Self::GoalHistoryItem { event, .. } => validate_goal_event(event)?,
            Self::GoalHistoryEnd { event_count } if event_count.value() == 0 => {
                return Err(FrameValidationError::GoalShape);
            }
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
                let canonical = canonical_metadata_tags(tags.clone(), None)
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
                if next_after_session_id.is_some() && session_count.value() == 0 {
                    return Err(FrameValidationError::MetadataShape);
                }
            }
            Self::ConversationSummary { conversation } => conversation.validate()?,
            Self::ModelCapabilityItem { capabilities, .. } => capabilities.validate()?,
            Self::ModelCapabilitiesEnd { capability_count }
                if capability_count.value() > MAX_MODEL_CAPABILITY_CATALOG_ENTRIES as u64 =>
            {
                return Err(FrameValidationError::ModelSettingsShape);
            }
            Self::ReviewPassCompleted {
                state: ReviewPassLifecycle::Queued | ReviewPassLifecycle::Running,
                ..
            } => {
                return Err(FrameValidationError::ReviewShape);
            }
            Self::ReviewOrchestration { snapshot } => {
                validate_review_orchestration_snapshot(snapshot)?;
            }
            Self::TemplateSummary { name, version } => {
                validate_session_template_name(name)?;
                if version.value() == 0 {
                    return Err(FrameValidationError::TemplateShape);
                }
            }
            Self::SessionSummary {
                placement_version,
                placement,
                ..
            } => {
                if placement_version.value() == 0 {
                    return Err(FrameValidationError::PlacementShape);
                }
                validate_session_placement_shape(placement)?;
            }
            Self::SessionPlacementUpdated {
                placement_version,
                placement,
                ..
            } => {
                if placement_version.value() == 0 {
                    return Err(FrameValidationError::PlacementShape);
                }
                validate_session_placement_shape(placement)?;
            }
            Self::ConversationPageEnd {
                conversation_count,
                next_after,
            } => {
                if next_after.is_some() && conversation_count.value() == 0 {
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
            Self::ImportedConversationEntry {
                position,
                content_kind,
                text_preview,
                ..
            } => {
                if position.value() == 0 {
                    return Err(FrameValidationError::ImportedConversationEntryShape);
                }
                if let Some(preview) = text_preview {
                    // Only `Text` content has an exact attested text to
                    // preview, so a preview on any other kind contradicts the
                    // kind it accompanies.
                    if *content_kind != ImportedContentKind::Text {
                        return Err(FrameValidationError::ImportedConversationEntryShape);
                    }
                    preview.validate()?;
                }
            }
            Self::ConversationImportAppended {
                assembled_size_bytes,
            } if assembled_size_bytes.value() == 0 => {
                return Err(FrameValidationError::ConversationImportShape);
            }
            Self::BlobUploadBegun {
                expected_length_bytes,
                ..
            }
            | Self::BlobUploadAlreadyPresent {
                byte_length: expected_length_bytes,
                ..
            }
            | Self::BlobUploadCommitted {
                byte_length: expected_length_bytes,
                ..
            }
            | Self::BlobUploadAppended {
                assembled_length_bytes: expected_length_bytes,
            } if expected_length_bytes.value() == 0 => {
                return Err(FrameValidationError::BlobUploadShape);
            }
            Self::BlobMetadata { byte_length, .. } if byte_length.value() == 0 => {
                return Err(FrameValidationError::BlobReadShape);
            }
            Self::BlobChunkRead {
                offset_bytes,
                bytes,
                ..
            } if bytes.as_bytes().is_empty()
                || bytes.as_bytes().len() > MAX_BLOB_READ_BYTES
                || u64::try_from(bytes.as_bytes().len()).map_or(true, |length_bytes| {
                    offset_bytes.value().checked_add(length_bytes).is_none()
                }) =>
            {
                return Err(FrameValidationError::BlobReadShape);
            }
            Self::TranscriptModelCallUsage { usage, cost, .. }
                if cost.is_some()
                    && usage.input_tokens.is_none()
                    && usage.output_tokens.is_none()
                    && usage.cache_creation_input_tokens.is_none()
                    && usage.cache_read_input_tokens.is_none() =>
            {
                return Err(FrameValidationError::ModelCallUsageShape);
            }
            Self::TranscriptEntry {
                source_session_id,
                entry,
                ..
            } => validate_delegation_transcript_entry(*source_session_id, entry)?,
            Self::SessionSpawned { .. } => {}
            _ => {}
        }
        Ok(())
    }
}

fn validate_operator_status_message(message: &ServerMessage) -> Result<(), FrameValidationError> {
    let ServerMessage::OperatorStatus(message) = message else {
        return Ok(());
    };
    let valid = match message.as_ref() {
        OperatorStatusMessage::HeldSlot(item) => {
            operator_status_repository_is_valid(&item.repository)
                && operator_status_held_slot_origin_is_valid(&item.origin, item.singleton_scope)
                && operator_status_held_slot_origin_matches_singleton(
                    &item.origin,
                    item.singleton_scope,
                    item.singleton_pull_request_number,
                )
                && operator_status_rule_id_is_valid(&item.rule_id)
                && item.rule_version.value() > 0
                && operator_status_singleton_is_valid(
                    &item.repository,
                    &OperatorStatusSingletonAxes {
                        scope: item.singleton_scope,
                        repository: item.singleton_repository.as_deref(),
                        pull_request_number: item.singleton_pull_request_number,
                        stack_root_pull_request_number: item
                            .singleton_stack_root_pull_request_number,
                    },
                )
                && (1..=MAX_OPERATOR_STATUS_DISPATCH_SESSIONS).contains(&item.session_ids.len())
                && values_are_distinct(&item.session_ids)
                && item.blockers.len() <= MAX_OPERATOR_STATUS_HELD_SLOT_BLOCKERS
                && item.blockers.windows(2).all(|pair| {
                    operator_status_blocker_rank(pair[0]) < operator_status_blocker_rank(pair[1])
                })
        }
        OperatorStatusMessage::QueuedObligation(item) => {
            // A blocking occupant is either a watch dispatch, which names its
            // identity and its whole admitted session inventory, or one
            // independently commissioned live session, which names that single
            // session and no dispatch. Both a dispatch identity owning no
            // sessions and a dispatch-less occupant naming more than the one
            // session the obligation retains contradict the projection.
            let occupancy_is_valid = match item.occupying_dispatch_id {
                Some(_) => (1..=MAX_OPERATOR_STATUS_DISPATCH_SESSIONS)
                    .contains(&item.occupying_session_ids.len()),
                None => item.occupying_session_ids.len() <= 1,
            };
            let is_occupied =
                item.occupying_dispatch_id.is_some() || !item.occupying_session_ids.is_empty();
            operator_status_repository_is_valid(&item.repository)
                && operator_status_rule_id_is_valid(&item.rule_id)
                && item.rule_version.value() > 0
                && operator_status_singleton_is_valid(
                    &item.repository,
                    &OperatorStatusSingletonAxes {
                        scope: item.singleton_scope,
                        repository: item.singleton_repository.as_deref(),
                        pull_request_number: item.singleton_pull_request_number,
                        stack_root_pull_request_number: item
                            .singleton_stack_root_pull_request_number,
                    },
                )
                && operator_status_obligation_lineage_is_coherent(item)
                && values_are_distinct(&item.occupying_session_ids)
                && occupancy_is_valid
                // The projection reports a remaining cooldown only while the
                // eligibility instant is still ahead of the read, and rounds
                // that strictly positive interval up, so the smallest value it
                // can carry is one second. A zero would name a cooldown that
                // has already lapsed while still claiming to withhold the
                // obligation.
                && item
                    .cooldown_remaining_seconds
                    .is_none_or(|remaining| remaining.value() > 0)
                && !(item.cooldown_remaining_seconds.is_some() && item.cooldown_never_eligible)
                && !(item.ready
                    && (is_occupied
                        || item.cooldown_remaining_seconds.is_some()
                        || item.cooldown_never_eligible))
        }
        OperatorStatusMessage::PullRequestConvergence(item) => {
            operator_status_repository_is_valid(&item.repository)
                && item.pull_request_number.value() > 0
                && operator_status_sha_is_valid(&item.head_sha)
                && operator_status_branch_is_valid(&item.base_branch)
                && operator_status_sha_is_valid(&item.base_revision)
                && item.unresolved_thread_count.value() <= MAX_OPERATOR_STATUS_UNRESOLVED_THREADS
                && item.gating_check_count.value() <= MAX_OPERATOR_STATUS_GATING_CHECKS
                && u64::try_from(item.non_green_gating_checks.len())
                    .is_ok_and(|count| count <= item.gating_check_count.value())
                && item.non_green_gating_checks.iter().all(|name| {
                    operator_status_text_is_valid(name, MAX_OPERATOR_STATUS_CHECK_NAME_UTF8_BYTES)
                })
                && item
                    .non_green_gating_checks
                    .windows(2)
                    .all(|pair| pair[0] <= pair[1])
                && operator_status_convergence_verdict_matches_evidence(item)
                && operator_status_convergence_base_branch_matches_verdict(item)
        }
        OperatorStatusMessage::PendingStaleReviewClearance(item) => {
            operator_status_repository_is_valid(&item.repository)
                && item.pull_request_number.value() > 0
                && operator_status_sha_is_valid(&item.current_head_sha)
                && operator_status_text_is_valid(
                    &item.review_node_id,
                    MAX_OPERATOR_STATUS_REVIEW_NODE_ID_UTF8_BYTES,
                )
                && operator_status_reviewer_is_valid(&item.reviewer)
                && operator_status_sha_is_valid(&item.reviewed_head_sha)
                && item.current_head_sha != item.reviewed_head_sha
        }
        OperatorStatusMessage::LifecycleWeek(item) => {
            // Every pair is a rate, so no numerator may exceed its own
            // denominator; the trim only removes members, so the headline's
            // denominator cannot exceed the untrimmed cohort the overflow rate
            // is over; and `failed_unknown` is one arm of the headline's
            // numerator rather than a count beside it.
            operator_status_calendar_date_is_valid(&item.week_start_date)
                && item.completion_failure_numerator.value()
                    <= item.completion_failure_denominator.value()
                && item.failed_unknown_count.value() <= item.completion_failure_numerator.value()
                && item.completion_failure_denominator.value() <= item.overflow_denominator.value()
                && item.overflow_numerator.value() <= item.overflow_denominator.value()
                && item.finish_given_overflow_numerator.value() <= item.overflow_numerator.value()
                && item.wall_numerator.value() <= item.wall_denominator.value()
                && item.classified_terminal_turn_count.value() <= item.terminal_turn_count.value()
                && item.classified_known_failed_call_count.value()
                    <= item.known_failed_call_count.value()
        }
        OperatorStatusMessage::LifecycleDeadlineViolation(item) => {
            // The two report the one fact together: a session with no armed
            // record has no expiry to be past, and a session whose expiry is
            // past has a record.
            item.deadline_missing == item.expired_for_seconds.is_none()
        }
        OperatorStatusMessage::Start {} | OperatorStatusMessage::End(_) => true,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::OperatorStatusShape)
    }
}

/// Accepts exactly a real `YYYY-MM-DD` calendar date.
///
/// A week label is what a reader groups by, and `2026-99-99` has the shape
/// without being a day.
fn operator_status_calendar_date_is_valid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    // Integer parsing accepts a leading sign, so `+026-08-31` has the width
    // without having the shape.
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return false;
    }
    let Some(Ok(year)) = value.get(0..4).map(str::parse::<i64>) else {
        return false;
    };
    let Some(Ok(month)) = value.get(5..7).map(str::parse::<u32>) else {
        return false;
    };
    let Some(Ok(day)) = value.get(8..10).map(str::parse::<u32>) else {
        return false;
    };
    (1..=12).contains(&month) && day >= 1 && day <= operator_status_days_in_month(year, month)
}

/// Returns how many days one month of one year has.
const fn operator_status_days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

fn operator_status_held_slot_origin_is_valid(
    origin: &OperatorStatusHeldSlotOrigin,
    singleton_scope: OperatorStatusSingletonScope,
) -> bool {
    match origin {
        OperatorStatusHeldSlotOrigin::PullRequest {
            pull_request_number,
        } => pull_request_number.value() > 0,
        // A branch workflow-run completion names no pull request, so the
        // singleton it takes can only be keyed by the rule or the repository.
        // A pull-request- or stack-scoped hold would have to name a pull
        // request the branch fact never carried, so the two fields are only
        // separately admissible and must be validated together.
        OperatorStatusHeldSlotOrigin::Branch { branch } => {
            operator_status_branch_is_valid(branch)
                && matches!(
                    singleton_scope,
                    OperatorStatusSingletonScope::Rule | OperatorStatusSingletonScope::Repo
                )
        }
    }
}

/// Holds the held-slot projection's own identity on the wire.
///
/// The durable projection joins each dispatch batch to the very
/// `repo_watch_event` row it was admitted from, reads the origin pull request
/// from that row, and carries the batch's singleton beside it. That singleton
/// was keyed from the same event, so a pull-request-scoped hold names the very
/// pull request its origin names; the two can never diverge in a row
/// persistence produced.
///
/// A stack-scoped hold carries no such equality. Its singleton names the root
/// of the open pull-request component the origin belongs to, which is a
/// different pull request whenever the origin is not itself that root, so the
/// stack axis is left to the scope shape alone. A branch origin never reaches
/// either pull-request scope, which the adjacent origin validator settles.
fn operator_status_held_slot_origin_matches_singleton(
    origin: &OperatorStatusHeldSlotOrigin,
    singleton_scope: OperatorStatusSingletonScope,
    singleton_pull_request_number: Option<CanonicalU64>,
) -> bool {
    match (origin, singleton_scope) {
        (
            OperatorStatusHeldSlotOrigin::PullRequest {
                pull_request_number,
            },
            OperatorStatusSingletonScope::PullRequest,
        ) => singleton_pull_request_number == Some(*pull_request_number),
        _ => true,
    }
}

/// Holds the durable obligation lineage on the wire.
///
/// Persistence opens an obligation naming one evaluated event as both its first
/// and its latest, with a matched count of one. Every later coalesced
/// evaluation replaces the latest event with a distinct one and increments the
/// count, and an event is evaluated at most once per rule version, so the count
/// stands at one exactly while the two endpoints are the same event. A count of
/// one across differing endpoints, or a larger count across identical ones,
/// names a lineage no obligation row can hold.
fn operator_status_obligation_lineage_is_coherent(
    item: &OperatorStatusQueuedObligationMessage,
) -> bool {
    item.matched_event_count.value() > 0
        && (item.matched_event_count.value() == 1) == (item.first_event_id == item.latest_event_id)
}

/// Holds the durable
/// `repo_watch_convergence_verdict_matches_evidence` constraint on the wire.
/// The stored assessment settles on the unconverged verdict exactly when the
/// pull request carries at least one blocker, so either converged verdict
/// contradicts every blocker the row carries beside it. Exactly one durable
/// disjunct — the unsettled provider snapshot — is not carried on this wire, so
/// the implication is only enforced in the direction the frame can prove: an
/// unconverged verdict stays admissible against wholly clean carried evidence,
/// while a converged verdict requires each carried condition to be clean.
fn operator_status_convergence_verdict_matches_evidence(
    item: &OperatorStatusPullRequestConvergenceMessage,
) -> bool {
    match item.verdict {
        OperatorStatusConvergenceVerdict::NotConverged => true,
        OperatorStatusConvergenceVerdict::InternallyConverged
        | OperatorStatusConvergenceVerdict::MergeReady => {
            item.unresolved_thread_count.value() == 0
                && item.non_green_gating_checks.is_empty()
                && item.mergeable_state == OperatorStatusMergeableState::Mergeable
                && item.gating_check_count.value() > 0
                && item.review_decision != OperatorStatusReviewDecision::ChangesRequested
        }
    }
}

/// Holds the durable base-branch pair on the wire.
///
/// Two constraints sit beside the evidence constraint on the same assessment
/// row, and the status projection reads the verdict and the base branch from
/// that one row: a merge-ready verdict is settled only against `main`, and an
/// internally-converged verdict only against a branch that is not `main`. The
/// pair is what distinguishes the two converged verdicts, so a merge-ready row
/// on a release branch or an internally-converged row on the trunk names an
/// assessment persistence cannot hold.
///
/// The unconverged verdict carries no base-branch constraint, and neither does
/// the seal beside it: a seal is retained from the assessment that earned it
/// and outlives later ones, so a pull request retargeted after it was sealed
/// carries that seal beside its new base branch.
fn operator_status_convergence_base_branch_matches_verdict(
    item: &OperatorStatusPullRequestConvergenceMessage,
) -> bool {
    match item.verdict {
        OperatorStatusConvergenceVerdict::NotConverged => true,
        OperatorStatusConvergenceVerdict::MergeReady => {
            item.base_branch == OPERATOR_STATUS_TRUNK_BASE_BRANCH
        }
        OperatorStatusConvergenceVerdict::InternallyConverged => {
            item.base_branch != OPERATOR_STATUS_TRUNK_BASE_BRANCH
        }
    }
}

/// The singleton axes of one operator-status row, each named at its call site.
///
/// The two numeric axes carry one type and mean different things, so they are
/// supplied by name rather than by position: a pull-request number transposed
/// with a stack-root pull-request number would otherwise compile silently and
/// admit rows the singleton grammar refuses.
struct OperatorStatusSingletonAxes<'a> {
    scope: OperatorStatusSingletonScope,
    repository: Option<&'a str>,
    pull_request_number: Option<CanonicalU64>,
    stack_root_pull_request_number: Option<CanonicalU64>,
}

/// Holds the singleton axes of one row against the row's own identity.
///
/// Every repository-keyed singleton is keyed from the repository of the very
/// event whose row carries it, and an obligation coalesces only across events
/// sharing its singleton key, so a carried singleton repository is that row's
/// own repository rather than an independent slug. The row's repository is
/// checked against the slug grammar by the caller, so the equality carries that
/// grammar onto the singleton axis with it.
fn operator_status_singleton_is_valid(
    row_repository: &str,
    axes: &OperatorStatusSingletonAxes<'_>,
) -> bool {
    let OperatorStatusSingletonAxes {
        scope,
        repository,
        pull_request_number,
        stack_root_pull_request_number,
    } = axes;
    let repository_is_valid = repository.is_none_or(|value| value == row_repository);
    repository_is_valid
        && match scope {
            OperatorStatusSingletonScope::PullRequest => {
                repository.is_some()
                    && pull_request_number.is_some_and(|value| value.value() > 0)
                    && stack_root_pull_request_number.is_none()
            }
            OperatorStatusSingletonScope::Stack => {
                repository.is_some()
                    && pull_request_number.is_none()
                    && stack_root_pull_request_number.is_some_and(|value| value.value() > 0)
            }
            OperatorStatusSingletonScope::Rule => {
                repository.is_none()
                    && pull_request_number.is_none()
                    && stack_root_pull_request_number.is_none()
            }
            OperatorStatusSingletonScope::Repo => {
                repository.is_some()
                    && pull_request_number.is_none()
                    && stack_root_pull_request_number.is_none()
            }
        }
}

fn operator_status_text_is_valid(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.contains('\0')
}

/// Holds the repository-slug grammar on the wire.
///
/// Mirrors the `RepositorySlug` constructor and the durable
/// `repo_watch_repository_is_valid` check: exactly one separator, each segment
/// nonempty and neither `.` nor `..`, and every byte an ASCII letter, digit,
/// hyphen, underscore, or dot. The constructor lowercases what it admits and
/// the durable check refuses anything else, so only the normalized spelling
/// ever reaches this wire and an uppercase byte is refused with the rest.
fn operator_status_repository_is_valid(value: &str) -> bool {
    let mut segments = value.split('/');
    let namespace = segments.next().unwrap_or_default();
    let name = segments.next().unwrap_or_default();
    operator_status_text_is_valid(value, MAX_OPERATOR_STATUS_REPOSITORY_UTF8_BYTES)
        && segments.next().is_none()
        && operator_status_repository_segment_is_valid(namespace)
        && operator_status_repository_segment_is_valid(name)
}

/// Holds one side of a repository slug.
fn operator_status_repository_segment_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

/// Holds the rule-identity grammar on the wire.
///
/// Mirrors the `RepoWatchRuleId` constructor and the durable
/// `repo_watch_rule_id_is_valid` check: every byte an ASCII letter, digit,
/// hyphen, underscore, or dot. Unlike the slug and the login, a rule identity
/// is the operator's own spelling and is never case-normalized, so both cases
/// are admitted.
fn operator_status_rule_id_is_valid(value: &str) -> bool {
    operator_status_text_is_valid(value, MAX_OPERATOR_STATUS_RULE_ID_UTF8_BYTES)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Holds the branch-name grammar on the wire.
///
/// Mirrors the `BranchName` constructor and the durable
/// `repo_watch_branch_is_valid` check, which are the same git ref-name rules:
/// the name is not `@`, does not begin with a hyphen, does not end with a dot,
/// carries neither `..` nor `@{`, carries no space, control byte, delete byte,
/// or one of `~^:?*[\`, and every slash-separated component is nonempty, does
/// not begin with a dot, and does not end with `.lock`. Both producers store
/// the name without its `refs/heads/` prefix, so the prefix is not stripped
/// again here.
fn operator_status_branch_is_valid(value: &str) -> bool {
    operator_status_text_is_valid(value, MAX_OPERATOR_STATUS_BRANCH_UTF8_BYTES)
        && value != "@"
        && !value.starts_with('-')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.bytes().any(|byte| {
            byte <= 0x20
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && value
            .split('/')
            .all(operator_status_branch_component_is_valid)
}

/// Holds one slash-separated component of a branch name.
fn operator_status_branch_component_is_valid(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('.') && !value.ends_with(".lock")
}

/// Holds the reviewer-login grammar on the wire.
///
/// Mirrors the `RepoWatchAuthorLogin` constructor and the durable
/// `repo_watch_login_is_valid` check: an optional literal App-bot suffix is set
/// aside, and the base left behind is nonempty, no wider than its own ceiling,
/// begins and ends with something other than a hyphen, carries no doubled
/// hyphen, and spells itself in ASCII lowercase letters, digits, hyphens, and
/// underscores. Both producers lowercase what they admit, so only the
/// normalized spelling reaches this wire.
fn operator_status_reviewer_is_valid(value: &str) -> bool {
    let base = value
        .strip_suffix(OPERATOR_STATUS_BOT_LOGIN_SUFFIX)
        .unwrap_or(value);
    operator_status_text_is_valid(value, MAX_OPERATOR_STATUS_REVIEWER_UTF8_BYTES)
        && !base.is_empty()
        && base.len() <= MAX_OPERATOR_STATUS_REVIEWER_BASE_UTF8_BYTES
        && !base.starts_with('-')
        && !base.ends_with('-')
        && !base.contains("--")
        && base.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn serialize_operator_status_check_names<SerializerT>(
    names: &[String],
    serializer: SerializerT,
) -> Result<SerializerT::Ok, SerializerT::Error>
where
    SerializerT: Serializer,
{
    let mut sequence = serializer.serialize_seq(Some(names.len()))?;
    for name in names {
        sequence.serialize_element(&STANDARD_BASE64.encode(name.as_bytes()))?;
    }
    sequence.end()
}

fn deserialize_operator_status_check_names<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<String>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|encoded| {
            let decoded = STANDARD_BASE64.decode(encoded.as_bytes()).map_err(|_| {
                serde::de::Error::custom("operator-status check name is not canonical base64")
            })?;
            if STANDARD_BASE64.encode(&decoded) != encoded {
                return Err(serde::de::Error::custom(
                    "operator-status check name is not canonical base64",
                ));
            }
            String::from_utf8(decoded)
                .map_err(|_| serde::de::Error::custom("operator-status check name is not UTF-8"))
        })
        .collect()
}

fn operator_status_sha_is_valid(value: &str) -> bool {
    value.len() == OPERATOR_STATUS_COMMIT_SHA_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn operator_status_blocker_rank(blocker: OperatorStatusHeldSlotBlocker) -> u8 {
    match blocker {
        OperatorStatusHeldSlotBlocker::UndeliveredAction => 0,
        OperatorStatusHeldSlotBlocker::DeliveryTurnRuntimeRelevant => 1,
        OperatorStatusHeldSlotBlocker::LiveRuntimeTurn => 2,
        OperatorStatusHeldSlotBlocker::PursuingGoal => 3,
    }
}

fn values_are_distinct<ValueT>(values: &[ValueT]) -> bool
where
    ValueT: Eq + std::hash::Hash,
{
    let mut distinct = HashSet::with_capacity(values.len());
    values.iter().all(|value| distinct.insert(value))
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

fn deserialize_optional_non_null<'de, DeserializerT, ValueT>(
    deserializer: DeserializerT,
) -> Result<Option<ValueT>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
    ValueT: Deserialize<'de>,
{
    ValueT::deserialize(deserializer).map(Some)
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
    /// Constructs a single-version response frame.
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
        if let ServerMessage::TranscriptTurn { state, .. } = &self.message {
            state.validate()?;
        }
        if let ServerMessage::SessionDefaultsReplaced { system_prompt, .. } = &self.message {
            validate_system_prompt_member(system_prompt)?;
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
                if let Some(RejectionDetail::ImportedFrontierPositionOutOfRange {
                    requested_position,
                    last_position,
                    ..
                }) = detail.value()
                {
                    // An imported conversation's positions are the contiguous
                    // sequence `1..=last_position`, so a nonpositive bound or a
                    // requested ordinal inside that range contradicts the
                    // rejection the detail states.
                    if last_position.value() == 0
                        || requested_position.value() <= last_position.value()
                    {
                        return Err(FrameValidationError::ImportedFrontierRangeShape);
                    }
                }
                if let Some(detail) = detail.value() {
                    if detail.is_bulk_ingest() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                    } else if detail.is_conversation_import() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_conversation_import_detail(detail)?;
                    } else if detail.is_blob_upload() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_blob_upload_detail(detail)?;
                    } else if detail.is_blob_read() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_blob_read_detail(detail)?;
                    } else if *code != ErrorCode::Rejected {
                        return Err(FrameValidationError::ErrorDetailShape);
                    } else {
                        validate_rejection_detail(detail)?;
                    }
                } else if *code == ErrorCode::Rejected {
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

fn validate_rejection_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::SessionPlacementCurrentVersionMismatch {
            expected_placement_version,
            current_placement_version,
            ..
        } => {
            expected_placement_version.value() > 0
                && current_placement_version.value() > 0
                && expected_placement_version != current_placement_version
        }
        RejectionDetail::SessionPlacementVersionExhausted {
            current_placement_version,
            ..
        } => current_placement_version.value() == u64::MAX,
        RejectionDetail::DelegationEventOrdinalExhausted { last, .. } => last.value() == u64::MAX,
        RejectionDetail::DelegationDeliverySequenceExhausted { last, .. } => {
            last.value() == u64::MAX
        }
        RejectionDetail::SessionNotFound { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestNotFound { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::ToolRequestNotInSession { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::DelegationRequestNotInTurn { .. }
        | RejectionDetail::DelegationToolRequestNotExecutable { .. }
        | RejectionDetail::DelegationSpawnConflict { .. }
        | RejectionDetail::DelegatedChildIdentityCollision { .. }
        | RejectionDetail::DelegationRelationNotFound { .. }
        | RejectionDetail::DelegationAwaitConflict { .. }
        | RejectionDetail::DelegationMessageConflict { .. }
        | RejectionDetail::DelegationMessageIdentityCollision { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. } => true,
        RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {}
        | RejectionDetail::ConversationImportSourceTooLarge { .. }
        | RejectionDetail::ConversationImportSourceSizeMismatch { .. }
        | RejectionDetail::ConversationImportConversionFailed { .. }
        | RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::ErrorDetailShape)
    }
}

fn validate_conversation_import_detail(
    detail: RejectionDetail,
) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::ConversationImportAlreadyInProgress {}
        | RejectionDetail::ConversationImportNotInProgress {} => true,
        RejectionDetail::ConversationImportSourceTooLarge {
            limit_bytes,
            declared_size_bytes,
            actual_size_bytes,
        } => {
            limit_bytes.value() > 0
                && match actual_size_bytes {
                    Some(actual) => {
                        actual.value() > limit_bytes.value()
                            && (declared_size_bytes.value() <= limit_bytes.value()
                                || declared_size_bytes == actual)
                    }
                    None => declared_size_bytes.value() > limit_bytes.value(),
                }
        }
        RejectionDetail::ConversationImportSourceSizeMismatch {
            declared_size_bytes,
            actual_size_bytes,
        } => declared_size_bytes != actual_size_bytes,
        RejectionDetail::ConversationImportConversionFailed {
            class,
            record_ordinal,
        } => match class {
            ConversationImportRejectionClass::EmptySource => record_ordinal.is_none(),
            ConversationImportRejectionClass::BlankLine
            | ConversationImportRejectionClass::InvalidUtf8
            | ConversationImportRejectionClass::InvalidJson
            | ConversationImportRejectionClass::JsonDepthExceeded
            | ConversationImportRejectionClass::TopLevelNotObject
            | ConversationImportRejectionClass::InvalidRecordType
            | ConversationImportRejectionClass::InvalidSourceMetadata
            | ConversationImportRejectionClass::InvalidMessageEnvelope
            | ConversationImportRejectionClass::InvalidMessageRole
            | ConversationImportRejectionClass::MessageRoleMismatch
            | ConversationImportRejectionClass::InvalidMessageContent
            | ConversationImportRejectionClass::InvalidContentBlock
            | ConversationImportRejectionClass::InvalidToolResultBlock
            | ConversationImportRejectionClass::InvalidReasoning
            | ConversationImportRejectionClass::InvalidToolCall
            | ConversationImportRejectionClass::InvalidToolResult => {
                record_ordinal.is_some_and(|ordinal| ordinal.value() > 0)
            }
        },
        RejectionDetail::SessionNotFound { .. }
        | RejectionDetail::AttachmentBlobNotFound { .. }
        | RejectionDetail::AttachmentByteBudgetExceeded { .. }
        | RejectionDetail::UnsupportedReasoningLevel { .. }
        | RejectionDetail::UnsupportedFastMode { .. }
        | RejectionDetail::UnsupportedServiceTier { .. }
        | RejectionDetail::SessionPlacementCurrentVersionMismatch { .. }
        | RejectionDetail::SessionPlacementVersionExhausted { .. }
        | RejectionDetail::GoalCommandRejected { .. }
        | RejectionDetail::SessionLifecycleCommandRejected { .. }
        | RejectionDetail::ActiveTurnPresent { .. }
        | RejectionDetail::CommissionTargetBusy { .. }
        | RejectionDetail::ActiveTurnMismatch { .. }
        | RejectionDetail::NoActiveTurn { .. }
        | RejectionDetail::TurnNotAwaitingReconciliation { .. }
        | RejectionDetail::InterruptAlreadyApplied { .. }
        | RejectionDetail::InterruptUnavailableWhileAwaitingApproval { .. }
        | RejectionDetail::SafePointUnavailableWhileStopping { .. }
        | RejectionDetail::ToolRequestNotFound { .. }
        | RejectionDetail::ToolRequestAlreadyResolved { .. }
        | RejectionDetail::ToolRequestNotEarliestUndecided { .. }
        | RejectionDetail::ToolRequestNotInSession { .. }
        | RejectionDetail::ToolRequestNotDelegateDenied { .. }
        | RejectionDetail::ToolRequestNotTerminallyDenied { .. }
        | RejectionDetail::ToolDenialAlreadyOverridden { .. }
        | RejectionDetail::DelegationRequestNotInTurn { .. }
        | RejectionDetail::DelegationToolRequestNotExecutable { .. }
        | RejectionDetail::DelegationSpawnConflict { .. }
        | RejectionDetail::DelegatedChildIdentityCollision { .. }
        | RejectionDetail::DelegationRelationNotFound { .. }
        | RejectionDetail::DelegationAwaitConflict { .. }
        | RejectionDetail::DelegationMessageConflict { .. }
        | RejectionDetail::DelegationMessageIdentityCollision { .. }
        | RejectionDetail::DelegationEventOrdinalExhausted { .. }
        | RejectionDetail::DelegationDeliverySequenceExhausted { .. }
        | RejectionDetail::DefaultsVersionMismatch { .. }
        | RejectionDetail::UnknownModelAlias { .. }
        | RejectionDetail::AcceptancePositionExhausted { .. }
        | RejectionDetail::DefaultsVersionExhausted { .. }
        | RejectionDetail::ImportedConversationNotFound { .. }
        | RejectionDetail::ImportedFrontierPositionOutOfRange { .. } => false,
        RejectionDetail::BulkIngestAlreadyInProgress { .. }
        | RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {}
        | RejectionDetail::BlobUploadLengthOutOfRange { .. }
        | RejectionDetail::BlobUploadSizeExceeded { .. }
        | RejectionDetail::BlobUploadLengthMismatch { .. }
        | RejectionDetail::BlobUploadDigestMismatch { .. }
        | RejectionDetail::BlobReadLengthOutOfRange { .. }
        | RejectionDetail::BlobReadRangeOutOfBounds { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::ConversationImportShape)
    }
}

fn validate_blob_upload_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::BlobUploadAlreadyInProgress {}
        | RejectionDetail::BlobUploadNotInProgress {} => true,
        RejectionDetail::BlobUploadLengthOutOfRange {
            min_length_bytes,
            max_length_bytes,
            declared_length_bytes,
        } => {
            min_length_bytes.value() > 0
                && min_length_bytes.value() <= max_length_bytes.value()
                && (declared_length_bytes.value() < min_length_bytes.value()
                    || declared_length_bytes.value() > max_length_bytes.value())
        }
        RejectionDetail::BlobUploadSizeExceeded {
            expected_length_bytes,
            actual_length_bytes,
        } => {
            expected_length_bytes.value() > 0
                && actual_length_bytes.value() > expected_length_bytes.value()
        }
        RejectionDetail::BlobUploadLengthMismatch {
            expected_length_bytes,
            actual_length_bytes,
        } => expected_length_bytes.value() > 0 && expected_length_bytes != actual_length_bytes,
        RejectionDetail::BlobUploadDigestMismatch {
            expected_digest,
            actual_digest,
        } => expected_digest != actual_digest,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::BlobUploadShape)
    }
}

fn validate_blob_read_detail(detail: RejectionDetail) -> Result<(), FrameValidationError> {
    let valid = match detail {
        RejectionDetail::BlobReadLengthOutOfRange {
            min_length_bytes,
            max_length_bytes,
            requested_length_bytes,
        } => {
            min_length_bytes.value() == 1
                && max_length_bytes.value() == MAX_BLOB_READ_BYTES as u64
                && (requested_length_bytes.value() < min_length_bytes.value()
                    || requested_length_bytes.value() > max_length_bytes.value())
        }
        RejectionDetail::BlobReadRangeOutOfBounds {
            offset_bytes,
            length_bytes,
            blob_length_bytes,
            ..
        } => {
            (1..=MAX_BLOB_READ_BYTES as u64).contains(&length_bytes.value())
                && blob_length_bytes.value() > 0
                && (offset_bytes
                    .value()
                    .checked_add(length_bytes.value())
                    .is_none_or(|end| end > blob_length_bytes.value()))
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FrameValidationError::BlobReadShape)
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
    /// A transcript turn carried an impossible correlated state shape.
    TurnStateShape,
    /// A tool approval event carried inconsistent decision provenance.
    ToolApprovalShape,
    /// A metadata request or response carried an invalid correlated shape.
    MetadataShape,
    /// A unified conversation-listing frame carried an invalid shape.
    ConversationListShape,
    /// A repository-watch operator-status row carried an invalid shape.
    OperatorStatusShape,
    SystemPromptShape,
    /// A chunked conversation-import frame carried a contradictory shape.
    ConversationImportShape,
    /// A chunked immutable-blob frame carried a contradictory shape.
    BlobUploadShape,
    /// A blob metadata, range, or range-rejection value contradicted its bounds.
    BlobReadShape,
    /// An imported-frontier request carried a nonpositive position.
    ImportedFrontierShape,
    /// A context-compaction request carried a nonpositive position.
    ContextCompactionShape,
    /// An imported-conversation entry carried a nonpositive position.
    ImportedConversationEntryShape,
    /// An imported text preview exceeded its bound or contradicted its own
    /// truncation marker.
    ImportedTextPreviewShape,
    /// An out-of-range imported rejection stated a range its own requested
    /// position falls inside, or an empty selectable range.
    ImportedFrontierRangeShape,
    /// A submit-input delivery carried forbidden or missing correlated fields.
    InputDeliveryShape,
    /// Ordered user parts violated their canonical shape or resource bounds.
    UserContentShape,
    /// A template name or positive version carried an invalid shape.
    TemplateShape,
    /// A review lifecycle or orchestration frame carried an invalid shape.
    ReviewShape,
    /// A model-call usage row carried cost without any reported usage axis.
    ModelCallUsageShape,
    /// A goal request, state, or event carried an invalid shape.
    GoalShape,
    /// A delegation update carried an invalid correlated shape.
    DelegationShape,
    /// Model settings or capability data carried a contradictory shape.
    ModelSettingsShape,
    /// A dotted placement or its root-global-read acknowledgement is invalid.
    PlacementShape,
    /// A commissioned-session authority fence carried an invalid shape.
    DispatchFenceShape,
}

impl fmt::Display for FrameValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedVersion => "frame version is unsupported",
            Self::UncorrelatedClientRequest => "client request identity is uncorrelated",
            Self::UncorrelatedSuccess => "successful server message is uncorrelated",
            Self::UncorrelatedApplicationError => "application server error is uncorrelated",
            Self::ErrorDetailShape => "server error detail does not match its code",
            Self::TurnStateShape => "transcript turn state is inconsistent",
            Self::ToolApprovalShape => "tool approval event shape is inconsistent",
            Self::MetadataShape => "session metadata frame shape is inconsistent",
            Self::ConversationListShape => {
                "unified conversation-listing frame shape is inconsistent"
            }
            Self::OperatorStatusShape => "operator-status frame shape is inconsistent",
            Self::SystemPromptShape => "frame omits its required system-prompt member",
            Self::ConversationImportShape => "conversation-import frame shape is inconsistent",
            Self::BlobUploadShape => "blob-upload frame shape is inconsistent",
            Self::BlobReadShape => "blob-read frame shape is inconsistent",
            Self::ImportedFrontierShape => "imported frontier position is not positive",
            Self::ContextCompactionShape => "compaction through position is not positive",
            Self::ImportedConversationEntryShape => {
                "imported conversation entry position is not positive"
            }
            Self::ImportedTextPreviewShape => "imported text preview shape is inconsistent",
            Self::ImportedFrontierRangeShape => "imported frontier rejection range is inconsistent",
            Self::InputDeliveryShape => "submit-input delivery shape is inconsistent",
            Self::UserContentShape => "ordered user content shape is inconsistent",
            Self::TemplateShape => "session-template frame shape is inconsistent",
            Self::ReviewShape => "review workflow frame shape is inconsistent",
            Self::ModelCallUsageShape => "model-call usage frame shape is inconsistent",
            Self::GoalShape => "commissioned-goal frame shape is inconsistent",
            Self::DelegationShape => "session-delegation frame shape is inconsistent",
            Self::ModelSettingsShape => "model-settings frame shape is inconsistent",
            Self::PlacementShape => "session-placement frame shape is inconsistent",
            Self::DispatchFenceShape => "commissioned-session fence shape is inconsistent",
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
                .write_str("process-protocol version is unsupported; supported version is 1"),
        }
    }
}

impl Error for FrameDecodeError {}

/// Outgoing frame could not be encoded within the protocol boundary.
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
        BillingRateVersion, BlobChunk, BulkIngestKind, CanonicalBlobDigest, CanonicalDigest,
        CanonicalDollarAmount, CanonicalU64, CanonicalUuid, CanonicalValueError, ClientFrame,
        ClientRequest, CommandId, CommissionedSessionFence, ContentFragment, ConversationCursor,
        ConversationImportFormat, ConversationImportRejectionClass, ConversationImportSource,
        ConversationOrigin, ConversationOriginFilter, ConversationSummary, CurrentModelCall,
        CurrentModelCallState, DelegationMessageDirection, DelegationOutcome, DelegationPolicy,
        DelegationProvenance, DelegationReason, DelegationToolRequestState, DelegationWaitMode,
        DescendantTerminationScope, EffectiveModelSettings, ErrorCode, ErrorDetail,
        FailedModelCallCause, FailedModelCallDisposition, FailedTerminalModelCall, FastMode,
        FastModeOverlay, FrameDecodeErrorKind, FrameEncodeError, FrameValidationError,
        GoalBlockedProvenance, GoalBlockedReason, GoalCommandRejection, GoalHistoryEvent,
        GoalLifecycleState, ImportedContentKind, ImportedConversationSourceFormat,
        ImportedSessionRelationship, ImportedSourceSpeaker, ImportedSpeaker, ImportedTextPreview,
        InputContent, InputDelivery, MAX_CONTENT_FRAGMENT_BYTES, MAX_JSON_CONTAINER_DEPTH,
        MAX_SESSION_METADATA_INDEXED_UTF8_BYTES, MAX_SESSION_METADATA_TOTAL_UTF8_BYTES,
        MetadataActor, MetadataLastWriter, ModelCallCostLabel, ModelCallDisposition,
        ModelCallDollarCost, ModelCallState, ModelCallTokenUsage, ModelCapabilities,
        ModelChangeAdjustment, ModelSelection, ModelSettingSource, ModelSettingsOverlay,
        ModelSettingsPrecedence, ModelSettingsSnapshot, OpenAiServiceTier,
        OperatorStatusConvergenceSeal, OperatorStatusConvergenceVerdict, OperatorStatusEndMessage,
        OperatorStatusHeldSlotBlocker, OperatorStatusHeldSlotMessage, OperatorStatusHeldSlotOrigin,
        OperatorStatusLifecycleDeadlineViolationMessage, OperatorStatusLifecycleState,
        OperatorStatusLifecycleWeekMessage, OperatorStatusMergeableState, OperatorStatusMessage,
        OperatorStatusPendingStaleReviewClearanceMessage,
        OperatorStatusPullRequestConvergenceMessage, OperatorStatusQueuedObligationMessage,
        OperatorStatusReviewDecision, OperatorStatusSingletonScope, PROTOCOL_VERSION,
        PositiveCanonicalU64, ProtocolVersion, ReasoningLevel, RejectionDetail, RequestId,
        ReviewConcernTerminalOutcome, ReviewFindingEvent, ReviewImportTerminalOutcome,
        ReviewJudgmentDisposition, ReviewJudgmentEffectTerminalOutcome, ReviewJudgmentPlanMember,
        ReviewOrchestrationConcernInput, ReviewOrchestrationConcernSnapshot,
        ReviewOrchestrationConcernStatus, ReviewOrchestrationCounts, ReviewOrchestrationSnapshot,
        ReviewOrchestrationStageTemplateDigests, ReviewOrchestrationState, ReviewPassLifecycle,
        ReviewPassTerminalOutcome, ReviewPublicationOutcome, ReviewPublicationTerminalOutcome,
        ReviewRepairOutcome, ReviewRepairTerminalOutcome, ReviewTargetSubject,
        RunnerCapabilityClass, RunnerConnectionHealth, RunnerCredentialProfileName,
        RunnerPlacementRevision, RunnerProjection, RunnerProjectionSelector, RunnerProjectionState,
        RunnerRepositoryKey, RunnerSandboxProfile, RunnerStateTransitionState,
        RunnerWorkingDirectory, ServerFrame, ServerMessage, ServiceTier, SessionEvent,
        SessionLifecycleEffect, SessionLifecycleMembers, SessionMetadata, SettingOverlay,
        SystemPromptMember, SystemPromptText, ToolApprovalEventDecider, ToolApprovalEventDecision,
        ToolBatchState, ToolDecision, TranscriptEntry, TranscriptTextEntry, TranscriptToolApproval,
        TurnModelSettingsSnapshot, TurnState, UsageProvenance, UserAttachmentKind,
        UserInputContent, UserInputPart, decode_client_line, decode_server_line,
        encode_client_line, encode_server_line, operator_status_calendar_date_is_valid,
        validate_adjustments,
    };
    use signalbox_domain::ToolDecisionRationale;
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

    /// Arbitrary distinct identities whose field names preserve delegation wire roles.
    #[derive(Clone, Copy)]
    struct DelegationWireIdentities {
        parent_session: CanonicalUuid,
        parent_turn: CanonicalUuid,
        spawning_request: CanonicalUuid,
        await_request: CanonicalUuid,
        child_session: CanonicalUuid,
        child_message_turn: CanonicalUuid,
        message_request: CanonicalUuid,
        terminal_child_turn: CanonicalUuid,
        message: CanonicalUuid,
        parent_command: CanonicalUuid,
    }

    fn delegation_wire_identities() -> DelegationWireIdentities {
        DelegationWireIdentities {
            parent_session: uuid(1),
            parent_turn: uuid(2),
            spawning_request: uuid(3),
            await_request: uuid(4),
            child_session: uuid(5),
            child_message_turn: uuid(6),
            message_request: uuid(7),
            terminal_child_turn: uuid(8),
            message: uuid(9),
            parent_command: uuid(10),
        }
    }

    fn settings_snapshot_fixture() -> ModelSettingsSnapshot {
        let per_call = ModelSettingsOverlay {
            reasoning_level: SettingOverlay::Value(ReasoningLevel::High),
            fast_mode: FastModeOverlay::Inherit,
            service_tier: SettingOverlay::ProviderDefault,
        };
        let inherited = ModelSettingsOverlay::inherit_all();
        ModelSettingsSnapshot {
            precedence: ModelSettingsPrecedence {
                per_call,
                session: inherited,
                profile: inherited,
                global_default: inherited,
            },
            effective: EffectiveModelSettings {
                reasoning_level: Some(ReasoningLevel::High),
                fast_mode: FastMode::Disabled,
                service_tier: None,
            },
            reasoning_source: Some(ModelSettingSource::PerCall),
            fast_mode_source: None,
            service_tier_source: Some(ModelSettingSource::PerCall),
            validated_for_selection_id: Some(uuid(4)),
        }
    }

    fn provider_default_settings_snapshot_fixture() -> ModelSettingsSnapshot {
        let inherited = ModelSettingsOverlay::inherit_all();
        ModelSettingsSnapshot {
            precedence: ModelSettingsPrecedence {
                per_call: inherited,
                session: inherited,
                profile: inherited,
                global_default: inherited,
            },
            effective: EffectiveModelSettings {
                reasoning_level: None,
                fast_mode: FastMode::Disabled,
                service_tier: None,
            },
            reasoning_source: None,
            fast_mode_source: None,
            service_tier_source: None,
            validated_for_selection_id: None,
        }
    }

    fn session_settings_snapshot_fixture() -> ModelSettingsSnapshot {
        let inherited = ModelSettingsOverlay::inherit_all();
        let session = ModelSettingsOverlay {
            reasoning_level: SettingOverlay::Value(ReasoningLevel::High),
            fast_mode: FastModeOverlay::Inherit,
            service_tier: SettingOverlay::ProviderDefault,
        };
        ModelSettingsSnapshot {
            precedence: ModelSettingsPrecedence {
                per_call: inherited,
                session,
                profile: inherited,
                global_default: inherited,
            },
            effective: EffectiveModelSettings {
                reasoning_level: Some(ReasoningLevel::High),
                fast_mode: FastMode::Disabled,
                service_tier: None,
            },
            reasoning_source: Some(ModelSettingSource::Session),
            fast_mode_source: None,
            service_tier_source: Some(ModelSettingSource::Session),
            validated_for_selection_id: Some(uuid(4)),
        }
    }

    const SETTINGS_SNAPSHOT_JSON: &str = concat!(
        "{\"precedence\":{",
        "\"per_call\":{\"reasoning_level\":{\"kind\":\"value\",\"value\":\"high\"},",
        "\"fast_mode\":{\"kind\":\"inherit\"},",
        "\"service_tier\":{\"kind\":\"provider_default\"}},",
        "\"session\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
        "\"fast_mode\":{\"kind\":\"inherit\"},",
        "\"service_tier\":{\"kind\":\"inherit\"}},",
        "\"profile\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
        "\"fast_mode\":{\"kind\":\"inherit\"},",
        "\"service_tier\":{\"kind\":\"inherit\"}},",
        "\"global_default\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
        "\"fast_mode\":{\"kind\":\"inherit\"},",
        "\"service_tier\":{\"kind\":\"inherit\"}}},",
        "\"effective\":{\"reasoning_level\":\"high\",\"fast_mode\":\"disabled\",",
        "\"service_tier\":null},\"reasoning_source\":\"per_call\",",
        "\"fast_mode_source\":null,\"service_tier_source\":\"per_call\",",
        "\"validated_for_selection_id\":\"00000000-0000-0000-0000-000000000004\"}"
    );

    const PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON: &str = concat!(
        "{\"precedence\":{",
        "\"per_call\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
        "\"fast_mode\":{\"kind\":\"inherit\"},",
        "\"service_tier\":{\"kind\":\"inherit\"}},",
        "\"session\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
        "\"fast_mode\":{\"kind\":\"inherit\"},",
        "\"service_tier\":{\"kind\":\"inherit\"}},",
        "\"profile\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
        "\"fast_mode\":{\"kind\":\"inherit\"},",
        "\"service_tier\":{\"kind\":\"inherit\"}},",
        "\"global_default\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
        "\"fast_mode\":{\"kind\":\"inherit\"},",
        "\"service_tier\":{\"kind\":\"inherit\"}}},",
        "\"effective\":{\"reasoning_level\":null,\"fast_mode\":\"disabled\",",
        "\"service_tier\":null},\"reasoning_source\":null,",
        "\"fast_mode_source\":null,\"service_tier_source\":null,",
        "\"validated_for_selection_id\":null}"
    );

    fn orchestration_snapshot_fixture(
        state: ReviewOrchestrationState,
        status: ReviewOrchestrationConcernStatus,
        pass_id: Option<CanonicalUuid>,
        counts: ReviewOrchestrationCounts,
    ) -> Result<ReviewOrchestrationSnapshot, Box<dyn std::error::Error>> {
        let digest = CanonicalDigest::try_new("ab".repeat(32))?;
        Ok(ReviewOrchestrationSnapshot {
            attempt_id: uuid(3),
            target_id: uuid(4),
            state,
            concern_set_version: String::from("initial-five"),
            stage_template_digests: ReviewOrchestrationStageTemplateDigests {
                import: digest.clone(),
                judgment: digest.clone(),
                repair: digest.clone(),
                publication: digest.clone(),
            },
            concerns: vec![ReviewOrchestrationConcernSnapshot {
                key: String::from("correctness"),
                template_digest: digest,
                status,
                pass_id,
            }],
            counts,
        })
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
    fn assert_placement_version_mismatch_rejected(expected: u64, current: u64) {
        let error = ServerFrame::try_new(
            request(1).expect("fixture request identity is admitted"),
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("placement version mismatch"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::SessionPlacementCurrentVersionMismatch {
                        session_id: uuid(2),
                        expected_placement_version: CanonicalU64::new(expected),
                        current_placement_version: CanonicalU64::new(current),
                    },
                ),
            },
        )
        .expect_err("incoherent placement mismatch evidence is rejected");
        assert_eq!(error, FrameValidationError::ErrorDetailShape);
    }

    fn placement_version_exhaustion_frame(
        current: u64,
    ) -> Result<ServerFrame, FrameValidationError> {
        ServerFrame::try_new(
            request(1).expect("fixture request identity is admitted"),
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("placement version exhausted"),
                detail: ErrorDetail::rejected(RejectionDetail::SessionPlacementVersionExhausted {
                    session_id: uuid(2),
                    current_placement_version: CanonicalU64::new(current),
                }),
            },
        )
    }

    #[track_caller]
    fn assert_unsupported_version(version: &str) {
        let json = format!(
            "{{\"version\":{version},\"request_id\":\"9\",\"request\":{{\"type\":\"future_request\",\"anything\":true}}}}"
        );
        let error = decode_client_line(&line(&json)).expect_err("version must be unsupported");
        assert_eq!(error.kind(), FrameDecodeErrorKind::UnsupportedVersion);
        assert_eq!(error.request_id().value(), 9);
        assert!(error.to_string().contains("supported version is 1"));
    }

    fn unsupported_version_with_nested_object_payload(payload_depth: usize) -> String {
        let payload = format!(
            "{}0{}",
            r#"{"future":"#.repeat(payload_depth),
            "}".repeat(payload_depth)
        );
        format!("{{\"version\":15,\"request_id\":\"9\",\"request\":{payload}}}")
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
    fn assert_client_request_round_trip(
        request_id: RequestId,
        request: ClientRequest,
        expected_request_json: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let request_id_value = request_id.value();
        let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request)?;
        let encoded = encode_client_line(&frame)?;
        let expected = format!(
            "{{\"version\":{PROTOCOL_VERSION},\"request_id\":\"{request_id_value}\",\"request\":{expected_request_json}}}\n"
        );
        assert_eq!(String::from_utf8(encoded.clone())?, expected);
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    #[track_caller]
    fn assert_server_message_round_trip(
        request_id: RequestId,
        message: ServerMessage,
        expected_message_json: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let request_id_value = request_id.value();
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request_id, message)?;
        let encoded = encode_server_line(&frame)?;
        let expected = format!(
            "{{\"version\":{PROTOCOL_VERSION},\"request_id\":\"{request_id_value}\",\"message\":{}}}\n",
            expected_message_json
        );
        assert_eq!(String::from_utf8(encoded.clone())?, expected);
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn operator_status_request_and_rows_round_trip_in_one_closed_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(1)?,
            ClientRequest::ReadOperatorStatus {},
            r#"{"type":"read_operator_status"}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::HeldSlot(Box::new(
                OperatorStatusHeldSlotMessage {
                    dispatch_id: uuid(2),
                    repository: String::from("example/repo"),
                    origin: OperatorStatusHeldSlotOrigin::PullRequest {
                        pull_request_number: CanonicalU64::new(41),
                    },
                    rule_id: String::from("review"),
                    rule_version: CanonicalU64::new(1),
                    singleton_scope: OperatorStatusSingletonScope::PullRequest,
                    singleton_repository: Some(String::from("example/repo")),
                    singleton_pull_request_number: Some(CanonicalU64::new(41)),
                    singleton_stack_root_pull_request_number: None,
                    held_for_seconds: CanonicalU64::new(90),
                    session_ids: vec![uuid(3)],
                    blockers: vec![
                        OperatorStatusHeldSlotBlocker::UndeliveredAction,
                        OperatorStatusHeldSlotBlocker::PursuingGoal,
                    ],
                },
            )))),
            r#"{"type":"operator_status","kind":"held_slot","dispatch_id":"00000000-0000-0000-0000-000000000002","repository":"example/repo","origin":{"kind":"pull_request","pull_request_number":"41"},"rule_id":"review","rule_version":"1","singleton_scope":"pull_request","singleton_repository":"example/repo","singleton_pull_request_number":"41","singleton_stack_root_pull_request_number":null,"held_for_seconds":"90","session_ids":["00000000-0000-0000-0000-000000000003"],"blockers":["undelivered_action","pursuing_goal"]}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::HeldSlot(Box::new(
                OperatorStatusHeldSlotMessage {
                    dispatch_id: uuid(2),
                    repository: String::from("example/repo"),
                    origin: OperatorStatusHeldSlotOrigin::Branch {
                        branch: String::from("main"),
                    },
                    rule_id: String::from("review"),
                    rule_version: CanonicalU64::new(1),
                    singleton_scope: OperatorStatusSingletonScope::Rule,
                    singleton_repository: None,
                    singleton_pull_request_number: None,
                    singleton_stack_root_pull_request_number: None,
                    held_for_seconds: CanonicalU64::new(90),
                    session_ids: vec![uuid(3)],
                    blockers: Vec::new(),
                },
            )))),
            r#"{"type":"operator_status","kind":"held_slot","dispatch_id":"00000000-0000-0000-0000-000000000002","repository":"example/repo","origin":{"kind":"branch","branch":"main"},"rule_id":"review","rule_version":"1","singleton_scope":"rule","singleton_repository":null,"singleton_pull_request_number":null,"singleton_stack_root_pull_request_number":null,"held_for_seconds":"90","session_ids":["00000000-0000-0000-0000-000000000003"],"blockers":[]}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::QueuedObligation(
                Box::new(OperatorStatusQueuedObligationMessage {
                    obligation_id: uuid(4),
                    repository: String::from("example/repo"),
                    rule_id: String::from("review"),
                    rule_version: CanonicalU64::new(1),
                    singleton_scope: OperatorStatusSingletonScope::Rule,
                    singleton_repository: None,
                    singleton_pull_request_number: None,
                    singleton_stack_root_pull_request_number: None,
                    first_event_id: uuid(5),
                    latest_event_id: uuid(6),
                    matched_event_count: CanonicalU64::new(3),
                    waiting_for_seconds: CanonicalU64::new(45),
                    occupying_dispatch_id: None,
                    occupying_session_ids: Vec::new(),
                    cooldown_remaining_seconds: Some(CanonicalU64::new(15)),
                    cooldown_never_eligible: false,
                    ready: false,
                }),
            ))),
            r#"{"type":"operator_status","kind":"queued_obligation","obligation_id":"00000000-0000-0000-0000-000000000004","repository":"example/repo","rule_id":"review","rule_version":"1","singleton_scope":"rule","singleton_repository":null,"singleton_pull_request_number":null,"singleton_stack_root_pull_request_number":null,"first_event_id":"00000000-0000-0000-0000-000000000005","latest_event_id":"00000000-0000-0000-0000-000000000006","matched_event_count":"3","waiting_for_seconds":"45","occupying_dispatch_id":null,"occupying_session_ids":[],"cooldown_remaining_seconds":"15","cooldown_never_eligible":false,"ready":false}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::PullRequestConvergence(
                Box::new(OperatorStatusPullRequestConvergenceMessage {
                    repository: String::from("example/repo"),
                    pull_request_number: CanonicalU64::new(41),
                    head_sha: String::from("1111111111111111111111111111111111111111"),
                    base_branch: String::from("main"),
                    base_revision: String::from("2222222222222222222222222222222222222222"),
                    mergeable_state: OperatorStatusMergeableState::Mergeable,
                    review_decision: OperatorStatusReviewDecision::Approved,
                    unresolved_thread_count: CanonicalU64::new(0),
                    gating_check_count: CanonicalU64::new(2),
                    non_green_gating_checks: Vec::new(),
                    verdict: OperatorStatusConvergenceVerdict::MergeReady,
                    seal: Some(OperatorStatusConvergenceSeal::MergeReady),
                    assessed_seconds_ago: CanonicalU64::new(12),
                }),
            ))),
            r#"{"type":"operator_status","kind":"pull_request_convergence","repository":"example/repo","pull_request_number":"41","head_sha":"1111111111111111111111111111111111111111","base_branch":"main","base_revision":"2222222222222222222222222222222222222222","mergeable_state":"mergeable","review_decision":"approved","unresolved_thread_count":"0","gating_check_count":"2","non_green_gating_checks":[],"verdict":"merge_ready","seal":"merge_ready","assessed_seconds_ago":"12"}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::PendingStaleReviewClearance(Box::new(
                    OperatorStatusPendingStaleReviewClearanceMessage {
                        repository: String::from("example/repo"),
                        pull_request_number: CanonicalU64::new(41),
                        current_head_sha: String::from("1111111111111111111111111111111111111111"),
                        review_node_id: String::from("PRR_node"),
                        reviewer: String::from("reviewer"),
                        reviewed_head_sha: String::from("3333333333333333333333333333333333333333"),
                        pending_for_seconds: CanonicalU64::new(8),
                    },
                )),
            )),
            r#"{"type":"operator_status","kind":"pending_stale_review_clearance","repository":"example/repo","pull_request_number":"41","current_head_sha":"1111111111111111111111111111111111111111","review_node_id":"PRR_node","reviewer":"reviewer","reviewed_head_sha":"3333333333333333333333333333333333333333","pending_for_seconds":"8"}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::LifecycleWeek(
                Box::new(OperatorStatusLifecycleWeekMessage {
                    week_start_date: String::from("2026-08-31"),
                    completion_failure_numerator: CanonicalU64::new(3),
                    completion_failure_denominator: CanonicalU64::new(40),
                    failed_unknown_count: CanonicalU64::new(1),
                    overflow_numerator: CanonicalU64::new(5),
                    overflow_denominator: CanonicalU64::new(44),
                    finish_given_overflow_numerator: CanonicalU64::new(4),
                    wall_numerator: CanonicalU64::new(0),
                    wall_denominator: CanonicalU64::new(38),
                    wall_occurrence_count: CanonicalU64::new(0),
                    classified_terminal_turn_count: CanonicalU64::new(980),
                    terminal_turn_count: CanonicalU64::new(985),
                    classified_known_failed_call_count: CanonicalU64::new(91),
                    known_failed_call_count: CanonicalU64::new(95),
                }),
            ))),
            r#"{"type":"operator_status","kind":"lifecycle_week","week_start_date":"2026-08-31","completion_failure_numerator":"3","completion_failure_denominator":"40","failed_unknown_count":"1","overflow_numerator":"5","overflow_denominator":"44","finish_given_overflow_numerator":"4","wall_numerator":"0","wall_denominator":"38","wall_occurrence_count":"0","classified_terminal_turn_count":"980","terminal_turn_count":"985","classified_known_failed_call_count":"91","known_failed_call_count":"95"}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::LifecycleDeadlineViolation(Box::new(
                    OperatorStatusLifecycleDeadlineViolationMessage {
                        session_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(0x2a)),
                        state: OperatorStatusLifecycleState::Parked,
                        deadline_missing: false,
                        expired_for_seconds: Some(CanonicalU64::new(90)),
                    },
                )),
            )),
            r#"{"type":"operator_status","kind":"lifecycle_deadline_violation","session_id":"00000000-0000-0000-0000-00000000002a","state":"parked","deadline_missing":false,"expired_for_seconds":"90"}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::End(Box::new(
                OperatorStatusEndMessage {
                    held_slot_count: CanonicalU64::new(1),
                    queued_obligation_count: CanonicalU64::new(1),
                    pull_request_convergence_count: CanonicalU64::new(1),
                    pending_stale_review_clearance_count: CanonicalU64::new(1),
                    lifecycle_week_count: CanonicalU64::new(1),
                    lifecycle_deadline_violation_count: CanonicalU64::new(1),
                },
            )))),
            r#"{"type":"operator_status","kind":"end","held_slot_count":"1","queued_obligation_count":"1","pull_request_convergence_count":"1","pending_stale_review_clearance_count":"1","lifecycle_week_count":"1","lifecycle_deadline_violation_count":"1"}"#,
        )?;
        Ok(())
    }

    /// A metric row whose numerator exceeds its own population is not a rate.
    #[test]
    fn operator_status_rejects_a_lifecycle_week_that_is_not_a_rate()
    -> Result<(), Box<dyn std::error::Error>> {
        let impossible = ServerFrame::try_new(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::LifecycleWeek(
                Box::new(OperatorStatusLifecycleWeekMessage {
                    week_start_date: String::from("2026-08-31"),
                    completion_failure_numerator: CanonicalU64::new(41),
                    completion_failure_denominator: CanonicalU64::new(40),
                    failed_unknown_count: CanonicalU64::new(0),
                    overflow_numerator: CanonicalU64::new(0),
                    overflow_denominator: CanonicalU64::new(44),
                    finish_given_overflow_numerator: CanonicalU64::new(0),
                    wall_numerator: CanonicalU64::new(0),
                    wall_denominator: CanonicalU64::new(38),
                    wall_occurrence_count: CanonicalU64::new(0),
                    classified_terminal_turn_count: CanonicalU64::new(0),
                    terminal_turn_count: CanonicalU64::new(0),
                    classified_known_failed_call_count: CanonicalU64::new(0),
                    known_failed_call_count: CanonicalU64::new(0),
                }),
            ))),
        );
        assert!(matches!(
            impossible,
            Err(FrameValidationError::OperatorStatusShape)
        ));

        Ok(())
    }

    /// A session with no armed record has no expiry to be past, so the two
    /// fields cannot both speak.
    #[test]
    fn operator_status_rejects_a_deadline_violation_that_contradicts_itself()
    -> Result<(), Box<dyn std::error::Error>> {
        let contradictory_deadline = ServerFrame::try_new(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::LifecycleDeadlineViolation(Box::new(
                    OperatorStatusLifecycleDeadlineViolationMessage {
                        session_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(0x2a)),
                        state: OperatorStatusLifecycleState::Parked,
                        deadline_missing: true,
                        expired_for_seconds: Some(CanonicalU64::new(90)),
                    },
                )),
            )),
        );

        assert!(matches!(
            contradictory_deadline,
            Err(FrameValidationError::OperatorStatusShape)
        ));
        Ok(())
    }

    /// A digit-shaped value that names no day is not a week label.
    #[test]
    fn operator_status_rejects_a_week_label_that_names_no_day() {
        assert!(operator_status_calendar_date_is_valid("2026-08-31"));
        assert!(operator_status_calendar_date_is_valid("2024-02-29"));
        assert!(!operator_status_calendar_date_is_valid("2026-99-99"));
        assert!(!operator_status_calendar_date_is_valid("2026-02-29"));
        assert!(!operator_status_calendar_date_is_valid("2026-8-31"));
        assert!(!operator_status_calendar_date_is_valid("+026-08-31"));
        assert!(!operator_status_calendar_date_is_valid("2026-+8-31"));
        assert!(!operator_status_calendar_date_is_valid(
            "2026-08-31T00:00:00Z"
        ));
    }

    #[test]
    fn operator_status_rejects_contradictory_singletons_and_ready_waits()
    -> Result<(), Box<dyn std::error::Error>> {
        let invalid_singleton = ServerFrame::try_new(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::HeldSlot(Box::new(
                OperatorStatusHeldSlotMessage {
                    dispatch_id: uuid(2),
                    repository: String::from("example/repo"),
                    origin: OperatorStatusHeldSlotOrigin::PullRequest {
                        pull_request_number: CanonicalU64::new(41),
                    },
                    rule_id: String::from("review"),
                    rule_version: CanonicalU64::new(1),
                    singleton_scope: OperatorStatusSingletonScope::Rule,
                    singleton_repository: Some(String::from("example/repo")),
                    singleton_pull_request_number: None,
                    singleton_stack_root_pull_request_number: None,
                    held_for_seconds: CanonicalU64::new(1),
                    session_ids: vec![uuid(3)],
                    blockers: Vec::new(),
                },
            )))),
        );
        assert_eq!(
            invalid_singleton,
            Err(FrameValidationError::OperatorStatusShape)
        );

        let invalid_ready = ServerFrame::try_new(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::QueuedObligation(
                Box::new(OperatorStatusQueuedObligationMessage {
                    obligation_id: uuid(4),
                    repository: String::from("example/repo"),
                    rule_id: String::from("review"),
                    rule_version: CanonicalU64::new(1),
                    singleton_scope: OperatorStatusSingletonScope::Rule,
                    singleton_repository: None,
                    singleton_pull_request_number: None,
                    singleton_stack_root_pull_request_number: None,
                    first_event_id: uuid(5),
                    latest_event_id: uuid(6),
                    matched_event_count: CanonicalU64::new(2),
                    waiting_for_seconds: CanonicalU64::new(1),
                    occupying_dispatch_id: None,
                    occupying_session_ids: Vec::new(),
                    cooldown_remaining_seconds: Some(CanonicalU64::new(1)),
                    cooldown_never_eligible: false,
                    ready: true,
                }),
            ))),
        );
        assert_eq!(
            invalid_ready,
            Err(FrameValidationError::OperatorStatusShape)
        );
        Ok(())
    }

    /// An obligation blocked by an independently commissioned live session
    /// names exactly that one session and no dispatch. Both a dispatch identity
    /// owning no sessions and a dispatch-less occupant naming a second session
    /// contradict the projection, which retains a single external blocker.
    #[test]
    fn operator_status_admits_an_external_blocker_without_a_dispatch_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let externally_blocked = |dispatch, sessions, ready| {
            ServerFrame::try_new(
                request(1).expect("a valid request identity"),
                ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::QueuedObligation(
                    Box::new(OperatorStatusQueuedObligationMessage {
                        obligation_id: uuid(4),
                        repository: String::from("example/repo"),
                        rule_id: String::from("review"),
                        rule_version: CanonicalU64::new(1),
                        singleton_scope: OperatorStatusSingletonScope::Rule,
                        singleton_repository: None,
                        singleton_pull_request_number: None,
                        singleton_stack_root_pull_request_number: None,
                        first_event_id: uuid(5),
                        latest_event_id: uuid(6),
                        matched_event_count: CanonicalU64::new(2),
                        waiting_for_seconds: CanonicalU64::new(1),
                        occupying_dispatch_id: dispatch,
                        occupying_session_ids: sessions,
                        cooldown_remaining_seconds: None,
                        cooldown_never_eligible: false,
                        ready,
                    }),
                ))),
            )
        };

        assert!(externally_blocked(None, vec![uuid(7)], false).is_ok());
        assert_eq!(
            externally_blocked(None, vec![uuid(7)], true),
            Err(FrameValidationError::OperatorStatusShape)
        );
        assert_eq!(
            externally_blocked(None, vec![uuid(7), uuid(9)], false),
            Err(FrameValidationError::OperatorStatusShape)
        );
        assert_eq!(
            externally_blocked(Some(uuid(8)), Vec::new(), false),
            Err(FrameValidationError::OperatorStatusShape)
        );
        assert!(externally_blocked(Some(uuid(8)), vec![uuid(7)], false).is_ok());
        assert!(externally_blocked(Some(uuid(8)), vec![uuid(7), uuid(9)], false).is_ok());
        Ok(())
    }

    /// A rule matching branch workflow-run completion holds its singleton slot
    /// from a branch fact, which names a branch and never a pull request. That
    /// fact carries no pull request for a singleton to be keyed by, so a branch
    /// origin admits only the rule and repository scopes. A pull-request- or
    /// stack-scoped branch hold passes both field validators on its own yet
    /// names a slot no branch workflow event could ever have taken, so the
    /// origin and the singleton scope are validated together.
    #[test]
    fn operator_status_admits_a_branch_origin_held_slot() -> Result<(), Box<dyn std::error::Error>>
    {
        let held = |origin,
                    singleton_scope,
                    repository: Option<&str>,
                    pull_request: Option<u64>,
                    stack_root: Option<u64>| {
            ServerFrame::try_new(
                request(1).expect("a valid request identity"),
                ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::HeldSlot(Box::new(
                    OperatorStatusHeldSlotMessage {
                        dispatch_id: uuid(2),
                        repository: String::from("example/repo"),
                        origin,
                        rule_id: String::from("review"),
                        rule_version: CanonicalU64::new(1),
                        singleton_scope,
                        singleton_repository: repository.map(String::from),
                        singleton_pull_request_number: pull_request.map(CanonicalU64::new),
                        singleton_stack_root_pull_request_number: stack_root.map(CanonicalU64::new),
                        held_for_seconds: CanonicalU64::new(1),
                        session_ids: vec![uuid(3)],
                        blockers: Vec::new(),
                    },
                )))),
            )
        };
        let branch = || OperatorStatusHeldSlotOrigin::Branch {
            branch: String::from("main"),
        };
        let pull_request = || OperatorStatusHeldSlotOrigin::PullRequest {
            pull_request_number: CanonicalU64::new(41),
        };

        assert!(
            held(
                branch(),
                OperatorStatusSingletonScope::Rule,
                None,
                None,
                None
            )
            .is_ok()
        );
        assert!(
            held(
                branch(),
                OperatorStatusSingletonScope::Repo,
                Some("example/repo"),
                None,
                None
            )
            .is_ok()
        );
        assert_eq!(
            held(
                branch(),
                OperatorStatusSingletonScope::PullRequest,
                Some("example/repo"),
                Some(41),
                None
            ),
            Err(FrameValidationError::OperatorStatusShape)
        );
        assert_eq!(
            held(
                branch(),
                OperatorStatusSingletonScope::Stack,
                Some("example/repo"),
                None,
                Some(41)
            ),
            Err(FrameValidationError::OperatorStatusShape)
        );

        // Every admitted origin other than a branch fact names a pull request,
        // which keys any of the four singleton scopes.
        assert!(
            held(
                pull_request(),
                OperatorStatusSingletonScope::PullRequest,
                Some("example/repo"),
                Some(41),
                None
            )
            .is_ok()
        );
        assert!(
            held(
                pull_request(),
                OperatorStatusSingletonScope::Stack,
                Some("example/repo"),
                None,
                Some(41)
            )
            .is_ok()
        );
        assert!(
            held(
                pull_request(),
                OperatorStatusSingletonScope::Rule,
                None,
                None,
                None
            )
            .is_ok()
        );
        assert!(
            held(
                pull_request(),
                OperatorStatusSingletonScope::Repo,
                Some("example/repo"),
                None,
                None
            )
            .is_ok()
        );

        assert_eq!(
            held(
                OperatorStatusHeldSlotOrigin::Branch {
                    branch: String::new(),
                },
                OperatorStatusSingletonScope::Rule,
                None,
                None,
                None
            ),
            Err(FrameValidationError::OperatorStatusShape)
        );
        assert_eq!(
            held(
                OperatorStatusHeldSlotOrigin::PullRequest {
                    pull_request_number: CanonicalU64::new(0),
                },
                OperatorStatusSingletonScope::Rule,
                None,
                None,
                None
            ),
            Err(FrameValidationError::OperatorStatusShape)
        );
        Ok(())
    }

    #[test]
    fn operator_status_allows_a_seal_to_outlive_the_latest_assessment()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::PullRequestConvergence(
                Box::new(OperatorStatusPullRequestConvergenceMessage {
                    repository: String::from("example/repo"),
                    pull_request_number: CanonicalU64::new(41),
                    head_sha: String::from("1111111111111111111111111111111111111111"),
                    base_branch: String::from("main"),
                    base_revision: String::from("2222222222222222222222222222222222222222"),
                    mergeable_state: OperatorStatusMergeableState::Mergeable,
                    review_decision: OperatorStatusReviewDecision::Approved,
                    unresolved_thread_count: CanonicalU64::new(0),
                    gating_check_count: CanonicalU64::new(1),
                    non_green_gating_checks: vec![String::from("rust-checks")],
                    verdict: OperatorStatusConvergenceVerdict::NotConverged,
                    seal: Some(OperatorStatusConvergenceSeal::MergeReady),
                    assessed_seconds_ago: CanonicalU64::new(1),
                }),
            ))),
        );

        assert!(frame.is_ok());
        Ok(())
    }

    /// One merge-ready convergence row: every carried condition clean, beside
    /// the trunk base branch the durable side pairs that verdict with. Each
    /// case below restates only the field whose contradiction it names.
    fn merge_ready_convergence() -> OperatorStatusPullRequestConvergenceMessage {
        OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("example/repo"),
            pull_request_number: CanonicalU64::new(41),
            head_sha: String::from("1111111111111111111111111111111111111111"),
            base_branch: String::from("main"),
            base_revision: String::from("2222222222222222222222222222222222222222"),
            mergeable_state: OperatorStatusMergeableState::Mergeable,
            review_decision: OperatorStatusReviewDecision::Approved,
            unresolved_thread_count: CanonicalU64::new(0),
            gating_check_count: CanonicalU64::new(2),
            non_green_gating_checks: Vec::new(),
            verdict: OperatorStatusConvergenceVerdict::MergeReady,
            seal: None,
            assessed_seconds_ago: CanonicalU64::new(1),
        }
    }

    /// The same clean evidence beside the release base branch and the verdict
    /// the durable side pairs that branch with.
    fn internally_converged_convergence() -> OperatorStatusPullRequestConvergenceMessage {
        OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release/1"),
            verdict: OperatorStatusConvergenceVerdict::InternallyConverged,
            ..merge_ready_convergence()
        }
    }

    /// The same clean evidence beneath the unconverged verdict, which the
    /// durable side pairs with no base branch at all.
    fn not_converged_convergence() -> OperatorStatusPullRequestConvergenceMessage {
        OperatorStatusPullRequestConvergenceMessage {
            verdict: OperatorStatusConvergenceVerdict::NotConverged,
            ..merge_ready_convergence()
        }
    }

    #[track_caller]
    fn assert_convergence_admitted(item: OperatorStatusPullRequestConvergenceMessage) {
        let frame = ServerFrame::try_new(
            request(1).expect("a valid request identity"),
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::PullRequestConvergence(
                Box::new(item),
            ))),
        );
        assert!(frame.is_ok(), "convergence row must be admitted: {frame:?}");
    }

    #[track_caller]
    fn assert_convergence_rejected(item: OperatorStatusPullRequestConvergenceMessage) {
        let frame = ServerFrame::try_new(
            request(1).expect("a valid request identity"),
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::PullRequestConvergence(
                Box::new(item),
            ))),
        );
        assert_eq!(frame, Err(FrameValidationError::OperatorStatusShape));
    }

    /// A convergence row's verdict is settled by the evidence beside it. The
    /// durable `repo_watch_convergence_verdict_matches_evidence` constraint
    /// makes the unconverged verdict exactly the carried-blocker case, so
    /// either converged verdict is admitted only beside wholly clean evidence:
    /// no unresolved thread, no non-green check, a mergeable provider state, at
    /// least one gating check, and no requested change.
    #[test]
    fn operator_status_admits_a_converged_verdict_beside_clean_evidence() {
        assert_convergence_admitted(merge_ready_convergence());
        assert_convergence_admitted(internally_converged_convergence());
    }

    /// One rejection per contradiction class the wire carries: an empty gating
    /// inventory, an unresolved thread, a non-green check, each unmergeable
    /// provider state, and a requested change.
    #[test]
    fn operator_status_rejects_a_merge_ready_verdict_beside_each_contradiction() {
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            gating_check_count: CanonicalU64::new(0),
            ..merge_ready_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            unresolved_thread_count: CanonicalU64::new(1),
            ..merge_ready_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            non_green_gating_checks: vec![String::from("rust-checks")],
            ..merge_ready_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            mergeable_state: OperatorStatusMergeableState::Conflicting,
            ..merge_ready_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            mergeable_state: OperatorStatusMergeableState::Unknown,
            ..merge_ready_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            review_decision: OperatorStatusReviewDecision::ChangesRequested,
            ..merge_ready_convergence()
        });
    }

    /// The same contradiction classes refuse the other converged verdict, which
    /// the durable evidence constraint treats identically.
    #[test]
    fn operator_status_rejects_an_internally_converged_verdict_beside_each_contradiction() {
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            gating_check_count: CanonicalU64::new(0),
            ..internally_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            unresolved_thread_count: CanonicalU64::new(1),
            ..internally_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            non_green_gating_checks: vec![String::from("rust-checks")],
            ..internally_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            mergeable_state: OperatorStatusMergeableState::Conflicting,
            ..internally_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            mergeable_state: OperatorStatusMergeableState::Unknown,
            ..internally_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            review_decision: OperatorStatusReviewDecision::ChangesRequested,
            ..internally_converged_convergence()
        });
    }

    /// A review still awaiting its first decision blocks neither converged
    /// verdict, since only a requested change is a durable blocker.
    #[test]
    fn operator_status_admits_a_converged_verdict_beside_an_undecided_review() {
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            review_decision: OperatorStatusReviewDecision::None,
            ..merge_ready_convergence()
        });
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            review_decision: OperatorStatusReviewDecision::ReviewRequired,
            ..merge_ready_convergence()
        });
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            review_decision: OperatorStatusReviewDecision::None,
            ..internally_converged_convergence()
        });
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            review_decision: OperatorStatusReviewDecision::ReviewRequired,
            ..internally_converged_convergence()
        });
    }

    /// The unconverged verdict carries its own blockers freely and stays
    /// admissible beside wholly clean evidence, because the unsettled provider
    /// snapshot that alone justifies the latter never crosses this wire.
    #[test]
    fn operator_status_admits_an_unconverged_verdict_beside_any_carried_evidence() {
        assert_convergence_admitted(not_converged_convergence());
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            mergeable_state: OperatorStatusMergeableState::Conflicting,
            review_decision: OperatorStatusReviewDecision::ChangesRequested,
            unresolved_thread_count: CanonicalU64::new(3),
            non_green_gating_checks: vec![String::from("rust-checks")],
            ..not_converged_convergence()
        });
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            gating_check_count: CanonicalU64::new(0),
            ..not_converged_convergence()
        });
    }

    /// Two durable constraints sit beside the evidence constraint on the same
    /// assessment row, and the status projection reads the verdict and the base
    /// branch from that one row: a merge-ready verdict is settled only against
    /// `main`, an internally-converged verdict only against another branch. The
    /// pair is what separates the two converged verdicts, so each is refused on
    /// the other's branch even beside wholly clean evidence.
    #[test]
    fn operator_status_binds_each_converged_verdict_to_its_base_branch() {
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release/1"),
            ..merge_ready_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("main"),
            ..internally_converged_convergence()
        });

        // The unconverged verdict is settled without consulting the base
        // branch, so it is admitted on either one.
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("main"),
            ..not_converged_convergence()
        });
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release/1"),
            ..not_converged_convergence()
        });

        // A seal is retained from the assessment that earned it and outlives
        // later ones, so a pull request retargeted after it was sealed carries
        // that merge-ready seal beside a branch no merge-ready verdict could be
        // settled against. The seal therefore takes no base-branch pairing.
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release/1"),
            seal: Some(OperatorStatusConvergenceSeal::MergeReady),
            ..not_converged_convergence()
        });
    }

    /// The base branch is a git ref name on the wire, so the grammar the
    /// `BranchName` constructor and the durable `repo_watch_branch_is_valid`
    /// check share is mirrored here rather than a bare length bound.
    #[test]
    fn operator_status_rejects_a_convergence_base_branch_outside_the_ref_grammar() {
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from(".."),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release branch"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("-release"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release/"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release/.hidden"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release/1.lock"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release@{1}"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release^1"),
            ..not_converged_convergence()
        });

        // A slashed, dotted, and hyphenated name is an ordinary ref name.
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            base_branch: String::from("release/v1.2-rc"),
            ..not_converged_convergence()
        });
    }

    /// A convergence row's repository is a canonical `namespace/name` slug on the
    /// wire, so the grammar the `RepositorySlug` constructor and the durable
    /// `repo_watch_repository_is_valid` check share is mirrored here. Both
    /// producers lowercase what they admit, so an uppercase spelling is refused
    /// with the malformed ones.
    #[test]
    fn operator_status_rejects_a_repository_outside_the_slug_grammar() {
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("not-a-slug"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("example/repo/extra"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("example/"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("/repo"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("example/.."),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("example/re po"),
            ..not_converged_convergence()
        });
        assert_convergence_rejected(OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("Example/Repo"),
            ..not_converged_convergence()
        });

        // Dots, hyphens, and underscores spell an ordinary slug on both sides.
        assert_convergence_admitted(OperatorStatusPullRequestConvergenceMessage {
            repository: String::from("ex-am_ple/re.po-1"),
            ..not_converged_convergence()
        });
    }

    /// One held slot whose pull-request origin, singleton scope, and singleton
    /// axes all name the same pull request in the same repository, which is the
    /// only identity the projection can produce for a pull-request-scoped hold.
    fn held_slot_row() -> OperatorStatusHeldSlotMessage {
        OperatorStatusHeldSlotMessage {
            dispatch_id: uuid(2),
            repository: String::from("example/repo"),
            origin: OperatorStatusHeldSlotOrigin::PullRequest {
                pull_request_number: CanonicalU64::new(41),
            },
            rule_id: String::from("review"),
            rule_version: CanonicalU64::new(1),
            singleton_scope: OperatorStatusSingletonScope::PullRequest,
            singleton_repository: Some(String::from("example/repo")),
            singleton_pull_request_number: Some(CanonicalU64::new(41)),
            singleton_stack_root_pull_request_number: None,
            held_for_seconds: CanonicalU64::new(90),
            session_ids: vec![uuid(3)],
            blockers: Vec::new(),
        }
    }

    #[track_caller]
    fn assert_held_slot_admitted(item: OperatorStatusHeldSlotMessage) {
        let frame = ServerFrame::try_new(
            request(1).expect("a valid request identity"),
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::HeldSlot(Box::new(
                item,
            )))),
        );
        assert!(frame.is_ok(), "held-slot row must be admitted: {frame:?}");
    }

    #[track_caller]
    fn assert_held_slot_rejected(item: OperatorStatusHeldSlotMessage) {
        let frame = ServerFrame::try_new(
            request(1).expect("a valid request identity"),
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::HeldSlot(Box::new(
                item,
            )))),
        );
        assert_eq!(frame, Err(FrameValidationError::OperatorStatusShape));
    }

    /// One obligation owed by a rule-scoped singleton, opened by a single
    /// matched event and still waiting behind nothing in particular.
    fn queued_obligation_row() -> OperatorStatusQueuedObligationMessage {
        OperatorStatusQueuedObligationMessage {
            obligation_id: uuid(4),
            repository: String::from("example/repo"),
            rule_id: String::from("review"),
            rule_version: CanonicalU64::new(1),
            singleton_scope: OperatorStatusSingletonScope::Rule,
            singleton_repository: None,
            singleton_pull_request_number: None,
            singleton_stack_root_pull_request_number: None,
            first_event_id: uuid(5),
            latest_event_id: uuid(5),
            matched_event_count: CanonicalU64::new(1),
            waiting_for_seconds: CanonicalU64::new(45),
            occupying_dispatch_id: None,
            occupying_session_ids: Vec::new(),
            cooldown_remaining_seconds: None,
            cooldown_never_eligible: false,
            ready: false,
        }
    }

    #[track_caller]
    fn assert_obligation_admitted(item: OperatorStatusQueuedObligationMessage) {
        let frame = ServerFrame::try_new(
            request(1).expect("a valid request identity"),
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::QueuedObligation(
                Box::new(item),
            ))),
        );
        assert!(frame.is_ok(), "obligation row must be admitted: {frame:?}");
    }

    #[track_caller]
    fn assert_obligation_rejected(item: OperatorStatusQueuedObligationMessage) {
        let frame = ServerFrame::try_new(
            request(1).expect("a valid request identity"),
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::QueuedObligation(
                Box::new(item),
            ))),
        );
        assert_eq!(frame, Err(FrameValidationError::OperatorStatusShape));
    }

    /// One stale blocking review whose planned clearance is still unsettled.
    fn stale_review_clearance_row() -> OperatorStatusPendingStaleReviewClearanceMessage {
        OperatorStatusPendingStaleReviewClearanceMessage {
            repository: String::from("example/repo"),
            pull_request_number: CanonicalU64::new(41),
            current_head_sha: String::from("1111111111111111111111111111111111111111"),
            review_node_id: String::from("PRR_node"),
            reviewer: String::from("reviewer"),
            reviewed_head_sha: String::from("3333333333333333333333333333333333333333"),
            pending_for_seconds: CanonicalU64::new(8),
        }
    }

    #[track_caller]
    fn assert_stale_review_clearance_admitted(
        item: OperatorStatusPendingStaleReviewClearanceMessage,
    ) {
        let frame = ServerFrame::try_new(
            request(1).expect("a valid request identity"),
            ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::PendingStaleReviewClearance(Box::new(item)),
            )),
        );
        assert!(
            frame.is_ok(),
            "stale-review clearance row must be admitted: {frame:?}"
        );
    }

    #[track_caller]
    fn assert_stale_review_clearance_rejected(
        item: OperatorStatusPendingStaleReviewClearanceMessage,
    ) {
        let frame = ServerFrame::try_new(
            request(1).expect("a valid request identity"),
            ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::PendingStaleReviewClearance(Box::new(item)),
            )),
        );
        assert_eq!(frame, Err(FrameValidationError::OperatorStatusShape));
    }

    /// The held-slot projection joins each dispatch batch to the very event it
    /// was admitted from and reads the origin pull request off that row, while
    /// the batch's singleton was keyed from the same event. A pull-request
    /// singleton therefore names the very pull request its origin names, and a
    /// row naming two different ones is an identity persistence cannot hold.
    ///
    /// A stack singleton names the root of the open component the origin
    /// belongs to, which is a different pull request whenever the origin is not
    /// itself that root, so the stack axis takes no such equality.
    #[test]
    fn operator_status_binds_a_held_pull_request_singleton_to_its_origin() {
        assert_held_slot_admitted(held_slot_row());
        assert_held_slot_rejected(OperatorStatusHeldSlotMessage {
            singleton_pull_request_number: Some(CanonicalU64::new(42)),
            ..held_slot_row()
        });

        assert_held_slot_admitted(OperatorStatusHeldSlotMessage {
            singleton_scope: OperatorStatusSingletonScope::Stack,
            singleton_pull_request_number: None,
            singleton_stack_root_pull_request_number: Some(CanonicalU64::new(7)),
            ..held_slot_row()
        });
    }

    /// Every repository-keyed singleton is keyed from the repository of the
    /// very event whose row carries it, and an obligation coalesces only across
    /// events sharing its singleton key, so a carried singleton repository is
    /// the row's own repository and never an independent slug.
    #[test]
    fn operator_status_binds_a_singleton_repository_to_its_row_repository() {
        assert_held_slot_rejected(OperatorStatusHeldSlotMessage {
            singleton_repository: Some(String::from("other/repo")),
            ..held_slot_row()
        });
        assert_obligation_rejected(OperatorStatusQueuedObligationMessage {
            singleton_scope: OperatorStatusSingletonScope::Repo,
            singleton_repository: Some(String::from("other/repo")),
            ..queued_obligation_row()
        });
        assert_obligation_admitted(OperatorStatusQueuedObligationMessage {
            singleton_scope: OperatorStatusSingletonScope::Repo,
            singleton_repository: Some(String::from("example/repo")),
            ..queued_obligation_row()
        });

        // A rule-scoped obligation legitimately spans repositories and carries
        // no singleton repository at all, so its own repository stands alone.
        assert_obligation_admitted(queued_obligation_row());
    }

    /// Persistence opens an obligation naming one evaluated event as both its
    /// first and its latest with a matched count of one, and every later
    /// coalesced evaluation replaces the latest with a distinct event and
    /// increments the count. The count therefore stands at one exactly while
    /// the two endpoints are the same event.
    #[test]
    fn operator_status_binds_the_matched_event_count_to_its_endpoints() {
        assert_obligation_admitted(queued_obligation_row());
        assert_obligation_rejected(OperatorStatusQueuedObligationMessage {
            latest_event_id: uuid(6),
            ..queued_obligation_row()
        });
        assert_obligation_admitted(OperatorStatusQueuedObligationMessage {
            latest_event_id: uuid(6),
            matched_event_count: CanonicalU64::new(2),
            ..queued_obligation_row()
        });
        assert_obligation_rejected(OperatorStatusQueuedObligationMessage {
            matched_event_count: CanonicalU64::new(2),
            ..queued_obligation_row()
        });
        assert_obligation_rejected(OperatorStatusQueuedObligationMessage {
            matched_event_count: CanonicalU64::new(0),
            ..queued_obligation_row()
        });
    }

    /// The projection reports a remaining cooldown only while the eligibility
    /// instant is still ahead of the read, and rounds that strictly positive
    /// interval up, so the smallest value it can carry is one second. A zero
    /// names a cooldown that has already lapsed while still claiming to
    /// withhold the obligation, and an infinite eligibility is carried as the
    /// never-eligible flag rather than as any number at all.
    #[test]
    fn operator_status_rejects_a_zero_remaining_cooldown() {
        assert_obligation_rejected(OperatorStatusQueuedObligationMessage {
            cooldown_remaining_seconds: Some(CanonicalU64::new(0)),
            ..queued_obligation_row()
        });
        assert_obligation_admitted(OperatorStatusQueuedObligationMessage {
            cooldown_remaining_seconds: Some(CanonicalU64::new(1)),
            ..queued_obligation_row()
        });
        assert_obligation_admitted(OperatorStatusQueuedObligationMessage {
            cooldown_remaining_seconds: None,
            cooldown_never_eligible: true,
            ..queued_obligation_row()
        });
    }

    /// A rule identity is the operator's own spelling, admitted by the
    /// `RepoWatchRuleId` constructor and the durable `repo_watch_rule_id_is_valid`
    /// check as ASCII letters, digits, hyphens, underscores, and dots. Unlike a
    /// slug or a login it is never case-normalized, so both cases are admitted.
    #[test]
    fn operator_status_rejects_a_rule_id_outside_the_identity_grammar() {
        assert_obligation_rejected(OperatorStatusQueuedObligationMessage {
            rule_id: String::from("bad rule"),
            ..queued_obligation_row()
        });
        assert_obligation_rejected(OperatorStatusQueuedObligationMessage {
            rule_id: String::from("rule/one"),
            ..queued_obligation_row()
        });
        assert_obligation_rejected(OperatorStatusQueuedObligationMessage {
            rule_id: String::from("rule:one"),
            ..queued_obligation_row()
        });
        assert_obligation_admitted(OperatorStatusQueuedObligationMessage {
            rule_id: String::from("Review.rule-1_v2"),
            ..queued_obligation_row()
        });
    }

    /// A reviewer login is admitted by the `RepoWatchAuthorLogin` constructor
    /// and the durable `repo_watch_login_is_valid` check: an optional App-bot
    /// suffix is set aside, and the base left behind is nonempty, begins and
    /// ends with something other than a hyphen, carries no doubled hyphen, and
    /// spells itself in lowercase letters, digits, hyphens, and underscores.
    #[test]
    fn operator_status_rejects_a_reviewer_outside_the_login_grammar() {
        assert_stale_review_clearance_rejected(OperatorStatusPendingStaleReviewClearanceMessage {
            reviewer: String::from("-bot"),
            ..stale_review_clearance_row()
        });
        assert_stale_review_clearance_rejected(OperatorStatusPendingStaleReviewClearanceMessage {
            reviewer: String::from("bot-"),
            ..stale_review_clearance_row()
        });
        assert_stale_review_clearance_rejected(OperatorStatusPendingStaleReviewClearanceMessage {
            reviewer: String::from("re--viewer"),
            ..stale_review_clearance_row()
        });
        assert_stale_review_clearance_rejected(OperatorStatusPendingStaleReviewClearanceMessage {
            reviewer: String::from("Reviewer"),
            ..stale_review_clearance_row()
        });
        assert_stale_review_clearance_rejected(OperatorStatusPendingStaleReviewClearanceMessage {
            reviewer: String::from("rev iewer"),
            ..stale_review_clearance_row()
        });
        assert_stale_review_clearance_rejected(OperatorStatusPendingStaleReviewClearanceMessage {
            reviewer: String::from("[bot]"),
            ..stale_review_clearance_row()
        });

        assert_stale_review_clearance_admitted(OperatorStatusPendingStaleReviewClearanceMessage {
            reviewer: String::from("rev_iewer-1"),
            ..stale_review_clearance_row()
        });
        assert_stale_review_clearance_admitted(OperatorStatusPendingStaleReviewClearanceMessage {
            reviewer: String::from("dependabot[bot]"),
            ..stale_review_clearance_row()
        });
    }

    /// A held slot's branch origin is a git ref name on the same grammar the
    /// convergence row's base branch takes, so a malformed spelling is refused
    /// there too rather than passing a bare length bound.
    #[test]
    fn operator_status_rejects_a_held_slot_branch_outside_the_ref_grammar() {
        assert_held_slot_rejected(OperatorStatusHeldSlotMessage {
            origin: OperatorStatusHeldSlotOrigin::Branch {
                branch: String::from("feature branch"),
            },
            singleton_scope: OperatorStatusSingletonScope::Rule,
            singleton_repository: None,
            singleton_pull_request_number: None,
            ..held_slot_row()
        });
        assert_held_slot_rejected(OperatorStatusHeldSlotMessage {
            origin: OperatorStatusHeldSlotOrigin::Branch {
                branch: String::from("feature/.hidden"),
            },
            singleton_scope: OperatorStatusSingletonScope::Rule,
            singleton_repository: None,
            singleton_pull_request_number: None,
            ..held_slot_row()
        });
        assert_held_slot_admitted(OperatorStatusHeldSlotMessage {
            origin: OperatorStatusHeldSlotOrigin::Branch {
                branch: String::from("feature/v1.2-rc"),
            },
            singleton_scope: OperatorStatusSingletonScope::Rule,
            singleton_repository: None,
            singleton_pull_request_number: None,
            ..held_slot_row()
        });
    }

    #[test]
    fn maximum_operator_status_convergence_inventory_fits_one_frame()
    -> Result<(), Box<dyn std::error::Error>> {
        let maximum_check_name = "\u{1}".repeat(256);
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            RequestId::try_new(u64::MAX)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::PullRequestConvergence(
                Box::new(OperatorStatusPullRequestConvergenceMessage {
                    repository: String::from("example/repo"),
                    pull_request_number: CanonicalU64::new(41),
                    head_sha: String::from("1111111111111111111111111111111111111111"),
                    base_branch: String::from("main"),
                    base_revision: String::from("2222222222222222222222222222222222222222"),
                    mergeable_state: OperatorStatusMergeableState::Mergeable,
                    review_decision: OperatorStatusReviewDecision::ChangesRequested,
                    unresolved_thread_count: CanonicalU64::new(10_000),
                    gating_check_count: CanonicalU64::new(10_000),
                    non_green_gating_checks: vec![maximum_check_name; 10_000],
                    verdict: OperatorStatusConvergenceVerdict::NotConverged,
                    seal: None,
                    assessed_seconds_ago: CanonicalU64::new(u64::MAX),
                }),
            ))),
        )?;

        let encoded = encode_server_line(&frame)?;
        assert!(encoded.len() <= super::MAX_FRAME_BYTES);
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_goal_requests_and_history_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(1)?,
            ClientRequest::AttachGoal {
                command_id: command(2)?,
                session_id: uuid(3),
                statement: String::from("ship goal mode"),
            },
            r#"{"type":"attach_goal","command_id":"00000000-0000-0000-0000-000000000002","session_id":"00000000-0000-0000-0000-000000000003","statement":"ship goal mode"}"#,
        )?;
        assert_client_request_round_trip(
            request(4)?,
            ClientRequest::ResumeGoal {
                command_id: command(5)?,
                session_id: uuid(3),
                guidance: Some(String::from("use the user decision")),
            },
            r#"{"type":"resume_goal","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000003","guidance":"use the user decision"}"#,
        )?;
        assert_client_request_round_trip(
            request(6)?,
            ClientRequest::SupersedeGoal {
                command_id: command(7)?,
                session_id: uuid(3),
                statement: String::from("ship clarified goal mode"),
            },
            r#"{"type":"supersede_goal","command_id":"00000000-0000-0000-0000-000000000007","session_id":"00000000-0000-0000-0000-000000000003","statement":"ship clarified goal mode"}"#,
        )?;
        assert_client_request_round_trip(
            request(9)?,
            ClientRequest::StopSession {
                command_id: command(10)?,
                session_id: uuid(3),
                sticky: true,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
            r#"{"type":"stop_session","command_id":"00000000-0000-0000-0000-00000000000a","session_id":"00000000-0000-0000-0000-000000000003","sticky":true,"descendant_scope":"parent_alone"}"#,
        )?;
        assert_client_request_round_trip(
            request(9)?,
            ClientRequest::ReleaseStart {
                command_id: command(14)?,
                session_id: uuid(3),
            },
            r#"{"type":"release_start","command_id":"00000000-0000-0000-0000-00000000000e","session_id":"00000000-0000-0000-0000-000000000003"}"#,
        )?;
        assert_client_request_round_trip(
            request(9)?,
            ClientRequest::CloseSessionFailed {
                command_id: command(11)?,
                session_id: uuid(3),
                cause: None,
            },
            r#"{"type":"close_session_failed","command_id":"00000000-0000-0000-0000-00000000000b","session_id":"00000000-0000-0000-0000-000000000003","cause":null}"#,
        )?;
        assert_client_request_round_trip(
            request(9)?,
            ClientRequest::AdoptSession {
                command_id: command(12)?,
                session_id: uuid(3),
                finish_condition: Some(super::FinishCondition::Declared {
                    statement: String::from("the branch is green"),
                }),
            },
            r#"{"type":"adopt_session","command_id":"00000000-0000-0000-0000-00000000000c","session_id":"00000000-0000-0000-0000-000000000003","finish_condition":{"kind":"declared","statement":"the branch is green"}}"#,
        )?;
        assert_client_request_round_trip(
            request(9)?,
            ClientRequest::AdoptSession {
                command_id: command(13)?,
                session_id: uuid(3),
                finish_condition: Some(super::FinishCondition::ExternalGate),
            },
            r#"{"type":"adopt_session","command_id":"00000000-0000-0000-0000-00000000000d","session_id":"00000000-0000-0000-0000-000000000003","finish_condition":{"kind":"external_gate"}}"#,
        )?;
        assert_server_message_round_trip(
            request(9)?,
            ServerMessage::SessionLifecycleCommandApplied {
                session_id: uuid(3),
                effect: SessionLifecycleEffect::StartReleased {},
            },
            r#"{"type":"session_lifecycle_command_applied","session_id":"00000000-0000-0000-0000-000000000003","effect":{"type":"start_released"}}"#,
        )?;
        assert_server_message_round_trip(
            request(9)?,
            ServerMessage::SessionLifecycleCommandApplied {
                session_id: uuid(3),
                effect: SessionLifecycleEffect::ClosurePending {
                    live_turn_id: uuid(4),
                },
            },
            r#"{"type":"session_lifecycle_command_applied","session_id":"00000000-0000-0000-0000-000000000003","effect":{"type":"closure_pending","live_turn_id":"00000000-0000-0000-0000-000000000004"}}"#,
        )?;
        assert_server_message_round_trip(
            request(8)?,
            ServerMessage::GoalHistoryStart {
                session_id: uuid(3),
                current_generation: CanonicalU64::new(2),
                current_statement: String::from("ship clarified goal mode"),
            },
            r#"{"type":"goal_history_start","session_id":"00000000-0000-0000-0000-000000000003","current_generation":"2","current_statement":"ship clarified goal mode"}"#,
        )?;
        assert_server_message_round_trip(
            request(8)?,
            ServerMessage::GoalHistoryState {
                current_state: GoalLifecycleState::Pursuing {},
            },
            r#"{"type":"goal_history_state","current_state":{"type":"pursuing"}}"#,
        )?;
        assert_server_message_round_trip(
            request(9)?,
            ServerMessage::GoalHistoryItem {
                event_ordinal: CanonicalU64::new(3),
                generation: CanonicalU64::new(2),
                event: GoalHistoryEvent::Blocked {
                    reason: GoalBlockedReason::ExecutionFailure,
                    need: String::from("repair execution"),
                    provenance: GoalBlockedProvenance::ExecutionFailure { turn_id: uuid(10) },
                },
            },
            r#"{"type":"goal_history_item","event_ordinal":"3","generation":"2","event":{"type":"blocked","reason":"execution_failure","need":"repair execution","provenance":{"type":"execution_failure","turn_id":"00000000-0000-0000-0000-00000000000a"}}}"#,
        )?;
        assert_client_request_round_trip(
            request(11)?,
            ClientRequest::ReadGoal {
                session_id: uuid(3),
            },
            r#"{"type":"read_goal","session_id":"00000000-0000-0000-0000-000000000003"}"#,
        )?;
        assert_client_request_round_trip(
            request(12)?,
            ClientRequest::StopGoal {
                command_id: command(13)?,
                session_id: uuid(3),
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
            r#"{"type":"stop_goal","command_id":"00000000-0000-0000-0000-00000000000d","session_id":"00000000-0000-0000-0000-000000000003","descendant_scope":"parent_and_descendants"}"#,
        )?;
        assert_server_message_round_trip(
            request(14)?,
            ServerMessage::GoalTransitionApplied {
                session_id: uuid(3),
                event_ordinal: CanonicalU64::new(4),
                generation: CanonicalU64::new(2),
            },
            r#"{"type":"goal_transition_applied","session_id":"00000000-0000-0000-0000-000000000003","event_ordinal":"4","generation":"2"}"#,
        )?;
        assert_server_message_round_trip(
            request(15)?,
            ServerMessage::GoalHistoryEnd {
                event_count: CanonicalU64::new(4),
            },
            r#"{"type":"goal_history_end","event_count":"4"}"#,
        )?;
        assert_server_message_round_trip(
            request(16)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("goal command rejected"),
                detail: ErrorDetail::rejected(RejectionDetail::GoalCommandRejected {
                    session_id: uuid(3),
                    reason: GoalCommandRejection::AcceptancePositionExhausted,
                }),
            },
            r#"{"type":"error","code":"rejected","message":"goal command rejected","detail":{"type":"goal_command_rejected","session_id":"00000000-0000-0000-0000-000000000003","reason":"acceptance_position_exhausted"}}"#,
        )
    }

    #[test]
    fn inv033_split_goal_projection_fits_maximally_escaped_text_frames()
    -> Result<(), Box<dyn std::error::Error>> {
        let text = "\u{1}".repeat(MAX_CONTENT_FRAGMENT_BYTES);
        let start = ServerFrame::try_new(
            request(1)?,
            ServerMessage::GoalHistoryStart {
                session_id: uuid(2),
                current_generation: CanonicalU64::new(1),
                current_statement: text.clone(),
            },
        )?;
        let state = ServerFrame::try_new(
            request(1)?,
            ServerMessage::GoalHistoryState {
                current_state: GoalLifecycleState::Blocked {
                    reason: GoalBlockedReason::ExternalChangeRequired,
                    need: text,
                },
            },
        )?;
        let start_encoded = encode_server_line(&start)?;
        let state_encoded = encode_server_line(&state)?;

        assert!(start_encoded.len() < super::MAX_FRAME_BYTES);
        assert!(state_encoded.len() < super::MAX_FRAME_BYTES);
        assert_eq!(decode_server_line(&start_encoded)?, start);
        assert_eq!(decode_server_line(&state_encoded)?, state);
        Ok(())
    }

    #[test]
    fn inv033_goal_history_rejects_model_provenance_for_execution_failure() {
        let mismatched = ServerMessage::GoalHistoryItem {
            event_ordinal: CanonicalU64::new(2),
            generation: CanonicalU64::new(1),
            event: GoalHistoryEvent::Blocked {
                reason: GoalBlockedReason::ExecutionFailure,
                need: String::from("repair execution"),
                provenance: GoalBlockedProvenance::Model {
                    turn_id: uuid(3),
                    tool_request_id: uuid(4),
                },
            },
        };

        assert_eq!(
            ServerFrame::try_new(
                RequestId::try_new(1).expect("fixture request identity is admitted"),
                mismatched,
            )
            .expect_err("scheduler-only reason rejects model provenance"),
            FrameValidationError::GoalShape
        );
    }

    #[test]
    fn inv033_client_round_trip_preserves_closed_request_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new(
            request(u64::MAX)?,
            ClientRequest::SubmitInput {
                command_id: command(1)?,
                session_id: uuid(2),
                content: UserInputContent::text("hello".to_owned()),
                expected_defaults_version: Some(CanonicalU64::new(u64::MAX)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
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
        assert_eq!(
            content.parts(),
            &[UserInputPart::Text {
                text: String::from("hello")
            }]
        );
        assert!(String::from_utf8(encoded)?.contains("\"request_id\":\"18446744073709551615\""));
        Ok(())
    }

    /// INV-012 / INV-060: multipart request encoding preserves part order and
    /// every attachment metadata field in the one canonical array shape.
    #[test]
    fn inv012_inv060_multipart_input_wire_is_ordered_and_exact()
    -> Result<(), Box<dyn std::error::Error>> {
        let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            .parse::<CanonicalBlobDigest>()?;
        let content = UserInputContent::from_parts(vec![
            UserInputPart::Text {
                text: String::from("inspect "),
            },
            UserInputPart::Attachment {
                digest,
                kind: UserAttachmentKind::Image,
                media_type: String::from("image/png"),
                display_filename: Some(String::from("chart.png")),
            },
            UserInputPart::Text {
                text: String::from(" carefully"),
            },
        ]);
        let frame = ClientFrame::try_new(
            request(9)?,
            ClientRequest::SubmitInput {
                command_id: command(10)?,
                session_id: uuid(11),
                content,
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )?;

        let encoded = encode_client_line(&frame)?;

        assert_eq!(decode_client_line(&encoded)?, frame);
        assert_eq!(
            String::from_utf8(encoded)?,
            concat!(
                "{\"version\":1,\"request_id\":\"9\",\"request\":{",
                "\"type\":\"submit_input\",",
                "\"command_id\":\"00000000-0000-0000-0000-00000000000a\",",
                "\"session_id\":\"00000000-0000-0000-0000-00000000000b\",",
                "\"content\":[{\"type\":\"text\",\"text\":\"inspect \"},",
                "{\"type\":\"attachment\",",
                "\"digest\":\"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\",",
                "\"kind\":\"image\",\"media_type\":\"image/png\",",
                "\"display_filename\":\"chart.png\"},",
                "{\"type\":\"text\",\"text\":\" carefully\"}],",
                "\"expected_defaults_version\":\"1\",",
                "\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
                "\"fast_mode\":{\"kind\":\"inherit\"},",
                "\"service_tier\":{\"kind\":\"inherit\"}}}}\n"
            )
        );
        Ok(())
    }

    /// INV-012: multipart decoding stops at the public retained-parts bound.
    #[test]
    fn inv012_multipart_deserialization_stops_after_the_parts_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let oversized = vec![
            UserInputPart::Text {
                text: String::from("x"),
            };
            super::MAX_USER_INPUT_PARTS + 1
        ];
        let encoded = serde_json::to_vec(&oversized)?;
        let error = serde_json::from_slice::<UserInputContent>(&encoded)
            .expect_err("one part beyond the retained bound is rejected during decoding");

        assert!(error.to_string().contains("too many user-input parts"));
        Ok(())
    }

    #[test]
    fn user_input_debug_redacts_content_bearing_values() -> Result<(), Box<dyn std::error::Error>> {
        let private_text = "private user text";
        let private_filename = "private-filename.txt";
        let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            .parse::<CanonicalBlobDigest>()?;
        let content = UserInputContent::from_parts(vec![
            UserInputPart::Text {
                text: String::from(private_text),
            },
            UserInputPart::Attachment {
                digest,
                kind: UserAttachmentKind::File,
                media_type: String::from("text/plain"),
                display_filename: Some(String::from(private_filename)),
            },
        ]);

        let debug = format!("{content:?}");
        assert!(!debug.contains(private_text));
        assert!(!debug.contains(private_filename));
        assert!(debug.contains("<redacted>"));
        Ok(())
    }

    /// INV-012 / INV-060: attachment display filenames are required-nullable
    /// in both directions of the version-one wire.
    #[test]
    fn inv012_inv060_attachment_requires_display_filename_member() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"submit_input","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"attachment","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","kind":"image","media_type":"image/png"}],"expected_defaults_version":"1","model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"attachment","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","kind":"image","media_type":"image/png"}]}}}"#,
        );
    }

    #[test]
    fn inv033_transcript_user_entry_round_trips_ordered_multipart_content()
    -> Result<(), Box<dyn std::error::Error>> {
        let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            .parse::<CanonicalBlobDigest>()?;
        assert_server_message_round_trip(
            request(31)?,
            ServerMessage::TranscriptUserEntry {
                entry_index: CanonicalU64::new(0),
                source_session_id: uuid(1),
                entry_id: uuid(2),
                accepted_input_id: uuid(3),
                turn_id: uuid(4),
                content: UserInputContent::from_parts(vec![
                    UserInputPart::Text {
                        text: String::from("inspect "),
                    },
                    UserInputPart::Attachment {
                        digest,
                        kind: UserAttachmentKind::Image,
                        media_type: String::from("image/png"),
                        display_filename: Some(String::from("chart.png")),
                    },
                    UserInputPart::Text {
                        text: String::from(" carefully"),
                    },
                ]),
            },
            r#"{"type":"transcript_user_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000002","accepted_input_id":"00000000-0000-0000-0000-000000000003","turn_id":"00000000-0000-0000-0000-000000000004","content":[{"type":"text","text":"inspect "},{"type":"attachment","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","kind":"image","media_type":"image/png","display_filename":"chart.png"},{"type":"text","text":" carefully"}]}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_transcript_user_entry_rejects_malformed_multipart_content() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_user_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000002","accepted_input_id":"00000000-0000-0000-0000-000000000003","turn_id":"00000000-0000-0000-0000-000000000004","content":[{"type":"attachment","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","kind":"image","media_type":"image/png"}]}}"#,
        );
    }

    #[test]
    fn attachment_byte_budget_rejection_requires_a_positive_maximum() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"error","code":"rejected","message":"attachment budget exceeded","detail":{"type":"rejected","rejection":{"type":"attachment_byte_budget_exceeded","maximum_bytes":"0"}}}}"#,
        );
    }

    #[test]
    fn inv033_read_transcript_round_trips_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(7)?,
            ClientRequest::ReadTranscript {
                session_id: uuid(1),
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert_eq!(frame.version(), ProtocolVersion::One);
        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":1,"));
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_imported_text_entries_round_trip_in_the_single_vocabulary()
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
        let text_frame =
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(8)?, imported_text)?;
        let encoded_text = encode_server_line(&text_frame)?;
        assert!(String::from_utf8(encoded_text.clone())?.contains(
            r#""entry":{"type":"imported","imported_conversation_id":"00000000-0000-0000-0000-000000000003","imported_entry_id":"00000000-0000-0000-0000-000000000004","source_speaker":{"type":"attested","speaker":"user"}}"#
        ));
        assert_eq!(decode_server_line(&encoded_text)?, text_frame);
        Ok(())
    }

    #[test]
    fn inv033_imported_conservative_entries_round_trip_in_the_single_vocabulary()
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
        let conservative = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
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
    fn inv033_delegated_task_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(0),
            source_session_id: uuid(2),
            entry_id: uuid(3),
            entry: TranscriptEntry::DelegatedTask {
                spawning_request_id: uuid(4),
                parent_session_id: uuid(1),
                parent_turn_id: uuid(5),
                content: String::from("inspect the durable result"),
            },
        };

        assert_server_message_round_trip(
            request(10)?,
            message,
            r#"{"type":"transcript_entry","entry_index":"0","source_session_id":"00000000-0000-0000-0000-000000000002","entry_id":"00000000-0000-0000-0000-000000000003","entry":{"type":"delegated_task","spawning_request_id":"00000000-0000-0000-0000-000000000004","parent_session_id":"00000000-0000-0000-0000-000000000001","parent_turn_id":"00000000-0000-0000-0000-000000000005","content":"inspect the durable result"}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_delegation_message_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(1),
            source_session_id: uuid(2),
            entry_id: uuid(3),
            entry: TranscriptEntry::DelegationMessage {
                spawning_request_id: uuid(4),
                message_id: uuid(5),
                sender_session_id: uuid(1),
                recipient_session_id: uuid(2),
                ordinal: CanonicalU64::new(1),
                delivery_sequence: CanonicalU64::new(2),
                content: String::from("continue with the checked input"),
            },
        };

        assert_server_message_round_trip(
            request(11)?,
            message,
            r#"{"type":"transcript_entry","entry_index":"1","source_session_id":"00000000-0000-0000-0000-000000000002","entry_id":"00000000-0000-0000-0000-000000000003","entry":{"type":"delegation_message","spawning_request_id":"00000000-0000-0000-0000-000000000004","message_id":"00000000-0000-0000-0000-000000000005","sender_session_id":"00000000-0000-0000-0000-000000000001","recipient_session_id":"00000000-0000-0000-0000-000000000002","ordinal":"1","delivery_sequence":"2","content":"continue with the checked input"}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_foreground_delegation_result_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(2),
            source_session_id: uuid(1),
            entry_id: uuid(3),
            entry: TranscriptEntry::DelegationResult {
                await_request_id: uuid(4),
                spawning_request_id: uuid(5),
                child_session_id: uuid(2),
                mode: DelegationWaitMode::Foreground,
                delivery_sequence: None,
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("checked result")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: uuid(2),
                    child_turn_id: uuid(6),
                },
            },
        };

        assert_server_message_round_trip(
            request(12)?,
            message,
            r#"{"type":"transcript_entry","entry_index":"2","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000003","entry":{"type":"delegation_result","await_request_id":"00000000-0000-0000-0000-000000000004","spawning_request_id":"00000000-0000-0000-0000-000000000005","child_session_id":"00000000-0000-0000-0000-000000000002","mode":"foreground","delivery_sequence":null,"outcome":"returned","content":"checked result","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"00000000-0000-0000-0000-000000000002","child_turn_id":"00000000-0000-0000-0000-000000000006"}}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_background_delegation_result_entries_round_trip_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::TranscriptEntry {
            entry_index: CanonicalU64::new(3),
            source_session_id: uuid(1),
            entry_id: uuid(3),
            entry: TranscriptEntry::DelegationResult {
                await_request_id: uuid(4),
                spawning_request_id: uuid(5),
                child_session_id: uuid(2),
                mode: DelegationWaitMode::Background,
                delivery_sequence: Some(CanonicalU64::new(7)),
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("wake result")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: uuid(2),
                    child_turn_id: uuid(6),
                },
            },
        };

        assert_server_message_round_trip(
            request(13)?,
            message,
            r#"{"type":"transcript_entry","entry_index":"3","source_session_id":"00000000-0000-0000-0000-000000000001","entry_id":"00000000-0000-0000-0000-000000000003","entry":{"type":"delegation_result","await_request_id":"00000000-0000-0000-0000-000000000004","spawning_request_id":"00000000-0000-0000-0000-000000000005","child_session_id":"00000000-0000-0000-0000-000000000002","mode":"background","delivery_sequence":"7","outcome":"returned","content":"wake result","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"00000000-0000-0000-0000-000000000002","child_turn_id":"00000000-0000-0000-0000-000000000006"}}}"#,
        )?;
        Ok(())
    }

    /// Rejects one `delegation_terminated` wire shape named by its outcome and
    /// reason spelling. Every other member carries the canonical parent-goal
    /// cascade the admitted shapes also use.
    #[track_caller]
    fn assert_delegation_terminal_state_rejected(outcome: &str, reason: &str) {
        serde_json::from_value::<TurnState>(serde_json::json!({
            "type": "delegation_terminated",
            "spawning_request_id": "00000000-0000-0000-0000-000000000004",
            "outcome": outcome,
            "reason": reason,
            "provenance": {
                "type": "parent_goal_command",
                "parent_session_id": "00000000-0000-0000-0000-000000000001",
                "goal_generation": "2",
                "command_id": "00000000-0000-0000-0000-000000000007",
                "descendant_scope": "parent_and_descendants"
            }
        }))
        .expect_err("an inadmissible terminal outcome and reason pair must not decode");
    }

    /// Round trips one admitted `delegation_terminated` turn state through
    /// serde and through the frame validator every transcript read and initial
    /// follow snapshot runs.
    #[track_caller]
    fn assert_delegation_terminal_state_round_trips(
        outcome: DelegationOutcome,
        reason: DelegationReason,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::DelegationTerminated {
            spawning_request_id: uuid(4),
            outcome,
            reason,
            provenance: DelegationProvenance::ParentGoalCommand {
                parent_session_id: uuid(1),
                goal_generation: CanonicalU64::new(2),
                command_id: uuid(7),
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        };
        let encoded = serde_json::to_value(&state)?;
        assert_eq!(serde_json::from_value::<TurnState>(encoded)?, state);

        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(1),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state,
            },
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
        Ok(())
    }

    #[test]
    fn awaiting_child_turn_state_round_trips_exact_wait_provenance()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::ActiveAwaitingChild {
            await_request_id: uuid(4),
            spawning_request_id: uuid(5),
            child_session_id: uuid(2),
        };
        let encoded = serde_json::to_value(&state)?;
        let decoded = serde_json::from_value::<TurnState>(encoded.clone())?;

        assert_eq!(decoded, state);
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "active_awaiting_child",
                "await_request_id": "00000000-0000-0000-0000-000000000004",
                "spawning_request_id": "00000000-0000-0000-0000-000000000005",
                "child_session_id": "00000000-0000-0000-0000-000000000002"
            })
        );
        // A terminal delegated turn admits only a parent-policy reason and a
        // stopped/cancelled outcome. Crossed pairs such as
        // stopped/parent_cancelled are valid under a bound relationship's own
        // termination policy and are covered by
        // `inv033_delegation_terminal_turn_state_round_trips_crossed_parent_policy`;
        // these two remain inadmissible on either half.
        assert_delegation_terminal_state_rejected("stopped", "child_completed");
        assert_delegation_terminal_state_rejected("already_terminal", "parent_cancelled");
        Ok(())
    }

    #[test]
    fn runner_recovery_turn_state_round_trips_interrupted_attempt()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::ActiveAwaitingRunnerRecovery {
            runner_id: uuid(2),
            placement_revision: PositiveCanonicalU64::try_new(3)
                .expect("the fixture revision is positive"),
            tool_attempt_id: Some(uuid(4)),
        };
        let encoded = serde_json::to_value(&state)?;
        let decoded = serde_json::from_value::<TurnState>(encoded.clone())?;

        assert_eq!(decoded, state);
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "active_awaiting_runner_recovery",
                "runner_id": "00000000-0000-0000-0000-000000000002",
                "placement_revision": "3",
                "tool_attempt_id": "00000000-0000-0000-0000-000000000004"
            })
        );
        Ok(())
    }

    #[test]
    fn runner_recovery_revision_rejects_zero_before_state_construction() {
        assert_eq!(
            PositiveCanonicalU64::try_new(0),
            Err(CanonicalValueError::Decimal),
        );
    }

    #[test]
    fn runner_recovery_turn_state_round_trips_explicit_absent_attempt()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::ActiveAwaitingRunnerRecovery {
            runner_id: uuid(2),
            placement_revision: PositiveCanonicalU64::try_new(3)
                .expect("the fixture revision is positive"),
            tool_attempt_id: None,
        };
        let encoded = serde_json::to_value(&state)?;
        let decoded = serde_json::from_value::<TurnState>(encoded.clone())?;

        assert_eq!(decoded, state);
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "active_awaiting_runner_recovery",
                "runner_id": "00000000-0000-0000-0000-000000000002",
                "placement_revision": "3",
                "tool_attempt_id": null
            })
        );
        Ok(())
    }

    #[test]
    fn runner_recovery_turn_state_requires_nullable_attempt_member() {
        let rejected = serde_json::from_value::<TurnState>(serde_json::json!({
            "type": "active_awaiting_runner_recovery",
            "runner_id": "00000000-0000-0000-0000-000000000002",
            "placement_revision": "3"
        }))
        .expect_err("the nullable tool attempt remains a required wire member");

        assert!(rejected.to_string().contains("tool_attempt_id"));
    }

    /// INV-044: runner-recovery wire state preserves the positive placement
    /// revision required by its relational source.
    #[test]
    fn inv044_runner_recovery_turn_state_rejects_zero_placement_revision() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000002","acceptance_position":"1","model_settings":null,"state":{"type":"active_awaiting_runner_recovery","runner_id":"00000000-0000-0000-0000-000000000003","placement_revision":"0","tool_attempt_id":null}}}"#,
        );
    }

    /// INV-044: the public state type cannot be inhabited with the zero
    /// placement revision rejected by its enclosing frame.
    #[test]
    fn inv044_runner_recovery_turn_state_direct_decode_rejects_zero_revision() {
        let rejected = serde_json::from_value::<TurnState>(serde_json::json!({
            "type": "active_awaiting_runner_recovery",
            "runner_id": "00000000-0000-0000-0000-000000000003",
            "placement_revision": "0",
            "tool_attempt_id": null
        }))
        .expect_err("the public runner-recovery state requires a positive revision");

        assert!(rejected.to_string().contains("positive placement revision"));
    }

    #[test]
    fn inv033_delegation_terminal_turn_state_round_trips_parent_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::DelegationTerminated {
            spawning_request_id: uuid(4),
            outcome: DelegationOutcome::Stopped,
            reason: DelegationReason::ParentStopped,
            provenance: DelegationProvenance::ParentGoalCommand {
                parent_session_id: uuid(1),
                goal_generation: CanonicalU64::new(2),
                command_id: uuid(7),
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        };
        let encoded = serde_json::to_value(&state)?;
        let decoded = serde_json::from_value::<TurnState>(encoded.clone())?;

        assert_eq!(decoded, state);
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "delegation_terminated",
                "spawning_request_id": "00000000-0000-0000-0000-000000000004",
                "outcome": "stopped",
                "reason": "parent_stopped",
                "provenance": {
                    "type": "parent_goal_command",
                    "parent_session_id": "00000000-0000-0000-0000-000000000001",
                    "goal_generation": "2",
                    "command_id": "00000000-0000-0000-0000-000000000007",
                    "descendant_scope": "parent_and_descendants"
                }
            })
        );
        Ok(())
    }

    #[test]
    fn inv033_delegation_terminal_turn_state_round_trips_crossed_parent_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        // A bound relationship maps the parent verb through its own policy, so
        // a parent cancellation may terminalize a child with `stop` and a
        // parent stop may terminalize it with `cancel`. All four pairs must
        // survive validation and round trip, matching what `process_read`
        // projects.
        assert_delegation_terminal_state_round_trips(
            DelegationOutcome::Stopped,
            DelegationReason::ParentStopped,
        )?;
        assert_delegation_terminal_state_round_trips(
            DelegationOutcome::Stopped,
            DelegationReason::ParentCancelled,
        )?;
        assert_delegation_terminal_state_round_trips(
            DelegationOutcome::Cancelled,
            DelegationReason::ParentStopped,
        )?;
        assert_delegation_terminal_state_round_trips(
            DelegationOutcome::Cancelled,
            DelegationReason::ParentCancelled,
        )?;
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
        assert_unsupported_version("2");
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
            r#"{"version":1,"request_id":"9","request":{"future":1,"future":2}}"#,
        ))
        .expect_err("nested duplicate members are malformed for every version");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
        assert_eq!(error.request_id().value(), 9);
    }

    #[test]
    fn inv033_duplicate_top_level_members_are_malformed_before_classification() {
        let duplicate_version = decode_client_line(&line(
            r#"{"version":1,"version":1,"request_id":"9","request":{"type":"list_sessions"}}"#,
        ))
        .expect_err("a duplicate version is malformed");
        assert_eq!(
            duplicate_version.kind(),
            FrameDecodeErrorKind::MalformedFrame
        );
        assert_eq!(duplicate_version.request_id().value(), 9);

        let reversed_version = decode_client_line(&line(
            r#"{"version":1,"version":1,"request_id":"9","request":{"type":"list_sessions"}}"#,
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
    fn delegated_queued_turn_round_trips_exact_origin_provenance()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(1),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::QueuedDelegated {
                    spawning_request_id: uuid(2),
                    parent_session_id: uuid(3),
                    parent_turn_id: uuid(4),
                    content: InputContent::new(String::from("delegated task")),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"queued_delegated","spawning_request_id":"00000000-0000-0000-0000-000000000002","parent_session_id":"00000000-0000-0000-0000-000000000003","parent_turn_id":"00000000-0000-0000-0000-000000000004","content":"delegated task"}}"#,
        )
    }

    #[test]
    fn delegation_wake_queued_turn_round_trips_exact_delivery_range()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(1),
                acceptance_position: CanonicalU64::new(2),
                model_settings: None,
                state: TurnState::QueuedDelegationWake {
                    first_delivery_sequence: CanonicalU64::new(3),
                    through_delivery_sequence: CanonicalU64::new(5),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"2","model_settings":null,"state":{"type":"queued_delegation_wake","first_delivery_sequence":"3","through_delivery_sequence":"5"}}"#,
        )
    }

    #[test]
    fn delegation_wake_queued_turn_rejects_invalid_delivery_ranges() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"2","model_settings":null,"state":{"type":"queued_delegation_wake","first_delivery_sequence":"0","through_delivery_sequence":"5"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"2","model_settings":null,"state":{"type":"queued_delegation_wake","first_delivery_sequence":"5","through_delivery_sequence":"3"}}}"#,
        );
    }

    #[test]
    fn inv033_nested_unit_shapes_reject_unknown_members() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"sessions_start","extra":true}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"text","text":"queued"}],"extra":true}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"session_created","extra":true}}}"#,
        );
    }

    #[test]
    fn inv033_active_running_requires_current_model_call_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000002"}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_shape_requires_nullable_attempt_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_model_call":null}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_shape_requires_nullable_call_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":null}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_call_requires_an_attempt() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":null,"terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000003","disposition":"known_failed"}}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_call_accepts_only_failure_dispositions() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"completed"}}}}"#,
        );
    }

    #[test]
    fn inv033_failed_terminal_call_rejects_unknown_members() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","extra":true}}}}"#,
        );
    }

    #[test]
    fn failed_terminal_call_cause_is_a_closed_wire_classification()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(91)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(1),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(2),
                    terminal_attempt_id: Some(uuid(3)),
                    terminal_model_call: Some(FailedTerminalModelCall::known_failed_with_cause(
                        uuid(4),
                        FailedModelCallCause::QuotaExhausted,
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","cause":"quota_exhausted"}}}"#,
        )?;
        Ok(())
    }

    fn assert_attachment_failure_cause_round_trip(
        cause: FailedModelCallCause,
        spelling: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let encoded = encode_server_line(&ServerFrame {
            version: ProtocolVersion::One,
            request_id: request(92)?,
            message: ServerMessage::TranscriptTurn {
                turn_id: uuid(1),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(2),
                    terminal_attempt_id: Some(uuid(3)),
                    terminal_model_call: Some(FailedTerminalModelCall::known_failed_with_cause(
                        uuid(4),
                        cause,
                    )),
                },
            },
        })?;
        assert!(std::str::from_utf8(&encoded)?.contains(&format!("\"cause\":\"{spelling}\"")));
        let decoded = decode_server_line(&encoded)?;
        let ServerMessage::TranscriptTurn {
            state:
                TurnState::Failed {
                    terminal_model_call: Some(call),
                    ..
                },
            ..
        } = decoded.message
        else {
            panic!("attachment failure fixture keeps its terminal call");
        };
        assert_eq!(call.cause(), Some(cause));
        Ok(())
    }

    #[test]
    fn attachment_too_large_round_trips_as_a_closed_wire_classification()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_attachment_failure_cause_round_trip(
            FailedModelCallCause::AttachmentTooLarge,
            "attachment_too_large",
        )
    }

    #[test]
    fn attachment_missing_round_trips_as_a_closed_wire_classification()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_attachment_failure_cause_round_trip(
            FailedModelCallCause::AttachmentMissing,
            "attachment_missing",
        )
    }

    #[test]
    fn attachment_corrupt_round_trips_as_a_closed_wire_classification()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_attachment_failure_cause_round_trip(
            FailedModelCallCause::AttachmentCorrupt,
            "attachment_corrupt",
        )
    }

    #[test]
    fn failed_terminal_call_rejects_an_unknown_failure_cause() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","cause":"future_provider_error"}}}}"#,
        );
    }

    #[test]
    fn failed_terminal_call_rejects_explicit_null_cause() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"known_failed","cause":null}}}}"#,
        );
    }

    #[test]
    fn failed_terminal_call_rejects_a_cause_on_cancelled_disposition() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000004","disposition":"cancelled","cause":"quota_exhausted"}}}}"#,
        );
    }

    #[test]
    fn inv033_cancelled_terminal_shape_requires_nullable_call_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003"}}}"#,
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
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000002","terminal_attempt_id":"00000000-0000-0000-0000-000000000003","terminal_model_call":null,"terminal_model_call":null}}}"#,
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
                model_settings: None,
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
    fn canonical_dollar_amount_matches_the_wire_decimal_representation() {
        assert!(
            CanonicalDollarAmount::try_new(String::from("79228162514264337593543950335")).is_ok()
        );
        assert!(
            CanonicalDollarAmount::try_new(String::from("0.0000000000000000000000000001")).is_ok()
        );
        assert!(
            CanonicalDollarAmount::try_new(String::from("79228162514264337593543950336")).is_err()
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
    fn inv060_publication_ambiguity_has_one_stable_error_code()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::PublicationAmbiguous,
                message: "ambiguous publication".to_owned(),
                detail: ErrorDetail::none(),
            },
        )?;
        let encoded = encode_server_line(&frame)?;
        assert!(
            encoded
                .windows(br#""code":"publication_ambiguous""#.len())
                .any(|window| window == br#""code":"publication_ambiguous""#)
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
                    model_settings: None,
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
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
                placement: super::SessionPlacement::Pathless {},
                lifecycle: SessionLifecycleMembers::default(),
            },
        )?;
        assert_client_request_current_version(request(2)?, ClientRequest::ListSessions {})?;
        assert_client_request_current_version(
            request(80)?,
            ClientRequest::UpdateSessionPlacement {
                command_id: command(81)?,
                session_id: uuid(82),
                expected_placement_version: CanonicalU64::new(1),
                replacement: super::SessionPlacement::Pathless {},
            },
        )?;
        assert_client_request_current_version(
            request(3)?,
            ClientRequest::SubmitInput {
                command_id: command(5)?,
                session_id: uuid(6),
                content: UserInputContent::text(String::from("content")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
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
    fn inv033_metadata_list_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(1)?,
            ClientRequest::ListSessionMetadata {
                required_tags: vec![String::from("daily"), String::from("work")],
                title_contains: Some(String::from("Plan")),
                include_archived: true,
                page_size: CanonicalU64::new(25),
                after_session_id: Some(uuid(6)),
            },
            r#"{"type":"list_session_metadata","required_tags":["daily","work"],"title_contains":"Plan","include_archived":true,"page_size":"25","after_session_id":"00000000-0000-0000-0000-000000000006"}"#,
        )
    }

    #[test]
    fn inv033_metadata_read_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(2)?,
            ClientRequest::ReadSessionMetadata {
                session_id: uuid(6),
            },
            r#"{"type":"read_session_metadata","session_id":"00000000-0000-0000-0000-000000000006"}"#,
        )
    }

    #[test]
    fn inv033_metadata_replacement_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(3)?,
            ClientRequest::ReplaceSessionMetadata {
                command_id: command(5)?,
                session_id: uuid(6),
                metadata: metadata(true)?,
            },
            r#"{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":"Planning","tags":["daily","work"],"attributes":{"run":"17","trigger":""},"archived":true}}"#,
        )
    }

    /// INV-033: the unified listing request has one exact closed shape.
    #[test]
    fn inv033_list_conversations_request_has_an_exact_closed_shape()
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

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            format!(
                "{{\"version\":{PROTOCOL_VERSION},\"request_id\":\"1\",\
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
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","origin":"all","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"50"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"everything","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"50","after":{"origin":"native_session","conversation_id":"00000000-0000-0000-0000-000000000006","extra":true}}}"#,
        );
    }

    /// INV-033: structurally invalid title filters reject a listing request
    /// before application construction.
    #[test]
    fn inv033_list_conversations_validates_title_filter_shape() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":"","origin":"all","include_archived":false,"page_size":"50","after":null}}"#,
        );
        assert_client_malformed(
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"list_conversations\",\"title_contains\":\"a\\u0000b\",\"origin\":\"all\",\"include_archived\":false,\"page_size\":\"50\",\"after\":null}}",
        );
    }

    #[test]
    fn list_conversations_page_size_has_no_wire_policy() -> Result<(), Box<dyn std::error::Error>> {
        decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"0","after":null}}"#,
        ))?;
        decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request":{"type":"list_conversations","title_contains":null,"origin":"all","include_archived":false,"page_size":"101","after":null}}"#,
        ))?;
        Ok(())
    }

    /// INV-033: the three unified page messages keep their exact closed
    /// shapes across round trips.
    #[test]
    fn inv033_conversation_page_messages_have_exact_closed_shapes()
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

    /// INV-033: a page end never names a cursor for an empty page.
    #[test]
    fn inv033_conversation_page_end_rejects_cursor_for_empty_page() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_page_end","conversation_count":"0","next_after":{"origin":"native_session","conversation_id":"00000000-0000-0000-0000-000000000001"}}}"#,
        );
    }

    #[test]
    fn conversation_page_count_has_no_wire_policy() -> Result<(), Box<dyn std::error::Error>> {
        decode_server_line(&line(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_page_end","conversation_count":"101","next_after":null}}"#,
        ))?;
        Ok(())
    }

    /// INV-033: a native summary title follows the metadata title rules and
    /// an imported summary restates the derived display-title shape.
    #[test]
    fn inv033_conversation_summary_shapes_are_validated() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"native_session","session_id":"00000000-0000-0000-0000-000000000001","title":"","archived":false,"defaults_version":"1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":"line\nbreak","entry_count":"1","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":" padded","entry_count":"1","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"imported_conversation","imported_conversation_id":"00000000-0000-0000-0000-000000000004","title":null,"entry_count":"0","source_format":"codex_rollout_jsonl_v1"}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"conversation_summary","conversation":{"origin":"native_session","session_id":"00000000-0000-0000-0000-000000000001","title":null,"archived":false,"defaults_version":"0"}}}"#,
        );
    }

    /// INV-033: imported title length is deployment policy, not wire grammar.
    #[test]
    fn inv033_conversation_summary_admits_structurally_valid_long_imported_title()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::ConversationSummary {
            conversation: ConversationSummary::ImportedConversation {
                imported_conversation_id: uuid(4),
                title: Some("long title x".repeat(31)),
                entry_count: CanonicalU64::new(1),
                source_format: ImportedConversationSourceFormat::ClaudeCodeSessionJsonlV2,
            },
        };
        ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message)?;
        Ok(())
    }

    #[test]
    fn inv033_import_request_preserves_exact_bytes_and_format()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::ImportConversation {
            format: ConversationImportFormat::ClaudeCodeSessionJsonlV2,
            source: ConversationImportSource::new(vec![0, 255]),
        };

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"import_conversation\",\
             \"format\":\"claude_code_session_jsonl_v2\",\"source\":\"AP8=\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: chunked import has one exact closed begin/append/commit/abort
    /// request vocabulary.
    #[test]
    fn inv033_chunked_import_requests_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let begin = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                declared_size_bytes: CanonicalU64::new(5),
            },
        )?;
        let append = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(2)?,
            ClientRequest::AppendConversationImport {
                chunk: ConversationImportSource::new(vec![0, 255]),
            },
        )?;
        let commit = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(3)?,
            ClientRequest::CommitConversationImport {},
        )?;
        let abort = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(4)?,
            ClientRequest::AbortConversationImport {},
        )?;

        let encoded_begin = encode_client_line(&begin)?;
        let encoded_append = encode_client_line(&append)?;
        let encoded_commit = encode_client_line(&commit)?;
        let encoded_abort = encode_client_line(&abort)?;
        assert_eq!(
            String::from_utf8(encoded_begin.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"begin_conversation_import\",\"format\":\"codex_rollout_jsonl_v1\",\"declared_size_bytes\":\"5\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encoded_append.clone())?,
            "{\"version\":1,\"request_id\":\"2\",\"request\":{\"type\":\"append_conversation_import\",\"chunk\":\"AP8=\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encoded_commit.clone())?,
            "{\"version\":1,\"request_id\":\"3\",\"request\":{\"type\":\"commit_conversation_import\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encoded_abort.clone())?,
            "{\"version\":1,\"request_id\":\"4\",\"request\":{\"type\":\"abort_conversation_import\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded_begin)?, begin);
        assert_eq!(decode_client_line(&encoded_append)?, append);
        assert_eq!(decode_client_line(&encoded_commit)?, commit);
        assert_eq!(decode_client_line(&encoded_abort)?, abort);
        Ok(())
    }

    /// INV-033: every maximum-sized append still fits the unchanged complete
    /// frame bound, while a larger raw chunk is invalid before encoding.
    #[test]
    fn inv033_import_append_respects_the_existing_frame_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let maximum = ClientRequest::AppendConversationImport {
            chunk: ConversationImportSource::new(vec![
                b'x';
                super::MAX_CONVERSATION_IMPORT_CHUNK_BYTES
            ]),
        };
        let oversized = ClientRequest::AppendConversationImport {
            chunk: ConversationImportSource::new(vec![
                b'x';
                super::MAX_CONVERSATION_IMPORT_CHUNK_BYTES
                    + 1
            ]),
        };

        let maximum_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            RequestId::try_new(u64::MAX)?,
            maximum,
        )?;
        assert!(encode_client_line(&maximum_frame)?.len() <= super::MAX_FRAME_BYTES);
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, oversized),
            Err(FrameValidationError::ConversationImportShape)
        );
        Ok(())
    }

    /// INV-033: chunked-import transport acknowledgements have exact closed
    /// shapes; commit deliberately keeps the existing terminal receipts.
    #[test]
    fn inv033_chunked_import_acknowledgements_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::ConversationImportBegun {
                declared_size_bytes: CanonicalU64::new(9),
            },
            r#"{"type":"conversation_import_begun","declared_size_bytes":"9"}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::ConversationImportAppended {
                assembled_size_bytes: CanonicalU64::new(7),
            },
            r#"{"type":"conversation_import_appended","assembled_size_bytes":"7"}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::ConversationImportAborted {},
            r#"{"type":"conversation_import_aborted"}"#,
        )?;
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
                request(4)?,
                ServerMessage::ConversationImportAppended {
                    assembled_size_bytes: CanonicalU64::new(0),
                },
            ),
            Err(FrameValidationError::ConversationImportShape)
        );
        Ok(())
    }

    /// INV-060: blob upload requests carry one exact tagged digest, positive
    /// length, and canonical bounded chunk without an implicit blob class.
    #[test]
    fn inv060_blob_upload_requests_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        let begin = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::BeginBlobUpload {
                expected_digest: digest,
                expected_length_bytes: CanonicalU64::new(2),
            },
        )?;
        let append = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(2)?,
            ClientRequest::AppendBlobUpload {
                chunk: BlobChunk::new(vec![0, 255]),
            },
        )?;
        let commit = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(3)?,
            ClientRequest::CommitBlobUpload {},
        )?;
        let abort = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(4)?,
            ClientRequest::AbortBlobUpload {},
        )?;

        let encoded_begin = encode_client_line(&begin)?;
        let encoded_append = encode_client_line(&append)?;
        let encoded_commit = encode_client_line(&commit)?;
        let encoded_abort = encode_client_line(&abort)?;
        assert_eq!(
            String::from_utf8(encoded_begin.clone())?,
            format!(
                "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"begin_blob_upload\",\"expected_digest\":\"{digest}\",\"expected_length_bytes\":\"2\"}}}}\n"
            )
        );
        assert_eq!(
            String::from_utf8(encoded_append.clone())?,
            "{\"version\":1,\"request_id\":\"2\",\"request\":{\"type\":\"append_blob_upload\",\"chunk\":\"AP8=\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encoded_commit.clone())?,
            "{\"version\":1,\"request_id\":\"3\",\"request\":{\"type\":\"commit_blob_upload\"}}\n"
        );
        assert_eq!(
            String::from_utf8(encoded_abort.clone())?,
            "{\"version\":1,\"request_id\":\"4\",\"request\":{\"type\":\"abort_blob_upload\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded_begin)?, begin);
        assert_eq!(decode_client_line(&encoded_append)?, append);
        assert_eq!(decode_client_line(&encoded_commit)?, commit);
        assert_eq!(decode_client_line(&encoded_abort)?, abort);
        assert!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::One,
                request(5)?,
                ClientRequest::BeginBlobUpload {
                    expected_digest: digest,
                    expected_length_bytes: CanonicalU64::new(0),
                },
            )
            .is_ok()
        );
        Ok(())
    }

    /// INV-060: one maximum decoded blob chunk fits the frame cap, while an
    /// empty or one-byte-larger append is rejected before encoding.
    #[test]
    fn inv060_blob_upload_chunk_bound_is_enforced() -> Result<(), Box<dyn std::error::Error>> {
        let maximum = ClientRequest::AppendBlobUpload {
            chunk: BlobChunk::new(vec![b'x'; super::MAX_BLOB_CHUNK_BYTES]),
        };
        let oversized = ClientRequest::AppendBlobUpload {
            chunk: BlobChunk::new(vec![b'x'; super::MAX_BLOB_CHUNK_BYTES + 1]),
        };
        let empty = ClientRequest::AppendBlobUpload {
            chunk: BlobChunk::new(Vec::new()),
        };
        let maximum_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            RequestId::try_new(u64::MAX)?,
            maximum,
        )?;

        assert!(encode_client_line(&maximum_frame)?.len() <= super::MAX_FRAME_BYTES);
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, oversized),
            Err(FrameValidationError::BlobUploadShape)
        );
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(2)?, empty),
            Err(FrameValidationError::BlobUploadShape)
        );
        Ok(())
    }

    /// INV-060: upload lifecycle receipts echo the exact verified identity and
    /// positive cumulative sizes.
    #[test]
    fn inv060_blob_upload_acknowledgements_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::BlobUploadBegun {
                expected_digest: digest,
                expected_length_bytes: CanonicalU64::new(9),
            },
            &format!(
                "{{\"type\":\"blob_upload_begun\",\"expected_digest\":\"{digest}\",\"expected_length_bytes\":\"9\"}}"
            ),
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::BlobUploadAlreadyPresent {
                digest,
                byte_length: CanonicalU64::new(9),
            },
            &format!(
                "{{\"type\":\"blob_upload_already_present\",\"digest\":\"{digest}\",\"byte_length\":\"9\"}}"
            ),
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::BlobUploadAppended {
                assembled_length_bytes: CanonicalU64::new(7),
            },
            r#"{"type":"blob_upload_appended","assembled_length_bytes":"7"}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::BlobUploadCommitted {
                digest,
                byte_length: CanonicalU64::new(9),
            },
            &format!(
                "{{\"type\":\"blob_upload_committed\",\"digest\":\"{digest}\",\"byte_length\":\"9\"}}"
            ),
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::BlobUploadAborted {},
            r#"{"type":"blob_upload_aborted"}"#,
        )?;
        Ok(())
    }

    /// INV-060: bulk-ingest ownership and every upload lifecycle failure use
    /// one exhaustive content-silent invalid-request vocabulary.
    #[test]
    fn inv060_blob_upload_refusals_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let expected_digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        let actual_digest = CanonicalBlobDigest::from_bytes([0xcd; 32]);
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("bulk ingest was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::BulkIngestAlreadyInProgress {
                        active_kind: BulkIngestKind::BlobUpload,
                    },
                ),
            },
            r#"{"type":"error","code":"invalid_request","message":"bulk ingest was rejected","detail":{"type":"bulk_ingest_already_in_progress","active_kind":"blob_upload"}}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("blob upload was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::BlobUploadAlreadyInProgress {},
                ),
            },
            r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_already_in_progress"}}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("blob upload was rejected"),
                detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadNotInProgress {}),
            },
            r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_not_in_progress"}}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("blob upload was rejected"),
                detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadLengthOutOfRange {
                    min_length_bytes: CanonicalU64::new(1),
                    max_length_bytes: CanonicalU64::new(8),
                    declared_length_bytes: CanonicalU64::new(9),
                }),
            },
            r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_length_out_of_range","min_length_bytes":"1","max_length_bytes":"8","declared_length_bytes":"9"}}"#,
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("blob upload was rejected"),
                detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadSizeExceeded {
                    expected_length_bytes: CanonicalU64::new(8),
                    actual_length_bytes: CanonicalU64::new(9),
                }),
            },
            r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_size_exceeded","expected_length_bytes":"8","actual_length_bytes":"9"}}"#,
        )?;
        assert_server_message_round_trip(
            request(6)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("blob upload was rejected"),
                detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadLengthMismatch {
                    expected_length_bytes: CanonicalU64::new(8),
                    actual_length_bytes: CanonicalU64::new(7),
                }),
            },
            r#"{"type":"error","code":"invalid_request","message":"blob upload was rejected","detail":{"type":"blob_upload_length_mismatch","expected_length_bytes":"8","actual_length_bytes":"7"}}"#,
        )?;
        assert_server_message_round_trip(
            request(7)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("blob upload was rejected"),
                detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadDigestMismatch {
                    expected_digest,
                    actual_digest,
                }),
            },
            &format!(
                "{{\"type\":\"error\",\"code\":\"invalid_request\",\"message\":\"blob upload was rejected\",\"detail\":{{\"type\":\"blob_upload_digest_mismatch\",\"expected_digest\":\"{expected_digest}\",\"actual_digest\":\"{actual_digest}\"}}}}"
            ),
        )?;
        Ok(())
    }

    /// INV-060: a direct metadata read has exact closed request and response
    /// shapes with canonical decimal facts.
    #[test]
    fn inv060_blob_metadata_wire_shapes_are_exact() -> Result<(), Box<dyn std::error::Error>> {
        let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        let metadata = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::ReadBlobMetadata { digest },
        )?;
        let encoded_metadata = encode_client_line(&metadata)?;

        assert_eq!(
            String::from_utf8(encoded_metadata.clone())?,
            format!(
                "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"read_blob_metadata\",\"digest\":\"{digest}\"}}}}\n"
            )
        );
        assert_eq!(decode_client_line(&encoded_metadata)?, metadata);
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::BlobMetadata {
                digest,
                byte_length: CanonicalU64::new(9),
                replica_count: CanonicalU64::new(1),
            },
            &format!(
                "{{\"type\":\"blob_metadata\",\"digest\":\"{digest}\",\"byte_length\":\"9\",\"replica_count\":\"1\"}}"
            ),
        )?;
        Ok(())
    }

    /// INV-060: a direct range read has exact closed request and response
    /// shapes with canonical decimal bounds.
    #[test]
    fn inv060_blob_range_wire_shapes_are_exact() -> Result<(), Box<dyn std::error::Error>> {
        let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        let offset = 7_u64;
        let length = 2_u64;
        let offset_bytes = CanonicalU64::new(offset);
        let length_bytes = CanonicalU64::new(length);
        let chunk = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::ReadBlobChunk {
                digest,
                offset_bytes,
                length_bytes,
            },
        )?;
        let encoded_chunk = encode_client_line(&chunk)?;

        assert_eq!(
            String::from_utf8(encoded_chunk.clone())?,
            format!(
                "{{\"version\":1,\"request_id\":\"1\",\"request\":{{\"type\":\"read_blob_chunk\",\"digest\":\"{digest}\",\"offset_bytes\":\"{offset}\",\"length_bytes\":\"{length}\"}}}}\n"
            )
        );
        assert_eq!(decode_client_line(&encoded_chunk)?, chunk);
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::BlobChunkRead {
                digest,
                offset_bytes,
                bytes: BlobChunk::new(vec![0, 255]),
            },
            &format!(
                "{{\"type\":\"blob_chunk\",\"digest\":\"{digest}\",\"offset_bytes\":\"{offset}\",\"bytes\":\"AP8=\"}}"
            ),
        )?;
        Ok(())
    }

    /// INV-060: a successful range response must represent its exact
    /// half-open byte range.
    #[test]
    fn inv060_blob_range_response_rejects_overflowing_end() -> Result<(), Box<dyn std::error::Error>>
    {
        let result = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::BlobChunkRead {
                digest: CanonicalBlobDigest::from_bytes([0xab; 32]),
                offset_bytes: CanonicalU64::new(u64::MAX),
                bytes: BlobChunk::new(vec![0]),
            },
        );

        assert_eq!(result, Err(FrameValidationError::BlobReadShape));
        Ok(())
    }

    /// INV-060: invalid direct range lengths remain decodable so the daemon
    /// can return the contracted typed invalid-request response.
    #[test]
    fn inv060_blob_read_length_bound_reaches_request_handling()
    -> Result<(), Box<dyn std::error::Error>> {
        let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        let zero = ClientRequest::ReadBlobChunk {
            digest,
            offset_bytes: CanonicalU64::new(0),
            length_bytes: CanonicalU64::new(0),
        };
        let oversized = ClientRequest::ReadBlobChunk {
            digest,
            offset_bytes: CanonicalU64::new(0),
            length_bytes: CanonicalU64::new(super::MAX_BLOB_READ_BYTES as u64 + 1),
        };
        assert!(ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, zero).is_ok());
        assert!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(2)?, oversized).is_ok()
        );
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("blob read was rejected"),
                detail: ErrorDetail::invalid_request(RejectionDetail::BlobReadLengthOutOfRange {
                    min_length_bytes: CanonicalU64::new(1),
                    max_length_bytes: CanonicalU64::new(super::MAX_BLOB_READ_BYTES as u64),
                    requested_length_bytes: CanonicalU64::new(0),
                }),
            },
            r#"{"type":"error","code":"invalid_request","message":"blob read was rejected","detail":{"type":"blob_read_length_out_of_range","min_length_bytes":"1","max_length_bytes":"4194304","requested_length_bytes":"0"}}"#,
        )?;
        Ok(())
    }

    /// INV-060: an exact maximum direct range response remains inside the
    /// unchanged frame ceiling.
    #[test]
    fn inv060_maximum_blob_read_response_fits_one_frame() -> Result<(), Box<dyn std::error::Error>>
    {
        let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
        let maximum = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            RequestId::try_new(u64::MAX)?,
            ServerMessage::BlobChunkRead {
                digest,
                offset_bytes: CanonicalU64::new(0),
                bytes: BlobChunk::new(vec![b'x'; super::MAX_BLOB_READ_BYTES]),
            },
        )?;

        assert!(encode_server_line(&maximum)?.len() <= super::MAX_FRAME_BYTES);
        Ok(())
    }

    /// INV-060: an out-of-bounds read is one typed invalid request.
    #[test]
    fn inv060_blob_read_out_of_bounds_failure_is_typed() -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("blob read was rejected"),
                detail: ErrorDetail::invalid_request(RejectionDetail::BlobReadRangeOutOfBounds {
                    offset_bytes: CanonicalU64::new(u64::MAX),
                    length_bytes: CanonicalU64::new(1),
                    blob_length_bytes: CanonicalU64::new(9),
                }),
            },
            r#"{"type":"error","code":"invalid_request","message":"blob read was rejected","detail":{"type":"blob_read_range_out_of_bounds","offset_bytes":"18446744073709551615","length_bytes":"1","blob_length_bytes":"9"}}"#,
        )?;
        Ok(())
    }

    /// INV-060: exhausting absent replicas has a content-silent missing code.
    #[test]
    fn inv060_blob_missing_failure_is_typed() -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::BlobMissing,
                message: String::from("all recorded blob replicas are missing"),
                detail: ErrorDetail::none(),
            },
            r#"{"type":"error","code":"blob_missing","message":"all recorded blob replicas are missing"}"#,
        )?;
        Ok(())
    }

    /// INV-060: exhausting corrupt replicas has a content-silent corruption
    /// code.
    #[test]
    fn inv060_blob_corrupt_failure_is_typed() -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::BlobCorrupt,
                message: String::from("all usable blob replicas are corrupt"),
                detail: ErrorDetail::none(),
            },
            r#"{"type":"error","code":"blob_corrupt","message":"all usable blob replicas are corrupt"}"#,
        )?;
        Ok(())
    }

    /// INV-060: a size-exceeded refusal cannot claim the impossible zero
    /// expected length that begin-upload admission rejects.
    #[test]
    fn inv060_blob_upload_size_exceeded_rejects_zero_expected_length()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob upload was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadSizeExceeded {
                expected_length_bytes: CanonicalU64::new(0),
                actual_length_bytes: CanonicalU64::new(1),
            }),
        };

        assert_eq!(
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message),
            Err(FrameValidationError::BlobUploadShape)
        );
        Ok(())
    }

    /// INV-060: a length-mismatch refusal cannot claim the impossible zero
    /// expected length that begin-upload admission rejects.
    #[test]
    fn inv060_blob_upload_length_mismatch_rejects_zero_expected_length()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("blob upload was rejected"),
            detail: ErrorDetail::invalid_request(RejectionDetail::BlobUploadLengthMismatch {
                expected_length_bytes: CanonicalU64::new(0),
                actual_length_bytes: CanonicalU64::new(1),
            }),
        };

        assert_eq!(
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message),
            Err(FrameValidationError::BlobUploadShape)
        );
        Ok(())
    }

    /// INV-033: import-invalid-request evidence names exact sizes and only the
    /// content-silent converter class plus record ordinal.
    #[test]
    fn inv033_conversation_import_rejection_evidence_has_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceTooLarge {
                        limit_bytes: CanonicalU64::new(268_435_456),
                        declared_size_bytes: CanonicalU64::new(300_000_000),
                        actual_size_bytes: None,
                    },
                ),
            },
            r#"{"type":"error","code":"invalid_request","message":"conversation import was rejected","detail":{"type":"conversation_import_source_too_large","limit_bytes":"268435456","declared_size_bytes":"300000000","actual_size_bytes":null}}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportSourceSizeMismatch {
                        declared_size_bytes: CanonicalU64::new(100),
                        actual_size_bytes: CanonicalU64::new(99),
                    },
                ),
            },
            r#"{"type":"error","code":"invalid_request","message":"conversation import was rejected","detail":{"type":"conversation_import_source_size_mismatch","declared_size_bytes":"100","actual_size_bytes":"99"}}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportConversionFailed {
                        class: ConversationImportRejectionClass::InvalidJson,
                        record_ordinal: Some(CanonicalU64::new(7)),
                    },
                ),
            },
            r#"{"type":"error","code":"invalid_request","message":"conversation import was rejected","detail":{"type":"conversation_import_conversion_failed","class":"invalid_json","record_ordinal":"7"}}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                message: String::from("conversation import was rejected"),
                detail: ErrorDetail::invalid_request(
                    RejectionDetail::ConversationImportConversionFailed {
                        class: ConversationImportRejectionClass::EmptySource,
                        record_ordinal: None,
                    },
                ),
            },
            r#"{"type":"error","code":"invalid_request","message":"conversation import was rejected","detail":{"type":"conversation_import_conversion_failed","class":"empty_source","record_ordinal":null}}"#,
        )?;
        let empty_source_with_ordinal = ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("conversation import was rejected"),
            detail: ErrorDetail::invalid_request(
                RejectionDetail::ConversationImportConversionFailed {
                    class: ConversationImportRejectionClass::EmptySource,
                    record_ordinal: Some(CanonicalU64::new(1)),
                },
            ),
        };
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
                request(5)?,
                empty_source_with_ordinal,
            ),
            Err(FrameValidationError::ConversationImportShape)
        );
        let invalid_json_without_ordinal = ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("conversation import was rejected"),
            detail: ErrorDetail::invalid_request(
                RejectionDetail::ConversationImportConversionFailed {
                    class: ConversationImportRejectionClass::InvalidJson,
                    record_ordinal: None,
                },
            ),
        };
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
                request(6)?,
                invalid_json_without_ordinal,
            ),
            Err(FrameValidationError::ConversationImportShape)
        );
        let contradictory_observed_bound = ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            message: String::from("conversation import was rejected"),
            detail: ErrorDetail::invalid_request(
                RejectionDetail::ConversationImportSourceTooLarge {
                    limit_bytes: CanonicalU64::new(8),
                    declared_size_bytes: CanonicalU64::new(9),
                    actual_size_bytes: Some(CanonicalU64::new(10)),
                },
            ),
        };
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
                request(7)?,
                contradictory_observed_bound,
            ),
            Err(FrameValidationError::ConversationImportShape)
        );
        Ok(())
    }

    /// INV-033: imported-frontier creation has one exact closed request shape.
    #[test]
    fn inv033_imported_frontier_creation_has_an_exact_closed_shape()
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
            model_settings: ModelSettingsOverlay::inherit_all(),
        };

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"create_session_from_imported_frontier\",\"command_id\":\"00000000-0000-0000-0000-000000000004\",\"imported_conversation_id\":\"00000000-0000-0000-0000-000000000005\",\"through_position\":\"2\",\"relationship\":\"resume\",\"initial_model_selection\":{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000006\"},\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}}}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_imported_frontier_vocabulary_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command(4)?,
                imported_conversation_id: uuid(5),
                through_position: CanonicalU64::new(2),
                relationship: ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: uuid(6),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert_eq!(frame.version(), ProtocolVersion::One);
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: model-call usage has one exact closed shape.
    #[test]
    fn inv033_model_call_usage_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>>
    {
        let request_id = request(1)?;
        let message = ServerMessage::TranscriptModelCallUsage {
            model_call_index: CanonicalU64::new(0),
            turn_id: uuid(2),
            model_call_id: uuid(3),
            usage_provenance: UsageProvenance::Reported,
            usage: ModelCallTokenUsage {
                input_tokens: Some(CanonicalU64::new(10)),
                output_tokens: Some(CanonicalU64::new(0)),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(CanonicalU64::new(4)),
            },
            cost: Some(ModelCallDollarCost {
                amount_usd: CanonicalDollarAmount::try_new(String::from("0.125"))?,
                rate_version: BillingRateVersion::try_new(String::from("rates-v7"))?,
                label: ModelCallCostLabel::MeteredEquivalent,
            }),
        };

        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request_id, message)?;
        let encoded = encode_server_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            format!(
                "{{\"version\":{PROTOCOL_VERSION},\"request_id\":\"1\",\"message\":{{\"type\":\"transcript_model_call_usage\",\"model_call_index\":\"0\",\"turn_id\":\"00000000-0000-0000-0000-000000000002\",\"model_call_id\":\"00000000-0000-0000-0000-000000000003\",\"usage_provenance\":\"reported\",\"usage\":{{\"input_tokens\":\"10\",\"output_tokens\":\"0\",\"cache_creation_input_tokens\":null,\"cache_read_input_tokens\":\"4\"}},\"cost\":{{\"amount_usd\":\"0.125\",\"rate_version\":\"rates-v7\",\"label\":\"metered_equivalent\"}}}}}}\n"
            )
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn usage_rejects_an_omitted_evidence_field() {
        let error = decode_server_line(&line(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_model_call_usage","model_call_index":"0","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","usage_provenance":"reported","usage":{"input_tokens":null,"output_tokens":null,"cache_creation_input_tokens":null},"cost":null}}"#,
        ))
        .expect_err("required-nullable evidence fields cannot be omitted");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
    }

    #[test]
    fn usage_rejects_an_omitted_cost_member() {
        let error = decode_server_line(&line(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_model_call_usage","model_call_index":"0","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","usage_provenance":"reported","usage":{"input_tokens":null,"output_tokens":null,"cache_creation_input_tokens":null,"cache_read_input_tokens":null}}}"#,
        ))
        .expect_err("the derived cost member is required nullable");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
    }

    #[test]
    fn usage_rejects_cost_without_a_present_axis() {
        let error = decode_server_line(&line(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_model_call_usage","model_call_index":"0","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","usage_provenance":"reported","usage":{"input_tokens":null,"output_tokens":null,"cache_creation_input_tokens":null,"cache_read_input_tokens":null},"cost":{"amount_usd":"0","rate_version":"rates-v1","label":"real"}}}"#,
        ))
        .expect_err("a cost without derivation evidence must be rejected");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
    }

    #[test]
    fn usage_provenance_rejects_unknown_values() {
        let error = decode_server_line(&line(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_model_call_usage","model_call_index":"0","turn_id":"00000000-0000-0000-0000-000000000002","model_call_id":"00000000-0000-0000-0000-000000000003","usage_provenance":"inferred","usage":{"input_tokens":null,"output_tokens":null,"cache_creation_input_tokens":null,"cache_read_input_tokens":null},"cost":null}}"#,
        ))
        .expect_err("the usage provenance vocabulary is closed");
        assert_eq!(error.kind(), FrameDecodeErrorKind::MalformedFrame);
    }

    #[test]
    fn inv033_imported_frontier_rejects_zero_position() -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(2)?,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command(4)?,
                imported_conversation_id: uuid(5),
                through_position: CanonicalU64::new(0),
                relationship: ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: uuid(6),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        );

        assert_eq!(frame, Err(FrameValidationError::ImportedFrontierShape));
        Ok(())
    }

    #[test]
    fn inv033_imported_conversation_read_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::ReadImportedConversation {
            imported_conversation_id: uuid(5),
        };

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"read_imported_conversation\",\"imported_conversation_id\":\"00000000-0000-0000-0000-000000000005\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: an imported-conversation entry carries its position, exact
    /// attestation, content kind, and bounded preview in one closed shape.
    #[test]
    fn inv033_imported_conversation_entry_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(2),
            imported_entry_id: uuid(6),
            source_speaker: ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::Assistant,
            },
            content_kind: ImportedContentKind::Text,
            text_preview: Some(ImportedTextPreview::of_exact_text("imported answer")),
        };

        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message)?;
        let encoded = encode_server_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"message\":{\"type\":\"imported_conversation_entry\",\"position\":\"2\",\"imported_entry_id\":\"00000000-0000-0000-0000-000000000006\",\"source_speaker\":{\"type\":\"attested\",\"speaker\":\"assistant\"},\"content_kind\":\"text\",\"text_preview\":{\"preview\":\"imported answer\",\"truncated\":false}}}\n"
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    /// An entry whose content carries no exact attested text states that
    /// absence as an explicit null rather than an empty preview.
    #[test]
    fn inv033_imported_conversation_entry_states_an_absent_preview_as_null()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::ImportedConversationEntry {
                position: CanonicalU64::new(1),
                imported_entry_id: uuid(6),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::SourceEvent,
                text_preview: None,
            },
        )?;
        let encoded = encode_server_line(&frame)?;

        assert!(String::from_utf8(encoded.clone())?.contains("\"text_preview\":null"));
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    /// An imported-conversation entry position is one-based, so zero is not a
    /// selectable ordinal on the wire.
    #[test]
    fn inv033_imported_conversation_entry_rejects_zero_position()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::ImportedConversationEntry {
                position: CanonicalU64::new(0),
                imported_entry_id: uuid(6),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::SourceEvent,
                text_preview: None,
            },
        );

        assert_eq!(
            frame,
            Err(FrameValidationError::ImportedConversationEntryShape)
        );
        Ok(())
    }

    /// A preview cuts on a Unicode scalar boundary, so it is always an exact
    /// prefix of the source text and never a split encoding.
    #[test]
    fn imported_text_preview_cuts_on_a_scalar_boundary() {
        // 86 three-byte scalars are 258 bytes, so the configured 256-byte limit falls
        // inside the 86th scalar and the preview keeps only the first 85.
        let text = "\u{4e00}".repeat(86);
        let preview = ImportedTextPreview::of_exact_text_with_limit(&text, Some(256));

        assert_eq!(preview.preview(), "\u{4e00}".repeat(85));
        assert!(preview.truncated());
        assert!(text.starts_with(preview.preview()));
    }

    /// Text inside the bound is previewed exactly and is not marked truncated.
    #[test]
    fn imported_text_preview_retains_exact_text_within_its_bound() {
        let preview = ImportedTextPreview::of_exact_text("imported question");

        assert_eq!(preview.preview(), "imported question");
        assert!(!preview.truncated());
    }

    /// Attested empty text previews as exact empty text, distinguishing it
    /// from an entry that carries no attested text at all.
    #[test]
    fn imported_text_preview_retains_attested_empty_text() {
        let preview = ImportedTextPreview::of_exact_text("");

        assert_eq!(preview.preview(), "");
        assert!(!preview.truncated());
    }

    /// A preview deserialized on its own is checked exactly as an embedded one
    /// is, so no consumer can hold a bounded preview that violates its bound.
    #[test]
    fn imported_text_preview_validates_on_direct_deserialization() {
        let oversized = format!(
            "{{\"preview\":\"{}\",\"truncated\":false}}",
            "a".repeat(MAX_CONTENT_FRAGMENT_BYTES + 1)
        );

        assert!(serde_json::from_str::<ImportedTextPreview>(&oversized).is_err());
        assert!(
            serde_json::from_str::<ImportedTextPreview>(r#"{"preview":"","truncated":true}"#)
                .is_err()
        );
        assert_eq!(
            serde_json::from_str::<ImportedTextPreview>(r#"{"preview":"ab","truncated":true}"#)
                .expect("a bounded truncated preview decodes"),
            ImportedTextPreview {
                preview: String::from("ab"),
                truncated: true,
            }
        );
    }

    /// A truncation marker over an empty preview contradicts the scalar cut,
    /// which always keeps at least one scalar of nonempty text.
    #[test]
    fn inv033_imported_text_preview_rejects_truncated_empty_text()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::ImportedConversationEntry {
                position: CanonicalU64::new(1),
                imported_entry_id: uuid(6),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::Text,
                text_preview: Some(ImportedTextPreview {
                    preview: String::new(),
                    truncated: true,
                }),
            },
        );

        assert_eq!(frame, Err(FrameValidationError::ImportedTextPreviewShape));
        Ok(())
    }

    /// A preview states an entry's exact attested text, so attaching one to a
    /// kind that has no such text is a contradictory frame rather than extra
    /// information the client may present.
    #[test]
    fn inv033_imported_conversation_entry_rejects_a_preview_on_nontext_content()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::ImportedConversationEntry {
                position: CanonicalU64::new(1),
                imported_entry_id: uuid(6),
                source_speaker: ImportedSourceSpeaker::NotAttested {},
                content_kind: ImportedContentKind::ToolCall,
                text_preview: Some(ImportedTextPreview::of_exact_text("lookup")),
            },
        );

        assert_eq!(
            frame,
            Err(FrameValidationError::ImportedConversationEntryShape)
        );
        Ok(())
    }

    /// A requested ordinal inside the stated range contradicts the rejection
    /// carrying it, so the frame is refused rather than rendered.
    #[test]
    fn inv033_imported_range_rejection_refuses_a_selectable_requested_position()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the command was rejected by current durable state"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::ImportedFrontierPositionOutOfRange {
                        imported_conversation_id: uuid(5),
                        requested_position: CanonicalU64::new(2),
                        last_position: CanonicalU64::new(2),
                    },
                ),
            },
        );

        assert_eq!(frame, Err(FrameValidationError::ImportedFrontierRangeShape));
        Ok(())
    }

    /// An imported conversation is nonempty, so a zero selectable bound cannot
    /// describe one.
    #[test]
    fn inv033_imported_range_rejection_refuses_an_empty_selectable_range()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the command was rejected by current durable state"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::ImportedFrontierPositionOutOfRange {
                        imported_conversation_id: uuid(5),
                        requested_position: CanonicalU64::new(1),
                        last_position: CanonicalU64::new(0),
                    },
                ),
            },
        );

        assert_eq!(frame, Err(FrameValidationError::ImportedFrontierRangeShape));
        Ok(())
    }

    /// INV-033: an out-of-range imported position is a rejection naming the
    /// conversation's selectable range, never the absent-session `not_found`.
    #[test]
    fn inv033_names_the_imported_position_range() -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::Error {
            code: ErrorCode::Rejected,
            message: String::from("the command was rejected by current durable state"),
            detail: ErrorDetail::rejected(RejectionDetail::ImportedFrontierPositionOutOfRange {
                imported_conversation_id: uuid(5),
                requested_position: CanonicalU64::new(999_999),
                last_position: CanonicalU64::new(2),
            }),
        };

        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message)?;
        let encoded = encode_server_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"message\":{\"type\":\"error\",\"code\":\"rejected\",\"message\":\"the command was rejected by current durable state\",\"detail\":{\"type\":\"imported_frontier_position_out_of_range\",\"imported_conversation_id\":\"00000000-0000-0000-0000-000000000005\",\"requested_position\":\"999999\",\"last_position\":\"2\"}}}\n"
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: an absent imported conversation names an imported conversation
    /// as the missing target.
    #[test]
    fn inv033_names_the_absent_imported_conversation() -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the command was rejected by current durable state"),
                detail: ErrorDetail::rejected(RejectionDetail::ImportedConversationNotFound {
                    imported_conversation_id: uuid(5),
                }),
            },
        )?;
        let encoded = encode_server_line(&frame)?;

        assert!(
            String::from_utf8(encoded.clone())?
                .contains("\"type\":\"imported_conversation_not_found\"")
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_submit_request_round_trips_in_the_single_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::SubmitInput {
                command_id: command(4)?,
                session_id: uuid(6),
                content: UserInputContent::text(String::from("ordinary work")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":1,"));
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_turn_control_vocabulary_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::StopTurn {
                command_id: command(4)?,
                session_id: uuid(6),
                expected_active_turn_id: uuid(7),
                content: UserInputContent::text(String::from("continue after the stop")),
                expected_defaults_version: CanonicalU64::new(1),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":1,"));
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: reconciliation has one exact closed request shape.
    #[test]
    fn inv033_reconcile_turn_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::ReconcileTurn {
            command_id: command(4)?,
            session_id: uuid(6),
            expected_active_turn_id: uuid(7),
            content: UserInputContent::text(String::from("continue after reconciliation")),
            expected_defaults_version: CanonicalU64::new(1),
            model_settings: ModelSettingsOverlay::inherit_all(),
        };

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"reconcile_turn\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"expected_active_turn_id\":\"00000000-0000-0000-0000-000000000007\",\
             \"content\":[{\"type\":\"text\",\"text\":\"continue after reconciliation\"}],\
             \"expected_defaults_version\":\"1\",\
             \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
             \"fast_mode\":{\"kind\":\"inherit\"},\
             \"service_tier\":{\"kind\":\"inherit\"}}}}\n"
        );
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
    fn inv033_import_source_requires_canonical_padded_base64() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v1","source":"AA"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v1","source":"AB=="}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"import_conversation","format":"codex_rollout_jsonl_v1","source":"AA==="}}"#,
        );
    }

    #[test]
    fn inv033_submit_exchange_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ClientRequest::SubmitInput {
                command_id: command(2)?,
                session_id: uuid(3),
                content: UserInputContent::text(String::from("content")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )?;
        let encoded_request = encode_client_line(&request_frame)?;
        assert_eq!(decode_client_line(&encoded_request)?, request_frame);
        let response_frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ServerMessage::InputSubmitted {
                session_id: uuid(3),
                accepted_input_id: uuid(4),
                acceptance_position: CanonicalU64::new(1),
                turn_id: uuid(5),
                model_settings: settings_snapshot_fixture(),
            },
        )?;
        let encoded_response = encode_server_line(&response_frame)?;
        assert_eq!(decode_server_line(&encoded_response)?, response_frame);
        Ok(())
    }

    /// INV-033: the cursorless provider-text message round trips exactly.
    #[test]
    fn inv033_provider_text_message_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ClientRequest::ReadReviewTarget { target_id: uuid(2) },
        )?;
        let encoded_request = encode_client_line(&request_frame)?;
        let delta = ServerMessage::ProviderTextDelta {
            session_id: uuid(3),
            turn_id: uuid(4),
            model_call_id: uuid(5),
            part_index: CanonicalU64::new(6),
            content: ContentFragment::try_new(String::from("already [redacted]"))?,
        };

        assert_eq!(decode_client_line(&encoded_request)?, request_frame);
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request_id, delta)?;
        let encoded_delta = encode_server_line(&frame)?;
        assert_eq!(decode_server_line(&encoded_delta)?, frame);
        Ok(())
    }

    /// INV-033: review target registration has one exact closed shape.
    #[test]
    fn inv033_review_target_exchange_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(1)?;
        let request_value = ClientRequest::CreateReviewTarget {
            command_id: command(2)?,
            target_id: uuid(3),
            provider: String::from("example-host"),
            repository: String::from("example/repository"),
            subject: ReviewTargetSubject::ChangeRequest {
                number: CanonicalU64::new(42),
            },
            head_revision: String::from("head-revision"),
            base_revision: Some(String::from("base-revision")),
            stack_parent_target_id: None,
        };
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"create_review_target\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"target_id\":\"00000000-0000-0000-0000-000000000003\",\
             \"provider\":\"example-host\",\"repository\":\"example/repository\",\
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
    fn review_orchestration_start_has_one_exact_v1_shape() -> Result<(), Box<dyn std::error::Error>>
    {
        let request_id = request(11)?;
        let frame = ClientFrame::try_new(
            request_id,
            ClientRequest::StartReviewOrchestration {
                command_id: command(2)?,
                attempt_id: uuid(3),
                target_id: uuid(4),
                concern_set_version: String::from("initial-five"),
                import_template_name: String::from("review.import"),
                judgment_template_name: String::from("review.judgment"),
                repair_template_name: String::from("review.repair"),
                publication_template_name: String::from("review.publication"),
                concerns: vec![ReviewOrchestrationConcernInput {
                    key: String::from("correctness"),
                    template_name: String::from("review.concern.correctness"),
                }],
            },
        )?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"11\",\"request\":{\"type\":\"start_review_orchestration\",\"command_id\":\"00000000-0000-0000-0000-000000000002\",\"attempt_id\":\"00000000-0000-0000-0000-000000000003\",\"target_id\":\"00000000-0000-0000-0000-000000000004\",\"concern_set_version\":\"initial-five\",\"import_template_name\":\"review.import\",\"judgment_template_name\":\"review.judgment\",\"repair_template_name\":\"review.repair\",\"publication_template_name\":\"review.publication\",\"concerns\":[{\"key\":\"correctness\",\"template_name\":\"review.concern.correctness\"}]}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        assert_server_message_round_trip(
            request_id,
            ServerMessage::ReviewOrchestrationStarted {
                attempt_id: uuid(3),
            },
            r#"{"type":"review_orchestration_started","attempt_id":"00000000-0000-0000-0000-000000000003"}"#,
        )
    }

    #[test]
    fn review_finding_event_request_round_trips_under_its_generalized_name()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new(
            request(12)?,
            ClientRequest::RecordReviewFindingEvent {
                command_id: command(2)?,
                run_id: uuid(3),
                pass_id: uuid(4),
                turn_id: uuid(5),
                output_frontier_id: Some(uuid(6)),
                finding_id: uuid(7),
                event_ordinal: CanonicalU64::new(2),
                event: ReviewFindingEvent::Duplicate {
                    canonical_finding_id: uuid(8),
                },
            },
        )?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"12\",\"request\":{\"type\":\"record_review_finding_event\",\"command_id\":\"00000000-0000-0000-0000-000000000002\",\"run_id\":\"00000000-0000-0000-0000-000000000003\",\"pass_id\":\"00000000-0000-0000-0000-000000000004\",\"turn_id\":\"00000000-0000-0000-0000-000000000005\",\"output_frontier_id\":\"00000000-0000-0000-0000-000000000006\",\"finding_id\":\"00000000-0000-0000-0000-000000000007\",\"event_ordinal\":\"2\",\"event\":{\"kind\":\"duplicate\",\"canonical_finding_id\":\"00000000-0000-0000-0000-000000000008\"}}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn review_pass_success_round_trips_with_terminal_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let succeeded = ClientFrame::try_new(
            request(13)?,
            ClientRequest::CompleteReviewPass {
                command_id: command(2)?,
                run_id: uuid(3),
                pass_id: uuid(4),
                turn_id: Some(uuid(5)),
                output_frontier_id: Some(uuid(6)),
                outcome: ReviewPassTerminalOutcome::Succeeded,
            },
        )?;
        assert_eq!(
            decode_client_line(&encode_client_line(&succeeded)?)?,
            succeeded
        );
        Ok(())
    }

    #[test]
    fn review_pass_completion_rejects_evidence_for_another_outcome() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"13","request":{"type":"complete_review_pass","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":null,"outcome":"succeeded"}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"13","request":{"type":"complete_review_pass","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":"00000000-0000-0000-0000-000000000006","outcome":"failed"}}"#,
        );
    }

    #[test]
    fn review_pass_completion_requires_the_nullable_turn_member() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"13","request":{"type":"complete_review_pass","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","output_frontier_id":null,"outcome":"cancelled"}}"#,
        );
    }

    #[test]
    fn review_orchestration_stage_requests_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let digest = CanonicalDigest::try_new("cd".repeat(32))?;
        let import = ClientFrame::try_new(
            request(19)?,
            ClientRequest::RecordReviewImportOutcome {
                command_id: command(2)?,
                attempt_id: uuid(3),
                pass_id: Some(uuid(4)),
                external_link_id: None,
                context_digest: Some(digest),
                outcome: ReviewImportTerminalOutcome::Succeeded,
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&import)?)?, import);
        let concern = ClientFrame::try_new(
            request(20)?,
            ClientRequest::RecordReviewConcernOutcome {
                command_id: command(2)?,
                attempt_id: uuid(3),
                concern: String::from("correctness"),
                pass_id: Some(uuid(5)),
                outcome: ReviewConcernTerminalOutcome::Succeeded,
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&concern)?)?, concern);
        let plan = ClientFrame::try_new(
            request(21)?,
            ClientRequest::RecordReviewJudgmentPlan {
                command_id: command(2)?,
                attempt_id: uuid(3),
                analysis_pass_id: uuid(6),
                members: vec![ReviewJudgmentPlanMember {
                    finding_id: uuid(7),
                    disposition: ReviewJudgmentDisposition::Accepted {},
                }],
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&plan)?)?, plan);
        let effect = ClientFrame::try_new(
            request(22)?,
            ClientRequest::RecordReviewJudgmentEffect {
                command_id: command(2)?,
                attempt_id: uuid(3),
                finding_id: uuid(7),
                event_pass_id: Some(uuid(8)),
                outcome: ReviewJudgmentEffectTerminalOutcome::Applied,
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&effect)?)?, effect);
        let repairs = ClientFrame::try_new(
            request(23)?,
            ClientRequest::RecordReviewRepairOutcomes {
                command_id: command(2)?,
                attempt_id: uuid(3),
                outcomes: vec![ReviewRepairOutcome {
                    finding_id: uuid(7),
                    event_pass_id: Some(uuid(9)),
                    outcome: ReviewRepairTerminalOutcome::Fixed,
                }],
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&repairs)?)?, repairs);
        let publications = ClientFrame::try_new(
            request(24)?,
            ClientRequest::RecordReviewPublicationOutcomes {
                command_id: command(2)?,
                attempt_id: uuid(3),
                outcomes: vec![ReviewPublicationOutcome {
                    finding_id: uuid(7),
                    external_link_id: Some(uuid(10)),
                    outcome: ReviewPublicationTerminalOutcome::Published,
                }],
            },
        )?;
        assert_eq!(
            decode_client_line(&encode_client_line(&publications)?)?,
            publications
        );
        let read = ClientFrame::try_new(
            request(25)?,
            ClientRequest::ReadReviewOrchestration {
                attempt_id: uuid(3),
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&read)?)?, read);
        assert_server_message_round_trip(
            request(26)?,
            ServerMessage::ReviewOrchestrationAdvanced {
                attempt_id: uuid(3),
                state: ReviewOrchestrationState::AwaitingPublication,
            },
            r#"{"type":"review_orchestration_advanced","attempt_id":"00000000-0000-0000-0000-000000000003","state":"awaiting_publication"}"#,
        )
    }

    #[test]
    fn review_import_success_requires_pass_and_context_evidence() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"14","request":{"type":"record_review_import_outcome","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","pass_id":null,"external_link_id":null,"context_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","outcome":"succeeded"}}"#,
        );
    }

    #[test]
    fn review_concern_failure_requires_pass_evidence() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"14","request":{"type":"record_review_concern_outcome","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","concern":"correctness","pass_id":null,"outcome":"failed"}}"#,
        );
    }

    #[test]
    fn review_incomplete_judgment_effect_rejects_pass_evidence() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"14","request":{"type":"record_review_judgment_effect","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","finding_id":"00000000-0000-0000-0000-000000000004","event_pass_id":"00000000-0000-0000-0000-000000000005","outcome":"blocked"}}"#,
        );
    }

    #[test]
    fn review_fixed_repair_requires_event_pass_evidence() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"14","request":{"type":"record_review_repair_outcomes","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","outcomes":[{"finding_id":"00000000-0000-0000-0000-000000000004","event_pass_id":null,"outcome":"fixed"}]}}"#,
        );
    }

    #[test]
    fn review_published_outcome_requires_external_link_evidence() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"14","request":{"type":"record_review_publication_outcomes","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","outcomes":[{"finding_id":"00000000-0000-0000-0000-000000000004","external_link_id":null,"outcome":"published"}]}}"#,
        );
    }

    #[test]
    fn review_blocked_finding_event_round_trips_with_null_frontier()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new(
            request(27)?,
            ClientRequest::RecordReviewFindingEvent {
                command_id: command(2)?,
                run_id: uuid(3),
                pass_id: uuid(4),
                turn_id: uuid(5),
                output_frontier_id: None,
                finding_id: uuid(7),
                event_ordinal: CanonicalU64::new(2),
                event: ReviewFindingEvent::BlockedWithReason {
                    reason: String::from("requires reconciliation"),
                    external_link_id: None,
                },
            },
        )?;
        let encoded = encode_client_line(&frame)?;

        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn review_finding_event_rejects_frontier_mismatched_to_event() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"27","request":{"type":"record_review_finding_event","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":"00000000-0000-0000-0000-000000000006","finding_id":"00000000-0000-0000-0000-000000000007","event_ordinal":"1","event":{"kind":"blocked_with_reason","reason":"requires reconciliation","external_link_id":null}}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"27","request":{"type":"record_review_finding_event","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":null,"finding_id":"00000000-0000-0000-0000-000000000007","event_ordinal":"1","event":{"kind":"accepted"}}}"#,
        );
    }

    #[test]
    fn review_finding_event_refuses_unknown_members() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"14","request":{"type":"record_review_finding_event","command_id":"00000000-0000-0000-0000-000000000002","run_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","turn_id":"00000000-0000-0000-0000-000000000005","output_frontier_id":"00000000-0000-0000-0000-000000000006","finding_id":"00000000-0000-0000-0000-000000000007","event_ordinal":"1","event":{"kind":"accepted","future":true}}}"#,
        );
    }

    #[test]
    fn review_concern_outcome_refuses_unknown_tokens() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"14","request":{"type":"record_review_concern_outcome","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","concern":"correctness","pass_id":null,"outcome":"future"}}"#,
        );
    }

    #[test]
    fn review_import_refuses_noncanonical_digest() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"15","request":{"type":"record_review_import_outcome","command_id":"00000000-0000-0000-0000-000000000002","attempt_id":"00000000-0000-0000-0000-000000000003","pass_id":"00000000-0000-0000-0000-000000000004","external_link_id":null,"context_digest":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","outcome":"succeeded"}}"#,
        );
    }

    #[test]
    fn review_orchestration_read_refuses_noncanonical_identity() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"15","request":{"type":"read_review_orchestration","attempt_id":"00000000-0000-0000-0000-00000000000A"}}"#,
        );
    }

    #[test]
    fn review_orchestration_refuses_oversized_concern_inventory()
    -> Result<(), Box<dyn std::error::Error>> {
        let concern = ReviewOrchestrationConcernInput {
            key: String::from("correctness"),
            template_name: String::from("correctness"),
        };
        let oversized = ClientFrame::try_new(
            request(15)?,
            ClientRequest::StartReviewOrchestration {
                command_id: command(2)?,
                attempt_id: uuid(3),
                target_id: uuid(4),
                concern_set_version: String::from("initial"),
                import_template_name: String::from("import"),
                judgment_template_name: String::from("judgment"),
                repair_template_name: String::from("repair"),
                publication_template_name: String::from("publication"),
                concerns: vec![concern; 33],
            },
        );
        assert_eq!(oversized, Err(FrameValidationError::ReviewShape));
        Ok(())
    }

    #[test]
    fn review_orchestration_refuses_oversized_judgment_inventory()
    -> Result<(), Box<dyn std::error::Error>> {
        let oversized = ClientFrame::try_new(
            request(16)?,
            ClientRequest::RecordReviewJudgmentPlan {
                command_id: command(2)?,
                attempt_id: uuid(3),
                analysis_pass_id: uuid(4),
                members: vec![
                    ReviewJudgmentPlanMember {
                        finding_id: uuid(5),
                        disposition: ReviewJudgmentDisposition::Accepted {},
                    };
                    1_025
                ],
            },
        );
        assert_eq!(oversized, Err(FrameValidationError::ReviewShape));
        Ok(())
    }

    #[test]
    fn review_orchestration_snapshot_round_trips_with_frozen_inventory()
    -> Result<(), Box<dyn std::error::Error>> {
        let digest = CanonicalDigest::try_new("ab".repeat(32))?;
        let snapshot = ReviewOrchestrationSnapshot {
            attempt_id: uuid(3),
            target_id: uuid(4),
            state: ReviewOrchestrationState::AwaitingJudgment,
            concern_set_version: String::from("initial-five"),
            stage_template_digests: ReviewOrchestrationStageTemplateDigests {
                import: digest.clone(),
                judgment: digest.clone(),
                repair: digest.clone(),
                publication: digest.clone(),
            },
            concerns: vec![ReviewOrchestrationConcernSnapshot {
                key: String::from("correctness"),
                template_digest: digest,
                status: ReviewOrchestrationConcernStatus::Succeeded,
                pass_id: Some(uuid(5)),
            }],
            counts: ReviewOrchestrationCounts {
                finding_count: CanonicalU64::new(2),
                judgment_member_count: CanonicalU64::new(0),
                judgment_effect_applied_count: CanonicalU64::new(0),
                repair_fixed_count: CanonicalU64::new(0),
                publication_published_count: CanonicalU64::new(0),
            },
        };
        let frame = ServerFrame::try_new(
            request(17)?,
            ServerMessage::ReviewOrchestration { snapshot },
        )?;
        let encoded = encode_server_line(&frame)?;
        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn review_orchestration_snapshot_preserves_superseded_concern_status()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = orchestration_snapshot_fixture(
            ReviewOrchestrationState::FanoutIncomplete,
            ReviewOrchestrationConcernStatus::Superseded,
            Some(uuid(5)),
            ReviewOrchestrationCounts {
                finding_count: CanonicalU64::new(0),
                judgment_member_count: CanonicalU64::new(0),
                judgment_effect_applied_count: CanonicalU64::new(0),
                repair_fixed_count: CanonicalU64::new(0),
                publication_published_count: CanonicalU64::new(0),
            },
        )?;
        let frame = ServerFrame::try_new(
            request(18)?,
            ServerMessage::ReviewOrchestration { snapshot },
        )?;
        let encoded = encode_server_line(&frame)?;

        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    #[test]
    fn review_orchestration_snapshot_rejects_terminal_state_with_pending_concern()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = orchestration_snapshot_fixture(
            ReviewOrchestrationState::Complete,
            ReviewOrchestrationConcernStatus::Pending,
            None,
            ReviewOrchestrationCounts {
                finding_count: CanonicalU64::new(0),
                judgment_member_count: CanonicalU64::new(0),
                judgment_effect_applied_count: CanonicalU64::new(0),
                repair_fixed_count: CanonicalU64::new(0),
                publication_published_count: CanonicalU64::new(0),
            },
        )?;

        let frame = ServerFrame::try_new(
            request(18)?,
            ServerMessage::ReviewOrchestration { snapshot },
        );

        assert_eq!(frame, Err(FrameValidationError::ReviewShape));
        Ok(())
    }

    #[test]
    fn review_orchestration_snapshot_rejects_incomplete_state_with_complete_judgment()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = orchestration_snapshot_fixture(
            ReviewOrchestrationState::AwaitingJudgmentEffects,
            ReviewOrchestrationConcernStatus::Succeeded,
            Some(uuid(5)),
            ReviewOrchestrationCounts {
                finding_count: CanonicalU64::new(1),
                judgment_member_count: CanonicalU64::new(1),
                judgment_effect_applied_count: CanonicalU64::new(1),
                repair_fixed_count: CanonicalU64::new(0),
                publication_published_count: CanonicalU64::new(0),
            },
        )?;

        let frame = ServerFrame::try_new(
            request(19)?,
            ServerMessage::ReviewOrchestration { snapshot },
        );

        assert_eq!(frame, Err(FrameValidationError::ReviewShape));
        Ok(())
    }

    #[test]
    fn review_orchestration_snapshot_rejects_overlapping_terminal_counts()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = orchestration_snapshot_fixture(
            ReviewOrchestrationState::Complete,
            ReviewOrchestrationConcernStatus::Succeeded,
            Some(uuid(5)),
            ReviewOrchestrationCounts {
                finding_count: CanonicalU64::new(1),
                judgment_member_count: CanonicalU64::new(1),
                judgment_effect_applied_count: CanonicalU64::new(1),
                repair_fixed_count: CanonicalU64::new(1),
                publication_published_count: CanonicalU64::new(1),
            },
        )?;

        let frame = ServerFrame::try_new(
            request(20)?,
            ServerMessage::ReviewOrchestration { snapshot },
        );

        assert_eq!(frame, Err(FrameValidationError::ReviewShape));
        Ok(())
    }

    #[test]
    fn review_pass_completed_receipt_round_trips_terminal_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let terminal = ServerFrame::try_new(
            request(18)?,
            ServerMessage::ReviewPassCompleted {
                run_id: uuid(6),
                pass_id: uuid(7),
                state: ReviewPassLifecycle::Blocked,
            },
        )?;
        assert_eq!(
            decode_server_line(&encode_server_line(&terminal)?)?,
            terminal
        );
        Ok(())
    }

    #[test]
    fn inv033_inv046_adds_forward_only_defaults_replacement()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = request(6)?;
        let request_value = ClientRequest::ReplaceSessionDefaults {
            command_id: command(1)?,
            session_id: uuid(2),
            expected_defaults_version: CanonicalU64::new(3),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            model_settings: ModelSettingsOverlay::inherit_all(),
            dangerous_tool_auto_approval: true,
            system_prompt: SystemPromptMember::present(None),
        };
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"6\",\"request\":{\"type\":\"replace_session_defaults\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"expected_defaults_version\":\"3\",\"model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
             \"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}},\
             \"dangerous_tool_auto_approval\":true,\"system_prompt\":null}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let replacement_receipt = ServerMessage::SessionDefaultsReplaced {
            session_id: uuid(2),
            defaults_version: CanonicalU64::new(4),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            model_settings: provider_default_settings_snapshot_fixture(),
            dangerous_tool_auto_approval: true,
            system_prompt: SystemPromptMember::present(None),
        };
        assert_server_message_round_trip(
            request(7)?,
            replacement_receipt,
            &format!(
                "{{\"type\":\"session_defaults_replaced\",\"session_id\":\"00000000-0000-0000-0000-000000000002\",\"defaults_version\":\"4\",\"model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"}},\"model_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"dangerous_tool_auto_approval\":true,\"system_prompt\":null}}"
            ),
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
        assert_server_message_round_trip(
            request(9)?,
            exhaustion,
            r#"{"type":"error","code":"rejected","message":"defaults version exhausted","detail":{"type":"defaults_version_exhausted","session_id":"00000000-0000-0000-0000-000000000002","current":"18446744073709551615"}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_inv046_adds_the_bounded_session_system_prompt()
    -> Result<(), Box<dyn std::error::Error>> {
        // The system-prompt member is required.
        // Every admitted frame must carry the member explicitly.
        assert_client_malformed(
            r#"{"version":1,"request_id":"3","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"}}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"4","request":{"type":"replace_session_defaults","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","expected_defaults_version":"3","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}}"#,
        );
        // The defaults read version member is required and nullable.
        assert_client_malformed(
            r#"{"version":1,"request_id":"5","request":{"type":"read_session_defaults","session_id":"00000000-0000-0000-0000-000000000002"}}"#,
        );
        // A present prompt is nonempty and rejects U+0000.
        assert_client_malformed(
            r#"{"version":1,"request_id":"6","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"system_prompt":""}}"#,
        );
        assert_client_malformed(
            "{\"version\":1,\"request_id\":\"7\",\"request\":{\"type\":\"create_session\",\"command_id\":\"00000000-0000-0000-0000-000000000001\",\"initial_model_selection\":{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\"system_prompt\":\"a\\u0000b\"}}",
        );

        let request_id = request(8)?;
        let create = ClientRequest::CreateSession {
            command_id: command(1)?,
            initial_model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
                "exact prompt text".to_owned(),
            )?)),
            placement: super::SessionPlacement::Pathless {},
            lifecycle: SessionLifecycleMembers::default(),
        };
        let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, create)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"8\",\"request\":{\"type\":\"create_session\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"initial_model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
             \"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}},\
             \"system_prompt\":\"exact prompt text\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let promptless_create = ClientRequest::CreateSession {
            command_id: command(1)?,
            initial_model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(None),
            placement: super::SessionPlacement::Pathless {},
            lifecycle: SessionLifecycleMembers::default(),
        };
        let promptless_frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, promptless_create)?;
        let promptless_encoded = encode_client_line(&promptless_frame)?;
        assert_eq!(
            String::from_utf8(promptless_encoded.clone())?,
            "{\"version\":1,\"request_id\":\"8\",\"request\":{\"type\":\"create_session\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"initial_model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
             \"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}},\
             \"system_prompt\":null}}\n"
        );
        assert_eq!(decode_client_line(&promptless_encoded)?, promptless_frame);
        let decoded_null = decode_client_line(&line(
            r#"{"version":1,"request_id":"8","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"system_prompt":null}}"#,
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
            model_settings: ModelSettingsOverlay::inherit_all(),
            dangerous_tool_auto_approval: false,
            system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
                "exact prompt text".to_owned(),
            )?)),
        };
        let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request(9)?, replace)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"9\",\"request\":{\"type\":\"replace_session_defaults\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"expected_defaults_version\":\"3\",\"model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
             \"fast_mode\":{\"kind\":\"inherit\"},\"service_tier\":{\"kind\":\"inherit\"}},\
             \"dangerous_tool_auto_approval\":false,\
             \"system_prompt\":\"exact prompt text\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let read_current = ClientRequest::ReadSessionDefaults {
            session_id: uuid(2),
            defaults_version: None,
        };
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(10)?, read_current)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"10\",\"request\":{\"type\":\"read_session_defaults\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"defaults_version\":null}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let read_named = ClientRequest::ReadSessionDefaults {
            session_id: uuid(2),
            defaults_version: Some(CanonicalU64::new(3)),
        };
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(11)?, read_named)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"11\",\"request\":{\"type\":\"read_session_defaults\",\
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
            model_settings: provider_default_settings_snapshot_fixture(),
            dangerous_tool_auto_approval: true,
            system_prompt: SystemPromptMember::present(Some(SystemPromptText::try_new(
                "exact prompt text".to_owned(),
            )?)),
        };
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(12)?, receipt)?;
        let encoded = encode_server_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"12\",\"message\":{\"type\":\"session_defaults_replaced\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000002\",\
             \"defaults_version\":\"4\",\"model_selection\":{\"kind\":\"direct\",\
             \"selection_id\":\"00000000-0000-0000-0000-000000000004\"},\
             \"model_settings\":"
                .to_owned()
                + PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON
                + ",\
             \"dangerous_tool_auto_approval\":true,\
             \"system_prompt\":\"exact prompt text\"}}\n"
        );
        assert_eq!(decode_server_line(&encoded)?, frame);
        assert_server_malformed(
            r#"{"version":1,"request_id":"12","message":{"type":"session_defaults_replaced","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"4","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":true}}"#,
        );

        let defaults_read = ServerMessage::SessionDefaults {
            session_id: uuid(2),
            defaults_version: CanonicalU64::new(4),
            model_selection: ModelSelection::Direct {
                selection_id: uuid(4),
            },
            model_settings: provider_default_settings_snapshot_fixture(),
            dangerous_tool_auto_approval: false,
            system_prompt: Some(SystemPromptText::try_new("exact prompt text".to_owned())?),
        };
        assert_server_message_round_trip(
            request(13)?,
            defaults_read,
            &format!(
                "{{\"type\":\"session_defaults\",\"session_id\":\"00000000-0000-0000-0000-000000000002\",\"defaults_version\":\"4\",\"model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"}},\"model_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"dangerous_tool_auto_approval\":false,\"system_prompt\":\"exact prompt text\"}}"
            ),
        )?;
        assert_server_message_round_trip(
            request(14)?,
            ServerMessage::SessionDefaults {
                session_id: uuid(2),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                model_settings: provider_default_settings_snapshot_fixture(),
                dangerous_tool_auto_approval: false,
                system_prompt: None,
            },
            &format!(
                "{{\"type\":\"session_defaults\",\"session_id\":\"00000000-0000-0000-0000-000000000002\",\"defaults_version\":\"1\",\"model_selection\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"}},\"model_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"dangerous_tool_auto_approval\":false,\"system_prompt\":null}}"
            ),
        )?;
        Ok(())
    }

    /// INV-033: prompt text enforces structural content rules only.
    #[test]
    fn inv033_system_prompt_text_rejects_empty_and_nul_content() {
        let admitted = SystemPromptText::try_new(String::from("exact √ prompt"))
            .expect("structurally valid text is admitted");
        assert_eq!(admitted.as_str(), "exact √ prompt");
        assert!(SystemPromptText::try_new(String::new()).is_err());
        assert!(SystemPromptText::try_new("a\u{0}b".to_owned()).is_err());
    }

    /// INV-033: deployment limits use one closed required nullable wire shape.
    #[test]
    fn inv033_deployment_limits_have_exact_closed_wire_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(1)?,
            ClientRequest::ReadDeploymentLimits {},
            r#"{"type":"read_deployment_limits"}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::DeploymentLimits {
                max_message_utf8_bytes: Some(CanonicalU64::new(7)),
                max_system_prompt_utf8_bytes: None,
                terminal_input_channel_capacity: Some(CanonicalU64::new(3)),
                min_metadata_page_size: Some(CanonicalU64::new(1)),
                max_metadata_page_size: None,
                max_review_findings_per_run: Some(CanonicalU64::new(9)),
            },
            r#"{"type":"deployment_limits","max_message_utf8_bytes":"7","max_system_prompt_utf8_bytes":null,"terminal_input_channel_capacity":"3","min_metadata_page_size":"1","max_metadata_page_size":null,"max_review_findings_per_run":"9"}"#,
        )?;
        Ok(())
    }

    /// INV-033 / INV-047: template frames have exact closed shapes.
    #[test]
    fn inv033_inv047_template_frames_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let create = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ClientRequest::CreateSessionFromTemplate {
                command_id: command(2)?,
                template_name: "reviewer".to_owned(),
                placement: super::SessionPlacement::Pathless {},
                lifecycle: SessionLifecycleMembers::default(),
            },
        )?;
        let encoded_create = encode_client_line(&create)?;
        assert_eq!(
            String::from_utf8(encoded_create.clone())?,
            r#"{"version":1,"request_id":"1","request":{"type":"create_session_from_template","command_id":"00000000-0000-0000-0000-000000000002","template_name":"reviewer"}}
"#
        );
        assert_eq!(decode_client_line(&encoded_create)?, create);

        let list = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(2)?,
            ClientRequest::ListTemplates {},
        )?;
        let encoded_list = encode_client_line(&list)?;
        assert_eq!(
            String::from_utf8(encoded_list.clone())?,
            r#"{"version":1,"request_id":"2","request":{"type":"list_templates"}}
"#
        );
        assert_eq!(decode_client_line(&encoded_list)?, list);

        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::TemplatesStart {},
            r#"{"type":"templates_start"}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::TemplateSummary {
                name: "reviewer".to_owned(),
                version: CanonicalU64::new(7),
            },
            r#"{"type":"template_summary","name":"reviewer","version":"7"}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::TemplatesEnd {
                template_count: CanonicalU64::new(1),
            },
            r#"{"type":"templates_end","template_count":"1"}"#,
        )?;
        Ok(())
    }

    #[test]
    fn root_placement_creation_and_update_frames_record_global_read_intent_loudly()
    -> Result<(), Box<dyn std::error::Error>> {
        let root_path = "operator";
        let root = super::SessionPlacement::try_root_global_read(String::from(root_path))?;
        let create = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(70)?,
            ClientRequest::CreateSession {
                command_id: command(71)?,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: uuid(72),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
                placement: root.clone(),
                lifecycle: SessionLifecycleMembers::default(),
            },
        )?;
        assert_eq!(
            String::from_utf8(encode_client_line(&create)?)?,
            r#"{"version":1,"request_id":"70","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000047","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000048"},"model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"system_prompt":null,"placement":{"kind":"root_global_read","path":"operator","intent":"acknowledged"}}}
"#
        );
        let update = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(73)?,
            ClientRequest::UpdateSessionPlacement {
                command_id: command(74)?,
                session_id: uuid(75),
                expected_placement_version: CanonicalU64::new(1),
                replacement: root,
            },
        )?;
        assert_eq!(decode_client_line(&encode_client_line(&update)?)?, update);
        assert_eq!(
            super::SessionPlacement::try_scoped(String::from(root_path)),
            Err(super::CanonicalValueError::Placement)
        );
        Ok(())
    }

    #[test]
    fn inv033_session_placement_constructor_rejects_paths_over_the_structural_byte_bound() {
        let maximum_structural_path = vec!["x".repeat(64); 64].join(".");
        let frame_sized_empty_segments = ".".repeat(super::MAX_FRAME_BYTES - 1);

        assert!(super::SessionPlacement::try_scoped(maximum_structural_path).is_ok());
        assert_eq!(
            super::SessionPlacement::try_scoped(frame_sized_empty_segments),
            Err(super::CanonicalValueError::Placement)
        );
    }

    #[test]
    fn inv033_session_placement_frames_admit_the_complete_structural_range() {
        let maximum_structural_path = vec!["x".repeat(64); 64].join(".");
        let frame = format!(
            r#"{{"version":1,"request_id":"1","request":{{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000047","initial_model_selection":{{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000048"}},"model_settings":{{"reasoning_level":{{"kind":"inherit"}},"fast_mode":{{"kind":"inherit"}},"service_tier":{{"kind":"inherit"}}}},"system_prompt":null,"placement":{{"kind":"scoped","path":"{maximum_structural_path}"}}}}}}
"#
        );

        decode_client_line(frame.as_bytes()).expect("complete structural path is request-admitted");
        let response = ServerFrame::try_new(
            request(2).expect("fixture request identity is admitted"),
            ServerMessage::SessionSummary {
                session_id: uuid(3),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Alias { alias_id: uuid(4) },
                placement_version: CanonicalU64::new(1),
                placement: super::SessionPlacement::Scoped {
                    path: maximum_structural_path,
                },
                runner: None,
            },
        )
        .expect("legacy structural placement remains response-encodable");
        let encoded = encode_server_line(&response).expect("response encoding succeeds");
        assert_eq!(
            decode_server_line(&encoded).expect("response decoding succeeds"),
            response
        );
    }

    #[test]
    fn inv033_session_placement_rejection_versions_are_coherent() {
        assert_placement_version_mismatch_rejected(0, 2);
        assert_placement_version_mismatch_rejected(1, 0);
        assert_placement_version_mismatch_rejected(2, 2);
        assert_eq!(
            placement_version_exhaustion_frame(1)
                .expect_err("nonmaximum placement version cannot be exhausted"),
            FrameValidationError::ErrorDetailShape
        );
        assert!(placement_version_exhaustion_frame(u64::MAX).is_ok());

        let valid = ServerFrame::try_new(
            request(1).expect("fixture request identity is admitted"),
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("placement version mismatch"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::SessionPlacementCurrentVersionMismatch {
                        session_id: uuid(2),
                        expected_placement_version: CanonicalU64::new(1),
                        current_placement_version: CanonicalU64::new(2),
                    },
                ),
            },
        );
        assert!(valid.is_ok());
    }

    /// INV-033: invalid template names or versions cannot enter admitted frames.
    #[test]
    fn inv033_template_frames_require_valid_values() -> Result<(), Box<dyn std::error::Error>> {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"create_session_from_template","command_id":"00000000-0000-0000-0000-000000000002","template_name":"Reviewer"}}"#,
        );
        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
                request(1)?,
                ServerMessage::TemplateSummary {
                    name: "reviewer".to_owned(),
                    version: CanonicalU64::new(0),
                },
            )
            .expect_err("zero template version is rejected"),
            FrameValidationError::TemplateShape
        );
        Ok(())
    }

    /// INV-033: a frame at the single version is admitted unchanged.
    #[test]
    fn inv033_single_protocol_version_is_admitted() -> Result<(), Box<dyn std::error::Error>> {
        let frame = ClientFrame::try_new(request(1)?, ClientRequest::ListSessions {})?;
        let encoded = encode_client_line(&frame)?;

        assert_eq!(frame.version(), ProtocolVersion::One);
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: the integer immediately below the single version is refused.
    #[test]
    fn inv033_version_below_single_version_is_refused() {
        assert_unsupported_version("0");
    }

    /// INV-033: closed-enum decoding refuses an unknown version member.
    #[test]
    fn inv033_unknown_protocol_version_member_is_refused() {
        let error = serde_json::from_str::<ProtocolVersion>("2")
            .expect_err("an unknown protocol version must be refused");

        assert!(error.to_string().contains("frame version is unsupported"));
    }

    /// INV-033: the model-alias catalog has exact closed shapes.
    #[test]
    fn inv033_model_alias_catalog_has_exact_closed_shapes() -> Result<(), Box<dyn std::error::Error>>
    {
        let request_id = request(1)?;
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request_id,
            ClientRequest::ListModelAliases {},
        )?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"list_model_aliases\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::ModelAliasesStart {},
            r#"{"type":"model_aliases_start"}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::ModelAliasSummary {
                alias_id: uuid(4),
                selection_id: uuid(5),
            },
            r#"{"type":"model_alias_summary","alias_id":"00000000-0000-0000-0000-000000000004","selection_id":"00000000-0000-0000-0000-000000000005"}"#,
        )?;
        assert_server_message_round_trip(
            request(6)?,
            ServerMessage::ModelAliasesEnd {
                alias_count: CanonicalU64::new(1),
            },
            r#"{"type":"model_aliases_end","alias_count":"1"}"#,
        )?;
        Ok(())
    }

    /// One commissioned-session request carries its complete composite —
    /// fence, statement, and first input — in one closed shape, and its
    /// receipt names the created session and the fence record.
    #[test]
    fn inv033_commission_session_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(1)?,
            ClientRequest::CommissionSession {
                command_id: command(2)?,
                template_name: String::from("review-response"),
                fence: CommissionedSessionFence::PullRequest {
                    repository: String::from("sample-user/sample-repository"),
                    pull_request: CanonicalU64::new(12),
                    head_sha: String::from("1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d"),
                    head_repository: String::from("sample-user/sample-repository"),
                    head_branch: String::from("agent/sample-feature"),
                    base_branch: String::from("main"),
                },
                statement: String::from("Address the findings on pull request 12."),
                content: InputContent::new(String::from("Respond to the review threads.")),
            },
            concat!(
                "{\"type\":\"commission_session\",",
                "\"command_id\":\"00000000-0000-0000-0000-000000000002\",",
                "\"template_name\":\"review-response\",",
                "\"fence\":{\"target\":\"pull_request\",",
                "\"repository\":\"sample-user/sample-repository\",",
                "\"pull_request\":\"12\",",
                "\"head_sha\":\"1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d\",",
                "\"head_repository\":\"sample-user/sample-repository\",",
                "\"head_branch\":\"agent/sample-feature\",",
                "\"base_branch\":\"main\"},",
                "\"statement\":\"Address the findings on pull request 12.\",",
                "\"content\":\"Respond to the review threads.\"}"
            ),
        )?;
        assert_client_request_round_trip(
            request(3)?,
            ClientRequest::CommissionSession {
                command_id: command(4)?,
                template_name: String::from("branch-watch"),
                fence: CommissionedSessionFence::Branch {
                    repository: String::from("sample-user/sample-repository"),
                    branch: String::from("main"),
                },
                statement: String::from("Investigate the failing workflow on main."),
                content: InputContent::new(String::from("The nightly workflow failed.")),
            },
            concat!(
                "{\"type\":\"commission_session\",",
                "\"command_id\":\"00000000-0000-0000-0000-000000000004\",",
                "\"template_name\":\"branch-watch\",",
                "\"fence\":{\"target\":\"branch\",",
                "\"repository\":\"sample-user/sample-repository\",",
                "\"branch\":\"main\"},",
                "\"statement\":\"Investigate the failing workflow on main.\",",
                "\"content\":\"The nightly workflow failed.\"}"
            ),
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::SessionCommissioned {
                session_id: uuid(6),
                dispatch_id: uuid(7),
            },
            concat!(
                "{\"type\":\"session_commissioned\",",
                "\"session_id\":\"00000000-0000-0000-0000-000000000006\",",
                "\"dispatch_id\":\"00000000-0000-0000-0000-000000000007\"}"
            ),
        )?;

        let zero_pull_request = ClientRequest::CommissionSession {
            command_id: command(8)?,
            template_name: String::from("review-response"),
            fence: CommissionedSessionFence::PullRequest {
                repository: String::from("sample-user/sample-repository"),
                pull_request: CanonicalU64::new(0),
                head_sha: String::from("1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d"),
                head_repository: String::from("sample-user/sample-repository"),
                head_branch: String::from("agent/sample-feature"),
                base_branch: String::from("main"),
            },
            statement: String::from("Address the findings."),
            content: InputContent::new(String::from("Respond.")),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(9)?, zero_pull_request),
            Err(FrameValidationError::DispatchFenceShape)
        );

        let empty_statement = ClientRequest::CommissionSession {
            command_id: command(10)?,
            template_name: String::from("review-response"),
            fence: CommissionedSessionFence::Branch {
                repository: String::from("sample-user/sample-repository"),
                branch: String::from("main"),
            },
            statement: String::new(),
            content: InputContent::new(String::from("Respond.")),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(11)?, empty_statement),
            Err(FrameValidationError::GoalShape)
        );

        let uppercase_template = ClientRequest::CommissionSession {
            command_id: command(12)?,
            template_name: String::from("Review-Response"),
            fence: CommissionedSessionFence::Branch {
                repository: String::from("sample-user/sample-repository"),
                branch: String::from("main"),
            },
            statement: String::from("Address the findings."),
            content: InputContent::new(String::from("Respond.")),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::One,
                request(13)?,
                uppercase_template
            ),
            Err(FrameValidationError::TemplateShape)
        );
        Ok(())
    }

    /// request shape, and a requested semantic position must be nonzero.
    #[test]
    fn inv033_compaction_request_has_an_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let compact = ClientRequest::CompactSession {
            command_id: command(1)?,
            session_id: uuid(2),
            through_position: Some(CanonicalU64::new(7)),
        };

        let frame = ClientFrame::try_new_for_version(ProtocolVersion::One, request(3)?, compact)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            concat!(
                "{\"version\":1,\"request_id\":\"3\",\"request\":{",
                "\"type\":\"compact_session\",",
                "\"command_id\":\"00000000-0000-0000-0000-000000000001\",",
                "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
                "\"through_position\":\"7\"}}\n"
            )
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        let zero = ClientRequest::CompactSession {
            command_id: command(4)?,
            session_id: uuid(5),
            through_position: Some(CanonicalU64::new(0)),
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(6)?, zero,),
            Err(FrameValidationError::ContextCompactionShape)
        );
        Ok(())
    }

    #[test]
    fn inv033_import_outcomes_have_distinct_closed_shapes() -> Result<(), Box<dyn std::error::Error>>
    {
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

    /// its exact closed shape across one encode/decode round trip.
    #[test]
    fn inv033_stop_turn_request_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>>
    {
        let request_id = request(1)?;
        let request_value = ClientRequest::StopTurn {
            command_id: command(4)?,
            session_id: uuid(6),
            expected_active_turn_id: uuid(7),
            content: UserInputContent::text(String::from("continue after the stop")),
            expected_defaults_version: CanonicalU64::new(1),
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            model_settings: ModelSettingsOverlay::inherit_all(),
        };

        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request_id, request_value)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"stop_turn\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"expected_active_turn_id\":\"00000000-0000-0000-0000-000000000007\",\
             \"content\":[{\"type\":\"text\",\"text\":\"continue after the stop\"}],\
             \"expected_defaults_version\":\"1\",\
             \"descendant_scope\":\"parent_and_descendants\",\
             \"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},\
             \"fast_mode\":{\"kind\":\"inherit\"},\
             \"service_tier\":{\"kind\":\"inherit\"}}}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: tool decisions keep exact wire forms across one round trip.
    #[test]
    fn inv033_decide_tool_request_has_exact_closed_decision_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let approval = ClientRequest::DecideToolRequest {
            command_id: command(4)?,
            session_id: uuid(6),
            tool_request_id: uuid(7),
            decision: ToolDecision::Approve {},
        };
        let approval_frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, approval)?;
        let encoded_approval = encode_client_line(&approval_frame)?;
        assert_eq!(
            String::from_utf8(encoded_approval.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"decide_tool_request\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"tool_request_id\":\"00000000-0000-0000-0000-000000000007\",\
             \"decision\":{\"type\":\"approve\"}}}\n"
        );
        assert_eq!(decode_client_line(&encoded_approval)?, approval_frame);

        let denial_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
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
            "{\"version\":1,\"request_id\":\"2\",\"request\":{\"type\":\"decide_tool_request\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000005\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"tool_request_id\":\"00000000-0000-0000-0000-000000000007\",\
             \"decision\":{\"type\":\"deny\",\"reason\":\"writes outside the workspace\"}}}\n"
        );
        assert_eq!(decode_client_line(&encoded_denial)?, denial_frame);

        assert_client_malformed(
            r#"{"version":1,"request_id":"3","request":{"type":"decide_tool_request","command_id":"00000000-0000-0000-0000-000000000004","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"approve","reason":"approve carries no reason"}}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"4","request":{"type":"decide_tool_request","command_id":"00000000-0000-0000-0000-000000000004","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"deny"}}}"#,
        );
        Ok(())
    }

    #[test]
    fn inv033_tool_approval_user_approve_event_round_trips()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(8),
                session_id: uuid(6),
                event: SessionEvent::ToolApprovalDecided {
                    turn_id: uuid(7),
                    tool_request_id: uuid(8),
                    decision: ToolApprovalEventDecision::Approve {},
                    decider: ToolApprovalEventDecider::User {
                        command_id: uuid(9),
                    },
                    rationale: None,
                },
            },
            r#"{"type":"session_event","cursor":"8","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"user","command_id":"00000000-0000-0000-0000-000000000009"},"rationale":null}}"#,
        )
    }

    #[test]
    fn inv033_tool_approval_user_deny_event_round_trips_with_reason()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(15)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(9),
                session_id: uuid(6),
                event: SessionEvent::ToolApprovalDecided {
                    turn_id: uuid(7),
                    tool_request_id: uuid(8),
                    decision: ToolApprovalEventDecision::Deny {
                        reason: Some(String::from("user declined")),
                    },
                    decider: ToolApprovalEventDecider::User {
                        command_id: uuid(9),
                    },
                    rationale: None,
                },
            },
            r#"{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":"user declined"},"decider":{"type":"user","command_id":"00000000-0000-0000-0000-000000000009"},"rationale":null}}"#,
        )
    }

    #[test]
    fn inv033_tool_approval_delegate_deny_event_round_trips_null_reason_for_empty_derivation()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(9),
                session_id: uuid(6),
                event: SessionEvent::ToolApprovalDecided {
                    turn_id: uuid(7),
                    tool_request_id: uuid(8),
                    decision: ToolApprovalEventDecision::Deny { reason: None },
                    decider: ToolApprovalEventDecider::Delegate {
                        model_selection_id: uuid(10),
                        model_call_id: uuid(11),
                    },
                    rationale: Some(String::from("   ")),
                },
            },
            r#"{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":null},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":"   "}}"#,
        )
    }

    #[test]
    fn inv033_transcript_tool_approval_round_trips_historical_delegate_provenance()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(16)?,
            ServerMessage::TranscriptEntry {
                entry_index: CanonicalU64::new(2),
                source_session_id: uuid(6),
                entry_id: uuid(7),
                entry: TranscriptEntry::AssistantToolUse {
                    turn_id: uuid(8),
                    model_call_id: uuid(9),
                    tool_request_id: uuid(10),
                    tool_name: String::from("publish"),
                    arguments: String::from("{}"),
                    approval: Some(TranscriptToolApproval {
                        decision: ToolApprovalEventDecision::Deny { reason: None },
                        decider: ToolApprovalEventDecider::Delegate {
                            model_selection_id: uuid(11),
                            model_call_id: uuid(12),
                        },
                        rationale: Some(String::from("   ")),
                    }),
                },
            },
            r#"{"type":"transcript_entry","entry_index":"2","source_session_id":"00000000-0000-0000-0000-000000000006","entry_id":"00000000-0000-0000-0000-000000000007","entry":{"type":"assistant_tool_use","turn_id":"00000000-0000-0000-0000-000000000008","model_call_id":"00000000-0000-0000-0000-000000000009","tool_request_id":"00000000-0000-0000-0000-00000000000a","tool_name":"publish","arguments":"{}","approval":{"decision":{"type":"deny","reason":null},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000b","model_call_id":"00000000-0000-0000-0000-00000000000c"},"rationale":"   "}}}"#,
        )
    }

    #[test]
    fn inv033_transcript_tool_approval_rejects_explicit_null() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"16","message":{"type":"transcript_entry","entry_index":"2","source_session_id":"00000000-0000-0000-0000-000000000006","entry_id":"00000000-0000-0000-0000-000000000007","entry":{"type":"assistant_tool_use","turn_id":"00000000-0000-0000-0000-000000000008","model_call_id":"00000000-0000-0000-0000-000000000009","tool_request_id":"00000000-0000-0000-0000-00000000000a","tool_name":"publish","arguments":"{}","approval":null}}}"#,
        );
    }

    #[test]
    fn inv033_tool_approval_user_decider_rejects_delegate_rationale() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"5","message":{"type":"session_event","cursor":"8","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"user","command_id":"00000000-0000-0000-0000-000000000009"},"rationale":"forged judge rationale"}}}"#,
        );
    }

    /// INV-033: the override request carries its exact closed wire shape.
    #[test]
    fn inv033_override_denied_tool_request_has_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let override_request = ClientRequest::OverrideDeniedToolRequest {
            command_id: command(4)?,
            session_id: uuid(6),
            tool_request_id: uuid(7),
        };
        let frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, override_request)?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            "{\"version\":1,\"request_id\":\"1\",\"request\":{\"type\":\"override_denied_tool_request\",\
             \"command_id\":\"00000000-0000-0000-0000-000000000004\",\
             \"session_id\":\"00000000-0000-0000-0000-000000000006\",\
             \"tool_request_id\":\"00000000-0000-0000-0000-000000000007\"}}\n"
        );
        assert_eq!(decode_client_line(&encoded)?, frame);

        assert_client_malformed(
            r#"{"version":1,"request_id":"2","request":{"type":"override_denied_tool_request","command_id":"00000000-0000-0000-0000-000000000004","session_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","decision":{"type":"approve"}}}"#,
        );
        Ok(())
    }

    /// INV-033: the override receipt and every override rejection carry their
    /// exact closed wire shapes.
    #[test]
    fn inv033_override_denial_responses_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::ToolDenialOverridden {
                tool_request_id: uuid(7),
            },
            r#"{"type":"tool_denial_overridden","tool_request_id":"00000000-0000-0000-0000-000000000007"}"#,
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the request carries no delegate denial"),
                detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotDelegateDenied {
                    tool_request_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"the request carries no delegate denial","detail":{"type":"tool_request_not_delegate_denied","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )?;
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the denial is still resolving"),
                detail: ErrorDetail::rejected(RejectionDetail::ToolRequestNotTerminallyDenied {
                    tool_request_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"the denial is still resolving","detail":{"type":"tool_request_not_terminally_denied","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )?;
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("an override is already recorded for the denial"),
                detail: ErrorDetail::rejected(RejectionDetail::ToolDenialAlreadyOverridden {
                    tool_request_id: uuid(7),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"an override is already recorded for the denial","detail":{"type":"tool_denial_already_overridden","tool_request_id":"00000000-0000-0000-0000-000000000007"}}"#,
        )
    }

    #[test]
    fn inv033_tool_approval_user_override_event_round_trips()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(9),
                session_id: uuid(6),
                event: SessionEvent::ToolApprovalDecided {
                    turn_id: uuid(7),
                    tool_request_id: uuid(8),
                    decision: ToolApprovalEventDecision::Approve {},
                    decider: ToolApprovalEventDecider::UserOverride {
                        command_id: uuid(9),
                        overridden_tool_request_id: uuid(12),
                    },
                    rationale: None,
                },
            },
            r#"{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"user_override","command_id":"00000000-0000-0000-0000-000000000009","overridden_tool_request_id":"00000000-0000-0000-0000-00000000000c"},"rationale":null}}"#,
        )
    }

    /// A user-override decider is approve-only and carries no rationale: a
    /// denial or a rationale under that decider is a malformed frame.
    #[test]
    fn inv033_tool_approval_user_override_decider_is_approve_only() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"6","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":null},"decider":{"type":"user_override","command_id":"00000000-0000-0000-0000-000000000009","overridden_tool_request_id":"00000000-0000-0000-0000-00000000000c"},"rationale":null}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"7","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"user_override","command_id":"00000000-0000-0000-0000-000000000009","overridden_tool_request_id":"00000000-0000-0000-0000-00000000000c"},"rationale":"forged rationale"}}}"#,
        );
    }

    #[test]
    fn inv033_tool_approval_delegate_decider_requires_rationale() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"6","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":null},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":null}}}"#,
        );
    }

    #[test]
    fn inv033_tool_approval_delegate_deny_event_round_trips_with_derived_reason()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(9),
                session_id: uuid(6),
                event: SessionEvent::ToolApprovalDecided {
                    turn_id: uuid(7),
                    tool_request_id: uuid(8),
                    decision: ToolApprovalEventDecision::Deny {
                        reason: Some(String::from("request exceeds the stated scope")),
                    },
                    decider: ToolApprovalEventDecider::Delegate {
                        model_selection_id: uuid(10),
                        model_call_id: uuid(11),
                    },
                    rationale: Some(String::from("request exceeds the stated scope")),
                },
            },
            r#"{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":"request exceeds the stated scope"},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":"request exceeds the stated scope"}}"#,
        )
    }

    #[test]
    fn inv033_tool_approval_delegate_denial_rejects_underived_reason() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"7","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"deny","reason":"forged user reason"},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":"bounded rationale"}}}"#,
        );
    }

    #[test]
    fn inv033_tool_approval_delegate_rationale_rejects_oversize() {
        const RATIONALE_FILLER: &str = "x";
        let oversized_rationale =
            RATIONALE_FILLER.repeat(ToolDecisionRationale::MAX_UTF8_BYTES + 1);
        let oversized_frame = [
            r#"{"version":1,"request_id":"10","message":{"type":"session_event","cursor":"9","session_id":"00000000-0000-0000-0000-000000000006","event":{"type":"tool_approval_decided","turn_id":"00000000-0000-0000-0000-000000000007","tool_request_id":"00000000-0000-0000-0000-000000000008","decision":{"type":"approve"},"decider":{"type":"delegate","model_selection_id":"00000000-0000-0000-0000-00000000000a","model_call_id":"00000000-0000-0000-0000-00000000000b"},"rationale":""#,
            oversized_rationale.as_str(),
            r#""}}}"#,
        ]
        .concat();

        assert_server_malformed(&oversized_frame);
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
    #[test]
    fn inv033_tool_decision_responses_have_exact_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let approval_receipt = ServerMessage::ToolRequestDecided {
            tool_request_id: uuid(7),
            decision: ToolDecision::Approve {},
        };
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
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_list_requires_cursor_member() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"50"}}"#,
        );
    }

    #[test]
    fn inv033_metadata_list_rejects_duplicate_required_tags() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":["same","same"],"title_contains":null,"include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_list_rejects_empty_title_query() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":"","include_archived":false,"page_size":"50","after_session_id":null}}"#,
        );
    }

    #[test]
    fn metadata_list_page_size_has_no_wire_policy() -> Result<(), Box<dyn std::error::Error>> {
        decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"0","after_session_id":null}}"#,
        ))?;
        decode_client_line(&line(
            r#"{"version":1,"request_id":"1","request":{"type":"list_session_metadata","required_tags":[],"title_contains":null,"include_archived":false,"page_size":"101","after_session_id":null}}"#,
        ))?;
        Ok(())
    }

    #[test]
    fn inv033_metadata_replacement_rejects_empty_title() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":"","tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_replacement_requires_title_member() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_replacement_rejects_duplicate_tags() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":null,"tags":["same","same"],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_duplicate_metadata_attribute_member_is_malformed() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"replace_session_metadata","command_id":"00000000-0000-0000-0000-000000000005","session_id":"00000000-0000-0000-0000-000000000006","metadata":{"title":null,"tags":[],"attributes":{"same":"first","\u0073ame":"second"},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_required_tag_deserializer_has_no_deployment_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let required_tags = serde_json::to_string(&numbered_metadata_strings(3))?;
        let json = format!(
            r#"{{"version":1,"request_id":"1","request":{{"type":"list_session_metadata","required_tags":{required_tags},"title_contains":null,"include_archived":false,"page_size":"50","after_session_id":null}}}}"#
        );
        decode_client_line(&line(&json))?;
        Ok(())
    }

    #[test]
    fn inv033_metadata_summary_tag_deserializer_has_no_deployment_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut tags = numbered_metadata_strings(3);
        tags.sort();
        let tags = serde_json::to_string(&tags)?;
        let json = format!(
            r#"{{"version":1,"request_id":"1","message":{{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"}},"dangerous_tool_auto_approval":false,"title":null,"tags":{tags},"archived":false,"last_writer":{{"updated_at_unix_micros":"1","actor":{{"type":"user"}}}}}}}}"#
        );
        decode_server_line(&line(&json))?;
        Ok(())
    }

    /// Pins one actor's exact bytes on both frames that carry a last writer.
    /// A failure names the actor at the call site rather than a loop position.
    #[track_caller]
    fn assert_metadata_actor_round_trips(
        actor: MetadataActor,
        expected_actor_json: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let writer = MetadataLastWriter::new(CanonicalU64::new(1), actor);
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::SessionMetadataReplaced {
                session_id: uuid(1),
                metadata: SessionMetadata::empty(),
                last_writer: writer,
            },
            &format!(
                r#"{{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{{"title":null,"tags":[],"attributes":{{}},"archived":false}},"last_writer":{{"updated_at_unix_micros":"1","actor":{expected_actor_json}}}}}"#
            ),
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::SessionMetadata {
                session_id: uuid(1),
                metadata: SessionMetadata::empty(),
                last_writer: Some(writer),
            },
            &format!(
                r#"{{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{{"title":null,"tags":[],"attributes":{{}},"archived":false}},"last_writer":{{"updated_at_unix_micros":"1","actor":{expected_actor_json}}}}}"#
            ),
        )?;
        Ok(())
    }

    /// INV-033: the last-writer actor projects every agency durable metadata can
    /// record. The domain projection this pins is total by type, so a later
    /// agency reaches the wire only through a variant added here.
    #[test]
    fn inv033_metadata_writer_actor_round_trips_every_agency()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_actor_round_trips(MetadataActor::User {}, r#"{"type":"user"}"#)?;
        assert_metadata_actor_round_trips(MetadataActor::Core {}, r#"{"type":"core"}"#)?;
        assert_metadata_actor_round_trips(
            MetadataActor::Model { turn_id: uuid(2) },
            r#"{"type":"model","turn_id":"00000000-0000-0000-0000-000000000002"}"#,
        )?;
        assert_metadata_actor_round_trips(MetadataActor::Recovery {}, r#"{"type":"recovery"}"#)?;
        assert_metadata_actor_round_trips(
            MetadataActor::Tool {
                tool_request_id: uuid(3),
            },
            r#"{"type":"tool","tool_request_id":"00000000-0000-0000-0000-000000000003"}"#,
        )?;
        Ok(())
    }

    /// INV-033: the actor vocabulary stays closed — an unadmitted spelling and a
    /// variant carrying the wrong reference are both malformed frames.
    #[test]
    fn inv033_metadata_writer_actor_rejects_unadmitted_shapes() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":{"updated_at_unix_micros":"1","actor":{"type":"operator"}}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":{"updated_at_unix_micros":"1","actor":{"type":"tool","turn_id":"00000000-0000-0000-0000-000000000002"}}}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false},"last_writer":{"updated_at_unix_micros":"1","actor":{"type":"recovery","turn_id":"00000000-0000-0000-0000-000000000002"}}}}"#,
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
            SessionMetadata::try_new_with_count_limits(
                None,
                numbered_metadata_strings(3),
                Vec::new(),
                false,
                Some(2),
                None,
            )
            .is_err()
        );
        assert!(
            SessionMetadata::try_new_with_count_limits(
                None,
                Vec::new(),
                numbered_metadata_attributes(3),
                false,
                None,
                Some(2),
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
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::SessionMetadataReplaced {
                session_id: uuid(1),
                metadata: exact,
                last_writer: MetadataLastWriter::new(CanonicalU64::new(1), MetadataActor::User {}),
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
        ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, exact)?;

        let over_total = ClientRequest::ListSessionMetadata {
            required_tags: Vec::new(),
            title_contains: Some("x".repeat(MAX_SESSION_METADATA_TOTAL_UTF8_BYTES + 1)),
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after_session_id: None,
        };
        assert_eq!(
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, over_total),
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
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, over_indexed),
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
                MetadataActor::User {},
            )),
        };

        assert_eq!(
            ServerFrame::try_new_for_version(
                ProtocolVersion::One,
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
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"},"dangerous_tool_auto_approval":false,"tags":[],"archived":false,"last_writer":null}}"#,
        );
    }

    #[test]
    fn inv033_metadata_summary_requires_nullable_last_writer_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000002"},"dangerous_tool_auto_approval":false,"title":null,"tags":[],"archived":false}}"#,
        );
    }

    #[test]
    fn inv033_metadata_page_end_requires_nullable_cursor_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata_page_end","session_count":"0"}}"#,
        );
    }

    #[test]
    fn inv033_metadata_point_read_requires_nullable_last_writer_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":null,"tags":[],"attributes":{},"archived":false}}}"#,
        );
    }

    #[test]
    fn inv033_metadata_point_read_requires_nullable_title_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"session_metadata","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"tags":[],"attributes":{},"archived":false},"last_writer":null}}"#,
        );
    }

    #[track_caller]
    fn assert_metadata_message_rejected(
        message: ServerMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, message),
            Err(FrameValidationError::MetadataShape)
        );
        Ok(())
    }

    #[test]
    fn inv033_metadata_summary_rejects_unsorted_tags() -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_message_rejected(ServerMessage::SessionMetadataSummary {
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
                MetadataActor::User {},
            )),
        })
    }

    #[test]
    fn inv033_metadata_summary_rejects_unwritten_nondefault_content()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_message_rejected(ServerMessage::SessionMetadataSummary {
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
        })
    }

    #[test]
    fn inv033_metadata_read_rejects_written_content_without_a_writer()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_message_rejected(ServerMessage::SessionMetadata {
            session_id: uuid(1),
            metadata: metadata(false)?,
            last_writer: None,
        })
    }

    #[test]
    fn inv033_metadata_page_rejects_a_cursor_after_an_empty_page()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_metadata_message_rejected(ServerMessage::SessionMetadataPageEnd {
            session_count: CanonicalU64::new(0),
            next_after_session_id: Some(uuid(1)),
        })
    }

    #[test]
    fn metadata_page_count_has_no_wire_policy() -> Result<(), Box<dyn std::error::Error>> {
        ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            ServerMessage::SessionMetadataPageEnd {
                session_count: CanonicalU64::new(101),
                next_after_session_id: None,
            },
        )?;
        Ok(())
    }

    #[test]
    fn inv033_single_vocabulary_admits_reconciliation_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let model_reconciliation = ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::ReconciliationRequired {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_model_call_id: uuid(8),
            },
        };
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(1)?,
            model_reconciliation,
        )?;
        let encoded = encode_server_line(&frame)?;
        assert!(String::from_utf8(encoded.clone())?.starts_with("{\"version\":1,"));
        assert_eq!(decode_server_line(&encoded)?, frame);

        let tool_reconciliation = ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::ToolReconciliationRequired {
                terminal_frontier_id: uuid(6),
                terminal_attempt_id: uuid(7),
                terminal_tool_attempt_id: uuid(9),
            },
        };
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(2)?,
            tool_reconciliation,
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
        assert_server_message_round_trip(
            request(3)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::ActiveAwaitingToolRecovery {
                    ended_attempt_id: uuid(7),
                    recovery_tool_attempt_id: uuid(9),
                    automatic_reconciliation_attempts: CanonicalU64::new(2),
                    operator_action_required: false,
                },
            },
            concat!(
                "{\"type\":\"transcript_turn\",\"turn_id\":\"00000000-0000-0000-0000-000000000003\",",
                "\"acceptance_position\":\"1\",\"model_settings\":null,\"state\":{",
                "\"type\":\"active_awaiting_tool_recovery\",",
                "\"ended_attempt_id\":\"00000000-0000-0000-0000-000000000007\",",
                "\"recovery_tool_attempt_id\":\"00000000-0000-0000-0000-000000000009\",",
                "\"automatic_reconciliation_attempts\":\"2\",",
                "\"operator_action_required\":false}}"
            ),
        )?;
        Ok(())
    }

    /// INV-033 / INV-048: queued goal retirement has one exact closed wire
    /// shape and round-trips its immutable turn identity.
    #[test]
    fn inv033_inv048_goal_turn_retired_event_round_trips() -> Result<(), Box<dyn std::error::Error>>
    {
        let message = ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(1),
            session_id: uuid(1),
            event: SessionEvent::GoalTurnRetired { turn_id: uuid(2) },
        };
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(3)?, message)?;

        assert_eq!(
            String::from_utf8(encode_server_line(&frame)?)?,
            concat!(
                "{\"version\":1,\"request_id\":\"3\",\"message\":{\"type\":\"session_event\",\"cursor\":\"1\",",
                "\"session_id\":\"00000000-0000-0000-0000-000000000001\",",
                "\"event\":{\"type\":\"goal_turn_retired\",",
                "\"turn_id\":\"00000000-0000-0000-0000-000000000002\"}}}\n"
            )
        );
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
        Ok(())
    }

    #[test]
    fn inv033_delegation_client_requests_round_trip_their_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        const SPAWN_FRAME_REQUEST: u64 = 34;
        const AWAIT_FRAME_REQUEST: u64 = 35;
        const MESSAGE_FRAME_REQUEST: u64 = 36;
        let ids = delegation_wire_identities();

        assert_client_request_round_trip(
            request(SPAWN_FRAME_REQUEST)?,
            ClientRequest::SpawnSession {
                session_id: ids.parent_session,
                turn_id: ids.parent_turn,
                tool_request_id: ids.spawning_request,
                task: String::from("inspect the failure"),
                relationship: DelegationPolicy::Bound {
                    on_parent_stopped: super::BoundChildAction::Stop,
                    on_parent_cancelled: super::BoundChildAction::Cancel,
                },
            },
            r#"{"type":"spawn_session","session_id":"00000000-0000-0000-0000-000000000001","turn_id":"00000000-0000-0000-0000-000000000002","tool_request_id":"00000000-0000-0000-0000-000000000003","task":"inspect the failure","relationship":{"type":"bound","on_parent_stopped":"stop","on_parent_cancelled":"cancel"}}"#,
        )?;
        assert_client_request_round_trip(
            request(AWAIT_FRAME_REQUEST)?,
            ClientRequest::AwaitSession {
                session_id: ids.parent_session,
                turn_id: ids.parent_turn,
                tool_request_id: ids.await_request,
                child_session_id: ids.child_session,
                mode: DelegationWaitMode::Foreground,
            },
            r#"{"type":"await_session","session_id":"00000000-0000-0000-0000-000000000001","turn_id":"00000000-0000-0000-0000-000000000002","tool_request_id":"00000000-0000-0000-0000-000000000004","child_session_id":"00000000-0000-0000-0000-000000000005","mode":"foreground"}"#,
        )?;
        assert_client_request_round_trip(
            request(MESSAGE_FRAME_REQUEST)?,
            ClientRequest::SendSessionMessage {
                session_id: ids.child_session,
                turn_id: ids.child_message_turn,
                tool_request_id: ids.message_request,
                peer_session_id: ids.parent_session,
                content: String::from("status update"),
            },
            r#"{"type":"send_session_message","session_id":"00000000-0000-0000-0000-000000000005","turn_id":"00000000-0000-0000-0000-000000000006","tool_request_id":"00000000-0000-0000-0000-000000000007","peer_session_id":"00000000-0000-0000-0000-000000000001","content":"status update"}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_delegation_receipts_round_trip_result_and_delivery_correlation()
    -> Result<(), Box<dyn std::error::Error>> {
        const SPAWN_RECEIPT_REQUEST: u64 = 37;
        const AWAIT_RECEIPT_REQUEST: u64 = 38;
        const RESULT_RECEIPT_REQUEST: u64 = 39;
        const MESSAGE_RECEIPT_REQUEST: u64 = 40;
        let ids = delegation_wire_identities();

        assert_server_message_round_trip(
            request(SPAWN_RECEIPT_REQUEST)?,
            ServerMessage::SessionSpawned {
                tool_request_id: ids.spawning_request,
                child_session_id: ids.child_session,
                relationship: DelegationPolicy::Background {},
            },
            r#"{"type":"session_spawned","tool_request_id":"00000000-0000-0000-0000-000000000003","child_session_id":"00000000-0000-0000-0000-000000000005","relationship":{"type":"background"}}"#,
        )?;
        assert_server_message_round_trip(
            request(AWAIT_RECEIPT_REQUEST)?,
            ServerMessage::SessionAwaitRegistered {
                tool_request_id: ids.await_request,
                child_session_id: ids.child_session,
                mode: DelegationWaitMode::Background,
            },
            r#"{"type":"session_await_registered","tool_request_id":"00000000-0000-0000-0000-000000000004","child_session_id":"00000000-0000-0000-0000-000000000005","mode":"background"}"#,
        )?;
        assert_server_message_round_trip(
            request(RESULT_RECEIPT_REQUEST)?,
            ServerMessage::ChildResult {
                await_request_id: ids.await_request,
                spawning_request_id: ids.spawning_request,
                child_session_id: ids.child_session,
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("done")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: ids.child_session,
                    child_turn_id: ids.terminal_child_turn,
                },
            },
            r#"{"type":"child_result","await_request_id":"00000000-0000-0000-0000-000000000004","spawning_request_id":"00000000-0000-0000-0000-000000000003","child_session_id":"00000000-0000-0000-0000-000000000005","outcome":"returned","content":"done","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"00000000-0000-0000-0000-000000000005","child_turn_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(MESSAGE_RECEIPT_REQUEST)?,
            ServerMessage::SessionMessageSent {
                tool_request_id: ids.message_request,
                message_id: ids.message,
                direction: DelegationMessageDirection::ChildToParent,
                ordinal: CanonicalU64::new(2),
                delivery_sequence: CanonicalU64::new(7),
            },
            r#"{"type":"session_message_sent","tool_request_id":"00000000-0000-0000-0000-000000000007","message_id":"00000000-0000-0000-0000-000000000009","direction":"child_to_parent","ordinal":"2","delivery_sequence":"7"}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_delegation_request_rejections_round_trip_closed_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        const NOT_EXECUTABLE_FRAME_REQUEST: u64 = 41;
        const ORDINAL_EXHAUSTED_FRAME_REQUEST: u64 = 42;
        const PREPARED_FRAME_REQUEST: u64 = 43;
        const APPROVED_FRAME_REQUEST: u64 = 44;
        let ids = delegation_wire_identities();

        assert_server_message_round_trip(
            request(NOT_EXECUTABLE_FRAME_REQUEST)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("delegation request is not executable"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::DelegationToolRequestNotExecutable {
                        tool_request_id: ids.spawning_request,
                        state: DelegationToolRequestState::AttemptEnded,
                    },
                ),
            },
            r#"{"type":"error","code":"rejected","message":"delegation request is not executable","detail":{"type":"delegation_tool_request_not_executable","tool_request_id":"00000000-0000-0000-0000-000000000003","state":"attempt_ended"}}"#,
        )?;
        assert_server_message_round_trip(
            request(PREPARED_FRAME_REQUEST)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("delegation request is not executable"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::DelegationToolRequestNotExecutable {
                        tool_request_id: ids.message_request,
                        state: DelegationToolRequestState::Prepared,
                    },
                ),
            },
            r#"{"type":"error","code":"rejected","message":"delegation request is not executable","detail":{"type":"delegation_tool_request_not_executable","tool_request_id":"00000000-0000-0000-0000-000000000007","state":"prepared"}}"#,
        )?;
        assert_server_message_round_trip(
            request(APPROVED_FRAME_REQUEST)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("delegation request is not executable"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::DelegationToolRequestNotExecutable {
                        tool_request_id: ids.await_request,
                        state: DelegationToolRequestState::Approved,
                    },
                ),
            },
            r#"{"type":"error","code":"rejected","message":"delegation request is not executable","detail":{"type":"delegation_tool_request_not_executable","tool_request_id":"00000000-0000-0000-0000-000000000004","state":"approved"}}"#,
        )?;
        assert_server_message_round_trip(
            request(ORDINAL_EXHAUSTED_FRAME_REQUEST)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("delegation event ordinal exhausted"),
                detail: ErrorDetail::rejected(RejectionDetail::DelegationEventOrdinalExhausted {
                    spawning_request_id: ids.spawning_request,
                    last: CanonicalU64::new(u64::MAX),
                }),
            },
            &format!(
                "{{\"type\":\"error\",\"code\":\"rejected\",\"message\":\"delegation event ordinal exhausted\",\"detail\":{{\"type\":\"delegation_event_ordinal_exhausted\",\"spawning_request_id\":\"00000000-0000-0000-0000-000000000003\",\"last\":\"{}\"}}}}",
                u64::MAX
            ),
        )?;
        Ok(())
    }

    #[test]
    fn inv033_delivery_sequence_exhaustion_round_trips_closed_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        const FRAME_REQUEST: u64 = 51;
        let ids = delegation_wire_identities();

        assert_server_message_round_trip(
            request(FRAME_REQUEST)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("delegation delivery sequence exhausted"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::DelegationDeliverySequenceExhausted {
                        recipient_session_id: ids.child_session,
                        last: CanonicalU64::new(u64::MAX),
                    },
                ),
            },
            &format!(
                "{{\"type\":\"error\",\"code\":\"rejected\",\"message\":\"delegation delivery sequence exhausted\",\"detail\":{{\"type\":\"delegation_delivery_sequence_exhausted\",\"recipient_session_id\":\"{}\",\"last\":\"{}\"}}}}",
                ids.child_session,
                u64::MAX
            ),
        )?;
        Ok(())
    }

    #[test]
    fn inv033_message_identity_collision_round_trips_closed_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        const FRAME_REQUEST: u64 = 54;
        let ids = delegation_wire_identities();

        assert_server_message_round_trip(
            request(FRAME_REQUEST)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("delegation message identity collision"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::DelegationMessageIdentityCollision {
                        message_id: ids.message,
                    },
                ),
            },
            &format!(
                "{{\"type\":\"error\",\"code\":\"rejected\",\"message\":\"delegation message identity collision\",\"detail\":{{\"type\":\"delegation_message_identity_collision\",\"message_id\":\"{}\"}}}}",
                ids.message
            ),
        )?;
        Ok(())
    }

    #[test]
    fn inv033_delivery_sequence_exhaustion_rejects_a_nonterminal_counter()
    -> Result<(), Box<dyn std::error::Error>> {
        const FRAME_REQUEST: u64 = 52;
        let ids = delegation_wire_identities();
        let frame = ServerFrame::try_new(
            request(FRAME_REQUEST)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("delegation delivery sequence exhausted"),
                detail: ErrorDetail::rejected(
                    RejectionDetail::DelegationDeliverySequenceExhausted {
                        recipient_session_id: ids.child_session,
                        last: CanonicalU64::new(u64::MAX - 1),
                    },
                ),
            },
        );

        assert_eq!(frame, Err(FrameValidationError::ErrorDetailShape));
        Ok(())
    }

    #[test]
    fn inv033_delegation_request_content_validation_is_left_to_application_input()
    -> Result<(), Box<dyn std::error::Error>> {
        const EMPTY_TASK_FRAME_REQUEST: u64 = 43;
        const NUL_MESSAGE_FRAME_REQUEST: u64 = 44;
        const OVERSIZED_TASK_FRAME_REQUEST: u64 = 45;
        let ids = delegation_wire_identities();
        let empty_task = ClientFrame::try_new(
            request(EMPTY_TASK_FRAME_REQUEST)?,
            ClientRequest::SpawnSession {
                session_id: ids.parent_session,
                turn_id: ids.parent_turn,
                tool_request_id: ids.spawning_request,
                task: String::new(),
                relationship: DelegationPolicy::Background {},
            },
        )?;
        let nul_message = ClientFrame::try_new(
            request(NUL_MESSAGE_FRAME_REQUEST)?,
            ClientRequest::SendSessionMessage {
                session_id: ids.child_session,
                turn_id: ids.child_message_turn,
                tool_request_id: ids.message_request,
                peer_session_id: ids.parent_session,
                content: String::from("status\0update"),
            },
        )?;
        let oversized_task = ClientFrame::try_new(
            request(OVERSIZED_TASK_FRAME_REQUEST)?,
            ClientRequest::SpawnSession {
                session_id: ids.parent_session,
                turn_id: ids.parent_turn,
                tool_request_id: ids.spawning_request,
                task: "x".repeat(MAX_CONTENT_FRAGMENT_BYTES + 1),
                relationship: DelegationPolicy::Background {},
            },
        )?;

        assert_eq!(
            decode_client_line(&encode_client_line(&empty_task)?)?,
            empty_task
        );
        assert_eq!(
            decode_client_line(&encode_client_line(&nul_message)?)?,
            nul_message
        );
        assert_eq!(
            decode_client_line(&encode_client_line(&oversized_task)?)?,
            oversized_task
        );
        Ok(())
    }

    #[test]
    fn inv033_parent_caused_child_results_keep_policy_action_separate_from_parent_reason()
    -> Result<(), Box<dyn std::error::Error>> {
        const STOPPED_BY_CANCEL_FRAME_REQUEST: u64 = 46;
        const CANCELLED_BY_STOP_FRAME_REQUEST: u64 = 47;
        let ids = delegation_wire_identities();
        let stopped_by_parent_cancel = ServerFrame::try_new(
            request(STOPPED_BY_CANCEL_FRAME_REQUEST)?,
            ServerMessage::ChildResult {
                await_request_id: ids.await_request,
                spawning_request_id: ids.spawning_request,
                child_session_id: ids.child_session,
                outcome: DelegationOutcome::Stopped,
                content: None,
                reason: DelegationReason::ParentCancelled,
                provenance: DelegationProvenance::ParentTurnCommand {
                    parent_session_id: ids.parent_session,
                    parent_turn_id: ids.parent_turn,
                    command_id: ids.parent_command,
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                },
            },
        )?;
        let cancelled_by_parent_stop = ServerFrame::try_new(
            request(CANCELLED_BY_STOP_FRAME_REQUEST)?,
            ServerMessage::ChildResult {
                await_request_id: ids.await_request,
                spawning_request_id: ids.spawning_request,
                child_session_id: ids.child_session,
                outcome: DelegationOutcome::Cancelled,
                content: None,
                reason: DelegationReason::ParentStopped,
                provenance: DelegationProvenance::ParentTurnCommand {
                    parent_session_id: ids.parent_session,
                    parent_turn_id: ids.parent_turn,
                    command_id: ids.parent_command,
                    descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                },
            },
        )?;

        assert_eq!(
            decode_server_line(&encode_server_line(&stopped_by_parent_cancel)?)?,
            stopped_by_parent_cancel
        );
        assert_eq!(
            decode_server_line(&encode_server_line(&cancelled_by_parent_stop)?)?,
            cancelled_by_parent_stop
        );
        Ok(())
    }

    #[test]
    fn inv033_child_result_rejects_repeated_spawn_and_await_request_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        const REPEATED_REQUEST_FRAME_REQUEST: u64 = 48;
        let ids = delegation_wire_identities();
        let repeated_request = ServerFrame::try_new(
            request(REPEATED_REQUEST_FRAME_REQUEST)?,
            ServerMessage::ChildResult {
                await_request_id: ids.spawning_request,
                spawning_request_id: ids.spawning_request,
                child_session_id: ids.child_session,
                outcome: DelegationOutcome::Returned,
                content: Some(String::from("done")),
                reason: DelegationReason::ChildCompleted,
                provenance: DelegationProvenance::ChildTurn {
                    child_session_id: ids.child_session,
                    child_turn_id: ids.terminal_child_turn,
                },
            },
        );

        assert_eq!(repeated_request, Err(FrameValidationError::DelegationShape));
        Ok(())
    }

    #[test]
    fn inv033_message_receipt_rejects_zero_delivery_sequence()
    -> Result<(), Box<dyn std::error::Error>> {
        const ZERO_DELIVERY_FRAME_REQUEST: u64 = 49;
        let ids = delegation_wire_identities();
        let zero_delivery = ServerFrame::try_new(
            request(ZERO_DELIVERY_FRAME_REQUEST)?,
            ServerMessage::SessionMessageSent {
                tool_request_id: ids.message_request,
                message_id: ids.message,
                direction: DelegationMessageDirection::ParentToChild,
                ordinal: CanonicalU64::new(2),
                delivery_sequence: CanonicalU64::new(0),
            },
        );

        assert_eq!(zero_delivery, Err(FrameValidationError::DelegationShape));
        Ok(())
    }

    #[test]
    fn inv033_await_registration_rejects_foreground_mode() -> Result<(), Box<dyn std::error::Error>>
    {
        const FOREGROUND_REGISTRATION_FRAME_REQUEST: u64 = 50;
        let ids = delegation_wire_identities();
        let foreground_registration = ServerFrame::try_new(
            request(FOREGROUND_REGISTRATION_FRAME_REQUEST)?,
            ServerMessage::SessionAwaitRegistered {
                tool_request_id: ids.await_request,
                child_session_id: ids.child_session,
                mode: DelegationWaitMode::Foreground,
            },
        );

        assert_eq!(
            foreground_registration,
            Err(FrameValidationError::DelegationShape)
        );
        Ok(())
    }

    #[test]
    fn inv033_message_receipt_rejects_the_reserved_spawn_ordinal()
    -> Result<(), Box<dyn std::error::Error>> {
        const FRAME_REQUEST: u64 = 53;
        let ids = delegation_wire_identities();
        let receipt = ServerFrame::try_new(
            request(FRAME_REQUEST)?,
            ServerMessage::SessionMessageSent {
                tool_request_id: ids.message_request,
                message_id: ids.message,
                direction: DelegationMessageDirection::ParentToChild,
                ordinal: CanonicalU64::new(1),
                delivery_sequence: CanonicalU64::new(1),
            },
        );

        assert_eq!(receipt, Err(FrameValidationError::DelegationShape));
        Ok(())
    }

    #[test]
    fn delegation_session_events_round_trip_their_closed_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(40)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::ChildSpawned {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    relationship: DelegationPolicy::Background {},
                },
            },
            r#"{"type":"session_event","cursor":"1","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"child_spawned","spawning_request_id":"00000000-0000-0000-0000-000000000002","child_session_id":"00000000-0000-0000-0000-000000000003","relationship":{"type":"background"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(41)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(2),
                session_id: uuid(1),
                event: SessionEvent::ChildWaiting {
                    await_request_id: uuid(4),
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    mode: DelegationWaitMode::Background,
                },
            },
            r#"{"type":"session_event","cursor":"2","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"child_waiting","await_request_id":"00000000-0000-0000-0000-000000000004","spawning_request_id":"00000000-0000-0000-0000-000000000002","child_session_id":"00000000-0000-0000-0000-000000000003","mode":"background"}}"#,
        )?;
        assert_server_message_round_trip(
            request(42)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(3),
                session_id: uuid(3),
                event: SessionEvent::SessionMessage {
                    spawning_request_id: uuid(2),
                    message_id: uuid(5),
                    sender_session_id: uuid(1),
                    recipient_session_id: uuid(3),
                    ordinal: CanonicalU64::new(2),
                    delivery_sequence: CanonicalU64::new(7),
                    content: String::from("status"),
                },
            },
            r#"{"type":"session_event","cursor":"3","session_id":"00000000-0000-0000-0000-000000000003","event":{"type":"session_message","spawning_request_id":"00000000-0000-0000-0000-000000000002","message_id":"00000000-0000-0000-0000-000000000005","sender_session_id":"00000000-0000-0000-0000-000000000001","recipient_session_id":"00000000-0000-0000-0000-000000000003","ordinal":"2","delivery_sequence":"7","content":"status"}}"#,
        )?;
        assert_server_message_round_trip(
            request(43)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(4),
                session_id: uuid(1),
                event: SessionEvent::ChildResult {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    outcome: DelegationOutcome::Returned,
                    content: Some(String::from("done")),
                    reason: DelegationReason::ChildCompleted,
                    provenance: DelegationProvenance::ChildTurn {
                        child_session_id: uuid(3),
                        child_turn_id: uuid(6),
                    },
                },
            },
            r#"{"type":"session_event","cursor":"4","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"child_result","spawning_request_id":"00000000-0000-0000-0000-000000000002","child_session_id":"00000000-0000-0000-0000-000000000003","outcome":"returned","content":"done","reason":"child_completed","provenance":{"type":"child_turn","child_session_id":"00000000-0000-0000-0000-000000000003","child_turn_id":"00000000-0000-0000-0000-000000000006"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(44)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(5),
                session_id: uuid(1),
                event: SessionEvent::ChildLifecycleDisposition {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    outcome: DelegationOutcome::Stopped,
                    reason: DelegationReason::ParentStopped,
                    provenance: DelegationProvenance::ParentTurnCommand {
                        parent_session_id: uuid(1),
                        parent_turn_id: uuid(7),
                        command_id: uuid(8),
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                },
            },
            r#"{"type":"session_event","cursor":"5","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"child_lifecycle_disposition","spawning_request_id":"00000000-0000-0000-0000-000000000002","child_session_id":"00000000-0000-0000-0000-000000000003","outcome":"stopped","reason":"parent_stopped","provenance":{"type":"parent_turn_command","parent_session_id":"00000000-0000-0000-0000-000000000001","parent_turn_id":"00000000-0000-0000-0000-000000000007","command_id":"00000000-0000-0000-0000-000000000008","descendant_scope":"parent_and_descendants"}}}"#,
        )?;
        Ok(())
    }

    /// Round trips one child-addressed lifecycle disposition through the frame
    /// validator. The header session is the terminalized child, and the
    /// canonical provenance names the commanding parent's descendant-scoped
    /// turn command.
    #[track_caller]
    fn assert_child_addressed_disposition_round_trips(
        outcome: DelegationOutcome,
        reason: DelegationReason,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame = ServerFrame::try_new(
            request(1)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(5),
                session_id: uuid(3),
                event: SessionEvent::ChildLifecycleDisposition {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    outcome,
                    reason,
                    provenance: DelegationProvenance::ParentTurnCommand {
                        parent_session_id: uuid(1),
                        parent_turn_id: uuid(7),
                        command_id: uuid(8),
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                },
            },
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
        Ok(())
    }

    #[test]
    fn child_addressed_lifecycle_disposition_round_trips_for_a_child_follower()
    -> Result<(), Box<dyn std::error::Error>> {
        // A descendant cascade addresses the terminalization to the child
        // itself so live child followers observe it. A bound relationship maps
        // the parent verb through its own policy, so all four outcome and
        // reason pairs reach the child follower.
        assert_child_addressed_disposition_round_trips(
            DelegationOutcome::Stopped,
            DelegationReason::ParentStopped,
        )?;
        assert_child_addressed_disposition_round_trips(
            DelegationOutcome::Cancelled,
            DelegationReason::ParentCancelled,
        )?;
        assert_child_addressed_disposition_round_trips(
            DelegationOutcome::Stopped,
            DelegationReason::ParentCancelled,
        )?;
        assert_child_addressed_disposition_round_trips(
            DelegationOutcome::Cancelled,
            DelegationReason::ParentStopped,
        )?;
        Ok(())
    }

    #[test]
    fn child_addressed_lifecycle_disposition_rejects_non_terminal_and_self_authored_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        // Continue-running and already-terminal remain parent-addressed only:
        // they report a child the cascade did not terminalize.
        let child_addressed_continue = ServerFrame::try_new(
            request(1)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(3),
                event: SessionEvent::ChildLifecycleDisposition {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    outcome: DelegationOutcome::ContinueRunning,
                    reason: DelegationReason::ParentStopped,
                    provenance: DelegationProvenance::ParentTurnCommand {
                        parent_session_id: uuid(1),
                        parent_turn_id: uuid(7),
                        command_id: uuid(8),
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                },
            },
        );
        // A child-addressed row must carry a foreign parent's authority; it can
        // never name itself as the commanding parent.
        let self_commanded = ServerFrame::try_new(
            request(2)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(2),
                session_id: uuid(3),
                event: SessionEvent::ChildLifecycleDisposition {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    outcome: DelegationOutcome::Stopped,
                    reason: DelegationReason::ParentStopped,
                    provenance: DelegationProvenance::ParentTurnCommand {
                        parent_session_id: uuid(3),
                        parent_turn_id: uuid(7),
                        command_id: uuid(8),
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                },
            },
        );
        // The parent-alone scope carries no descendant authority either way.
        let child_addressed_parent_alone = ServerFrame::try_new(
            request(3)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(3),
                session_id: uuid(3),
                event: SessionEvent::ChildLifecycleDisposition {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    outcome: DelegationOutcome::Stopped,
                    reason: DelegationReason::ParentStopped,
                    provenance: DelegationProvenance::ParentTurnCommand {
                        parent_session_id: uuid(1),
                        parent_turn_id: uuid(7),
                        command_id: uuid(8),
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                },
            },
        );

        assert_eq!(
            child_addressed_continue,
            Err(FrameValidationError::DelegationShape)
        );
        assert_eq!(self_commanded, Err(FrameValidationError::DelegationShape));
        assert_eq!(
            child_addressed_parent_alone,
            Err(FrameValidationError::DelegationShape)
        );
        Ok(())
    }

    #[test]
    fn delegation_session_events_reject_contradictory_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let missing_returned_content = ServerFrame::try_new(
            request(45)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::ChildResult {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    outcome: DelegationOutcome::Returned,
                    content: None,
                    reason: DelegationReason::ChildCompleted,
                    provenance: DelegationProvenance::ChildTurn {
                        child_session_id: uuid(3),
                        child_turn_id: uuid(6),
                    },
                },
            },
        );
        let parent_alone_disposition = ServerFrame::try_new(
            request(46)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(2),
                session_id: uuid(1),
                event: SessionEvent::ChildLifecycleDisposition {
                    spawning_request_id: uuid(2),
                    child_session_id: uuid(3),
                    outcome: DelegationOutcome::ContinueRunning,
                    reason: DelegationReason::ParentStopped,
                    provenance: DelegationProvenance::ParentTurnCommand {
                        parent_session_id: uuid(1),
                        parent_turn_id: uuid(7),
                        command_id: uuid(8),
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                },
            },
        );
        let zero_message_sequence = ServerFrame::try_new(
            request(47)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(3),
                session_id: uuid(1),
                event: SessionEvent::SessionMessage {
                    spawning_request_id: uuid(2),
                    message_id: uuid(5),
                    sender_session_id: uuid(1),
                    recipient_session_id: uuid(3),
                    ordinal: CanonicalU64::new(1),
                    delivery_sequence: CanonicalU64::new(0),
                    content: String::from("status"),
                },
            },
        );

        assert_eq!(
            missing_returned_content,
            Err(FrameValidationError::DelegationShape)
        );
        assert_eq!(
            parent_alone_disposition,
            Err(FrameValidationError::DelegationShape)
        );
        assert_eq!(
            zero_message_sequence,
            Err(FrameValidationError::DelegationShape)
        );
        Ok(())
    }

    #[test]
    fn inv033_inherits_imported_transcript_and_tool_event_shapes()
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
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(1)?, imported)?;
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
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(2)?, tool_event)?;
        assert_eq!(
            decode_server_line(&encode_server_line(&tool_frame)?)?,
            tool_frame
        );
        Ok(())
    }

    /// INV-033: an explicit null is not a member of the closed delivery vocabulary.
    #[test]
    fn inv033_submit_delivery_rejects_explicit_null() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"submit_input","command_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","content":"content","expected_defaults_version":"1","delivery":null}}"#,
        );
    }

    /// INV-033: steering has one exact closed shape.
    #[test]
    fn inv033_steering_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
        let steering_request = ClientRequest::SubmitInput {
            command_id: command(1)?,
            session_id: uuid(2),
            content: UserInputContent::text(String::from("steering")),
            expected_defaults_version: None,
            model_settings: ModelSettingsOverlay::inherit_all(),
            delivery: Some(InputDelivery::Steer {
                expected_active_turn_id: uuid(3),
            }),
        };
        let steering_frame =
            ClientFrame::try_new_for_version(ProtocolVersion::One, request(1)?, steering_request)?;
        let encoded = encode_client_line(&steering_frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            concat!(
                "{\"version\":1,\"request_id\":\"1\",\"request\":{",
                "\"type\":\"submit_input\",",
                "\"command_id\":\"00000000-0000-0000-0000-000000000001\",",
                "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
                "\"content\":[{\"type\":\"text\",\"text\":\"steering\"}],",
                "\"expected_defaults_version\":null,",
                "\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
                "\"fast_mode\":{\"kind\":\"inherit\"},",
                "\"service_tier\":{\"kind\":\"inherit\"}},",
                "\"delivery\":{\"type\":\"steer\",",
                "\"expected_active_turn_id\":",
                "\"00000000-0000-0000-0000-000000000003\"}}}\n"
            )
        );
        assert_eq!(decode_client_line(&encoded)?, steering_frame);
        Ok(())
    }

    /// INV-033: queueing carries its exact active-turn and defaults guards.
    #[test]
    fn inv033_queueing_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>> {
        let queue_frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(2)?,
            ClientRequest::SubmitInput {
                command_id: command(4)?,
                session_id: uuid(2),
                content: UserInputContent::text(String::from("queued")),
                expected_defaults_version: Some(CanonicalU64::new(7)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Queue {
                    expected_active_turn_id: uuid(3),
                }),
            },
        )?;
        let encoded = encode_client_line(&queue_frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            concat!(
                "{\"version\":1,\"request_id\":\"2\",\"request\":{",
                "\"type\":\"submit_input\",",
                "\"command_id\":\"00000000-0000-0000-0000-000000000004\",",
                "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
                "\"content\":[{\"type\":\"text\",\"text\":\"queued\"}],",
                "\"expected_defaults_version\":\"7\",",
                "\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
                "\"fast_mode\":{\"kind\":\"inherit\"},",
                "\"service_tier\":{\"kind\":\"inherit\"}},",
                "\"delivery\":{\"type\":\"queue\",",
                "\"expected_active_turn_id\":",
                "\"00000000-0000-0000-0000-000000000003\"}}}\n"
            )
        );
        assert_eq!(decode_client_line(&encoded)?, queue_frame);
        Ok(())
    }

    /// INV-033: explicit start-when-idle has one closed shape.
    #[test]
    fn inv033_explicit_start_when_idle_has_a_closed_shape() -> Result<(), Box<dyn std::error::Error>>
    {
        let frame = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(3)?,
            ClientRequest::SubmitInput {
                command_id: command(5)?,
                session_id: uuid(2),
                content: UserInputContent::text(String::from("start")),
                expected_defaults_version: Some(CanonicalU64::new(7)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::StartWhenIdle {}),
            },
        )?;
        let encoded = encode_client_line(&frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            concat!(
                "{\"version\":1,\"request_id\":\"3\",\"request\":{",
                "\"type\":\"submit_input\",",
                "\"command_id\":\"00000000-0000-0000-0000-000000000005\",",
                "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
                "\"content\":[{\"type\":\"text\",\"text\":\"start\"}],",
                "\"expected_defaults_version\":\"7\",",
                "\"model_settings\":{\"reasoning_level\":{\"kind\":\"inherit\"},",
                "\"fast_mode\":{\"kind\":\"inherit\"},",
                "\"service_tier\":{\"kind\":\"inherit\"}},",
                "\"delivery\":{\"type\":\"start_when_idle\"}}}\n"
            )
        );
        assert_eq!(decode_client_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-033: configured start and queue treatments reject a missing
    /// defaults guard before encoding.
    #[test]
    fn inv033_configured_delivery_rejects_missing_defaults()
    -> Result<(), Box<dyn std::error::Error>> {
        let start = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(4)?,
            ClientRequest::SubmitInput {
                command_id: command(6)?,
                session_id: uuid(2),
                content: UserInputContent::text(String::from("start without defaults")),
                expected_defaults_version: None,
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::StartWhenIdle {}),
            },
        );
        assert_eq!(start, Err(FrameValidationError::InputDeliveryShape));

        let queue = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(5)?,
            ClientRequest::SubmitInput {
                command_id: command(7)?,
                session_id: uuid(2),
                content: UserInputContent::text(String::from("queue without defaults")),
                expected_defaults_version: None,
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Queue {
                    expected_active_turn_id: uuid(3),
                }),
            },
        );
        assert_eq!(queue, Err(FrameValidationError::InputDeliveryShape));
        Ok(())
    }

    /// INV-033: configuration-free steering rejects an independently supplied
    /// defaults version before encoding.
    #[test]
    fn inv033_steering_rejects_independent_defaults_configuration()
    -> Result<(), Box<dyn std::error::Error>> {
        let invalid = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(3)?,
            ClientRequest::SubmitInput {
                command_id: command(5)?,
                session_id: uuid(2),
                content: UserInputContent::text(String::from("misconfigured steering")),
                expected_defaults_version: Some(CanonicalU64::new(7)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Steer {
                    expected_active_turn_id: uuid(3),
                }),
            },
        );
        assert_eq!(invalid, Err(FrameValidationError::InputDeliveryShape));

        let zero = ClientFrame::try_new_for_version(
            ProtocolVersion::One,
            request(4)?,
            ClientRequest::SubmitInput {
                command_id: command(6)?,
                session_id: uuid(2),
                content: UserInputContent::text(String::from("zero-version steering")),
                expected_defaults_version: Some(CanonicalU64::new(0)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Steer {
                    expected_active_turn_id: uuid(3),
                }),
            },
        );
        assert_eq!(zero, Err(FrameValidationError::InputDeliveryShape));
        Ok(())
    }

    /// INV-033: steering against an already-stopping turn carries the exact
    #[test]
    fn inv033_stopping_steering_rejection_has_exact_closed_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(4)?,
            ServerMessage::Error {
                code: ErrorCode::Rejected,
                message: String::from("the active turn is already stopping"),
                detail: ErrorDetail::rejected(RejectionDetail::SafePointUnavailableWhileStopping {
                    session_id: uuid(2),
                    active_turn_id: uuid(3),
                    existing_command_id: uuid(5),
                }),
            },
            r#"{"type":"error","code":"rejected","message":"the active turn is already stopping","detail":{"type":"safe_point_unavailable_while_stopping","session_id":"00000000-0000-0000-0000-000000000002","active_turn_id":"00000000-0000-0000-0000-000000000003","existing_command_id":"00000000-0000-0000-0000-000000000005"}}"#,
        )
    }

    /// its accepted input, position, and exact source turn.
    #[test]
    fn inv033_steering_receipt_has_an_exact_closed_shape() -> Result<(), Box<dyn std::error::Error>>
    {
        let steering_response = ServerMessage::SteeringSubmitted {
            session_id: uuid(2),
            accepted_input_id: uuid(6),
            acceptance_position: CanonicalU64::new(8),
            source_turn_id: uuid(3),
        };
        let response_frame =
            ServerFrame::try_new_for_version(ProtocolVersion::One, request(4)?, steering_response)?;
        let encoded = encode_server_line(&response_frame)?;
        assert_eq!(
            String::from_utf8(encoded.clone())?,
            concat!(
                "{\"version\":1,\"request_id\":\"4\",\"message\":{",
                "\"type\":\"steering_submitted\",",
                "\"session_id\":\"00000000-0000-0000-0000-000000000002\",",
                "\"accepted_input_id\":",
                "\"00000000-0000-0000-0000-000000000006\",",
                "\"acceptance_position\":\"8\",\"source_turn_id\":",
                "\"00000000-0000-0000-0000-000000000003\"}}\n"
            )
        );
        assert_eq!(decode_server_line(&encoded)?, response_frame);
        Ok(())
    }

    #[test]
    fn submit_content_bound_is_enforced_before_wire_encoding()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = "x".repeat(MAX_CONTENT_FRAGMENT_BYTES + 1);
        let result = ClientFrame::try_new(
            request(1)?,
            ClientRequest::SubmitInput {
                command_id: command(5)?,
                session_id: uuid(6),
                content: UserInputContent::text(content),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        );
        assert_eq!(result, Err(FrameValidationError::UserContentShape));
        Ok(())
    }

    #[test]
    fn inv033_server_message_family_has_exact_closed_wire_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::SessionCreated {
                session_id: uuid(1),
                model_settings: provider_default_settings_snapshot_fixture(),
            },
            &format!(
                "{{\"type\":\"session_created\",\"session_id\":\"00000000-0000-0000-0000-000000000001\",\"model_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON}}}"
            ),
        )?;
        assert_server_message_round_trip(
            request(2)?,
            ServerMessage::InputSubmitted {
                session_id: uuid(1),
                accepted_input_id: uuid(2),
                acceptance_position: CanonicalU64::new(1),
                turn_id: uuid(3),
                model_settings: settings_snapshot_fixture(),
            },
            &format!(
                "{{\"type\":\"input_submitted\",\"session_id\":\"00000000-0000-0000-0000-000000000001\",\"accepted_input_id\":\"00000000-0000-0000-0000-000000000002\",\"acceptance_position\":\"1\",\"turn_id\":\"00000000-0000-0000-0000-000000000003\",\"model_settings\":{SETTINGS_SNAPSHOT_JSON}}}"
            ),
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
                placement_version: CanonicalU64::new(1),
                placement: super::SessionPlacement::Pathless {},
                runner: None,
            },
            r#"{"type":"session_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"1","model_selection":{"kind":"alias","alias_id":"00000000-0000-0000-0000-000000000004"},"placement_version":"1","placement":{"kind":"pathless"},"runner":null}"#,
        )?;
        assert_server_message_round_trip(
            request(5)?,
            ServerMessage::SessionsEnd {
                session_count: CanonicalU64::new(1),
            },
            r#"{"type":"sessions_end","session_count":"1"}"#,
        )?;
        let writer = MetadataLastWriter::new(CanonicalU64::new(17), MetadataActor::User {});
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
            r#"{"type":"session_metadata_summary","session_id":"00000000-0000-0000-0000-000000000001","defaults_version":"2","model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"dangerous_tool_auto_approval":false,"title":"Planning","tags":["daily","work"],"archived":true,"last_writer":{"updated_at_unix_micros":"17","actor":{"type":"user"}}}"#,
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
            r#"{"type":"session_metadata_replaced","session_id":"00000000-0000-0000-0000-000000000001","metadata":{"title":"Planning","tags":["daily","work"],"attributes":{"run":"17","trigger":""},"archived":true},"last_writer":{"updated_at_unix_micros":"17","actor":{"type":"user"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(6)?,
            ServerMessage::TranscriptSnapshotStart {
                session_id: uuid(1),
                cursor: CanonicalU64::new(5),
                runner: None,
            },
            r#"{"type":"transcript_snapshot_start","session_id":"00000000-0000-0000-0000-000000000001","cursor":"5","runner":null}"#,
        )?;
        assert_server_message_round_trip(
            request(7)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Refused {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: uuid(8),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"refused","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(14)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id: uuid(2),
                    content: UserInputContent::text("queued request".to_owned()),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"text","text":"queued request"}]}}"#,
        )?;
        assert_server_message_round_trip(
            request(15)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::ActiveRunning {
                    current_attempt_id: uuid(7),
                    current_model_call: None,
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":null}}"#,
        )?;
        assert_server_message_round_trip(
            request(16)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::ActiveRunning {
                    current_attempt_id: uuid(7),
                    current_model_call: Some(CurrentModelCall::new(
                        uuid(8),
                        CurrentModelCallState::Prepared {},
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"prepared"}}}}"#,
        )?;
        assert_server_message_round_trip(
            request(17)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::ActiveRunning {
                    current_attempt_id: uuid(7),
                    current_model_call: Some(CurrentModelCall::new(
                        uuid(8),
                        CurrentModelCallState::InFlight {},
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"in_flight"}}}}"#,
        )?;
        assert_server_message_round_trip(
            request(20)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::ActiveRunning {
                    current_attempt_id: uuid(7),
                    current_model_call: Some(CurrentModelCall::new(
                        uuid(8),
                        CurrentModelCallState::CancellationRequested {},
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"active_running","current_attempt_id":"00000000-0000-0000-0000-000000000007","current_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","state":{"type":"cancellation_requested"}}}}"#,
        )?;
        assert_server_message_round_trip(
            request(21)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: None,
                    terminal_model_call: None,
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":null,"terminal_model_call":null}}"#,
        )?;
        assert_server_message_round_trip(
            request(22)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: Some(uuid(7)),
                    terminal_model_call: None,
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":null}}"#,
        )?;
        assert_server_message_round_trip(
            request(23)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: Some(uuid(7)),
                    terminal_model_call: Some(FailedTerminalModelCall::new(
                        uuid(8),
                        FailedModelCallDisposition::KnownFailed,
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","disposition":"known_failed"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(24)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Failed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: Some(uuid(7)),
                    terminal_model_call: Some(FailedTerminalModelCall::new(
                        uuid(8),
                        FailedModelCallDisposition::Cancelled,
                    )),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"failed","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call":{"model_call_id":"00000000-0000-0000-0000-000000000008","disposition":"cancelled"}}}"#,
        )?;
        assert_server_message_round_trip(
            request(25)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Cancelled {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: None,
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":null}}"#,
        )?;
        assert_server_message_round_trip(
            request(26)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Cancelled {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: Some(uuid(8)),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"cancelled","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
        )?;
        assert_server_message_round_trip(
            request(27)?,
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::ReconciliationRequired {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: uuid(8),
                },
            },
            r#"{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","model_settings":null,"state":{"type":"reconciliation_required","terminal_frontier_id":"00000000-0000-0000-0000-000000000006","terminal_attempt_id":"00000000-0000-0000-0000-000000000007","terminal_model_call_id":"00000000-0000-0000-0000-000000000008"}}"#,
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
                    content: UserInputContent::text("accepted request".to_owned()),
                },
            },
            r#"{"type":"session_event","cursor":"2","session_id":"00000000-0000-0000-0000-000000000001","event":{"type":"input_accepted","accepted_input_id":"00000000-0000-0000-0000-000000000002","turn_id":"00000000-0000-0000-0000-000000000003","acceptance_position":"1","content":[{"type":"text","text":"accepted request"}]}}"#,
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

    #[test]
    fn model_capability_catalog_wire_vocabulary_is_exact() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_client_request_round_trip(
            request(41)?,
            ClientRequest::ListModelCapabilities {},
            r#"{"type":"list_model_capabilities"}"#,
        )?;
        assert_server_message_round_trip(
            request(41)?,
            ServerMessage::ModelCapabilityItem {
                selection_id: uuid(4),
                capabilities: ModelCapabilities {
                    reasoning_levels: vec![ReasoningLevel::Low, ReasoningLevel::XHigh],
                    fast_mode_supported: true,
                    service_tiers: vec![ServiceTier::OpenAi(OpenAiServiceTier::Priority)],
                },
            },
            r#"{"type":"model_capability_item","selection_id":"00000000-0000-0000-0000-000000000004","capabilities":{"reasoning_levels":["low","xhigh"],"fast_mode_supported":true,"service_tiers":[{"provider":"open_ai","value":"priority"}]}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn runner_projection_round_trips_complete_current_loss()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::TranscriptSnapshotStart {
                session_id: uuid(1),
                cursor: CanonicalU64::new(9),
                runner: Some(RunnerProjection::try_new(
                    RunnerProjectionSelector::CapabilityClass {
                        name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))?,
                    },
                    Some(uuid(2)),
                    RunnerPlacementRevision::try_new(3).expect("the fixture revision is positive"),
                    RunnerSandboxProfile::WorkspaceRestricted,
                    Some(RunnerCredentialProfileName::try_new(String::from(
                        "readonly",
                    ))?),
                    Some(RunnerRepositoryKey::try_new(String::from("signalbox"))?),
                    Some(RunnerWorkingDirectory::try_new(String::from(
                        "workspace/project",
                    ))?),
                    None,
                    RunnerProjectionState::RunnerLost,
                )?),
            },
            r#"{"type":"transcript_snapshot_start","session_id":"00000000-0000-0000-0000-000000000001","cursor":"9","runner":{"selector":{"type":"capability_class","name":"linux.workspace"},"runner_id":"00000000-0000-0000-0000-000000000002","placement_revision":"3","sandbox_profile":"workspace-restricted","credential_profile":"readonly","repository":"signalbox","working_directory":"workspace/project","connection_health":null,"state":"runner_lost"}}"#,
        )
    }

    #[test]
    fn runner_projection_round_trips_pinned_suspect_health()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::TranscriptSnapshotStart {
                session_id: uuid(1),
                cursor: CanonicalU64::new(9),
                runner: Some(RunnerProjection::try_new(
                    RunnerProjectionSelector::Runner { runner_id: uuid(2) },
                    Some(uuid(2)),
                    RunnerPlacementRevision::try_new(3).expect("the fixture revision is positive"),
                    RunnerSandboxProfile::WorkspaceRestricted,
                    None,
                    None,
                    None,
                    Some(RunnerConnectionHealth::Suspect),
                    RunnerProjectionState::Pinned,
                )?),
            },
            r#"{"type":"transcript_snapshot_start","session_id":"00000000-0000-0000-0000-000000000001","cursor":"9","runner":{"selector":{"type":"runner","runner_id":"00000000-0000-0000-0000-000000000002"},"runner_id":"00000000-0000-0000-0000-000000000002","placement_revision":"3","sandbox_profile":"workspace-restricted","credential_profile":null,"repository":null,"working_directory":null,"connection_health":"suspect","state":"pinned"}}"#,
        )
    }

    #[test]
    fn runner_projection_rejects_cross_wired_exact_runner() {
        let selected = uuid(1);
        let current = uuid(2);
        let projection = RunnerProjection::try_new(
            RunnerProjectionSelector::Runner {
                runner_id: selected,
            },
            Some(current),
            RunnerPlacementRevision::try_new(1).expect("the fixture revision is positive"),
            RunnerSandboxProfile::WorkspaceRestricted,
            None,
            None,
            None,
            None,
            RunnerProjectionState::RunnerLostBeforePin,
        );

        assert_eq!(
            projection,
            Err(super::CanonicalValueError::RunnerProjection)
        );
    }

    #[test]
    fn runner_projection_rejects_loss_without_exact_runner() {
        let projection = RunnerProjection::try_new(
            RunnerProjectionSelector::CapabilityClass {
                name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))
                    .expect("the fixture capability is valid"),
            },
            None,
            RunnerPlacementRevision::try_new(1).expect("the fixture revision is positive"),
            RunnerSandboxProfile::WorkspaceRestricted,
            None,
            None,
            None,
            None,
            RunnerProjectionState::RunnerLost,
        );

        assert_eq!(
            projection,
            Err(super::CanonicalValueError::RunnerProjection)
        );
    }

    #[test]
    fn runner_projection_rejects_capability_selector_for_pre_pin_loss() {
        let projection = RunnerProjection::try_new(
            RunnerProjectionSelector::CapabilityClass {
                name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))
                    .expect("the fixture capability is valid"),
            },
            Some(uuid(1)),
            RunnerPlacementRevision::try_new(1).expect("the fixture revision is positive"),
            RunnerSandboxProfile::WorkspaceRestricted,
            None,
            None,
            None,
            None,
            RunnerProjectionState::RunnerLostBeforePin,
        );

        assert_eq!(
            projection,
            Err(super::CanonicalValueError::RunnerProjection)
        );
    }

    #[test]
    fn turn_settings_event_round_trip_preserves_override_provenance()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::SessionEvent {
            cursor: CanonicalU64::new(9),
            session_id: uuid(1),
            event: SessionEvent::TurnModelSettingsResolved {
                accepted_input_id: uuid(2),
                turn_id: uuid(3),
                defaults_version: CanonicalU64::new(7),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: settings_snapshot_fixture().precedence.per_call,
                settings: settings_snapshot_fixture(),
                adjusted_from_selection_id: None,
                adjustments: Vec::new(),
            },
        };

        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(42)?, message)?;
        let encoded = encode_server_line(&frame)?;
        let decoded = decode_server_line(&encoded)?;

        assert_eq!(decoded, frame);
        Ok(())
    }

    /// INV-032 / INV-053: a late follower's authoritative turn projection
    /// carries the same complete frozen settings evidence as the durable event.
    #[test]
    fn inv032_inv053_transcript_turn_round_trips_frozen_settings()
    -> Result<(), Box<dyn std::error::Error>> {
        let settings = settings_snapshot_fixture();
        let message = ServerMessage::TranscriptTurn {
            turn_id: uuid(3),
            acceptance_position: CanonicalU64::new(1),
            model_settings: Some(TurnModelSettingsSnapshot {
                turn_id: uuid(3),
                accepted_input_id: uuid(2),
                defaults_version: CanonicalU64::new(7),
                requested_model: ModelSelection::Direct {
                    selection_id: uuid(4),
                },
                selected_direct_id: uuid(4),
                per_call_override: settings.precedence.per_call,
                settings,
                adjusted_from_selection_id: None,
                adjustments: Vec::new(),
            }),
            state: TurnState::Queued {
                accepted_input_id: uuid(2),
                content: UserInputContent::text("settings-aware turn".to_owned()),
            },
        };
        let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(43)?, message)?;
        let encoded = encode_server_line(&frame)?;

        assert_eq!(decode_server_line(&encoded)?, frame);
        Ok(())
    }

    /// INV-012: queued user content is validated before a server frame can be
    /// encoded, including when no model-settings snapshot is present.
    #[test]
    fn inv012_transcript_turn_rejects_invalid_queued_content_before_encoding() {
        let result = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id: uuid(2),
                    content: UserInputContent::from_parts(Vec::new()),
                },
            },
        );

        assert_eq!(result, Err(FrameValidationError::UserContentShape));
    }

    /// INV-033: queued turn settings evidence belongs to the accepted input
    /// named by the authoritative queued state.
    #[test]
    fn inv033_transcript_turn_rejects_settings_for_another_queued_input() {
        let settings = settings_snapshot_fixture();
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: Some(TurnModelSettingsSnapshot {
                    turn_id: uuid(3),
                    accepted_input_id: uuid(5),
                    defaults_version: CanonicalU64::new(7),
                    requested_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    selected_direct_id: uuid(4),
                    per_call_override: settings.precedence.per_call,
                    settings,
                    adjusted_from_selection_id: None,
                    adjustments: Vec::new(),
                }),
                state: TurnState::Queued {
                    accepted_input_id: uuid(2),
                    content: UserInputContent::text("settings-aware turn".to_owned()),
                },
            },
        )
        .expect_err("queued settings must name the queued accepted input");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: terminal turn settings evidence belongs to the turn named by
    /// the authoritative transcript projection.
    #[test]
    fn inv033_transcript_turn_rejects_settings_for_another_terminal_turn() {
        let settings = settings_snapshot_fixture();
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::TranscriptTurn {
                turn_id: uuid(3),
                acceptance_position: CanonicalU64::new(1),
                model_settings: Some(TurnModelSettingsSnapshot {
                    turn_id: uuid(5),
                    accepted_input_id: uuid(2),
                    defaults_version: CanonicalU64::new(7),
                    requested_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    selected_direct_id: uuid(4),
                    per_call_override: settings.precedence.per_call,
                    settings,
                    adjusted_from_selection_id: None,
                    adjustments: Vec::new(),
                }),
                state: TurnState::Completed {
                    terminal_frontier_id: uuid(6),
                    terminal_attempt_id: uuid(7),
                    terminal_model_call_id: uuid(8),
                },
            },
        )
        .expect_err("terminal settings must name the projected turn");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: required-nullable turn settings cannot be omitted.
    #[test]
    fn inv033_transcript_turn_requires_model_settings_member() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000001","acceptance_position":"1","state":{"type":"queued","accepted_input_id":"00000000-0000-0000-0000-000000000002","content":[{"type":"text","text":"queued request"}]}}}"#,
        );
    }

    /// INV-033: complete settings snapshots cannot contradict their retained
    /// precedence provenance.
    #[test]
    fn inv033_model_settings_snapshot_rejects_inconsistent_effective_values() {
        let mut model_settings = session_settings_snapshot_fixture();
        model_settings.effective.reasoning_level = Some(ReasoningLevel::Low);
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionCreated {
                session_id: uuid(1),
                model_settings,
            },
        )
        .expect_err("effective settings must resolve from retained provenance");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: only the exact all-inherit provider-default snapshot is
    /// model-independent.
    #[test]
    fn inv033_nondefault_settings_snapshot_requires_validation_identity() {
        let mut model_settings = session_settings_snapshot_fixture();
        model_settings.validated_for_selection_id = None;

        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionCreated {
                session_id: uuid(1),
                model_settings,
            },
        )
        .expect_err("nondefault settings require their validating direct selection");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: durable defaults snapshots cannot retain a per-call layer.
    #[test]
    fn inv033_defaults_snapshot_rejects_per_call_settings() {
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionCreated {
                session_id: uuid(1),
                model_settings: settings_snapshot_fixture(),
            },
        )
        .expect_err("defaults cannot retain an origin-only per-call contribution");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: the separately reported per-call contribution must equal the
    /// retained precedence layer.
    #[test]
    fn inv033_turn_settings_event_rejects_crosswired_per_call_override() {
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::TurnModelSettingsResolved {
                    accepted_input_id: uuid(2),
                    turn_id: uuid(3),
                    defaults_version: CanonicalU64::new(1),
                    requested_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    selected_direct_id: uuid(4),
                    per_call_override: ModelSettingsOverlay::inherit_all(),
                    settings: settings_snapshot_fixture(),
                    adjusted_from_selection_id: None,
                    adjustments: Vec::new(),
                },
            },
        )
        .expect_err("event provenance must match the sealed per-call layer");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: adjustments require a distinct prior direct validation identity.
    #[test]
    fn inv033_turn_settings_event_rejects_unchanged_adjustment_source() {
        let mut settings = settings_snapshot_fixture();
        settings.precedence.session = settings.precedence.per_call;
        settings.precedence.per_call = ModelSettingsOverlay::inherit_all();
        settings.reasoning_source = Some(ModelSettingSource::Session);
        settings.service_tier_source = Some(ModelSettingSource::Session);
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::TurnModelSettingsResolved {
                    accepted_input_id: uuid(2),
                    turn_id: uuid(3),
                    defaults_version: CanonicalU64::new(1),
                    requested_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    selected_direct_id: uuid(4),
                    per_call_override: ModelSettingsOverlay::inherit_all(),
                    settings,
                    adjusted_from_selection_id: Some(uuid(4)),
                    adjustments: vec![ModelChangeAdjustment::ReasoningLevelClamped {
                        from: ReasoningLevel::XHigh,
                        to: ReasoningLevel::High,
                    }],
                },
            },
        )
        .expect_err("the selected model cannot also be the adjustment source");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: a distinct prior direct selection authenticates automatic
    /// model-change adjustment evidence for the frozen turn.
    #[test]
    fn inv033_turn_settings_event_accepts_distinct_adjustment_source() {
        let mut settings = settings_snapshot_fixture();
        settings.precedence.session = settings.precedence.per_call;
        settings.precedence.per_call = ModelSettingsOverlay::inherit_all();
        settings.reasoning_source = Some(ModelSettingSource::Session);
        settings.service_tier_source = Some(ModelSettingSource::Session);
        let result = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::TurnModelSettingsResolved {
                    accepted_input_id: uuid(2),
                    turn_id: uuid(3),
                    defaults_version: CanonicalU64::new(1),
                    requested_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    selected_direct_id: uuid(4),
                    per_call_override: ModelSettingsOverlay::inherit_all(),
                    settings,
                    adjusted_from_selection_id: Some(uuid(5)),
                    adjustments: vec![ModelChangeAdjustment::ReasoningLevelClamped {
                        from: ReasoningLevel::XHigh,
                        to: ReasoningLevel::High,
                    }],
                },
            },
        );

        assert!(result.is_ok());
    }

    /// INV-033: caller and adjustment evidence must derive the exact installed
    /// defaults snapshot.
    #[test]
    fn inv033_settings_change_event_rejects_unrelated_installed_snapshot() {
        let prior_settings = provider_default_settings_snapshot_fixture();
        let installed_settings = session_settings_snapshot_fixture();
        let model = ModelSelection::Direct {
            selection_id: uuid(4),
        };

        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::SessionModelSettingsChanged {
                    command_id: command(2).expect("fixture command identity is admitted"),
                    prior_defaults_version: CanonicalU64::new(1),
                    installed_defaults_version: CanonicalU64::new(2),
                    prior_model: model,
                    installed_model: model,
                    prior_settings,
                    installed_settings,
                    caller_override: ModelSettingsOverlay::inherit_all(),
                    adjustments: Vec::new(),
                },
            },
        )
        .expect_err("an all-inherit caller cannot install an unrelated session layer");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: an automatic model-change adjustment cannot rewrite a value
    /// explicitly supplied by the caller of that same defaults replacement.
    #[test]
    fn inv033_settings_change_rejects_adjustment_to_caller_explicit_value() {
        let prior_settings = provider_default_settings_snapshot_fixture();
        let mut installed_settings = provider_default_settings_snapshot_fixture();
        installed_settings.precedence.session.reasoning_level =
            SettingOverlay::Value(ReasoningLevel::Low);
        installed_settings.effective.reasoning_level = Some(ReasoningLevel::Low);
        installed_settings.reasoning_source = Some(ModelSettingSource::Session);
        installed_settings.validated_for_selection_id = Some(uuid(4));
        let caller_override = ModelSettingsOverlay {
            reasoning_level: SettingOverlay::Value(ReasoningLevel::High),
            fast_mode: FastModeOverlay::Inherit,
            service_tier: SettingOverlay::Inherit,
        };

        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::SessionModelSettingsChanged {
                    command_id: command(2).expect("fixture command identity is admitted"),
                    prior_defaults_version: CanonicalU64::new(1),
                    installed_defaults_version: CanonicalU64::new(2),
                    prior_model: ModelSelection::Direct {
                        selection_id: uuid(3),
                    },
                    installed_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    prior_settings,
                    installed_settings,
                    caller_override,
                    adjustments: vec![ModelChangeAdjustment::ReasoningLevelClamped {
                        from: ReasoningLevel::High,
                        to: ReasoningLevel::Low,
                    }],
                },
            },
        )
        .expect_err("caller-owned settings cannot be adjusted automatically");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-033: defaults reads bind a direct model to the snapshot validation
    /// identity.
    #[test]
    fn inv033_defaults_read_rejects_crosswired_direct_settings() {
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionDefaults {
                session_id: uuid(1),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: uuid(5),
                },
                model_settings: session_settings_snapshot_fixture(),
                dangerous_tool_auto_approval: false,
                system_prompt: None,
            },
        )
        .expect_err("direct defaults require settings validated for that selection");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    /// INV-012 / INV-033: settings-change events reject both reserved command
    /// identities during wire decoding.
    #[test]
    fn inv012_inv033_settings_change_event_rejects_command_sentinels() {
        let nil = format!(
            "{{\"version\":1,\"request_id\":\"1\",\"message\":{{\"type\":\"session_event\",\"cursor\":\"1\",\"session_id\":\"00000000-0000-0000-0000-000000000001\",\"event\":{{\"type\":\"session_model_settings_changed\",\"command_id\":\"00000000-0000-0000-0000-000000000000\",\"prior_defaults_version\":\"1\",\"installed_defaults_version\":\"2\",\"prior_model\":{{\"kind\":\"direct\",\"selection_id\":\"00000000-0000-0000-0000-000000000004\"}},\"installed_model\":{{\"kind\":\"alias\",\"alias_id\":\"00000000-0000-0000-0000-000000000005\"}},\"prior_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"installed_settings\":{PROVIDER_DEFAULT_SETTINGS_SNAPSHOT_JSON},\"caller_override\":{{\"reasoning_level\":{{\"kind\":\"inherit\"}},\"fast_mode\":{{\"kind\":\"inherit\"}},\"service_tier\":{{\"kind\":\"inherit\"}}}},\"adjustments\":[]}}}}}}"
        );
        let all_ones = nil.replace(
            "00000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        );

        assert_server_malformed(&nil);
        assert_server_malformed(&all_ones);
    }

    #[test]
    fn fast_mode_overlay_rejects_provider_default_on_the_wire() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"provider_default"},"service_tier":{"kind":"inherit"}},"system_prompt":null}}"#,
        );
    }

    /// INV-033: steering inherits its source turn and cannot carry an
    /// independent settings contribution.
    #[test]
    fn inv033_steering_rejects_a_model_settings_override() {
        let mut model_settings = ModelSettingsOverlay::inherit_all();
        model_settings.reasoning_level = SettingOverlay::Value(ReasoningLevel::High);
        let error = ClientFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ClientRequest::SubmitInput {
                command_id: command(1).expect("fixture command identity is admitted"),
                session_id: uuid(2),
                content: UserInputContent::text(String::from("steer")),
                expected_defaults_version: None,
                model_settings,
                delivery: Some(InputDelivery::Steer {
                    expected_active_turn_id: uuid(3),
                }),
            },
        )
        .expect_err("steering cannot override its source turn settings");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    #[test]
    fn capability_item_rejects_noncanonical_reasoning_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let message = ServerMessage::ModelCapabilityItem {
            selection_id: uuid(4),
            capabilities: ModelCapabilities {
                reasoning_levels: vec![ReasoningLevel::High, ReasoningLevel::Low],
                fast_mode_supported: false,
                service_tiers: Vec::new(),
            },
        };

        let error = ServerFrame::try_new_for_version(ProtocolVersion::One, request(43)?, message)
            .expect_err("capability sets use canonical ascending wire order");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
        Ok(())
    }

    #[test]
    fn nested_model_setting_tags_reject_unknown_members() {
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"model_settings":{"reasoning_level":{"kind":"inherit","extra":1},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"system_prompt":null}}"#,
        );
        assert_client_malformed(
            r#"{"version":1,"request_id":"1","request":{"type":"create_session","command_id":"00000000-0000-0000-0000-000000000001","initial_model_selection":{"kind":"direct","selection_id":"00000000-0000-0000-0000-000000000004"},"model_settings":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit","extra":1},"service_tier":{"kind":"inherit"}},"system_prompt":null}}"#,
        );
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"model_capability_item","selection_id":"00000000-0000-0000-0000-000000000004","capabilities":{"reasoning_levels":[],"fast_mode_supported":false,"service_tiers":[{"provider":"open_ai","value":"priority","extra":1}]}}}"#,
        );
    }

    #[test]
    fn settings_change_event_rejects_zero_prior_version() {
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::SessionModelSettingsChanged {
                    command_id: command(2).expect("fixture command identity is admitted"),
                    prior_defaults_version: CanonicalU64::new(0),
                    installed_defaults_version: CanonicalU64::new(1),
                    prior_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    installed_model: ModelSelection::Alias { alias_id: uuid(5) },
                    prior_settings: provider_default_settings_snapshot_fixture(),
                    installed_settings: provider_default_settings_snapshot_fixture(),
                    caller_override: ModelSettingsOverlay::inherit_all(),
                    adjustments: Vec::new(),
                },
            },
        )
        .expect_err("a settings change cannot precede the initial defaults epoch");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    #[test]
    fn settings_change_event_rejects_a_no_op() {
        let model = ModelSelection::Direct {
            selection_id: uuid(4),
        };
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::SessionModelSettingsChanged {
                    command_id: command(2).expect("fixture command identity is admitted"),
                    prior_defaults_version: CanonicalU64::new(1),
                    installed_defaults_version: CanonicalU64::new(2),
                    prior_model: model,
                    installed_model: model,
                    prior_settings: provider_default_settings_snapshot_fixture(),
                    installed_settings: provider_default_settings_snapshot_fixture(),
                    caller_override: ModelSettingsOverlay::inherit_all(),
                    adjustments: Vec::new(),
                },
            },
        )
        .expect_err("a durable settings-change event must record an actual change");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    #[test]
    fn turn_settings_event_rejects_a_mismatched_direct_selection() {
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::TurnModelSettingsResolved {
                    accepted_input_id: uuid(2),
                    turn_id: uuid(3),
                    defaults_version: CanonicalU64::new(1),
                    requested_model: ModelSelection::Direct {
                        selection_id: uuid(5),
                    },
                    selected_direct_id: uuid(4),
                    per_call_override: settings_snapshot_fixture().precedence.per_call,
                    settings: settings_snapshot_fixture(),
                    adjusted_from_selection_id: None,
                    adjustments: Vec::new(),
                },
            },
        )
        .expect_err("only an alias can resolve to a distinct direct selection");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    #[test]
    fn turn_settings_event_admits_model_independent_provider_defaults() {
        let result = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::TurnModelSettingsResolved {
                    accepted_input_id: uuid(2),
                    turn_id: uuid(3),
                    defaults_version: CanonicalU64::new(1),
                    requested_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    selected_direct_id: uuid(4),
                    per_call_override: ModelSettingsOverlay::inherit_all(),
                    settings: provider_default_settings_snapshot_fixture(),
                    adjusted_from_selection_id: None,
                    adjustments: Vec::new(),
                },
            },
        );

        assert!(result.is_ok());
    }

    #[test]
    fn turn_settings_event_requires_validation_for_non_default_settings() {
        let mut settings = settings_snapshot_fixture();
        settings.validated_for_selection_id = None;
        let error = ServerFrame::try_new(
            RequestId::try_new(1).expect("fixture request identity is admitted"),
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id: uuid(1),
                event: SessionEvent::TurnModelSettingsResolved {
                    accepted_input_id: uuid(2),
                    turn_id: uuid(3),
                    defaults_version: CanonicalU64::new(1),
                    requested_model: ModelSelection::Direct {
                        selection_id: uuid(4),
                    },
                    selected_direct_id: uuid(4),
                    per_call_override: settings.precedence.per_call,
                    settings,
                    adjusted_from_selection_id: None,
                    adjustments: Vec::new(),
                },
            },
        )
        .expect_err("non-default settings require exact validation provenance");

        assert_eq!(error, FrameValidationError::ModelSettingsShape);
    }

    #[test]
    fn adjustment_inventory_rejects_duplicates_order_and_excess() {
        let duplicate = vec![
            ModelChangeAdjustment::ReasoningLevelClamped {
                from: ReasoningLevel::XHigh,
                to: ReasoningLevel::High,
            },
            ModelChangeAdjustment::ReasoningLevelCleared {
                from: ReasoningLevel::High,
            },
        ];
        let reversed = vec![
            ModelChangeAdjustment::FastModeDisabled {},
            ModelChangeAdjustment::ReasoningLevelCleared {
                from: ReasoningLevel::High,
            },
        ];
        let excessive = vec![
            ModelChangeAdjustment::ReasoningLevelCleared {
                from: ReasoningLevel::High,
            },
            ModelChangeAdjustment::FastModeDisabled {},
            ModelChangeAdjustment::ServiceTierCleared {
                from: ServiceTier::OpenAi(OpenAiServiceTier::Priority),
            },
            ModelChangeAdjustment::ServiceTierCleared {
                from: ServiceTier::OpenAi(OpenAiServiceTier::Flex),
            },
        ];

        assert_eq!(
            validate_adjustments(&duplicate),
            Err(FrameValidationError::ModelSettingsShape)
        );
        assert_eq!(
            validate_adjustments(&reversed),
            Err(FrameValidationError::ModelSettingsShape)
        );
        assert_eq!(
            validate_adjustments(&excessive),
            Err(FrameValidationError::ModelSettingsShape)
        );
    }

    #[test]
    fn runner_state_transition_round_trips_complete_placement_facts()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(2),
                session_id: uuid(3),
                event: SessionEvent::RunnerStateTransition {
                    runner_id: uuid(4),
                    placement_revision: RunnerPlacementRevision::try_new(5)
                        .expect("the fixture placement revision is positive"),
                    sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
                    working_directory: Some(
                        RunnerWorkingDirectory::try_new(String::from("workspace/project"))
                            .expect("the fixture working directory is valid"),
                    ),
                    state: RunnerStateTransitionState::WorkingDirectoryChanged,
                },
            },
            r#"{"type":"session_event","cursor":"2","session_id":"00000000-0000-0000-0000-000000000003","event":{"type":"runner_state_transition","runner_id":"00000000-0000-0000-0000-000000000004","placement_revision":"5","sandbox_profile":"workspace-restricted","working_directory":"workspace/project","state":"working_directory_changed"}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_inv044_runner_placed_session_summary_round_trips_complete_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        let runner = RunnerProjection::try_new(
            RunnerProjectionSelector::CapabilityClass {
                name: RunnerCapabilityClass::try_new(String::from("linux.workspace"))?,
            },
            Some(uuid(4)),
            RunnerPlacementRevision::try_new(3)
                .expect("the fixture placement revision is positive"),
            RunnerSandboxProfile::WorkspaceRestricted,
            Some(RunnerCredentialProfileName::try_new(String::from(
                "readonly",
            ))?),
            Some(RunnerRepositoryKey::try_new(String::from("primary"))?),
            Some(RunnerWorkingDirectory::try_new(String::from(
                "workspace/project",
            ))?),
            None,
            RunnerProjectionState::RunnerLost,
        )?;

        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::SessionSummary {
                session_id: uuid(2),
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Alias { alias_id: uuid(3) },
                placement_version: CanonicalU64::new(1),
                placement: super::SessionPlacement::Pathless {},
                runner: Some(runner),
            },
            r#"{"type":"session_summary","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"1","model_selection":{"kind":"alias","alias_id":"00000000-0000-0000-0000-000000000003"},"placement_version":"1","placement":{"kind":"pathless"},"runner":{"selector":{"type":"capability_class","name":"linux.workspace"},"runner_id":"00000000-0000-0000-0000-000000000004","placement_revision":"3","sandbox_profile":"workspace-restricted","credential_profile":"readonly","repository":"primary","working_directory":"workspace/project","connection_health":null,"state":"runner_lost"}}"#,
        )?;
        Ok(())
    }

    #[test]
    fn inv033_session_summary_rejects_an_omitted_required_nullable_runner() {
        let encoded = br#"{"version":1,"request_id":"1","message":{"type":"session_summary","session_id":"00000000-0000-0000-0000-000000000002","defaults_version":"1","model_selection":{"kind":"alias","alias_id":"00000000-0000-0000-0000-000000000003"},"placement_version":"1","placement":{"kind":"pathless"}}}
"#;

        assert!(decode_server_line(encoded).is_err());
    }

    #[test]
    fn runner_state_transition_revision_rejects_zero_at_construction_and_decode() {
        assert_eq!(RunnerPlacementRevision::try_new(0), None);
        assert!(serde_json::from_str::<RunnerPlacementRevision>(r#""0""#).is_err());
    }

    #[test]
    fn runner_working_directory_rejects_every_invalid_wire_shape() {
        assert_eq!(
            RunnerWorkingDirectory::try_new(String::new()),
            Err(super::CanonicalValueError::RunnerWorkingDirectory)
        );
        assert_eq!(
            RunnerWorkingDirectory::try_new(String::from("bad\0path")),
            Err(super::CanonicalValueError::RunnerWorkingDirectory)
        );
        assert_eq!(
            RunnerWorkingDirectory::try_new("x".repeat(RunnerWorkingDirectory::MAX_UTF8_BYTES + 1)),
            Err(super::CanonicalValueError::RunnerWorkingDirectory)
        );
        assert!(serde_json::from_str::<RunnerWorkingDirectory>(r#"""#).is_err());
    }
}

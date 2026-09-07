//! Canonical scalars, frame errors, and bounded framing probes.

use std::{collections::HashSet, error::Error, fmt};

use base64::{Engine as _, engine::general_purpose::STANDARD as STANDARD_BASE64};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{MapAccess, Visitor},
};
use serde_json::value::RawValue;
use signalbox_domain::{BlobDigest, BlobDigestParseError};
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
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Maximum decoded source bytes carried by one conversation-import append.
///
/// The half-frame raw-byte bound leaves fixed headroom for canonical padded
/// base64, the request envelope, and the maximum-width correlation identity.
pub const MAX_CONVERSATION_IMPORT_CHUNK_BYTES: usize = MAX_FRAME_BYTES / 2;

/// Maximum decoded bytes carried by one immutable-blob append.
pub const MAX_BLOB_CHUNK_BYTES: usize = MAX_FRAME_BYTES / 2;

/// Maximum decoded bytes returned by one direct blob-range request.
pub const MAX_BLOB_READ_BYTES: usize = MAX_FRAME_BYTES / 2;

/// Maximum number of simultaneously open JSON objects and arrays in one frame.
pub const MAX_JSON_CONTAINER_DEPTH: usize = 127;

/// Maximum UTF-8 bytes in one transcript content fragment.
pub const MAX_CONTENT_FRAGMENT_BYTES: usize = 1024 * 1024;

/// Maximum total UTF-8 bytes in one complete metadata object or filter.
pub const MAX_SESSION_METADATA_TOTAL_UTF8_BYTES: usize = 262_144;

/// Maximum UTF-8 bytes in one indexed metadata tag or attribute key.
pub const MAX_SESSION_METADATA_INDEXED_UTF8_BYTES: usize = 1_024;

/// Maximum entries in one deployment model-alias catalog.
pub const MAX_MODEL_ALIAS_CATALOG_ENTRIES: usize = 10_000;

/// Maximum entries in one deployment model-capability catalog.
pub const MAX_MODEL_CAPABILITY_CATALOG_ENTRIES: usize = 10_000;

/// Maximum canonical decimal USD amount text.
pub const MAX_DOLLAR_AMOUNT_BYTES: usize = 30;

/// Maximum UTF-8 bytes in one deployment-owned billing rate version.
pub const MAX_RATE_VERSION_UTF8_BYTES: usize = 128;

/// Maximum finding-indexed members in one review-orchestration request.
pub const MAX_REVIEW_ORCHESTRATION_MEMBERS: usize = 1_024;

#[derive(signalbox_derive::Accessors)]
/// A lowercase hyphenated UUID at the process boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CanonicalUuid(
    /// Returns the underlying UUID for an explicit adapter mapping.
    #[get(copy, as = "into_uuid")]
    Uuid,
);

impl CanonicalUuid {
    /// Constructs the canonical wire value from a UUID.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
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

#[derive(signalbox_derive::Accessors)]
/// A full-range unsigned 64-bit value encoded as its shortest decimal string.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalU64(
    /// Returns the numeric value after canonical decoding.
    #[get(copy, as = "value")]
    u64,
);

impl CanonicalU64 {
    /// Wraps an unsigned value for precision-safe wire encoding.
    pub const fn new(value: u64) -> Self {
        Self(value)
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

#[derive(signalbox_derive::Accessors)]
/// A positive unsigned 64-bit value encoded as its shortest decimal string.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PositiveCanonicalU64(
    /// Returns the positive numeric value.
    #[get(copy, as = "value")]
    u64,
);

impl PositiveCanonicalU64 {
    /// Checks that the represented wire integer is positive.
    pub const fn try_new(value: u64) -> Result<Self, CanonicalValueError> {
        if value == 0 {
            return Err(CanonicalValueError::Decimal);
        }
        Ok(Self(value))
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

#[derive(signalbox_derive::Accessors)]
/// A lowercase 32-byte digest encoded as exactly 64 hexadecimal characters.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalDigest(
    /// Borrows the exact canonical hexadecimal spelling.
    #[get(str, as = "as_str")]
    String,
);

impl CanonicalDigest {
    /// Checks the exact lowercase hexadecimal digest spelling.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        let mut decoded = [0_u8; 32];
        if hex::decode_to_slice(&value, &mut decoded).is_err() || hex::encode(decoded) != value {
            return Err(CanonicalValueError::Digest);
        }
        Ok(Self(value))
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

#[derive(signalbox_derive::Accessors)]
/// Exact external blob identity including its fixed SHA-256 algorithm tag.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalBlobDigest(
    /// Returns the validated digest for an explicit adapter mapping.
    #[get(copy, as = "into_digest")]
    BlobDigest,
);

impl CanonicalBlobDigest {
    /// Constructs the exact SHA-256 identity from an already-computed digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(BlobDigest::from_bytes(bytes))
    }

    /// Wraps one validated domain digest for the process boundary.
    pub const fn from_digest(value: BlobDigest) -> Self {
        Self(value)
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

#[derive(signalbox_derive::Accessors)]
/// Request correlation identity. Zero is reserved for uncorrelated errors.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(
    /// Returns the numeric identity after canonical decoding.
    #[get(copy, as = "value")]
    u64,
);

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

    pub(crate) const fn is_correlated(self) -> bool {
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

#[derive(signalbox_derive::Accessors)]
/// Exact user input content carried to the application admission boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputContent(
    /// Borrows exact decoded text.
    #[get(str, as = "as_str")]
    String,
);

impl InputContent {
    /// Wraps decoded content without applying application admission policy.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Transfers ownership of the exact decoded text.
    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(signalbox_derive::Accessors)]
/// One bounded transcript-content fragment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentFragment(
    /// Borrows exact fragment text.
    #[get(str, as = "as_str")]
    String,
);

impl ContentFragment {
    /// Applies the per-fragment UTF-8 byte bound.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        if value.len() > MAX_CONTENT_FRAGMENT_BYTES {
            Err(CanonicalValueError::Content)
        } else {
            Ok(Self(value))
        }
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

#[derive(signalbox_derive::Accessors)]
/// Canonical nonnegative decimal USD text with no exponent or redundant zeroes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalDollarAmount(
    /// Borrows the canonical decimal spelling.
    #[get(str, as = "as_str")]
    String,
);

impl CanonicalDollarAmount {
    /// Validates one shortest nonnegative base-ten decimal spelling.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
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

#[derive(signalbox_derive::Accessors)]
/// One bounded deployment-owned rate version carried as cost provenance.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BillingRateVersion(
    /// Borrows the exact rate version.
    #[get(str, as = "as_str")]
    String,
);

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

#[derive(signalbox_derive::Accessors)]
/// One exact session system prompt on the wire.
///
/// A present prompt is nonempty and rejects U+0000; absence is JSON null on
/// the owning member, never empty text. The daemon applies deployment policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SystemPromptText(
    /// Borrows the exact admitted prompt text.
    #[get(str, as = "as_str")]
    String,
);

impl SystemPromptText {
    /// Applies the structural nonempty and U+0000-free admission rules.
    pub fn try_new(value: String) -> Result<Self, CanonicalValueError> {
        if value.is_empty() || value.contains('\0') {
            Err(CanonicalValueError::SystemPrompt)
        } else {
            Ok(Self(value))
        }
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

    pub(crate) const fn is_absent(&self) -> bool {
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

#[derive(signalbox_derive::OperatorError)]
/// Invalid canonical scalar at the wire boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalValueError {
    #[error("UUID is not canonical lowercase hyphenated text")]
    /// UUID was not lowercase canonical hyphenated text.
    Uuid,
    #[error("command identity is a reserved sentinel")]
    /// Command UUID used a reserved sentinel.
    CommandId,
    #[error("unsigned integer is not canonical decimal text")]
    /// Decimal text was not the shortest full-range unsigned spelling.
    Decimal,
    #[error("client request identity must be nonzero")]
    /// Client request identity was zero.
    RequestId,
    #[error("content fragment exceeds the process-protocol UTF-8 byte bound")]
    /// A transcript fragment exceeded its UTF-8 byte bound.
    Content,
    #[error("session metadata value is invalid")]
    /// Session metadata violated its exact string, set, map, or page bound.
    Metadata,
    #[error("digest is not canonical lowercase 64-character hexadecimal text")]
    /// Digest was not exactly 64 lowercase hexadecimal characters.
    Digest,
    #[error("session system prompt is empty, oversized, or contains U+0000")]
    /// A session system prompt was empty, contained U+0000, or exceeded its
    /// UTF-8 byte bound.
    SystemPrompt,
    #[error("session placement is invalid")]
    /// A dotted session placement or root-global-read decision was invalid.
    Placement,
    #[error("runner working directory is invalid")]
    /// Runner working-directory text was empty, NUL-bearing, or oversized.
    RunnerWorkingDirectory,
    #[error("runner catalog name is invalid")]
    /// Runner capability, credential-profile, or repository name was invalid.
    RunnerCatalogName,
    #[error("runner projection state is invalid")]
    /// Runner projection state and exact-runner evidence were inconsistent.
    RunnerProjection,
    #[error("dollar amount is not canonical nonnegative decimal text")]
    /// Dollar amount was not canonical bounded nonnegative decimal text.
    DollarAmount,
    #[error("billing rate version is invalid")]
    /// Billing rate version was empty, padded, NUL-bearing, or oversized.
    RateVersion,
}

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

#[derive(signalbox_derive::Accessors)]
/// Exact caller-supplied source bytes carried as canonical padded base64.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationImportSource(
    /// Borrows the exact decoded source snapshot.
    #[get(slice, as = "as_bytes")]
    Vec<u8>,
);

impl ConversationImportSource {
    /// Wraps one complete source snapshot without interpreting it.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
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

pub(crate) fn parse_decimal_u64(value: &str) -> Result<u64, CanonicalValueError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| CanonicalValueError::Decimal)?;
    if parsed.to_string() != value {
        return Err(CanonicalValueError::Decimal);
    }
    Ok(parsed)
}

pub(crate) fn deserialize_required_nullable<'de, DeserializerT, ValueT>(
    deserializer: DeserializerT,
) -> Result<Option<ValueT>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
    ValueT: Deserialize<'de>,
{
    Option::<ValueT>::deserialize(deserializer)
}

pub(crate) fn deserialize_optional_non_null<'de, DeserializerT, ValueT>(
    deserializer: DeserializerT,
) -> Result<Option<ValueT>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
    ValueT: Deserialize<'de>,
{
    ValueT::deserialize(deserializer).map(Some)
}

#[derive(signalbox_derive::OperatorError)]
/// A structurally invalid frame value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameValidationError {
    #[error("OAuth credential frame shape is inconsistent")]
    /// An OAuth profile or device-authorization detail violates its bounds.
    OauthCredentialShape,
    #[error("frame version is unsupported")]
    /// In-memory frame used another version.
    UnsupportedVersion,
    #[error("client request identity is uncorrelated")]
    /// A client request used reserved correlation identity zero.
    UncorrelatedClientRequest,
    #[error("successful server message is uncorrelated")]
    /// A success response used reserved correlation identity zero.
    UncorrelatedSuccess,
    #[error("application server error is uncorrelated")]
    /// A non-framing error used reserved correlation identity zero.
    UncorrelatedApplicationError,
    #[error("server error detail does not match its code")]
    /// Rejection detail did not match the error code.
    ErrorDetailShape,
    #[error("transcript turn state is inconsistent")]
    /// A transcript turn carried an impossible correlated state shape.
    TurnStateShape,
    #[error("tool approval event shape is inconsistent")]
    /// A tool approval event carried inconsistent decision provenance.
    ToolApprovalShape,
    #[error("session metadata frame shape is inconsistent")]
    /// A metadata request or response carried an invalid correlated shape.
    MetadataShape,
    #[error("unified conversation-listing frame shape is inconsistent")]
    /// A unified conversation-listing frame carried an invalid shape.
    ConversationListShape,
    #[error("operator-status frame shape is inconsistent")]
    /// An operator-status row carried an invalid shape.
    OperatorStatusShape,
    #[error("frame omits its required system-prompt member")]
    SystemPromptShape,
    #[error("conversation-import frame shape is inconsistent")]
    /// A chunked conversation-import frame carried a contradictory shape.
    ConversationImportShape,
    #[error("blob-upload frame shape is inconsistent")]
    /// A chunked immutable-blob frame carried a contradictory shape.
    BlobUploadShape,
    #[error("blob-read frame shape is inconsistent")]
    /// A blob metadata, range, or range-rejection value contradicted its bounds.
    BlobReadShape,
    #[error("imported frontier position is not positive")]
    /// An imported-frontier request carried a nonpositive position.
    ImportedFrontierShape,
    #[error("compaction through position is not positive")]
    /// A context-compaction request carried a nonpositive position.
    ContextCompactionShape,
    #[error("imported conversation entry position is not positive")]
    /// An imported-conversation entry carried a nonpositive position.
    ImportedConversationEntryShape,
    #[error("imported text preview shape is inconsistent")]
    /// An imported text preview exceeded its bound or contradicted its own
    /// truncation marker.
    ImportedTextPreviewShape,
    #[error("imported frontier rejection range is inconsistent")]
    /// An out-of-range imported rejection stated a range its own requested
    /// position falls inside, or an empty selectable range.
    ImportedFrontierRangeShape,
    #[error("submit-input delivery shape is inconsistent")]
    /// A submit-input delivery carried forbidden or missing correlated fields.
    InputDeliveryShape,
    #[error("ordered user content shape is inconsistent")]
    /// Ordered user parts violated their canonical shape or resource bounds.
    UserContentShape,
    #[error("session-template frame shape is inconsistent")]
    /// A template name or positive version carried an invalid shape.
    TemplateShape,
    #[error("review workflow frame shape is inconsistent")]
    /// A review lifecycle or orchestration frame carried an invalid shape.
    ReviewShape,
    #[error("model-call usage frame shape is inconsistent")]
    /// A model-call usage row carried cost without any reported usage axis.
    ModelCallUsageShape,
    #[error("commissioned-goal frame shape is inconsistent")]
    /// A goal request, state, or event carried an invalid shape.
    GoalShape,
    #[error("session-delegation frame shape is inconsistent")]
    /// A delegation update carried an invalid correlated shape.
    DelegationShape,
    #[error("model-settings frame shape is inconsistent")]
    /// Model settings or capability data carried a contradictory shape.
    ModelSettingsShape,
    #[error("session-placement frame shape is inconsistent")]
    /// A dotted placement or its root-global-read acknowledgement is invalid.
    PlacementShape,
    #[error("commissioned-session fence shape is inconsistent")]
    /// A commissioned-session authority fence carried an invalid shape.
    DispatchFenceShape,
}

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

    pub(crate) const fn malformed(request_id: RequestId) -> Self {
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

#[derive(signalbox_derive::OperatorError)]
/// Outgoing frame could not be encoded within the protocol boundary.
#[derive(Debug)]
pub enum FrameEncodeError {
    #[error("invalid process-protocol frame: {field_0}")]
    /// In-memory value violated its closed frame shape.
    Validation(#[source] FrameValidationError),
    #[error("process-protocol frame serialization failed")]
    /// JSON serialization failed.
    Json(#[source] serde_json::Error),
    #[error("process-protocol frame is oversized")]
    /// Encoded frame exceeded the inclusive byte cap.
    OversizedFrame,
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

pub(crate) fn checked_line_content(
    line: &[u8],
    allow_uncorrelated: bool,
) -> Result<&[u8], FrameDecodeError> {
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

pub(crate) struct ProbedHeader {
    pub(crate) request_id: RequestId,
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

pub(crate) fn probe_header(
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

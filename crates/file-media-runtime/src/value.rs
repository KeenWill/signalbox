use std::{error::Error, fmt, num::NonZeroU64, str::FromStr, sync::Arc};

use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;

const SHA256_PREFIX: &str = "sha256:";
// numeric-bound: ceiling - bounds retained caller media-type text
const MAX_DECLARED_MEDIA_TYPE_BYTES: usize = 255;
// numeric-bound: ceiling - bounds retained caller display-name text
const MAX_DISPLAY_FILENAME_BYTES: usize = 255;
// numeric-bound: ceiling - bounds registry identity and selector storage
const MAX_NAME_BYTES: usize = 64;
// numeric-bound: ceiling - bounds retained reader-revision text
const MAX_REVISION_BYTES: usize = 32;
// numeric-bound: ceiling - bounds retained and parsed view-schema memory
const MAX_SCHEMA_BYTES: usize = 65_536;
// numeric-bound: ceiling - bounds retained and parsed processor-metadata memory
const MAX_METADATA_BYTES: usize = 16_384;
// numeric-bound: ceiling - bounds retained untrusted continuation state
const MAX_CONTINUATION_CURSOR_BYTES: usize = 1_024;

/// SHA-256 identity at the provider-neutral boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileDigest([u8; 32]);

impl FileDigest {
    /// Reconstitutes a digest already verified by the blob layer.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the fixed digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for FileDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{SHA256_PREFIX}{}", hex::encode(self.0))
    }
}

impl FromStr for FileDigest {
    type Err = RegistryValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let encoded = value
            .strip_prefix(SHA256_PREFIX)
            .ok_or(RegistryValueError::Digest)?;
        if encoded.len() != 64
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(RegistryValueError::Digest);
        }
        let mut bytes = [0_u8; 32];
        hex::decode_to_slice(encoded, &mut bytes).map_err(|_| RegistryValueError::Digest)?;
        Ok(Self(bytes))
    }
}

/// Caller intent for one use of immutable bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AttachmentKind {
    /// Image intent.
    Image,
    /// Document intent.
    Document,
    /// General file intent.
    File,
}

/// Exact bounded caller-declared media type.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct DeclaredMediaType(Arc<str>);

impl DeclaredMediaType {
    /// Admits a nonempty visible-ASCII value without normalization.
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, RegistryValueError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_DECLARED_MEDIA_TYPE_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Err(RegistryValueError::DeclaredMediaType);
        }
        Ok(Self(value))
    }

    /// Borrows the exact caller spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parses only a parameter-free canonical essence.
    pub fn canonical_essence(&self) -> Result<CanonicalMediaType, MediaTypeParseError> {
        CanonicalMediaType::from_str(self.as_str())
    }
}

impl fmt::Debug for DeclaredMediaType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeclaredMediaType([REDACTED])")
    }
}

/// Bounded attachment basename supplied by a caller.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct DisplayFilename(Arc<str>);

impl DisplayFilename {
    /// Admits one nonempty basename without path or null characters.
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, RegistryValueError> {
        let value = value.into();
        let invalid = value.is_empty()
            || value.len() > MAX_DISPLAY_FILENAME_BYTES
            || value.as_ref() == "."
            || value.as_ref() == ".."
            || value.contains('/')
            || value.contains('\\')
            || value.contains('\0');
        if invalid {
            Err(RegistryValueError::DisplayFilename)
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows the exact caller spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DisplayFilename {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DisplayFilename([REDACTED])")
    }
}

/// One semantic use of immutable file bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct FileUse {
    digest: FileDigest,
    byte_length: NonZeroU64,
    attachment_kind: AttachmentKind,
    declared_media_type: DeclaredMediaType,
    display_filename: Option<DisplayFilename>,
}

impl FileUse {
    /// Constructs one checked file use from already-admitted caller metadata.
    pub const fn new(
        digest: FileDigest,
        byte_length: NonZeroU64,
        attachment_kind: AttachmentKind,
        declared_media_type: DeclaredMediaType,
        display_filename: Option<DisplayFilename>,
    ) -> Self {
        Self {
            digest,
            byte_length,
            attachment_kind,
            declared_media_type,
            display_filename,
        }
    }

    /// Returns the immutable byte identity.
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Returns the catalogued positive length.
    pub const fn byte_length(&self) -> NonZeroU64 {
        self.byte_length
    }

    /// Returns caller attachment intent.
    pub const fn attachment_kind(&self) -> AttachmentKind {
        self.attachment_kind
    }

    /// Borrows the exact declared type.
    pub const fn declared_media_type(&self) -> &DeclaredMediaType {
        &self.declared_media_type
    }

    /// Borrows the optional display basename.
    pub const fn display_filename(&self) -> Option<&DisplayFilename> {
        self.display_filename.as_ref()
    }
}

impl fmt::Debug for FileUse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileUse")
            .field("digest", &self.digest)
            .field("byte_length", &self.byte_length)
            .field("attachment_kind", &self.attachment_kind)
            .field("declared_media_type", &"[REDACTED]")
            .field(
                "display_filename",
                &self.display_filename.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Canonical lowercase ASCII media-type essence with no parameters.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalMediaType(Arc<str>);

impl CanonicalMediaType {
    /// Borrows the canonical `type/subtype` spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalMediaType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CanonicalMediaType {
    type Err = MediaTypeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > MAX_DECLARED_MEDIA_TYPE_BYTES {
            return Err(MediaTypeParseError);
        }
        let parsed = value
            .parse::<mime::Mime>()
            .map_err(|_| MediaTypeParseError)?;
        if parsed.params().next().is_some() || parsed.essence_str() != value {
            return Err(MediaTypeParseError);
        }
        Ok(Self(Arc::from(value)))
    }
}

/// A media type was not a canonical parameter-free essence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaTypeParseError;

impl fmt::Display for MediaTypeParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("media type is not a canonical lowercase ASCII essence")
    }
}

impl Error for MediaTypeParseError {}

macro_rules! checked_name {
    ($name:ident, $label:literal) => {
        #[doc = $label]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Admits one canonical lowercase ASCII registry token.
            pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, RegistryValueError> {
                let value = value.into();
                if valid_registry_name(&value) {
                    Ok(Self(value))
                } else {
                    Err(RegistryValueError::Name)
                }
            }

            /// Borrows the canonical spelling.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

checked_name!(FileReaderProviderName, "One compiled provider identity.");
checked_name!(FileReaderName, "One reader identity within a provider.");
checked_name!(ReadViewName, "One provider-owned read-view name.");
checked_name!(
    ReasonCode,
    "One registered sanitized processor reason code."
);

fn valid_registry_name(value: &str) -> bool {
    value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
}

/// Immutable reader implementation revision.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileReaderRevision(Arc<str>);

impl FileReaderRevision {
    /// Admits one bounded visible-ASCII revision label.
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, RegistryValueError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_REVISION_BYTES
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(RegistryValueError::Revision);
        }
        Ok(Self(value))
    }

    /// Borrows the exact revision spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable provider, reader, and revision tuple.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReaderIdentity {
    provider: FileReaderProviderName,
    reader: FileReaderName,
    revision: FileReaderRevision,
}

impl ReaderIdentity {
    /// Constructs one checked tuple.
    pub const fn new(
        provider: FileReaderProviderName,
        reader: FileReaderName,
        revision: FileReaderRevision,
    ) -> Self {
        Self {
            provider,
            reader,
            revision,
        }
    }

    /// Borrows the provider identity.
    pub const fn provider(&self) -> &FileReaderProviderName {
        &self.provider
    }

    /// Borrows the reader identity.
    pub const fn reader(&self) -> &FileReaderName {
        &self.reader
    }

    /// Borrows the immutable revision.
    pub const fn revision(&self) -> &FileReaderRevision {
        &self.revision
    }
}

/// Canonical compact object-rooted JSON Schema declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalJsonObjectSchema {
    compact: Arc<str>,
    value: serde_json::Value,
}

impl CanonicalJsonObjectSchema {
    /// Parses, bounds, and canonicalizes one object-rooted schema.
    pub fn try_new(value: &str) -> Result<Self, RegistryValueError> {
        if value.len() > MAX_SCHEMA_BYTES || value.contains('\0') {
            return Err(RegistryValueError::Schema);
        }
        let parsed =
            parse_json_without_duplicate_members(value).map_err(|_| RegistryValueError::Schema)?;
        let object = parsed.as_object().ok_or(RegistryValueError::Schema)?;
        if object.get("type").and_then(serde_json::Value::as_str) != Some("object") {
            return Err(RegistryValueError::Schema);
        }
        let compact = serde_json::to_string(&parsed).map_err(|_| RegistryValueError::Schema)?;
        if compact.len() > MAX_SCHEMA_BYTES {
            return Err(RegistryValueError::Schema);
        }
        Ok(Self {
            compact: Arc::from(compact),
            value: parsed,
        })
    }

    /// Borrows the compact canonical JSON spelling.
    pub fn as_str(&self) -> &str {
        &self.compact
    }

    /// Borrows the parsed schema object.
    pub const fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

/// Bounded canonical processor metadata object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedMetadata {
    compact: Arc<str>,
    value: serde_json::Value,
}

impl BoundedMetadata {
    /// Parses a processor-supplied JSON object and rejects excess or malformed data.
    pub fn try_new(value: &str) -> Result<Self, RegistryValueError> {
        if value.len() > MAX_METADATA_BYTES || value.contains('\0') {
            return Err(RegistryValueError::Metadata);
        }
        let parsed = parse_json_without_duplicate_members(value)
            .map_err(|_| RegistryValueError::Metadata)?;
        if !parsed.is_object() {
            return Err(RegistryValueError::Metadata);
        }
        let compact = serde_json::to_string(&parsed).map_err(|_| RegistryValueError::Metadata)?;
        if compact.len() > MAX_METADATA_BYTES {
            return Err(RegistryValueError::Metadata);
        }
        Ok(Self {
            compact: Arc::from(compact),
            value: parsed,
        })
    }

    /// Borrows the compact canonical JSON object.
    pub fn as_str(&self) -> &str {
        &self.compact
    }

    /// Borrows the parsed object.
    pub const fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

/// Parses structured JSON while rejecting duplicate object members and compiled-limit excess.
pub fn parse_json_without_duplicate_members(
    value: &str,
) -> Result<serde_json::Value, serde_json::Error> {
    parse_json_without_duplicate_members_bounded(
        value,
        JsonParseLimits {
            maximum_nodes: crate::MAX_STRUCTURED_NODES,
            maximum_container_entries: crate::MAX_OBSERVED_CONTAINER_ENTRIES,
        },
    )
}

/// Caller-labeled ceilings for structured JSON parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonParseLimits {
    /// Maximum total JSON values admitted.
    pub maximum_nodes: u64,
    /// Maximum members or elements admitted in any one container.
    pub maximum_container_entries: u64,
}

/// Parses structured JSON with caller-labeled node and container-entry ceilings.
pub fn parse_json_without_duplicate_members_bounded(
    value: &str,
    limits: JsonParseLimits,
) -> Result<serde_json::Value, serde_json::Error> {
    let raw = serde_json::from_str::<Box<RawValue>>(value)?;
    let mut budget = JsonParseBudget {
        remaining_nodes: limits.maximum_nodes,
        maximum_container_entries: limits.maximum_container_entries,
    };
    parse_raw_json(raw.get(), 0, &mut budget)
}

struct JsonParseBudget {
    remaining_nodes: u64,
    maximum_container_entries: u64,
}

impl JsonParseBudget {
    fn admit_node(&mut self) -> Result<(), serde_json::Error> {
        self.remaining_nodes = self.remaining_nodes.checked_sub(1).ok_or_else(|| {
            serde_json::Error::custom("JSON node count exceeds the effective ceiling")
        })?;
        Ok(())
    }

    fn admits_container_entries(&self, entries: u64) -> bool {
        entries <= self.maximum_container_entries
    }
}

fn parse_raw_json(
    value: &str,
    depth: u32,
    budget: &mut JsonParseBudget,
) -> Result<serde_json::Value, serde_json::Error> {
    budget.admit_node()?;
    match value.trim_start().as_bytes().first() {
        Some(b'{') if depth < crate::MAX_STRUCTURED_DEPTH => {
            deserialize_seed(value, DuplicateAwareObject { depth, budget })
        }
        Some(b'[') if depth < crate::MAX_STRUCTURED_DEPTH => {
            deserialize_seed(value, DuplicateAwareArray { depth, budget })
        }
        Some(b'{' | b'[') => Err(serde_json::Error::custom(
            "JSON nesting depth exceeds the compiled ceiling",
        )),
        _ => serde_json::from_str(value),
    }
}

fn deserialize_seed<Seed>(value: &str, seed: Seed) -> Result<serde_json::Value, serde_json::Error>
where
    for<'de> Seed: DeserializeSeed<'de, Value = serde_json::Value>,
{
    let mut deserializer = serde_json::Deserializer::from_str(value);
    let parsed = seed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(parsed)
}

struct DuplicateAwareObject<'a> {
    depth: u32,
    budget: &'a mut JsonParseBudget,
}

impl<'de> DeserializeSeed<'de> for DuplicateAwareObject<'_> {
    type Value = serde_json::Value;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(DuplicateAwareObjectVisitor {
            depth: self.depth,
            budget: self.budget,
        })
    }
}

struct DuplicateAwareArray<'a> {
    depth: u32,
    budget: &'a mut JsonParseBudget,
}

impl<'de> DeserializeSeed<'de> for DuplicateAwareArray<'_> {
    type Value = serde_json::Value;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(DuplicateAwareArrayVisitor {
            depth: self.depth,
            budget: self.budget,
        })
    }
}

struct DuplicateAwareObjectVisitor<'a> {
    depth: u32,
    budget: &'a mut JsonParseBudget,
}

impl<'de> Visitor<'de> for DuplicateAwareObjectVisitor<'_> {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object members")
    }

    fn visit_map<Access>(self, mut object: Access) -> Result<Self::Value, Access::Error>
    where
        Access: MapAccess<'de>,
    {
        let mut values = std::collections::BTreeMap::new();
        let mut entries = 0_u64;
        while let Some(name) = object.next_key::<String>()? {
            entries = entries
                .checked_add(1)
                .ok_or_else(|| Access::Error::custom("JSON container entry count overflowed"))?;
            if !self.budget.admits_container_entries(entries) {
                return Err(Access::Error::custom(
                    "JSON container entries exceed the effective ceiling",
                ));
            }
            if values.contains_key(&name) {
                return Err(Access::Error::custom("duplicate JSON object member"));
            }
            let raw = object.next_value::<Box<RawValue>>()?;
            let value = parse_raw_json(raw.get(), self.depth + 1, self.budget)
                .map_err(Access::Error::custom)?;
            values.insert(name, value);
        }
        Ok(serde_json::Value::Object(values.into_iter().collect()))
    }
}

struct DuplicateAwareArrayVisitor<'a> {
    depth: u32,
    budget: &'a mut JsonParseBudget,
}

impl<'de> Visitor<'de> for DuplicateAwareArrayVisitor<'_> {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON array without duplicate object members")
    }

    fn visit_seq<Access>(self, mut sequence: Access) -> Result<Self::Value, Access::Error>
    where
        Access: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        let mut entries = 0_u64;
        while let Some(raw) = sequence.next_element::<Box<RawValue>>()? {
            entries = entries
                .checked_add(1)
                .ok_or_else(|| Access::Error::custom("JSON container entry count overflowed"))?;
            if !self.budget.admits_container_entries(entries) {
                return Err(Access::Error::custom(
                    "JSON container entries exceed the effective ceiling",
                ));
            }
            values.push(
                parse_raw_json(raw.get(), self.depth + 1, self.budget)
                    .map_err(Access::Error::custom)?,
            );
        }
        Ok(serde_json::Value::Array(values))
    }
}

/// Stable visible-part selector for repeated digest uses.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VisiblePartSelector(Arc<str>);

impl VisiblePartSelector {
    /// Admits one bounded opaque ASCII selector.
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, RegistryValueError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_NAME_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(RegistryValueError::Selector);
        }
        Ok(Self(value))
    }

    /// Borrows the opaque selector.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Checked opaque continuation cursor returned by one bounded read.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReadContinuationCursor(Arc<str>);

impl ReadContinuationCursor {
    /// Admits one bounded control-free restart-ephemeral cursor.
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, RegistryValueError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_CONTINUATION_CURSOR_BYTES
            || value.contains('\0')
            || value.chars().any(char::is_control)
        {
            return Err(RegistryValueError::Continuation);
        }
        Ok(Self(value))
    }

    /// Borrows the opaque cursor spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owned opaque cursor spelling.
    pub fn into_string(self) -> String {
        self.0.to_string()
    }
}

/// Closed construction failure for provider-neutral checked values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryValueError {
    /// Invalid digest spelling.
    Digest,
    /// Invalid declared media type.
    DeclaredMediaType,
    /// Invalid display basename.
    DisplayFilename,
    /// Invalid registry name.
    Name,
    /// Invalid reader revision.
    Revision,
    /// Invalid object-rooted JSON Schema.
    Schema,
    /// Invalid processor metadata object.
    Metadata,
    /// Invalid visible-part selector.
    Selector,
    /// Invalid read-continuation cursor.
    Continuation,
}

impl fmt::Display for RegistryValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Digest => "file digest is invalid",
            Self::DeclaredMediaType => "declared media type is invalid",
            Self::DisplayFilename => "display filename is invalid",
            Self::Name => "registry name is invalid",
            Self::Revision => "reader revision is invalid",
            Self::Schema => "view schema is invalid",
            Self::Metadata => "processor metadata is invalid",
            Self::Selector => "visible-part selector is invalid",
            Self::Continuation => "read continuation cursor is invalid",
        })
    }
}

impl Error for RegistryValueError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_metadata(depth: u32) -> String {
        let depth = usize::try_from(depth).expect("compiled depth ceiling fits usize");
        format!("{{\"value\":{}0{}}}", "[".repeat(depth), "]".repeat(depth))
    }

    fn flat_array(entries: usize) -> String {
        format!("[{}]", vec!["0"; entries].join(","))
    }

    #[test]
    fn bounded_parser_rejects_nodes_during_deserialization() {
        let input = flat_array(2);

        let outcome = parse_json_without_duplicate_members_bounded(
            &input,
            JsonParseLimits {
                maximum_nodes: 2,
                maximum_container_entries: 2,
            },
        );

        assert!(outcome.is_err());
    }

    #[test]
    fn bounded_parser_rejects_container_entries_during_deserialization() {
        let input = flat_array(2);

        let outcome = parse_json_without_duplicate_members_bounded(
            &input,
            JsonParseLimits {
                maximum_nodes: 3,
                maximum_container_entries: 1,
            },
        );

        assert!(outcome.is_err());
    }

    #[test]
    fn canonical_media_types_use_standard_syntax_and_reject_noncanonical_forms() {
        assert!(CanonicalMediaType::from_str("text/plain").is_ok());
        assert!(CanonicalMediaType::from_str("!text/!plain").is_ok());
        assert!(CanonicalMediaType::from_str("text/plain; charset=utf-8").is_err());
        assert!(CanonicalMediaType::from_str("Text/plain").is_err());
        assert!(CanonicalMediaType::from_str("text").is_err());
        assert!(CanonicalMediaType::from_str("text/plain other").is_err());
        assert!(CanonicalMediaType::from_str(&format!("a/{}", "b".repeat(254))).is_err());
    }

    #[test]
    fn display_filenames_preserve_blob_valid_control_characters() {
        let newline_filename = "line\nbreak.txt";
        let delete_filename = "delete\u{7f}.txt";
        let newline = DisplayFilename::try_new(newline_filename)
            .expect("blob-valid newline filename remains representable");
        let delete = DisplayFilename::try_new(delete_filename)
            .expect("blob-valid delete-character filename remains representable");

        assert_eq!(newline.as_str(), newline_filename);
        assert_eq!(delete.as_str(), delete_filename);
        assert_eq!(
            DisplayFilename::try_new("null\0.txt"),
            Err(RegistryValueError::DisplayFilename)
        );
    }

    #[test]
    fn metadata_rejects_nesting_above_the_compiled_ceiling() {
        let input = nested_metadata(crate::MAX_STRUCTURED_DEPTH);

        let outcome = BoundedMetadata::try_new(&input);

        assert_eq!(outcome, Err(RegistryValueError::Metadata));
    }

    #[test]
    fn metadata_is_parsed_as_data_and_canonically_escaped() {
        let compact_input = r#"{"note":"</tool><script>alert(1)</script>"}"#;
        let metadata = BoundedMetadata::try_new(compact_input)
            .expect("synthetic injection-shaped JSON remains inert data");

        assert_eq!(metadata.as_str(), compact_input);
        assert_eq!(
            metadata.value()["note"],
            serde_json::Value::String(String::from("</tool><script>alert(1)</script>"))
        );
    }

    #[test]
    fn metadata_rejects_duplicate_object_members() {
        let outcome = BoundedMetadata::try_new(r#"{"kind":"safe","kind":"attacker"}"#);

        assert_eq!(outcome, Err(RegistryValueError::Metadata));
    }

    #[test]
    fn metadata_preserves_arbitrary_precision_numbers() {
        let input = r#"{"n":123456789012345678901234567890}"#;
        let metadata = BoundedMetadata::try_new(input)
            .expect("arbitrary-precision fixture remains valid metadata");

        assert_eq!(metadata.as_str(), input);
    }

    #[test]
    fn metadata_preserves_reserved_number_key_objects() {
        let input = r#"{"$serde_json::private::Number":"1"}"#;
        let metadata = BoundedMetadata::try_new(input)
            .expect("the reserved spelling remains an ordinary object member");

        assert_eq!(metadata.as_str(), input);
        assert_eq!(
            metadata.value()["$serde_json::private::Number"],
            serde_json::Value::String(String::from("1"))
        );
    }

    #[test]
    fn metadata_preserves_nested_reserved_number_key_objects() {
        let input = r#"{"nested":{"$serde_json::private::Number":"1","tail":true}}"#;
        let metadata = BoundedMetadata::try_new(input)
            .expect("nested reserved spelling remains ordinary object data");

        assert_eq!(metadata.as_str(), input);
        assert_eq!(
            metadata.value()["nested"]["$serde_json::private::Number"],
            serde_json::Value::String(String::from("1"))
        );
    }

    #[test]
    fn metadata_canonicalizes_object_members_lexically() {
        let input = r#"{"z":0,"a":{"z":0,"a":1}}"#;
        let expected = r#"{"a":{"a":1,"z":0},"z":0}"#;
        let metadata = BoundedMetadata::try_new(input)
            .expect("unordered object fixture remains valid metadata");

        assert_eq!(metadata.as_str(), expected);
    }

    #[test]
    fn schema_rejects_nested_duplicate_object_members() {
        let outcome = CanonicalJsonObjectSchema::try_new(
            r#"{"type":"object","properties":{"value":{"type":"string","type":"number"}}}"#,
        );

        assert_eq!(outcome, Err(RegistryValueError::Schema));
    }
}

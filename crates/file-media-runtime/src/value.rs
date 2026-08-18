use std::{error::Error, fmt, num::NonZeroU64, str::FromStr, sync::Arc};

use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};

const SHA256_PREFIX: &str = "sha256:";
// numeric-bound: not-a-bound - fixed lowercase SHA-256 hexadecimal width
const SHA256_HEX_BYTES: usize = 64;
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
        formatter.write_str(SHA256_PREFIX)?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for FileDigest {
    type Err = RegistryValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let encoded = value
            .strip_prefix(SHA256_PREFIX)
            .ok_or(RegistryValueError::Digest)?;
        if encoded.len() != SHA256_HEX_BYTES {
            return Err(RegistryValueError::Digest);
        }
        let mut bytes = [0_u8; 32];
        for (destination, pair) in bytes.iter_mut().zip(encoded.as_bytes().chunks_exact(2)) {
            let high = lowercase_hex(pair[0]).ok_or(RegistryValueError::Digest)?;
            let low = lowercase_hex(pair[1]).ok_or(RegistryValueError::Digest)?;
            *destination = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn lowercase_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
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
        if value.len() > MAX_DECLARED_MEDIA_TYPE_BYTES || value.contains(';') {
            return Err(MediaTypeParseError);
        }
        let Some((type_name, subtype_name)) = value.split_once('/') else {
            return Err(MediaTypeParseError);
        };
        if subtype_name.contains('/')
            || !valid_media_token(type_name)
            || !valid_media_token(subtype_name)
        {
            return Err(MediaTypeParseError);
        }
        Ok(Self(Arc::from(value)))
    }
}

fn valid_media_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                )
        })
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

pub(crate) fn parse_json_without_duplicate_members(
    value: &str,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(value);
    let parsed = DuplicateAwareJson.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(parsed)
}

struct DuplicateAwareJson;

impl<'de> DeserializeSeed<'de> for DuplicateAwareJson {
    type Value = serde_json::Value;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateAwareJsonVisitor)
    }
}

struct DuplicateAwareJsonVisitor;

impl<'de> Visitor<'de> for DuplicateAwareJsonVisitor {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object members")
    }

    fn visit_bool<Error>(self, value: bool) -> Result<Self::Value, Error> {
        Ok(serde_json::Value::Bool(value))
    }

    fn visit_i64<Error>(self, value: i64) -> Result<Self::Value, Error> {
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_u64<Error>(self, value: u64) -> Result<Self::Value, Error> {
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_f64<Error>(self, value: f64) -> Result<Self::Value, Error>
    where
        Error: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| Error::custom("non-finite JSON number"))
    }

    fn visit_str<Error>(self, value: &str) -> Result<Self::Value, Error> {
        Ok(serde_json::Value::String(value.to_owned()))
    }

    fn visit_string<Error>(self, value: String) -> Result<Self::Value, Error> {
        Ok(serde_json::Value::String(value))
    }

    fn visit_none<Error>(self) -> Result<Self::Value, Error> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<Error>(self) -> Result<Self::Value, Error> {
        Ok(serde_json::Value::Null)
    }

    fn visit_seq<Access>(self, mut sequence: Access) -> Result<Self::Value, Access::Error>
    where
        Access: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(DuplicateAwareJson)? {
            values.push(value);
        }
        Ok(serde_json::Value::Array(values))
    }

    fn visit_map<Access>(self, mut object: Access) -> Result<Self::Value, Access::Error>
    where
        Access: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(name) = object.next_key::<String>()? {
            if values.contains_key(&name) {
                return Err(Access::Error::custom("duplicate JSON object member"));
            }
            let value = object.next_value_seed(DuplicateAwareJson)?;
            values.insert(name, value);
        }
        Ok(serde_json::Value::Object(values))
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
        })
    }
}

impl Error for RegistryValueError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_media_types_reject_parameters_and_uppercase() {
        assert!(CanonicalMediaType::from_str("text/plain").is_ok());
        assert!(CanonicalMediaType::from_str("text/plain; charset=utf-8").is_err());
        assert!(CanonicalMediaType::from_str("Text/plain").is_err());
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
    fn schema_rejects_nested_duplicate_object_members() {
        let outcome = CanonicalJsonObjectSchema::try_new(
            r#"{"type":"object","properties":{"value":{"type":"string","type":"number"}}}"#,
        );

        assert_eq!(outcome, Err(RegistryValueError::Schema));
    }
}

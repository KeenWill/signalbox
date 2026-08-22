//! Pluggable corpus-store contracts governed by the evaluation-system specification.

use std::{
    error::Error,
    fmt,
    future::{Future, ready},
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{ApprovalJudgeCorpus, manifest::load_manifest_corpus};

/// A boxed store operation, keeping the pluggable trait object-safe.
pub type CorpusStoreFuture<'a, Value> =
    Pin<Box<dyn Future<Output = Result<Value, CorpusStoreError>> + Send + 'a>>;

/// Logical corpus identity within one Signalbox instance.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusKey {
    /// Stable suite name.
    pub name: String,
    /// Corpus release chosen by its author.
    pub version: String,
}

/// A SHA-256 digest rendered as lowercase hexadecimal at every text boundary.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// Creates a digest from its 32 raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut rendered = String::with_capacity(64);
        for byte in self.0 {
            rendered.push(char::from(HEX[usize::from(byte >> 4)]));
            rendered.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        rendered
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Sha256Digest")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl FromStr for Sha256Digest {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(DigestParseError);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = decode_hex(pair[0]).ok_or(DigestParseError)?;
            let low = decode_hex(pair[1]).ok_or(DigestParseError)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn decode_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl Serialize for Sha256Digest {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(de::Error::custom)
    }
}

/// A hexadecimal SHA-256 digest was not exactly 64 lowercase digits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DigestParseError;

impl fmt::Display for DigestParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SHA-256 digest must be exactly 64 lowercase hexadecimal digits")
    }
}

impl Error for DigestParseError {}

/// Durable provenance for a corpus registration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CorpusSourceDescriptor {
    /// Cases loaded from a repository checkout at a relative path.
    Repository {
        /// Repository identity recorded as author-supplied provenance.
        repository: String,
        /// Slash-separated path relative to the repository checkout root.
        path: String,
    },
    /// Cases authored directly into the instance database.
    DatabaseNative,
    /// Cases held by a content-addressed blob store.
    BlobReference {
        /// Optional instance-local store binding name.
        store: Option<String>,
        /// SHA-256 content identity of the serialized case source.
        digest: Sha256Digest,
        /// Expected source byte length.
        byte_length: u64,
    },
}

/// Metadata an instance uses to enumerate a corpus and its origin.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusRegistration {
    /// Instance-local logical identity.
    key: CorpusKey,
    /// Version of the case representation.
    format_version: u32,
    /// Storage-form-independent logical corpus digest.
    corpus_sha256: Sha256Digest,
    /// Number of cases covered by the digest.
    case_count: u64,
    /// Where this registration's content originated.
    source: CorpusSourceDescriptor,
}

impl CorpusRegistration {
    /// Constructs registration metadata from an admitted corpus.
    pub fn new(
        key: CorpusKey,
        source: CorpusSourceDescriptor,
        corpus: &ApprovalJudgeCorpus,
    ) -> Result<Self, crate::manifest::ManifestError> {
        crate::validate_corpus(corpus).map_err(crate::manifest::ManifestError::Corpus)?;
        crate::manifest::validate_registration_metadata(&key, &source)?;
        let corpus_sha256 = crate::manifest::corpus_digest(corpus)?;
        let case_count = u64::try_from(corpus.cases.len())
            .map_err(|_| crate::manifest::ManifestError::LengthOverflow)?;
        Ok(Self {
            key,
            format_version: corpus.format_version,
            corpus_sha256,
            case_count,
            source,
        })
    }

    /// Reconstructs validated registration metadata without loading case rows.
    pub(crate) fn from_stored_metadata(
        key: CorpusKey,
        format_version: u32,
        corpus_sha256: Sha256Digest,
        case_count: u64,
        source: CorpusSourceDescriptor,
    ) -> Result<Self, crate::manifest::ManifestError> {
        if format_version != crate::CORPUS_FORMAT_VERSION {
            return Err(crate::manifest::ManifestError::UnsupportedCorpusVersion {
                observed: format_version,
            });
        }
        if case_count == 0 {
            return Err(crate::manifest::ManifestError::Corpus(
                crate::CorpusLoadError::EmptyCorpus,
            ));
        }
        crate::manifest::validate_registration_metadata(&key, &source)?;
        Ok(Self {
            key,
            format_version,
            corpus_sha256,
            case_count,
            source,
        })
    }

    /// Returns the instance-local logical identity.
    #[must_use]
    pub const fn key(&self) -> &CorpusKey {
        &self.key
    }

    /// Returns the admitted case-representation version.
    #[must_use]
    pub const fn format_version(&self) -> u32 {
        self.format_version
    }

    /// Returns the storage-independent logical corpus digest.
    #[must_use]
    pub const fn corpus_sha256(&self) -> Sha256Digest {
        self.corpus_sha256
    }

    /// Returns the number of cases covered by the digest.
    #[must_use]
    pub const fn case_count(&self) -> u64 {
        self.case_count
    }

    /// Returns the admitted source descriptor.
    #[must_use]
    pub const fn source(&self) -> &CorpusSourceDescriptor {
        &self.source
    }
}

/// Pluggable corpus lookup used by the evaluation harness.
pub trait CorpusStore: Send + Sync {
    /// Enumerates registrations available to this store instance.
    fn enumerate(&self) -> CorpusStoreFuture<'_, Vec<CorpusRegistration>>;

    /// Loads one digest-verified corpus by logical identity.
    fn load<'a>(&'a self, key: &'a CorpusKey) -> CorpusStoreFuture<'a, ApprovalJudgeCorpus>;
}

/// Repository-file corpus store backed by one portable manifest.
#[derive(Clone, Debug)]
pub struct DiskCorpusStore {
    manifest_path: PathBuf,
    registration: CorpusRegistration,
    corpus: ApprovalJudgeCorpus,
}

impl DiskCorpusStore {
    /// Opens a manifest and verifies the repository-backed case source it names.
    pub fn open(manifest_path: impl AsRef<Path>) -> Result<Self, CorpusStoreError> {
        let manifest_path = manifest_path.as_ref().to_path_buf();
        let loaded = load_manifest_corpus(&manifest_path).map_err(CorpusStoreError::Manifest)?;
        require_disk_store_source(loaded.registration.source())?;
        Ok(Self {
            manifest_path,
            registration: loaded.registration,
            corpus: loaded.corpus,
        })
    }

    /// Returns the manifest path that configured this store.
    #[must_use]
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }
}

fn require_disk_store_source(source: &CorpusSourceDescriptor) -> Result<(), CorpusStoreError> {
    match source {
        CorpusSourceDescriptor::Repository { .. } => Ok(()),
        CorpusSourceDescriptor::DatabaseNative => {
            Err(CorpusStoreError::DatabaseNativeRequiresDatabaseImport)
        }
        CorpusSourceDescriptor::BlobReference { .. } => {
            Err(CorpusStoreError::BlobBackendUnavailable)
        }
    }
}

impl CorpusStore for DiskCorpusStore {
    fn enumerate(&self) -> CorpusStoreFuture<'_, Vec<CorpusRegistration>> {
        Box::pin(ready(Ok(vec![self.registration.clone()])))
    }

    fn load<'a>(&'a self, key: &'a CorpusKey) -> CorpusStoreFuture<'a, ApprovalJudgeCorpus> {
        let result = if key == self.registration.key() {
            Ok(self.corpus.clone())
        } else {
            Err(CorpusStoreError::NotFound(key.clone()))
        };
        Box::pin(ready(result))
    }
}

/// A pluggable corpus store could not complete an operation.
#[derive(Debug)]
pub enum CorpusStoreError {
    /// The requested logical corpus is not registered.
    NotFound(CorpusKey),
    /// A manifest or its content failed validation.
    Manifest(crate::manifest::ManifestError),
    /// Durable database access failed.
    Database(sqlx::Error),
    /// Stored case JSON did not decode into the strict case shape.
    StoredCaseJson(serde_json::Error),
    /// A database row violated the store representation.
    CorruptRegistration(CorpusStoreCorruption),
    /// Stored content failed shared corpus or registration admission.
    CorruptStoredAdmission(crate::manifest::ManifestError),
    /// The manifest names a blob, but this slice has no blob backend.
    BlobBackendUnavailable,
    /// Repository content was supplied without verified manifest source bytes.
    RepositorySourceRequiresManifestImport,
    /// Embedded database-native content was supplied to the repository-file store.
    DatabaseNativeRequiresDatabaseImport,
}

/// Closed corruption classifications for durable corpus rows and registrations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CorpusStoreCorruption {
    /// An in-memory case count cannot be represented durably.
    CaseCountOutOfRange,
    /// An in-memory replay position cannot be represented durably.
    CasePositionOutOfRange,
    /// An in-memory format version cannot be represented durably.
    FormatVersionOutOfRange,
    /// A stored signed case count was negative.
    NegativeCaseCount,
    /// A stored signed format version was negative.
    NegativeFormatVersion,
    /// Registration and content format versions differ.
    FormatVersionMismatch,
    /// Registration and content case counts differ.
    CaseCountMismatch,
    /// Registration and content digests differ.
    CorpusDigestMismatch,
    /// The durable replay sequence differs from its stored identity.
    ReplayDigestMismatch,
    /// A durable case row key differs from the identity in its JSON payload.
    CaseIdMismatch,
    /// The logical key already names different metadata or cases.
    RegistrationConflict,
    /// A durable source discriminator is unknown.
    UnknownSourceKind(String),
    /// A source-kind-required durable column is null.
    MissingSourceField(&'static str),
    /// A durable blob byte length is not an unsigned 64-bit integer.
    InvalidBlobByteLength,
    /// A durable digest is not exactly 32 bytes.
    InvalidDigestLength,
}

impl fmt::Display for CorpusStoreCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CaseCountOutOfRange => {
                formatter.write_str("case count exceeds PostgreSQL bigint")
            }
            Self::CasePositionOutOfRange => {
                formatter.write_str("case position exceeds PostgreSQL bigint")
            }
            Self::FormatVersionOutOfRange => {
                formatter.write_str("format version exceeds PostgreSQL integer")
            }
            Self::NegativeCaseCount => formatter.write_str("stored case count is negative"),
            Self::NegativeFormatVersion => formatter.write_str("stored format version is negative"),
            Self::FormatVersionMismatch => {
                formatter.write_str("registration and corpus format versions differ")
            }
            Self::CaseCountMismatch => {
                formatter.write_str("registration and stored case counts differ")
            }
            Self::CorpusDigestMismatch => {
                formatter.write_str("registration digest does not match stored cases")
            }
            Self::ReplayDigestMismatch => {
                formatter.write_str("stored replay order does not match its durable digest")
            }
            Self::CaseIdMismatch => {
                formatter.write_str("stored case identity does not match its row key")
            }
            Self::RegistrationConflict => {
                formatter.write_str("logical key already names different metadata or cases")
            }
            Self::UnknownSourceKind(observed) => {
                write!(formatter, "unknown source kind {observed:?}")
            }
            Self::MissingSourceField(field) => {
                write!(formatter, "required source field {field} is null")
            }
            Self::InvalidBlobByteLength => formatter.write_str("blob byte length is invalid"),
            Self::InvalidDigestLength => formatter.write_str("SHA-256 digest is not 32 bytes"),
        }
    }
}

impl fmt::Display for CorpusStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(key) => write!(
                formatter,
                "corpus {}/{} is not registered",
                key.name, key.version
            ),
            Self::Manifest(source) => write!(formatter, "corpus manifest is invalid: {source}"),
            Self::Database(source) => {
                write!(formatter, "corpus database operation failed: {source}")
            }
            Self::StoredCaseJson(source) => {
                write!(formatter, "stored case JSON is invalid: {source}")
            }
            Self::CorruptRegistration(corruption) => {
                write!(formatter, "corpus registration is corrupt: {corruption}")
            }
            Self::CorruptStoredAdmission(source) => {
                write!(formatter, "stored corpus registration is corrupt: {source}")
            }
            Self::BlobBackendUnavailable => formatter.write_str(
                "blob-backed corpus content cannot be loaded: no blob corpus backend is configured",
            ),
            Self::RepositorySourceRequiresManifestImport => formatter.write_str(
                "repository-backed corpus content must be imported through a verified manifest",
            ),
            Self::DatabaseNativeRequiresDatabaseImport => formatter.write_str(
                "database-native corpus content must be imported through the database store",
            ),
        }
    }
}

impl Error for CorpusStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Manifest(source) => Some(source),
            Self::Database(source) => Some(source),
            Self::StoredCaseJson(source) => Some(source),
            Self::CorruptStoredAdmission(source) => Some(source),
            Self::NotFound(_)
            | Self::CorruptRegistration(_)
            | Self::BlobBackendUnavailable
            | Self::RepositorySourceRequiresManifestImport
            | Self::DatabaseNativeRequiresDatabaseImport => None,
        }
    }
}

impl From<sqlx::Error> for CorpusStoreError {
    fn from(source: sqlx::Error) -> Self {
        Self::Database(source)
    }
}

#[cfg(test)]
mod tests {
    use super::{CorpusSourceDescriptor, CorpusStoreError, require_disk_store_source};

    #[test]
    fn disk_store_rejects_database_native_sources() {
        let error = require_disk_store_source(&CorpusSourceDescriptor::DatabaseNative)
            .expect_err("database-native content requires database import");

        assert!(matches!(
            error,
            CorpusStoreError::DatabaseNativeRequiresDatabaseImport
        ));
    }

    #[test]
    fn source_descriptor_rejects_unknown_variant_fields() {
        let error = serde_json::from_value::<CorpusSourceDescriptor>(serde_json::json!({
            "kind": "repository",
            "repository": "KeenWill/signalbox",
            "path": "corpora/cases.json",
            "digest": "0000000000000000000000000000000000000000000000000000000000000000"
        }))
        .expect_err("unknown source metadata is rejected");

        assert!(error.to_string().contains("unknown field `digest`"));
    }
}

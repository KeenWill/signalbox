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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CorpusSourceDescriptor {
    /// Cases loaded from a repository checkout at a relative path.
    Repository {
        /// Repository identity recorded as author-supplied provenance.
        repository: String,
        /// Slash-separated path relative to the manifest.
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusRegistration {
    /// Instance-local logical identity.
    pub key: CorpusKey,
    /// Version of the case representation.
    pub format_version: u32,
    /// Storage-form-independent logical corpus digest.
    pub corpus_sha256: Sha256Digest,
    /// Number of cases covered by the digest.
    pub case_count: u64,
    /// Where this registration's content originated.
    pub source: CorpusSourceDescriptor,
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

impl CorpusStore for DiskCorpusStore {
    fn enumerate(&self) -> CorpusStoreFuture<'_, Vec<CorpusRegistration>> {
        Box::pin(ready(Ok(vec![self.registration.clone()])))
    }

    fn load<'a>(&'a self, key: &'a CorpusKey) -> CorpusStoreFuture<'a, ApprovalJudgeCorpus> {
        let result = if key == &self.registration.key {
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
    /// A database row violated the store representation.
    CorruptRegistration(String),
    /// The manifest names a blob, but this slice has no blob backend.
    BlobBackendUnavailable,
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
            Self::CorruptRegistration(detail) => {
                write!(formatter, "corpus registration is corrupt: {detail}")
            }
            Self::BlobBackendUnavailable => formatter.write_str(
                "blob-backed corpus content cannot be loaded: no blob corpus backend is configured",
            ),
        }
    }
}

impl Error for CorpusStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Manifest(source) => Some(source),
            Self::Database(source) => Some(source),
            Self::NotFound(_) | Self::CorruptRegistration(_) | Self::BlobBackendUnavailable => None,
        }
    }
}

impl From<sqlx::Error> for CorpusStoreError {
    fn from(source: sqlx::Error) -> Self {
        Self::Database(source)
    }
}

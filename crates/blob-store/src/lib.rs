//! Streaming contract shared by immutable blob-store adapters.
//!
//! The normative specification is `docs/spec/blob-storage.md`.

use std::{error::Error, fmt, future::Future, num::NonZeroU64, pin::Pin, sync::Arc};

use signalbox_domain::BlobDigest;
use tokio::io::AsyncRead;

#[cfg(feature = "test-support")]
pub mod conformance;

/// Maximum ASCII bytes in one durable deployment store name.
pub const MAX_BLOB_STORE_NAME_BYTES: usize = 64;

/// Maximum bytes one adapter range operation may retain in memory.
pub const MAX_BLOB_RANGE_BYTES: u64 = 4_194_304;

/// Maximum durable store identities in one version-one deployment catalog.
pub const MAX_BLOB_STORES: usize = 32;

/// Maximum UTF-8 bytes in one durable recorded object key.
pub const MAX_BLOB_OBJECT_KEY_BYTES: usize = 1024;

/// A validated durable deployment identity for one blob store.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlobStoreName(Arc<str>);

impl BlobStoreName {
    /// Admits one canonical lowercase-ASCII deployment store name.
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self, BlobStoreNameError> {
        let value = value.into();
        let mut bytes = value.as_bytes().iter().copied();
        let valid_first = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase());
        let valid_rest = bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
        });
        if !valid_first || !valid_rest || value.len() > MAX_BLOB_STORE_NAME_BYTES {
            Err(BlobStoreNameError { rejected: value })
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows the exact durable spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobStoreName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A deployment store name did not match `[a-z][a-z0-9_-]{0,63}`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobStoreNameError {
    rejected: Arc<str>,
}

impl BlobStoreNameError {
    /// Borrows the exact rejected name.
    pub fn rejected(&self) -> &str {
        &self.rejected
    }
}

impl fmt::Display for BlobStoreNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("blob store name is invalid")
    }
}

impl Error for BlobStoreNameError {}

/// One safe, relative object key recorded in the replica catalog.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlobObjectKey(Arc<str>);

impl BlobObjectKey {
    /// Derives the version-one object key for a digest.
    pub fn for_digest(digest: BlobDigest) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let bytes = digest.as_bytes();
        let mut encoded = String::with_capacity(64);
        for byte in bytes {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(Arc::from(format!(
            "sha256/{}/{}/{}",
            &encoded[0..2],
            &encoded[2..4],
            encoded
        )))
    }

    /// Reconstitutes a recorded relative object key without reinterpreting its layout.
    pub fn try_from_recorded(value: impl Into<Arc<str>>) -> Result<Self, BlobObjectKeyError> {
        let value = value.into();
        let path = std::path::Path::new(value.as_ref());
        let safe = !value.is_empty()
            && value.len() <= MAX_BLOB_OBJECT_KEY_BYTES
            && !value.contains('\0')
            && !value.contains('\\')
            && !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)));
        if safe {
            Ok(Self(value))
        } else {
            Err(BlobObjectKeyError { rejected: value })
        }
    }

    /// Borrows the exact recorded spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobObjectKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A recorded object key was not a safe nonempty relative path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobObjectKeyError {
    rejected: Arc<str>,
}

impl BlobObjectKeyError {
    /// Borrows the exact rejected key.
    pub fn rejected(&self) -> &str {
        &self.rejected
    }
}

impl fmt::Display for BlobObjectKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("blob object key is invalid")
    }
}

impl Error for BlobObjectKeyError {}

/// Expected immutable identity and positive stored length for publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedBlob {
    digest: BlobDigest,
    byte_length: u64,
}

impl ExpectedBlob {
    /// Constructs one expected blob from a type-level positive length.
    pub const fn new(digest: BlobDigest, byte_length: NonZeroU64) -> Self {
        Self {
            digest,
            byte_length: byte_length.get(),
        }
    }

    /// Constructs one expected nonempty blob.
    pub const fn try_new(digest: BlobDigest, byte_length: u64) -> Result<Self, EmptyBlobError> {
        if byte_length == 0 {
            Err(EmptyBlobError)
        } else {
            let Some(byte_length) = NonZeroU64::new(byte_length) else {
                return Err(EmptyBlobError);
            };
            Ok(Self::new(digest, byte_length))
        }
    }

    /// Returns the expected content identity.
    pub const fn digest(self) -> BlobDigest {
        self.digest
    }

    /// Returns the expected positive byte length.
    pub const fn byte_length(self) -> u64 {
        self.byte_length
    }
}

/// An empty byte sequence cannot be admitted as a blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmptyBlobError;

impl fmt::Display for EmptyBlobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a blob must contain at least one byte")
    }
}

impl Error for EmptyBlobError {}

/// Whether publication created the destination or verified an existing one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlobPutOutcome {
    /// This operation atomically published and verified the object.
    Published { key: BlobObjectKey },
    /// This operation atomically replaced a corrupt recorded object.
    Repaired { key: BlobObjectKey },
    /// The final key already contained the exact expected bytes.
    AlreadyPresent { key: BlobObjectKey },
}

impl BlobPutOutcome {
    /// Returns the exact key whose durable bytes were verified.
    pub const fn key(&self) -> &BlobObjectKey {
        match self {
            Self::Published { key } | Self::Repaired { key } | Self::AlreadyPresent { key } => key,
        }
    }
}

/// One opened store object and its backend-reported length.
pub struct OpenedBlob {
    byte_length: u64,
    reader: BlobReader,
}

impl OpenedBlob {
    /// Constructs one opened streaming object.
    pub fn new(byte_length: u64, reader: BlobReader) -> Self {
        Self {
            byte_length,
            reader,
        }
    }

    /// Returns the backend-reported byte length.
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    /// Takes the streaming reader without materializing its content.
    pub fn into_reader(self) -> BlobReader {
        self.reader
    }
}

impl fmt::Debug for OpenedBlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenedBlob")
            .field("byte_length", &self.byte_length)
            .field("reader", &"[STREAM]")
            .finish()
    }
}

/// Owned asynchronous byte stream used at store boundaries.
pub type BlobReader = Box<dyn AsyncRead + Send + Unpin>;

/// Boxed store operation future, keeping the adapter contract object-safe.
pub type BlobStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, BlobStoreError>> + Send + 'a>>;

/// One streaming immutable-object store.
pub trait BlobStore: Send + Sync {
    /// Publishes and verifies one expected blob under its content-derived key.
    fn put<'a>(
        &'a self,
        expected: ExpectedBlob,
        source: BlobReader,
    ) -> BlobStoreFuture<'a, BlobPutOutcome>;

    /// Opens one recorded object as a stream without materializing it.
    fn open<'a>(&'a self, key: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob>;

    /// Verifies one exact object generation, then rewinds and returns that generation.
    fn open_verified<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
    ) -> BlobStoreFuture<'a, OpenedBlob>;

    /// Re-verifies one exact object generation while retaining one bounded range.
    fn open_range<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
        offset: u64,
        byte_length: NonZeroU64,
    ) -> BlobStoreFuture<'a, OpenedBlob>;
}

/// Closed class of one adapter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobStoreFailureKind {
    /// The recorded object does not exist in this store.
    NotFound,
    /// Stored or supplied bytes do not match the expected digest and length.
    VerificationFailed,
    /// Backend I/O failed without proving absence or corruption.
    Unavailable,
}

/// Expected and observed facts retained by a verification failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobVerificationFailure {
    expected: ExpectedBlob,
    observed_digest: Option<BlobDigest>,
    observed_length: u64,
}

impl BlobVerificationFailure {
    /// Constructs one failed verification from its exact observed facts.
    pub const fn new(
        expected: ExpectedBlob,
        observed_digest: Option<BlobDigest>,
        observed_length: u64,
    ) -> Self {
        Self {
            expected,
            observed_digest,
            observed_length,
        }
    }

    /// Returns the expected immutable identity and length.
    pub const fn expected(self) -> ExpectedBlob {
        self.expected
    }

    /// Returns the digest when the complete observed stream was hashed.
    pub const fn observed_digest(self) -> Option<BlobDigest> {
        self.observed_digest
    }

    /// Returns the observed or saturated byte count.
    pub const fn observed_length(self) -> u64 {
        self.observed_length
    }
}

/// Failure returned by a blob-store adapter.
#[derive(Debug)]
pub struct BlobStoreError {
    kind: BlobStoreFailureKind,
    operation: &'static str,
    verification: Option<BlobVerificationFailure>,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl BlobStoreError {
    /// Constructs a typed backend I/O failure retaining its adapter source.
    pub fn io(operation: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind: BlobStoreFailureKind::Unavailable,
            operation,
            verification: None,
            source: Some(Box::new(source)),
        }
    }

    /// Constructs a typed missing-object failure.
    pub const fn not_found(operation: &'static str) -> Self {
        Self {
            kind: BlobStoreFailureKind::NotFound,
            operation,
            verification: None,
            source: None,
        }
    }

    /// Constructs an unavailable failure for a violated local adapter assumption.
    pub const fn unavailable(operation: &'static str) -> Self {
        Self {
            kind: BlobStoreFailureKind::Unavailable,
            operation,
            verification: None,
            source: None,
        }
    }

    /// Constructs a verification failure retaining expected and observed facts.
    pub fn verification(operation: &'static str, failure: BlobVerificationFailure) -> Self {
        Self {
            kind: BlobStoreFailureKind::VerificationFailed,
            operation,
            verification: Some(failure),
            source: None,
        }
    }

    /// Returns the closed failure classification.
    pub const fn kind(&self) -> BlobStoreFailureKind {
        self.kind
    }

    /// Returns retained verification facts for a verification failure.
    pub const fn verification_failure(&self) -> Option<BlobVerificationFailure> {
        self.verification
    }
}

impl fmt::Display for BlobStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "blob store {} failed: {:?}",
            self.operation, self.kind
        )?;
        if let Some(failure) = self.verification {
            write!(
                formatter,
                "; expected {} bytes at {}, observed {} bytes",
                failure.expected.byte_length(),
                failure.expected.digest(),
                failure.observed_length
            )?;
            if let Some(digest) = failure.observed_digest {
                write!(formatter, " at {digest}")?;
            }
        }
        Ok(())
    }
}

impl Error for BlobStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::BlobDigest;

    use super::{
        BlobObjectKey, BlobStoreName, ExpectedBlob, MAX_BLOB_OBJECT_KEY_BYTES,
        MAX_BLOB_STORE_NAME_BYTES,
    };

    #[test]
    fn object_key_is_content_derived_and_sharded() {
        let digest = BlobDigest::from_bytes([0xab; 32]);

        assert_eq!(
            BlobObjectKey::for_digest(digest).as_str(),
            "sha256/ab/ab/abababababababababababababababababababababababababababababababab"
        );
    }

    #[test]
    fn recorded_object_key_rejects_parent_escape() {
        assert!(BlobObjectKey::try_from_recorded("../outside").is_err());
    }

    #[test]
    fn store_name_rejects_noncanonical_spelling() {
        let admitted = "primary_store-1";

        assert!(BlobStoreName::try_new(" primary ").is_err());
        assert!(BlobStoreName::try_new("Primary").is_err());
        assert!(BlobStoreName::try_new("é").is_err());
        assert!(BlobStoreName::try_new("a".repeat(MAX_BLOB_STORE_NAME_BYTES + 1)).is_err());
        assert_eq!(
            BlobStoreName::try_new(admitted)
                .expect("the canonical fixture is admitted")
                .as_str(),
            admitted
        );
    }

    #[test]
    fn recorded_object_key_rejects_oversized_spelling() {
        assert!(
            BlobObjectKey::try_from_recorded("a".repeat(MAX_BLOB_OBJECT_KEY_BYTES + 1)).is_err()
        );
    }

    #[test]
    fn expected_blob_rejects_empty_content() {
        assert!(ExpectedBlob::try_new(BlobDigest::digest(&[]), 0).is_err());
    }
}

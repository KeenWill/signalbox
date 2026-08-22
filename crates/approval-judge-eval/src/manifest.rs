//! Portable corpus manifests governed by the evaluation-system specification.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};

use crate::{
    ApprovalJudgeCase, ApprovalJudgeCorpus, CORPUS_FORMAT_VERSION,
    store::{CorpusKey, CorpusRegistration, CorpusSourceDescriptor, Sha256Digest},
};

/// The portable manifest representation accepted by this slice.
pub const CORPUS_MANIFEST_VERSION: u32 = 1;
const CORPUS_DIGEST_DOMAIN: &[u8] = b"signalbox-eval-corpus-v1\0";
// Hard metadata ceilings keep manifests and indexed durable identities bounded;
// ordinary corpus names and paths remain far below them.
const MAX_IDENTITY_BYTES: usize = 128;
const MAX_REPOSITORY_BYTES: usize = 2_048;
const MAX_REPOSITORY_PATH_BYTES: usize = 1_024;
const MAX_PORTABLE_PATH_COMPONENT_UNITS: usize = 255;
const MAX_BLOB_STORE_BYTES: usize = 64;

/// A self-describing, portable corpus registration document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusManifest {
    /// Manifest representation version.
    manifest_version: u32,
    /// Stable suite name.
    name: String,
    /// Corpus release chosen by its author.
    version: String,
    /// Version of the referenced case representation.
    corpus_format_version: u32,
    /// Location and origin of the case content.
    case_source: ManifestCaseSource,
    /// Digests that bind the manifest to its logical and serialized content.
    integrity: CorpusIntegrity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCorpusManifest {
    manifest_version: u32,
    name: String,
    version: String,
    corpus_format_version: u32,
    case_source: ManifestCaseSource,
    integrity: CorpusIntegrity,
}

impl<'de> Deserialize<'de> for CorpusManifest {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let raw = RawCorpusManifest::deserialize(deserializer)?;
        Self::try_from(raw).map_err(de::Error::custom)
    }
}

impl TryFrom<RawCorpusManifest> for CorpusManifest {
    type Error = ManifestError;

    fn try_from(raw: RawCorpusManifest) -> Result<Self, Self::Error> {
        let manifest = Self {
            manifest_version: raw.manifest_version,
            name: raw.name,
            version: raw.version,
            corpus_format_version: raw.corpus_format_version,
            case_source: raw.case_source,
            integrity: raw.integrity,
        };
        validate_manifest_header(&manifest)?;
        Ok(manifest)
    }
}

impl CorpusManifest {
    /// Returns the manifest representation version.
    #[must_use]
    pub const fn manifest_version(&self) -> u32 {
        self.manifest_version
    }

    /// Returns the stable suite name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the author-chosen corpus release.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the referenced case-representation version.
    #[must_use]
    pub const fn corpus_format_version(&self) -> u32 {
        self.corpus_format_version
    }

    /// Returns the admitted case source.
    #[must_use]
    pub const fn case_source(&self) -> &ManifestCaseSource {
        &self.case_source
    }

    /// Returns the admitted integrity material.
    #[must_use]
    pub const fn integrity(&self) -> &CorpusIntegrity {
        &self.integrity
    }
}

/// Case content forms a portable manifest can name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManifestCaseSource {
    /// A file in the repository containing the manifest.
    Repository {
        /// Author-supplied repository identity retained as provenance.
        repository: String,
        /// Path relative to the manifest file.
        path: String,
    },
    /// Case content embedded for direct database-native import.
    DatabaseNative {
        /// Cases to insert in replay order.
        cases: Vec<ApprovalJudgeCase>,
    },
    /// Content-addressed case bytes held in blob storage.
    BlobReference {
        /// Optional instance-local store binding name.
        store: Option<String>,
        /// SHA-256 identity of the serialized case source.
        digest: Sha256Digest,
        /// Expected byte length of that source.
        byte_length: u64,
    },
}

/// Integrity material for both the logical corpus and its constituent cases.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusIntegrity {
    /// Storage-form-independent digest of the logical cases.
    pub corpus_sha256: Sha256Digest,
    /// Digest of repository source bytes, when those bytes are locally addressable.
    pub source_sha256: Option<Sha256Digest>,
    /// Per-case canonical JSON digests keyed by stable case identity.
    pub cases: Vec<CaseIntegrity>,
}

/// Integrity material for one logical case.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseIntegrity {
    /// Stable case identity.
    pub id: String,
    /// SHA-256 over the case's RFC-8785-compatible canonical JSON.
    pub sha256: Sha256Digest,
}

/// A manifest resolved to verified content and store registration metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedManifestCorpus {
    /// Parsed manifest.
    pub manifest: CorpusManifest,
    /// Metadata suitable for store enumeration.
    pub registration: CorpusRegistration,
    /// Verified cases in replay order.
    pub corpus: ApprovalJudgeCorpus,
}

/// Decodes and structurally validates a portable manifest.
pub fn decode_manifest(bytes: &[u8]) -> Result<CorpusManifest, ManifestError> {
    let raw: RawCorpusManifest = serde_json::from_slice(bytes).map_err(ManifestError::Json)?;
    CorpusManifest::try_from(raw)
}

/// Reads a manifest and resolves its repository or embedded case source.
pub fn load_manifest_corpus(path: impl AsRef<Path>) -> Result<LoadedManifestCorpus, ManifestError> {
    let supplied_path = path.as_ref();
    let path = fs::canonicalize(supplied_path).map_err(|source| ManifestError::Read {
        path: supplied_path.to_path_buf(),
        source,
    })?;
    let bytes = fs::read(&path).map_err(|source| ManifestError::Read {
        path: path.clone(),
        source,
    })?;
    let manifest = decode_manifest(&bytes)?;
    let (corpus, source) = match &manifest.case_source {
        ManifestCaseSource::Repository {
            repository,
            path: relative,
        } => {
            let relative_path = portable_relative_path(relative)?;
            let source_path = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(relative_path);
            let resolved_source_path = resolved_repository_source(&path, &source_path)?;
            let source_bytes =
                fs::read(&resolved_source_path).map_err(|source| ManifestError::Read {
                    path: resolved_source_path,
                    source,
                })?;
            let observed = digest_bytes(&source_bytes);
            let expected = manifest
                .integrity
                .source_sha256
                .ok_or(ManifestError::MissingSourceDigest)?;
            if observed != expected {
                return Err(ManifestError::SourceDigestMismatch { expected, observed });
            }
            let corpus = crate::decode_corpus(&source_bytes).map_err(ManifestError::Corpus)?;
            let durable_path = repository_relative_path(&path, &source_path)?;
            (
                corpus,
                CorpusSourceDescriptor::Repository {
                    repository: repository.clone(),
                    path: durable_path,
                },
            )
        }
        ManifestCaseSource::DatabaseNative { cases } => {
            if manifest.integrity.source_sha256.is_some() {
                return Err(ManifestError::UnexpectedSourceDigest);
            }
            if cases.is_empty() {
                return Err(ManifestError::Corpus(crate::CorpusLoadError::EmptyCorpus));
            }
            (
                ApprovalJudgeCorpus {
                    format_version: manifest.corpus_format_version,
                    cases: cases.clone(),
                },
                CorpusSourceDescriptor::DatabaseNative,
            )
        }
        ManifestCaseSource::BlobReference { .. } => {
            if manifest.integrity.source_sha256.is_some() {
                return Err(ManifestError::UnexpectedSourceDigest);
            }
            return Err(ManifestError::BlobBackendUnavailable);
        }
    };
    validate_manifest_content(manifest, corpus, source)
}

/// Computes the version-one logical corpus digest owned by the evaluation spec.
pub fn corpus_digest(corpus: &ApprovalJudgeCorpus) -> Result<Sha256Digest, ManifestError> {
    validate_digest_corpus(corpus)?;
    validate_unique_case_ids(&corpus.cases)?;
    let mut cases: Vec<_> = corpus.cases.iter().collect();
    cases.sort_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
    let count = u64::try_from(cases.len()).map_err(|_| ManifestError::LengthOverflow)?;
    let mut hasher = Sha256::new();
    hasher.update(CORPUS_DIGEST_DOMAIN);
    hasher.update(count.to_be_bytes());
    for case in cases {
        let bytes = canonical_case_json(case)?;
        let length = u64::try_from(bytes.len()).map_err(|_| ManifestError::LengthOverflow)?;
        hasher.update(length.to_be_bytes());
        hasher.update(bytes);
    }
    Ok(finish_digest(hasher))
}

/// Computes canonical per-case integrity in case-identifier order.
pub fn case_integrity(corpus: &ApprovalJudgeCorpus) -> Result<Vec<CaseIntegrity>, ManifestError> {
    validate_digest_corpus(corpus)?;
    validate_unique_case_ids(&corpus.cases)?;
    let mut cases: Vec<_> = corpus.cases.iter().collect();
    cases.sort_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
    cases
        .into_iter()
        .map(|case| {
            Ok(CaseIntegrity {
                id: case.id.clone(),
                sha256: digest_bytes(&canonical_case_json(case)?),
            })
        })
        .collect()
}

/// Computes SHA-256 over exact source bytes.
#[must_use]
pub fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    finish_digest(hasher)
}

fn finish_digest(hasher: Sha256) -> Sha256Digest {
    Sha256Digest::from_bytes(hasher.finalize().into())
}

fn canonical_case_json(case: &ApprovalJudgeCase) -> Result<Vec<u8>, ManifestError> {
    // This schema contains strings, nulls, and objects only. serde_json's map
    // representation is bytewise-key-ordered without `preserve_order`, so its
    // compact serialization is the RFC 8785 form for every admitted value.
    let value = serde_json::to_value(case).map_err(ManifestError::Json)?;
    serde_json::to_vec(&value).map_err(ManifestError::Json)
}

fn validate_manifest_header(manifest: &CorpusManifest) -> Result<(), ManifestError> {
    if manifest.manifest_version != CORPUS_MANIFEST_VERSION {
        return Err(ManifestError::UnsupportedManifestVersion {
            observed: manifest.manifest_version,
        });
    }
    if manifest.corpus_format_version != CORPUS_FORMAT_VERSION {
        return Err(ManifestError::UnsupportedCorpusVersion {
            observed: manifest.corpus_format_version,
        });
    }
    validate_identity_component("name", &manifest.name)?;
    validate_identity_component("version", &manifest.version)?;
    validate_integrity_case_ids(&manifest.integrity.cases)?;
    match &manifest.case_source {
        ManifestCaseSource::Repository { repository, path } => {
            validate_bounded_text("repository", repository, MAX_REPOSITORY_BYTES)?;
            validate_bounded_text("repository path", path, MAX_REPOSITORY_PATH_BYTES)?;
            portable_relative_path(path)?;
            if manifest.integrity.source_sha256.is_none() {
                return Err(ManifestError::MissingSourceDigest);
            }
        }
        ManifestCaseSource::DatabaseNative { cases } => {
            if manifest.integrity.source_sha256.is_some() {
                return Err(ManifestError::UnexpectedSourceDigest);
            }
            crate::validate_corpus(&ApprovalJudgeCorpus {
                format_version: manifest.corpus_format_version,
                cases: cases.clone(),
            })
            .map_err(ManifestError::Corpus)?;
        }
        ManifestCaseSource::BlobReference {
            store, byte_length, ..
        } => {
            if manifest.integrity.source_sha256.is_some() {
                return Err(ManifestError::UnexpectedSourceDigest);
            }
            if *byte_length == 0 {
                return Err(ManifestError::InvalidBlobByteLength);
            }
            if let Some(store) = store {
                validate_blob_store(store)?;
            }
        }
    }
    if manifest.integrity.cases.is_empty() {
        return Err(ManifestError::MissingCaseIntegrity);
    }
    Ok(())
}

fn validate_identity_component(field: &'static str, value: &str) -> Result<(), ManifestError> {
    validate_bounded_text(field, value, MAX_IDENTITY_BYTES)
}

fn validate_digest_corpus(corpus: &ApprovalJudgeCorpus) -> Result<(), ManifestError> {
    if corpus.format_version != CORPUS_FORMAT_VERSION {
        return Err(ManifestError::UnsupportedCorpusVersion {
            observed: corpus.format_version,
        });
    }
    crate::validate_corpus(corpus).map_err(ManifestError::Corpus)
}

pub(crate) fn validate_registration_metadata(
    key: &CorpusKey,
    source: &CorpusSourceDescriptor,
) -> Result<(), ManifestError> {
    validate_identity_component("name", &key.name)?;
    validate_identity_component("version", &key.version)?;
    match source {
        CorpusSourceDescriptor::Repository { repository, path } => {
            validate_bounded_text("repository", repository, MAX_REPOSITORY_BYTES)?;
            validate_bounded_text("repository path", path, MAX_REPOSITORY_PATH_BYTES)?;
            portable_relative_path(path)?;
        }
        CorpusSourceDescriptor::DatabaseNative => {}
        CorpusSourceDescriptor::BlobReference {
            store, byte_length, ..
        } => {
            if *byte_length == 0 {
                return Err(ManifestError::InvalidBlobByteLength);
            }
            if let Some(store) = store {
                validate_blob_store(store)?;
            }
        }
    }
    Ok(())
}

fn validate_bounded_text(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), ManifestError> {
    if value.trim().is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control)
    {
        return Err(ManifestError::InvalidIdentity(field));
    }
    Ok(())
}

fn validate_blob_store(value: &str) -> Result<(), ManifestError> {
    let mut bytes = value.bytes();
    let first_is_lowercase = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase());
    let rest_is_canonical = bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    });
    if !first_is_lowercase || !rest_is_canonical || value.len() > MAX_BLOB_STORE_BYTES {
        return Err(ManifestError::InvalidBlobStore);
    }
    Ok(())
}

fn portable_relative_path(value: &str) -> Result<PathBuf, ManifestError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || path.is_absolute()
        || !value.split('/').all(portable_path_component)
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ManifestError::NonPortablePath(value.to_owned()));
    }
    Ok(path.to_path_buf())
}

fn repository_relative_path(
    manifest_path: &Path,
    source_path: &Path,
) -> Result<String, ManifestError> {
    let checkout_root = manifest_path
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .ok_or_else(|| ManifestError::RepositoryRootUnavailable(manifest_path.to_path_buf()))?;
    let relative = source_path
        .strip_prefix(checkout_root)
        .map_err(|_| ManifestError::RepositoryRootUnavailable(manifest_path.to_path_buf()))?;
    let portable = relative
        .to_str()
        .ok_or_else(|| ManifestError::NonPortablePath(relative.display().to_string()))?
        .replace('\\', "/");
    portable_relative_path(&portable)?;
    Ok(portable)
}

fn resolved_repository_source(
    manifest_path: &Path,
    source_path: &Path,
) -> Result<PathBuf, ManifestError> {
    let checkout_root = manifest_path
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .ok_or_else(|| ManifestError::RepositoryRootUnavailable(manifest_path.to_path_buf()))?;
    let resolved_root = fs::canonicalize(checkout_root).map_err(|source| ManifestError::Read {
        path: checkout_root.to_path_buf(),
        source,
    })?;
    let resolved_source = fs::canonicalize(source_path).map_err(|source| ManifestError::Read {
        path: source_path.to_path_buf(),
        source,
    })?;
    if !resolved_source.starts_with(&resolved_root) {
        return Err(ManifestError::RepositorySourceOutsideCheckout(
            source_path.to_path_buf(),
        ));
    }
    Ok(resolved_source)
}

fn portable_path_component(component: &str) -> bool {
    let component_is_bounded = component.len() <= MAX_PORTABLE_PATH_COMPONENT_UNITS
        && component.encode_utf16().count() <= MAX_PORTABLE_PATH_COMPONENT_UNITS;
    let has_control_character = component.chars().any(char::is_control);
    let has_invalid_character = component
        .bytes()
        .any(|byte| matches!(byte, b'<' | b'>' | b':' | b'\"' | b'|' | b'?' | b'*'));
    let stem = component.split('.').next().unwrap_or_default();
    let uppercase_stem = stem.to_ascii_uppercase();
    let is_reserved_device = matches!(uppercase_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || uppercase_stem
            .strip_prefix("COM")
            .is_some_and(is_reserved_device_number)
        || uppercase_stem
            .strip_prefix("LPT")
            .is_some_and(is_reserved_device_number);
    !component.is_empty()
        && component_is_bounded
        && !has_control_character
        && !component.ends_with(['.', ' '])
        && !has_invalid_character
        && !is_reserved_device
}

fn is_reserved_device_number(suffix: &str) -> bool {
    matches!(
        suffix,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

fn validate_unique_case_ids(cases: &[ApprovalJudgeCase]) -> Result<(), ManifestError> {
    let mut ids = BTreeSet::new();
    for case in cases {
        if !ids.insert(&case.id) {
            return Err(ManifestError::DuplicateCaseId(case.id.clone()));
        }
    }
    Ok(())
}

fn validate_integrity_case_ids(cases: &[CaseIntegrity]) -> Result<(), ManifestError> {
    let mut ids = BTreeSet::new();
    for case in cases {
        validate_identity_component("case id", &case.id)?;
        if !ids.insert(&case.id) {
            return Err(ManifestError::DuplicateCaseId(case.id.clone()));
        }
    }
    Ok(())
}

fn validate_manifest_content(
    manifest: CorpusManifest,
    corpus: ApprovalJudgeCorpus,
    source: CorpusSourceDescriptor,
) -> Result<LoadedManifestCorpus, ManifestError> {
    if corpus.format_version != manifest.corpus_format_version {
        return Err(ManifestError::CorpusVersionMismatch {
            manifest: manifest.corpus_format_version,
            corpus: corpus.format_version,
        });
    }
    let observed_cases = case_integrity(&corpus)?;
    let mut expected_cases = manifest.integrity.cases.clone();
    expected_cases.sort_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
    if observed_cases != expected_cases {
        return Err(ManifestError::CaseIntegrityMismatch);
    }
    let observed_corpus = corpus_digest(&corpus)?;
    if observed_corpus != manifest.integrity.corpus_sha256 {
        return Err(ManifestError::CorpusDigestMismatch {
            expected: manifest.integrity.corpus_sha256,
            observed: observed_corpus,
        });
    }
    let registration = CorpusRegistration::new(
        CorpusKey {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
        },
        source,
        &corpus,
    )?;
    Ok(LoadedManifestCorpus {
        manifest,
        registration,
        corpus,
    })
}

/// A manifest or its referenced content failed closed.
#[derive(Debug)]
pub enum ManifestError {
    /// A file could not be read.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Strict JSON decoding or encoding failed.
    Json(serde_json::Error),
    /// The manifest representation is unknown.
    UnsupportedManifestVersion { observed: u32 },
    /// The case representation is unknown.
    UnsupportedCorpusVersion { observed: u32 },
    /// A logical identity component is empty, too long, or contains controls.
    InvalidIdentity(&'static str),
    /// A repository source escaped the manifest directory or used a platform-specific root.
    NonPortablePath(String),
    /// The repository checkout root could not be retained in durable provenance.
    RepositoryRootUnavailable(PathBuf),
    /// A repository source resolved outside its checkout root.
    RepositorySourceOutsideCheckout(PathBuf),
    /// A repository source omitted its exact-byte digest.
    MissingSourceDigest,
    /// A non-file source supplied an inapplicable exact-byte digest.
    UnexpectedSourceDigest,
    /// A blob reference declared an empty source.
    InvalidBlobByteLength,
    /// A blob store binding name was not canonical.
    InvalidBlobStore,
    /// Exact source bytes did not match the manifest.
    SourceDigestMismatch {
        expected: Sha256Digest,
        observed: Sha256Digest,
    },
    /// The corpus file itself failed admission.
    Corpus(crate::CorpusLoadError),
    /// Manifest and corpus format versions disagree.
    CorpusVersionMismatch { manifest: u32, corpus: u32 },
    /// Stable case identities were not unique.
    DuplicateCaseId(String),
    /// Per-case integrity did not match exactly.
    CaseIntegrityMismatch,
    /// A manifest omitted integrity for every case.
    MissingCaseIntegrity,
    /// Aggregate logical integrity did not match.
    CorpusDigestMismatch {
        expected: Sha256Digest,
        observed: Sha256Digest,
    },
    /// Blob reference shape is admitted, but no loader is implemented in this slice.
    BlobBackendUnavailable,
    /// A collection or serialized value exceeded the version-one u64 framing.
    LengthOverflow,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Json(source) => write!(formatter, "JSON is invalid: {source}"),
            Self::UnsupportedManifestVersion { observed } => write!(
                formatter,
                "manifest version {observed} is unsupported; expected {CORPUS_MANIFEST_VERSION}"
            ),
            Self::UnsupportedCorpusVersion { observed } => write!(
                formatter,
                "corpus format version {observed} is unsupported; expected {CORPUS_FORMAT_VERSION}"
            ),
            Self::InvalidIdentity(field) => write!(
                formatter,
                "manifest {field} is empty, too long, or contains control characters"
            ),
            Self::NonPortablePath(path) => write!(
                formatter,
                "repository case path {path:?} is not a portable relative path"
            ),
            Self::RepositoryRootUnavailable(path) => write!(
                formatter,
                "could not determine repository root for manifest {}",
                path.display()
            ),
            Self::RepositorySourceOutsideCheckout(path) => write!(
                formatter,
                "repository case source {} resolves outside the checkout",
                path.display()
            ),
            Self::MissingSourceDigest => {
                formatter.write_str("repository case source requires source_sha256")
            }
            Self::UnexpectedSourceDigest => {
                formatter.write_str("non-repository case source must not carry source_sha256")
            }
            Self::InvalidBlobByteLength => {
                formatter.write_str("blob source byte length must be positive")
            }
            Self::InvalidBlobStore => formatter.write_str("blob store binding name is invalid"),
            Self::SourceDigestMismatch { expected, observed } => write!(
                formatter,
                "case source digest mismatch: expected {expected}, observed {observed}"
            ),
            Self::Corpus(source) => {
                write!(formatter, "case source is not an admitted corpus: {source}")
            }
            Self::CorpusVersionMismatch { manifest, corpus } => write!(
                formatter,
                "manifest corpus format version {manifest} does not match source version {corpus}"
            ),
            Self::DuplicateCaseId(id) => {
                write!(formatter, "case identity {id:?} occurs more than once")
            }
            Self::CaseIntegrityMismatch => {
                formatter.write_str("per-case integrity does not match the manifest")
            }
            Self::MissingCaseIntegrity => {
                formatter.write_str("manifest requires at least one case-integrity entry")
            }
            Self::CorpusDigestMismatch { expected, observed } => write!(
                formatter,
                "logical corpus digest mismatch: expected {expected}, observed {observed}"
            ),
            Self::BlobBackendUnavailable => formatter
                .write_str("blob reference is valid but no blob corpus backend is configured"),
            Self::LengthOverflow => {
                formatter.write_str("corpus content exceeds version-one u64 framing")
            }
        }
    }
}

impl Error for ManifestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Json(source) => Some(source),
            Self::Corpus(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        CORPUS_MANIFEST_VERSION, CorpusManifest, ManifestCaseSource, case_integrity, corpus_digest,
        decode_manifest, load_manifest_corpus, validate_manifest_content,
    };
    use crate::CorpusSourceDescriptor;

    const SEED_MANIFEST: &[u8] = include_bytes!("../corpora/seed-v1.manifest.json");

    fn seed_manifest_path() -> &'static Path {
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/corpora/seed-v1.manifest.json"
        ))
    }

    #[test]
    fn portable_manifest_round_trip_preserves_source_and_integrity() {
        let manifest = decode_manifest(SEED_MANIFEST).expect("the seed manifest is valid");
        let encoded = serde_json::to_vec(&manifest).expect("the seed manifest serializes");
        let decoded = decode_manifest(&encoded).expect("the serialized manifest remains valid");

        assert_eq!(decoded, manifest);
        assert_eq!(decoded.manifest_version, CORPUS_MANIFEST_VERSION);
    }

    #[test]
    fn repository_manifest_resolves_and_verifies_the_seed_corpus() {
        let loaded = load_manifest_corpus(seed_manifest_path())
            .expect("the repository source matches every integrity field");

        assert_eq!(
            loaded.corpus.cases.len(),
            loaded.manifest.integrity.cases.len()
        );
        assert_eq!(loaded.registration.key().name, loaded.manifest.name);
        assert_eq!(loaded.registration.key().version, loaded.manifest.version);
        assert_eq!(
            usize::try_from(loaded.registration.case_count())
                .expect("the fixture case count fits usize"),
            loaded.manifest.integrity.cases.len()
        );
        assert_eq!(
            loaded.registration.source(),
            &manifest_source(&loaded.manifest)
        );
    }

    #[test]
    fn blob_reference_shape_round_trips_without_claiming_a_backend() {
        let mut manifest: CorpusManifest =
            decode_manifest(SEED_MANIFEST).expect("the seed manifest is valid");
        manifest.case_source = ManifestCaseSource::BlobReference {
            store: Some(String::from("evaluation-artifacts")),
            digest: manifest.integrity.corpus_sha256,
            byte_length: 4096,
        };
        manifest.integrity.source_sha256 = None;
        let encoded = serde_json::to_vec(&manifest).expect("the blob manifest serializes");
        let decoded = decode_manifest(&encoded).expect("the blob reference shape is admitted");

        assert_eq!(decoded, manifest);
    }

    #[test]
    fn database_native_manifest_rejects_empty_corpus() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["case_source"] = serde_json::json!({
            "kind": "database_native",
            "cases": []
        });
        manifest["integrity"]["source_sha256"] = serde_json::Value::Null;
        manifest["integrity"]["cases"] = serde_json::json!([]);
        let encoded = serde_json::to_vec(&manifest).expect("the empty manifest serializes");

        let error = decode_manifest(&encoded).expect_err("an empty embedded corpus is rejected");

        assert!(error.to_string().contains("no cases"));
    }

    #[test]
    fn blob_reference_manifest_rejects_repository_source_digest() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        let digest = manifest["integrity"]["corpus_sha256"].clone();
        manifest["case_source"] = serde_json::json!({
            "kind": "blob_reference",
            "store": null,
            "digest": digest,
            "byte_length": 1
        });
        let encoded = serde_json::to_vec(&manifest).expect("the contradictory manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a blob source cannot carry repository byte integrity");

        assert!(error.to_string().contains("must not carry source_sha256"));
    }

    #[test]
    fn direct_manifest_deserialization_runs_admission() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["manifest_version"] = serde_json::json!(CORPUS_MANIFEST_VERSION + 1);
        let encoded = serde_json::to_vec(&manifest).expect("the invalid manifest serializes");

        let error = serde_json::from_slice::<CorpusManifest>(&encoded)
            .expect_err("direct deserialization rejects an unsupported manifest version");

        assert!(error.to_string().contains("manifest version"));
        assert!(error.to_string().contains("unsupported"));
    }

    #[cfg(unix)]
    #[test]
    fn repository_source_symlink_must_remain_inside_checkout() {
        use std::os::unix::fs::symlink;

        let fixture =
            std::env::temp_dir().join(format!("signalbox-corpus-symlink-{}", std::process::id()));
        let checkout = fixture.join("checkout");
        let outside = fixture.join("outside.json");
        fs::create_dir_all(checkout.join(".git")).expect("the synthetic checkout is created");
        fs::write(&outside, b"{}").expect("the external source is created");
        symlink(&outside, checkout.join("cases.json"))
            .expect("the escaping source symlink is created");

        let error = super::resolved_repository_source(
            &checkout.join("corpus.manifest.json"),
            &checkout.join("cases.json"),
        )
        .expect_err("a source resolving outside the checkout is rejected");

        assert!(matches!(
            error,
            super::ManifestError::RepositorySourceOutsideCheckout(_)
        ));
        fs::remove_dir_all(&fixture).expect("the synthetic checkout is removed");
    }

    #[test]
    fn repository_manifest_requires_source_digest_during_decoding() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["integrity"]["source_sha256"] = serde_json::Value::Null;
        let encoded = serde_json::to_vec(&manifest).expect("the digest-less manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a repository source without exact-byte integrity is rejected");

        assert!(error.to_string().contains("requires source_sha256"));
    }

    #[test]
    fn database_native_manifest_rejects_blank_label_provenance_during_decoding() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        let corpus: serde_json::Value =
            serde_json::from_slice(include_bytes!("../corpora/seed-v1.json"))
                .expect("the seed corpus is valid JSON");
        manifest["case_source"] = serde_json::json!({
            "kind": "database_native",
            "cases": corpus["cases"].clone()
        });
        manifest["case_source"]["cases"][0]["label_provenance"] =
            serde_json::Value::String(String::from(" \t "));
        manifest["integrity"]["source_sha256"] = serde_json::Value::Null;
        let encoded = serde_json::to_vec(&manifest)
            .expect("the embedded corpus without provenance serializes");

        let error = decode_manifest(&encoded)
            .expect_err("an embedded corpus with blank label provenance is rejected");

        assert!(error.to_string().contains("has no label provenance"));
    }

    #[test]
    fn database_native_manifest_rejects_duplicate_case_ids_during_decoding() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        let corpus: serde_json::Value =
            serde_json::from_slice(include_bytes!("../corpora/seed-v1.json"))
                .expect("the seed corpus is valid JSON");
        manifest["case_source"] = serde_json::json!({
            "kind": "database_native",
            "cases": corpus["cases"].clone()
        });
        manifest["case_source"]["cases"][1]["id"] =
            manifest["case_source"]["cases"][0]["id"].clone();
        manifest["integrity"]["source_sha256"] = serde_json::Value::Null;
        let encoded =
            serde_json::to_vec(&manifest).expect("the duplicate embedded corpus serializes");

        let error = decode_manifest(&encoded)
            .expect_err("an embedded corpus with duplicate case identities is rejected");

        assert!(error.to_string().contains("appears more than once"));
    }

    #[test]
    fn manifest_source_variants_reject_unknown_fields() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["case_source"]["future_field"] = serde_json::Value::Bool(true);
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error =
            decode_manifest(&encoded).expect_err("an unknown source-variant field is rejected");

        assert!(error.to_string().contains("unknown field `future_field`"));
    }

    #[test]
    fn manifest_integrity_rejects_blank_case_ids_during_decoding() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["integrity"]["cases"][0]["id"] = serde_json::Value::String(String::from("   "));
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a blank integrity case identity is rejected during decoding");

        assert!(error.to_string().contains("case id"));
    }

    #[test]
    fn manifest_rejects_whitespace_only_name_during_decoding() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["name"] = serde_json::Value::String(String::from("   "));
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a whitespace-only manifest name is rejected during decoding");

        assert!(error.to_string().contains("name"));
    }

    #[test]
    fn manifest_rejects_whitespace_only_version_during_decoding() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["version"] = serde_json::Value::String(String::from("   "));
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a whitespace-only manifest version is rejected during decoding");

        assert!(error.to_string().contains("version"));
    }

    #[test]
    fn manifest_integrity_rejects_duplicate_case_ids_during_decoding() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["integrity"]["cases"][1]["id"] = manifest["integrity"]["cases"][0]["id"].clone();
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("duplicate integrity case identities are rejected during decoding");

        assert!(error.to_string().contains("occurs more than once"));
    }

    #[test]
    fn repository_manifest_rejects_windows_drive_prefix_on_every_host() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["case_source"]["path"] = serde_json::Value::String(String::from("C:/cases.json"));
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error =
            decode_manifest(&encoded).expect_err("a Windows drive-prefixed path is never portable");

        assert!(error.to_string().contains("not a portable relative path"));
    }

    #[test]
    fn repository_manifest_rejects_windows_invalid_component_characters() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["case_source"]["path"] =
            serde_json::Value::String(String::from("corpora/cases:data.json"));
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a platform-specific component character is rejected");

        assert!(error.to_string().contains("not a portable relative path"));
    }

    #[test]
    fn repository_manifest_rejects_windows_reserved_device_names() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["case_source"]["path"] =
            serde_json::Value::String(String::from("corpora/CON.json"));
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error =
            decode_manifest(&encoded).expect_err("a Windows reserved device component is rejected");

        assert!(error.to_string().contains("not a portable relative path"));
    }

    #[test]
    fn repository_manifest_rejects_superscript_windows_device_names() {
        assert!(!super::portable_path_component("COM¹.json"));
        assert!(!super::portable_path_component("COM²"));
        assert!(!super::portable_path_component("COM³.txt"));
        assert!(!super::portable_path_component("LPT¹"));
        assert!(!super::portable_path_component("LPT².json"));
        assert!(!super::portable_path_component("LPT³"));
    }

    #[test]
    fn repository_manifest_requires_case_integrity_during_decoding() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["integrity"]["cases"] = serde_json::json!([]);
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a repository manifest without case integrity is rejected");

        assert!(
            error
                .to_string()
                .contains("at least one case-integrity entry")
        );
    }

    #[test]
    fn relative_manifest_path_resolves_before_checkout_discovery() {
        let expected = decode_manifest(SEED_MANIFEST).expect("the seed manifest passes admission");
        let current = std::env::current_dir().expect("the test working directory is available");
        let relative = seed_manifest_path()
            .strip_prefix(&current)
            .expect("the seed manifest is beneath the test working directory");

        let loaded = load_manifest_corpus(relative)
            .expect("a relative manifest path discovers the containing checkout");

        assert_eq!(loaded.manifest.name(), expected.name());
    }

    #[test]
    fn repository_manifest_rejects_windows_trailing_component_dots() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["case_source"]["path"] =
            serde_json::Value::String(String::from("corpora/cases./data.json"));
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a component with a platform-specific trailing dot is rejected");

        assert!(error.to_string().contains("not a portable relative path"));
    }

    #[test]
    fn repository_manifest_rejects_oversized_utf8_path_components() {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(SEED_MANIFEST).expect("the seed manifest is valid JSON");
        manifest["case_source"]["path"] =
            serde_json::Value::String(format!("corpora/{}.json", "é".repeat(128)));
        let encoded = serde_json::to_vec(&manifest).expect("the modified manifest serializes");

        let error = decode_manifest(&encoded)
            .expect_err("a component exceeding the portable byte ceiling is rejected");

        assert!(error.to_string().contains("not a portable relative path"));
    }

    #[test]
    fn repository_manifest_rejects_control_characters_in_path_components() {
        assert!(!super::portable_path_component("cases\n.json"));
        assert!(!super::portable_path_component("cases\0.json"));
    }

    #[test]
    fn manifest_case_integrity_is_compared_by_identity() {
        let mut manifest = decode_manifest(SEED_MANIFEST).expect("the seed manifest is valid");
        manifest.integrity.cases.reverse();
        let corpus = crate::decode_corpus(include_bytes!("../corpora/seed-v1.json"))
            .expect("the seed corpus is valid");
        let source = manifest_source(&manifest);

        validate_manifest_content(manifest, corpus, source)
            .expect("per-case integrity ordering is not part of manifest validity");
    }

    #[test]
    fn corpus_digest_rejects_unsupported_corpus_versions() {
        let mut corpus = crate::decode_corpus(include_bytes!("../corpora/seed-v1.json"))
            .expect("the seed corpus is valid");
        corpus.format_version = crate::CORPUS_FORMAT_VERSION + 1;

        let error = corpus_digest(&corpus)
            .expect_err("the version-one digest helper rejects unsupported content");

        assert!(error.to_string().contains("unsupported"));
    }

    #[test]
    fn case_integrity_rejects_unsupported_corpus_versions() {
        let mut corpus = crate::decode_corpus(include_bytes!("../corpora/seed-v1.json"))
            .expect("the seed corpus is valid");
        corpus.format_version = crate::CORPUS_FORMAT_VERSION + 1;

        let error = case_integrity(&corpus)
            .expect_err("the version-one case helper rejects unsupported content");

        assert!(error.to_string().contains("unsupported"));
    }

    fn manifest_source(manifest: &CorpusManifest) -> CorpusSourceDescriptor {
        match &manifest.case_source {
            ManifestCaseSource::Repository { repository, path } => {
                CorpusSourceDescriptor::Repository {
                    repository: repository.clone(),
                    path: format!("crates/approval-judge-eval/corpora/{path}"),
                }
            }
            ManifestCaseSource::DatabaseNative { .. } => CorpusSourceDescriptor::DatabaseNative,
            ManifestCaseSource::BlobReference {
                store,
                digest,
                byte_length,
            } => CorpusSourceDescriptor::BlobReference {
                store: store.clone(),
                digest: *digest,
                byte_length: *byte_length,
            },
        }
    }
}

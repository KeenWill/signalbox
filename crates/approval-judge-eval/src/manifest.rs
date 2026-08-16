use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ApprovalJudgeCase, ApprovalJudgeCorpus, CORPUS_FORMAT_VERSION,
    store::{CorpusKey, CorpusRegistration, CorpusSourceDescriptor, Sha256Digest},
};

/// The portable manifest representation accepted by this slice.
pub const CORPUS_MANIFEST_VERSION: u32 = 1;
const CORPUS_DIGEST_DOMAIN: &[u8] = b"signalbox-eval-corpus-v1\0";

/// A self-describing, portable corpus registration document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusManifest {
    /// Manifest representation version.
    pub manifest_version: u32,
    /// Stable suite name.
    pub name: String,
    /// Corpus release chosen by its author.
    pub version: String,
    /// Version of the referenced case representation.
    pub corpus_format_version: u32,
    /// Location and origin of the case content.
    pub case_source: ManifestCaseSource,
    /// Digests that bind the manifest to its logical and serialized content.
    pub integrity: CorpusIntegrity,
}

/// Case content forms a portable manifest can name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    let manifest: CorpusManifest = serde_json::from_slice(bytes).map_err(ManifestError::Json)?;
    validate_manifest_header(&manifest)?;
    Ok(manifest)
}

/// Reads a manifest and resolves its repository or embedded case source.
pub fn load_manifest_corpus(path: impl AsRef<Path>) -> Result<LoadedManifestCorpus, ManifestError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| ManifestError::Read {
        path: path.to_path_buf(),
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
            let source_bytes = fs::read(&source_path).map_err(|source| ManifestError::Read {
                path: source_path,
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
            (
                corpus,
                CorpusSourceDescriptor::Repository {
                    repository: repository.clone(),
                    path: relative.clone(),
                },
            )
        }
        ManifestCaseSource::DatabaseNative { cases } => {
            if manifest.integrity.source_sha256.is_some() {
                return Err(ManifestError::UnexpectedSourceDigest);
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
            return Err(ManifestError::BlobBackendUnavailable);
        }
    };
    validate_manifest_content(manifest, corpus, source)
}

/// Computes the version-one logical corpus digest owned by the evaluation spec.
pub fn corpus_digest(corpus: &ApprovalJudgeCorpus) -> Result<Sha256Digest, ManifestError> {
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
    Ok(())
}

fn validate_identity_component(field: &'static str, value: &str) -> Result<(), ManifestError> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(ManifestError::InvalidIdentity(field));
    }
    Ok(())
}

fn portable_relative_path(value: &str) -> Result<PathBuf, ManifestError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ManifestError::NonPortablePath(value.to_owned()));
    }
    Ok(path.to_path_buf())
}

fn validate_unique_case_ids(cases: &[ApprovalJudgeCase]) -> Result<(), ManifestError> {
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
    if observed_cases != manifest.integrity.cases {
        return Err(ManifestError::CaseIntegrityMismatch);
    }
    let observed_corpus = corpus_digest(&corpus)?;
    if observed_corpus != manifest.integrity.corpus_sha256 {
        return Err(ManifestError::CorpusDigestMismatch {
            expected: manifest.integrity.corpus_sha256,
            observed: observed_corpus,
        });
    }
    let case_count =
        u64::try_from(corpus.cases.len()).map_err(|_| ManifestError::LengthOverflow)?;
    let registration = CorpusRegistration {
        key: CorpusKey {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
        },
        format_version: corpus.format_version,
        corpus_sha256: observed_corpus,
        case_count,
        source,
    };
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
    /// A repository source omitted its exact-byte digest.
    MissingSourceDigest,
    /// A non-file source supplied an inapplicable exact-byte digest.
    UnexpectedSourceDigest,
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
            Self::MissingSourceDigest => {
                formatter.write_str("repository case source requires source_sha256")
            }
            Self::UnexpectedSourceDigest => {
                formatter.write_str("database-native case source must not carry source_sha256")
            }
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
    use std::{path::Path, str::FromStr};

    use super::{
        CORPUS_MANIFEST_VERSION, CorpusManifest, ManifestCaseSource, decode_manifest,
        load_manifest_corpus,
    };
    use crate::{CorpusSourceDescriptor, Sha256Digest};

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
        assert_eq!(decoded.name, "approval-judge-seed");
        assert_eq!(decoded.version, "1");
        assert_eq!(
            decoded.integrity.corpus_sha256,
            Sha256Digest::from_str(
                "ed0b10acde362cc4103570f58184acbb6bc4932cc03b6a7123074bfa52b8f539"
            )
            .expect("the fixture digest is valid")
        );
    }

    #[test]
    fn repository_manifest_resolves_and_verifies_the_seed_corpus() {
        let loaded = load_manifest_corpus(seed_manifest_path())
            .expect("the repository source matches every integrity field");

        assert_eq!(loaded.corpus.cases.len(), 3);
        assert_eq!(loaded.registration.key.name, loaded.manifest.name);
        assert_eq!(loaded.registration.key.version, loaded.manifest.version);
        assert_eq!(loaded.registration.case_count, 3);
        assert_eq!(
            loaded.registration.source,
            CorpusSourceDescriptor::Repository {
                repository: String::from("https://github.com/KeenWill/signalbox"),
                path: String::from("seed-v1.json"),
            }
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
}

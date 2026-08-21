//! Catalog-backed, bounded, text-only blob-read tools.

use std::{error::Error, fmt, num::NonZeroU64, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence, ToolPreauthorization,
};
use signalbox_domain::{
    BlobDigest, NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail,
    ToolPermissionDefault,
};
use signalbox_persistence::blob::BlobCatalogRepository;
use signalbox_tool_contract::{ToolContract, compile_contract_definition};
use signalbox_tool_schema_derive::ToolSchema;
use tokio::sync::Semaphore;

use crate::{
    blob_read_runtime::{BlobReadError, read_blob_chunk, read_blob_metadata},
    blob_storage_runtime::BlobStoreRegistry,
};

pub(crate) const BLOB_METADATA_NAME: &str = "blob_metadata";
pub(crate) const BLOB_READ_NAME: &str = "blob_read";
pub(crate) const BLOB_TOOL_NAMES: &[&str] = &[BLOB_METADATA_NAME, BLOB_READ_NAME];
const MAX_READ_BYTES: u64 = 524_288;
const INVALID_ARGUMENTS: &str = "expected exact canonical blob-read arguments";

#[derive(Debug, Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
struct MetadataArguments {
    #[tool_schema(description = "Canonical sha256 blob digest.")]
    digest: String,
}

#[derive(Debug, Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    #[tool_schema(description = "Canonical sha256 blob digest.")]
    digest: String,
    #[tool_schema(description = "Canonical decimal byte offset.")]
    offset_bytes: String,
    #[tool_schema(description = "Canonical decimal length from 1 through 524288.")]
    length_bytes: String,
}

struct MetadataContract;
impl ToolContract for MetadataContract {
    type Arguments = MetadataArguments;
    const NAME: &'static str = BLOB_METADATA_NAME;
    const DESCRIPTION: &'static str = "Returns bounded catalog metadata for an attached blob.";
}

struct ReadContract;
impl ToolContract for ReadContract {
    type Arguments = ReadArguments;
    const NAME: &'static str = BLOB_READ_NAME;
    const DESCRIPTION: &'static str =
        "Reads one authorized attached-blob byte range as canonical padded base64.";
}

#[derive(Clone, Debug)]
struct BlobValidator {
    read: bool,
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for BlobValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode(arguments, self.read)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }

    fn preauthorization(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<ToolPreauthorization, ToolExecutionErrorDetail> {
        match decode(arguments, self.read).map_err(|_| self.detail.clone())? {
            DecodedArguments::Metadata(digest) => Ok(ToolPreauthorization::BlobMetadata { digest }),
            DecodedArguments::Read { digest, length, .. } => Ok(ToolPreauthorization::BlobRead {
                digest,
                decoded_bytes: length,
            }),
        }
    }
}

#[derive(Clone)]
/// Compiled generic blob-read declarations and their catalog-backed executor.
pub struct BlobTools {
    catalog: CompiledToolCatalog,
    executor: BlobToolExecutor,
}

impl BlobTools {
    /// Constructs the family from the durable catalog and configured stores.
    pub fn try_new(
        repository: BlobCatalogRepository,
        registry: Option<Arc<BlobStoreRegistry>>,
    ) -> Result<Self, BlobToolConstructionError> {
        let detail = ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS))
            .map_err(|_| BlobToolConstructionError)?;
        let metadata = compile_contract_definition::<MetadataContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        )
        .map_err(|_| BlobToolConstructionError)?;
        let read = compile_contract_definition::<ReadContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        )
        .map_err(|_| BlobToolConstructionError)?;
        let catalog = CompiledToolCatalog::try_new([
            CompiledTool::new(
                metadata,
                BlobValidator {
                    read: false,
                    detail: detail.clone(),
                },
            ),
            CompiledTool::new(read, BlobValidator { read: true, detail }),
        ])
        .map_err(|_| BlobToolConstructionError)?;
        let read_budget = registry.as_ref().map_or_else(
            || Arc::new(Semaphore::new(0)),
            |registry| registry.read_budget(),
        );
        Ok(Self {
            catalog,
            executor: BlobToolExecutor {
                repository,
                registry,
                read_budget,
            },
        })
    }

    /// Separates immutable declarations from executor dispatch.
    pub fn into_parts(self) -> (CompiledToolCatalog, BlobToolExecutor) {
        (self.catalog, self.executor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A static blob-read declaration could not be compiled.
pub struct BlobToolConstructionError;

impl fmt::Display for BlobToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("blob-read tool construction failed")
    }
}
impl Error for BlobToolConstructionError {}

#[derive(Clone)]
/// Daemon executor for bounded catalog-backed blob reads.
pub struct BlobToolExecutor {
    repository: BlobCatalogRepository,
    registry: Option<Arc<BlobStoreRegistry>>,
    read_budget: Arc<Semaphore>,
}

impl fmt::Debug for BlobToolExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BlobToolExecutor { .. }")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Sanitized catalog or store failure from a blob-read executor.
pub struct BlobToolExecutorError;
impl fmt::Display for BlobToolExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("blob-read tool failed")
    }
}
impl Error for BlobToolExecutorError {}
impl ClassifyOperatorFailure for BlobToolExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        }
    }
}

impl ToolExecutor for BlobToolExecutor {
    type Error = BlobToolExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let read = match invocation.request().name().as_str() {
            BLOB_METADATA_NAME => false,
            BLOB_READ_NAME => true,
            _ => return Err(BlobToolExecutorError),
        };
        let decoded =
            decode(invocation.request().arguments(), read).map_err(|_| BlobToolExecutorError)?;
        let evidence = match decoded {
            DecodedArguments::Metadata(digest) => {
                match read_blob_metadata(&self.repository, digest).await {
                    Ok(metadata) => completed(&MetadataResult {
                        digest: digest.to_string(),
                        byte_length: metadata.byte_length.to_string(),
                        replica_count: metadata.replica_count.to_string(),
                    })?,
                    Err(error) => failed(error)?,
                }
            }
            DecodedArguments::Read {
                digest,
                offset,
                length,
            } => {
                let _permit = Arc::clone(&self.read_budget)
                    .try_acquire_owned()
                    .map_err(|_| BlobToolExecutorError)?;
                match read_blob_chunk(
                    self.registry.as_deref(),
                    &self.repository,
                    digest,
                    offset,
                    length,
                )
                .await
                {
                    Ok(bytes) => completed(&ReadResult {
                        digest: digest.to_string(),
                        offset_bytes: offset.to_string(),
                        bytes_base64: STANDARD.encode(bytes),
                    })?,
                    Err(error) => failed(error)?,
                }
            }
        };
        Ok(invocation.bind(evidence))
    }
}

enum DecodedArguments {
    Metadata(BlobDigest),
    Read {
        digest: BlobDigest,
        offset: u64,
        length: NonZeroU64,
    },
}

fn decode(
    arguments: &NormalizedToolArguments,
    read: bool,
) -> Result<DecodedArguments, BlobToolExecutorError> {
    if read {
        let arguments: ReadArguments =
            serde_json::from_str(arguments.as_str()).map_err(|_| BlobToolExecutorError)?;
        let digest = arguments
            .digest
            .parse()
            .map_err(|_| BlobToolExecutorError)?;
        let offset = canonical_u64(&arguments.offset_bytes).ok_or(BlobToolExecutorError)?;
        let length = canonical_u64(&arguments.length_bytes)
            .filter(|length| (1..=MAX_READ_BYTES).contains(length))
            .and_then(NonZeroU64::new)
            .ok_or(BlobToolExecutorError)?;
        Ok(DecodedArguments::Read {
            digest,
            offset,
            length,
        })
    } else {
        let arguments: MetadataArguments =
            serde_json::from_str(arguments.as_str()).map_err(|_| BlobToolExecutorError)?;
        Ok(DecodedArguments::Metadata(
            arguments
                .digest
                .parse()
                .map_err(|_| BlobToolExecutorError)?,
        ))
    }
}

fn canonical_u64(value: &str) -> Option<u64> {
    let parsed = value.parse::<u64>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

#[derive(Serialize)]
struct MetadataResult {
    digest: String,
    byte_length: String,
    replica_count: String,
}

#[derive(Serialize)]
struct ReadResult {
    digest: String,
    offset_bytes: String,
    bytes_base64: String,
}

fn completed(value: &impl Serialize) -> Result<ToolExecutorEvidence, BlobToolExecutorError> {
    serde_json::to_string(value)
        .map(ToolExecutorEvidence::CompletedText)
        .map_err(|_| BlobToolExecutorError)
}

fn failed(error: BlobReadError) -> Result<ToolExecutorEvidence, BlobToolExecutorError> {
    let detail = match error {
        BlobReadError::NotFound => "blob_not_found",
        BlobReadError::RangeOutOfBounds { .. } => "range_out_of_bounds",
        BlobReadError::Missing => "blob_missing",
        BlobReadError::Corrupt => "blob_corrupt",
        BlobReadError::Unavailable => return Err(BlobToolExecutorError),
        BlobReadError::Integrity => "blob_integrity_failure",
    };
    Ok(ToolExecutorEvidence::KnownFailed {
        detail: ToolExecutionErrorDetail::try_new(String::from(detail)).ok(),
    })
}

#[cfg(test)]
mod tests {
    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
    use signalbox_domain::ToolResultText;

    use super::*;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(String::from(value))
            .expect("the fixture is canonical JSON")
    }

    #[test]
    fn blob_read_validator_derives_the_exact_bounded_charge() {
        let digest = BlobDigest::digest(b"attached bytes");
        let validator = BlobValidator {
            read: true,
            detail: ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS))
                .expect("the static detail is valid"),
        };
        let input = arguments(&format!(
            r#"{{"digest":"{digest}","offset_bytes":"0","length_bytes":"524288"}}"#
        ));

        assert_eq!(
            validator
                .preauthorization(&input)
                .expect("the maximum request is admitted"),
            ToolPreauthorization::BlobRead {
                digest,
                decoded_bytes: NonZeroU64::new(MAX_READ_BYTES).expect("the maximum is positive"),
            }
        );
    }

    #[test]
    fn blob_read_validator_rejects_noncanonical_and_oversize_lengths() {
        let digest = BlobDigest::digest(b"attached bytes");
        let validator = BlobValidator {
            read: true,
            detail: ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS))
                .expect("the static detail is valid"),
        };
        let leading_zero = arguments(&format!(
            r#"{{"digest":"{digest}","offset_bytes":"0","length_bytes":"01"}}"#
        ));
        let oversize = arguments(&format!(
            r#"{{"digest":"{digest}","offset_bytes":"0","length_bytes":"524289"}}"#
        ));

        assert!(validator.validate(&leading_zero).is_err());
        assert!(validator.validate(&oversize).is_err());
    }

    #[test]
    fn blob_metadata_validator_derives_visibility_admission_without_a_byte_charge() {
        let digest = BlobDigest::digest(b"attached bytes");
        let validator = BlobValidator {
            read: false,
            detail: ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS))
                .expect("the static detail is valid"),
        };
        let input = arguments(&format!(r#"{{"digest":"{digest}"}}"#));

        assert_eq!(
            validator
                .preauthorization(&input)
                .expect("canonical metadata arguments are admitted"),
            ToolPreauthorization::BlobMetadata { digest }
        );
    }

    #[tokio::test]
    async fn blob_tool_catalog_exposes_two_exact_auto_effect_free_declarations() {
        let tools = BlobTools::try_new(
            BlobCatalogRepository::new(
                sqlx::PgPool::connect_lazy("postgres://localhost/test")
                    .expect("the fixture URL parses"),
            ),
            None,
        )
        .expect("the static declarations compile");
        let definitions = tools.catalog.definitions();
        let [metadata, read] = definitions.as_ref() else {
            panic!("the blob family has exactly two tools")
        };

        assert_eq!(metadata.name().as_str(), BLOB_METADATA_NAME);
        assert_eq!(read.name().as_str(), BLOB_READ_NAME);
        assert_eq!(metadata.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(read.effect_class(), ToolEffectClass::EffectFree);
        assert!(matches!(
            tools
                .catalog
                .validate_arguments(read.name(), &arguments("{}")),
            Err(ToolCatalogValidationFailure::InvalidArguments { .. })
        ));
    }

    #[test]
    fn maximum_blob_read_result_fits_the_existing_text_result_cap() {
        let digest = BlobDigest::digest(b"attached bytes");
        let ToolExecutorEvidence::CompletedText(result) = completed(&ReadResult {
            digest: digest.to_string(),
            offset_bytes: String::from("0"),
            bytes_base64: STANDARD.encode(vec![0_u8; MAX_READ_BYTES as usize]),
        })
        .expect("the bounded result serializes") else {
            panic!("the result helper emits completed text")
        };

        assert!(ToolResultText::try_new(result).is_ok());
    }
}

//! Typed file tools composed with rendered-frontier authority and isolated parsers.

use std::{num::NonZeroU64, sync::Arc};

use signalbox_application::{
    CompiledToolCatalog, CorrelatedToolExecutorEvidence, OperatorFailureClass,
    RenderedAttachmentSelector, ToolExecutionInvocation, ToolExecutor,
};
use signalbox_domain::{BlobDigest, ToolRequest, UserContentPart};
use signalbox_file_media_processor_runtime::{SandboxedFileMediaProcessor, WorkerBinding};
use signalbox_file_media_provider_runtime::{
    ContinuationAuthority, FileUseResolutionError, FileUseResolver, FileUseResolverFuture,
    RegistryFileMediaAgentService, ResolvedFileUse,
};
use signalbox_file_media_runtime::{
    AttachmentKind, DeclaredMediaType, DisplayFilename, FileDigest, FileMediaCeilings,
    FileMediaProcessCeilings, FileMediaRegistry, FileUse, NeverCancelled, ProcessorIsolation,
    SourceReadError, SourceReadFuture, VerifiedBlobSource, VisiblePartSelector,
};
use signalbox_persistence::{
    blob::{BlobCatalogEntry, BlobCatalogRepository, BlobCatalogRepositoryError},
    tool_loop::PostgresToolLoopRepository,
};
use signalbox_tools_file_media::{FileInspectServiceRequest, FileMediaTools};
use sqlx::PgPool;

use super::DaemonToolExecutorError;
use crate::{
    BlobStoreRegistry,
    blob_read_runtime::{BlobReadError, read_blob_chunk},
};

/// Store-backed file tool composition; every parser operation goes through the worker port.
#[derive(Clone, Debug)]
pub struct DaemonFileMediaExecutor {
    pool: PgPool,
    stores: Arc<BlobStoreRegistry>,
    registry: FileMediaRegistry,
    processor: SandboxedFileMediaProcessor,
    continuations: ContinuationAuthority,
    models: Option<signalbox_model_provider_runtime::RuntimeModelCatalog>,
    capabilities: signalbox_model_runtime::ModelCapabilityCatalog,
}

impl DaemonFileMediaExecutor {
    /// Uses the issuing call's durable serving target to enforce image presentation bounds.
    pub fn with_model_configuration(
        mut self,
        configuration: &crate::HubModelConfiguration,
    ) -> Self {
        self.models = Some(configuration.runtime_model_catalog());
        self.capabilities = configuration.runtime_model_capability_catalog();
        self
    }

    /// Composes the compiled families after proving each worker isolation profile.
    pub async fn compose(
        pool: PgPool,
        stores: Arc<BlobStoreRegistry>,
    ) -> Result<(CompiledToolCatalog, Self), DaemonToolExecutorError> {
        let executable =
            std::env::current_exe().map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
        let worker = executable
            .parent()
            .ok_or_else(DaemonToolExecutorError::pre_dispatch)?
            .join("signalbox-file-media-text-worker");
        let image_worker = worker.with_file_name("signalbox-file-media-image-worker");
        Self::compose_with_workers(pool, stores, worker, image_worker).await
    }

    async fn compose_with_workers(
        pool: PgPool,
        stores: Arc<BlobStoreRegistry>,
        worker: std::path::PathBuf,
        image_worker: std::path::PathBuf,
    ) -> Result<(CompiledToolCatalog, Self), DaemonToolExecutorError> {
        let declaration = signalbox_file_media_adapters_text::text_family_declaration()
            .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
        let binding = WorkerBinding::try_new(worker, declaration.clone())
            .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
        let mut declarations = vec![declaration];
        let mut bindings = vec![binding];
        {
            let worker = image_worker;
            let declaration = signalbox_file_media_adapters_image::image_family_declaration()
                .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
            bindings.push(
                WorkerBinding::try_new(worker, declaration.clone())
                    .map_err(|_| DaemonToolExecutorError::pre_dispatch())?,
            );
            declarations.push(declaration);
        }
        let processor = SandboxedFileMediaProcessor::try_new(
            "/usr/bin/bwrap",
            bindings,
            FileMediaProcessCeilings::version_one(),
        )
        .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
        let isolation = processor.verify_isolation().await;
        if isolation != ProcessorIsolation::Available {
            return Err(DaemonToolExecutorError::pre_dispatch());
        }
        let registry =
            FileMediaRegistry::try_new(declarations, FileMediaCeilings::version_one(), isolation)
                .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
        let executor = Self {
            pool,
            stores,
            models: None,
            capabilities: signalbox_model_runtime::ModelCapabilityCatalog::empty(),
            registry,
            processor,
            continuations: ContinuationAuthority::generate()
                .map_err(|_| DaemonToolExecutorError::pre_dispatch())?,
        };
        let (catalog, _) = FileMediaTools::try_new(executor.service(None))
            .map_err(|_| DaemonToolExecutorError::pre_dispatch())?
            .into_parts();
        Ok((catalog, executor))
    }

    fn service(
        &self,
        request: Option<ToolRequest>,
    ) -> RegistryFileMediaAgentService<
        DaemonFileUseResolver,
        SandboxedFileMediaProcessor,
        NeverCancelled,
    > {
        RegistryFileMediaAgentService::new(
            self.registry.clone(),
            DaemonFileUseResolver {
                request,
                visibility: PostgresToolLoopRepository::new(self.pool.clone()),
                catalog: BlobCatalogRepository::new(self.pool.clone()),
                stores: Arc::clone(&self.stores),
                models: self.models.clone(),
                capabilities: self.capabilities.clone(),
            },
            self.processor.clone(),
            NeverCancelled,
            self.continuations.clone(),
        )
        .with_artifact_publisher(Arc::new(DaemonArtifactPublisher {
            stores: self.stores.clone(),
            catalog: BlobCatalogRepository::new(self.pool.clone()),
        }))
    }
}

impl ToolExecutor for DaemonFileMediaExecutor {
    type Error = DaemonToolExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let service = self.service(Some(invocation.request().clone()));
        let (_, mut executor) = FileMediaTools::try_new(service)
            .map_err(|_| DaemonToolExecutorError::pre_dispatch())?
            .into_parts();
        let Ok(permit) = self.stores.read_budget().try_acquire_owned() else {
            let detail = signalbox_domain::ToolExecutionErrorDetail::try_new(String::from(
                r#"{"status":"blob_unavailable"}"#,
            ))
            .map_err(|_| DaemonToolExecutorError::pre_dispatch())?;
            return Ok(
                invocation.bind(signalbox_application::ToolExecutorEvidence::KnownFailed {
                    detail: Some(detail),
                }),
            );
        };
        let outcome =
            signalbox_application::with_released_scheduler_admission(executor.execute(invocation))
                .await;
        drop(permit);
        outcome.map_err(|error| DaemonToolExecutorError::from_error(&error))
    }
}

struct DaemonFileUseResolver {
    request: Option<ToolRequest>,
    visibility: PostgresToolLoopRepository,
    catalog: BlobCatalogRepository,
    stores: Arc<BlobStoreRegistry>,
    models: Option<signalbox_model_provider_runtime::RuntimeModelCatalog>,
    capabilities: signalbox_model_runtime::ModelCapabilityCatalog,
}

#[derive(Debug)]
struct DaemonArtifactPublisher {
    stores: Arc<BlobStoreRegistry>,
    catalog: BlobCatalogRepository,
}

impl signalbox_file_media_provider_runtime::FileMediaArtifactPublisher for DaemonArtifactPublisher {
    fn publish<'a>(
        &'a self,
        artifact: &'a signalbox_file_media_runtime::ValidatedMediaArtifact,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), signalbox_tools_file_media::FileMediaServiceFailure>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(crate::blob_upload_runtime::publish_generated_artifact(
            &self.stores,
            &self.catalog,
            artifact,
        ))
    }
}

impl FileUseResolver for DaemonFileUseResolver {
    type Source = CatalogFileSource;

    fn resolve(
        &mut self,
        request: FileInspectServiceRequest,
    ) -> FileUseResolverFuture<'_, Self::Source> {
        Box::pin(async move {
            let invalid = || operator_resolution_error(OperatorFailureClass::FailClosedCorruption);
            let selector = request
                .visible_part()
                .map(|value| {
                    RenderedAttachmentSelector::parse(value.as_str())
                        .ok_or(FileUseResolutionError::BlobNotVisible)
                })
                .transpose()?;
            let digest = BlobDigest::from_bytes(*request.digest().as_bytes());
            let visible = self
                .visibility
                .resolve_visible_attachment(
                    self.request.as_ref().ok_or_else(invalid)?,
                    digest,
                    selector,
                )
                .await
                .map_err(|error| {
                    FileUseResolutionError::Operator(
                        signalbox_tools_file_media::FileMediaExecutorError::from_error(&error),
                    )
                })?
                .ok_or(FileUseResolutionError::BlobNotVisible)?;
            let entry = self
                .catalog
                .find(digest)
                .await
                .map_err(catalog_resolution_error)?
                .ok_or(FileUseResolutionError::BlobMissing)?;
            let image_target = if let Some(models) = &self.models {
                let target = self
                    .visibility
                    .file_use_target(self.request.as_ref().ok_or_else(invalid)?)
                    .await
                    .map_err(|error| {
                        FileUseResolutionError::Operator(
                            signalbox_tools_file_media::FileMediaExecutorError::from_error(&error),
                        )
                    })?;
                let model = models.resolve(target).ok_or_else(invalid)?;
                self.capabilities
                    .resolve(&signalbox_model_runtime::ResolvedTarget::new(
                        model.provider_model().to_owned(),
                    ))
                    .ok_or_else(invalid)?
                    .image_presentation()
                    .cloned()
            } else {
                None
            };
            let length = NonZeroU64::new(entry.expected().byte_length()).ok_or_else(invalid)?;
            let UserContentPart::Attachment {
                kind,
                media_type,
                display_filename,
                ..
            } = visible.part
            else {
                return Err(invalid());
            };
            let file_use = FileUse::new(
                request.digest(),
                length,
                match kind {
                    signalbox_domain::AttachmentKind::Image => AttachmentKind::Image,
                    signalbox_domain::AttachmentKind::Document => AttachmentKind::Document,
                    signalbox_domain::AttachmentKind::File => AttachmentKind::File,
                },
                DeclaredMediaType::try_new(media_type.as_str()).map_err(|_| invalid())?,
                display_filename
                    .map(|name| DisplayFilename::try_new(name.as_str()))
                    .transpose()
                    .map_err(|_| invalid())?,
            );
            let selector = VisiblePartSelector::try_new(visible.selector.to_string())
                .map_err(|_| invalid())?;
            Ok(ResolvedFileUse::new(
                file_use,
                CatalogFileSource {
                    entry,
                    length,
                    stores: Arc::clone(&self.stores),
                },
                selector,
            )
            .with_image_target(image_target))
        })
    }
}

struct CatalogFileSource {
    entry: BlobCatalogEntry,
    length: NonZeroU64,
    stores: Arc<BlobStoreRegistry>,
}

impl VerifiedBlobSource for CatalogFileSource {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes(*self.entry.expected().digest().as_bytes())
    }
    fn byte_length(&self) -> NonZeroU64 {
        self.length
    }
    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
        Box::pin(async move {
            if length.get() > signalbox_blob_store::MAX_BLOB_RANGE_BYTES
                || offset
                    .checked_add(length.get())
                    .is_none_or(|end| end > self.length.get())
            {
                return Err(SourceReadError::RangeOutOfBounds);
            }
            read_blob_chunk(&self.stores, &self.entry, offset, length)
                .await
                .map_err(|error| match error {
                    BlobReadError::NotFound | BlobReadError::Missing => SourceReadError::Missing,
                    BlobReadError::Corrupt => SourceReadError::Corrupt,
                    BlobReadError::Unavailable => SourceReadError::Unavailable,
                    BlobReadError::RangeOutOfBounds => SourceReadError::RangeOutOfBounds,
                    BlobReadError::Integrity => SourceReadError::Integrity,
                })
        })
    }
}

fn operator_resolution_error(class: OperatorFailureClass) -> FileUseResolutionError {
    FileUseResolutionError::Operator(
        signalbox_tools_file_media::FileMediaExecutorError::from_class(class),
    )
}

fn catalog_resolution_error(error: BlobCatalogRepositoryError) -> FileUseResolutionError {
    match error {
        BlobCatalogRepositoryError::Database(_) => {
            operator_resolution_error(OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            })
        }
        BlobCatalogRepositoryError::CommitAmbiguous(_) => {
            operator_resolution_error(OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            })
        }
        BlobCatalogRepositoryError::Corruption(_) => {
            operator_resolution_error(OperatorFailureClass::FailClosedCorruption)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use signalbox_application::ToolCatalog;

    async fn configured_blob_stores(
        pool: &PgPool,
    ) -> Result<(tempfile::TempDir, Arc<BlobStoreRegistry>), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let staging = root.path().join("staging");
        let store = root.path().join("store");
        std::fs::create_dir(&staging)?;
        std::fs::create_dir(&store)?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700))?;
        let configuration = format!(
            r#"
[blob_storage]
version = 1
staging_directory = {:?}
max_blob_bytes = 21474836480
[[blob_storage.stores]]
name = "fixture"
namespace_id = "00000000-0000-0000-0000-000000133001"
kind = "filesystem"
root_directory = {:?}
[blob_storage.routes]
user_attachment = "fixture"
tool_artifact = "fixture"
imported_source = "fixture"
generated_artifact = "fixture"
"#,
            staging, store
        );
        let document: toml_edit::DocumentMut = configuration.parse()?;
        let configuration =
            crate::BlobStorageConfiguration::parse(document.get("blob_storage"), 1)?
                .ok_or("fixture blob configuration")?;
        let stores = Arc::new(
            BlobStoreRegistry::initialize(Some(&configuration), pool.clone())
                .await?
                .ok_or("configured fixture stores")?,
        );
        Ok((root, stores))
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn catalog_blob_source_rejects_nonexact_ranges_before_store_io()
    -> Result<(), Box<dyn std::error::Error>> {
        use sha2::{Digest as _, Sha256};
        use signalbox_blob_store::ExpectedBlob;
        use signalbox_persistence::blob::{BlobReplicaRecord, BlobStoreBindingRecord};

        let (_database, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
        let (root, stores) = configured_blob_stores(&pool).await?;
        let bytes = b"exact range";
        let digest = BlobDigest::from_bytes(Sha256::digest(bytes).into());
        let length = NonZeroU64::new(u64::try_from(bytes.len())?).expect("nonempty fixture");
        let expected = ExpectedBlob::try_new(digest, length.get())?;
        let (name, store) = stores.routed_store(crate::BlobStorageClass::UserAttachment);
        let published = store
            .put(expected, Box::new(std::io::Cursor::new(bytes.to_vec())))
            .await?;
        let entry = BlobCatalogRepository::new(pool.clone())
            .register_verified_replica(
                expected,
                BlobStoreBindingRecord::new(name.clone(), stores.namespace_id(name)),
                BlobReplicaRecord::new(name.clone(), published.key().clone()),
            )
            .await?;
        let source = CatalogFileSource {
            entry,
            length,
            stores,
        };
        let one = NonZeroU64::new(1).expect("one byte");
        let two = NonZeroU64::new(2).expect("two bytes");
        assert_eq!(source.read_range(0, length).await?, bytes);
        assert_eq!(source.read_range(length.get() - 1, one).await?, b"e");
        for (offset, requested) in [
            (length.get() - 1, two),
            (length.get(), one),
            (length.get() + 1, one),
            (u64::MAX, one),
        ] {
            assert_eq!(
                source.read_range(offset, requested).await,
                Err(SourceReadError::RangeOutOfBounds)
            );
        }
        std::fs::remove_file(root.path().join("store").join(published.key().as_str()))?;
        assert_eq!(
            source.read_range(0, one).await,
            Err(SourceReadError::Missing)
        );
        assert_eq!(
            source.read_range(length.get(), one).await,
            Err(SourceReadError::RangeOutOfBounds),
            "range rejection precedes store access"
        );
        pool.close().await;
        Ok(())
    }

    #[test]
    fn catalog_database_failures_keep_the_operator_path() {
        use signalbox_application::ClassifyOperatorFailure as _;
        for (failure, class) in [
            (
                BlobCatalogRepositoryError::Database(sqlx::Error::PoolClosed),
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: false,
                },
            ),
            (
                BlobCatalogRepositoryError::CommitAmbiguous(sqlx::Error::PoolClosed),
                OperatorFailureClass::Infrastructure {
                    commit_ambiguous: true,
                },
            ),
            (
                BlobCatalogRepositoryError::Corruption(
                    signalbox_persistence::blob::BlobCatalogCorruption::InvalidDigest,
                ),
                OperatorFailureClass::FailClosedCorruption,
            ),
        ] {
            let FileUseResolutionError::Operator(error) = catalog_resolution_error(failure) else {
                panic!("database errors must not become file failures")
            };
            assert_eq!(
                DaemonToolExecutorError::from_error(&error).operator_failure_class(),
                class
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL and the delegated real file-media sandbox profile"]
    async fn file_tools_enter_the_daemon_catalog_with_a_composed_frontier_resolver()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_database, pool, _) =
            signalbox_persistence::test_support::postgres::migrated_postgres(4).await?;
        let (_root, stores) = configured_blob_stores(&pool).await?;
        let worker = std::env::var_os("NEXTEST_BIN_EXE_signalbox_file_media_text_worker")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .expect("test executable")
                    .parent()
                    .and_then(std::path::Path::parent)
                    .expect("Cargo test binary under deps")
                    .join("signalbox-file-media-text-worker")
            });
        let base = super::super::DaemonToolCatalog::try_new([]).expect("empty base catalog");
        assert!(
            !base
                .definitions()
                .iter()
                .any(|tool| tool.name().as_str() == "file_inspect")
        );
        let image_worker = std::env::var_os("NEXTEST_BIN_EXE_signalbox_file_media_image_worker")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| worker.with_file_name("signalbox-file-media-image-worker"));
        let (family, executor) = DaemonFileMediaExecutor::compose_with_workers(
            pool.clone(),
            stores,
            worker,
            image_worker,
        )
        .await?;
        let composed = base.with_compiled_catalog(family)?;
        let definitions = composed.definitions();
        assert_eq!(
            definitions
                .iter()
                .map(|tool| tool.name().as_str())
                .collect::<Vec<_>>(),
            ["file_inspect", "file_read"]
        );
        assert!(
            definitions.iter().all(
                |tool| tool.effect_class() == signalbox_domain::ToolEffectClass::ExternalEffect
            )
        );
        drop(executor);
        pool.close().await;
        Ok(())
    }
}

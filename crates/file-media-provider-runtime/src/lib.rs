//! Application bridge from visible blob uses to the provider-neutral registry.
//!
//! The resolver port is the sole authority for rendered-frontier visibility and
//! verified-source construction. It returns no store locator, path, credential,
//! or open database transaction to the registry or processor.

use std::{future::Future, pin::Pin};

use signalbox_domain::BlobDigest;
use signalbox_file_media_runtime::{
    CancellationSignal, FileDigest, FileMediaFailure, FileMediaProcessor, FileMediaRegistry,
    FileReadRequest, FileUse, InspectionRequest, VerifiedBlobSource,
};
use signalbox_tools_file_media::{
    FileInspectServiceRequest, FileMediaAgentService, FileMediaAgentServiceFuture,
    FileReadServiceRequest,
};

/// Converts the domain blob identity without changing its exact bytes.
pub const fn neutral_file_digest(digest: BlobDigest) -> FileDigest {
    FileDigest::from_bytes(*digest.as_bytes())
}

/// One authorized semantic use and its placement-free verified source.
#[derive(Debug)]
pub struct ResolvedFileUse<Source> {
    file_use: FileUse,
    source: Source,
}

impl<Source> ResolvedFileUse<Source> {
    /// Constructs evidence returned by a visibility-authorizing resolver.
    pub const fn new(file_use: FileUse, source: Source) -> Self {
        Self { file_use, source }
    }

    /// Borrows exact semantic use metadata.
    pub const fn file_use(&self) -> &FileUse {
        &self.file_use
    }

    /// Borrows the verified placement-free source.
    pub const fn source(&self) -> &Source {
        &self.source
    }

    /// Returns both owned parts.
    pub fn into_parts(self) -> (FileUse, Source) {
        (self.file_use, self.source)
    }
}

/// Boxed future returned by a rendered-frontier resolver.
pub type FileUseResolverFuture<'a, Source> =
    Pin<Box<dyn Future<Output = Result<ResolvedFileUse<Source>, FileMediaFailure>> + Send + 'a>>;

/// Resolves exactly one visible use and ends catalog work before source I/O.
pub trait FileUseResolver: Send {
    /// Placement-free source type returned with each authorization decision.
    type Source: VerifiedBlobSource;

    /// Reuses the blob-read rendered-frontier allow-set and selects one use.
    fn resolve(
        &mut self,
        request: FileInspectServiceRequest,
    ) -> FileUseResolverFuture<'_, Self::Source>;
}

/// Registry-backed implementation of both stable agent tools.
#[derive(Debug)]
pub struct RegistryFileMediaAgentService<Resolver, Processor, Cancellation> {
    registry: FileMediaRegistry,
    resolver: Resolver,
    processor: Processor,
    cancellation: Cancellation,
}

impl<Resolver, Processor, Cancellation>
    RegistryFileMediaAgentService<Resolver, Processor, Cancellation>
{
    /// Composes one immutable registry with visibility, processing, and cancellation ports.
    pub const fn new(
        registry: FileMediaRegistry,
        resolver: Resolver,
        processor: Processor,
        cancellation: Cancellation,
    ) -> Self {
        Self {
            registry,
            resolver,
            processor,
            cancellation,
        }
    }

    /// Borrows the immutable registry snapshot.
    pub const fn registry(&self) -> &FileMediaRegistry {
        &self.registry
    }
}

impl<Resolver, Processor, Cancellation> FileMediaAgentService
    for RegistryFileMediaAgentService<Resolver, Processor, Cancellation>
where
    Resolver: FileUseResolver,
    Processor: FileMediaProcessor,
    Cancellation: CancellationSignal,
{
    fn inspect(
        &mut self,
        request: FileInspectServiceRequest,
    ) -> FileMediaAgentServiceFuture<'_, signalbox_file_media_runtime::FileInspection> {
        Box::pin(async move {
            let requested_digest = request.digest();
            let visible_part = request.visible_part().cloned();
            let resolved = self.resolver.resolve(request).await?;
            let (file_use, source) = resolved.into_parts();
            if file_use.digest() != requested_digest {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            self.registry
                .inspect(
                    &self.processor,
                    InspectionRequest {
                        source: file_use,
                        visible_part,
                    },
                    &source,
                    &self.cancellation,
                )
                .await
        })
    }

    fn read(
        &mut self,
        request: FileReadServiceRequest,
    ) -> FileMediaAgentServiceFuture<'_, signalbox_file_media_runtime::FileReadResult> {
        Box::pin(async move {
            let requested_digest = request.target().digest();
            let visible_part = request.target().visible_part().cloned();
            let view = request.view().clone();
            let options = request
                .options()
                .cloned()
                .map(|options| serde_json::Value::Object(options.into_iter().collect()));
            let continuation = request.continuation().cloned();
            let target =
                FileInspectServiceRequest::from_parts(requested_digest, visible_part.clone());
            let resolved = self.resolver.resolve(target).await?;
            let (file_use, source) = resolved.into_parts();
            if file_use.digest() != requested_digest {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            self.registry
                .read(
                    &self.processor,
                    FileReadRequest {
                        inspection: InspectionRequest {
                            source: file_use,
                            visible_part,
                        },
                        view,
                        options,
                        continuation,
                    },
                    &source,
                    &self.cancellation,
                )
                .await
        })
    }
}

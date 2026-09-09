//! Application bridge from visible blob uses to the provider-neutral registry.
//!
//! The resolver port is the sole authority for rendered-frontier visibility and
//! verified-source construction. It returns no store locator, path, credential,
//! or open database transaction to the registry or processor.

mod continuation;
pub use continuation::ContinuationAuthority;

use continuation::ContinuationState;
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
    selector: signalbox_file_media_runtime::VisiblePartSelector,
}

impl<Source> ResolvedFileUse<Source> {
    /// Constructs evidence returned by a visibility-authorizing resolver.
    pub const fn new(
        file_use: FileUse,
        source: Source,
        selector: signalbox_file_media_runtime::VisiblePartSelector,
    ) -> Self {
        Self {
            file_use,
            source,
            selector,
        }
    }

    /// Borrows exact semantic use metadata.
    pub const fn file_use(&self) -> &FileUse {
        &self.file_use
    }

    /// Borrows the verified placement-free source.
    pub const fn source(&self) -> &Source {
        &self.source
    }

    /// Returns the exact use, range source, and resolved occurrence selector.
    pub fn into_parts(
        self,
    ) -> (
        FileUse,
        Source,
        signalbox_file_media_runtime::VisiblePartSelector,
    ) {
        (self.file_use, self.source, self.selector)
    }
}

/// Boxed future returned by a rendered-frontier resolver.
pub type FileUseResolverFuture<'a, Source> = Pin<
    Box<dyn Future<Output = Result<ResolvedFileUse<Source>, FileUseResolutionError>> + Send + 'a>,
>;

/// Closed failure algebra owned by rendered-frontier resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileUseResolutionError {
    /// Digest is outside the rendered-frontier allow-set.
    BlobNotVisible,
    /// Blob catalog identity is absent.
    BlobMissing,
    /// Every replica contradicted exact bytes.
    BlobCorrupt,
    /// Blob access is temporarily unavailable.
    BlobUnavailable,
    /// Resolver authority or evidence was internally inconsistent.
    Internal,
}

impl From<FileUseResolutionError> for FileMediaFailure {
    fn from(value: FileUseResolutionError) -> Self {
        match value {
            FileUseResolutionError::BlobNotVisible => Self::BlobNotVisible,
            FileUseResolutionError::BlobMissing => Self::BlobMissing,
            FileUseResolutionError::BlobCorrupt => Self::BlobCorrupt,
            FileUseResolutionError::BlobUnavailable => Self::BlobUnavailable,
            FileUseResolutionError::Internal => Self::ProcessorFailed,
        }
    }
}

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
    continuations: ContinuationAuthority,
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
        continuations: ContinuationAuthority,
    ) -> Self {
        Self {
            registry,
            resolver,
            processor,
            cancellation,
            continuations,
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
            let resolved = self
                .resolver
                .resolve(request)
                .await
                .map_err(FileMediaFailure::from)?;
            let (file_use, source, selector) = resolved.into_parts();
            if file_use.digest() != requested_digest {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            self.registry
                .inspect(
                    &self.processor,
                    InspectionRequest {
                        source: file_use,
                        visible_part: Some(selector),
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
            let view = request.view().clone();
            let preceding = request
                .continuation()
                .map(|cursor| self.continuations.open(cursor))
                .transpose()?;
            let visible_part = request.target().visible_part().cloned();
            let resolved = self
                .resolver
                .resolve(FileInspectServiceRequest::from_parts(
                    requested_digest,
                    visible_part,
                ))
                .await
                .map_err(FileMediaFailure::from)?;
            let (file_use, source, selector) = resolved.into_parts();
            if file_use.digest() != requested_digest {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            if preceding
                .as_ref()
                .is_some_and(|state| !state.matches(&file_use, &selector, &view))
            {
                return Err(FileMediaFailure::InvalidViewArguments);
            }
            let input = match &preceding {
                Some(state) => state.runtime_input()?,
                None => request.clone().into_runtime_input(),
            };
            let expected_reader = preceding
                .as_ref()
                .map(ContinuationState::reader)
                .transpose()?;
            let (reader, mut result) = self
                .registry
                .read_with_reader(
                    &self.processor,
                    FileReadRequest {
                        inspection: InspectionRequest {
                            source: file_use.clone(),
                            visible_part: Some(selector.clone()),
                        },
                        view: view.clone(),
                        input,
                    },
                    &source,
                    &self.cancellation,
                    expected_reader.as_ref(),
                )
                .await?;
            let continuation = match &mut result {
                signalbox_file_media_runtime::FileReadResult::Text { continuation, .. }
                | signalbox_file_media_runtime::FileReadResult::Structured {
                    continuation, ..
                } => continuation,
            };
            if let signalbox_file_media_runtime::ReadContinuation::More { cursor } = continuation {
                let state = ContinuationState::new(
                    &file_use,
                    &selector,
                    &view,
                    &reader,
                    preceding.map_or_else(
                        || request.options().cloned().unwrap_or_default(),
                        |state| state.options,
                    ),
                    cursor,
                );
                *cursor = self.continuations.seal(&state)?;
            }
            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests;

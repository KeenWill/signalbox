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
    FileMediaServiceFailure, FileReadServiceRequest,
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
    image_target: Option<signalbox_model_runtime::ImagePresentationCapability>,
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
            image_target: None,
        }
    }

    /// Attaches the issuing model's effective image presentation capability.
    pub fn with_image_target(
        mut self,
        capability: Option<signalbox_model_runtime::ImagePresentationCapability>,
    ) -> Self {
        self.image_target = capability;
        self
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
    /// Authority infrastructure failed; retain its operator class through tool execution.
    Operator(signalbox_tools_file_media::FileMediaExecutorError),
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

impl From<FileUseResolutionError> for FileMediaServiceFailure {
    fn from(value: FileUseResolutionError) -> Self {
        Self::File(match value {
            FileUseResolutionError::Operator(error) => return Self::Operator(error),
            FileUseResolutionError::BlobNotVisible => FileMediaFailure::BlobNotVisible,
            FileUseResolutionError::BlobMissing => FileMediaFailure::BlobMissing,
            FileUseResolutionError::BlobCorrupt => FileMediaFailure::BlobCorrupt,
            FileUseResolutionError::BlobUnavailable => FileMediaFailure::BlobUnavailable,
            FileUseResolutionError::Internal => FileMediaFailure::ProcessorFailed,
        })
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

/// Publishes and verifies validated bytes, then registers their immutable catalog identity.
pub trait FileMediaArtifactPublisher: Send + Sync + std::fmt::Debug {
    /// Completes publication and catalog registration before a durable result can be returned.
    fn publish<'a>(
        &'a self,
        artifact: &'a signalbox_file_media_runtime::ValidatedMediaArtifact,
    ) -> Pin<Box<dyn Future<Output = Result<(), FileMediaServiceFailure>> + Send + 'a>>;
}

/// Registry-backed implementation of both stable agent tools.
#[derive(Debug)]
pub struct RegistryFileMediaAgentService<Resolver, Processor, Cancellation> {
    registry: FileMediaRegistry,
    resolver: Resolver,
    processor: Processor,
    cancellation: Cancellation,
    continuations: ContinuationAuthority,
    publisher: Option<std::sync::Arc<dyn FileMediaArtifactPublisher>>,
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
            publisher: None,
        }
    }

    /// Composes generated-artifact publication after independent output validation.
    pub fn with_artifact_publisher(
        mut self,
        publisher: std::sync::Arc<dyn FileMediaArtifactPublisher>,
    ) -> Self {
        self.publisher = Some(publisher);
        self
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
                .map_err(FileMediaServiceFailure::from)?;
            let (file_use, source, selector) = resolved.into_parts();
            if file_use.digest() != requested_digest {
                return Err(FileMediaFailure::ProcessorFailed.into());
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
                .map_err(Into::into)
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
                .map_err(FileMediaServiceFailure::from)?;
            let image_target = resolved.image_target.clone();
            let (file_use, source, selector) = resolved.into_parts();
            if file_use.digest() != requested_digest {
                return Err(FileMediaFailure::ProcessorFailed.into());
            }
            if preceding
                .as_ref()
                .is_some_and(|state| !state.matches(&file_use, &selector, &view))
            {
                return Err(FileMediaFailure::InvalidViewArguments.into());
            }
            let input = match &preceding {
                Some(state) => state.runtime_input()?,
                None => request.clone().into_runtime_input(),
            };
            let expected_reader = preceding
                .as_ref()
                .map(ContinuationState::reader)
                .transpose()?;
            let (reader, prepared) = self
                .registry
                .prepare_read_with_reader(
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
            let mut result = match prepared {
                signalbox_file_media_runtime::PreparedFileRead::Result(result) => result,
                signalbox_file_media_runtime::PreparedFileRead::Generated(generated) => {
                    let artifact = self
                        .registry
                        .validate_generated(&self.processor, generated, &self.cancellation)
                        .await?;
                    let reference = artifact.reference();
                    if image_target.as_ref().is_none_or(|target| {
                        !target.admits(
                            reference.presented().media_type().as_str(),
                            reference.byte_length().get(),
                        )
                    }) {
                        return Err(FileMediaFailure::OutputUnitTooLarge.into());
                    }
                    self.publisher
                        .as_ref()
                        .ok_or(FileMediaFailure::UnsupportedView)?
                        .publish(&artifact)
                        .await?;
                    signalbox_file_media_runtime::FileReadResult::Reference(
                        artifact.into_reference(),
                    )
                }
            };
            if let signalbox_file_media_runtime::FileReadResult::Reference(reference) = &result {
                let target = image_target
                    .as_ref()
                    .ok_or(FileMediaFailure::UnsupportedView)?;
                if !target.admits(
                    reference.presented().media_type().as_str(),
                    reference.byte_length().get(),
                ) {
                    result = signalbox_file_media_runtime::FileReadResult::Structured {
                        body: reference.large_image_description(),
                        continuation: signalbox_file_media_runtime::ReadContinuation::Complete,
                    };
                }
            }
            let continuation = match &mut result {
                signalbox_file_media_runtime::FileReadResult::Reference(_) => return Ok(result),
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

#[cfg(test)]
mod image_tests;

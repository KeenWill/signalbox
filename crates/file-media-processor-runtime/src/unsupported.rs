use std::{error::Error, fmt, path::PathBuf};

use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProcessCeilings, FileMediaProcessor, FileMediaProcessorFuture,
    FileMediaProviderDeclaration, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProcessorFailure, ProcessorIsolation, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReaderIdentity, VerifiedBlobSource,
};

/// Provider-to-worker binding unavailable outside Linux.
#[derive(Clone, Debug)]
pub struct WorkerBinding {
    declaration: FileMediaProviderDeclaration,
}

impl WorkerBinding {
    /// Rejects construction because this platform cannot provide the sandbox.
    pub fn try_new(
        _program: impl Into<PathBuf>,
        _declaration: FileMediaProviderDeclaration,
    ) -> Result<Self, SandboxedFileMediaProcessorConstructionError> {
        Err(SandboxedFileMediaProcessorConstructionError::Unsupported)
    }

    /// Borrows the provider declaration associated with this binding.
    pub const fn declaration(&self) -> &FileMediaProviderDeclaration {
        &self.declaration
    }
}

/// Non-Linux counterpart of the Linux sandbox processor.
#[derive(Clone, Debug)]
pub struct SandboxedFileMediaProcessor {
    ceilings: FileMediaProcessCeilings,
}

impl SandboxedFileMediaProcessor {
    /// Rejects construction because this platform cannot provide the sandbox.
    pub fn try_new(
        _bubblewrap: impl Into<PathBuf>,
        _bindings: Vec<WorkerBinding>,
        _ceilings: FileMediaProcessCeilings,
    ) -> Result<Self, SandboxedFileMediaProcessorConstructionError> {
        Err(SandboxedFileMediaProcessorConstructionError::Unsupported)
    }

    /// Reports that the accepted Linux isolation profile is unavailable.
    pub async fn verify_isolation(&self) -> ProcessorIsolation {
        ProcessorIsolation::Unavailable
    }

    /// Returns the effective process ceilings.
    pub const fn ceilings(&self) -> FileMediaProcessCeilings {
        self.ceilings
    }
}

impl FileMediaProcessor for SandboxedFileMediaProcessor {
    fn probe<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        Box::pin(async { Err(ProcessorFailure::Unavailable.into()) })
    }

    fn validate<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderValidationRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        Box::pin(async { Err(ProcessorFailure::Unavailable.into()) })
    }

    fn read<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderReadRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        Box::pin(async { Err(ProcessorFailure::Unavailable.into()) })
    }
}

/// Checked sandbox configuration could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxedFileMediaProcessorConstructionError {
    /// This platform cannot provide the accepted sandbox.
    Unsupported,
    /// Bubblewrap was invalid.
    Bubblewrap,
    /// A worker executable was invalid.
    Worker,
    /// Executable snapshots exceeded their aggregate ceiling.
    ExecutableSnapshots,
    /// Worker bindings exceeded their count ceiling.
    WorkerBindings,
    /// Reader declarations exceeded their registry-compatible count ceilings.
    ReaderInventory,
    /// Process ceilings were invalid.
    Ceilings,
    /// The per-invocation controller was unavailable.
    TaskController,
    /// A provider was duplicated.
    DuplicateProvider,
    /// A reader identity was duplicated.
    DuplicateReader,
}

impl fmt::Display for SandboxedFileMediaProcessorConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("file-media sandbox is unsupported on this platform")
    }
}

impl Error for SandboxedFileMediaProcessorConstructionError {}

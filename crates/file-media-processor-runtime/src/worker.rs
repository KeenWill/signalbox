use std::{collections::BTreeMap, error::Error, fmt, num::NonZeroU64, sync::Arc};

use signalbox_file_media_runtime::{
    FileMediaProvider, FileMediaProviderDeclaration, NeverCancelled, ProcessorFailure,
    ReaderIdentity, SourceReadError, SourceReadFuture, VerifiedBlobSource,
};
use tokio::io::{AsyncWriteExt as _, Stdin, Stdout};
use tokio::sync::Mutex;

use crate::{
    broker::{read_frame, write_frame},
    protocol::{
        DaemonFrame, Invocation, WorkerFrame, declaration_fingerprint_ordered, decode_bytes,
    },
};

/// Immutable worker-side inventory of compiled format providers.
pub struct WorkerCatalog {
    providers: Vec<Box<dyn FileMediaProvider>>,
    provider_order: Vec<usize>,
    readers: BTreeMap<ReaderIdentity, usize>,
}

impl fmt::Debug for WorkerCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerCatalog")
            .field("provider_count", &self.providers.len())
            .field("reader_count", &self.readers.len())
            .finish()
    }
}

impl WorkerCatalog {
    /// Builds a deterministic dispatch inventory from compiled providers.
    pub fn try_new(
        providers: Vec<Box<dyn FileMediaProvider>>,
    ) -> Result<Self, WorkerCatalogConstructionError> {
        if providers.is_empty() {
            return Err(WorkerCatalogConstructionError::Empty);
        }
        let mut readers = BTreeMap::new();
        let mut provider_names = Vec::new();
        for (index, provider) in providers.iter().enumerate() {
            let declaration = provider.declaration();
            if provider_names.contains(declaration.provider()) {
                return Err(WorkerCatalogConstructionError::DuplicateProvider);
            }
            provider_names.push(declaration.provider().clone());
            for reader in declaration.readers() {
                if readers.insert(reader.identity().clone(), index).is_some() {
                    return Err(WorkerCatalogConstructionError::DuplicateReader);
                }
            }
        }
        let mut provider_order = (0..providers.len()).collect::<Vec<_>>();
        provider_order.sort_by(|left, right| provider_names[*left].cmp(&provider_names[*right]));
        Ok(Self {
            providers,
            provider_order,
            readers,
        })
    }

    /// Returns declarations in worker construction order for daemon registration.
    pub fn declarations(&self) -> Vec<FileMediaProviderDeclaration> {
        self.providers
            .iter()
            .map(|provider| provider.declaration())
            .collect()
    }

    fn declaration_fingerprint(
        &self,
        requested_providers: &[std::ffi::OsString],
    ) -> Result<[u8; 32], WorkerServiceError> {
        let mut selected = if requested_providers.is_empty() {
            self.provider_order.clone()
        } else {
            Vec::with_capacity(requested_providers.len())
        };
        for requested in requested_providers {
            let requested = requested.to_str().ok_or(WorkerServiceError::Protocol)?;
            let index = self
                .provider_order
                .iter()
                .copied()
                .find(|index| self.providers[*index].declaration().provider().as_str() == requested)
                .ok_or(WorkerServiceError::Protocol)?;
            if selected.contains(&index) {
                return Err(WorkerServiceError::Protocol);
            }
            selected.push(index);
        }
        selected.sort_by(|left, right| {
            self.providers[*left]
                .declaration()
                .provider()
                .cmp(self.providers[*right].declaration().provider())
        });
        Ok(declaration_fingerprint_ordered(
            selected.len(),
            selected
                .iter()
                .map(|index| self.providers[*index].declaration()),
        ))
    }

    fn provider(
        &self,
        reader: &ReaderIdentity,
    ) -> Result<&dyn FileMediaProvider, WorkerServiceError> {
        let index = self
            .readers
            .get(reader)
            .copied()
            .ok_or(WorkerServiceError::Protocol)?;
        self.providers
            .get(index)
            .map(Box::as_ref)
            .ok_or(WorkerServiceError::Protocol)
    }
}

/// Static worker catalog could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerCatalogConstructionError {
    /// At least one provider is required by a worker executable.
    Empty,
    /// Two compiled providers used the same name.
    DuplicateProvider,
    /// Two declarations used the same reader identity.
    DuplicateReader,
}

impl fmt::Display for WorkerCatalogConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "worker provider inventory is empty",
            Self::DuplicateProvider => "worker provider identity is duplicated",
            Self::DuplicateReader => "worker reader identity is duplicated",
        })
    }
}

impl Error for WorkerCatalogConstructionError {}

/// Content-silent worker service failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerServiceError {
    /// Request framing, checked values, identity, or source protocol was invalid.
    Protocol,
    /// A compiled provider failed without a complete typed result.
    Provider,
}

impl fmt::Display for WorkerServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Protocol => "file-media worker protocol failed",
            Self::Provider => "file-media provider failed",
        })
    }
}

impl Error for WorkerServiceError {}

/// Serves exactly one daemon invocation over length-delimited standard I/O.
pub async fn serve_one(catalog: &WorkerCatalog) -> Result<(), WorkerServiceError> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.first().is_some_and(|argument| {
        argument == std::ffi::OsStr::new("--signalbox-file-media-isolation-probe")
    }) {
        let fingerprint = catalog.declaration_fingerprint(&arguments[1..])?;
        let mut output = tokio::io::stdout();
        output
            .write_all(fingerprint.as_slice())
            .await
            .map_err(|_| WorkerServiceError::Protocol)?;
        return output
            .shutdown()
            .await
            .map_err(|_| WorkerServiceError::Protocol);
    }
    if !arguments.is_empty() {
        return Err(WorkerServiceError::Protocol);
    }
    let mut input = tokio::io::stdin();
    let output = tokio::io::stdout();
    let initial: DaemonFrame = read_frame(&mut input)
        .await
        .map_err(|_| WorkerServiceError::Protocol)?;
    let DaemonFrame::Invocation { invocation } = initial else {
        return Err(WorkerServiceError::Protocol);
    };
    let invocation = *invocation;
    let source_wire = *invocation.source();
    let source = BrokeredWorkerSource::new(source_wire, input, output)?;
    let frame = dispatch(catalog, invocation, &source).await?;
    let mut transport = source.transport.lock().await;
    write_frame(&mut transport.output, &frame)
        .await
        .map_err(|_| WorkerServiceError::Protocol)?;
    transport
        .output
        .shutdown()
        .await
        .map_err(|_| WorkerServiceError::Protocol)
}

async fn dispatch(
    catalog: &WorkerCatalog,
    invocation: Invocation,
    source: &BrokeredWorkerSource,
) -> Result<WorkerFrame, WorkerServiceError> {
    match invocation {
        Invocation::Probe { reader, .. } => {
            let reader =
                ReaderIdentity::try_from(reader).map_err(|_| WorkerServiceError::Protocol)?;
            let provider = catalog.provider(&reader)?;
            let output = provider
                .probe(&reader, source, &NeverCancelled)
                .await
                .map_err(map_provider_failure)?;
            Ok(WorkerFrame::ProbeResult { output })
        }
        Invocation::Validate {
            reader, request, ..
        } => {
            let reader =
                ReaderIdentity::try_from(reader).map_err(|_| WorkerServiceError::Protocol)?;
            let request: signalbox_file_media_runtime::FileMediaProviderValidationRequest = request
                .try_into()
                .map_err(|_| WorkerServiceError::Protocol)?;
            require_source_identity(source, &request.source)?;
            let provider = catalog.provider(&reader)?;
            let output = provider
                .inspect(&reader, request, source, &NeverCancelled)
                .await
                .map_err(map_provider_failure)?;
            Ok(WorkerFrame::ValidationResult { output })
        }
        Invocation::Read {
            reader, request, ..
        } => {
            let reader =
                ReaderIdentity::try_from(reader).map_err(|_| WorkerServiceError::Protocol)?;
            let request: signalbox_file_media_runtime::FileMediaProviderReadRequest = request
                .try_into()
                .map_err(|_| WorkerServiceError::Protocol)?;
            require_source_identity(source, &request.source)?;
            let provider = catalog.provider(&reader)?;
            let output = provider
                .read(&reader, request, source, &NeverCancelled)
                .await
                .map_err(map_provider_failure)?;
            Ok(WorkerFrame::ReadResult { output })
        }
    }
}

fn require_source_identity(
    source: &BrokeredWorkerSource,
    requested: &signalbox_file_media_runtime::FileUse,
) -> Result<(), WorkerServiceError> {
    if source.digest() == requested.digest() && source.byte_length() == requested.byte_length() {
        Ok(())
    } else {
        Err(WorkerServiceError::Protocol)
    }
}

fn map_provider_failure(_: ProcessorFailure) -> WorkerServiceError {
    WorkerServiceError::Provider
}

struct WorkerTransport {
    input: Stdin,
    output: Stdout,
}

struct BrokeredWorkerSource {
    digest: signalbox_file_media_runtime::FileDigest,
    byte_length: NonZeroU64,
    transport: Arc<Mutex<WorkerTransport>>,
}

impl BrokeredWorkerSource {
    fn new(
        source: crate::protocol::WireSource,
        input: Stdin,
        output: Stdout,
    ) -> Result<Self, WorkerServiceError> {
        Ok(Self {
            digest: source.digest(),
            byte_length: source
                .byte_length()
                .map_err(|_| WorkerServiceError::Protocol)?,
            transport: Arc::new(Mutex::new(WorkerTransport { input, output })),
        })
    }
}

impl VerifiedBlobSource for BrokeredWorkerSource {
    fn digest(&self) -> signalbox_file_media_runtime::FileDigest {
        self.digest
    }

    fn byte_length(&self) -> NonZeroU64 {
        self.byte_length
    }

    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
        Box::pin(async move {
            let mut transport = self.transport.lock().await;
            let request = WorkerFrame::ReadRange {
                offset,
                length: length.get(),
            };
            write_frame(&mut transport.output, &request)
                .await
                .map_err(|_| SourceReadError::Unavailable)?;
            let response: DaemonFrame = read_frame(&mut transport.input)
                .await
                .map_err(|_| SourceReadError::Unavailable)?;
            match response {
                DaemonFrame::RangeBytes { bytes_base64 } => {
                    let bytes =
                        decode_bytes(&bytes_base64).map_err(|_| SourceReadError::Integrity)?;
                    if bytes.len()
                        == usize::try_from(length.get()).map_err(|_| SourceReadError::Integrity)?
                    {
                        Ok(bytes)
                    } else {
                        Err(SourceReadError::Integrity)
                    }
                }
                DaemonFrame::RangeFailure => Err(SourceReadError::Unavailable),
                DaemonFrame::Invocation { .. } => Err(SourceReadError::Integrity),
            }
        })
    }
}

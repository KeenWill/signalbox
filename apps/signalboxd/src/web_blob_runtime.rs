//! Browser blob projection and isolated deterministic image derivatives.

use std::{
    error::Error,
    fmt,
    fs::File as StandardFile,
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use image::{ImageFormat, ImageReader, Limits};
use sha2::{Digest as _, Sha256};
use signalbox_application::{
    BlobDerivationServiceOutcome, DeterministicBlobDerivationRequest,
    DeterministicBlobDerivationService, DeterministicBlobProducer, UuidV7BlobDerivationIdGenerator,
};
use signalbox_blob_store::{BlobPutOutcome, ExpectedBlob};
use signalbox_domain::{BlobDerivation, BlobDigest, BlobTransformation, BlobTransformationName};
use signalbox_persistence::{
    blob::{BlobCatalogEntry, BlobCatalogRepository, BlobReplicaRecord, BlobStoreBindingRecord},
    blob_derivation::{BlobDerivationRepository, BlobDerivationRepositoryError},
};
use signalbox_tools_exec::{
    CaptureCompleteness, ExecArguments, ExecutionConfinement, ProcessOutcome,
    SandboxedCommandRunner, TokioProcessRunner,
};
use sqlx::PgPool;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::Semaphore,
    time::Instant,
};

use crate::{BlobStorageClass, BlobStoreRegistry};

const WORKER_ARGUMENT: &str = "--web-image-derivative-worker-v1";
const THUMBNAIL_EDGE_PX: u32 = 256;
const PREVIEW_EDGE_PX: u32 = 1600;
const MAX_IMAGE_AXIS: u32 = 16_384;
const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_DECODER_ALLOCATION_BYTES: u64 = 320 * 1024 * 1024;
const MAX_DERIVATIVE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ACTIVE_IMAGE_DERIVATIONS: usize = 2;
const WORKER_TIMEOUT_SECONDS: u64 = 120;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// Closed deterministic image representation produced by this slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebImageDerivativeKind {
    Thumbnail,
    Preview,
}

impl WebImageDerivativeKind {
    const fn edge_px(self) -> u32 {
        match self {
            Self::Thumbnail => THUMBNAIL_EDGE_PX,
            Self::Preview => PREVIEW_EDGE_PX,
        }
    }

    const fn procedure_name(self) -> &'static str {
        match self {
            Self::Thumbnail => "image.thumbnail",
            Self::Preview => "image.preview",
        }
    }

    fn transformation(self) -> Result<BlobTransformation, WebBlobRuntimeError> {
        BlobTransformation::try_new(
            BlobTransformationName::try_new(self.procedure_name())
                .map_err(|_| WebBlobRuntimeError::Integrity)?,
            1,
            &serde_json::json!({"edge_px": self.edge_px(), "format": "image/png"}),
        )
        .map_err(|_| WebBlobRuntimeError::Integrity)
    }
}

/// Production catalog, store, and isolated-producer composition for browser reads.
#[derive(Clone)]
pub struct WebBlobRuntime {
    catalog: BlobCatalogRepository,
    derivations: BlobDerivationRepository,
    registry: Arc<BlobStoreRegistry>,
    image_producer: Option<ImageProducer>,
}

impl fmt::Debug for WebBlobRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebBlobRuntime")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl WebBlobRuntime {
    /// Composes browser blob access from the already-reconciled store registry.
    pub fn new(
        pool: PgPool,
        registry: Arc<BlobStoreRegistry>,
        supervisor_program: Option<PathBuf>,
        worker_program: impl Into<PathBuf>,
    ) -> Result<Self, WebBlobRuntimeError> {
        let worker_program = worker_program.into();
        let catalog = BlobCatalogRepository::new(pool.clone());
        let image_producer = supervisor_program
            .map(|supervisor_program| {
                let implementation =
                    digest_file(&worker_program).map_err(|_| WebBlobRuntimeError::Unavailable)?;
                Ok(ImageProducer {
                    catalog: catalog.clone(),
                    registry: registry.clone(),
                    supervisor_program,
                    worker_program,
                    implementation,
                    budget: Arc::new(Semaphore::new(MAX_ACTIVE_IMAGE_DERIVATIONS)),
                })
            })
            .transpose()?;
        Ok(Self {
            catalog: catalog.clone(),
            derivations: BlobDerivationRepository::new(pool),
            registry,
            image_producer,
        })
    }

    pub(crate) async fn entry(
        &self,
        digest: BlobDigest,
    ) -> Result<BlobCatalogEntry, WebBlobRuntimeError> {
        self.catalog
            .find(digest)
            .await
            .map_err(|_| WebBlobRuntimeError::Unavailable)?
            .ok_or(WebBlobRuntimeError::NotFound)
    }

    pub(crate) const fn registry(&self) -> &Arc<BlobStoreRegistry> {
        &self.registry
    }

    pub(crate) const fn supports_image_derivatives(&self) -> bool {
        self.image_producer.is_some()
    }

    pub(crate) async fn derive_image(
        &self,
        input: BlobDigest,
        kind: WebImageDerivativeKind,
    ) -> Result<BlobDerivation, WebBlobRuntimeError> {
        let producer = self
            .image_producer
            .clone()
            .ok_or(WebBlobRuntimeError::IsolationUnavailable)?;
        let request = DeterministicBlobDerivationRequest::try_new(
            [input],
            kind.transformation()?,
            producer.implementation,
        )
        .map_err(|_| WebBlobRuntimeError::Integrity)?;
        let mut service = DeterministicBlobDerivationService::new(
            UuidV7BlobDerivationIdGenerator,
            self.derivations.clone(),
            producer,
        );
        match service.execute(request).await.map_err(map_service_error)? {
            BlobDerivationServiceOutcome::Reused(derivation)
            | BlobDerivationServiceOutcome::Produced(derivation) => Ok(derivation),
        }
    }
}

#[derive(Clone, Debug)]
struct ImageProducer {
    catalog: BlobCatalogRepository,
    registry: Arc<BlobStoreRegistry>,
    supervisor_program: PathBuf,
    worker_program: PathBuf,
    implementation: BlobDigest,
    budget: Arc<Semaphore>,
}

impl DeterministicBlobProducer for ImageProducer {
    type Error = WebBlobRuntimeError;

    async fn produce(
        &mut self,
        inputs: &[BlobDigest],
        transformation: &BlobTransformation,
    ) -> Result<Box<[BlobDigest]>, Self::Error> {
        let input = inputs
            .first()
            .copied()
            .ok_or(WebBlobRuntimeError::Integrity)?;
        if inputs.len() != 1 {
            return Err(WebBlobRuntimeError::Integrity);
        }
        let edge_px = transformation_edge(transformation)?;
        let _permit = self
            .budget
            .clone()
            .try_acquire_owned()
            .map_err(|_| WebBlobRuntimeError::Busy)?;
        let workspace = tempfile::tempdir().map_err(|_| WebBlobRuntimeError::Unavailable)?;
        let input_path = workspace.path().join("input");
        let output_path = workspace.path().join("output.png");
        let worker_path = workspace.path().join("worker");
        copy_verified_input(&self.catalog, &self.registry, input, &input_path).await?;
        tokio::fs::copy(&self.worker_program, &worker_path)
            .await
            .map_err(|_| WebBlobRuntimeError::Unavailable)?;
        let copied_implementation =
            digest_file(&worker_path).map_err(|_| WebBlobRuntimeError::Unavailable)?;
        if copied_implementation != self.implementation {
            return Err(WebBlobRuntimeError::Integrity);
        }
        make_executable(&worker_path)?;
        run_isolated_worker(workspace.path(), &self.supervisor_program, edge_px).await?;
        let expected = expected_output(&output_path).await?;
        publish_output(&self.catalog, &self.registry, &output_path, expected).await?;
        Ok(Box::new([expected.digest()]))
    }
}

fn transformation_edge(transformation: &BlobTransformation) -> Result<u32, WebBlobRuntimeError> {
    let thumbnail = WebImageDerivativeKind::Thumbnail.transformation()?;
    let preview = WebImageDerivativeKind::Preview.transformation()?;
    if transformation == &thumbnail {
        Ok(THUMBNAIL_EDGE_PX)
    } else if transformation == &preview {
        Ok(PREVIEW_EDGE_PX)
    } else {
        Err(WebBlobRuntimeError::Integrity)
    }
}

async fn copy_verified_input(
    catalog: &BlobCatalogRepository,
    registry: &BlobStoreRegistry,
    digest: BlobDigest,
    destination: &Path,
) -> Result<(), WebBlobRuntimeError> {
    let entry = catalog
        .find(digest)
        .await
        .map_err(|_| WebBlobRuntimeError::Unavailable)?
        .ok_or(WebBlobRuntimeError::NotFound)?;
    if entry.expected().byte_length() > MAX_IMAGE_INPUT_BYTES {
        return Err(WebBlobRuntimeError::ProducerFailed);
    }
    let deadline = Instant::now() + Duration::from_secs(WORKER_TIMEOUT_SECONDS);
    for replica in entry.replicas() {
        let Some(store) = registry.recorded_store(replica.store()) else {
            return Err(WebBlobRuntimeError::Integrity);
        };
        let copied = tokio::time::timeout_at(deadline, async {
            let opened = store
                .open(replica.object_key())
                .await
                .map_err(|_| CandidateCopyError::Read)?;
            if opened.byte_length() != entry.expected().byte_length() {
                return Err(CandidateCopyError::Read);
            }
            copy_input_candidate(
                opened.into_reader(),
                destination,
                entry.expected().byte_length(),
            )
            .await
        })
        .await;
        match copied {
            Ok(Ok(observed_digest)) if observed_digest == digest => return Ok(()),
            Ok(Ok(_)) | Ok(Err(CandidateCopyError::Read)) | Err(_) => continue,
            Ok(Err(CandidateCopyError::Write)) => return Err(WebBlobRuntimeError::Unavailable),
        }
    }
    Err(WebBlobRuntimeError::Corrupt)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateCopyError {
    Read,
    Write,
}

async fn copy_input_candidate(
    mut reader: signalbox_blob_store::BlobReader,
    destination: &Path,
    expected_length: u64,
) -> Result<BlobDigest, CandidateCopyError> {
    let mut output = tokio::fs::File::create(destination)
        .await
        .map_err(|_| CandidateCopyError::Write)?;
    let mut hasher = Sha256::new();
    let mut observed = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| CandidateCopyError::Read)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(u64::try_from(read).map_err(|_| CandidateCopyError::Read)?)
            .ok_or(CandidateCopyError::Read)?;
        if observed > expected_length {
            return Err(CandidateCopyError::Read);
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .await
            .map_err(|_| CandidateCopyError::Write)?;
    }
    output
        .flush()
        .await
        .map_err(|_| CandidateCopyError::Write)?;
    if observed != expected_length {
        return Err(CandidateCopyError::Read);
    }
    Ok(BlobDigest::from_bytes(hasher.finalize().into()))
}

async fn run_isolated_worker(
    workspace: &Path,
    supervisor_program: &Path,
    edge_px: u32,
) -> Result<(), WebBlobRuntimeError> {
    let runner = TokioProcessRunner::try_new(supervisor_program)
        .map_err(|_| WebBlobRuntimeError::IsolationUnavailable)?;
    let mut runner = SandboxedCommandRunner::try_new(runner, workspace)
        .map_err(|_| WebBlobRuntimeError::IsolationUnavailable)?;
    let result = runner
        .try_run(ExecArguments {
            program: String::from("./worker"),
            arguments: vec![
                String::from(WORKER_ARGUMENT),
                String::from("input"),
                String::from("output.png"),
                edge_px.to_string(),
            ],
            working_directory: String::from("."),
            timeout_seconds: WORKER_TIMEOUT_SECONDS,
        })
        .await
        .map_err(|_| WebBlobRuntimeError::Integrity)?;
    let successful = result.confinement == ExecutionConfinement::FilesystemConfined
        && result.outcome == ProcessOutcome::Exited { code: Some(0) }
        && result.stdout.completeness == CaptureCompleteness::Complete
        && result.stderr.completeness == CaptureCompleteness::Complete;
    if successful {
        Ok(())
    } else {
        Err(WebBlobRuntimeError::ProducerFailed)
    }
}

async fn expected_output(path: &Path) -> Result<ExpectedBlob, WebBlobRuntimeError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| WebBlobRuntimeError::ProducerFailed)?;
    if metadata.len() == 0 || metadata.len() > MAX_DERIVATIVE_BYTES {
        return Err(WebBlobRuntimeError::ProducerFailed);
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| WebBlobRuntimeError::ProducerFailed)?;
    let byte_length = u64::try_from(bytes.len()).map_err(|_| WebBlobRuntimeError::Integrity)?;
    if byte_length != metadata.len() {
        return Err(WebBlobRuntimeError::ProducerFailed);
    }
    ExpectedBlob::try_new(BlobDigest::digest(&bytes), byte_length)
        .map_err(|_| WebBlobRuntimeError::ProducerFailed)
}

async fn publish_output(
    catalog: &BlobCatalogRepository,
    registry: &BlobStoreRegistry,
    path: &Path,
    expected: ExpectedBlob,
) -> Result<(), WebBlobRuntimeError> {
    let (store_name, store) = registry.routed_store(BlobStorageClass::GeneratedArtifact);
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| WebBlobRuntimeError::Unavailable)?;
    let publication = store
        .put(expected, Box::new(file))
        .await
        .map_err(|_| WebBlobRuntimeError::Unavailable)?;
    let key = match publication {
        BlobPutOutcome::Published { key }
        | BlobPutOutcome::Repaired { key }
        | BlobPutOutcome::AlreadyPresent { key } => key,
    };
    catalog
        .register_verified_replica(
            expected,
            BlobStoreBindingRecord::new(store_name.clone(), registry.namespace_id(store_name)),
            BlobReplicaRecord::new(store_name.clone(), key),
        )
        .await
        .map_err(|_| WebBlobRuntimeError::Unavailable)?;
    Ok(())
}

fn digest_file(path: &Path) -> io::Result<BlobDigest> {
    let mut file = StandardFile::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(BlobDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), WebBlobRuntimeError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| WebBlobRuntimeError::Unavailable)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), WebBlobRuntimeError> {
    Err(WebBlobRuntimeError::IsolationUnavailable)
}

fn map_service_error(
    error: signalbox_application::BlobDerivationServiceError<
        BlobDerivationRepositoryError,
        WebBlobRuntimeError,
    >,
) -> WebBlobRuntimeError {
    match error {
        signalbox_application::BlobDerivationServiceError::Store(_) => {
            WebBlobRuntimeError::Unavailable
        }
        signalbox_application::BlobDerivationServiceError::Producer(error) => error,
        signalbox_application::BlobDerivationServiceError::InvalidProducerOutput(_) => {
            WebBlobRuntimeError::Integrity
        }
    }
}

/// Closed browser blob runtime failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebBlobRuntimeError {
    NotFound,
    Busy,
    Corrupt,
    Unavailable,
    IsolationUnavailable,
    ProducerFailed,
    Integrity,
}

impl fmt::Display for WebBlobRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("browser blob operation failed")
    }
}

impl Error for WebBlobRuntimeError {}

/// Runs the hidden, no-network image worker mode before daemon startup.
pub fn run_web_image_derivative_worker_if_requested() -> Option<ExitCode> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let mode = arguments.next()?;
    if mode != WORKER_ARGUMENT {
        return None;
    }
    let result = worker_transform(arguments);
    Some(if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn worker_transform(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<(), ()> {
    let input = arguments.next().ok_or(())?;
    let output = arguments.next().ok_or(())?;
    let edge = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| matches!(*value, THUMBNAIL_EDGE_PX | PREVIEW_EDGE_PX))
        .ok_or(())?;
    if arguments.next().is_some() {
        return Err(());
    }
    let reader = ImageReader::open(&input)
        .map_err(|_| ())?
        .with_guessed_format()
        .map_err(|_| ())?;
    let (width, height) = reader.into_dimensions().map_err(|_| ())?;
    validate_encoded_dimensions(width, height)?;
    let mut reader = ImageReader::open(&input)
        .map_err(|_| ())?
        .with_guessed_format()
        .map_err(|_| ())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_AXIS);
    limits.max_image_height = Some(MAX_IMAGE_AXIS);
    limits.max_alloc = Some(MAX_DECODER_ALLOCATION_BYTES);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| ())?;
    image
        .thumbnail(edge, edge)
        .save_with_format(output, ImageFormat::Png)
        .map_err(|_| ())
}

fn validate_encoded_dimensions(width: u32, height: u32) -> Result<(), ()> {
    let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or(())?;
    if width > MAX_IMAGE_AXIS || height > MAX_IMAGE_AXIS || pixels > MAX_IMAGE_PIXELS {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "image fixtures use explicit expectations"
    )]

    use std::{ffi::OsString, fs::File, io};

    use image::{GenericImageView as _, Rgba, RgbaImage};
    use tokio::io::{AsyncRead, ReadBuf};

    use super::{
        CandidateCopyError, MAX_DERIVATIVE_BYTES, copy_input_candidate, expected_output,
        validate_encoded_dimensions, worker_transform,
    };

    struct FailingReader {
        bytes: &'static [u8],
        emitted: bool,
    }

    impl AsyncRead for FailingReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            if self.emitted {
                return std::task::Poll::Ready(Err(io::Error::other("fixture read failure")));
            }
            buffer.put_slice(self.bytes);
            self.emitted = true;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn deterministic_image_worker_bounds_the_long_edge_and_emits_png() {
        let workspace = tempfile::tempdir().expect("fixture workspace exists");
        let input = workspace.path().join("input.png");
        let output = workspace.path().join("output.png");
        RgbaImage::from_pixel(800, 400, Rgba([20, 40, 60, 255]))
            .save(&input)
            .expect("fixture input is encoded");

        worker_transform(
            [
                input.into_os_string(),
                output.clone().into_os_string(),
                OsString::from("256"),
            ]
            .into_iter(),
        )
        .expect("worker accepts the bounded image");
        let decoded = image::open(output).expect("worker output is a decodable PNG");

        assert_eq!(decoded.dimensions(), (256, 128));
    }

    #[test]
    fn image_worker_rejects_oversized_encoded_pixel_count_before_decode() {
        let result = validate_encoded_dimensions(8_193, 8_192);

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn derivative_output_ceiling_is_checked_before_materialization() {
        let workspace = tempfile::tempdir().expect("fixture workspace exists");
        let output = workspace.path().join("oversized.png");
        File::create(&output)
            .expect("fixture output exists")
            .set_len(MAX_DERIVATIVE_BYTES + 1)
            .expect("fixture output is sparse and oversized");

        let result = expected_output(&output).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn input_candidate_rejects_a_midstream_read_failure() {
        let workspace = tempfile::tempdir().expect("fixture workspace exists");
        let destination = workspace.path().join("input");
        let reader = Box::new(FailingReader {
            bytes: b"partial",
            emitted: false,
        });

        let result = copy_input_candidate(reader, &destination, 14).await;

        assert_eq!(result, Err(CandidateCopyError::Read));
    }
}

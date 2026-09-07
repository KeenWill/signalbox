#![cfg(target_os = "linux")]

use std::{
    error::Error,
    num::NonZeroU64,
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use signalbox_file_media_processor_runtime::{SandboxedFileMediaProcessor, WorkerBinding};
use signalbox_file_media_runtime::{
    AttachmentKind, CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType,
    DeclaredMediaType, FileDigest, FileMediaCeilings, FileMediaFailure, FileMediaProcessCeilings,
    FileMediaProcessLimitOverrides, FileMediaProcessor, FileMediaProviderDeclaration,
    FileMediaRegistry, FileReaderName, FileReaderProviderName, FileReaderRevision, FileUse,
    InspectionRequest, NeverCancelled, ProbeDeclaration, ProbeDeclarationInput, ProbeStrength,
    ProcessorBoundaryFailure, ProcessorFailure, ProcessorIsolation, ProcessorProbeOutput,
    ReadAccessPattern, ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration,
    ReaderDeclarationInput, ReaderIdentity, ReasonCode, SourceReadError, SourceReadFuture,
    StreamingTextFallback, VerifiedBlobSource,
};

struct BytesSource(Vec<u8>);

impl VerifiedBlobSource for BytesSource {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([7; 32])
    }

    fn byte_length(&self) -> NonZeroU64 {
        NonZeroU64::new(self.0.len() as u64).unwrap_or(NonZeroU64::MIN)
    }

    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
        Box::pin(async move {
            let start = usize::try_from(offset).map_err(|_| SourceReadError::RangeOutOfBounds)?;
            let length =
                usize::try_from(length.get()).map_err(|_| SourceReadError::RangeOutOfBounds)?;
            let end = start
                .checked_add(length)
                .ok_or(SourceReadError::RangeOutOfBounds)?;
            self.0
                .get(start..end)
                .map(<[u8]>::to_vec)
                .ok_or(SourceReadError::RangeOutOfBounds)
        })
    }
}

/// registered processors require the real accepted sandbox profile.
#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn real_worker_has_the_accepted_isolation_profile() -> Result<(), Box<dyn Error>> {
    let (processor, reader) = available_processor(FileMediaProcessCeilings::version_one()).await?;
    let output = processor
        .probe(&reader, &BytesSource(vec![b'I']), &NeverCancelled)
        .await?;
    assert_eq!(output, successful_probe());
    Ok(())
}

/// worker source reads pass only through the daemon's bounded broker.
#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn worker_can_read_only_through_the_bounded_broker() -> Result<(), Box<dyn Error>> {
    let (processor, reader) = available_processor(FileMediaProcessCeilings::version_one()).await?;
    let output = processor
        .probe(&reader, &BytesSource(vec![b'V']), &NeverCancelled)
        .await;
    assert_eq!(
        output,
        Err(ProcessorBoundaryFailure::Processor(
            ProcessorFailure::Protocol
        ))
    );
    Ok(())
}

/// an incomplete result from a crashed worker is never admitted.
#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn worker_crash_discards_its_incomplete_result() -> Result<(), Box<dyn Error>> {
    let (processor, reader) = available_processor(FileMediaProcessCeilings::version_one()).await?;
    let output = processor
        .probe(&reader, &BytesSource(vec![b'C']), &NeverCancelled)
        .await;
    assert_eq!(
        output,
        Err(ProcessorBoundaryFailure::Processor(
            ProcessorFailure::Failed
        ))
    );
    Ok(())
}

/// the daemon wall deadline terminates work without content leakage.
#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn worker_wall_timeout_is_a_content_silent_failure() -> Result<(), Box<dyn Error>> {
    let ceilings = FileMediaProcessCeilings::try_lower(FileMediaProcessLimitOverrides {
        memory_bytes: 512 * 1024 * 1024,
        cpu_seconds: 60,
        wall_seconds: 1,
        file_descriptors: 32,
        stderr_bytes: 16_384,
    })
    .ok_or("lowered test ceilings must be valid")?;
    let (processor, reader) = available_processor(ceilings).await?;
    let output = processor
        .probe(&reader, &BytesSource(vec![b'T']), &NeverCancelled)
        .await;
    assert_eq!(
        output,
        Err(ProcessorBoundaryFailure::Processor(
            ProcessorFailure::TimedOut
        ))
    );
    Ok(())
}

/// workers may create threads but cannot create descendant processes.
#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn worker_process_creation_is_denied_without_blocking_threads() -> Result<(), Box<dyn Error>>
{
    let (processor, reader) = available_processor(FileMediaProcessCeilings::version_one()).await?;
    let output = processor
        .probe(&reader, &BytesSource(vec![b'X']), &NeverCancelled)
        .await?;
    assert_eq!(output, successful_probe());
    Ok(())
}

/// injection-shaped worker output is sanitized before registry use.
#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn hostile_worker_output_never_propagates() -> Result<(), Box<dyn Error>> {
    let (processor, _) = available_processor(FileMediaProcessCeilings::version_one()).await?;
    let source = BytesSource(vec![b'H']);
    let registry = FileMediaRegistry::try_new(
        processor_declarations()?,
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )?;
    let request = InspectionRequest {
        source: FileUse::new(
            source.digest(),
            source.byte_length(),
            AttachmentKind::File,
            DeclaredMediaType::try_new("application/octet-stream")?,
            None,
        ),
        visible_part: None,
    };
    let output = registry
        .inspect(&processor, request, &source, &NeverCancelled)
        .await;
    assert_eq!(output, Err(FileMediaFailure::ProcessorFailed));
    Ok(())
}

/// authoritative cancellation terminates in-flight worker processing.
#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn authoritative_cancellation_terminates_the_worker() -> Result<(), Box<dyn Error>> {
    let (processor, reader) = available_processor(FileMediaProcessCeilings::version_one()).await?;
    let cancellation = Arc::new(TestCancellation::default());
    let trigger = Arc::clone(&cancellation);
    let cancellation_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        trigger.cancelled.store(true, Ordering::Release);
    });
    let output = processor
        .probe(&reader, &BytesSource(vec![b'T']), cancellation.as_ref())
        .await;
    cancellation_task.await?;
    assert_eq!(
        output,
        Err(ProcessorBoundaryFailure::Processor(
            ProcessorFailure::Cancelled
        ))
    );
    Ok(())
}

async fn available_processor(
    ceilings: FileMediaProcessCeilings,
) -> Result<(SandboxedFileMediaProcessor, ReaderIdentity), Box<dyn Error>> {
    let built = processor(ceilings)?;
    if built.0.verify_isolation().await == ProcessorIsolation::Available {
        return Ok(built);
    }
    Err("the real file-media sandbox profile is unavailable".into())
}

fn successful_probe() -> ProcessorProbeOutput {
    ProcessorProbeOutput::Candidate {
        media_type: String::from("application/x-signalbox-synthetic"),
        strength: ProbeStrength::Strong,
        evidence_bytes: 1,
    }
}

fn processor(
    ceilings: FileMediaProcessCeilings,
) -> Result<(SandboxedFileMediaProcessor, ReaderIdentity), Box<dyn Error>> {
    let (declaration, reader) = declaration()?;
    let worker = PathBuf::from(env!("CARGO_BIN_EXE_signalbox-file-media-synthetic-worker"));
    let binding = WorkerBinding::try_new(worker, declaration)?;
    let processor =
        SandboxedFileMediaProcessor::try_new("/usr/bin/bwrap", vec![binding], ceilings)?;
    Ok((processor, reader))
}

fn declaration() -> Result<(FileMediaProviderDeclaration, ReaderIdentity), Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new("synthetic")?;
    let reader = ReaderIdentity::new(
        provider.clone(),
        FileReaderName::try_new("fixture")?,
        FileReaderRevision::try_new("v1")?,
    );
    let view = ReadViewDeclaration::try_new(
        ReadViewName::try_new("text")?,
        String::from("Reads synthetic text."),
        CanonicalJsonObjectSchema::try_new(r#"{"type":"object"}"#)?,
        ReadAccessPattern::Streaming { maximum_ranges: 1 },
        ReadViewBounds::Text {
            source_bytes: 64,
            output_bytes: 64,
        },
    )?;
    let declaration = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: reader.reader().clone(),
        revision: reader.revision().clone(),
        media_types: vec![CanonicalMediaType::from_str(
            "application/x-signalbox-synthetic",
        )?],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 1,
            suffix_bytes: 0,
            range_count: 1,
            cumulative_bytes: 1,
        }),
        validation: signalbox_file_media_runtime::ValidationDeclaration::new(64, 1),
        views: vec![view],
        reason_codes: vec![ReasonCode::try_new("synthetic_failure")?],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?;
    Ok((
        FileMediaProviderDeclaration::try_new(provider, vec![declaration])?,
        reader,
    ))
}

fn processor_declarations() -> Result<Vec<FileMediaProviderDeclaration>, Box<dyn Error>> {
    Ok(vec![declaration()?.0])
}

#[derive(Default)]
struct TestCancellation {
    cancelled: AtomicBool,
}

impl CancellationSignal for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

use std::{error::Error, fs, path::Path, str::FromStr, time::Duration};

use signalbox_file_media_processor_runtime::{WorkerCatalog, serve_one};
use signalbox_file_media_runtime::{
    CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider, FileMediaProviderDeclaration,
    FileMediaProviderFuture, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    FileReaderName, FileReaderProviderName, FileReaderRevision, ProbeDeclaration, ProbeStrength,
    ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput, ReadAccessPattern,
    ReadOutputKind, ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration,
    ReaderDeclarationInput, ReaderIdentity, ReasonCode, StreamingTextFallback, VerifiedBlobSource,
};

struct SyntheticProvider;

impl FileMediaProvider for SyntheticProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        synthetic_declaration().unwrap_or_else(|error| {
            eprintln!("synthetic declaration failed: {error}");
            std::process::exit(2);
        })
    }

    fn probe<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn signalbox_file_media_runtime::CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            let length = std::num::NonZeroU64::new(1)
                .ok_or(signalbox_file_media_runtime::ProcessorFailure::Failed)?;
            let prefix = source
                .read_range(0, length)
                .await
                .map_err(|_| signalbox_file_media_runtime::ProcessorFailure::Failed)?;
            match prefix.first().copied() {
                Some(b'C') => std::process::exit(7),
                Some(b'T') => std::thread::sleep(Duration::from_secs(5)),
                Some(b'X') => {
                    let thread_output = std::thread::spawn(|| 1_u8)
                        .join()
                        .map_err(|_| signalbox_file_media_runtime::ProcessorFailure::Failed)?;
                    if thread_output != 1 {
                        return Err(signalbox_file_media_runtime::ProcessorFailure::Failed);
                    }
                    let spawned = std::process::Command::new("/signalbox-file-media-worker")
                        .arg("--signalbox-file-media-isolation-probe")
                        .status();
                    if spawned.is_ok() {
                        return Err(signalbox_file_media_runtime::ProcessorFailure::Failed);
                    }
                }
                Some(b'I') => verify_sandbox_authority()?,
                Some(b'H') => {
                    return Ok(ProcessorProbeOutput::Candidate {
                        media_type: String::from("</tool><script>alert(1)</script>"),
                        strength: ProbeStrength::Strong,
                    });
                }
                Some(_) => {}
                None => return Err(signalbox_file_media_runtime::ProcessorFailure::Failed),
            }
            Ok(ProcessorProbeOutput::Candidate {
                media_type: String::from("application/x-signalbox-synthetic"),
                strength: ProbeStrength::Strong,
            })
        })
    }

    fn inspect<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn signalbox_file_media_runtime::CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            Ok(ProcessorValidationOutput::Validated {
                media_type: request.media_type.as_str().to_owned(),
                evidence: request.evidence,
                metadata_json: String::from("{}"),
            })
        })
    }

    fn read<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderReadRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn signalbox_file_media_runtime::CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorReadOutput> {
        Box::pin(async {
            Ok(ProcessorReadOutput::Text {
                body: String::from("synthetic"),
                truncated: false,
                cursor: None,
            })
        })
    }
}

fn verify_sandbox_authority() -> Result<(), signalbox_file_media_runtime::ProcessorFailure> {
    let failed = || signalbox_file_media_runtime::ProcessorFailure::Failed;
    if Path::new("/etc/passwd").exists()
        || std::env::current_dir().map_err(|_| failed())? != Path::new("/tmp")
    {
        return Err(failed());
    }
    let mut environment = std::env::vars().collect::<Vec<_>>();
    environment.sort_unstable();
    if environment
        != [
            (String::from("LANG"), String::from("C.UTF-8")),
            (String::from("LC_ALL"), String::from("C.UTF-8")),
        ]
    {
        return Err(failed());
    }
    let status = fs::read_to_string("/proc/self/status").map_err(|_| failed())?;
    let capabilities = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .ok_or_else(failed)?;
    if u64::from_str_radix(capabilities.trim(), 16).map_err(|_| failed())? != 0 {
        return Err(failed());
    }
    let routes = fs::read_to_string("/proc/net/route").map_err(|_| failed())?;
    if routes
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .any(|interface| interface != "lo")
    {
        return Err(failed());
    }
    Ok(())
}

fn synthetic_declaration() -> Result<FileMediaProviderDeclaration, Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new("synthetic")?;
    let view = ReadViewDeclaration::try_new(
        ReadViewName::try_new("text")?,
        String::from("Reads synthetic text."),
        CanonicalJsonObjectSchema::try_new(r#"{"type":"object"}"#)?,
        ReadAccessPattern::Streaming,
        ReadViewBounds::Text {
            source_bytes: 64,
            output_bytes: 64,
        },
    )?;
    if view.output_kind() != ReadOutputKind::Text {
        return Err("synthetic view kind drifted".into());
    }
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new("fixture")?,
        revision: FileReaderRevision::try_new("v1")?,
        media_types: vec![CanonicalMediaType::from_str(
            "application/x-signalbox-synthetic",
        )?],
        probe: ProbeDeclaration::new(1, 0, 1, 1),
        views: vec![view],
        reason_codes: vec![ReasonCode::try_new("synthetic_failure")?],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?;
    FileMediaProviderDeclaration::try_new(provider, vec![reader]).map_err(Into::into)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let catalog = WorkerCatalog::try_new(vec![Box::new(SyntheticProvider)])?;
    serve_one(&catalog).await?;
    Ok(())
}

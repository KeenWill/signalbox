mod fixtures;

use std::error::Error;

use fixtures::{ArchiveFixture, MemorySource};
use signalbox_file_media_adapter_archives::{ArchiveProvider, declaration};
use signalbox_file_media_runtime::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProvider, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileMediaRegistry, FileReadRequest, FileReadResult,
    InspectionRequest, NeverCancelled, ProcessorIsolation, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReadContinuation, ReadViewName, ReaderIdentity,
    VerifiedBlobSource,
};

struct DirectProcessor {
    provider: ArchiveProvider,
}

impl DirectProcessor {
    const fn new() -> Self {
        Self {
            provider: ArchiveProvider::new(),
        }
    }
}

impl FileMediaProcessor for DirectProcessor {
    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        self.provider.probe(reader, source, cancellation)
    }

    fn validate<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        self.provider.inspect(reader, request, source, cancellation)
    }

    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        self.provider.read(reader, request, source, cancellation)
    }
}

#[test]
fn declaration_registers_four_archive_formats_under_available_isolation()
-> Result<(), Box<dyn Error>> {
    let registry = registry()?;

    assert_eq!(registry.providers(), &[declaration()?]);
    assert_eq!(declaration()?.readers().len(), 4);
    Ok(())
}

#[tokio::test]
async fn generated_zip_validates_and_enumerates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::zip()?).await?);
    Ok(())
}

#[tokio::test]
async fn generated_tar_validates_and_enumerates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::tar()?).await?);
    Ok(())
}

#[tokio::test]
async fn generated_gzip_validates_and_enumerates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::gzip()?).await?);
    Ok(())
}

#[tokio::test]
async fn generated_zstd_validates_and_enumerates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::zstd()?).await?);
    Ok(())
}

#[tokio::test]
async fn zstd_with_leading_skippable_frame_validates_and_enumerates() -> Result<(), Box<dyn Error>>
{
    assert_valid_inventory(valid_inventory(ArchiveFixture::zstd_with_skippable_frame()?).await?);
    Ok(())
}

#[tokio::test]
async fn truncated_zip_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::truncated_zip()?).await?,
        "malformed_archive",
    );
    Ok(())
}

#[tokio::test]
async fn truncated_tar_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::truncated_tar()?).await?,
        "malformed_archive",
    );
    Ok(())
}

#[tokio::test]
async fn truncated_gzip_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::truncated_gzip()?).await?,
        "malformed_archive",
    );
    Ok(())
}

#[tokio::test]
async fn truncated_zstd_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::truncated_zstd()?).await?,
        "malformed_archive",
    );
    Ok(())
}

#[tokio::test]
async fn unsupported_zip_compression_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>>
{
    assert_malformed(
        malformed_inspection(ArchiveFixture::unsupported_compression_zip()?).await?,
        "unsupported_compression_method",
    );
    Ok(())
}

#[tokio::test]
async fn locked_zip_is_terminal_without_a_password_channel() -> Result<(), Box<dyn Error>> {
    let source = ArchiveFixture::locked_zip()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::EncryptedOrLocked);
    Ok(())
}

#[tokio::test]
async fn zip_slip_name_is_rejected_without_path_materialization() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zip_slip()?).await?,
        "hostile_entry_name",
    );
    Ok(())
}

#[tokio::test]
async fn zip_symlink_is_rejected_without_following() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zip_symlink()?).await?,
        "link_entry",
    );
    Ok(())
}

#[tokio::test]
async fn data_bearing_zip_directory_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::data_bearing_zip_directory()?).await?,
        "special_entry",
    );
    Ok(())
}

#[tokio::test]
async fn tar_symlink_is_rejected_without_following() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::tar_symlink()?).await?,
        "link_entry",
    );
    Ok(())
}

#[tokio::test]
async fn hostile_gzip_filename_is_rejected_as_untrusted_input() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::hostile_gzip_name()?).await?,
        "hostile_entry_name",
    );
    Ok(())
}

#[tokio::test]
async fn recursive_zip_entry_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::recursive_zip()?).await?,
        "recursive_container",
    );
    Ok(())
}

#[tokio::test]
async fn disguised_recursive_zip_payload_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::disguised_recursive_zip()?).await?,
        "recursive_container",
    );
    Ok(())
}

#[tokio::test]
async fn disguised_empty_zip_payload_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::disguised_empty_zip()?).await?,
        "recursive_container",
    );
    Ok(())
}

#[tokio::test]
async fn disguised_v7_tar_payload_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::disguised_v7_tar()?).await?,
        "recursive_container",
    );
    Ok(())
}

#[tokio::test]
async fn recursive_zstd_payload_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::recursive_zstd()?).await?,
        "recursive_container",
    );
    Ok(())
}

#[tokio::test]
async fn compressed_zip_bomb_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zip_bomb()?).await?,
        "expanded_size_limit",
    );
    Ok(())
}

#[tokio::test]
async fn compressed_gzip_bomb_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::gzip_bomb()?).await?,
        "expanded_size_limit",
    );
    Ok(())
}

#[tokio::test]
async fn compressed_zstd_bomb_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zstd_bomb()?).await?,
        "expanded_size_limit",
    );
    Ok(())
}

#[tokio::test]
async fn tar_declared_size_bomb_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::tar_declared_bomb()?).await?,
        "expanded_size_limit",
    );
    Ok(())
}

#[tokio::test]
async fn excessive_zip_entry_count_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::excessive_zip_entries()?).await?,
        "entry_count_limit",
    );
    Ok(())
}

#[tokio::test]
async fn unknown_bytes_remain_a_typed_unknown_inspection() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::unknown(b"not an archive".to_vec())?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn unrecognized_declared_archive_remains_unknown() -> Result<(), Box<dyn Error>> {
    let source = ArchiveFixture::mislabeled_zip()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn hostile_view_arguments_are_typed_and_content_silent() -> Result<(), Box<dyn Error>> {
    let source = ArchiveFixture::zip()?.into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        serde_json::json!({"extract_to": "../../host"}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::InvalidViewArguments));
    Ok(())
}

struct ValidInventory {
    inspection: FileInspection,
    body: serde_json::Value,
    expected_format: &'static str,
    expected_name: &'static str,
}

async fn valid_inventory(fixture: ArchiveFixture) -> Result<ValidInventory, Box<dyn Error>> {
    let expected_format = fixture.expected_format();
    let expected_name = fixture.expected_name();
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;
    let result = read(&DirectProcessor::new(), &source, serde_json::json!({})).await?;
    let body = complete_structure(result)?;

    Ok(ValidInventory {
        inspection,
        body,
        expected_format,
        expected_name,
    })
}

#[track_caller]
fn assert_valid_inventory(actual: ValidInventory) {
    assert_eq!(actual.inspection.status(), FileInspectionStatus::Validated);
    assert_eq!(actual.body["format"], actual.expected_format);
    assert_eq!(actual.body["entries"][0]["name"], actual.expected_name);
}

struct MalformedInspection {
    inspection: FileInspection,
    reason: String,
}

async fn malformed_inspection(
    fixture: ArchiveFixture,
) -> Result<MalformedInspection, Box<dyn Error>> {
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;
    let reason = String::from(malformed_reason(&inspection)?);
    Ok(MalformedInspection { inspection, reason })
}

#[track_caller]
fn assert_malformed(actual: MalformedInspection, expected_reason: &str) {
    assert_eq!(actual.inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(actual.reason, expected_reason);
}

fn registry() -> Result<FileMediaRegistry, Box<dyn Error>> {
    Ok(FileMediaRegistry::try_new(
        vec![declaration()?],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )?)
}

async fn inspect(
    processor: &dyn FileMediaProcessor,
    source: &MemorySource,
) -> Result<FileInspection, FileMediaFailure> {
    let request = InspectionRequest {
        source: source
            .file_use()
            .map_err(|_| FileMediaFailure::ProcessorFailed)?,
        visible_part: None,
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .inspect(processor, request, source, &NeverCancelled)
        .await
}

async fn read(
    processor: &dyn FileMediaProcessor,
    source: &MemorySource,
    options: serde_json::Value,
) -> Result<FileReadResult, FileMediaFailure> {
    let request = FileReadRequest {
        inspection: InspectionRequest {
            source: source
                .file_use()
                .map_err(|_| FileMediaFailure::ProcessorFailed)?,
            visible_part: None,
        },
        view: ReadViewName::try_new("entries").map_err(|_| FileMediaFailure::ProcessorFailed)?,
        options,
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .read(processor, request, source, &NeverCancelled)
        .await
}

fn malformed_reason(inspection: &FileInspection) -> Result<&str, Box<dyn Error>> {
    match inspection {
        FileInspection::Malformed { reason_code, .. } => Ok(reason_code.as_str()),
        _ => Err("expected malformed archive".into()),
    }
}

fn complete_structure(result: FileReadResult) -> Result<serde_json::Value, Box<dyn Error>> {
    match result {
        FileReadResult::Structured {
            body,
            continuation: ReadContinuation::Complete,
        } => Ok(body),
        _ => Err("expected complete structured result".into()),
    }
}

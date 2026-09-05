//! Integration coverage for the archive adapter contract in `docs/spec/file-and-media.md`.

mod fixtures;

use std::error::Error;

use fixtures::{ArchiveFixture, MemorySource};
use signalbox_file_media_adapter_archives::{ArchiveProvider, declaration};
use signalbox_file_media_runtime::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProvider, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileMediaRegistry, FileMediaRegistryConstructionError,
    FileReadInput, FileReadRequest, FileReadResult, InspectionRequest, NeverCancelled,
    ProcessorBoundaryFailure, ProcessorFailure, ProcessorIsolation, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReadAccessPattern, ReadContinuation,
    ReadViewName, ReaderIdentity, VerifiedBlobSource,
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
        Box::pin(async move {
            self.provider
                .probe(reader, source, cancellation)
                .await
                .map_err(map_provider_failure)
        })
    }

    fn validate<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            self.provider
                .inspect(reader, request, source, cancellation)
                .await
                .map_err(map_provider_failure)
        })
    }

    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        Box::pin(async move {
            self.provider
                .read(reader, request, source, cancellation)
                .await
                .map_err(map_provider_failure)
        })
    }
}

fn map_provider_failure(
    _: signalbox_file_media_runtime::FileMediaProviderFailure,
) -> ProcessorBoundaryFailure {
    ProcessorFailure::Failed.into()
}

#[test]
fn declaration_registers_four_archive_formats_under_available_isolation()
-> Result<(), Box<dyn Error>> {
    let registry = registry()?;

    assert_eq!(registry.providers(), &[declaration()?]);
    let declaration = declaration()?;
    assert_eq!(declaration.readers().len(), 4);
    assert_eq!(declaration.observed_container_entries(), Some(1_000));
    let gzip_view = declaration.readers()[0]
        .views()
        .first()
        .ok_or("GZIP reader must declare its entries view")?;
    assert_eq!(
        gzip_view.access(),
        ReadAccessPattern::Streaming { maximum_ranges: 1 }
    );
    let tar_view = declaration.readers()[1]
        .views()
        .first()
        .ok_or("TAR reader must declare its entries view")?;
    assert_eq!(
        tar_view.access(),
        ReadAccessPattern::Streaming { maximum_ranges: 1 }
    );
    let zip_view = declaration.readers()[2]
        .views()
        .first()
        .ok_or("ZIP reader must declare its entries view")?;
    assert_eq!(
        zip_view.access(),
        ReadAccessPattern::Streaming { maximum_ranges: 1 }
    );
    let zstd_view = declaration.readers()[3]
        .views()
        .first()
        .ok_or("Zstandard reader must declare its entries view")?;
    assert_eq!(
        zstd_view.access(),
        ReadAccessPattern::Streaming { maximum_ranges: 1 }
    );
    Ok(())
}

#[test]
fn declaration_rejects_an_effective_entry_ceiling_below_its_bound() -> Result<(), Box<dyn Error>> {
    let ceilings = FileMediaCeilings {
        observed_container_entries: 999,
        ..FileMediaCeilings::version_one()
    };

    let outcome = FileMediaRegistry::try_new(
        vec![declaration()?],
        ceilings,
        ProcessorIsolation::Available,
    );

    assert_eq!(
        outcome.unwrap_err(),
        FileMediaRegistryConstructionError::ContainerBounds
    );
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
async fn concatenated_tar_segments_are_all_inspected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::concatenated_tar_with_hostile_second_segment()?)
            .await?,
        "hostile_entry_name",
    );
    Ok(())
}

#[tokio::test]
async fn zip_with_preamble_validates_and_enumerates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::zip_with_preamble()?).await?);
    Ok(())
}

#[tokio::test]
async fn declared_zip_after_probe_prefix_validates_and_enumerates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::zip_after_long_preamble()?).await?);
    Ok(())
}

#[tokio::test]
async fn declared_zip_after_signature_like_long_preamble_validates_and_enumerates()
-> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zip_after_signature_like_long_preamble()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn zip_with_signature_like_preamble_validates_and_enumerates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zip_with_gzip_signature_preamble()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn legacy_zip_filename_is_decoded_before_validation() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::legacy_named_zip()?).await?);
    Ok(())
}

#[tokio::test]
async fn zero_sized_zip_directory_stream_is_decoded() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zero_sized_data_bearing_zip_directory()?).await?,
        "special_entry",
    );
    Ok(())
}

#[tokio::test]
async fn empty_tar_validates() -> Result<(), Box<dyn Error>> {
    let source = ArchiveFixture::empty_tar().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn generated_gzip_validates_and_enumerates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::gzip()?).await?);
    Ok(())
}

#[tokio::test]
async fn zip_signature_in_gzip_extra_does_not_create_ambiguity() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::gzip_with_zip_signature_in_extra()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn latin1_gzip_filename_is_decoded_before_validation() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(valid_inventory(ArchiveFixture::latin1_named_gzip()?).await?);
    Ok(())
}

#[tokio::test]
async fn hostile_later_gzip_member_filename_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::gzip_with_hostile_later_member()?).await?,
        "hostile_entry_name",
    );
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
async fn zip_signature_in_zstd_skippable_frame_does_not_create_ambiguity()
-> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zstd_with_zip_signature_in_skippable_frame()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn zstd_stream_with_only_skippable_frames_validates() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zstd_with_only_skippable_frames()).await?,
    );
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
async fn dictionary_dependent_zstd_is_malformed_without_a_dictionary() -> Result<(), Box<dyn Error>>
{
    assert_malformed(
        malformed_inspection(ArchiveFixture::dictionary_zstd()?).await?,
        "malformed_archive",
    );
    Ok(())
}

#[tokio::test]
async fn later_dictionary_dependent_zstd_frame_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::concatenated_dictionary_zstd()?).await?,
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
async fn mode_only_data_bearing_zip_directory_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::mode_only_data_bearing_zip_directory()?).await?,
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
async fn data_bearing_tar_directory_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::data_bearing_tar_directory()?).await?,
        "special_entry",
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
async fn gzip_signature_like_payload_is_not_a_recursive_archive() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zip_with_gzip_signature_text_payload()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn zstd_signature_like_payload_is_not_a_recursive_archive() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zip_with_zstd_signature_text_payload()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn oversized_nested_gzip_is_rejected_as_recursive() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zip_with_oversized_nested_gzip()?).await?,
        "recursive_container",
    );
    Ok(())
}

#[tokio::test]
async fn corrupt_oversized_nested_gzip_is_not_recursive() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zip_with_corrupt_oversized_nested_gzip()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn oversized_nested_zstd_is_rejected_as_recursive() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zip_with_oversized_nested_zstd()?).await?,
        "recursive_container",
    );
    Ok(())
}

#[tokio::test]
async fn corrupt_dictionary_zstd_payload_is_not_recursive() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zip_with_corrupt_dictionary_zstd_payload()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn tar_signature_like_payload_is_not_a_recursive_archive() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zip_with_tar_signature_text_payload()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn zip_signature_text_is_not_a_recursive_archive() -> Result<(), Box<dyn Error>> {
    assert_valid_inventory(
        valid_inventory(ArchiveFixture::zip_with_signature_text_payload()?).await?,
    );
    Ok(())
}

#[tokio::test]
async fn recursive_zip_split_across_gzip_members_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::gzip_with_split_zip_signature()?).await?,
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
async fn disguised_zip_after_long_preamble_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::disguised_recursive_zip_after_long_preamble()?)
            .await?,
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
async fn disguised_empty_tar_payload_is_rejected() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::disguised_empty_tar()?).await?,
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
async fn gzip_logical_entry_obeys_the_per_entry_ceiling() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::gzip_entry_bomb()?).await?,
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
async fn zstd_logical_entry_obeys_the_per_entry_ceiling() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zstd_entry_bomb()?).await?,
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
async fn duplicate_zip_central_directory_names_are_a_typed_malformed_inspection()
-> Result<(), Box<dyn Error>> {
    assert_malformed(
        malformed_inspection(ArchiveFixture::zip_with_duplicate_central_directory_names()?).await?,
        "malformed_archive",
    );
    Ok(())
}

#[tokio::test]
async fn zip_inside_zstd_skippable_frame_is_ambiguous() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::unknown(fixtures::zip_inside_zstd_skippable_frame()?)?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Ambiguous);
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
async fn invalid_unanchored_zip_signature_remains_unknown() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::unknown(b"plain bytes PK\x03\x04 without a ZIP".to_vec())?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn oversized_unanchored_zip_signature_remains_unknown() -> Result<(), Box<dyn Error>> {
    let mut bytes = vec![b'x'; 256 * 1_024 + 1];
    bytes[32..36].copy_from_slice(b"PK\x03\x04");
    let source = MemorySource::unknown(bytes)?;
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
async fn oversized_unrecognized_declared_archive_remains_unknown() -> Result<(), Box<dyn Error>> {
    let source = ArchiveFixture::oversized_mislabeled_zip()?.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn escape_expanding_entry_names_are_a_typed_output_bound_failure()
-> Result<(), Box<dyn Error>> {
    let source = MemorySource::new(
        fixtures::zip_with_escape_expanding_entry_names()?,
        "application/zip",
    )?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;
    assert_eq!(inspection.status(), FileInspectionStatus::Validated);

    let result = read(&DirectProcessor::new(), &source, serde_json::json!({})).await;

    assert_eq!(result, Err(FileMediaFailure::OutputUnitTooLarge));
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
        input: FileReadInput::Initial { options },
    };
    registry()
        .map_err(|_| FileMediaFailure::ProcessorFailed)?
        .read(processor, request, source, &NeverCancelled)
        .await
}

fn malformed_reason(inspection: &FileInspection) -> Result<&str, Box<dyn Error>> {
    match inspection {
        FileInspection::Malformed { reason_code, .. } => Ok(reason_code.as_str()),
        FileInspection::Validated(_)
        | FileInspection::Unknown { .. }
        | FileInspection::Ambiguous { .. }
        | FileInspection::DeclaredMismatch { .. }
        | FileInspection::EncryptedOrLocked { .. } => Err("expected malformed archive".into()),
    }
}

fn complete_structure(result: FileReadResult) -> Result<serde_json::Value, Box<dyn Error>> {
    match result {
        FileReadResult::Structured {
            body,
            continuation: ReadContinuation::Complete,
        } => Ok(body),
        FileReadResult::Text { .. }
        | FileReadResult::Structured {
            continuation: ReadContinuation::More { .. },
            ..
        } => Err("expected complete structured result".into()),
    }
}

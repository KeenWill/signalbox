mod fixtures;

use std::error::Error;

use fixtures::{MemorySource, VideoFixture};
use signalbox_file_media_adapter_video::{VideoProvider, declaration};
use signalbox_file_media_runtime::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProvider, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileMediaRegistry, FileReadRequest, FileReadResult,
    InspectionRequest, NeverCancelled, ProcessorIsolation, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReadContinuation, ReadViewName, ReaderIdentity,
    VerifiedBlobSource,
};

struct DirectProcessor {
    provider: VideoProvider,
}

impl DirectProcessor {
    const fn new() -> Self {
        Self {
            provider: VideoProvider::new(),
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

struct AdversarialOutputProcessor {
    direct: DirectProcessor,
}

impl AdversarialOutputProcessor {
    const fn new() -> Self {
        Self {
            direct: DirectProcessor::new(),
        }
    }
}

impl FileMediaProcessor for AdversarialOutputProcessor {
    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        self.direct.probe(reader, source, cancellation)
    }

    fn validate<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        self.direct.validate(reader, request, source, cancellation)
    }

    fn read<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderReadRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        Box::pin(async {
            Ok(ProcessorReadOutput::Text {
                body: String::from("decoder\0injection"),
                truncated: false,
                cursor: None,
            })
        })
    }
}

#[test]
fn declaration_registers_mp4_and_webm_under_available_isolation() -> Result<(), Box<dyn Error>> {
    let registry = registry()?;

    assert_eq!(registry.providers(), &[declaration()?]);
    Ok(())
}

#[tokio::test]
async fn generated_mp4_validates_and_reports_metadata() -> Result<(), Box<dyn Error>> {
    let fixture = VideoFixture::ordinary_mp4();
    let expected_duration = fixture.expected_duration_milliseconds();
    let expected_tracks = fixture.expected_video_tracks();
    let expected_container = fixture.expected_container();
    let expected_profile = fixture.expected_profile();
    let (inspection, body) = inspect_and_read_metadata(fixture).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    assert_eq!(body["duration_milliseconds"], expected_duration);
    assert_eq!(body["video_tracks"], expected_tracks);
    assert_eq!(body["container"], expected_container);
    assert_eq!(body["profile"], expected_profile);
    Ok(())
}

#[tokio::test]
async fn generated_webm_validates_and_reports_metadata() -> Result<(), Box<dyn Error>> {
    let fixture = VideoFixture::ordinary_webm();
    let expected_duration = fixture.expected_duration_milliseconds();
    let expected_tracks = fixture.expected_video_tracks();
    let expected_container = fixture.expected_container();
    let expected_profile = fixture.expected_profile();
    let (inspection, body) = inspect_and_read_metadata(fixture).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    assert_eq!(body["duration_milliseconds"], expected_duration);
    assert_eq!(body["video_tracks"], expected_tracks);
    assert_eq!(body["container"], expected_container);
    assert_eq!(body["profile"], expected_profile);
    Ok(())
}

#[tokio::test]
async fn truncated_mp4_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::truncated_mp4(), "malformed_video").await
}

#[tokio::test]
async fn truncated_webm_is_a_typed_malformed_inspection() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::truncated_webm(), "malformed_video").await
}

#[tokio::test]
async fn encrypted_mp4_is_terminal_without_a_key_channel() -> Result<(), Box<dyn Error>> {
    assert_locked(VideoFixture::encrypted_mp4()).await
}

#[tokio::test]
async fn encrypted_webm_is_terminal_without_a_key_channel() -> Result<(), Box<dyn Error>> {
    assert_locked(VideoFixture::encrypted_webm()).await
}

#[tokio::test]
async fn nested_mp4_movie_is_rejected_as_a_recursive_container() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::recursive_mp4(), "recursive_container").await
}

#[tokio::test]
async fn nested_webm_segment_is_rejected_as_a_recursive_container() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::recursive_webm(), "recursive_container").await
}

#[tokio::test]
async fn excessive_mp4_box_count_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::excessive_mp4_boxes(), "structure_limit").await
}

#[tokio::test]
async fn excessive_webm_element_count_is_a_typed_bounded_failure() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::excessive_webm_elements(), "structure_limit").await
}

#[tokio::test]
async fn zero_mp4_timescale_is_rejected_before_duration_output() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::zero_timescale_mp4(), "malformed_video").await
}

#[tokio::test]
async fn nonfinite_webm_duration_is_rejected_before_output() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::nonfinite_duration_webm(), "malformed_video").await
}

#[tokio::test]
async fn unknown_bytes_remain_a_typed_unknown_inspection() -> Result<(), Box<dyn Error>> {
    let source = MemorySource::unknown(b"not video".to_vec())?;
    let inspection =
        inspect_as(&DirectProcessor::new(), &source, "application/octet-stream").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn declared_video_type_with_unrecognized_bytes_remains_unknown() -> Result<(), Box<dyn Error>>
{
    let source = MemorySource::unknown(b"not video".to_vec())?;
    let inspection = inspect_as(&DirectProcessor::new(), &source, "video/mp4").await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn ordinary_large_mp4_validates_from_the_bounded_metadata_prefix()
-> Result<(), Box<dyn Error>> {
    let source = VideoFixture::ordinary_large_mp4().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn partial_mp4_header_at_metadata_cutoff_is_an_accepted_truncated_tail()
-> Result<(), Box<dyn Error>> {
    let source = VideoFixture::large_mp4_with_partial_header_at_metadata_cutoff().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn partial_mp4_extended_header_at_metadata_cutoff_is_an_accepted_truncated_tail()
-> Result<(), Box<dyn Error>> {
    let source =
        VideoFixture::large_mp4_with_partial_extended_header_at_metadata_cutoff().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn header_only_avc1_sample_entry_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::header_only_avc1_mp4(), "malformed_video").await
}

#[tokio::test]
async fn unsupported_iso_bmff_brand_is_not_claimed_as_mp4() -> Result<(), Box<dyn Error>> {
    let source = VideoFixture::unsupported_brand_mp4().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn matroska_doctype_is_not_claimed_as_webm() -> Result<(), Box<dyn Error>> {
    let source = VideoFixture::matroska_ebml().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Unknown);
    Ok(())
}

#[tokio::test]
async fn encryption_like_bytes_inside_clear_mp4_payload_are_not_locked()
-> Result<(), Box<dyn Error>> {
    let source = VideoFixture::clear_mp4_with_encryption_like_payload_bytes().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn truncated_mandatory_mp4_movie_header_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(VideoFixture::truncated_mvhd_mp4(), "malformed_video").await
}

#[tokio::test]
async fn duplicate_webm_timestamp_scale_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::duplicate_timestamp_scale_webm(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn iso6_mp4_brand_validates() -> Result<(), Box<dyn Error>> {
    let source = VideoFixture::mp4_with_iso6_brand().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn space_padded_mp4_brand_validates() -> Result<(), Box<dyn Error>> {
    let source = VideoFixture::mp4_with_space_padded_brand().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn hevc_mp4_video_sample_entry_validates() -> Result<(), Box<dyn Error>> {
    let source = VideoFixture::hevc_mp4().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn truncated_hevc_configuration_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::hevc_mp4_with_truncated_configuration(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn duplicate_mp4_handlers_are_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::mp4_media_with_duplicate_handlers(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn unsupported_ebml_read_version_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::webm_with_unsupported_ebml_read_version(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn unsupported_webm_doctype_read_version_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::webm_with_unsupported_doctype_read_version(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn webm_video_track_with_audio_codec_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::webm_video_track_with_audio_codec(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn webm_track_without_mandatory_fields_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::webm_track_missing_number_and_codec(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn duplicate_webm_track_numbers_are_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::webm_with_duplicate_track_numbers(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn durationless_webm_validates_with_unavailable_duration() -> Result<(), Box<dyn Error>> {
    let fixture = VideoFixture::durationless_webm();
    let (inspection, body) = inspect_and_read_metadata(fixture).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    assert_eq!(body["duration_milliseconds"], serde_json::Value::Null);
    Ok(())
}

#[tokio::test]
async fn unknown_sized_final_webm_cluster_is_permitted() -> Result<(), Box<dyn Error>> {
    let source = VideoFixture::webm_with_unknown_sized_final_cluster().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn fragmented_mp4_uses_movie_extends_duration() -> Result<(), Box<dyn Error>> {
    let fixture = VideoFixture::fragmented_mp4_with_movie_extends_duration();
    let expected_duration = fixture.expected_duration_milliseconds();
    let (inspection, body) = inspect_and_read_metadata(fixture).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    assert_eq!(body["duration_milliseconds"], expected_duration);
    Ok(())
}

#[tokio::test]
async fn fragmented_mp4_without_movie_extends_duration_reports_unavailable_duration()
-> Result<(), Box<dyn Error>> {
    let fixture = VideoFixture::fragmented_mp4_without_movie_extends_duration();
    let (inspection, body) = inspect_and_read_metadata(fixture).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    assert_eq!(body["duration_milliseconds"], serde_json::Value::Null);
    Ok(())
}

#[tokio::test]
async fn mp4_video_track_without_sample_description_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::mp4_video_track_without_sample_description(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn mp4_track_with_split_media_evidence_is_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::mp4_track_with_split_media_evidence(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn partial_webm_header_at_metadata_cutoff_is_an_accepted_truncated_tail()
-> Result<(), Box<dyn Error>> {
    let source = VideoFixture::large_webm_with_partial_header_at_metadata_cutoff().into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Validated);
    Ok(())
}

#[tokio::test]
async fn duplicate_webm_tracks_elements_are_malformed() -> Result<(), Box<dyn Error>> {
    assert_malformed(
        VideoFixture::webm_with_duplicate_tracks_elements(),
        "malformed_video",
    )
    .await
}

#[tokio::test]
async fn hostile_view_arguments_are_typed_and_content_silent() -> Result<(), Box<dyn Error>> {
    let source = VideoFixture::ordinary_mp4().into_source()?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({"frame": "../../host"}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::InvalidViewArguments));
    Ok(())
}

#[tokio::test]
async fn adversarial_decoder_output_kind_is_rejected_by_registry_sanitization()
-> Result<(), Box<dyn Error>> {
    let source = VideoFixture::ordinary_webm().into_source()?;
    let result = read(
        &AdversarialOutputProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await;

    assert_eq!(result, Err(FileMediaFailure::ProcessorFailed));
    Ok(())
}

async fn inspect_and_read_metadata(
    fixture: VideoFixture,
) -> Result<(FileInspection, serde_json::Value), Box<dyn Error>> {
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;
    let result = read(
        &DirectProcessor::new(),
        &source,
        "metadata",
        serde_json::json!({}),
    )
    .await?;
    let body = complete_structure(result)?;
    Ok((inspection, body))
}

async fn assert_locked(fixture: VideoFixture) -> Result<(), Box<dyn Error>> {
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::EncryptedOrLocked);
    Ok(())
}

async fn assert_malformed(fixture: VideoFixture, reason: &str) -> Result<(), Box<dyn Error>> {
    let source = fixture.into_source()?;
    let inspection = inspect(&DirectProcessor::new(), &source).await?;

    assert_eq!(inspection.status(), FileInspectionStatus::Malformed);
    assert_eq!(malformed_reason(&inspection)?, reason);
    Ok(())
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

async fn inspect_as(
    processor: &dyn FileMediaProcessor,
    source: &MemorySource,
    declared_media_type: &str,
) -> Result<FileInspection, FileMediaFailure> {
    let request = InspectionRequest {
        source: source
            .file_use_as(declared_media_type)
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
    view: &str,
    options: serde_json::Value,
) -> Result<FileReadResult, FileMediaFailure> {
    let request = FileReadRequest {
        inspection: InspectionRequest {
            source: source
                .file_use()
                .map_err(|_| FileMediaFailure::ProcessorFailed)?,
            visible_part: None,
        },
        view: ReadViewName::try_new(view).map_err(|_| FileMediaFailure::ProcessorFailed)?,
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
        _ => Err("expected malformed video".into()),
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

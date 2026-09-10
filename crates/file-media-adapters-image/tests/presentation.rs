//! Real worker evidence for bounded direct and derived image presentation.

#[allow(dead_code)]
mod fixtures;

use signalbox_file_media_processor_runtime::{SandboxedFileMediaProcessor, WorkerBinding};
use signalbox_file_media_runtime::*;
use std::{
    error::Error,
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

struct StreamedImage {
    prefix: Vec<u8>,
    length: NonZeroU64,
    requested: AtomicU64,
    maximum: AtomicU64,
}
impl VerifiedBlobSource for StreamedImage {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([7; 32])
    }
    fn byte_length(&self) -> NonZeroU64 {
        self.length
    }
    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
        Box::pin(async move {
            if length.get() > MAX_PROBE_PREFIX_BYTES
                || offset
                    .checked_add(length.get())
                    .is_none_or(|end| end > self.length.get())
            {
                return Err(SourceReadError::RangeOutOfBounds);
            }
            self.requested.fetch_add(length.get(), Relaxed);
            self.maximum.fetch_max(length.get(), Relaxed);
            let mut section = vec![0; length.get() as usize];
            for (index, byte) in section.iter_mut().enumerate() {
                *byte = self
                    .prefix
                    .get(offset as usize + index)
                    .copied()
                    .unwrap_or(0);
            }
            Ok(section)
        })
    }
}
fn request(
    source: &StreamedImage,
    view: &str,
    options: serde_json::Value,
) -> Result<FileReadRequest, Box<dyn Error>> {
    Ok(FileReadRequest {
        inspection: InspectionRequest {
            source: FileUse::new(
                source.digest(),
                source.byte_length(),
                AttachmentKind::Image,
                DeclaredMediaType::try_new("image/png")?,
                None,
            ),
            visible_part: None,
        },
        view: ReadViewName::try_new(view)?,
        input: FileReadInput::Initial { options },
    })
}
async fn composed() -> Result<(FileMediaRegistry, SandboxedFileMediaProcessor), Box<dyn Error>> {
    let declaration = signalbox_file_media_adapters_image::image_family_declaration()
        .map_err(|_| "declaration")?;
    let worker = std::env::var_os("NEXTEST_BIN_EXE_signalbox_file_media_image_worker")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_BIN_EXE_signalbox-file-media-image-worker"))
        });
    let processor = SandboxedFileMediaProcessor::try_new(
        "/usr/bin/bwrap",
        vec![WorkerBinding::try_new(worker, declaration.clone())?],
        FileMediaProcessCeilings::version_one(),
    )?;
    assert_eq!(
        processor.verify_isolation().await,
        ProcessorIsolation::Available
    );
    Ok((
        FileMediaRegistry::try_new(
            vec![declaration],
            FileMediaCeilings::version_one(),
            ProcessorIsolation::Available,
        )?,
        processor,
    ))
}

#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn file_inspect_and_read_describe_five_gibibytes_under_the_worker_memory_limit()
-> Result<(), Box<dyn Error>> {
    let source = StreamedImage {
        prefix: fixtures::valid(fixtures::FixtureFormat::Png)?,
        length: NonZeroU64::new(5 * 1024 * 1024 * 1024).unwrap(),
        requested: AtomicU64::new(0),
        maximum: AtomicU64::new(0),
    };
    assert!(FileMediaProcessCeilings::version_one().memory_bytes() < source.length.get());
    let (registry, processor) = composed().await?;
    let request = request(&source, "image", serde_json::json!({}))?;
    let inspected = registry
        .inspect(
            &processor,
            request.inspection.clone(),
            &source,
            &NeverCancelled,
        )
        .await?;
    assert!(matches!(inspected, FileInspection::Validated(_)));
    let result = registry
        .read(&processor, request, &source, &NeverCancelled)
        .await?;
    let FileReadResult::Structured { body, .. } = result else {
        panic!("large source must be described")
    };
    assert_eq!(body["status"], "large_image");
    assert_eq!(body["width"], 3);
    assert_eq!(body["height"], 2);
    assert!(body.to_string().len() < 1024);
    assert!(source.maximum.load(Relaxed) <= MAX_PROBE_PREFIX_BYTES);
    assert!(source.requested.load(Relaxed) < 1024 * 1024);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the delegated real file-media sandbox profile"]
async fn file_read_direct_and_deterministic_crop_use_the_isolated_binary_channel()
-> Result<(), Box<dyn Error>> {
    let prefix = fixtures::valid(fixtures::FixtureFormat::Png)?;
    let source = StreamedImage {
        length: NonZeroU64::new(prefix.len() as u64).unwrap(),
        prefix,
        requested: AtomicU64::new(0),
        maximum: AtomicU64::new(0),
    };
    let (registry, processor) = composed().await?;
    let direct = registry
        .read(
            &processor,
            request(&source, "image", serde_json::json!({}))?,
            &source,
            &NeverCancelled,
        )
        .await?;
    let FileReadResult::Reference(direct) = direct else {
        panic!("direct image reference")
    };
    assert_eq!(direct.presented(), direct.source());
    let mut previous = None;
    for _ in 0..2 {
        let (_, prepared) = registry
            .prepare_read_with_reader(
                &processor,
                request(
                    &source,
                    "crop",
                    serde_json::json!({"x":1,"y":0,"width":2,"height":1}),
                )?,
                &source,
                &NeverCancelled,
                None,
            )
            .await?;
        let PreparedFileRead::Generated(generated) = prepared else {
            panic!("generated crop")
        };
        let validated = registry
            .validate_generated(&processor, generated, &NeverCancelled)
            .await?;
        assert_eq!(validated.reference().source().digest(), source.digest());
        assert_ne!(validated.reference().presented().digest(), source.digest());
        let decoded = image::load_from_memory(validated.bytes())?;
        assert_eq!((decoded.width(), decoded.height()), (2, 1));
        if let Some(previous) = &previous {
            assert_eq!(validated.bytes(), previous);
        }
        previous = Some(validated.bytes().to_vec());
    }
    Ok(())
}

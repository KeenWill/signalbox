use super::*;
use signalbox_file_media_runtime::*;
use signalbox_tools_file_media::{FileReadServiceInput, FileReadServiceRequest};
use std::{
    collections::BTreeMap,
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
};

struct Source;
impl VerifiedBlobSource for Source {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([1; 32])
    }
    fn byte_length(&self) -> NonZeroU64 {
        NonZeroU64::MIN
    }
    fn read_range(&self, _: u64, _: NonZeroU64) -> SourceReadFuture<'_> {
        panic!("the authority fixture must perform no source I/O")
    }
}

struct Resolver {
    visible: Arc<AtomicBool>,
}
impl FileUseResolver for Resolver {
    type Source = Source;
    fn resolve(&mut self, request: FileInspectServiceRequest) -> FileUseResolverFuture<'_, Source> {
        Box::pin(async move {
            if !self.visible.load(Relaxed) || request.digest() != Source.digest() {
                return Err(FileUseResolutionError::BlobNotVisible);
            }
            let selected = request
                .visible_part()
                .ok_or(FileUseResolutionError::BlobNotVisible)?;
            if !["entry_0", "entry_1"].contains(&selected.as_str()) {
                return Err(FileUseResolutionError::BlobNotVisible);
            }
            Ok(ResolvedFileUse::new(
                FileUse::new(
                    Source.digest(),
                    NonZeroU64::MIN,
                    AttachmentKind::File,
                    DeclaredMediaType::try_new("text/plain").expect("fixture media type"),
                    None,
                ),
                Source,
                selected.clone(),
            ))
        })
    }
}

struct Processor {
    calls: Arc<AtomicUsize>,
}
impl FileMediaProcessor for Processor {
    fn probe<'a>(
        &'a self,
        _: &'a ReaderIdentity,
        _: &'a dyn VerifiedBlobSource,
        _: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        self.calls.fetch_add(1, Relaxed);
        Box::pin(async {
            Ok(ProcessorProbeOutput::Candidate {
                media_type: "text/plain".into(),
                strength: ProbeStrength::Strong,
                evidence_bytes: 1,
            })
        })
    }
    fn validate<'a>(
        &'a self,
        _: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        _: &'a dyn VerifiedBlobSource,
        _: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            Ok(ProcessorValidationOutput::Validated {
                media_type: "text/plain".into(),
                evidence: request.evidence,
                metadata_json: "{}".into(),
            })
        })
    }
    fn read<'a>(
        &'a self,
        _: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        _: &'a dyn VerifiedBlobSource,
        _: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        Box::pin(async move {
            let marker = match request.input {
                FileReadInput::Initial { options } => {
                    options["marker"].as_str().unwrap_or_default().to_owned()
                }
                FileReadInput::Continuation { cursor } => cursor.as_str().to_owned(),
            };
            Ok(ProcessorReadOutput::Text {
                body: marker.clone(),
                truncated: true,
                cursor: Some(marker),
            })
        })
    }
}

fn registry() -> FileMediaRegistry {
    let provider = FileReaderProviderName::try_new("fixture").unwrap();
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new("text").unwrap(),
        revision: FileReaderRevision::try_new("v1").unwrap(),
        media_types: vec!["text/plain".parse().unwrap()],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 1,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: 1,
        }),
        validation: ValidationDeclaration::new(1, 1),
        views: vec![
            ReadViewDeclaration::try_new(
                ReadViewName::try_new("text").unwrap(),
                "fixture text".into(),
                CanonicalJsonObjectSchema::try_new(r#"{"type":"object"}"#).unwrap(),
                ReadAccessPattern::Streaming { maximum_ranges: 1 },
                ReadViewBounds::Text {
                    source_bytes: 1,
                    output_bytes: 64,
                },
            )
            .unwrap(),
        ],
        reason_codes: vec![ReasonCode::try_new("malformed").unwrap()],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })
    .unwrap();
    FileMediaRegistry::try_new(
        vec![FileMediaProviderDeclaration::try_new(provider, vec![reader]).unwrap()],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )
    .unwrap()
}

fn request(selector: &str, input: FileReadServiceInput) -> FileReadServiceRequest {
    FileReadServiceRequest::from_parts(
        FileInspectServiceRequest::from_parts(
            Source.digest(),
            Some(VisiblePartSelector::try_new(selector).unwrap()),
        ),
        ReadViewName::try_new("text").unwrap(),
        input,
    )
}

#[tokio::test]
async fn continuation_preserves_options_and_cannot_select_another_visible_use() {
    let visible = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut service = RegistryFileMediaAgentService::new(
        registry(),
        Resolver {
            visible: visible.clone(),
        },
        Processor {
            calls: calls.clone(),
        },
        NeverCancelled,
        ContinuationAuthority::generate().unwrap(),
    );
    let first = service
        .read(request(
            "entry_0",
            FileReadServiceInput::Initial {
                options: BTreeMap::from([("marker".into(), serde_json::json!("chosen"))]),
            },
        ))
        .await
        .unwrap();
    let FileReadResult::Text {
        body,
        continuation: ReadContinuation::More { cursor },
    } = first
    else {
        panic!("fixture first page has continuation")
    };
    assert_eq!(body, "chosen");
    let wrong_use = service
        .read(request(
            "entry_1",
            FileReadServiceInput::Continuation {
                cursor: cursor.clone(),
            },
        ))
        .await;
    assert_eq!(
        wrong_use,
        Err(FileMediaFailure::InvalidViewArguments.into())
    );
    assert_eq!(calls.load(Relaxed), 1);
    let second = service
        .read(request(
            "entry_0",
            FileReadServiceInput::Continuation {
                cursor: cursor.clone(),
            },
        ))
        .await
        .unwrap();
    let FileReadResult::Text { body, .. } = second else {
        panic!("fixture second page is text")
    };
    assert_eq!(body, "chosen");
    visible.store(false, Relaxed);
    let hidden = service
        .read(request(
            "entry_0",
            FileReadServiceInput::Continuation { cursor },
        ))
        .await;
    assert_eq!(hidden, Err(FileMediaFailure::BlobNotVisible.into()));
    assert_eq!(calls.load(Relaxed), 2);
}

#[tokio::test]
async fn invisible_digest_and_unselected_repeated_use_never_reach_the_processor() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut service = RegistryFileMediaAgentService::new(
        registry(),
        Resolver {
            visible: Arc::new(AtomicBool::new(true)),
        },
        Processor {
            calls: calls.clone(),
        },
        NeverCancelled,
        ContinuationAuthority::generate().unwrap(),
    );
    let hidden = service
        .inspect(FileInspectServiceRequest::from_parts(
            FileDigest::from_bytes([2; 32]),
            Some(VisiblePartSelector::try_new("entry_0").unwrap()),
        ))
        .await;
    assert_eq!(hidden, Err(FileMediaFailure::BlobNotVisible.into()));
    let repeated = service
        .inspect(FileInspectServiceRequest::from_parts(Source.digest(), None))
        .await;
    assert_eq!(repeated, Err(FileMediaFailure::BlobNotVisible.into()));
    assert_eq!(calls.load(Relaxed), 0);
}

#[test]
fn continuation_requires_the_issuing_process_key_and_unchanged_authenticated_state() {
    let key = ContinuationAuthority::generate().unwrap();
    let other = ContinuationAuthority::generate().unwrap();
    let state = ContinuationState::new(
        &FileUse::new(
            Source.digest(),
            NonZeroU64::MIN,
            AttachmentKind::File,
            DeclaredMediaType::try_new("text/plain").unwrap(),
            None,
        ),
        &VisiblePartSelector::try_new("entry_0").unwrap(),
        &ReadViewName::try_new("text").unwrap(),
        registry().providers()[0].readers()[0].identity(),
        BTreeMap::new(),
        &ReadContinuationCursor::try_new("section_1").unwrap(),
    );
    let sealed = key.seal(&state).unwrap();
    assert!(key.open(&sealed).is_ok());
    assert!(matches!(
        other.open(&sealed),
        Err(FileMediaFailure::InvalidViewArguments)
    ));
    let tampered = ReadContinuationCursor::try_new(format!("x{}", sealed.as_str())).unwrap();
    assert!(matches!(
        key.open(&tampered),
        Err(FileMediaFailure::InvalidViewArguments)
    ));
}

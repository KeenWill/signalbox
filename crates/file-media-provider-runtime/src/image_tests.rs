use super::*;
use signalbox_file_media_runtime::*;
use signalbox_tools_file_media::{FileReadServiceInput, FileReadServiceRequest};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
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
        Box::pin(async { Err(SourceReadError::Unavailable) })
    }
}
struct Resolver {
    bound: u64,
    document: bool,
}
impl FileUseResolver for Resolver {
    type Source = Source;
    fn resolve(&mut self, _: FileInspectServiceRequest) -> FileUseResolverFuture<'_, Source> {
        Box::pin(async move {
            Ok(ResolvedFileUse::new(
                FileUse::new(
                    Source.digest(),
                    Source.byte_length(),
                    if self.document {
                        AttachmentKind::Document
                    } else {
                        AttachmentKind::Image
                    },
                    DeclaredMediaType::try_new(if self.document {
                        "application/pdf"
                    } else {
                        "image/png"
                    })
                    .unwrap(),
                    None,
                ),
                Source,
                VisiblePartSelector::try_new("fixture").unwrap(),
            )
            .with_image_target(Some(
                signalbox_model_runtime::ImagePresentationCapability::new(
                    ["image/png".into()].into(),
                    NonZeroU64::new(self.bound.max(1)).unwrap(),
                    NonZeroUsize::new(1024).unwrap(),
                    64,
                ),
            ))
            .with_document_target(if self.document && self.bound > 0 {
                Some(
                    signalbox_model_runtime::DocumentPresentationCapability::new(
                        ["application/pdf".into()].into(),
                        NonZeroU64::new(self.bound).unwrap(),
                        NonZeroUsize::new(1024).unwrap(),
                        64,
                    ),
                )
            } else {
                None
            }))
        })
    }
}
struct Processor {
    invalid_generated: bool,
    document: bool,
}
impl FileMediaProcessor for Processor {
    fn probe<'a>(
        &'a self,
        _: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        _: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            if self.invalid_generated && source.digest() != Source.digest() {
                return Ok(ProcessorProbeOutput::NoMatch);
            }
            Ok(ProcessorProbeOutput::Candidate {
                media_type: if self.document {
                    "application/pdf".into()
                } else {
                    "image/png".into()
                },
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
                media_type: if self.document {
                    "application/pdf".into()
                } else {
                    "image/png".into()
                },
                evidence: request.evidence,
                metadata_json: r#"{"width":1,"height":1}"#.into(),
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
            Ok(if matches!(request.view.as_str(), "image" | "document") {
                ProcessorReadOutput::DirectReference {
                    media_type: if self.document {
                        "application/pdf".into()
                    } else {
                        "image/png".into()
                    },
                }
            } else {
                ProcessorReadOutput::GeneratedImage {
                    media_type: if self.document {
                        "application/pdf".into()
                    } else {
                        "image/png".into()
                    },
                    provider: "fixture".into(),
                    reader: "png".into(),
                    revision: "v1".into(),
                    byte_length: 3,
                    bytes: vec![1, 2, 3],
                }
            })
        })
    }
}
fn registry(document: bool) -> FileMediaRegistry {
    let provider = FileReaderProviderName::try_new("fixture").unwrap();
    let mut views: Vec<_> = [
        ("image", ImageViewKind::Direct),
        ("crop", ImageViewKind::Generated),
    ]
    .into_iter()
    .map(|(name, kind)| {
        ReadViewDeclaration::try_new(
            ReadViewName::try_new(name).unwrap(),
            "fixture".into(),
            CanonicalJsonObjectSchema::try_new(r#"{"type":"object"}"#).unwrap(),
            ReadAccessPattern::Streaming { maximum_ranges: 1 },
            ReadViewBounds::Image {
                source_bytes: 64,
                width: 1,
                height: 1,
                pixels: 1,
                output_bytes: 64,
            },
        )
        .unwrap()
        .with_image_output(kind, vec!["image/png".parse().unwrap()])
    })
    .collect();
    if document {
        views = vec![
            ReadViewDeclaration::try_new(
                ReadViewName::try_new("document").unwrap(),
                "Native PDF".into(),
                CanonicalJsonObjectSchema::try_new(r#"{"type":"object"}"#).unwrap(),
                ReadAccessPattern::Streaming { maximum_ranges: 1 },
                ReadViewBounds::File {
                    source_bytes: 64,
                    output_bytes: 64,
                },
            )
            .unwrap(),
        ];
    }
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new("png").unwrap(),
        revision: FileReaderRevision::try_new("v1").unwrap(),
        media_types: vec![
            if document {
                "application/pdf"
            } else {
                "image/png"
            }
            .parse()
            .unwrap(),
        ],
        probe: ProbeDeclaration::prefix_only(1),
        validation: ValidationDeclaration::new(64, 1),
        views,
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
#[derive(Debug)]
struct Publisher {
    calls: Arc<AtomicUsize>,
    fails: bool,
}
impl FileMediaArtifactPublisher for Publisher {
    fn publish<'a>(
        &'a self,
        _: &'a ValidatedMediaArtifact,
    ) -> Pin<Box<dyn Future<Output = Result<(), FileMediaServiceFailure>> + Send + 'a>> {
        self.calls.fetch_add(1, Relaxed);
        Box::pin(async move {
            if self.fails {
                Err(FileMediaFailure::BlobUnavailable.into())
            } else {
                Ok(())
            }
        })
    }
}
fn request(view: &str) -> FileReadServiceRequest {
    FileReadServiceRequest::from_parts(
        FileInspectServiceRequest::from_parts(
            Source.digest(),
            Some(VisiblePartSelector::try_new("fixture").unwrap()),
        ),
        ReadViewName::try_new(view).unwrap(),
        FileReadServiceInput::Initial {
            options: std::collections::BTreeMap::new(),
        },
    )
}
#[tokio::test]
async fn invalid_or_oversized_derived_bytes_never_reach_publication() {
    for (invalid_generated, bound) in [(true, 64), (false, 2)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut service = RegistryFileMediaAgentService::new(
            registry(false),
            Resolver {
                bound,
                document: false,
            },
            Processor {
                invalid_generated,
                document: false,
            },
            NeverCancelled,
            ContinuationAuthority::generate().unwrap(),
        )
        .with_artifact_publisher(Arc::new(Publisher {
            calls: calls.clone(),
            fails: false,
        }));
        assert!(service.read(request("crop")).await.is_err());
        assert_eq!(calls.load(Relaxed), 0);
    }
}
#[tokio::test]
async fn generated_reference_requires_successful_publication_and_direct_reference_does_not_publish()
{
    for fails in [true, false] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut service = RegistryFileMediaAgentService::new(
            registry(false),
            Resolver {
                bound: 64,
                document: false,
            },
            Processor {
                invalid_generated: false,
                document: false,
            },
            NeverCancelled,
            ContinuationAuthority::generate().unwrap(),
        )
        .with_artifact_publisher(Arc::new(Publisher {
            calls: calls.clone(),
            fails,
        }));
        assert!(matches!(
            service.read(request("image")).await,
            Ok(FileReadResult::Reference(_))
        ));
        assert_eq!(calls.load(Relaxed), 0);
        let result = service.read(request("crop")).await;
        assert_eq!(result.is_err(), fails);
        assert_eq!(calls.load(Relaxed), 1);
    }
}

#[tokio::test]
async fn native_documents_require_their_own_target_capability_and_do_not_publish() {
    for bound in [0, 64] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut service = RegistryFileMediaAgentService::new(
            registry(true),
            Resolver {
                bound,
                document: true,
            },
            Processor {
                invalid_generated: false,
                document: true,
            },
            NeverCancelled,
            ContinuationAuthority::generate().unwrap(),
        )
        .with_artifact_publisher(Arc::new(Publisher {
            calls: calls.clone(),
            fails: true,
        }));
        let result = service.read(request("document")).await;
        if bound == 0 {
            assert!(matches!(
                result,
                Err(FileMediaServiceFailure::File(
                    FileMediaFailure::UnsupportedView
                ))
            ));
        } else {
            let FileReadResult::Reference(reference) = result.unwrap() else {
                panic!("document reference");
            };
            assert_eq!(reference.kind(), MediaPresentationKind::Document);
            assert_eq!(reference.presented(), reference.source());
        }
        assert_eq!(calls.load(Relaxed), 0);
    }
}

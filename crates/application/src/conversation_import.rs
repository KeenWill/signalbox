//! Source-neutral conversation-ingestion orchestration.
//!
//! Format adapters implement [`ImportedConversationConverter`]; persistence
//! adapters implement [`ImportedConversationStore`]. The application supplies
//! hub identities and performs one complete resolve-or-insert operation.

use std::{collections::BTreeSet, error::Error, fmt, future::Future};

use signalbox_domain::{
    ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedConversationSourceDigest, ImportedTranscriptEntryId,
};

/// Application effect supplying fresh imported-record identities.
pub trait ImportedConversationIdGenerator {
    /// Generates one imported-conversation candidate.
    fn next_conversation_id(&mut self) -> ImportedConversationId;

    /// Generates one imported-entry candidate.
    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId;
}

/// Production UUIDv7 imported-record identity generator.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7ImportedConversationIdGenerator;

impl ImportedConversationIdGenerator for UuidV7ImportedConversationIdGenerator {
    fn next_conversation_id(&mut self) -> ImportedConversationId {
        ImportedConversationId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
        ImportedTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Format-versioned edge conversion into one source-neutral domain aggregate.
pub trait ImportedConversationConverter {
    /// Typed source-format parse or conversion failure.
    type Error;

    /// Returns the exact source family and converter version implemented.
    fn format(&self) -> ImportedConversationFormat;

    /// Converts source bytes using only caller-supplied hub identities.
    fn convert<NextEntryId>(
        &mut self,
        conversation: ImportedConversationId,
        source: &[u8],
        next_entry_id: NextEntryId,
    ) -> Result<ImportedConversation, Self::Error>
    where
        NextEntryId: FnMut() -> ImportedTranscriptEntryId;
}

/// Checked result of one append-only store resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedConversationStoreOutcome {
    /// The candidate aggregate became the new durable snapshot.
    Inserted {
        /// Newly durable candidate identity.
        conversation: ImportedConversationId,
        /// Durable ordered-source digest.
        source_digest: ImportedConversationSourceDigest,
    },
    /// The exact format and ordered source were already durable.
    AlreadyImported {
        /// Previously durable aggregate identity.
        conversation: ImportedConversationId,
        /// Previously durable ordered-source digest.
        source_digest: ImportedConversationSourceDigest,
    },
}

impl ImportedConversationStoreOutcome {
    /// Returns the newly or previously durable imported conversation.
    pub const fn conversation(self) -> ImportedConversationId {
        match self {
            Self::Inserted { conversation, .. } | Self::AlreadyImported { conversation, .. } => {
                conversation
            }
        }
    }

    /// Returns the checked durable source digest.
    pub const fn source_digest(self) -> ImportedConversationSourceDigest {
        match self {
            Self::Inserted { source_digest, .. } | Self::AlreadyImported { source_digest, .. } => {
                source_digest
            }
        }
    }
}

/// Atomic append-only store boundary for one complete imported conversation.
pub trait ImportedConversationStore {
    /// Adapter-specific infrastructure, collision, or integrity failure.
    type Error;

    /// Inserts a new snapshot or resolves its exact durable duplicate.
    fn resolve_or_insert(
        &mut self,
        conversation: ImportedConversation,
    ) -> impl Future<Output = Result<ImportedConversationStoreOutcome, Self::Error>> + Send;
}

/// Successful pure-ingestion outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportConversationOutcome {
    /// A new immutable imported conversation was inserted.
    Inserted {
        /// Newly durable candidate identity.
        conversation: ImportedConversationId,
    },
    /// Exact reingestion resolved an existing immutable conversation.
    AlreadyImported {
        /// Previously durable aggregate identity.
        conversation: ImportedConversationId,
    },
}

impl ImportConversationOutcome {
    /// Returns the newly or previously durable imported conversation.
    pub const fn conversation(self) -> ImportedConversationId {
        match self {
            Self::Inserted { conversation } | Self::AlreadyImported { conversation } => {
                conversation
            }
        }
    }
}

/// Conversation-ingestion orchestration failure.
#[derive(Debug, Eq, PartialEq)]
pub enum ImportConversationError<ConverterError, StoreError> {
    /// The source converter rejected the complete input.
    Conversion(ConverterError),
    /// The converter returned an aggregate under another hub identity.
    ConverterIdentityMismatch {
        /// The hub identity supplied to the converter.
        supplied: ImportedConversationId,
        /// The identity carried by the converted aggregate.
        converted: ImportedConversationId,
    },
    /// The converter returned a format other than the one it declares.
    ConverterFormatMismatch {
        /// The converter's declared format.
        declared: ImportedConversationFormat,
        /// The format carried by the converted aggregate.
        converted: ImportedConversationFormat,
    },
    /// The converter returned an entry identity not issued by the hub callback.
    ConverterEntryIdentityNotIssued {
        /// Unissued identity carried by the converted aggregate.
        entry: ImportedTranscriptEntryId,
    },
    /// The store reported a digest other than the converted exact source.
    StoreSourceDigestMismatch {
        /// The converted aggregate digest.
        expected: ImportedConversationSourceDigest,
        /// The store-reported digest.
        actual: ImportedConversationSourceDigest,
    },
    /// A newly inserted store result named another aggregate identity.
    StoreInsertedIdentityMismatch {
        /// Candidate identity carried by the converted aggregate.
        expected: ImportedConversationId,
        /// Store-reported inserted identity.
        actual: ImportedConversationId,
    },
    /// The append-only store could not resolve or insert the aggregate.
    Store(StoreError),
}

impl<ConverterError, StoreError> fmt::Display
    for ImportConversationError<ConverterError, StoreError>
where
    ConverterError: fmt::Display,
    StoreError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conversion(error) => write!(formatter, "conversation conversion failed: {error}"),
            Self::ConverterIdentityMismatch {
                supplied,
                converted,
            } => write!(
                formatter,
                "conversation converter identity mismatch: supplied {supplied:?}, converted {converted:?}"
            ),
            Self::ConverterFormatMismatch {
                declared,
                converted,
            } => write!(
                formatter,
                "conversation converter format mismatch: declared {declared:?}, converted {converted:?}"
            ),
            Self::ConverterEntryIdentityNotIssued { entry } => write!(
                formatter,
                "conversation converter returned an unissued entry identity: {entry:?}"
            ),
            Self::StoreSourceDigestMismatch { expected, actual } => write!(
                formatter,
                "conversation store source-digest mismatch: expected {expected:?}, actual {actual:?}"
            ),
            Self::StoreInsertedIdentityMismatch { expected, actual } => write!(
                formatter,
                "conversation store inserted-identity mismatch: expected {expected:?}, actual {actual:?}"
            ),
            Self::Store(error) => write!(formatter, "conversation import store failed: {error}"),
        }
    }
}

impl<ConverterError, StoreError> Error for ImportConversationError<ConverterError, StoreError>
where
    ConverterError: Error + 'static,
    StoreError: Error + 'static,
{
}

/// Coordinates one conversion and idempotent append-only ingestion.
#[derive(Debug)]
pub struct ImportConversationService<Generator, Converter, Store> {
    ids: Generator,
    converter: Converter,
    store: Store,
}

impl<Generator, Converter, Store> ImportConversationService<Generator, Converter, Store> {
    /// Composes identity, conversion, and storage ports.
    pub const fn new(ids: Generator, converter: Converter, store: Store) -> Self {
        Self {
            ids,
            converter,
            store,
        }
    }

    /// Returns all ports, primarily for explicit ownership handoff.
    pub fn into_parts(self) -> (Generator, Converter, Store) {
        (self.ids, self.converter, self.store)
    }
}

impl<Generator, Converter, Store> ImportConversationService<Generator, Converter, Store>
where
    Generator: ImportedConversationIdGenerator,
    Converter: ImportedConversationConverter,
    Store: ImportedConversationStore,
{
    /// Converts once and resolves or inserts one complete checked aggregate.
    ///
    /// The service performs no retry and no session, command, scheduler, or
    /// outbox effect. Candidate identities consumed by conversion or exact
    /// duplicate resolution are simply discarded.
    pub async fn execute(
        &mut self,
        source: &[u8],
    ) -> Result<ImportConversationOutcome, ImportConversationError<Converter::Error, Store::Error>>
    {
        let Self {
            ids,
            converter,
            store,
        } = self;
        let candidate = ids.next_conversation_id();
        let declared = converter.format();
        let mut issued_entries = BTreeSet::new();
        let converted = converter
            .convert(candidate, source, || {
                let entry = ids.next_entry_id();
                issued_entries.insert(entry);
                entry
            })
            .map_err(ImportConversationError::Conversion)?;
        if converted.id() != candidate {
            return Err(ImportConversationError::ConverterIdentityMismatch {
                supplied: candidate,
                converted: converted.id(),
            });
        }
        if converted.format() != declared {
            return Err(ImportConversationError::ConverterFormatMismatch {
                declared,
                converted: converted.format(),
            });
        }
        if let Some(unissued) = converted
            .entries()
            .iter()
            .map(|entry| entry.identity())
            .find(|entry| !issued_entries.contains(entry))
        {
            return Err(ImportConversationError::ConverterEntryIdentityNotIssued {
                entry: unissued,
            });
        }
        let expected_digest = converted.source_digest();
        let stored = store
            .resolve_or_insert(converted)
            .await
            .map_err(ImportConversationError::Store)?;
        if stored.source_digest() != expected_digest {
            return Err(ImportConversationError::StoreSourceDigestMismatch {
                expected: expected_digest,
                actual: stored.source_digest(),
            });
        }
        match stored {
            ImportedConversationStoreOutcome::Inserted { conversation, .. } => {
                if conversation != candidate {
                    return Err(ImportConversationError::StoreInsertedIdentityMismatch {
                        expected: candidate,
                        actual: conversation,
                    });
                }
                Ok(ImportConversationOutcome::Inserted { conversation })
            }
            ImportedConversationStoreOutcome::AlreadyImported { conversation, .. } => {
                Ok(ImportConversationOutcome::AlreadyImported { conversation })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        error::Error,
        fmt,
        future::{Future, ready},
    };

    use signalbox_domain::{
        ImportedConversation, ImportedConversationFormat, ImportedConversationId,
        ImportedConversationSourceDigest, ImportedRawRecordPosition, ImportedRawSourceRecord,
        ImportedRecordEntryPosition, ImportedSourceAttestation, ImportedSourceMetadata,
        ImportedSpeaker, ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
        ImportedTranscriptContent, ImportedTranscriptEntryId, ImportedTranscriptEntryInput,
        ImportedTranscriptPosition,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        ImportConversationError, ImportConversationOutcome, ImportConversationService,
        ImportedConversationConverter, ImportedConversationIdGenerator, ImportedConversationStore,
        ImportedConversationStoreOutcome, UuidV7ImportedConversationIdGenerator,
    };

    fn conversation(value: u128) -> ImportedConversationId {
        ImportedConversationId::from_uuid(Uuid::from_u128(value))
    }

    fn entry(value: u128) -> ImportedTranscriptEntryId {
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value))
    }

    fn text(value: &str) -> ImportedText {
        ImportedText::new(String::from(value))
    }

    fn object(source_type: &str) -> ImportedStructuredValue {
        ImportedStructuredValue::Object(
            vec![ImportedStructuredObjectMember::new(
                text("type"),
                ImportedStructuredValue::String(text(source_type)),
            )]
            .into_boxed_slice(),
        )
    }

    fn metadata(speaker: ImportedSpeaker) -> ImportedSourceMetadata {
        ImportedSourceMetadata::new(
            ImportedSourceAttestation::Attested(text("record")),
            ImportedSourceAttestation::AttestedAbsent,
            ImportedSourceAttestation::Attested(text("source-session")),
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::Attested(false),
            ImportedSourceAttestation::Attested(false),
            ImportedSourceAttestation::Attested(speaker),
        )
    }

    fn converted(
        owner: ImportedConversationId,
        entries: [ImportedTranscriptEntryId; 2],
        format: ImportedConversationFormat,
    ) -> ImportedConversation {
        let raws = vec![
            ImportedRawSourceRecord::from_converted(
                br#"{"type":"user","message":{"role":"user","content":"first"}}"#.to_vec(),
                object("user"),
            ),
            ImportedRawSourceRecord::from_converted(
                br#"{"type":"assistant","message":{"role":"assistant","content":"second"}}"#
                    .to_vec(),
                object("assistant"),
            ),
        ];
        ImportedConversation::from_converted_records(
            owner,
            format,
            raws,
            vec![
                ImportedTranscriptEntryInput::new(
                    entries[0],
                    owner,
                    ImportedTranscriptPosition::first(),
                    ImportedRawRecordPosition::first(),
                    ImportedRecordEntryPosition::first(),
                    ImportedSourceAttestation::Attested(ImportedSpeaker::User),
                    ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text(
                        "first",
                    ))),
                    metadata(ImportedSpeaker::User),
                ),
                ImportedTranscriptEntryInput::new(
                    entries[1],
                    owner,
                    ImportedTranscriptPosition::try_from_u64(2)
                        .expect("fixture imported position is positive"),
                    ImportedRawRecordPosition::try_from_u64(2)
                        .expect("fixture raw position is positive"),
                    ImportedRecordEntryPosition::first(),
                    ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
                    ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(text(
                        "second",
                    ))),
                    metadata(ImportedSpeaker::Assistant),
                ),
            ],
        )
        .expect("fixture aggregate is complete")
    }

    #[derive(Debug)]
    struct FakeIds {
        conversations: VecDeque<ImportedConversationId>,
        entries: VecDeque<ImportedTranscriptEntryId>,
        conversation_calls: usize,
        entry_calls: usize,
    }

    impl FakeIds {
        fn new(
            conversations: impl IntoIterator<Item = ImportedConversationId>,
            entries: impl IntoIterator<Item = ImportedTranscriptEntryId>,
        ) -> Self {
            Self {
                conversations: conversations.into_iter().collect(),
                entries: entries.into_iter().collect(),
                conversation_calls: 0,
                entry_calls: 0,
            }
        }
    }

    impl ImportedConversationIdGenerator for FakeIds {
        fn next_conversation_id(&mut self) -> ImportedConversationId {
            self.conversation_calls += 1;
            self.conversations
                .pop_front()
                .expect("fixture conversation identity")
        }

        fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
            self.entry_calls += 1;
            self.entries.pop_front().expect("fixture entry identity")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeConversionError {
        Rejected,
    }

    impl fmt::Display for FakeConversionError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("rejected")
        }
    }

    impl Error for FakeConversionError {}

    #[derive(Debug)]
    struct FakeConverter {
        returned_owner: Option<ImportedConversationId>,
        returned_entries: Option<[ImportedTranscriptEntryId; 2]>,
        reject: bool,
        observed: Vec<(ImportedConversationId, Vec<u8>)>,
    }

    impl ImportedConversationConverter for FakeConverter {
        type Error = FakeConversionError;

        fn format(&self) -> ImportedConversationFormat {
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1
        }

        fn convert<NextEntryId>(
            &mut self,
            owner: ImportedConversationId,
            source: &[u8],
            mut next_entry_id: NextEntryId,
        ) -> Result<ImportedConversation, Self::Error>
        where
            NextEntryId: FnMut() -> ImportedTranscriptEntryId,
        {
            self.observed.push((owner, source.to_vec()));
            if self.reject {
                return Err(FakeConversionError::Rejected);
            }
            let issued_entries = [next_entry_id(), next_entry_id()];
            Ok(converted(
                self.returned_owner.unwrap_or(owner),
                self.returned_entries.unwrap_or(issued_entries),
                ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            ))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeStoreError {
        Unavailable,
    }

    impl fmt::Display for FakeStoreError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("unavailable")
        }
    }

    impl Error for FakeStoreError {}

    #[derive(Debug)]
    struct FakeStore {
        response: Result<ImportedConversationStoreOutcome, FakeStoreError>,
        observed: Vec<ImportedConversation>,
    }

    impl ImportedConversationStore for FakeStore {
        type Error = FakeStoreError;

        fn resolve_or_insert(
            &mut self,
            imported: ImportedConversation,
        ) -> impl Future<Output = Result<ImportedConversationStoreOutcome, Self::Error>> + Send
        {
            self.observed.push(imported);
            ready(self.response)
        }
    }

    fn service(
        candidate: ImportedConversationId,
        entries: [ImportedTranscriptEntryId; 2],
        store_response: Result<ImportedConversationStoreOutcome, FakeStoreError>,
    ) -> ImportConversationService<FakeIds, FakeConverter, FakeStore> {
        ImportConversationService::new(
            FakeIds::new([candidate], entries),
            FakeConverter {
                returned_owner: None,
                returned_entries: None,
                reject: false,
                observed: Vec::new(),
            },
            FakeStore {
                response: store_response,
                observed: Vec::new(),
            },
        )
    }

    fn candidate_digest(
        candidate: ImportedConversationId,
        entries: [ImportedTranscriptEntryId; 2],
    ) -> ImportedConversationSourceDigest {
        converted(
            candidate,
            entries,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
        )
        .source_digest()
    }

    /// INV-038: first ingestion converts once and commits one complete candidate.
    #[tokio::test]
    async fn s28_inv038_first_ingestion_returns_inserted_candidate() {
        let candidate = conversation(1);
        let entries = [entry(2), entry(3)];
        let mut service = service(
            candidate,
            entries,
            Ok(ImportedConversationStoreOutcome::Inserted {
                conversation: candidate,
                source_digest: candidate_digest(candidate, entries),
            }),
        );

        assert_eq!(
            service
                .execute(b"source bytes")
                .await
                .expect("complete source inserts"),
            ImportConversationOutcome::Inserted {
                conversation: candidate,
            }
        );
        let (ids, converter, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 2);
        assert_eq!(
            converter.observed,
            vec![(candidate, b"source bytes".to_vec())]
        );
        assert_eq!(store.observed.len(), 1);
        assert_eq!(store.observed[0].id(), candidate);
    }

    /// INV-038: exact reingestion discards candidates and returns the existing
    /// immutable imported-conversation identity.
    #[tokio::test]
    async fn s28_inv038_exact_reingestion_returns_existing_identity() {
        let candidate = conversation(1);
        let entries = [entry(2), entry(3)];
        let existing = conversation(99);
        let mut service = service(
            candidate,
            entries,
            Ok(ImportedConversationStoreOutcome::AlreadyImported {
                conversation: existing,
                source_digest: candidate_digest(candidate, entries),
            }),
        );

        assert_eq!(
            service
                .execute(b"same source")
                .await
                .expect("exact duplicate resolves"),
            ImportConversationOutcome::AlreadyImported {
                conversation: existing,
            }
        );
        let (ids, _, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 2);
        assert_eq!(store.observed.len(), 1);
    }

    #[tokio::test]
    async fn s28_inv038_conversion_failure_never_reaches_store() {
        let candidate = conversation(1);
        let entries = [entry(2), entry(3)];
        let mut service = service(
            candidate,
            entries,
            Ok(ImportedConversationStoreOutcome::Inserted {
                conversation: candidate,
                source_digest: candidate_digest(candidate, entries),
            }),
        );
        service.converter.reject = true;

        assert_eq!(
            service.execute(b"rejected").await,
            Err(ImportConversationError::Conversion(
                FakeConversionError::Rejected
            ))
        );
        let (ids, converter, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 0);
        assert_eq!(converter.observed.len(), 1);
        assert!(store.observed.is_empty());
    }

    #[tokio::test]
    async fn s28_inv001_inv038_converter_identity_mismatch_never_reaches_store() {
        let candidate = conversation(1);
        let entries = [entry(2), entry(3)];
        let converted = conversation(9);
        let mut service = service(
            candidate,
            entries,
            Ok(ImportedConversationStoreOutcome::Inserted {
                conversation: candidate,
                source_digest: candidate_digest(candidate, entries),
            }),
        );
        service.converter.returned_owner = Some(converted);

        assert_eq!(
            service.execute(b"cross-wired").await,
            Err(ImportConversationError::ConverterIdentityMismatch {
                supplied: candidate,
                converted,
            })
        );
        let (_, _, store) = service.into_parts();
        assert!(store.observed.is_empty());
    }

    #[tokio::test]
    async fn s28_inv001_converter_unissued_entry_identity_never_reaches_store() {
        let candidate = conversation(1);
        let issued_entries = [entry(2), entry(3)];
        let unissued_entry = entry(9);
        let mut service = service(
            candidate,
            issued_entries,
            Ok(ImportedConversationStoreOutcome::Inserted {
                conversation: candidate,
                source_digest: candidate_digest(candidate, issued_entries),
            }),
        );
        service.converter.returned_entries = Some([unissued_entry, entry(10)]);

        assert_eq!(
            service.execute(b"cross-wired").await,
            Err(ImportConversationError::ConverterEntryIdentityNotIssued {
                entry: unissued_entry,
            })
        );
        let (_, _, store) = service.into_parts();
        assert!(store.observed.is_empty());
    }

    #[tokio::test]
    async fn s28_inv038_store_source_digest_mismatch_fails_closed() {
        let candidate = conversation(1);
        let entries = [entry(2), entry(3)];
        let expected_digest = candidate_digest(candidate, entries);
        let different_digest = ImportedConversationSourceDigest::from_bytes([9; 32]);
        let mut service = service(
            candidate,
            entries,
            Ok(ImportedConversationStoreOutcome::AlreadyImported {
                conversation: conversation(99),
                source_digest: different_digest,
            }),
        );
        assert_eq!(
            service.execute(b"source").await,
            Err(ImportConversationError::StoreSourceDigestMismatch {
                expected: expected_digest,
                actual: different_digest,
            })
        );
    }

    #[tokio::test]
    async fn s28_inv038_store_inserted_identity_mismatch_fails_closed() {
        let candidate = conversation(1);
        let entries = [entry(2), entry(3)];
        let wrong_identity = conversation(99);
        let mut service = service(
            candidate,
            entries,
            Ok(ImportedConversationStoreOutcome::Inserted {
                conversation: wrong_identity,
                source_digest: candidate_digest(candidate, entries),
            }),
        );
        assert_eq!(
            service.execute(b"source").await,
            Err(ImportConversationError::StoreInsertedIdentityMismatch {
                expected: candidate,
                actual: wrong_identity,
            })
        );
    }

    #[tokio::test]
    async fn s28_inv038_store_failure_is_not_retried() {
        let candidate = conversation(1);
        let entries = [entry(2), entry(3)];
        let mut service = service(candidate, entries, Err(FakeStoreError::Unavailable));

        assert_eq!(
            service.execute(b"complete").await,
            Err(ImportConversationError::Store(FakeStoreError::Unavailable))
        );
        let (ids, _, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 2);
        assert_eq!(store.observed.len(), 1);
    }

    #[track_caller]
    fn assert_uuid_v7_candidate(value: Uuid) {
        assert_eq!(value.get_variant(), Variant::RFC4122);
        assert_eq!(value.get_version(), Some(Version::SortRand));
        assert!(!value.is_nil());
        assert!(!value.is_max());
    }

    /// INV-001: production generators supply fresh UUIDv7 values for both
    /// imported identity kinds.
    #[test]
    fn inv001_production_generator_supplies_distinct_uuid_v7_candidates() {
        let mut ids = UuidV7ImportedConversationIdGenerator;
        let first_conversation = ids.next_conversation_id().into_uuid();
        let second_conversation = ids.next_conversation_id().into_uuid();
        let first_entry = ids.next_entry_id().into_uuid();
        let second_entry = ids.next_entry_id().into_uuid();

        assert_uuid_v7_candidate(first_conversation);
        assert_uuid_v7_candidate(second_conversation);
        assert_uuid_v7_candidate(first_entry);
        assert_uuid_v7_candidate(second_entry);
        assert_ne!(first_conversation, second_conversation);
        assert_ne!(first_conversation, first_entry);
        assert_ne!(first_conversation, second_entry);
        assert_ne!(second_conversation, first_entry);
        assert_ne!(second_conversation, second_entry);
        assert_ne!(first_entry, second_entry);
    }
}

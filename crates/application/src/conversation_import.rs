//! Source-neutral conversation-ingestion orchestration.
//!
//! Format adapters implement [`ImportedConversationConverter`]; persistence
//! adapters implement [`ImportedConversationStore`]. The application supplies
//! hub identities and performs one complete resolve-or-insert operation.

use std::{error::Error, fmt, future::Future};

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

/// One physical source record that a resilient converter did not import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedConversationSkippedRecord<Failure> {
    source_line: u64,
    failure: Failure,
}

impl<Failure> ImportedConversationSkippedRecord<Failure> {
    /// Records one typed failure at its one-based physical source line.
    pub const fn new(source_line: u64, failure: Failure) -> Self {
        Self {
            source_line,
            failure,
        }
    }

    /// Returns the one-based physical source line that was skipped.
    pub const fn source_line(&self) -> u64 {
        self.source_line
    }

    /// Borrows the typed reason the record was skipped.
    pub const fn failure(&self) -> &Failure {
        &self.failure
    }

    /// Returns the physical source line and typed failure.
    pub fn into_parts(self) -> (u64, Failure) {
        (self.source_line, self.failure)
    }
}

/// Checked accepted records together with every rejected physical record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportedConversationConversionReport<Failure> {
    /// At least one record converted into the checked aggregate.
    Converted {
        /// Aggregate containing only accepted records in physical order.
        conversation: ImportedConversation,
        /// Every rejected record in physical source order.
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
    /// Every physical source record failed record-local validation.
    NoValidRecords {
        /// Every rejected record in physical source order.
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
}

/// Opt-in record-resilient extension of the strict converter seam.
pub trait ResilientImportedConversationConverter: ImportedConversationConverter {
    /// Typed reason one physical record was skipped.
    type RecordFailure;

    /// Converts every valid physical record and reports every rejected record.
    ///
    /// Record-local failures do not consume entry identities. Failures that
    /// prevent a checked aggregate or an exact report remain outer errors.
    fn convert_resilient<NextEntryId>(
        &mut self,
        conversation: ImportedConversationId,
        source: &[u8],
        next_entry_id: NextEntryId,
    ) -> Result<ImportedConversationConversionReport<Self::RecordFailure>, Self::Error>
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

/// Record-resilient import outcome and every rejected physical source record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportConversationReport<Failure> {
    /// At least one record converted, but rejected source evidence prevents storage.
    Converted {
        /// Checked aggregate containing the accepted records.
        conversation: ImportedConversation,
        /// Every rejected record in physical source order.
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
    /// Every record converted and the complete source was durably resolved or inserted.
    Imported {
        /// Durable outcome for the complete checked aggregate.
        outcome: ImportConversationOutcome,
        /// Empty rejected-record set, retained for one uniform report shape.
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
    /// Every physical source record failed record-local validation.
    NoValidRecords {
        /// Every rejected record in physical source order.
        skipped_records: Box<[ImportedConversationSkippedRecord<Failure>]>,
    },
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
    /// Emitted entry identities did not exactly match callback issuance order.
    ConverterEntryIdentitySequenceMismatch,
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
            Self::ConverterEntryIdentitySequenceMismatch => formatter.write_str(
                "conversation converter entry identities did not match callback issuance",
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

fn validate_converted<ConverterError, StoreError>(
    candidate: ImportedConversationId,
    declared: ImportedConversationFormat,
    issued_entries: &[ImportedTranscriptEntryId],
    converted: &ImportedConversation,
) -> Result<(), ImportConversationError<ConverterError, StoreError>> {
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
    if converted
        .entries()
        .iter()
        .map(|entry| entry.identity())
        .ne(issued_entries.iter().copied())
    {
        return Err(ImportConversationError::ConverterEntryIdentitySequenceMismatch);
    }
    Ok(())
}

async fn store_validated<ConverterError, Store>(
    store: &mut Store,
    candidate: ImportedConversationId,
    converted: ImportedConversation,
) -> Result<ImportConversationOutcome, ImportConversationError<ConverterError, Store::Error>>
where
    Store: ImportedConversationStore,
{
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
        let mut issued_entries = Vec::new();
        let converted = converter
            .convert(candidate, source, || {
                let entry = ids.next_entry_id();
                issued_entries.push(entry);
                entry
            })
            .map_err(ImportConversationError::Conversion)?;
        validate_converted::<Converter::Error, Store::Error>(
            candidate,
            declared,
            &issued_entries,
            &converted,
        )?;
        store_validated::<Converter::Error, _>(store, candidate, converted).await
    }
}

impl<Generator, Converter, Store> ImportConversationService<Generator, Converter, Store>
where
    Generator: ImportedConversationIdGenerator,
    Converter: ResilientImportedConversationConverter,
    Store: ImportedConversationStore,
{
    /// Converts valid records, reports rejected records, and stores at most one aggregate.
    pub async fn execute_resilient(
        &mut self,
        source: &[u8],
    ) -> Result<
        ImportConversationReport<Converter::RecordFailure>,
        ImportConversationError<Converter::Error, Store::Error>,
    > {
        let Self {
            ids,
            converter,
            store,
        } = self;
        let candidate = ids.next_conversation_id();
        let declared = converter.format();
        let mut issued_entries = Vec::new();
        let report = converter
            .convert_resilient(candidate, source, || {
                let entry = ids.next_entry_id();
                issued_entries.push(entry);
                entry
            })
            .map_err(ImportConversationError::Conversion)?;
        let (converted, skipped_records) = match report {
            ImportedConversationConversionReport::Converted {
                conversation,
                skipped_records,
            } => (conversation, skipped_records),
            ImportedConversationConversionReport::NoValidRecords { skipped_records } => {
                if !issued_entries.is_empty() {
                    return Err(ImportConversationError::ConverterEntryIdentitySequenceMismatch);
                }
                return Ok(ImportConversationReport::NoValidRecords { skipped_records });
            }
        };
        validate_converted::<Converter::Error, Store::Error>(
            candidate,
            declared,
            &issued_entries,
            &converted,
        )?;
        if !skipped_records.is_empty() {
            return Ok(ImportConversationReport::Converted {
                conversation: converted,
                skipped_records,
            });
        }
        let outcome = store_validated::<Converter::Error, _>(store, candidate, converted).await?;
        Ok(ImportConversationReport::Imported {
            outcome,
            skipped_records,
        })
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
        ImportConversationError, ImportConversationOutcome, ImportConversationReport,
        ImportConversationService, ImportedConversationConversionReport,
        ImportedConversationConverter, ImportedConversationIdGenerator,
        ImportedConversationSkippedRecord, ImportedConversationStore,
        ImportedConversationStoreOutcome, ResilientImportedConversationConverter,
        UuidV7ImportedConversationIdGenerator,
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

    fn message_record(source_type: &str, content: &str) -> ImportedStructuredValue {
        ImportedStructuredValue::Object(
            vec![
                ImportedStructuredObjectMember::new(
                    text("type"),
                    ImportedStructuredValue::String(text(source_type)),
                ),
                ImportedStructuredObjectMember::new(
                    text("message"),
                    ImportedStructuredValue::Object(
                        vec![
                            ImportedStructuredObjectMember::new(
                                text("role"),
                                ImportedStructuredValue::String(text(source_type)),
                            ),
                            ImportedStructuredObjectMember::new(
                                text("content"),
                                ImportedStructuredValue::String(text(content)),
                            ),
                        ]
                        .into_boxed_slice(),
                    ),
                ),
            ]
            .into_boxed_slice(),
        )
    }

    fn metadata(speaker: ImportedSpeaker) -> ImportedSourceMetadata {
        ImportedSourceMetadata::new(
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
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
                message_record("user", "first"),
            ),
            ImportedRawSourceRecord::from_converted(
                br#"{"type":"assistant","message":{"role":"assistant","content":"second"}}"#
                    .to_vec(),
                message_record("assistant", "second"),
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
        request_extra_entry: bool,
        reject: bool,
        resilient_no_valid_records: bool,
        resilient_no_skips: bool,
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
            if self.request_extra_entry {
                let _unused = next_entry_id();
            }
            Ok(converted(
                self.returned_owner.unwrap_or(owner),
                self.returned_entries.unwrap_or(issued_entries),
                ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            ))
        }
    }

    impl ResilientImportedConversationConverter for FakeConverter {
        type RecordFailure = FakeConversionError;

        fn convert_resilient<NextEntryId>(
            &mut self,
            owner: ImportedConversationId,
            source: &[u8],
            next_entry_id: NextEntryId,
        ) -> Result<ImportedConversationConversionReport<Self::RecordFailure>, Self::Error>
        where
            NextEntryId: FnMut() -> ImportedTranscriptEntryId,
        {
            if self.resilient_no_valid_records {
                self.observed.push((owner, source.to_vec()));
                return Ok(ImportedConversationConversionReport::NoValidRecords {
                    skipped_records: vec![ImportedConversationSkippedRecord::new(
                        1,
                        FakeConversionError::Rejected,
                    )]
                    .into_boxed_slice(),
                });
            }
            let conversation = self.convert(owner, source, next_entry_id)?;
            let skipped_records = if self.resilient_no_skips {
                Vec::new()
            } else {
                vec![ImportedConversationSkippedRecord::new(
                    2,
                    FakeConversionError::Rejected,
                )]
            };
            Ok(ImportedConversationConversionReport::Converted {
                conversation,
                skipped_records: skipped_records.into_boxed_slice(),
            })
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
                request_extra_entry: false,
                reject: false,
                resilient_no_valid_records: false,
                resilient_no_skips: false,
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

    /// first ingestion converts once and commits one complete candidate.
    #[tokio::test]
    async fn s28_first_ingestion_returns_inserted_candidate() {
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

    /// S28: partial ingestion returns the checked accepted aggregate
    /// and every loss without claiming the incomplete source is durable.
    #[tokio::test]
    async fn s28_resilient_ingestion_reports_exact_skips_without_storage() {
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

        let report = service
            .execute_resilient(b"partial source")
            .await
            .expect("accepted records should be returned with exact skips");
        let ImportConversationReport::Converted {
            conversation,
            skipped_records,
        } = report
        else {
            panic!("fixture converter returns a partial checked aggregate")
        };

        assert_eq!(conversation.id(), candidate);
        assert_eq!(skipped_records.len(), 1);
        assert_eq!(skipped_records[0].source_line(), 2);
        assert_eq!(skipped_records[0].failure(), &FakeConversionError::Rejected);
        let (ids, converter, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 2);
        assert_eq!(
            converter.observed,
            vec![(candidate, b"partial source".to_vec())]
        );
        assert!(store.observed.is_empty());
    }

    /// S28: a resilient conversion with no losses may use the same
    /// exact-source durable resolution as strict conversion.
    #[tokio::test]
    async fn s28_resilient_complete_ingestion_stores_with_no_skips() {
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
        service.converter.resilient_no_skips = true;

        let report = service
            .execute_resilient(b"complete source")
            .await
            .expect("complete resilient conversion should be stored");
        let ImportConversationReport::Imported {
            outcome,
            skipped_records,
        } = report
        else {
            panic!("fixture converter returns a complete checked aggregate")
        };

        assert_eq!(
            outcome,
            ImportConversationOutcome::Inserted {
                conversation: candidate
            }
        );
        assert!(skipped_records.is_empty());
        let (ids, converter, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 2);
        assert_eq!(
            converter.observed,
            vec![(candidate, b"complete source".to_vec())]
        );
        assert_eq!(store.observed.len(), 1);
    }

    /// S28: all-invalid nonempty input reports every loss without
    /// minting entry identities or attempting a durable write.
    #[tokio::test]
    async fn s28_resilient_ingestion_with_no_valid_records_never_stores() {
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
        service.converter.resilient_no_valid_records = true;

        let report = service
            .execute_resilient(b"rejected source")
            .await
            .expect("all-invalid input still returns its exact losses");
        let ImportConversationReport::NoValidRecords { skipped_records } = report else {
            panic!("fixture converter rejects every record")
        };

        assert_eq!(skipped_records.len(), 1);
        assert_eq!(skipped_records[0].source_line(), 1);
        assert_eq!(skipped_records[0].failure(), &FakeConversionError::Rejected);
        let (ids, converter, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 0);
        assert_eq!(
            converter.observed,
            vec![(candidate, b"rejected source".to_vec())]
        );
        assert!(store.observed.is_empty());
    }

    /// exact reingestion discards candidates and returns the existing
    /// immutable imported-conversation identity.
    #[tokio::test]
    async fn s28_exact_reingestion_returns_existing_identity() {
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
    async fn s28_conversion_failure_never_reaches_store() {
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
    async fn s28_converter_identity_mismatch_never_reaches_store() {
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
    async fn s28_converter_unissued_entry_identity_never_reaches_store() {
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
            Err(ImportConversationError::ConverterEntryIdentitySequenceMismatch)
        );
        let (_, _, store) = service.into_parts();
        assert!(store.observed.is_empty());
    }

    #[tokio::test]
    async fn s28_converter_reordered_entry_identities_never_reach_store() {
        let candidate = conversation(1);
        let issued_entries = [entry(2), entry(3)];
        let mut service = service(
            candidate,
            issued_entries,
            Ok(ImportedConversationStoreOutcome::Inserted {
                conversation: candidate,
                source_digest: candidate_digest(candidate, issued_entries),
            }),
        );
        service.converter.returned_entries = Some([issued_entries[1], issued_entries[0]]);

        assert_eq!(
            service.execute(b"reordered").await,
            Err(ImportConversationError::ConverterEntryIdentitySequenceMismatch)
        );
        let (_, _, store) = service.into_parts();
        assert!(store.observed.is_empty());
    }

    #[tokio::test]
    async fn s28_converter_extra_identity_request_never_reaches_store() {
        let candidate = conversation(1);
        let issued_entries = [entry(2), entry(3)];
        let mut service = service(
            candidate,
            issued_entries,
            Ok(ImportedConversationStoreOutcome::Inserted {
                conversation: candidate,
                source_digest: candidate_digest(candidate, issued_entries),
            }),
        );
        service.ids.entries.push_back(entry(4));
        service.converter.request_extra_entry = true;

        assert_eq!(
            service.execute(b"extra request").await,
            Err(ImportConversationError::ConverterEntryIdentitySequenceMismatch)
        );
        let (ids, _, store) = service.into_parts();
        assert_eq!(ids.entry_calls, 3);
        assert!(store.observed.is_empty());
    }

    #[tokio::test]
    async fn s28_store_source_digest_mismatch_fails_closed() {
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
    async fn s28_store_inserted_identity_mismatch_fails_closed() {
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
    async fn s28_store_failure_is_not_retried() {
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

    /// production generators supply fresh UUIDv7 values for both
    /// imported identity kinds.
    #[test]
    fn production_generator_supplies_distinct_uuid_v7_candidates() {
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

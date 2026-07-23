//! Source-neutral conversation-import orchestration.
//!
//! Format adapters implement [`ImportedConversationConverter`]; persistence
//! adapters implement [`ImportedConversationStore`]. The application supplies
//! hub identities and commits only one completely converted domain aggregate.

use std::{error::Error, fmt, future::Future};

use signalbox_domain::{
    ImportedConversation, ImportedConversationFormat, ImportedConversationId,
    ImportedTranscriptEntryId,
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

/// Atomic append-only store boundary for one complete imported conversation.
pub trait ImportedConversationStore {
    /// Adapter-specific infrastructure, collision, or integrity failure.
    type Error;

    /// Inserts one complete aggregate exactly once.
    fn insert(
        &mut self,
        conversation: ImportedConversation,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Conversation-import orchestration failure.
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
    /// The append-only store could not commit the complete aggregate.
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

/// Coordinates one conversion and append-only import.
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
    /// Converts one source and commits only its complete checked aggregate.
    ///
    /// The service calls the converter and store at most once and performs no
    /// retry. Identity candidates consumed by a failed conversion are simply
    /// discarded.
    pub async fn execute(
        &mut self,
        source: &[u8],
    ) -> Result<ImportedConversationId, ImportConversationError<Converter::Error, Store::Error>>
    {
        let Self {
            ids,
            converter,
            store,
        } = self;
        let conversation = ids.next_conversation_id();
        let declared = converter.format();
        let converted = converter
            .convert(conversation, source, || ids.next_entry_id())
            .map_err(ImportConversationError::Conversion)?;
        if converted.id() != conversation {
            return Err(ImportConversationError::ConverterIdentityMismatch {
                supplied: conversation,
                converted: converted.id(),
            });
        }
        if converted.format() != declared {
            return Err(ImportConversationError::ConverterFormatMismatch {
                declared,
                converted: converted.format(),
            });
        }
        store
            .insert(converted)
            .await
            .map_err(ImportConversationError::Store)?;
        Ok(conversation)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::{Future, ready},
    };

    use signalbox_domain::{
        ImportedConversation, ImportedConversationFormat, ImportedConversationId,
        ImportedConversationReconstitutionInput, ImportedSeedDisposition,
        ImportedSourceAttestation, ImportedSourceMetadata, ImportedSpeaker,
        ImportedTranscriptContent, ImportedTranscriptEntryId,
        ImportedTranscriptEntryReconstitutionInput, ImportedTranscriptPosition,
        NonEmptyUnicodeText,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        ImportConversationError, ImportConversationService, ImportedConversationConverter,
        ImportedConversationIdGenerator, ImportedConversationStore,
        UuidV7ImportedConversationIdGenerator,
    };

    fn conversation(value: u128) -> ImportedConversationId {
        ImportedConversationId::from_uuid(Uuid::from_u128(value))
    }

    fn entry(value: u128) -> ImportedTranscriptEntryId {
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(value))
    }

    fn text(value: &str) -> NonEmptyUnicodeText {
        NonEmptyUnicodeText::try_new(String::from(value)).expect("fixture text is admitted")
    }

    fn converted(
        owner: ImportedConversationId,
        entries: [ImportedTranscriptEntryId; 2],
        format: ImportedConversationFormat,
    ) -> ImportedConversation {
        let source = ImportedSourceMetadata::new(
            ImportedSourceAttestation::Attested(text("record")),
            ImportedSourceAttestation::AttestedAbsent,
            ImportedSourceAttestation::Attested(text("source-session")),
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::Attested(false),
            ImportedSourceAttestation::Attested(false),
        );
        ImportedConversationReconstitutionInput::new(
            owner,
            owner,
            format,
            2,
            vec![
                ImportedTranscriptEntryReconstitutionInput::new(
                    entries[0],
                    owner,
                    ImportedTranscriptPosition::first(),
                    ImportedSpeaker::User,
                    ImportedTranscriptContent::Text(text("first")),
                    source.clone(),
                    ImportedSeedDisposition::Included,
                ),
                ImportedTranscriptEntryReconstitutionInput::new(
                    entries[1],
                    owner,
                    ImportedTranscriptPosition::try_from_u64(2)
                        .expect("fixture position is positive"),
                    ImportedSpeaker::Assistant,
                    ImportedTranscriptContent::Text(text("second")),
                    source,
                    ImportedSeedDisposition::Included,
                ),
            ],
        )
        .reconstitute()
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

    impl std::fmt::Display for FakeConversionError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("rejected")
        }
    }

    impl std::error::Error for FakeConversionError {}

    #[derive(Debug)]
    struct FakeConverter {
        declared: ImportedConversationFormat,
        converted: ImportedConversationFormat,
        returned_owner: Option<ImportedConversationId>,
        reject: bool,
        observed: Vec<(ImportedConversationId, Vec<u8>)>,
    }

    impl ImportedConversationConverter for FakeConverter {
        type Error = FakeConversionError;

        fn format(&self) -> ImportedConversationFormat {
            self.declared
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
            Ok(converted(
                self.returned_owner.unwrap_or(owner),
                [next_entry_id(), next_entry_id()],
                self.converted,
            ))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeStoreError {
        Unavailable,
    }

    impl std::fmt::Display for FakeStoreError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("unavailable")
        }
    }

    impl std::error::Error for FakeStoreError {}

    #[derive(Debug)]
    struct FakeStore {
        response: Result<(), FakeStoreError>,
        observed: Vec<ImportedConversation>,
    }

    impl ImportedConversationStore for FakeStore {
        type Error = FakeStoreError;

        fn insert(
            &mut self,
            imported: ImportedConversation,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.observed.push(imported);
            ready(self.response)
        }
    }

    fn service(
        converter: FakeConverter,
        store_response: Result<(), FakeStoreError>,
    ) -> ImportConversationService<FakeIds, FakeConverter, FakeStore> {
        ImportConversationService::new(
            FakeIds::new([conversation(1)], [entry(2), entry(3)]),
            converter,
            FakeStore {
                response: store_response,
                observed: Vec::new(),
            },
        )
    }

    /// INV-038: one source conversion receives only caller-minted identities
    /// and exactly one complete aggregate reaches the append-only store.
    #[tokio::test]
    async fn inv038_converts_and_stores_one_complete_aggregate() {
        let format = ImportedConversationFormat::ClaudeCodeSessionJsonlV1;
        let mut service = service(
            FakeConverter {
                declared: format,
                converted: format,
                returned_owner: None,
                reject: false,
                observed: Vec::new(),
            },
            Ok(()),
        );

        let imported = service
            .execute(b"source bytes")
            .await
            .expect("the complete conversion is stored");
        assert_eq!(imported, conversation(1));

        let (ids, converter, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 2);
        assert_eq!(
            converter.observed,
            vec![(conversation(1), b"source bytes".to_vec())]
        );
        assert_eq!(store.observed.len(), 1);
        assert_eq!(store.observed[0].id(), conversation(1));
        assert_eq!(store.observed[0].format(), format);
    }

    /// A conversion failure consumes no entry identities and performs no store
    /// call, so partial imported records never become durable.
    #[tokio::test]
    async fn conversion_failure_never_reaches_store() {
        let format = ImportedConversationFormat::ClaudeCodeSessionJsonlV1;
        let mut service = service(
            FakeConverter {
                declared: format,
                converted: format,
                returned_owner: None,
                reject: true,
                observed: Vec::new(),
            },
            Ok(()),
        );

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

    /// A converter cannot substitute another hub identity before persistence.
    #[tokio::test]
    async fn converter_identity_mismatch_never_reaches_store() {
        let format = ImportedConversationFormat::ClaudeCodeSessionJsonlV1;
        let mut service = service(
            FakeConverter {
                declared: format,
                converted: format,
                returned_owner: Some(conversation(9)),
                reject: false,
                observed: Vec::new(),
            },
            Ok(()),
        );

        assert_eq!(
            service.execute(b"cross-wired").await,
            Err(ImportConversationError::ConverterIdentityMismatch {
                supplied: conversation(1),
                converted: conversation(9),
            })
        );
        let (_, _, store) = service.into_parts();
        assert!(store.observed.is_empty());
    }

    /// A store failure is returned after exactly one complete insert attempt;
    /// the service does not retry with another identity.
    #[tokio::test]
    async fn store_failure_is_not_retried() {
        let format = ImportedConversationFormat::ClaudeCodeSessionJsonlV1;
        let mut service = service(
            FakeConverter {
                declared: format,
                converted: format,
                returned_owner: None,
                reject: false,
                observed: Vec::new(),
            },
            Err(FakeStoreError::Unavailable),
        );

        assert_eq!(
            service.execute(b"complete").await,
            Err(ImportConversationError::Store(FakeStoreError::Unavailable))
        );
        let (ids, _, store) = service.into_parts();
        assert_eq!(ids.conversation_calls, 1);
        assert_eq!(ids.entry_calls, 2);
        assert_eq!(store.observed.len(), 1);
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

        for value in [
            first_conversation,
            second_conversation,
            first_entry,
            second_entry,
        ] {
            assert_eq!(value.get_variant(), Variant::RFC4122);
            assert_eq!(value.get_version(), Some(Version::SortRand));
            assert!(!value.is_nil());
            assert!(!value.is_max());
        }
        assert_ne!(first_conversation, second_conversation);
        assert_ne!(first_conversation, first_entry);
        assert_ne!(first_conversation, second_entry);
        assert_ne!(second_conversation, first_entry);
        assert_ne!(second_conversation, second_entry);
        assert_ne!(first_entry, second_entry);
    }
}

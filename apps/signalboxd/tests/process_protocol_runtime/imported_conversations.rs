//! Imported conversations coverage.

use super::*;

pub(crate) fn test_session_credential_pin() -> signalbox_persistence::SessionCredentialPin {
    signalbox_persistence::SessionCredentialPin::try_new(vec![
        signalbox_persistence::SessionModelCredential::new(
            "test-model-family",
            "test-model-primary",
        ),
    ])
    .expect("test credential pin is valid")
}

#[derive(Debug)]
pub(crate) struct FixedImportIds {
    pub(crate) conversations: VecDeque<ImportedConversationId>,
    pub(crate) entries: VecDeque<ImportedTranscriptEntryId>,
}

impl ImportedConversationIdGenerator for FixedImportIds {
    fn next_conversation_id(&mut self) -> ImportedConversationId {
        self.conversations
            .pop_front()
            .expect("the fixture supplies one conversation identity")
    }

    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
        self.entries
            .pop_front()
            .expect("the fixture supplies one identity per imported entry")
    }
}

#[derive(Debug)]
pub(crate) struct FixedImportedSessionIds {
    pub(crate) sessions: VecDeque<SessionId>,
    pub(crate) semantic_entries: VecDeque<SemanticTranscriptEntryId>,
    pub(crate) frontiers: VecDeque<ContextFrontierId>,
}

impl CreateSessionFromImportedFrontierIdGenerator for FixedImportedSessionIds {
    fn next_session_id(&mut self) -> SessionId {
        self.sessions
            .pop_front()
            .expect("the fixture supplies one session identity")
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.semantic_entries
            .pop_front()
            .expect("the fixture supplies one semantic identity per prefix entry")
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("the fixture supplies one seed frontier identity")
    }
}

pub(crate) const IMPORTED_USER_CONTENT: &str = "imported user";

pub(crate) async fn create_imported_session(
    pool: &PgPool,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x100));
    let imported_entries = [
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x200)),
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x201)),
    ];
    let source = concat!(
        "{\"type\":\"user\",\"message\":{\"content\":\"<user-content>\"}}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[",
        "{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"lookup\",",
        "\"input\":{\"query\":\"synthetic\"}}]}}"
    )
    .replace("<user-content>", IMPORTED_USER_CONTENT);
    let mut import_service = ImportConversationService::new(
        FixedImportIds {
            conversations: [conversation].into(),
            entries: imported_entries.into(),
        },
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    assert_eq!(
        import_service.execute(source.as_bytes()).await?,
        ImportConversationOutcome::Inserted { conversation }
    );
    let (_, _, import_repository) = import_service.into_parts();
    let stored = import_repository
        .load(conversation)
        .await?
        .expect("the synthetic imported conversation is durable");
    let frontier = stored
        .frontiers()
        .last()
        .expect("the final imported entry exposes a seed boundary");

    let session = SessionId::from_uuid(Uuid::from_u128(0x300));
    let mut create_service = CreateSessionFromImportedFrontierService::new(
        FixedImportedSessionIds {
            sessions: [session].into(),
            semantic_entries: [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x400)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x401)),
            ]
            .into(),
            frontiers: [ContextFrontierId::from_uuid(Uuid::from_u128(0x500))].into(),
        },
        ImportedSessionRepository::new(pool.clone(), test_session_credential_pin()),
    );
    let outcome = create_service
        .execute(CreateSessionFromImportedFrontierRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x600)),
            frontier,
            ImportedSessionRelationship::Resume,
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(Uuid::from_u128(1)),
            )),
        )?)
        .await?;
    assert!(matches!(
        outcome,
        CreateSessionFromImportedFrontierOutcome::Applied(result)
            if result.session() == session
    ));
    Ok(CanonicalUuid::from_uuid(session.into_uuid()))
}

/// the user-visible operation distinguishes first insertion from exact-snapshot reimport while
/// retaining the winner's identity.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn single_shot_and_chunked_import_resolve_the_same_snapshot() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let source = ConversationImportSource::new(
        concat!(
            "{\"sessionId\":\"operational-claude\",\"type\":\"user\",",
            "\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"sessionId\":\"operational-claude\",\"type\":\"assistant\",",
            "\"message\":{\"role\":\"assistant\",\"content\":\"answer\"}}"
        )
        .as_bytes()
        .to_vec(),
    );

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::ImportConversation {
                format: ConversationImportFormat::ClaudeCodeSessionJsonlV2,
                source: source.clone(),
            },
        )
        .await?;
    let inserted = response_within(&mut connection).await?;
    let stored_id: Uuid =
        sqlx::query_scalar("SELECT imported_conversation_id FROM imported_conversation")
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(
        inserted.message(),
        &ServerMessage::ConversationImportInserted {
            imported_conversation_id: CanonicalUuid::from_uuid(stored_id),
        }
    );

    let declared_size_bytes = CanonicalU64::new(u64::try_from(source.as_bytes().len())?);
    connection
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::ClaudeCodeSessionJsonlV2,
                declared_size_bytes,
            },
        )
        .await?;
    let begun = response_within(&mut connection).await?;
    assert_eq!(
        begun.message(),
        &ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        }
    );
    connection
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::AppendConversationImport { chunk: source },
        )
        .await?;
    let appended = response_within(&mut connection).await?;
    assert_eq!(
        appended.message(),
        &ServerMessage::ConversationImportAppended {
            assembled_size_bytes: declared_size_bytes,
        }
    );
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::CommitConversationImport {},
        )
        .await?;
    let already_imported = response_within(&mut connection).await?;
    assert_eq!(
        already_imported.message(),
        &ServerMessage::ConversationImportAlreadyImported {
            imported_conversation_id: CanonicalUuid::from_uuid(stored_id),
        }
    );

    drop(connection);
    runtime.stop().await
}

/// disconnect discards per-connection partial import state.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn disconnect_discards_a_partial_chunked_import() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let chunk = vec![b'x'];
    let declared_size_bytes = CanonicalU64::new(u64::try_from(chunk.len())?);
    let mut abandoned = Connection::connect(runtime.socket()).await?;
    abandoned
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                declared_size_bytes,
            },
        )
        .await?;
    let begun = response_within(&mut abandoned).await?;
    assert_eq!(
        begun.message(),
        &ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        }
    );
    abandoned
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::AppendConversationImport {
                chunk: ConversationImportSource::new(chunk.clone()),
            },
        )
        .await?;
    let appended = response_within(&mut abandoned).await?;
    assert_eq!(
        appended.message(),
        &ServerMessage::ConversationImportAppended {
            assembled_size_bytes: declared_size_bytes,
        }
    );
    drop(abandoned);

    let mut replacement = Connection::connect(runtime.socket()).await?;
    replacement
        .request_version(
            ProtocolVersion::One,
            3,
            ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                declared_size_bytes,
            },
        )
        .await?;
    let replacement_begun = response_within(&mut replacement).await?;
    assert_eq!(
        replacement_begun.message(),
        &ServerMessage::ConversationImportBegun {
            declared_size_bytes,
        }
    );
    replacement
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::AbortConversationImport {},
        )
        .await?;
    let aborted = response_within(&mut replacement).await?;
    assert_eq!(
        aborted.message(),
        &ServerMessage::ConversationImportAborted {}
    );

    drop(replacement);
    runtime.stop().await
}

/// One durable synthetic imported conversation and the identities its
/// selectable positions carry.
pub(crate) struct ImportedInspectionFixture {
    pub(crate) conversation: CanonicalUuid,
    pub(crate) user_entry: CanonicalUuid,
    pub(crate) tool_entry: CanonicalUuid,
    pub(crate) user_text: &'static str,
    /// The greatest selectable position, which is also the entry count: the
    /// two-record source below emits exactly one entry per record.
    pub(crate) last_position: CanonicalU64,
}

impl ImportedInspectionFixture {
    /// The exact attested user text at position one. Position two is a tool
    /// call, which the conservative projection carries as a kind alone.
    const USER_TEXT: &'static str = "imported question";

    pub(crate) async fn insert(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x900));
        let user_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x901));
        let tool_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x902));
        let source = concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",",
            "\"content\":\"imported question\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[",
            "{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"lookup\",",
            "\"input\":{\"query\":\"synthetic\"}}]}}"
        );
        let mut import_service = ImportConversationService::new(
            FixedImportIds {
                conversations: [conversation].into(),
                entries: [user_entry, tool_entry].into(),
            },
            ClaudeCodeJsonlConverter,
            ImportedConversationRepository::new(pool.clone()),
        );
        assert_eq!(
            import_service.execute(source.as_bytes()).await?,
            ImportConversationOutcome::Inserted { conversation }
        );
        Ok(Self {
            conversation: CanonicalUuid::from_uuid(conversation.into_uuid()),
            user_entry: CanonicalUuid::from_uuid(user_entry.into_uuid()),
            tool_entry: CanonicalUuid::from_uuid(tool_entry.into_uuid()),
            user_text: Self::USER_TEXT,
            last_position: CanonicalU64::new(2),
        })
    }
}

/// the inspection read names every selectable imported position with its attestation, content kind,
/// and bounded preview, so the ordinal `create_session_from_imported_frontier` consumes is
/// observable before it is consumed.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn reads_every_selectable_imported_position() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let fixture = ImportedInspectionFixture::insert(&runtime.pool).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::ReadImportedConversation {
                imported_conversation_id: fixture.conversation,
            },
        )
        .await?;

    let start = response_within(&mut connection).await?;
    assert_eq!(
        start.message(),
        &ServerMessage::ImportedConversationStart {
            imported_conversation_id: fixture.conversation,
        }
    );
    let first = response_within(&mut connection).await?;
    assert_eq!(
        first.message(),
        &ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(1),
            imported_entry_id: fixture.user_entry,
            source_speaker: ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::User,
            },
            content_kind: ImportedContentKind::Text,
            text_preview: Some(ImportedTextPreview::of_exact_text(fixture.user_text)),
        }
    );
    let second = response_within(&mut connection).await?;
    assert_eq!(
        second.message(),
        &ServerMessage::ImportedConversationEntry {
            position: fixture.last_position,
            imported_entry_id: fixture.tool_entry,
            source_speaker: ImportedSourceSpeaker::Attested {
                speaker: ImportedSpeaker::Assistant,
            },
            content_kind: ImportedContentKind::ToolCall,
            text_preview: None,
        }
    );
    let end = response_within(&mut connection).await?;
    assert_eq!(
        end.message(),
        &ServerMessage::ImportedConversationEnd {
            imported_conversation_id: fixture.conversation,
            entry_count: fixture.last_position,
        }
    );

    drop(connection);
    runtime.stop().await
}

/// an absent imported conversation is a read miss naming an imported conversation, never the
/// absent-session diagnostic.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn read_names_an_absent_imported_conversation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::ReadImportedConversation {
                imported_conversation_id: CanonicalUuid::from_uuid(Uuid::from_u128(0x9ff)),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    let ServerMessage::Error { code, message, .. } = response.message() else {
        panic!("an absent imported conversation returns an error");
    };
    assert_eq!(*code, ErrorCode::NotFound);
    assert_eq!(message, "the requested imported conversation was not found");

    drop(connection);
    runtime.stop().await
}

/// a valid imported conversation carrying an out-of-range position is a rejection naming the
/// selectable range, not a `not_found` claiming the identity was absent.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn continuation_names_the_selectable_position_range() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let fixture = ImportedInspectionFixture::insert(&runtime.pool).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command()?,
                imported_conversation_id: fixture.conversation,
                through_position: CanonicalU64::new(999_999),
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    let ServerMessage::Error { code, detail, .. } = response.message() else {
        panic!("an out-of-range imported position returns an error");
    };
    assert_eq!(*code, ErrorCode::Rejected);
    assert_eq!(
        detail.value(),
        Some(RejectionDetail::ImportedFrontierPositionOutOfRange {
            imported_conversation_id: fixture.conversation,
            requested_position: CanonicalU64::new(999_999),
            last_position: fixture.last_position,
        })
    );

    drop(connection);
    runtime.stop().await
}

/// an absent imported conversation on the continuation command names an imported conversation as
/// the missing target.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn continuation_names_an_absent_imported_conversation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let absent = CanonicalUuid::from_uuid(Uuid::from_u128(0x9ff));
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command()?,
                imported_conversation_id: absent,
                through_position: CanonicalU64::new(1),
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    let ServerMessage::Error { code, detail, .. } = response.message() else {
        panic!("an absent imported conversation returns an error");
    };
    assert_eq!(*code, ErrorCode::Rejected);
    assert_eq!(
        detail.value(),
        Some(RejectionDetail::ImportedConversationNotFound {
            imported_conversation_id: absent,
        })
    );

    drop(connection);
    runtime.stop().await
}

/// the imported wire address resolves against the immutable aggregate before settings admission, so
/// an absent conversation and an out-of-range position each win over an explicit setting the
/// selected model cannot support.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn imported_address_precedes_settings_validation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let fixture = ImportedInspectionFixture::insert(&runtime.pool).await?;
    let absent = CanonicalUuid::from_uuid(Uuid::from_u128(0x28f0));
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request(
            1,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command()?,
                imported_conversation_id: absent,
                through_position: CanonicalU64::new(1),
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: next_direct_selection_id(),
                },
                model_settings: low_reasoning_override(),
            },
        )
        .await?;
    let missing_conversation = response_within(&mut connection).await?.message().clone();

    connection
        .request(
            2,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id: command()?,
                imported_conversation_id: fixture.conversation,
                through_position: CanonicalU64::new(999_999),
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: next_direct_selection_id(),
                },
                model_settings: low_reasoning_override(),
            },
        )
        .await?;
    let out_of_range = response_within(&mut connection).await?.message().clone();

    assert_eq!(
        protocol_error_code(&missing_conversation),
        ErrorCode::Rejected
    );
    assert_eq!(
        protocol_error_detail(&missing_conversation),
        Some(RejectionDetail::ImportedConversationNotFound {
            imported_conversation_id: absent,
        })
    );
    assert_eq!(protocol_error_code(&out_of_range), ErrorCode::Rejected);
    assert_eq!(
        protocol_error_detail(&out_of_range),
        Some(RejectionDetail::ImportedFrontierPositionOutOfRange {
            imported_conversation_id: fixture.conversation,
            requested_position: CanonicalU64::new(999_999),
            last_position: fixture.last_position,
        })
    );

    drop(connection);
    runtime.stop().await
}

/// the explicit Codex selection reaches the fixed Codex converter rather than applying format
/// detection or the Claude Code interpretation.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn selects_the_codex_rollout_converter() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let source = ConversationImportSource::new(
        concat!(
            "{\"timestamp\":\"2026-07-25T00:00:00Z\",\"type\":\"response_item\",",
            "\"payload\":{\"type\":\"message\",\"role\":\"user\",",
            "\"content\":[{\"type\":\"input_text\",\"text\":\"question\"}]}}"
        )
        .as_bytes()
        .to_vec(),
    );

    connection
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::ImportConversation {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                source,
            },
        )
        .await?;
    let inserted = response_within(&mut connection).await?;
    let stored_id: Uuid =
        sqlx::query_scalar("SELECT imported_conversation_id FROM imported_conversation")
            .fetch_one(&runtime.pool)
            .await?;
    assert_eq!(
        inserted.message(),
        &ServerMessage::ConversationImportInserted {
            imported_conversation_id: CanonicalUuid::from_uuid(stored_id),
        }
    );
    let stored = ImportedConversationRepository::new(runtime.pool.clone())
        .load(ImportedConversationId::from_uuid(stored_id))
        .await?
        .expect("the successful operation inserted its imported conversation");
    assert_eq!(
        stored.format(),
        ImportedConversationFormat::CodexRolloutJsonlV1
    );

    drop(connection);
    runtime.stop().await
}

/// Requires the next response to be the inserted-import receipt and returns
/// the inserted imported-conversation identity.
pub(crate) async fn require_inserted_import_receipt(
    connection: &mut Connection,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    match response_within(connection).await?.message() {
        ServerMessage::ConversationImportInserted {
            imported_conversation_id,
        } => Ok(*imported_conversation_id),
        message => Err(io::Error::other(format!("unexpected import receipt: {message:?}")).into()),
    }
}

/// Requires the next response to be one unified conversation summary.
pub(crate) async fn require_conversation_summary(
    connection: &mut Connection,
) -> Result<ConversationSummary, Box<dyn Error>> {
    match response_within(connection).await?.message() {
        ServerMessage::ConversationSummary { conversation } => Ok(conversation.clone()),
        message => Err(io::Error::other(format!("unexpected unified summary: {message:?}")).into()),
    }
}

/// Splits one native and one imported summary out of a pair listed in either
/// order.
pub(crate) fn partition_native_and_imported(
    first: ConversationSummary,
    second: ConversationSummary,
) -> Result<(ConversationSummary, ConversationSummary), Box<dyn Error>> {
    match (first, second) {
        (
            native @ ConversationSummary::NativeSession { .. },
            imported @ ConversationSummary::ImportedConversation { .. },
        )
        | (
            imported @ ConversationSummary::ImportedConversation { .. },
            native @ ConversationSummary::NativeSession { .. },
        ) => Ok((native, imported)),
        pair => Err(io::Error::other(format!("unexpected unified summary pair: {pair:?}")).into()),
    }
}

/// the unified request lists native sessions and imported conversations in one unified page whose
/// imported row carries the derived title, entry count, and stored source format.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn lists_native_and_imported_conversations() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let native_session = create_alias_session(&mut connection).await?;
    let source_title = "question";
    let source = ConversationImportSource::new(
        concat!(
            "{\"timestamp\":\"2026-07-25T00:00:00Z\",\"type\":\"response_item\",",
            "\"payload\":{\"type\":\"message\",\"role\":\"user\",",
            "\"content\":[{\"type\":\"input_text\",\"text\":\"<title>\"}]}}"
        )
        .replace("<title>", source_title)
        .into_bytes(),
    );
    connection
        .request_version(
            ProtocolVersion::One,
            30,
            ClientRequest::ImportConversation {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                source,
            },
        )
        .await?;
    let imported_id = require_inserted_import_receipt(&mut connection).await?;

    connection
        .request_version(
            ProtocolVersion::One,
            31,
            ClientRequest::ListConversations {
                title_contains: None,
                origin: ConversationOriginFilter::All,
                include_archived: false,
                page_size: CanonicalU64::new(10),
                after: None,
            },
        )
        .await?;
    assert!(matches!(
        response_within(&mut connection).await?.message(),
        ServerMessage::ConversationPageStart {}
    ));
    let first_summary = require_conversation_summary(&mut connection).await?;
    let second_summary = require_conversation_summary(&mut connection).await?;
    let page_end = response_within(&mut connection).await?;
    let ServerMessage::ConversationPageEnd {
        conversation_count,
        next_after: None,
    } = page_end.message()
    else {
        panic!(
            "fixture expected conversation page end, got {:?}",
            page_end.message()
        );
    };
    assert_eq!(conversation_count.value(), 2);
    assert!(
        first_summary.cursor().conversation_id().into_uuid()
            < second_summary.cursor().conversation_id().into_uuid(),
        "unified summaries must arrive in strict identity order"
    );
    let (native, imported) = partition_native_and_imported(first_summary, second_summary)?;
    let ConversationSummary::NativeSession {
        session_id,
        title: None,
        archived: false,
        defaults_version,
    } = native
    else {
        panic!("fixture expected native conversation summary");
    };
    assert_eq!(session_id, native_session);
    assert_eq!(defaults_version.value(), 1);
    let ConversationSummary::ImportedConversation {
        imported_conversation_id,
        title: Some(title),
        entry_count,
        source_format: ImportedConversationSourceFormat::CodexRolloutJsonlV1,
    } = imported
    else {
        panic!("fixture expected imported conversation summary");
    };
    assert_eq!(imported_conversation_id, imported_id);
    assert_eq!(title, source_title);
    assert_eq!(entry_count.value(), 1);

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn read_streams_conservative_imported_seed_snapshot() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;

    let mut read_connection = Connection::connect(runtime.socket()).await?;
    read_connection
        .request_version(
            ProtocolVersion::One,
            2,
            ClientRequest::ReadTranscript { session_id },
        )
        .await?;
    let start = response_within(&mut read_connection).await?;
    assert_eq!(start.version(), ProtocolVersion::One);
    let ServerMessage::TranscriptSnapshotStart {
        session_id: selected,
        ..
    } = start.message()
    else {
        panic!(
            "fixture expected transcript start, got {:?}",
            start.message()
        );
    };
    assert_eq!(*selected, session_id);
    let model_calls_end = response_within(&mut read_connection).await?;
    assert_eq!(transcript_model_call_count(model_calls_end.message()), 0);
    let imported_text = response_within(&mut read_connection).await?;
    assert_eq!(imported_text.version(), ProtocolVersion::One);
    let ServerMessage::TranscriptTextEntry {
        entry_index,
        entry:
            TranscriptTextEntry::Imported {
                source_speaker:
                    ImportedSourceSpeaker::Attested {
                        speaker: ImportedSpeaker::User,
                    },
                ..
            },
        ..
    } = imported_text.message()
    else {
        panic!(
            "fixture expected imported text entry, got {:?}",
            imported_text.message()
        );
    };
    assert_eq!(entry_index.value(), 0);
    let content = response_within(&mut read_connection).await?;
    let ServerMessage::TranscriptContent {
        entry_index,
        fragment_index,
        final_fragment: true,
        content_fragment,
    } = content.message()
    else {
        panic!(
            "fixture expected imported text content, got {:?}",
            content.message()
        );
    };
    assert_eq!(entry_index.value(), 0);
    assert_eq!(fragment_index.value(), 0);
    assert_eq!(content_fragment.as_str(), IMPORTED_USER_CONTENT);
    let conservative = response_within(&mut read_connection).await?;
    let ServerMessage::TranscriptEntry {
        entry_index,
        entry:
            TranscriptEntry::Imported {
                source_speaker:
                    ImportedSourceSpeaker::Attested {
                        speaker: ImportedSpeaker::Assistant,
                    },
                content_kind: ImportedContentKind::ToolCall,
                ..
            },
        ..
    } = conservative.message()
    else {
        panic!(
            "fixture expected conservative imported entry, got {:?}",
            conservative.message()
        );
    };
    assert_eq!(entry_index.value(), 1);
    let end = response_within(&mut read_connection).await?;
    assert_eq!(end.version(), ProtocolVersion::One);
    let ServerMessage::TranscriptSnapshotEnd {
        turn_count,
        entry_count,
        ..
    } = end.message()
    else {
        panic!("fixture expected transcript end, got {:?}", end.message());
    };
    assert_eq!(turn_count.value(), 0);
    assert_eq!(entry_count.value(), 2);

    drop(read_connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn submit_accepts_imported_session_continuation() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;

    let mut submit_connection = Connection::connect(runtime.socket()).await?;
    submit_connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: UserInputContent::text(String::from("native continuation")),
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            },
        )
        .await?;
    let accepted = response_within(&mut submit_connection).await?;
    assert_eq!(accepted.version(), ProtocolVersion::One);
    assert_eq!(submitted_session(accepted.message()), session_id);

    drop(submit_connection);
    runtime.stop().await
}

/// an equal imported-continuation replay is decided from its durable command before the current
/// deployment revalidates model settings.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn imported_session_replays_after_capability_removal() -> Result<(), Box<dyn Error>> {
    let mut runtime = RunningRuntime::start().await?;
    let fixture = ImportedInspectionFixture::insert(&runtime.pool).await?;
    let command_id = command()?;
    let requested_settings = ModelSettingsOverlay {
        reasoning_level: SettingOverlay::Value(ReasoningLevel::Low),
        fast_mode: signalbox_process_protocol::FastModeOverlay::Inherit,
        service_tier: SettingOverlay::Inherit,
    };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request(
            1,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id,
                imported_conversation_id: fixture.conversation,
                through_position: fixture.last_position,
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
            },
        )
        .await?;
    let applied = session_created_facts(response_within(&mut connection).await?.message());
    drop(connection);

    let configuration_without_reasoning =
        MODEL_CONFIGURATION.replace("reasoning_levels = [\"low\"]\n", "");
    let _recovered_turn_count = runtime
        .restart_with_model_configuration(&configuration_without_reasoning)
        .await?;
    let mut replay_connection = Connection::connect(runtime.socket()).await?;
    replay_connection
        .request(
            2,
            ClientRequest::CreateSessionFromImportedFrontier {
                command_id,
                imported_conversation_id: fixture.conversation,
                through_position: fixture.last_position,
                relationship: signalbox_process_protocol::ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: primary_direct_selection_id(),
                },
                model_settings: requested_settings,
            },
        )
        .await?;
    let replayed = session_created_facts(response_within(&mut replay_connection).await?.message());

    assert_eq!(replayed, applied);

    drop(replay_connection);
    runtime.stop().await
}

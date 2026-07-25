#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{collections::VecDeque, error::Error};

use signalbox_application::{
    CreateSessionFromImportedFrontierIdGenerator, CreateSessionFromImportedFrontierOutcome,
    CreateSessionFromImportedFrontierRequest, CreateSessionFromImportedFrontierService,
    EligibilityNudge, EligibilityNudgeOutcome, ImportConversationOutcome,
    ImportConversationService, ImportedConversationIdGenerator, InProcessAttemptDispatchGate,
    InProcessToolDispatchGate, ModelCallCredentialReference, ModelCallExecutionIdGenerator,
    ModelCallExecutionOutcome, ModelCallExecutionService, ModelConversationMessage,
    ScriptedModelCallProvider, ScriptedModelCallStep, StartEligibleTurnIdGenerator,
    StartEligibleTurnOutcome, StartEligibleTurnService, SubmitInputIdGenerator, SubmitInputOutcome,
    SubmitInputRequest, SubmitInputService,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_domain::{
    AcceptedInputId, AssistantText, ContextFrontierId, DeliveryRequest, DirectModelSelection,
    DurableCommandId, ImportedConversationId, ImportedSessionRelationship,
    ImportedTranscriptEntryId, ModelCallId, ModelCallTerminalObservation, ModelSelectionOverride,
    ModelSelectionRequest, ModelTargetCatalog, ModelTargetDefinition, PerInputConfigurationChoices,
    ProviderModelIdentity, ResolvedProviderTarget, SemanticTranscriptEntryId,
    SemanticTranscriptEntryRef, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
    SessionId, SubmitInputAppliedResult, SubmitInputResult, TurnAttemptId, TurnId, UserContent,
};
use signalbox_persistence::{
    conversation_import::ImportedConversationRepository,
    create_session_from_imported_frontier::ImportedSessionRepository,
    local_test_connection_options, migrate,
    model_execution::PostgresModelCallRepository,
    session::{SessionRepository, SessionRepositoryError},
    start_eligible_turn::StartEligibleTurnRepository,
    submit_input::SubmitInputRepository,
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_imported_conversation_e2e";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

#[derive(Debug)]
struct FixedImportIds {
    conversations: VecDeque<ImportedConversationId>,
    entries: VecDeque<ImportedTranscriptEntryId>,
}

impl ImportedConversationIdGenerator for FixedImportIds {
    fn next_conversation_id(&mut self) -> ImportedConversationId {
        self.conversations
            .pop_front()
            .expect("one imported-conversation identity is supplied")
    }

    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
        self.entries
            .pop_front()
            .expect("one imported-entry identity is supplied per normalized entry")
    }
}

#[derive(Debug)]
struct FixedImportedSessionIds {
    sessions: VecDeque<SessionId>,
    semantic_entries: VecDeque<SemanticTranscriptEntryId>,
    frontiers: VecDeque<ContextFrontierId>,
}

impl CreateSessionFromImportedFrontierIdGenerator for FixedImportedSessionIds {
    fn next_session_id(&mut self) -> SessionId {
        self.sessions
            .pop_front()
            .expect("one imported-session identity is supplied")
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.semantic_entries
            .pop_front()
            .expect("one seed semantic identity is supplied per selected prefix entry")
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("one imported seed frontier identity is supplied")
    }
}

#[derive(Debug)]
struct FixedSubmitIds {
    accepted_inputs: VecDeque<AcceptedInputId>,
    turns: VecDeque<TurnId>,
    semantic_entries: VecDeque<SemanticTranscriptEntryId>,
    frontiers: VecDeque<ContextFrontierId>,
}

impl SubmitInputIdGenerator for FixedSubmitIds {
    fn next_accepted_input_id(&mut self) -> AcceptedInputId {
        self.accepted_inputs
            .pop_front()
            .expect("one accepted-input identity is supplied")
    }

    fn next_turn_id(&mut self) -> TurnId {
        self.turns
            .pop_front()
            .expect("one turn identity is supplied")
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.semantic_entries
            .pop_front()
            .expect("one cancellation semantic candidate is supplied")
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("one cancellation frontier candidate is supplied")
    }
}

#[derive(Debug)]
struct FixedActivationIds {
    origins: VecDeque<SemanticTranscriptEntryId>,
    frontiers: VecDeque<ContextFrontierId>,
    attempts: VecDeque<TurnAttemptId>,
}

impl StartEligibleTurnIdGenerator for FixedActivationIds {
    fn next_origin_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.origins
            .pop_front()
            .expect("one native origin identity is supplied per eligibility pass")
    }

    fn next_starting_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("one starting frontier identity is supplied per eligibility pass")
    }

    fn next_initial_attempt_id(&mut self) -> TurnAttemptId {
        self.attempts
            .pop_front()
            .expect("one initial attempt identity is supplied per eligibility pass")
    }
}

#[derive(Debug)]
struct FixedModelExecutionIds {
    calls: VecDeque<ModelCallId>,
    entries: VecDeque<SemanticTranscriptEntryId>,
    frontiers: VecDeque<ContextFrontierId>,
}

impl ModelCallExecutionIdGenerator for FixedModelExecutionIds {
    fn next_model_call_id(&mut self) -> ModelCallId {
        self.calls
            .pop_front()
            .expect("one call candidate is supplied per execution invocation")
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.entries
            .pop_front()
            .expect("the execution stage has every required semantic identity")
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("the execution stage has every required frontier identity")
    }

    fn next_turn_id(&mut self) -> TurnId {
        panic!("the fixture has no pending steering to reclassify")
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct AcceptingEligibilityNudge;

impl EligibilityNudge for AcceptingEligibilityNudge {
    fn nudge(&self, _session: SessionId) -> EligibilityNudgeOutcome {
        EligibilityNudgeOutcome::Enqueued
    }
}

fn input_choices() -> PerInputConfigurationChoices {
    PerInputConfigurationChoices::new(
        SessionConfigurationDefaultsVersion::first(),
        ModelSelectionOverride::UseSessionDefault,
    )
}

async fn assert_session_reloads(
    pool: &PgPool,
    session: SessionId,
) -> Result<(), SessionRepositoryError> {
    let loaded = SessionRepository::new(pool.clone())
        .load_session(session)
        .await?
        .expect("the completed imported session remains loadable");
    assert_eq!(loaded.id(), session);
    Ok(())
}

/// S28 / INV-002 / INV-015 / INV-038 / INV-039: synthetic Claude JSONL is
/// ingested losslessly, an interior imported boundary seeds one later session,
/// the exact prefix plus native origin reaches the provider, and the ordinary
/// native turn completes and reconstitutes from PostgreSQL.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn s28_inv002_inv015_inv038_inv039_import_seed_and_native_turn_complete_end_to_end()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x100));
    let imported_entries = [
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x200)),
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x201)),
        ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x202)),
    ];
    let source = concat!(
        "{\"type\":\"user\",\"message\":{\"content\":\"imported user\"}}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":\"imported assistant\"}}\n",
        "{\"type\":\"user\",\"message\":{\"content\":\"excluded later user\"}}"
    );
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
        .expect("the imported conversation is durable");
    assert_eq!(stored.raw_records().len(), 3);
    assert_eq!(stored.entries().len(), 3);
    let selected_frontier = stored
        .frontiers()
        .nth(1)
        .expect("the second of three boundaries is addressable");
    assert_eq!(selected_frontier.through_entry(), imported_entries[1]);

    let session = SessionId::from_uuid(Uuid::from_u128(0x300));
    let seed_semantic_entries = [
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x400)),
        SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x401)),
    ];
    let seed_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0x500));
    let direct_selection = DirectModelSelection::from_uuid(Uuid::from_u128(0xb00));
    let mut seed_service = CreateSessionFromImportedFrontierService::new(
        FixedImportedSessionIds {
            sessions: [session].into(),
            semantic_entries: seed_semantic_entries.into(),
            frontiers: [seed_frontier].into(),
        },
        ImportedSessionRepository::new(pool.clone()),
    );
    let CreateSessionFromImportedFrontierOutcome::Applied(created) = seed_service
        .execute(CreateSessionFromImportedFrontierRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0xc00)),
            selected_frontier,
            ImportedSessionRelationship::Resume,
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct_selection)),
        )?)
        .await?
    else {
        panic!("the selected interior imported boundary must seed a session");
    };
    assert_eq!(created.session(), session);

    let accepted_input = AcceptedInputId::from_uuid(Uuid::from_u128(0x600));
    let turn = TurnId::from_uuid(Uuid::from_u128(0x601));
    let native_content =
        UserContent::try_text("native continuation".to_owned()).expect("valid user content");
    let mut submit_service = SubmitInputService::new(
        FixedSubmitIds {
            accepted_inputs: [accepted_input].into(),
            turns: [turn].into(),
            semantic_entries: [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x602))].into(),
            frontiers: [ContextFrontierId::from_uuid(Uuid::from_u128(0x603))].into(),
        },
        SubmitInputRepository::new(pool.clone()),
        AcceptingEligibilityNudge,
        InProcessToolDispatchGate::default(),
    );
    let SubmitInputOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(origin),
    )) = submit_service
        .execute(SubmitInputRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0xc01)),
            session,
            native_content.clone(),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: input_choices(),
            },
        )?)
        .await?
    else {
        panic!("ordinary native input must be accepted by the imported session");
    };
    assert_eq!(origin.accepted_input(), accepted_input);
    assert_eq!(origin.turn(), turn);

    let origin_entry = SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x700));
    let starting_frontier = ContextFrontierId::from_uuid(Uuid::from_u128(0x701));
    let attempt = TurnAttemptId::from_uuid(Uuid::from_u128(0x702));
    let mut activation_service = StartEligibleTurnService::new(
        FixedActivationIds {
            origins: [origin_entry].into(),
            frontiers: [starting_frontier].into(),
            attempts: [attempt].into(),
        },
        StartEligibleTurnRepository::new(pool.clone()),
    );
    let StartEligibleTurnOutcome::Activated(activated) =
        activation_service.execute(session).await?
    else {
        panic!("the first native turn must activate from the imported seed");
    };
    assert_eq!(activated.turn(), turn);
    assert_eq!(activated.start().frontier().snapshot(), starting_frontier);
    let frontier_admission: (bool, bool) = sqlx::query_as(
        "SELECT
            first_native_starting_frontier_matches_seed($1, $2),
            first_native_starting_frontier_matches_seed($1, $3)",
    )
    .bind(session.into_uuid())
    .bind(seed_frontier.into_uuid())
    .bind(starting_frontier.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(frontier_admission, (false, true));

    let provider_identity = ProviderModelIdentity::from_uuid(Uuid::from_u128(0xb01));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        direct_selection,
        ResolvedProviderTarget::naming(provider_identity),
    )])
    .expect("one direct target forms a closed catalog");
    let model_repository = PostgresModelCallRepository::new(
        pool.clone(),
        targets,
        ModelCallCredentialReference::new("synthetic-provider-reference"),
    );
    let call = ModelCallId::from_uuid(Uuid::from_u128(0x800));
    let assistant_text =
        AssistantText::try_new("native assistant reply".to_owned()).expect("valid assistant text");
    let mut model_service = ModelCallExecutionService::new(
        FixedModelExecutionIds {
            calls: [call, ModelCallId::from_uuid(Uuid::from_u128(0x801))].into(),
            entries: [
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x900)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x901)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x902)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x903)),
            ]
            .into(),
            frontiers: [
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa00)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa01)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa02)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa03)),
                ContextFrontierId::from_uuid(Uuid::from_u128(0xa04)),
            ]
            .into(),
        },
        model_repository.clone(),
        model_repository.clone(),
        model_repository.clone(),
        model_repository,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![assistant_text.clone()],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
    );
    assert_eq!(
        model_service.execute(session).await?,
        ModelCallExecutionOutcome::Checkpointed(call)
    );
    assert!(matches!(
        model_service.execute(session).await?,
        ModelCallExecutionOutcome::ObservationCommitted(outcome)
            if matches!(*outcome, signalbox_domain::ModelCallTerminalOutcome::Completed(_))
    ));

    let (_, _, _, _, _, provider, _, _, retained) = model_service.into_parts();
    assert!(retained.is_none());
    let messages = provider
        .last_prepared_messages()
        .expect("the scripted provider observed one exact rendered frontier");
    assert_eq!(messages.len(), 3);
    assert!(matches!(
        &messages[0],
        ModelConversationMessage::ImportedUser {
            source,
            imported_entry,
            content,
        } if *source == SemanticTranscriptEntryRef::from_source(session, seed_semantic_entries[0])
            && *imported_entry == imported_entries[0]
            && content.as_str() == "imported user"
    ));
    assert!(matches!(
        &messages[1],
        ModelConversationMessage::ImportedAssistant {
            source,
            imported_entry,
            content,
        } if *source == SemanticTranscriptEntryRef::from_source(session, seed_semantic_entries[1])
            && *imported_entry == imported_entries[1]
            && content.as_str() == "imported assistant"
    ));
    assert!(matches!(
        &messages[2],
        ModelConversationMessage::User {
            source,
            accepted_input: rendered_input,
            content,
        } if *source == SemanticTranscriptEntryRef::from_source(session, origin_entry)
            && *rendered_input == accepted_input
            && content == &native_content
    ));

    let durable_terminal: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM context_frontier_member
              WHERE owning_session_id = $1
                AND context_frontier_id = $2),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1
                AND turn_id = $3
                AND state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'),
            (SELECT count(*)
               FROM semantic_transcript_entry
              WHERE source_session_id = $1
                AND payload_kind = 'imported_entry'),
            (SELECT count(*)
               FROM imported_transcript_entry
              WHERE imported_conversation_id = $4)",
    )
    .bind(session.into_uuid())
    .bind(starting_frontier.into_uuid())
    .bind(turn.into_uuid())
    .bind(conversation.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(durable_terminal, (3, 1, 2, 3));
    assert_session_reloads(&pool, session).await?;

    let mut terminal_reconstitution = StartEligibleTurnService::new(
        FixedActivationIds {
            origins: [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(0x704))].into(),
            frontiers: [ContextFrontierId::from_uuid(Uuid::from_u128(0x705))].into(),
            attempts: [TurnAttemptId::from_uuid(Uuid::from_u128(0x706))].into(),
        },
        StartEligibleTurnRepository::new(pool.clone()),
    );
    assert_eq!(
        terminal_reconstitution.execute(session).await?,
        StartEligibleTurnOutcome::NoEligibleTurn
    );

    pool.close().await;
    Ok(())
}

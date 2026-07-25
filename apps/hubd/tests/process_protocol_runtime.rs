#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{
    collections::VecDeque,
    error::Error,
    fs,
    io::{self, ErrorKind},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use signalbox_application::{
    CreateSessionFromImportedFrontierIdGenerator, CreateSessionFromImportedFrontierOutcome,
    CreateSessionFromImportedFrontierRequest, CreateSessionFromImportedFrontierService,
    ImportConversationOutcome, ImportConversationService, ImportedConversationIdGenerator,
    InProcessEligibilityWorkSource, InProcessToolDispatchGate,
};
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_domain::{
    ContextFrontierId, DirectModelSelection, DurableCommandId, ImportedConversationId,
    ImportedSessionRelationship, ImportedTranscriptEntryId, ModelSelectionRequest,
    SemanticTranscriptEntryId, SessionConfigurationDefaults, SessionId,
};
use signalbox_hubd::{
    HubModelConfiguration, LocalProcessListener, ProcessRuntime, ProcessRuntimeError,
};
use signalbox_persistence::{
    conversation_import::ImportedConversationRepository,
    create_session_from_imported_frontier::ImportedSessionRepository,
    local_test_connection_options, migrate, scheduler::PostgresEligibilitySweep,
};
use signalbox_process_protocol::{
    CanonicalU64, CanonicalUuid, ClientFrame, ClientRequest, CommandId, ErrorCode, InputContent,
    ModelSelection, RequestId, ServerFrame, ServerMessage, SessionEvent, TurnState,
    decode_server_line, encode_client_line,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::watch,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_process_runtime";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const MAX_SUBMITTED_INPUT_BYTES: usize = 1024 * 1024;
const OVERSIZED_SUBMITTED_INPUT_BYTES: usize = MAX_SUBMITTED_INPUT_BYTES + 1;
const MODEL_CONFIGURATION: &str = r#"
version = 1

[[models]]
selection_id = "00000000-0000-0000-0000-000000000001"
target_id = "00000000-0000-0000-0000-000000000003"
provider = "anthropic"
provider_model = "fixture-model"
max_output_tokens = 256

[[aliases]]
alias_id = "00000000-0000-0000-0000-000000000002"
selection_id = "00000000-0000-0000-0000-000000000001"
"#;

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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

struct SocketDirectory {
    directory: PathBuf,
    socket: PathBuf,
}

impl SocketDirectory {
    fn create() -> Result<Self, Box<dyn Error>> {
        let directory = PathBuf::from("/tmp").join(format!("signalbox-process-{}", Uuid::now_v7()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let socket = directory.join("hub.sock");
        Ok(Self { directory, socket })
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn cleanup(self) -> Result<(), Box<dyn Error>> {
        let mut lock = self.socket.into_os_string();
        lock.push(".lock");
        match fs::remove_file(PathBuf::from(lock)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::remove_dir(self.directory)?;
        Ok(())
    }
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Connection {
    async fn connect(path: &Path) -> Result<Self, Box<dyn Error>> {
        let stream = UnixStream::connect(path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    async fn request(
        &mut self,
        request_id: u64,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        self.request_for_version(3, request_id, request).await
    }

    async fn request_for_version(
        &mut self,
        version: u64,
        request_id: u64,
        request: ClientRequest,
    ) -> Result<(), Box<dyn Error>> {
        let frame =
            ClientFrame::try_new_for_version(version, RequestId::try_new(request_id)?, request)?;
        self.writer.write_all(&encode_client_line(&frame)?).await?;
        Ok(())
    }

    async fn response(&mut self) -> Result<ServerFrame, Box<dyn Error>> {
        let mut line = Vec::new();
        if self.reader.read_until(b'\n', &mut line).await? == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "the process server closed before its next frame",
            )
            .into());
        }
        Ok(decode_server_line(&line)?)
    }
}

fn command() -> Result<CommandId, Box<dyn Error>> {
    Ok(CommandId::try_from_uuid(Uuid::now_v7())?)
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
            .expect("one imported conversation identity is supplied")
    }

    fn next_entry_id(&mut self) -> ImportedTranscriptEntryId {
        self.entries
            .pop_front()
            .expect("one imported transcript identity is supplied")
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
            .expect("one imported session identity is supplied")
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        self.semantic_entries
            .pop_front()
            .expect("one seed semantic identity is supplied")
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        self.frontiers
            .pop_front()
            .expect("one seed frontier identity is supplied")
    }
}

async fn create_imported_session(pool: &PgPool) -> Result<CanonicalUuid, Box<dyn Error>> {
    let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(0x9100));
    let imported_entry = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(0x9101));
    let mut import = ImportConversationService::new(
        FixedImportIds {
            conversations: [conversation].into(),
            entries: [imported_entry].into(),
        },
        ClaudeCodeJsonlConverter,
        ImportedConversationRepository::new(pool.clone()),
    );
    assert_eq!(
        import
            .execute(br#"{"type":"user","message":{"content":"imported user"}}"#)
            .await?,
        ImportConversationOutcome::Inserted { conversation }
    );
    let (_, _, repository) = import.into_parts();
    let stored = repository
        .load(conversation)
        .await?
        .expect("the imported conversation is durable");
    let selected_frontier = stored
        .frontiers()
        .next()
        .expect("the one-entry imported boundary exists");

    let session = SessionId::from_uuid(Uuid::from_u128(0x9200));
    let mut create = CreateSessionFromImportedFrontierService::new(
        FixedImportedSessionIds {
            sessions: [session].into(),
            semantic_entries: [SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                0x9201,
            ))]
            .into(),
            frontiers: [ContextFrontierId::from_uuid(Uuid::from_u128(0x9202))].into(),
        },
        ImportedSessionRepository::new(pool.clone()),
    );
    let CreateSessionFromImportedFrontierOutcome::Applied(created) = create
        .execute(CreateSessionFromImportedFrontierRequest::try_new(
            DurableCommandId::from_uuid(Uuid::from_u128(0x9203)),
            selected_frontier,
            ImportedSessionRelationship::Resume,
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(Uuid::from_u128(1)),
            )),
        )?)
        .await?
    else {
        return Err(io::Error::other("the imported session fixture was not applied").into());
    };
    assert_eq!(created.session(), session);
    Ok(CanonicalUuid::from_uuid(session.into_uuid()))
}

struct RunningRuntime {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    socket_directory: SocketDirectory,
    shutdown: watch::Sender<bool>,
    runtime_task: JoinHandle<Result<(), ProcessRuntimeError>>,
    _work_source: InProcessEligibilityWorkSource<PostgresEligibilitySweep>,
}

impl RunningRuntime {
    async fn start() -> Result<Self, Box<dyn Error>> {
        let (container, pool) = postgres().await?;
        let socket_directory = SocketDirectory::create()?;
        let listener = LocalProcessListener::bind(socket_directory.socket())?;
        let sweep = PostgresEligibilitySweep::new(pool.clone());
        let (eligibility_nudge, work_source) = InProcessEligibilityWorkSource::new(sweep);
        let model_configuration = HubModelConfiguration::parse(MODEL_CONFIGURATION)?;
        let runtime = ProcessRuntime::new(
            listener,
            pool.clone(),
            eligibility_nudge,
            InProcessToolDispatchGate::default(),
            model_configuration,
        );
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let runtime_task = tokio::spawn(runtime.run(shutdown_receiver));
        Ok(Self {
            container,
            pool,
            socket_directory,
            shutdown,
            runtime_task,
            _work_source: work_source,
        })
    }

    fn socket(&self) -> &Path {
        self.socket_directory.socket()
    }

    async fn stop(self) -> Result<(), Box<dyn Error>> {
        self.shutdown.send(true)?;
        timeout(Duration::from_secs(10), self.runtime_task).await???;
        self.pool.close().await;
        self.socket_directory.cleanup()?;
        drop(self.container);
        Ok(())
    }
}

async fn create_alias_session(
    connection: &mut Connection,
) -> Result<CanonicalUuid, Box<dyn Error>> {
    connection
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Alias {
                    alias_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                },
            },
        )
        .await?;
    match connection.response().await?.message() {
        ServerMessage::SessionCreated { session_id } => Ok(*session_id),
        message => Err(io::Error::other(format!(
            "unexpected create-session fixture response: {message:?}"
        ))
        .into()),
    }
}

async fn submit_first_input(
    connection: &mut Connection,
    session_id: CanonicalUuid,
    content: String,
) -> Result<(CanonicalUuid, CanonicalUuid), Box<dyn Error>> {
    connection
        .request(
            2,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new(content),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    match connection.response().await?.message() {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            accepted_input_id,
            acceptance_position,
            turn_id,
        } if *submitted_session == session_id && acceptance_position.value() == 1 => {
            Ok((*accepted_input_id, *turn_id))
        }
        message => Err(io::Error::other(format!(
            "unexpected first-input fixture response: {message:?}"
        ))
        .into()),
    }
}

async fn response_within(connection: &mut Connection) -> Result<ServerFrame, Box<dyn Error>> {
    timeout(Duration::from_secs(5), connection.response()).await?
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_lists_the_alias_session_projection() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let alias_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));

    connection
        .request(2, ClientRequest::ListSessions {})
        .await?;

    let start = response_within(&mut connection).await?;
    assert!(matches!(start.message(), ServerMessage::SessionsStart {}));
    let summary = response_within(&mut connection).await?;
    assert!(matches!(
        summary.message(),
        ServerMessage::SessionSummary {
            session_id: listed,
            defaults_version,
            model_selection: ModelSelection::Alias {
                alias_id: listed_alias
            },
        } if *listed == session_id
            && defaults_version.value() == 1
            && *listed_alias == alias_id
    ));
    let end = response_within(&mut connection).await?;
    assert!(matches!(
        end.message(),
        ServerMessage::SessionsEnd { session_count } if session_count.value() == 1
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_rejects_oversized_submitted_input() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;

    connection
        .request(
            2,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new("x".repeat(OVERSIZED_SUBMITTED_INPUT_BYTES)),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;

    let response = response_within(&mut connection).await?;
    assert!(matches!(
        response.message(),
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn version_one_rejects_imported_session_operations_before_mutation_or_snapshot()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let session_id = create_imported_session(&runtime.pool).await?;
    let mut connection = Connection::connect(runtime.socket()).await?;

    connection
        .request_for_version(
            1,
            1,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new(String::from("must not be accepted")),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let submit = response_within(&mut connection).await?;
    assert!(matches!(
        submit.message(),
        ServerMessage::Error {
            code: ErrorCode::UnsupportedVersion,
            message,
            ..
        } if message.contains("version 2")
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*)
               FROM accepted_input
              WHERE session_id = $1",
        )
        .bind(session_id.into_uuid())
        .fetch_one(&runtime.pool)
        .await?,
        0,
        "the compatibility preflight must precede durable input acceptance"
    );

    connection
        .request_for_version(1, 2, ClientRequest::ReadTranscript { session_id })
        .await?;
    let read = response_within(&mut connection).await?;
    assert!(matches!(
        read.message(),
        ServerMessage::Error {
            code: ErrorCode::UnsupportedVersion,
            message,
            ..
        } if message.contains("version 2")
    ));

    connection
        .request_for_version(1, 3, ClientRequest::FollowSession { session_id })
        .await?;
    let follow = response_within(&mut connection).await?;
    assert!(matches!(
        follow.message(),
        ServerMessage::Error {
            code: ErrorCode::UnsupportedVersion,
            message,
            ..
        } if message.contains("version 2")
    ));

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_admits_exact_limit_submitted_input() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;

    let _submitted = submit_first_input(
        &mut connection,
        session_id,
        "x".repeat(MAX_SUBMITTED_INPUT_BYTES),
    )
    .await?;

    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn process_runtime_reads_one_queued_transcript_snapshot() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let content = "queued input".to_owned();
    let (accepted_input, turn) =
        submit_first_input(&mut connection, session_id, content.clone()).await?;

    connection
        .request(3, ClientRequest::ReadTranscript { session_id })
        .await?;

    let start = response_within(&mut connection).await?;
    assert!(matches!(
        start.message(),
        ServerMessage::TranscriptSnapshotStart {
            session_id: snapshot_session,
            cursor,
        } if *snapshot_session == session_id && cursor.value() == 2
    ));
    let queued_turn = response_within(&mut connection).await?;
    assert!(matches!(
        queued_turn.message(),
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state:
                TurnState::Queued {
                    accepted_input_id,
                    content: projected_content,
                },
        } if *turn_id == turn
            && acceptance_position.value() == 1
            && *accepted_input_id == accepted_input
            && projected_content.as_str() == content
    ));
    let end = response_within(&mut connection).await?;
    assert!(matches!(
        end.message(),
        ServerMessage::TranscriptSnapshotEnd {
            session_id: snapshot_session,
            cursor,
            turn_count,
            entry_count,
        } if *snapshot_session == session_id
            && cursor.value() == 2
            && turn_count.value() == 1
            && entry_count.value() == 0
    ));

    drop(connection);
    runtime.stop().await
}

/// S24 / INV-032: a follow subscription formed before its snapshot observes
/// the next committed outbox event strictly above that snapshot's cursor.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s24_process_runtime_follow_snapshot_handoff_has_no_race() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut commands = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut commands).await?;
    let first_content = "x".repeat(MAX_SUBMITTED_INPUT_BYTES);
    let (first_accepted_input, first_turn) =
        submit_first_input(&mut commands, session_id, first_content.clone()).await?;
    let mut follow = Connection::connect(runtime.socket()).await?;
    follow
        .request(5, ClientRequest::FollowSession { session_id })
        .await?;
    let follow_cursor = match follow.response().await?.message() {
        ServerMessage::TranscriptSnapshotStart {
            session_id: snapshot_session,
            cursor,
        } if *snapshot_session == session_id => cursor.value(),
        message => {
            return Err(io::Error::other(format!("unexpected follow start: {message:?}")).into());
        }
    };

    // The exact-limit queued content keeps the snapshot writer blocked after
    // its start frame. Commit the next update before draining the snapshot so
    // only a subscription formed before snapshot transmission can retain it.
    commands
        .request(
            6,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new("second input".to_owned()),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let second_accepted_input = match commands.response().await?.message() {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            accepted_input_id,
            acceptance_position,
            ..
        } if *submitted_session == session_id && acceptance_position.value() == 2 => {
            *accepted_input_id
        }
        message => {
            return Err(io::Error::other(format!("unexpected second submit: {message:?}")).into());
        }
    };

    let queued_turn = response_within(&mut follow).await?;
    assert!(matches!(
        queued_turn.message(),
        ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position,
            state:
                TurnState::Queued {
                    accepted_input_id,
                    content: projected_content,
                },
        } if *turn_id == first_turn
            && acceptance_position.value() == 1
            && *accepted_input_id == first_accepted_input
            && projected_content.as_str() == first_content
    ));
    let snapshot_end = response_within(&mut follow).await?;
    assert!(matches!(
        snapshot_end.message(),
        ServerMessage::TranscriptSnapshotEnd {
            session_id: snapshot_session,
            cursor,
            turn_count,
            entry_count,
        } if *snapshot_session == session_id
            && cursor.value() == follow_cursor
            && turn_count.value() == 1
            && entry_count.value() == 0
    ));

    let followed = response_within(&mut follow).await?;
    assert!(matches!(
        followed.message(),
        ServerMessage::SessionEvent {
            cursor,
            session_id: event_session,
            event:
                SessionEvent::InputAccepted {
                    accepted_input_id,
                    acceptance_position,
                    content,
                    ..
                },
        } if cursor.value() > follow_cursor
            && *event_session == session_id
            && *accepted_input_id == second_accepted_input
            && acceptance_position.value() == 2
            && content.as_str() == "second input"
    ));

    drop(commands);
    drop(follow);
    runtime.stop().await
}

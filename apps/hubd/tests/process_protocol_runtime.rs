#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{
    error::Error,
    fs,
    io::{self, ErrorKind},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use signalbox_application::InProcessEligibilityWorkSource;
use signalbox_hubd::{LocalProcessListener, ProcessRuntime};
use signalbox_persistence::{
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
    time::timeout,
};
use uuid::Uuid;

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_process_runtime";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const OVERSIZED_SUBMITTED_INPUT_BYTES: usize = 1024 * 1024 + 1;

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
        let frame = ClientFrame::try_new(RequestId::try_new(request_id)?, request)?;
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

/// S24 / INV-032 / INV-033: the guarded process runtime serves every
/// version-one operation, and a follow subscription formed before its snapshot
/// observes the next committed outbox event strictly above that snapshot's
/// cursor.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn s24_process_runtime_serves_snapshot_first_follow_without_a_race()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = postgres().await?;
    let socket_directory = SocketDirectory::create()?;
    let listener = LocalProcessListener::bind(socket_directory.socket())?;
    let sweep = PostgresEligibilitySweep::new(pool.clone());
    let (eligibility_nudge, _work_source) = InProcessEligibilityWorkSource::new(sweep);
    let runtime = ProcessRuntime::new(listener, pool.clone(), eligibility_nudge);
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let runtime_task = tokio::spawn(runtime.run(shutdown_receiver));

    let mut commands = Connection::connect(socket_directory.socket()).await?;
    let selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    commands
        .request(
            1,
            ClientRequest::CreateSession {
                command_id: command()?,
                initial_model_selection: ModelSelection::Direct { selection_id },
            },
        )
        .await?;
    let session_id = match commands.response().await?.message() {
        ServerMessage::SessionCreated { session_id } => *session_id,
        message => return Err(io::Error::other(format!("unexpected create: {message:?}")).into()),
    };

    commands.request(2, ClientRequest::ListSessions {}).await?;
    assert!(matches!(
        commands.response().await?.message(),
        ServerMessage::SessionsStart {}
    ));
    assert!(matches!(
        commands.response().await?.message(),
        ServerMessage::SessionSummary {
            session_id: listed,
            defaults_version,
            model_selection: ModelSelection::Direct {
                selection_id: listed_selection
            },
        } if *listed == session_id
            && defaults_version.value() == 1
            && *listed_selection == selection_id
    ));
    assert!(matches!(
        commands.response().await?.message(),
        ServerMessage::SessionsEnd { session_count } if session_count.value() == 1
    ));

    commands
        .request(
            30,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new("x".repeat(OVERSIZED_SUBMITTED_INPUT_BYTES)),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    assert!(matches!(
        commands.response().await?.message(),
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));

    commands
        .request(
            3,
            ClientRequest::SubmitInput {
                command_id: command()?,
                session_id,
                content: InputContent::new("first input".to_owned()),
                expected_defaults_version: CanonicalU64::new(1),
            },
        )
        .await?;
    let first_turn = match commands.response().await?.message() {
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            acceptance_position,
            turn_id,
            ..
        } if *submitted_session == session_id && acceptance_position.value() == 1 => *turn_id,
        message => {
            return Err(io::Error::other(format!("unexpected first submit: {message:?}")).into());
        }
    };

    let mut transcript = Connection::connect(socket_directory.socket()).await?;
    transcript
        .request(4, ClientRequest::ReadTranscript { session_id })
        .await?;
    let transcript_cursor = match transcript.response().await?.message() {
        ServerMessage::TranscriptSnapshotStart {
            session_id: snapshot_session,
            cursor,
        } if *snapshot_session == session_id => cursor.value(),
        message => {
            return Err(
                io::Error::other(format!("unexpected transcript start: {message:?}")).into(),
            );
        }
    };
    let mut saw_first_turn = false;
    timeout(Duration::from_secs(5), async {
        loop {
            match transcript.response().await?.message() {
                ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position,
                    state:
                        TurnState::Queued {
                            content,
                            accepted_input_id: _,
                        },
                } if *turn_id == first_turn
                    && acceptance_position.value() == 1
                    && content.as_str() == "first input" =>
                {
                    saw_first_turn = true;
                }
                ServerMessage::TranscriptSnapshotEnd {
                    session_id: snapshot_session,
                    cursor,
                    ..
                } if *snapshot_session == session_id && cursor.value() == transcript_cursor => {
                    return Ok::<(), Box<dyn Error>>(());
                }
                ServerMessage::Error { code, .. } => {
                    return Err(io::Error::other(format!("transcript failed: {code:?}")).into());
                }
                _ => {}
            }
        }
    })
    .await??;
    assert!(saw_first_turn);

    let mut follow = Connection::connect(socket_directory.socket()).await?;
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
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                follow.response().await?.message(),
                ServerMessage::TranscriptSnapshotEnd {
                    session_id: snapshot_session,
                    cursor,
                    ..
                } if *snapshot_session == session_id && cursor.value() == follow_cursor
            ) {
                return Ok::<(), Box<dyn Error>>(());
            }
        }
    })
    .await??;

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
    assert!(matches!(
        commands.response().await?.message(),
        ServerMessage::InputSubmitted {
            session_id: submitted_session,
            acceptance_position,
            ..
        } if *submitted_session == session_id && acceptance_position.value() == 2
    ));

    let followed = timeout(Duration::from_secs(5), follow.response()).await??;
    assert!(matches!(
        followed.message(),
        ServerMessage::SessionEvent {
            cursor,
            session_id: event_session,
            event:
                SessionEvent::InputAccepted {
                    acceptance_position,
                    content,
                    ..
                },
        } if cursor.value() > follow_cursor
            && *event_session == session_id
            && acceptance_position.value() == 2
            && content.as_str() == "second input"
    ));

    shutdown.send(true)?;
    timeout(Duration::from_secs(10), runtime_task).await???;
    drop(commands);
    drop(transcript);
    drop(follow);
    pool.close().await;
    socket_directory.cleanup()?;
    drop(container);
    Ok(())
}

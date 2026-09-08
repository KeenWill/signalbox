//! Terminal consumers for the three durable runner recovery commands.

use super::*;
use arguments::RunnerCommand;

pub(crate) async fn run(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    command: RunnerCommand,
) -> Result<(), ClientError> {
    let supplied = match &command {
        RunnerCommand::Status { page_size, after } => {
            return status(client, output, *page_size, after.clone()).await;
        }
        RunnerCommand::Replace { command_id, .. }
        | RunnerCommand::Abandon { command_id, .. }
        | RunnerCommand::Promote { command_id, .. } => *command_id,
    };
    let (command_id, generated) = command_identity(supplied)?;
    if generated {
        output.recovery_value("command_id", &command_id.into_uuid().to_string())?;
    }
    let request = match command {
        RunnerCommand::Status { .. } => {
            return Err(ClientError::Protocol(
                "runner status entered mutation dispatch",
            ));
        }
        RunnerCommand::Replace {
            session, revision, ..
        } => ClientRequest::ReplaceLostRunner {
            command_id,
            session_id: session,
            revision,
        },
        RunnerCommand::Abandon { session, .. } => ClientRequest::AbandonLostRunner {
            command_id,
            session_id: session,
        },
        RunnerCommand::Promote {
            enrollment_request, ..
        } => ClientRequest::PromotePendingRunner {
            command_id,
            enrollment_request_id: enrollment_request,
        },
    };
    let mut connection = client.mutation_request(request.clone()).await?;
    let message = connection.message().await.map_err(ClientError::mutation)?;
    let matches_request = match (&request, &message) {
        (
            ClientRequest::ReplaceLostRunner { session_id, .. },
            ServerMessage::RunnerReplacementReceipt {
                command_id: observed_command,
                session_id: observed_session,
                ..
            },
        )
        | (
            ClientRequest::AbandonLostRunner { session_id, .. },
            ServerMessage::RunnerAbandonmentReceipt {
                command_id: observed_command,
                session_id: observed_session,
                ..
            },
        ) => *observed_command == command_id && observed_session == session_id,
        (
            ClientRequest::PromotePendingRunner {
                enrollment_request_id,
                ..
            },
            ServerMessage::RunnerPromotionReceipt {
                command_id: observed_command,
                enrollment_request_id: observed_request,
                ..
            },
        ) => *observed_command == command_id && observed_request == enrollment_request_id,
        _ => false,
    };
    if matches_request {
        output.runner_recovery_receipt(&message)?;
        let rejected = matches!(
            message,
            ServerMessage::RunnerReplacementReceipt {
                outcome: signalbox_process_protocol::RunnerReplacementOutcome::Rejected { .. },
                ..
            } | ServerMessage::RunnerAbandonmentReceipt {
                outcome: signalbox_process_protocol::RunnerAbandonmentOutcome::Rejected { .. },
                ..
            } | ServerMessage::RunnerPromotionReceipt {
                outcome: signalbox_process_protocol::RunnerPromotionOutcome::Rejected { .. },
                ..
            }
        );
        if rejected {
            return Err(ClientError::Input(
                "runner recovery was rejected; see the recorded receipt",
            ));
        }
        return Ok(());
    }
    match message {
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail).mutation()),
        _ => Err(
            ClientError::Protocol("runner recovery returned an uncorrelated receipt").mutation(),
        ),
    }
}

async fn status(
    client: &mut ProcessClient,
    output: &mut Output<'_>,
    page_size: u32,
    after: Option<signalbox_process_protocol::RunnerStatusCursor>,
) -> Result<(), ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadRunnerStatus {
            page_size,
            after: after.clone(),
        })
        .await?;
    match connection.message().await? {
        ServerMessage::RunnerStatusStart {} => {}
        ServerMessage::Error {
            code,
            message,
            detail,
        } => return Err(ClientError::remote(code, message, detail)),
        _ => {
            return Err(ClientError::Protocol(
                "runner status omitted its page start",
            ));
        }
    }
    let mut spool = tempfile::tempfile()?;
    let mut page = RunnerStatusPage::new(page_size, after);
    loop {
        let frame = connection.frame().await?;
        let message = frame.message();
        if let ServerMessage::Error {
            code,
            message,
            detail,
        } = message
        {
            return Err(ClientError::remote(*code, message.clone(), *detail));
        }
        let ended = page.accept(message)?;
        spool.write_all(&encode_server_line(&frame)?)?;
        if ended {
            break;
        }
    }
    spool.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(spool);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        output.runner_status(decode_server_line(&line)?.message())?;
        line.clear();
    }
    Ok(())
}

struct RunnerStatusPage {
    page_size: u32,
    after: Option<signalbox_process_protocol::RunnerStatusCursor>,
    runner_count: u64,
    failure_count: u64,
    leak_count: u64,
    last: Option<signalbox_process_protocol::RunnerStatusCursor>,
}

impl RunnerStatusPage {
    fn new(page_size: u32, after: Option<signalbox_process_protocol::RunnerStatusCursor>) -> Self {
        Self {
            page_size,
            after,
            runner_count: 0,
            failure_count: 0,
            leak_count: 0,
            last: None,
        }
    }
    fn accept(&mut self, message: &ServerMessage) -> Result<bool, ClientError> {
        use signalbox_process_protocol::{RunnerOperationFailure, RunnerStatusCursor};
        let invalid = || ClientError::Protocol("runner status returned an inconsistent page");
        let cursor = match message {
            ServerMessage::RunnerStatus { status } => {
                self.runner_count += 1;
                match status {
                    signalbox_process_protocol::RunnerStatusFact::Enrollment {
                        runner_id, ..
                    } => RunnerStatusCursor::Enrollment {
                        runner_id: *runner_id,
                    },
                    signalbox_process_protocol::RunnerStatusFact::Placement {
                        session_id, ..
                    } => RunnerStatusCursor::Placement {
                        session_id: *session_id,
                    },
                }
            }
            ServerMessage::RunnerOperationFailure { failure } => {
                let RunnerOperationFailure::Provision { correlation, .. } = failure;
                self.failure_count += 1;
                RunnerStatusCursor::OperationFailure {
                    authorization_id: correlation.authorization_id,
                }
            }
            ServerMessage::RunnerWorkspaceLeak { leak } => {
                self.leak_count += 1;
                RunnerStatusCursor::WorkspaceLeak {
                    runner_id: leak.runner_id,
                    locator: leak.locator.clone(),
                    entry_digest: leak.entry_digest.clone(),
                }
            }
            ServerMessage::RunnerStatusEnd {
                runner_count,
                failure_count,
                leak_count,
                next_after,
            } => {
                if runner_count.value() != self.runner_count
                    || failure_count.value() != self.failure_count
                    || leak_count.value() != self.leak_count
                    || next_after
                        .as_ref()
                        .is_some_and(|cursor| Some(cursor) != self.last.as_ref())
                {
                    return Err(invalid());
                }
                return Ok(true);
            }
            _ => return Err(invalid()),
        };
        if self.runner_count + self.failure_count + self.leak_count > u64::from(self.page_size)
            || self
                .last
                .as_ref()
                .or(self.after.as_ref())
                .is_some_and(|prior| cursor_key(prior) >= cursor_key(&cursor))
        {
            return Err(invalid());
        }
        self.last = Some(cursor);
        Ok(false)
    }
}

fn cursor_key(
    cursor: &signalbox_process_protocol::RunnerStatusCursor,
) -> (u8, uuid::Uuid, &str, &str) {
    use signalbox_process_protocol::RunnerStatusCursor;
    match cursor {
        RunnerStatusCursor::Enrollment { runner_id } => (0, runner_id.into_uuid(), "", ""),
        RunnerStatusCursor::Placement { session_id } => (1, session_id.into_uuid(), "", ""),
        RunnerStatusCursor::OperationFailure { authorization_id } => {
            (2, authorization_id.into_uuid(), "", "")
        }
        RunnerStatusCursor::WorkspaceLeak {
            runner_id,
            locator,
            entry_digest,
        } => (3, runner_id.into_uuid(), locator, entry_digest.as_str()),
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use signalbox_process_protocol::{
        RunnerStatusCursor, RunnerWorkspaceLeak, RunnerWorkspaceLeakKind,
    };

    #[tokio::test]
    async fn runner_status_renders_only_a_complete_validated_inventory()
    -> Result<(), Box<dyn std::error::Error>> {
        use signalbox_process_protocol::decode_client_line;
        use tokio::{io::AsyncBufReadExt, net::UnixListener};
        const INVENTORY_COUNT: u64 = 2;
        for (page_size, end_count, succeeds) in [
            (2, Some(2), true),
            (1, Some(2), false),
            (2, Some(1), false),
            (2, None, false),
        ] {
            let directory = tempfile::tempdir()?;
            let socket = directory.path().join("client.sock");
            let listener = UnixListener::bind(&socket)?;
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                let mut line = Vec::new();
                reader.read_until(b'\n', &mut line).await?;
                let request = decode_client_line(&line).map_err(std::io::Error::other)?;
                let mut send = async |message| {
                    let frame = ServerFrame::try_new(request.request_id(), message)
                        .map_err(std::io::Error::other)?;
                    writer
                        .write_all(&encode_server_line(&frame).map_err(std::io::Error::other)?)
                        .await
                };
                send(ServerMessage::RunnerStatusStart {}).await?;
                for id in 1..=INVENTORY_COUNT {
                    send(ServerMessage::RunnerStatus {
                        status: signalbox_process_protocol::RunnerStatusFact::Enrollment {
                            runner_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(u128::from(
                                id,
                            ))),
                            enrollment_request_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(
                                u128::from(id),
                            )),
                            authority: signalbox_process_protocol::RunnerAuthorityState::Pending,
                            connection_health: None,
                        },
                    })
                    .await?;
                }
                if let Some(count) = end_count {
                    send(ServerMessage::RunnerStatusEnd {
                        runner_count: CanonicalU64::new(count),
                        failure_count: CanonicalU64::new(0),
                        leak_count: CanonicalU64::new(0),
                        next_after: None,
                    })
                    .await?;
                }
                Ok::<_, std::io::Error>(())
            });
            let mut client = ProcessClient::new(socket);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr, false);
            let result = status(&mut client, &mut output, page_size, None).await;
            server.await??;
            if succeeds {
                result?;
                assert_eq!(
                    String::from_utf8(stdout)?
                        .matches("runner_status\"")
                        .count(),
                    INVENTORY_COUNT as usize
                );
            } else {
                assert!(result.is_err());
                assert!(
                    stdout.is_empty(),
                    "unvalidated inventory must remain private"
                );
            }
        }
        Ok(())
    }

    fn failure() -> Result<ServerMessage, serde_json::Error> {
        serde_json::from_value(
            serde_json::json!({"type":"runner_operation_failure", "failure":{
                "operation_kind":"provision", "correlation":{
                    "authorization_id":"00000000-0000-0000-0000-000000000001",
                    "session_id":"00000000-0000-0000-0000-000000000002",
                    "runner_id":"00000000-0000-0000-0000-000000000003",
                    "placement_revision":"1", "registration_revision":"1", "repository":null,
                    "sandbox_profile":"ambient", "credential_profile":null
                }, "category":"sandbox_unavailable", "detail":{"code":"unavailable", "message":"[redacted]", "payload":{}}
            }}),
        )
    }

    fn leak() -> Result<ServerMessage, Box<dyn std::error::Error>> {
        Ok(ServerMessage::RunnerWorkspaceLeak {
            leak: RunnerWorkspaceLeak {
                runner_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(3)),
                kind: RunnerWorkspaceLeakKind::Unreconciled,
                locator: "sessions/work".to_owned(),
                entry_digest: serde_json::from_value(serde_json::json!(
                    "0000000000000000000000000000000000000000000000000000000000000000"
                ))?,
                session_id: None,
                placement_revision: None,
            },
        })
    }

    #[test]
    fn runner_status_bounds_and_continues_current_facts() -> Result<(), Box<dyn std::error::Error>>
    {
        let runner_id = CanonicalUuid::from_uuid(uuid::Uuid::from_u128(1));
        let fact = ServerMessage::RunnerStatus {
            status: signalbox_process_protocol::RunnerStatusFact::Enrollment {
                runner_id,
                enrollment_request_id: runner_id,
                authority: signalbox_process_protocol::RunnerAuthorityState::Pending,
                connection_health: None,
            },
        };
        let mut page = RunnerStatusPage::new(1, None);
        page.accept(&fact)?;
        assert!(
            page.accept(&failure()?).is_err(),
            "runner facts consume the shared budget"
        );
        let mut continuation =
            RunnerStatusPage::new(1, Some(RunnerStatusCursor::Enrollment { runner_id }));
        assert!(
            continuation.accept(&fact).is_err(),
            "enrollment continuation is exclusive"
        );
        let mut continuation =
            RunnerStatusPage::new(1, Some(RunnerStatusCursor::Enrollment { runner_id }));
        continuation.accept(&failure()?)?;
        assert!(
            continuation.accept(&fact).is_err(),
            "current facts cannot follow diagnostics"
        );
        Ok(())
    }

    #[test]
    fn runner_status_failures_cannot_follow_leaks() -> Result<(), Box<dyn std::error::Error>> {
        let mut page = RunnerStatusPage::new(100, None);
        page.accept(&failure()?)?;
        page.accept(&leak()?)?;
        assert!(page.accept(&failure()?).is_err());
        Ok(())
    }

    #[test]
    fn runner_status_continuation_is_exclusive() -> Result<(), Box<dyn std::error::Error>> {
        let cursor = RunnerStatusCursor::OperationFailure {
            authorization_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(1)),
        };
        let mut page = RunnerStatusPage::new(1, Some(cursor));
        assert!(page.accept(&failure()?).is_err());
        Ok(())
    }

    #[test]
    fn runner_status_checks_counts_and_last_cursor_before_rendering()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut page = RunnerStatusPage::new(1, None);
        page.accept(&failure()?)?;
        let end = ServerMessage::RunnerStatusEnd {
            runner_count: CanonicalU64::new(0),
            failure_count: CanonicalU64::new(0),
            leak_count: CanonicalU64::new(0),
            next_after: None,
        };
        assert!(page.accept(&end).is_err());
        let end = ServerMessage::RunnerStatusEnd {
            runner_count: CanonicalU64::new(0),
            failure_count: CanonicalU64::new(1),
            leak_count: CanonicalU64::new(0),
            next_after: Some(RunnerStatusCursor::OperationFailure {
                authorization_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(2)),
            }),
        };
        assert!(page.accept(&end).is_err());
        let end = ServerMessage::RunnerStatusEnd {
            runner_count: CanonicalU64::new(0),
            failure_count: CanonicalU64::new(1),
            leak_count: CanonicalU64::new(0),
            next_after: page.last.clone(),
        };
        assert!(page.accept(&end)?);
        Ok(())
    }
}

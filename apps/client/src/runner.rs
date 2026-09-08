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
    let mut page = RunnerStatusPage::new(page_size, after);
    loop {
        let message = connection.message().await?;
        if let ServerMessage::Error {
            code,
            message,
            detail,
        } = message
        {
            return Err(ClientError::remote(code, message, detail));
        }
        let ended = page.accept(&message)?;
        page.messages.push(message);
        if ended {
            break;
        }
    }
    for message in page.messages {
        output.runner_status(&message)?;
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
    messages: Vec<ServerMessage>,
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
            messages: Vec::new(),
        }
    }
    fn accept(&mut self, message: &ServerMessage) -> Result<bool, ClientError> {
        use signalbox_process_protocol::{RunnerOperationFailure, RunnerStatusCursor};
        let invalid = || ClientError::Protocol("runner status returned an inconsistent page");
        match message {
            ServerMessage::RunnerStatus { .. } => {
                if self.after.is_some() || self.failure_count != 0 || self.leak_count != 0 {
                    return Err(invalid());
                }
                self.runner_count += 1;
            }
            ServerMessage::RunnerOperationFailure { failure } => {
                if self.leak_count != 0
                    || matches!(self.after, Some(RunnerStatusCursor::WorkspaceLeak { .. }))
                {
                    return Err(invalid());
                }
                let RunnerOperationFailure::Provision { correlation, .. } = failure;
                let cursor = RunnerStatusCursor::OperationFailure {
                    authorization_id: correlation.authorization_id,
                };
                if let Some(RunnerStatusCursor::OperationFailure { authorization_id }) =
                    self.last.as_ref().or(self.after.as_ref())
                    && authorization_id.into_uuid() >= correlation.authorization_id.into_uuid()
                {
                    return Err(invalid());
                }
                self.last = Some(cursor);
                self.failure_count += 1;
            }
            ServerMessage::RunnerWorkspaceLeak { leak } => {
                if let Some(RunnerStatusCursor::WorkspaceLeak {
                    runner_id,
                    locator,
                    entry_digest,
                }) = self.last.as_ref().or(self.after.as_ref())
                    && (runner_id.into_uuid(), locator, entry_digest.as_str())
                        >= (
                            leak.runner_id.into_uuid(),
                            &leak.locator,
                            leak.entry_digest.as_str(),
                        )
                {
                    return Err(invalid());
                }
                self.last = Some(RunnerStatusCursor::WorkspaceLeak {
                    runner_id: leak.runner_id,
                    locator: leak.locator.clone(),
                    entry_digest: leak.entry_digest.clone(),
                });
                self.leak_count += 1;
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
        }
        if self.failure_count + self.leak_count > u64::from(self.page_size) {
            return Err(invalid());
        }
        Ok(false)
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use signalbox_process_protocol::{
        RunnerStatusCursor, RunnerWorkspaceLeak, RunnerWorkspaceLeakKind,
    };

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

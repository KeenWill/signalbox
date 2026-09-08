//! Operator read of retained runner facts.

use super::*;
use signalbox_persistence::runner_protocol::status::{self as store, RunnerStatusAfter};
use signalbox_process_protocol::{
    RunnerAuthorityState, RunnerFailureCategory, RunnerFailureDetail, RunnerOperationFailure,
    RunnerProvisionFailureCorrelation, RunnerStatusCursor, RunnerStatusFact,
};

pub(super) async fn handle_read_runner_status<Writer: AsyncWrite + Unpin>(
    writer: &mut Writer,
    version: ProtocolVersion,
    request_id: RequestId,
    page_size: u32,
    after: Option<RunnerStatusCursor>,
    services: &ConnectionServices,
    snapshot_permit: OwnedSemaphorePermit,
) -> Result<(), ProcessConnectionError> {
    let after = after.map(|cursor| match cursor {
        RunnerStatusCursor::Enrollment { runner_id } => {
            RunnerStatusAfter::Enrollment(runner_id.into_uuid())
        }
        RunnerStatusCursor::Placement { session_id } => {
            RunnerStatusAfter::Placement(session_id.into_uuid())
        }
        RunnerStatusCursor::OperationFailure { authorization_id } => {
            RunnerStatusAfter::OperationFailure(authorization_id.into_uuid())
        }
        RunnerStatusCursor::WorkspaceLeak { .. } => RunnerStatusAfter::WorkspaceLeak,
    });
    let spool = spool_runner_status(&services.pool, version, request_id, page_size, after).await;
    drop(snapshot_permit);
    match spool {
        Ok(mut file) => write_spooled_file(writer, &mut file).await,
        Err(code) => {
            write_error(
                writer,
                version,
                request_id,
                ProtocolError::without_detail(code),
            )
            .await
        }
    }
}

async fn spool_runner_status(
    pool: &PgPool,
    version: ProtocolVersion,
    request_id: RequestId,
    page_size: u32,
    after: Option<RunnerStatusAfter>,
) -> Result<tokio::fs::File, ErrorCode> {
    let page = store::read_runner_status(pool, page_size, after)
        .await
        .map_err(read_error)?;
    let mut file =
        tokio::fs::File::from_std(tempfile::tempfile().map_err(|_| ErrorCode::Unavailable)?);
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::RunnerStatusStart {},
    )
    .await
    .map_err(|_| ErrorCode::Unavailable)?;
    let runner_count = page.runners.len() as u64;
    for fact in page.runners {
        let message = project_fact(fact)?;
        write_spool_message(&mut file, version, request_id, message)
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
    }
    let failure_count = CanonicalU64::new(page.failures.len() as u64);
    for failure in page.failures {
        write_spool_message(&mut file, version, request_id, project_failure(failure)?)
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
    }
    write_spool_message(
        &mut file,
        version,
        request_id,
        ServerMessage::RunnerStatusEnd {
            runner_count: CanonicalU64::new(runner_count),
            failure_count,
            leak_count: CanonicalU64::new(0),
            next_after: page
                .next_after
                .map(|after| match after {
                    RunnerStatusAfter::Enrollment(id) => Ok(RunnerStatusCursor::Enrollment {
                        runner_id: wire_uuid(id),
                    }),
                    RunnerStatusAfter::Placement(id) => Ok(RunnerStatusCursor::Placement {
                        session_id: wire_uuid(id),
                    }),
                    RunnerStatusAfter::OperationFailure(id) => {
                        Ok(RunnerStatusCursor::OperationFailure {
                            authorization_id: wire_uuid(id),
                        })
                    }
                    RunnerStatusAfter::WorkspaceLeak => Err(ErrorCode::Internal),
                })
                .transpose()?,
        },
    )
    .await
    .map_err(|_| ErrorCode::Unavailable)?;
    file.flush().await.map_err(|_| ErrorCode::Unavailable)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    Ok(file)
}

fn read_error(error: store::RunnerStatusError) -> ErrorCode {
    match error {
        store::RunnerStatusError::InvalidPageSize => ErrorCode::InvalidRequest,
        store::RunnerStatusError::Database(_) => ErrorCode::Unavailable,
        store::RunnerStatusError::Corruption => ErrorCode::Internal,
    }
}

fn project_fact(status: store::RunnerStatusFact) -> Result<ServerMessage, ErrorCode> {
    use signalbox_domain::RunnerEnrollmentState as Authority;
    use signalbox_persistence::runner_protocol::RunnerConnectionState as Connection;
    use signalbox_process_protocol::RunnerConnectionHealth as Health;
    let status = match status {
        store::RunnerStatusFact::Enrollment {
            runner,
            request,
            authority,
            connection,
        } => RunnerStatusFact::Enrollment {
            runner_id: wire_uuid(runner.into_uuid()),
            enrollment_request_id: wire_uuid(request.into_uuid()),
            authority: match authority {
                Authority::Pending => RunnerAuthorityState::Pending,
                Authority::Active => RunnerAuthorityState::Active,
                Authority::Revoked => RunnerAuthorityState::Revoked,
            },
            connection_health: connection.map(|connection| match connection {
                Connection::Connected => Health::Connected,
                Connection::Suspect => Health::Suspect,
                Connection::Shutdown => Health::Shutdown,
                Connection::Lost => Health::Lost,
            }),
        },
        store::RunnerStatusFact::Placement { session, runner } => RunnerStatusFact::Placement {
            session_id: wire_uuid(session.into_uuid()),
            runner: super::transcript::wire_runner_projection(&runner)
                .map_err(|_| ErrorCode::Internal)?,
        },
    };
    Ok(ServerMessage::RunnerStatus { status })
}

fn project_failure(failure: store::RunnerStatusFailure) -> Result<ServerMessage, ErrorCode> {
    let authorization = failure.authorization;
    use signalbox_domain::RunnerProvisioningFailureKind as Category;
    let category = match failure.category {
        Category::CredentialUnavailable => RunnerFailureCategory::CredentialUnavailable,
        Category::RepositoryUnavailable => RunnerFailureCategory::RepositoryUnavailable,
        Category::SandboxUnavailable => RunnerFailureCategory::SandboxUnavailable,
        Category::WorkspaceConflict => RunnerFailureCategory::WorkspaceConflict,
    };
    let correlation = RunnerProvisionFailureCorrelation {
        authorization_id: wire_uuid(authorization.authorization.into_uuid()),
        session_id: wire_uuid(authorization.session.into_uuid()),
        placement_revision: authorization.placement_revision.into(),
        runner_id: wire_uuid(authorization.runner.into_uuid()),
        registration_revision: authorization.registration_revision.into(),
        repository: authorization
            .repository
            .map(|key| {
                signalbox_process_protocol::RunnerRepositoryKey::try_new(key.as_str().to_owned())
            })
            .transpose()
            .map_err(|_| ErrorCode::Internal)?,
        sandbox_profile: match authorization.sandbox {
            signalbox_domain::RunnerSandboxProfile::Ambient => {
                signalbox_process_protocol::RunnerSandboxProfile::Ambient
            }
            signalbox_domain::RunnerSandboxProfile::WorkspaceRestricted => {
                signalbox_process_protocol::RunnerSandboxProfile::WorkspaceRestricted
            }
        },
        credential_profile: authorization
            .credential_profile
            .map(|profile| {
                signalbox_process_protocol::RunnerCredentialProfileName::try_new(
                    profile.as_str().to_owned(),
                )
            })
            .transpose()
            .map_err(|_| ErrorCode::Internal)?,
    };
    Ok(ServerMessage::RunnerOperationFailure {
        failure: RunnerOperationFailure::Provision {
            correlation,
            category,
            detail: redact_detail(failure.detail)?,
        },
    })
}

fn redact_detail(value: serde_json::Value) -> Result<RunnerFailureDetail, ErrorCode> {
    let detail: signalbox_runner_wire::FailureDetail =
        serde_json::from_value(value).map_err(|_| ErrorCode::Internal)?;
    let detail =
        signalbox_runner_wire::FailureDetail::try_new(detail.code, detail.message, detail.payload)
            .map_err(|_| ErrorCode::Internal)?;
    let mut payload = detail.payload;
    redact_payload(&mut payload);
    Ok(RunnerFailureDetail {
        code: detail.code.as_str().to_owned(),
        message: "[redacted]".to_owned(),
        payload,
    })
}

fn redact_payload(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => text.clear(),
        serde_json::Value::Object(items) => {
            for value in items.values_mut() {
                redact_payload(value);
            }
        }
        serde_json::Value::Array(items) => {
            for value in items {
                redact_payload(value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runner_status_waits_for_shared_snapshot_reader_capacity()
    -> Result<(), Box<dyn std::error::Error>> {
        let request = ClientRequest::ReadRunnerStatus {
            page_size: 1,
            after: None,
        };
        let budget = Arc::new(Semaphore::new(1));
        let (_shutdown, mut shutdown) = watch::channel(false);
        let first = admit_snapshot_reader(&request, Arc::clone(&budget), &mut shutdown)
            .await?
            .expect("runtime is active")
            .expect("status reserves snapshot capacity");
        let mut second_shutdown = shutdown.clone();
        let mut second = Box::pin(admit_snapshot_reader(
            &request,
            Arc::clone(&budget),
            &mut second_shutdown,
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), second)
            .await??
            .expect("runtime remains active")
            .expect("next status reserves released capacity");
        assert_eq!(budget.available_permits(), 0);
        drop(second);
        assert_eq!(budget.available_permits(), 1);
        Ok(())
    }

    #[test]
    fn runner_status_redacts_nested_text_without_changing_retained_detail() {
        let retained = serde_json::json!({"code":"workspace_open_failed", "message":"cannot read /home/operator/project", "payload":{"path":"/private/credential", "nested":[{"reason":"file=/tmp/key", "tries":2}], "ready":false}});
        let projected = redact_detail(retained.clone()).expect("valid bounded detail");
        assert_eq!(projected.code, "workspace_open_failed");
        assert_eq!(projected.message, "[redacted]");
        assert_eq!(
            projected.payload,
            serde_json::json!({"path":"", "nested":[{"reason":"", "tries":2}], "ready":false})
        );
        assert_eq!(retained["payload"]["path"], "/private/credential");
    }

    #[test]
    fn runner_status_preserves_the_complete_retained_provision_correlation()
    -> Result<(), Box<dyn std::error::Error>> {
        use signalbox_domain::*;
        let authorization = RunnerReplacementProvisioning {
            authorization: RunnerProvisioningAuthorizationId::from_uuid(uuid::Uuid::now_v7()),
            command: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            enrollment: RunnerEnrollmentId::from_uuid(uuid::Uuid::now_v7()),
            registration_revision: RunnerGeneration::try_from_u64(1).unwrap(),
            session: SessionId::from_uuid(uuid::Uuid::now_v7()),
            placement_revision: RunnerGeneration::try_from_u64(1).unwrap(),
            runner: RunnerId::from_uuid(uuid::Uuid::now_v7()),
            repository: Some(WorkspaceRepositoryKey::try_new("project".to_owned()).unwrap()),
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            credential_profile: Some(
                CredentialProfileName::try_new("checkout".to_owned()).unwrap(),
            ),
            recovery: None,
        };
        let expected = RunnerProvisionFailureCorrelation {
            authorization_id: wire_uuid(authorization.authorization.into_uuid()),
            session_id: wire_uuid(authorization.session.into_uuid()),
            placement_revision: authorization.placement_revision.into(),
            runner_id: wire_uuid(authorization.runner.into_uuid()),
            registration_revision: authorization.registration_revision.into(),
            repository: Some(signalbox_process_protocol::RunnerRepositoryKey::try_new(
                "project".to_owned(),
            )?),
            sandbox_profile: signalbox_process_protocol::RunnerSandboxProfile::WorkspaceRestricted,
            credential_profile: Some(
                signalbox_process_protocol::RunnerCredentialProfileName::try_new(
                    "checkout".to_owned(),
                )?,
            ),
        };
        let projected = project_failure(store::RunnerStatusFailure {
            authorization,
            category: RunnerProvisioningFailureKind::RepositoryUnavailable,
            detail: serde_json::json!({"code":"repository_unavailable", "message":"/host/repository", "payload":{}}),
        }).expect("retained fixture projects");
        assert_eq!(
            projected,
            ServerMessage::RunnerOperationFailure {
                failure: RunnerOperationFailure::Provision {
                    correlation: expected,
                    category: RunnerFailureCategory::RepositoryUnavailable,
                    detail: RunnerFailureDetail {
                        code: "repository_unavailable".to_owned(),
                        message: "[redacted]".to_owned(),
                        payload: serde_json::json!({}),
                    },
                },
            }
        );
        Ok(())
    }

    #[test]
    fn runner_status_categories_serialize_every_runner_wire_category()
    -> Result<(), Box<dyn std::error::Error>> {
        use signalbox_runner_wire::FailureCategory as Wire;
        for category in [
            Wire::CredentialUnavailable,
            Wire::RepositoryUnavailable,
            Wire::SandboxUnavailable,
            Wire::WorkspaceConflict,
            Wire::WorkspaceCleanupFailed,
            Wire::LeaseAdmissionRefused,
        ] {
            let value = serde_json::to_value(category)?;
            let projected: RunnerFailureCategory = serde_json::from_value(value.clone())?;
            assert_eq!(serde_json::to_value(projected)?, value);
        }
        Ok(())
    }

    #[test]
    fn runner_status_rejects_corrupt_detail_before_redacting_it() {
        let retained =
            serde_json::json!({"code":"failure", "message":"message", "payload":{"tries":-1}});
        assert!(matches!(redact_detail(retained), Err(ErrorCode::Internal)));
    }
}

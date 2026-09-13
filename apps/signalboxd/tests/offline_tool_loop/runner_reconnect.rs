//! Canonical reconnect decisions through the production tool loop and registration service.

use super::*;
use signalbox_persistence::runner_protocol::RunnerConnectionTransition;
use signalbox_runner_wire::{
    Advertisement, CanonicalUuid, CapabilityName, DIGEST_VERSION, DirectiveAction, Enroll,
    LeaseClaim, LeasePhase, LeasePhaseKind, ReconnectInventory, ResultFrame, Resume,
    RetainedResult, SandboxProfile, TerminalResult, WireToolName,
};
use signalboxd::runner_protocol_runtime::{
    PostgresRunnerRegistrationService, RunnerEnrollmentResponse, RunnerRegistrationService as _,
};

#[derive(Clone, Copy, Debug)]
enum InventoryCase {
    Waiting,
    Received,
    Result,
    Started,
    Omitted,
}

#[derive(Clone, Copy, Debug)]
enum PriorConnection {
    Connected,
    Shutdown,
    Lost,
}

fn advertisement() -> Advertisement {
    Advertisement {
        default_working_directory: None,
        capability_classes: vec![
            CapabilityName::try_new("echo".to_owned()).expect("compiled class"),
        ],
        tools: vec![WireToolName::try_new("echo".to_owned()).expect("compiled tool")],
        workspace_capabilities: Vec::new(),
        sandbox_profiles: vec![SandboxProfile::Ambient],
        credential_profiles: Vec::new(),
        repositories: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn reconnect_phases_obey_recorded_results_and_connection_loss() -> Result<(), Box<dyn Error>>
{
    for prior in [
        PriorConnection::Connected,
        PriorConnection::Shutdown,
        PriorConnection::Lost,
    ] {
        for inventory in [
            InventoryCase::Waiting,
            InventoryCase::Received,
            InventoryCase::Result,
            InventoryCase::Started,
            InventoryCase::Omitted,
        ] {
            tokio::time::timeout(Duration::from_secs(60), check_reconnect(prior, inventory))
                .await??;
        }
    }
    Ok(())
}

async fn check_reconnect(
    prior: PriorConnection,
    inventory: InventoryCase,
) -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let fixture = ToolLoopFixture::with_creation_placement(
        DangerousToolAutoApproval::Disabled,
        None,
        migrated_postgres().await?,
        Some(super::runner_execution::placement(
            directory.path().to_owned(),
        )),
    )
    .await?;
    let service =
        PostgresRunnerRegistrationService::local(fixture.pool.clone()).expect("compiled catalog");
    let RunnerEnrollmentResponse::Active(receipt) = service
        .enroll(Enroll {
            request_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
            digest_version: DIGEST_VERSION,
            advertisement: advertisement(),
        })
        .await?
    else {
        panic!("first enrollment is active")
    };
    let dispatch_service = service.dispatch_service();
    let (catalog, executor) = offline_daemon_tools(
        OfflineWebTransport::unused(),
        UnusedSessionStatusWriter,
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let arguments = serde_json::json!({"text": "reconciled echo"}).to_string();
    let completes = !matches!(prior, PriorConnection::Lost)
        && matches!(
            inventory,
            InventoryCase::Waiting | InventoryCase::Received | InventoryCase::Result
        );
    let serial_successor =
        matches!(prior, PriorConnection::Connected) && matches!(inventory, InventoryCase::Result);
    if serial_successor {
        // A test-only registration stall exposes the interval between recording
        // the first result and opening the resumed physical connection.
        sqlx::raw_sql(
            "CREATE FUNCTION delay_resume_registration() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 PERFORM pg_sleep(0.2);
                 RETURN NEW;
             END $$;
             CREATE TRIGGER delay_resume_registration BEFORE INSERT ON runner_registration
             FOR EACH ROW EXECUTE FUNCTION delay_resume_registration();",
        )
        .execute(&fixture.pool)
        .await?;
    }
    let calls = if serial_successor {
        vec![("echo", arguments.as_str()), ("echo", arguments.as_str())]
    } else {
        vec![("echo", arguments.as_str())]
    };
    let mut scripts = vec![tool_use_script(&calls)];
    if completes {
        scripts.push(completion_script("observed"));
    }
    let (execution, runtime) = fixture.execution(
        scripts,
        catalog,
        executor.with_runner_dispatch(dispatch_service.clone()),
    );
    let execution = execution.with_runner_dispatch(dispatch_service);
    let reconcile = async {
        let offer = loop {
            if let Some(offer) = service
                .pending_tool_offer(receipt.enrollment_id, receipt.connection_epoch)
                .await?
            {
                break offer;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let (claimed, original_dispatch) = service
            .claim_tool_offer(
                receipt.enrollment_id,
                receipt.connection_epoch,
                LeaseClaim {
                    correlation: offer.correlation.clone(),
                },
            )
            .await?;
        assert_eq!(claimed.correlation, offer.correlation);
        match prior {
            PriorConnection::Connected => {}
            PriorConnection::Shutdown => {
                service
                    .transition_connection(
                        receipt.enrollment_id,
                        receipt.connection_epoch,
                        RunnerConnectionTransition::RunnerShutdown,
                    )
                    .await?;
            }
            PriorConnection::Lost => {
                service
                    .transition_connection(
                        receipt.enrollment_id,
                        receipt.connection_epoch,
                        RunnerConnectionTransition::TransportClosed,
                    )
                    .await?;
            }
        }
        let result = TerminalResult::Success {
            text: arguments.clone(),
        };
        let mut resumed_advertisement = advertisement();
        resumed_advertisement.default_working_directory =
            Some(directory.path().to_str().expect("fixture path").to_owned());
        let request = Resume {
            request_id: receipt.request_id,
            digest_version: DIGEST_VERSION,
            enrollment_id: receipt.enrollment_id,
            runner_id: receipt.runner_id,
            authentication_id: receipt.authentication_id,
            advertisement: resumed_advertisement,
            prior_registration_revision: receipt.registration_revision,
            inventory: ReconnectInventory {
                lease: (!matches!(inventory, InventoryCase::Omitted)).then(|| LeasePhase {
                    correlation: offer.correlation.clone(),
                    phase: match inventory {
                        InventoryCase::Waiting => LeasePhaseKind::WaitingDispatch,
                        InventoryCase::Received => LeasePhaseKind::DispatchReceived,
                        _ => LeasePhaseKind::ExecutionMayHaveStarted,
                    },
                }),
                result: matches!(inventory, InventoryCase::Result).then(|| RetainedResult {
                    correlation: offer.correlation.clone(),
                    result: result.clone(),
                }),
                ..Default::default()
            },
        };
        let first = service.resume(request.clone()).await;
        let resumed = if matches!(prior, PriorConnection::Shutdown) && !completes {
            assert_eq!(
                first.expect_err("a closed epoch is not rewritten to lost"),
                signalboxd::runner_protocol_runtime::RunnerRegistrationFailure::resume(
                    signalbox_runner_wire::AvailableCorrelation::ConnectionEpoch(
                        receipt.connection_epoch
                    ),
                    signalbox_runner_wire::RejectionCode::Unavailable
                )
            );
            service.resume(request.clone()).await?
        } else {
            first?
        };
        resumed.directives.validate_against(&request.inventory)?;
        assert!(resumed.connection_epoch > receipt.connection_epoch);
        assert_eq!(
            resumed.registration_revision.get(),
            receipt.registration_revision.get() + 1
        );
        if let Some(directive) = &resumed.directives.lease {
            assert_eq!(
                directive.action,
                if !completes {
                    DirectiveAction::FailStale
                } else if matches!(inventory, InventoryCase::Result) {
                    DirectiveAction::DiscardAsRecorded
                } else {
                    DirectiveAction::Await
                },
                "{prior:?}/{inventory:?}"
            );
        }
        if completes {
            if !matches!(inventory, InventoryCase::Result) {
                let (_, replayed) = service
                    .claim_tool_offer(
                        receipt.enrollment_id,
                        resumed.connection_epoch,
                        LeaseClaim {
                            correlation: offer.correlation.clone(),
                        },
                    )
                    .await?;
                assert_eq!(
                    replayed, original_dispatch,
                    "canonical dispatch is replayed unchanged"
                );
                service
                    .record_tool_result(
                        receipt.enrollment_id,
                        resumed.connection_epoch,
                        ResultFrame {
                            correlation: offer.correlation.clone(),
                            result: result.clone(),
                        },
                    )
                    .await?;
            }
            let recorded = service
                .record_tool_result(
                    receipt.enrollment_id,
                    resumed.connection_epoch,
                    ResultFrame {
                        correlation: offer.correlation.clone(),
                        result,
                    },
                )
                .await?;
            assert_eq!(recorded.correlation, offer.correlation);
            assert!(
                service
                    .record_tool_result(
                        receipt.enrollment_id,
                        resumed.connection_epoch,
                        ResultFrame {
                            correlation: offer.correlation,
                            result: TerminalResult::Success {
                                text: "conflicting result".to_owned()
                            }
                        }
                    )
                    .await
                    .is_err()
            );
            if serial_successor {
                let successor = loop {
                    if let Some(offer) = service
                        .pending_tool_offer(receipt.enrollment_id, resumed.connection_epoch)
                        .await?
                    {
                        break offer;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                assert_eq!(
                    successor.correlation.registration_revision, resumed.registration_revision,
                    "the next lease uses the resumed registration and physical epoch"
                );
                service
                    .claim_tool_offer(
                        receipt.enrollment_id,
                        resumed.connection_epoch,
                        LeaseClaim {
                            correlation: successor.correlation.clone(),
                        },
                    )
                    .await?;
                service
                    .record_tool_result(
                        receipt.enrollment_id,
                        resumed.connection_epoch,
                        ResultFrame {
                            correlation: successor.correlation,
                            result: TerminalResult::Success {
                                text: arguments.clone(),
                            },
                        },
                    )
                    .await?;
            }
        } else {
            assert!(
                service
                    .claim_tool_offer(
                        receipt.enrollment_id,
                        resumed.connection_epoch,
                        LeaseClaim {
                            correlation: offer.correlation
                        }
                    )
                    .await
                    .is_err()
            );
        }
        Ok::<_, Box<dyn Error>>(())
    };
    let (executed, reconciled) = tokio::join!(
        execution.execute(Box::new(fixture.activated.clone())),
        reconcile
    );
    reconciled?;
    executed?;
    assert_eq!(
        runtime.received_operations().len(),
        if completes { 2 } else { 1 },
        "{prior:?}/{inventory:?}"
    );
    if !completes {
        assert!(
            service
                .recovery_store()
                .load_runner_recovery_wait(fixture.session)
                .await?
                .is_some()
        );
    }
    Ok(())
}

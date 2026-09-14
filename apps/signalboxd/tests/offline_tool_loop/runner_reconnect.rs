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
    HistoricalResult,
}

#[derive(Clone, Copy, Debug)]
enum PriorConnection {
    Connected,
    RunnerShutdown,
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
        PriorConnection::RunnerShutdown,
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn historical_result_does_not_hide_an_omitted_successor_claim() -> Result<(), Box<dyn Error>>
{
    tokio::time::timeout(
        Duration::from_secs(60),
        check_reconnect(PriorConnection::Connected, InventoryCase::HistoricalResult),
    )
    .await?
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
    let completes = matches!(prior, PriorConnection::Connected)
        && matches!(
            inventory,
            InventoryCase::Waiting
                | InventoryCase::Received
                | InventoryCase::Result
                | InventoryCase::HistoricalResult
        );
    let historical_result = matches!(inventory, InventoryCase::HistoricalResult);
    let turn_completes = completes && !historical_result;
    let serial_successor = matches!(prior, PriorConnection::Connected)
        && matches!(
            inventory,
            InventoryCase::Result | InventoryCase::HistoricalResult
        );
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
    if turn_completes {
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
            PriorConnection::RunnerShutdown => {
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
                result: matches!(
                    inventory,
                    InventoryCase::Result | InventoryCase::HistoricalResult
                )
                .then(|| RetainedResult {
                    correlation: offer.correlation.clone(),
                    result: result.clone(),
                }),
                ..Default::default()
            },
        };
        let resumed = service.resume(request.clone()).await?;
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
                } else if matches!(
                    inventory,
                    InventoryCase::Result | InventoryCase::HistoricalResult
                ) {
                    DirectiveAction::DiscardAsRecorded
                } else {
                    DirectiveAction::Await
                },
                "{prior:?}/{inventory:?}"
            );
        }
        if completes {
            if !matches!(
                inventory,
                InventoryCase::Result | InventoryCase::HistoricalResult
            ) {
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
                if historical_result {
                    let reconciled = service.resume(request.clone()).await?;
                    assert!(reconciled.connection_epoch > resumed.connection_epoch);
                    assert_eq!(
                        reconciled
                            .directives
                            .result
                            .expect("historical result directive")
                            .action,
                        DirectiveAction::DiscardAsRecorded
                    );
                    assert_eq!(
                        service
                            .recovery_store()
                            .load_attempt_lease(signalbox_domain::ToolAttemptId::from_uuid(
                                successor.correlation.tool_attempt_id.into_uuid()
                            ))
                            .await?
                            .expect("omitted successor")
                            .state(),
                        signalbox_domain::RunnerLeaseState::LostClaimed
                    );
                    assert!(
                        service
                            .record_tool_result(
                                receipt.enrollment_id,
                                reconciled.connection_epoch,
                                ResultFrame {
                                    correlation: successor.correlation,
                                    result: TerminalResult::Success {
                                        text: arguments.clone()
                                    }
                                }
                            )
                            .await
                            .is_err()
                    );
                } else {
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
        if turn_completes { 2 } else { 1 },
        "{prior:?}/{inventory:?}"
    );
    if !turn_completes {
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn replacement_retries_a_parked_echo_and_wakes_continuation() -> Result<(), Box<dyn Error>> {
    let database = migrated_postgres().await?;
    tokio::time::timeout(Duration::from_secs(120), check_replacement_retry(database)).await?
}

async fn check_replacement_retry(database: (TestDatabase, PgPool)) -> Result<(), Box<dyn Error>> {
    use signalbox_application::{EligibilityWorkSource as _, InProcessEligibilityWorkSource};
    let directory = tempfile::tempdir()?;
    let fixture = ToolLoopFixture::with_creation_placement(
        DangerousToolAutoApproval::Disabled,
        None,
        database,
        Some(super::runner_execution::placement(
            directory.path().to_owned(),
        )),
    )
    .await?;
    let service =
        PostgresRunnerRegistrationService::local(fixture.pool.clone()).expect("compiled catalog");
    let RunnerEnrollmentResponse::Active(first) = service
        .enroll(Enroll {
            request_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
            digest_version: DIGEST_VERSION,
            advertisement: advertisement(),
        })
        .await?
    else {
        panic!("initial enrollment is active");
    };
    let dispatch = service.dispatch_service();
    let (catalog, executor) = offline_daemon_tools(
        OfflineWebTransport::unused(),
        UnusedSessionStatusWriter,
        UnusedCodeHostTransport,
        WebFetchEgressPolicy::deny_all(),
    )?
    .into_parts();
    let arguments = serde_json::json!({"text":"replacement echo"}).to_string();
    let runtime = Arc::new(ScriptedModel::<ModelCallId>::following([
        tool_use_script(&[("echo", arguments.as_str())]),
        completion_script("replacement observed"),
    ]));
    let provider = RuntimeModelCallProvider::new(
        RecordingScriptedModel {
            inner: Arc::clone(&runtime),
            shutdown_after_execute: None,
        },
        fixture.runtime_models.clone(),
        None,
    );
    let execution = PostgresProviderModelExecution::new(
        PostgresModelCallRepository::new(
            fixture.pool.clone(),
            fixture.targets.clone(),
            fixture.credential_reference.clone(),
        )
        .with_runner_recovery(service.recovery_store()),
        InProcessAttemptDispatchGate::default(),
        provider,
        None,
    )
    .with_tool_loop(
        dispatch.tool_dispatch_gate(),
        catalog,
        executor.with_runner_dispatch(dispatch.clone()),
    )
    .with_workspace_instructions(signalboxd::WorkspaceInstructionRuntime::new(
        fixture.pool.clone(),
        None,
        Vec::new(),
    ))
    .with_runner_dispatch(dispatch.clone());
    let lose = async {
        let offer = loop {
            if let Some(offer) = service
                .pending_tool_offer(first.enrollment_id, first.connection_epoch)
                .await?
            {
                break offer;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        service
            .claim_tool_offer(
                first.enrollment_id,
                first.connection_epoch,
                LeaseClaim {
                    correlation: offer.correlation.clone(),
                },
            )
            .await?;
        service
            .transition_connection(
                first.enrollment_id,
                first.connection_epoch,
                RunnerConnectionTransition::TransportClosed,
            )
            .await?;
        Ok::<_, Box<dyn Error>>(offer.correlation)
    };
    let (executed, lost) =
        tokio::join!(execution.execute(Box::new(fixture.activated.clone())), lose);
    executed?;
    let lost = lost?;
    let (nudge, mut work) = InProcessEligibilityWorkSource::with_options(
        signalbox_persistence::scheduler::PostgresEligibilitySweep::new(fixture.pool.clone()),
        None,
        None,
    );
    let service = service.with_eligibility_nudge(nudge);
    let successor = service
        .enroll(Enroll {
            request_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
            digest_version: DIGEST_VERSION,
            advertisement: advertisement(),
        })
        .await?;
    let (enrollment, epoch) = match successor {
        RunnerEnrollmentResponse::Active(receipt) => {
            (receipt.enrollment_id, receipt.connection_epoch)
        }
        RunnerEnrollmentResponse::Pending(receipt) => {
            (receipt.enrollment_id, receipt.connection_epoch)
        }
    };
    let replaced = service
        .recovery_store()
        .replace_lost_runner(signalbox_domain::ReplaceLostRunner {
            command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
            session: fixture.session,
            revision: None,
        })
        .await?;
    assert!(matches!(
        replaced,
        signalbox_persistence::runner_protocol::RunnerRecoveryOutcome::Recorded(
            signalbox_domain::ReplaceLostRunnerResult::Replaced { .. }
        )
    ));
    let retry = loop {
        if let Some(offer) = service.pending_tool_offer(enrollment, epoch).await? {
            break offer;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_ne!(retry.correlation.tool_attempt_id, lost.tool_attempt_id);
    assert_eq!(retry.correlation.lease_id, lost.lease_id);
    assert_eq!(
        retry.correlation.lease_generation.get(),
        lost.lease_generation.get() + 1
    );
    let gate = dispatch.tool_dispatch_gate();
    let mut stopping = Box::pin(gate.acquire(fixture.activated.turn()));
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(std::future::Future::poll(
            stopping.as_mut(),
            context
        )))
        .await
        .is_pending(),
        "the actual retry worker holds the stop gate until terminal settlement"
    );
    service
        .claim_tool_offer(
            enrollment,
            epoch,
            LeaseClaim {
                correlation: retry.correlation.clone(),
            },
        )
        .await?;
    service
        .record_tool_result(
            enrollment,
            epoch,
            ResultFrame {
                correlation: retry.correlation,
                result: TerminalResult::Success { text: arguments },
            },
        )
        .await?;
    assert_eq!(
        work.next().await?,
        fixture.session,
        "result commit wakes continuation without a periodic scan"
    );
    drop(stopping.await);
    execution.resume_active(fixture.session).await?;
    assert_eq!(runtime.received_operations().len(), 2);
    let relocations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_placement_boundary WHERE session_id = $1")
            .bind(fixture.session.into_uuid())
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(relocations, 1);
    Ok(())
}

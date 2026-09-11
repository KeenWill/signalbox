//! Pool exhaustion survives the socket snapshot, event, and policy read boundaries.
use super::*;
use signalbox_application::CommitModelCallObservationTransaction;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn pool_projection_snapshot_event_and_policy_agree_over_the_socket()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn_id) =
        submit_first_input(&mut connection, session_id, String::from("exhausted pool")).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let mut follower = Connection::connect(runtime.socket()).await?;
    follower
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    loop {
        if matches!(
            response_within(&mut follower).await?.message(),
            ServerMessage::TranscriptSnapshotEnd { .. }
        ) {
            break;
        }
    }
    sqlx::query("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','anthropic-primary','codex_home')").execute(&runtime.pool).await?;
    let configuration = support::parse_model_configuration(&MODEL_CONFIGURATION.replace(
        "on_pool_exhausted = \"park\"",
        "on_pool_exhausted = \"fail\"",
    ))?;
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("unused-fallback"),
    )
    .with_credential_pools(configuration.credential_pool_runtime_catalog());
    let outcome = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::now_v7()),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| panic!("fixture has no steering"),
        )
        .await?;
    assert!(matches!(
        outcome,
        PrepareInitialModelCallOutcome::PoolExhausted(_)
    ));
    let live = loop {
        let frame = response_within(&mut follower).await?;
        if let ServerMessage::SessionEvent {
            event: event @ SessionEvent::TurnCredentialPoolExhausted { .. },
            ..
        } = frame.message()
        {
            break event.clone();
        }
    };
    let messages = read_transcript_messages(&mut connection, 3, session_id).await?;
    let TurnState::FailedCredentialPoolExhausted {
        terminal_frontier_id,
        terminal_attempt_id,
        failure_entry_id,
        pool_policy_id,
        policy_members,
        members,
    } = turn_state_of(&messages, turn_id)
    else {
        panic!("typed exhaustion snapshot");
    };
    assert_eq!(policy_members, [String::from("anthropic-primary")]);
    assert_eq!(
        live,
        SessionEvent::TurnCredentialPoolExhausted {
            turn_id,
            terminal_frontier_id,
            terminal_attempt_id,
            failure_entry_id,
            pool_policy_id,
            policy_members: policy_members.clone(),
            members: members.clone()
        }
    );
    connection
        .request_version(
            ProtocolVersion::One,
            4,
            ClientRequest::ReadCredentialPoolPolicy {
                session_id,
                turn_id,
                pool_policy_id,
            },
        )
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        &ServerMessage::CredentialPoolPolicy {
            pool_policy_id,
            policy_members: policy_members.clone()
        }
    );
    let wrong_turn = CanonicalUuid::from_uuid(Uuid::now_v7());
    connection
        .request_version(
            ProtocolVersion::One,
            5,
            ClientRequest::ReadCredentialPoolPolicy {
                session_id,
                turn_id: wrong_turn,
                pool_policy_id,
            },
        )
        .await?;
    let rejection = response_within(&mut connection).await?;
    let ServerMessage::Error { detail, .. } = rejection.message() else {
        panic!("foreign turn must be rejected");
    };
    assert_eq!(
        *detail,
        signalbox_process_protocol::ErrorDetail::rejected(
            signalbox_process_protocol::RejectionDetail::UnknownPoolPolicy {
                session_id,
                turn_id: wrong_turn,
                pool_policy_id
            }
        )
    );
    drop(follower);
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn pool_projection_parked_wait_stays_readable_and_followable() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn_id) = submit_first_input(
        &mut connection,
        session_id,
        String::from("parked credential admission"),
    )
    .await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let mut prior_follower = Connection::connect(runtime.socket()).await?;
    prior_follower
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    loop {
        let frame = response_within(&mut prior_follower).await?;
        if matches!(frame.message(), ServerMessage::TranscriptSnapshotEnd { .. }) {
            break;
        }
    }
    sqlx::query("INSERT INTO credential_exclusion(kind,profile,origin) VALUES ('profile_quarantine','anthropic-primary','codex_home')").execute(&runtime.pool).await?;
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("unused-fallback"),
    )
    .with_credential_pools(configuration.credential_pool_runtime_catalog());
    let PrepareInitialModelCallOutcome::CredentialWait(wait) = repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::now_v7()),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| panic!("fixture has no steering"),
        )
        .await?
    else {
        panic!("clearable quarantine parks")
    };
    loop {
        let frame = response_within(&mut prior_follower).await?;
        if let ServerMessage::Error { code, .. } = frame.message() {
            assert_eq!(*code, ErrorCode::ResyncRequired);
            break;
        }
    }
    drop(prior_follower);
    let expected = TurnState::ActiveAwaitingCredentialAvailability {
        wait_attempt_id: CanonicalUuid::from_uuid(wait.attempt().into_uuid()),
        cause: signalbox_process_protocol::CredentialAvailabilityWaitCause::Exhausted,
    };
    let snapshot = read_transcript_messages(&mut connection, 3, session_id).await?;
    assert_eq!(turn_state_of(&snapshot, turn_id), expected);
    let mut follower = Connection::connect(runtime.socket()).await?;
    follower
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    let mut messages = Vec::new();
    loop {
        let frame = response_within(&mut follower).await?;
        let ended = matches!(frame.message(), ServerMessage::TranscriptSnapshotEnd { .. });
        messages.push(frame.message().clone());
        if ended {
            break;
        }
    }
    assert_eq!(turn_state_of(&messages, turn_id), expected);
    runtime.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn pool_projection_capacity_recovery_retains_live_groups_and_releases_ended_groups()
-> Result<(), Box<dyn Error>> {
    use signalbox_model_provider_runtime::InvocationProcessObserver;
    use signalbox_persistence::credential_invocations;
    use signalboxd::credential_invocations::CredentialInvocationProcesses;
    use std::num::NonZeroU32;
    use std::process::Stdio;

    let runtime = RunningRuntime::start().await?;
    credential_invocations::replace_registrations(
        &runtime.pool,
        &[(String::from("turn-control-fixture"), NonZeroU32::new(1))],
    )
    .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session = create_alias_session(&mut connection).await?;
    submit_first_input(
        &mut connection,
        session,
        String::from("reserved invocation"),
    )
    .await?;
    let (_, _, call) = authorize_issued_model_call(&runtime.pool, session).await?;
    // This fixture process waits for EOF without descendants or provider access.
    let mut child = tokio::process::Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let group = child.id().expect("spawned child has an identity");
    let observer =
        CredentialInvocationProcesses::new(runtime.pool.clone(), runtime.eligibility_nudge.clone());
    assert!(observer.register(call, group).await);
    observer.finished(call, None, true).await;
    observer.recover().await?;
    assert_eq!(
        credential_invocations::process_group(&runtime.pool, call)
            .await?
            .map(|(group, _)| group),
        Some(group),
        "completion and startup require proof that the invocation group ended"
    );
    drop(child.stdin.take());
    assert!(child.wait().await?.success());
    observer.recover().await?;
    assert_eq!(
        credential_invocations::process_group(&runtime.pool, call).await?,
        None
    );
    let released: bool = sqlx::query_scalar("SELECT released_at IS NOT NULL FROM credential_invocation_reservation WHERE model_call_id = $1")
        .bind(call.into_uuid()).fetch_one(&runtime.pool).await?;
    assert!(
        released,
        "startup releases only the proven-ended invocation"
    );
    runtime.stop().await
}

async fn prepare_projection_admission(
    repository: &PostgresModelCallRepository,
    session: SessionId,
) -> Result<PrepareInitialModelCallOutcome, Box<dyn Error>> {
    Ok(repository
        .prepare_initial_call(
            session,
            ModelCallId::from_uuid(Uuid::now_v7()),
            FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
            ),
            ContextFrontierId::from_uuid(Uuid::now_v7()),
            |_| panic!("fixture has no steering"),
        )
        .await?)
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn pool_projection_terminal_wait_release_correlates_the_predecessor_over_the_socket()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::credential_invocations;
    use signalbox_persistence::model_execution::{
        CredentialPoolRuntimeAction, CredentialPoolRuntimeExhaustion, CredentialPoolRuntimeMember,
        CredentialPoolRuntimePolicy,
    };
    use std::num::NonZeroU32;
    const FIRST: &str = "projection-first";
    const BOUNDED: &str = "projection-bounded";
    let runtime = RunningRuntime::start().await?;
    let configuration = support::parse_model_configuration(MODEL_CONFIGURATION)?;
    let make_repository = |members: &[&str]| {
        let pools = configuration
            .credential_pool_runtime_catalog()
            .into_keys()
            .map(|target| {
                (
                    target,
                    CredentialPoolRuntimePolicy::new(
                        String::from("projection-wait"),
                        members
                            .iter()
                            .map(|member| {
                                CredentialPoolRuntimeMember::new(
                                    (*member).to_owned(),
                                    NonZeroU32::new(1).expect("positive priority"),
                                )
                            })
                            .collect::<Vec<_>>(),
                        CredentialPoolRuntimeExhaustion::Fail,
                        CredentialPoolRuntimeAction::SwitchNow,
                        CredentialPoolRuntimeAction::SwitchNow,
                        CredentialPoolRuntimeAction::SwitchNow,
                        CredentialPoolRuntimeAction::Quarantine,
                    ),
                )
            })
            .collect();
        PostgresModelCallRepository::new(
            runtime.pool.clone(),
            configuration.target_catalog(),
            ModelCallCredentialReference::new("unused-fallback"),
        )
        .with_credential_pools(pools)
    };
    credential_invocations::replace_registrations(
        &runtime.pool,
        &[(BOUNDED.to_owned(), NonZeroU32::new(1))],
    )
    .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let donor_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, donor_id, String::from("hold capacity")).await?;
    let donor = SessionId::from_uuid(donor_id.into_uuid());
    activate_turn(&runtime.pool, donor).await?;
    let donor_repository = make_repository(&[BOUNDED]);
    let PrepareInitialModelCallOutcome::Checkpointed(donor_call) =
        prepare_projection_admission(&donor_repository, donor).await?
    else {
        panic!("donor prepares")
    };
    assert!(matches!(
        donor_repository.authorize_send(donor, donor_call).await?,
        AuthorizeModelCallOutcome::Authorized(_)
    ));
    let session_id = create_alias_session(&mut connection).await?;
    let (_, turn_id) = submit_first_input(
        &mut connection,
        session_id,
        String::from("wait release failure"),
    )
    .await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let mut repository = make_repository(&[FIRST, BOUNDED]);
    let PrepareInitialModelCallOutcome::Checkpointed(predecessor) =
        prepare_projection_admission(&repository, session).await?
    else {
        panic!("first member prepares")
    };
    let AuthorizeModelCallOutcome::Authorized(authorized) =
        repository.authorize_send(session, predecessor).await?
    else {
        panic!("first call authorizes")
    };
    repository
        .commit_observation(
            session,
            authorized
                .observation_correlation()
                .bind_provider_failure_observation_with_retry_after(
                    signalbox_domain::ProviderModelCallFailureCause::QuotaExhausted,
                    signalbox_domain::ProviderReportedTokenUsage::unreported(),
                    None,
                    true,
                ),
            signalbox_application::ModelCallTerminalIdentityCandidates::Availability {
                failed: FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                successor_attempt: signalbox_domain::TurnAttemptId::from_uuid(Uuid::now_v7()),
            },
            |_| panic!("fixture has no steering"),
        )
        .await?;
    let PrepareInitialModelCallOutcome::CredentialWait(wait) =
        prepare_projection_admission(&repository, session).await?
    else {
        panic!("bounded fallback parks")
    };
    let mut follower = Connection::connect(runtime.socket()).await?;
    follower
        .request_version(
            ProtocolVersion::One,
            1,
            ClientRequest::FollowSession { session_id },
        )
        .await?;
    let mut snapshot = Vec::new();
    loop {
        let frame = response_within(&mut follower).await?;
        let ended = matches!(frame.message(), ServerMessage::TranscriptSnapshotEnd { .. });
        snapshot.push(frame.message().clone());
        if ended {
            break;
        }
    }
    assert_eq!(
        turn_state_of(&snapshot, turn_id),
        TurnState::ActiveAwaitingCredentialAvailability {
            wait_attempt_id: CanonicalUuid::from_uuid(wait.attempt().into_uuid()),
            cause: signalbox_process_protocol::CredentialAvailabilityWaitCause::Contended,
        }
    );
    sqlx::query("INSERT INTO credential_pool_transient_exclusion (observation_model_call_id, credential_reference, cause_kind, reset_at) VALUES ($1,$2,'overloaded',transaction_timestamp() + interval '1 hour')")
        .bind(donor_call.into_uuid()).bind(BOUNDED).execute(&runtime.pool).await?;
    assert!(matches!(
        prepare_projection_admission(&repository, session).await?,
        PrepareInitialModelCallOutcome::WaitFailed(_)
    ));
    loop {
        let frame = response_within(&mut follower).await?;
        if let ServerMessage::Error { code, .. } = frame.message() {
            assert_eq!(*code, ErrorCode::ResyncRequired);
            break;
        }
    }
    let snapshot = read_transcript_messages(&mut connection, 3, session_id).await?;
    let TurnState::FailedAfterCredentialWait {
        terminal_attempt_id,
        predecessor_model_call,
        ..
    } = turn_state_of(&snapshot, turn_id)
    else {
        panic!("terminal release stays typed")
    };
    assert_ne!(terminal_attempt_id.into_uuid(), wait.attempt().into_uuid());
    assert_eq!(
        predecessor_model_call.model_call_id().into_uuid(),
        predecessor.into_uuid()
    );
    assert_eq!(
        predecessor_model_call.cause(),
        Some(signalbox_process_protocol::FailedModelCallCause::QuotaExhausted)
    );
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn capacity_reconciliation_observes_an_external_reset_while_the_model_is_parked()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::ReconciliationSweepInterval;
    use signalbox_domain::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
    use signalbox_persistence::credential_capacity::retain_credential_capacity_probe;
    use signalbox_persistence::model_execution::{
        CredentialPoolRuntimeAction, CredentialPoolRuntimeExhaustion, CredentialPoolRuntimeMember,
        CredentialPoolRuntimePolicy, CredentialPoolRuntimeTieBreak,
    };
    use signalboxd::credential_invocations::CodexCapacityRefresh;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};
    const PROFILE: &str = "capacity-home";
    let runtime = RunningRuntime::start().await?;
    let fixture = tempfile::tempdir()?;
    let executable = fixture.path().join("capacity-peer");
    std::fs::write(
        &executable,
        "#!/bin/sh\nread -r initialize\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nread -r initialized\nread -r capacity_read\ncat capacity-response\ncat > unexpected-input\n",
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let initial = SystemTime::now();
    let deadline = initial + Duration::from_secs(604800);
    let resets_at = deadline.duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
    let response = fixture.path().join("capacity-response");
    std::fs::write(
        &response,
        serde_json::json!({"id":2,"result":{"rateLimits":{"secondary":{"usedPercent":100,"resetsAt":resets_at}}}}).to_string() + "\n",
    )?;
    let configuration = support::parse_model_configuration(&format!(
        r#"{MODEL_CONFIGURATION}
[[credential_profiles]]
name = "{PROFILE}"
adapter = "codex_cli"
billing_kind = "subscription"
delivery = "codex_home"
codex_home = {home:?}
[codex_cli]
executable = {executable:?}
working_directory = {home:?}
"#,
        home = fixture.path().to_str().expect("test path"),
        executable = executable.to_str().expect("test path"),
    ))?;
    let pools = configuration
        .credential_pool_runtime_catalog()
        .into_keys()
        .map(|target| {
            (
                target,
                CredentialPoolRuntimePolicy::new(
                    String::from("capacity-pool"),
                    vec![CredentialPoolRuntimeMember::new(
                        PROFILE.to_owned(),
                        std::num::NonZeroU32::new(1).expect("positive priority"),
                    )],
                    CredentialPoolRuntimeExhaustion::Park,
                    CredentialPoolRuntimeAction::Stay,
                    CredentialPoolRuntimeAction::Stay,
                    CredentialPoolRuntimeAction::Stay,
                    CredentialPoolRuntimeAction::Stay,
                )
                .with_capacity_policy(
                    CredentialPoolRuntimeTieBreak::LeastUsed,
                    Some(0),
                    CredentialPoolRuntimeAction::Stay,
                ),
            )
        })
        .collect();
    let repository = PostgresModelCallRepository::new(
        runtime.pool.clone(),
        configuration.target_catalog(),
        ModelCallCredentialReference::new("unused-fallback"),
    )
    .with_credential_pools(pools);
    retain_credential_capacity_probe(
        &runtime.pool,
        PROFILE,
        &ProviderRateLimitSnapshot::new(
            initial,
            vec![ProviderRateLimitWindow::new(0, None, Some(deadline))],
        ),
    )
    .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let session_id = create_alias_session(&mut connection).await?;
    submit_first_input(&mut connection, session_id, String::from("await reset")).await?;
    let session = SessionId::from_uuid(session_id.into_uuid());
    activate_turn(&runtime.pool, session).await?;
    let PrepareInitialModelCallOutcome::CredentialWait(wait) =
        prepare_projection_admission(&repository, session).await?
    else {
        panic!("zero capacity parks")
    };
    let reload = signalboxd::configuration_reload::ConfigurationReload::new(
        runtime.pool.clone(),
        configuration.clone(),
        signalboxd::SessionTemplateConfiguration::default(),
        fixture.path().join("models.toml"),
        fixture.path().join("templates.toml"),
        None,
    )
    .map_err(|error| std::io::Error::other(format!("reload fixture: {error:?}")))?;
    let refresh = CodexCapacityRefresh::new(
        runtime.pool.clone(),
        reload,
        Some(ReconciliationSweepInterval::try_new(
            Duration::from_millis(10),
        )?),
        Duration::from_secs(5),
    );
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { refresh.run(receiver).await });
    wait_for_capacity_observation(&runtime.pool, PROFILE, initial, 0).await?;
    assert!(
        matches!(
            prepare_projection_admission(&repository, session).await?,
            PrepareInitialModelCallOutcome::CredentialWait(_)
        ),
        "an unchanged exhausted observation cannot release the wait"
    );
    let reset = SystemTime::now();
    let replacement = fixture.path().join("reset-response");
    std::fs::write(
        &replacement,
        serde_json::json!({"id":2,"result":{"rateLimits":{"secondary":{"usedPercent":0,"resetsAt":resets_at}}}}).to_string() + "\n",
    )?;
    std::fs::rename(replacement, &response)?;
    wait_for_capacity_observation(&runtime.pool, PROFILE, reset, 100).await?;
    shutdown.send(true)?;
    task.await?;
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM model_call WHERE session_id = $1")
        .bind(session.into_uuid())
        .fetch_one(&runtime.pool)
        .await?;
    assert_eq!(
        calls, 0,
        "periodic capacity reconciliation never dispatches a model"
    );
    let eligible: bool = sqlx::query_scalar("SELECT credential_wait_is_eligible($1)")
        .bind(wait.attempt().into_uuid())
        .fetch_one(&runtime.pool)
        .await?;
    assert!(eligible);
    assert!(matches!(
        prepare_projection_admission(&repository, session).await?,
        PrepareInitialModelCallOutcome::Checkpointed(_)
    ));
    runtime.stop().await?;
    Ok(())
}

async fn wait_for_capacity_observation(
    pool: &sqlx::PgPool,
    profile: &str,
    after: std::time::SystemTime,
    percent: i64,
) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = signalbox_persistence::credential_capacity::load_credential_rate_limits(
                &mut *pool.acquire().await?,
                profile,
            )
            .await?;
            if snapshot.is_some_and(|snapshot| {
                *snapshot.observed_at() > after
                    && *snapshot.windows()[0].remaining_percent() == percent
            }) {
                return Ok::<_, Box<dyn Error>>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?
}

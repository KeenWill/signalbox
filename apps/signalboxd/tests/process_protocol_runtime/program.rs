//! Program cancellation through the versioned process socket.
use super::*;
use signalbox_domain::ProgramRunId;
use signalbox_persistence::program_journal::ProgramJournalRepository;
use signalbox_process_protocol::{
    ProgramExecutableInput, ProgramGrant, ProgramRegistrationInput, ProgramRunState,
};
use signalbox_process_protocol::{ProgramRunCancellationOutcome, ProgramRunCancelledState};

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn evaluation_commands_print_sealed_scorecards_without_provider_access()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_evaluation().await?;
    let inputs = tempfile::tempdir()?;
    let corpus = inputs.path().join("corpus.json");
    let responses = inputs.path().join("responses.json");
    std::fs::write(
        &corpus,
        include_bytes!("../../../../crates/approval-judge-eval/corpora/seed-v1.json"),
    )?;
    std::fs::write(
        &responses,
        include_bytes!("../../../../crates/approval-judge-eval/corpora/seed-responses-v1.json"),
    )?;
    let output = tokio::process::Command::new(signalbox_test_bin::test_bin_path!(
        "signalbox-approval-judge-eval"
    ))
    .arg("--socket")
    .arg(runtime.socket())
    .arg(&corpus)
    .arg(&responses)
    .output()
    .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let scorecard: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let expected: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../crates/approval-judge-eval/corpora/seed-scorecard-v1.json"
    ))?;
    assert_eq!(scorecard, expected);
    let live = inputs.path().join("cases.jsonl");
    std::fs::write(
        &live,
        "{\"name\":\"synthetic-workflow-start\",\"category\":\"workflow_tools\",\"tool\":\"workflow_start\",\"arguments\":\"{\\\"name\\\":\\\"build\\\",\\\"revision\\\":\\\"1\\\",\\\"input\\\":[]}\",\"expected\":\"approve\",\"notes\":\"synthetic label\"}\ninvalid unselected row\n",
    )?;
    std::fs::write(
        &responses,
        r#"{"responses":[{"disposition":"approve","rationale":"Recorded permission."}]}"#,
    )?;
    let output =
        tokio::process::Command::new(signalbox_test_bin::test_bin_path!("approval-judge-eval"))
            .arg("--socket")
            .arg(runtime.socket())
            .arg("--cases")
            .arg(&live)
            .args(["--repeats", "1", "--limit", "1", "--responses"])
            .arg(&responses)
            .output()
            .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let live_scorecard: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(live_scorecard["total_cases"], 1);
    assert_eq!(live_scorecard["correct_majorities"], 1);
    assert_eq!(live_scorecard["failed_calls"], 0);
    assert_eq!(
        live_scorecard["categories"][0]["category"],
        "workflow_tools"
    );
    let sealed: Vec<sqlx::types::Json<serde_json::Value>> =
        sqlx::query_scalar("SELECT scorecard FROM evaluation_run")
            .fetch_all(&runtime.pool)
            .await?;
    assert_eq!(sealed.len(), 2);
    assert!(sealed.iter().any(|value| value.0 == expected));
    assert!(sealed.iter().any(|value| value.0 == live_scorecard));
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn evaluation_launch_rejects_corrupt_inputs_before_pinning() -> Result<(), Box<dyn Error>> {
    const RESPONSES: &[u8] =
        br#"{"responses":[{"disposition":"approve","rationale":"Synthetic permission."}]}"#;
    let mut fixture = CommittedBlobReadFixture::from_runtime(
        RunningRuntime::start_evaluation().await?,
        RESPONSES,
    )
    .await?;
    let corpus = br#"{"name":"synthetic-read","category":"workspace_benign","tool":"current_time","arguments":"{}","expected":"approve"}"#;
    let corpus_digest = CanonicalBlobDigest::from_digest(BlobDigest::digest(corpus));
    commit_blob_upload(
        &mut fixture.connection,
        corpus_digest,
        CanonicalU64::new(corpus.len() as u64),
        corpus,
    )
    .await?;
    // The replica remains valid JSON with its catalogued length but different content.
    let corrupt = std::str::from_utf8(RESPONSES)?.replace("Synthetic", "Corrupted");
    std::fs::write(fixture.object_path(), corrupt)?;
    let run_id = CanonicalUuid::from_uuid(uuid::Uuid::new_v4());
    let registration_id = CanonicalUuid::from_uuid(uuid::Uuid::new_v4());
    let request = ClientRequest::LaunchEvaluation {
        run_id,
        registration_id,
        input: signalbox_process_protocol::EvaluationInput {
            corpus: corpus_digest,
            format: signalbox_process_protocol::EvaluationCorpusFormat::Live,
            cases: vec![0],
            repeats: 1,
            recorded_responses: Some(fixture.wire_digest),
        },
    };
    assert!(matches!(
        program_request(&mut fixture.connection, request.clone()).await?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    std::fs::write(fixture.object_path(), RESPONSES)?;
    let corpus_path = fixture
        .runtime
        .blob_storage_root
        .as_ref()
        .unwrap()
        .store
        .join(BlobObjectKey::for_digest(corpus_digest.into_digest()).as_str());
    let corrupt = std::str::from_utf8(corpus)?.replace("current_time", "altered_time");
    assert_ne!(corrupt.as_bytes(), corpus);
    std::fs::write(&corpus_path, corrupt)?;
    assert!(matches!(
        program_request(&mut fixture.connection, request.clone()).await?,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    std::fs::write(corpus_path, corpus)?;
    assert!(matches!(
        program_request(&mut fixture.connection, request).await?,
        ServerMessage::ProgramRunStarted { .. }
    ));
    assert!(matches!(
        program_result(&mut fixture.connection, run_id)
            .await?
            .outcome,
        ProgramRunState::Succeeded { .. }
    ));
    fixture.runtime.stop().await
}

async fn program_request(
    connection: &mut Connection,
    request: ClientRequest,
) -> Result<ServerMessage, Box<dyn Error>> {
    connection
        .request_version(ProtocolVersion::One, 1, request)
        .await?;
    Ok(response_within(connection).await?.message().clone())
}

fn javascript_registration(artifact: &str) -> ProgramRegistrationInput {
    ProgramRegistrationInput {
        name: "socket-program".into(),
        revision: "1".into(),
        executable: ProgramExecutableInput::JavaScript {
            source: artifact.as_bytes().to_vec(),
            artifact: artifact.into(),
        },
        grants: vec![],
    }
}

async fn program_result(
    connection: &mut Connection,
    run_id: CanonicalUuid,
) -> Result<signalbox_process_protocol::ProgramRun, Box<dyn Error>> {
    timeout(RUNTIME_SETTLE_ALLOWANCE, async {
        loop {
            let message =
                program_request(connection, ClientRequest::ReadProgramRun { run_id }).await?;
            let ServerMessage::ProgramRunRead {
                run_id: observed,
                run,
            } = message
            else {
                return Err(format!("expected retained program run: {message:?}").into());
            };
            assert_eq!(observed, run_id);
            if !matches!(run.outcome, ProgramRunState::Running {}) {
                return Ok(run);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_javascript_admission_retries_preserve_input_and_result()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_programs().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let artifact = r#"import { defineProgram } from '@signalbox/program-sdk/v1';
        const codec = { decode(bytes) { if (bytes.length !== 3) throw Error('expected three bytes'); return bytes; }, encode(value) { return value; } };
        export default defineProgram({ input: codec, output: codec, async run(input) { return input; } });"#;
    let registration = javascript_registration(artifact);
    let request = ClientRequest::RegisterProgram {
        registration_id,
        registration: registration.clone(),
    };
    let registered = program_request(&mut connection, request.clone()).await?;
    assert_eq!(
        registered,
        ServerMessage::ProgramRegistered { registration_id }
    );
    assert_eq!(
        program_request(&mut connection, request).await?,
        registered,
        "equal registration retries return the same admission"
    );
    let input = vec![0, 255, 128]; // Exact codec fixture includes non-UTF-8 bytes.
    let start = ClientRequest::StartProgramRun {
        run_id,
        registration_id,
        input: input.clone(),
    };
    let started = program_request(&mut connection, start.clone()).await?;
    assert_eq!(
        started,
        ServerMessage::ProgramRunStarted {
            run_id,
            registration_id
        }
    );
    let retained = program_result(&mut connection, run_id).await?;
    assert_eq!(retained.registration_id, registration_id);
    assert_eq!(retained.input, input);
    assert_eq!(
        retained.outcome,
        ProgramRunState::Succeeded {
            result: input,
            result_extent: signalbox_process_protocol::ProgramByteExtent::Complete {}
        }
    );
    assert_eq!(
        program_request(&mut connection, start).await?,
        started,
        "equal start after completion returns the same admission"
    );
    assert_eq!(
        program_result(&mut connection, run_id).await?,
        retained,
        "retries preserve retained result"
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_conflicting_registrations_and_starts_are_refused() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_programs().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let registration = javascript_registration("");
    assert_eq!(
        program_request(
            &mut connection,
            ClientRequest::RegisterProgram {
                registration_id,
                registration: registration.clone()
            }
        )
        .await?,
        ServerMessage::ProgramRegistered { registration_id }
    );
    let mut changed = registration.clone();
    changed.grants = vec![ProgramGrant::Time];
    assert!(matches!(
        program_request(
            &mut connection,
            ClientRequest::RegisterProgram {
                registration_id,
                registration: changed
            }
        )
        .await?,
        ServerMessage::Error {
            code: ErrorCode::ConflictingReuse,
            ..
        }
    ));
    assert!(
        matches!(
            program_request(
                &mut connection,
                ClientRequest::RegisterProgram {
                    registration_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
                    registration
                }
            )
            .await?,
            ServerMessage::Error {
                code: ErrorCode::ConflictingReuse,
                ..
            }
        ),
        "name and revision cannot be rebound under another identity"
    );
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    program_request(
        &mut connection,
        ClientRequest::StartProgramRun {
            run_id,
            registration_id,
            input: vec![],
        },
    )
    .await?;
    assert!(
        matches!(
            program_request(
                &mut connection,
                ClientRequest::StartProgramRun {
                    run_id,
                    registration_id,
                    input: vec![1]
                }
            )
            .await?,
            ServerMessage::Error {
                code: ErrorCode::ConflictingReuse,
                ..
            }
        ),
        "start retries cannot change input"
    );
    drop(connection);
    runtime.stop().await
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_native_key_resolves_daemon_code_and_checks_input() -> Result<(), Box<dyn Error>> {
    use signalbox_workflow_runtime::native::NativeValue;
    use signalboxd::workflows::{ClockInput, ClockResult};
    let runtime = RunningRuntime::start_programs().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let registration = ProgramRegistrationInput {
        name: "clock-test".into(),
        revision: "1".into(),
        executable: ProgramExecutableInput::Native {
            entry: "clock".into(),
            revision: "1".into(),
        },
        grants: vec![ProgramGrant::Time],
    };
    assert_eq!(
        program_request(
            &mut connection,
            ClientRequest::RegisterProgram {
                registration_id,
                registration
            }
        )
        .await?,
        ServerMessage::ProgramRegistered { registration_id }
    );
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let input = ClockInput(731); // Arbitrary pilot input, distinct from the clock value.
    program_request(
        &mut connection,
        ClientRequest::StartProgramRun {
            run_id,
            registration_id,
            input: input.encode()?,
        },
    )
    .await?;
    let ProgramRunState::Succeeded { result, .. } =
        program_result(&mut connection, run_id).await?.outcome
    else {
        panic!("native clock must complete")
    };
    assert_eq!(ClockResult::decode(&result)?.input, input);
    let invalid = CanonicalUuid::from_uuid(Uuid::now_v7());
    program_request(
        &mut connection,
        ClientRequest::StartProgramRun {
            run_id: invalid,
            registration_id,
            input: vec![],
        },
    )
    .await?;
    assert_eq!(
        program_result(&mut connection, invalid).await?.outcome,
        ProgramRunState::Faulted {},
        "native input decoder rejects before code runs"
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_unknown_native_key_is_refused() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_programs().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let registration = ProgramRegistrationInput {
        name: "unavailable".into(),
        revision: "1".into(),
        executable: ProgramExecutableInput::Native {
            entry: "not-compiled".into(),
            revision: "1".into(),
        },
        grants: vec![],
    };
    assert!(matches!(
        program_request(
            &mut connection,
            ClientRequest::RegisterProgram {
                registration_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
                registration
            }
        )
        .await?,
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
async fn program_registration_effect_cannot_widen_user_assigned_grants()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_programs().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let child = Uuid::now_v7();
    let artifact = format!(
        r#"import {{ defineProgram, jsonCodec, register }} from '@signalbox/program-sdk/v1';
        export default defineProgram({{
            input: {{ decode(bytes) {{ return bytes; }} }},
            output: jsonCodec(value => value),
            async run() {{ return await register({{ id: '{child}', name: 'child', revision: '1', source: [], artifact: '', grants: ['time'] }}); }}
        }});"#
    );
    let mut registration = javascript_registration(&artifact);
    registration.grants = vec![ProgramGrant::Register];
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    assert_eq!(
        program_request(
            &mut connection,
            ClientRequest::RegisterProgram {
                registration_id,
                registration
            }
        )
        .await?,
        ServerMessage::ProgramRegistered { registration_id }
    );
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    program_request(
        &mut connection,
        ClientRequest::StartProgramRun {
            run_id,
            registration_id,
            input: vec![],
        },
    )
    .await?;
    let ProgramRunState::Succeeded { result, .. } =
        program_result(&mut connection, run_id).await?.outcome
    else {
        panic!("program retains the refusal result")
    };
    let result: serde_json::Value = serde_json::from_slice(&result)?;
    assert_eq!(
        result,
        serde_json::json!({"kind":"reject", "reason":"capability_denied"})
    );
    let registered: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM program_registration WHERE registration_id = $1)",
    )
    .bind(child)
    .fetch_one(&runtime.pool)
    .await?;
    assert!(
        !registered,
        "refused child registration must not retain widened grants"
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_read_retains_cancellation_and_its_command_identity() -> Result<(), Box<dyn Error>>
{
    use signalbox_domain::{
        ProgramRegistrationId,
        program_registration::{ProgramGrants, ProgramRegistrationRequest},
    };
    let runtime = RunningRuntime::start().await?;
    let registrations =
        signalbox_persistence::program_registration::ProgramRegistrationRepository::new(
            runtime.pool.clone(),
        );
    let registration = ProgramRegistrationId::from_uuid(Uuid::now_v7());
    registrations
        .register_user(
            registration,
            ProgramRegistrationRequest {
                name: "cancelled".into(),
                revision: "1".into(),
                source: vec![],
                artifact: String::new(),
                grants: ProgramGrants::new([]),
            },
        )
        .await?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let run = ProgramRunId::from_uuid(run_id.into_uuid());
    registrations
        .start_run(run, registration, b"retained input")
        .await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let command_id = command()?;
    let cancel = ClientRequest::CancelProgramRun { command_id, run_id };
    let receipt = program_request(&mut connection, cancel.clone()).await?;
    assert_eq!(
        receipt,
        ServerMessage::ProgramRunCancellationReceipt {
            command_id,
            run_id,
            outcome: ProgramRunCancellationOutcome::Applied {
                terminal_state: ProgramRunCancelledState::Cancelled,
                result: ()
            }
        }
    );
    let journal = ProgramJournalRepository::new(runtime.pool.clone());
    let cancelled = journal
        .load(run)
        .await?
        .expect("retained cancelled journal");
    assert_eq!(program_request(&mut connection, cancel).await?, receipt);
    assert_eq!(
        journal.load(run).await?.as_ref(),
        Some(&cancelled),
        "cancel retry appends no second delivery"
    );
    let retained = program_result(&mut connection, run_id).await?;
    assert_eq!(retained.input, b"retained input");
    assert_eq!(retained.outcome, ProgramRunState::Cancelled {});
    assert!(matches!(
        program_request(
            &mut connection,
            ClientRequest::CancelProgramRun {
                command_id,
                run_id: CanonicalUuid::from_uuid(Uuid::now_v7())
            }
        )
        .await?,
        ServerMessage::Error {
            code: ErrorCode::ConflictingReuse,
            ..
        }
    ));
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_cancellation_replays_its_committed_receipt() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    ProgramJournalRepository::new(runtime.pool.clone())
        .create_stream(ProgramRunId::from_uuid(run_id.into_uuid()))
        .await?;
    let command_id = command()?;
    let request = ClientRequest::CancelProgramRun { command_id, run_id };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(ProtocolVersion::One, 1, request.clone())
        .await?;
    let first = response_within(&mut connection).await?;
    assert_eq!(
        first.message(),
        &ServerMessage::ProgramRunCancellationReceipt {
            command_id,
            run_id,
            outcome: ProgramRunCancellationOutcome::Applied {
                terminal_state: ProgramRunCancelledState::Cancelled,
                result: ()
            },
        }
    );
    connection
        .request_version(ProtocolVersion::One, 2, request)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        first.message()
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn cancellation_of_success_returns_exact_result_on_every_retry() -> Result<(), Box<dyn Error>>
{
    let runtime = RunningRuntime::start().await?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let run = ProgramRunId::from_uuid(run_id.into_uuid());
    let journal = ProgramJournalRepository::new(runtime.pool.clone());
    journal.create_stream(run).await?;
    let result = b"retained output".to_vec();
    journal
        .complete_if_tail(
            run,
            0,
            signalbox_domain::InlineFramePayload::new(result.clone()),
        )
        .await?
        .expect("success");
    let command_id = command()?;
    let request = ClientRequest::CancelProgramRun { command_id, run_id };
    let mut connection = Connection::connect(runtime.socket()).await?;
    connection
        .request_version(ProtocolVersion::One, 1, request.clone())
        .await?;
    let first = response_within(&mut connection).await?;
    assert_eq!(
        first.message(),
        &ServerMessage::ProgramRunCancellationReceipt {
            command_id,
            run_id,
            outcome: ProgramRunCancellationOutcome::AlreadyTerminal(
                signalbox_process_protocol::ProgramRunTerminalState::Succeeded {
                    result,
                    result_extent: signalbox_process_protocol::ProgramByteExtent::Complete {}
                },
            ),
        }
    );
    connection
        .request_version(ProtocolVersion::One, 2, request)
        .await?;
    assert_eq!(
        response_within(&mut connection).await?.message(),
        first.message()
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_registration_rejects_nul_text_before_persistence() -> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_programs().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let valid = javascript_registration("");
    let mut name = valid.clone();
    name.name.push('\0');
    let mut revision = valid.clone();
    revision.revision.push('\0');
    let artifact = javascript_registration("\0");
    let mut native_entry = valid.clone();
    native_entry.executable = ProgramExecutableInput::Native {
        entry: "clock\0".into(),
        revision: "1".into(),
    };
    let mut native_revision = valid.clone();
    native_revision.executable = ProgramExecutableInput::Native {
        entry: "clock".into(),
        revision: "1\0".into(),
    };
    for (field, registration) in [
        ("name", name),
        ("revision", revision),
        ("artifact", artifact),
        ("native entry", native_entry),
        ("native revision", native_revision),
    ] {
        assert!(
            matches!(
                program_request(
                    &mut connection,
                    ClientRequest::RegisterProgram {
                        registration_id,
                        registration
                    }
                )
                .await?,
                ServerMessage::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                }
            ),
            "{field}"
        );
    }
    let mut registration = valid;
    // Source is binary content; only text fields exclude NUL.
    registration.executable = ProgramExecutableInput::JavaScript {
        source: vec![0],
        artifact: String::new(),
    };
    assert_eq!(
        program_request(
            &mut connection,
            ClientRequest::RegisterProgram {
                registration_id,
                registration
            }
        )
        .await?,
        ServerMessage::ProgramRegistered { registration_id }
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_start_reports_commit_ambiguity_when_the_runner_wake_is_closed()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start_stopped_programs().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    assert_eq!(
        program_request(
            &mut connection,
            ClientRequest::RegisterProgram {
                registration_id,
                registration: javascript_registration("")
            }
        )
        .await?,
        ServerMessage::ProgramRegistered { registration_id }
    );
    let input = vec![0, 255, 128]; // Exact stored bytes survive the post-commit wake failure.
    let request = ClientRequest::StartProgramRun {
        run_id,
        registration_id,
        input: input.clone(),
    };
    for attempt in ["initial start", "equal retry"] {
        assert!(
            matches!(
                program_request(&mut connection, request.clone()).await?,
                ServerMessage::Error {
                    code: ErrorCode::CommitAmbiguous,
                    ..
                }
            ),
            "{attempt}"
        );
    }
    let ServerMessage::ProgramRunRead { run, .. } =
        program_request(&mut connection, ClientRequest::ReadProgramRun { run_id }).await?
    else {
        panic!("committed run is readable");
    };
    assert_eq!(run.input, input);
    assert_eq!(run.outcome, ProgramRunState::Running {});
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_start_without_a_service_is_unavailable_before_admission()
-> Result<(), Box<dyn Error>> {
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    assert!(matches!(
        program_request(
            &mut connection,
            ClientRequest::StartProgramRun {
                run_id,
                registration_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
                input: vec![]
            }
        )
        .await?,
        ServerMessage::Error {
            code: ErrorCode::Unavailable,
            ..
        }
    ));
    assert!(
        ProgramJournalRepository::new(runtime.pool.clone())
            .load(ProgramRunId::from_uuid(run_id.into_uuid()))
            .await?
            .is_none()
    );
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_read_marks_large_retained_byte_prefixes_within_the_frame_budget()
-> Result<(), Box<dyn Error>> {
    use signalbox_domain::{
        ProgramRegistrationId,
        program_registration::{ProgramGrants, ProgramRegistrationRequest},
    };
    use signalbox_persistence::program_registration::ProgramRegistrationRepository;
    use signalbox_process_protocol::{MAX_FRAME_BYTES, ProgramByteExtent, encode_server_line};
    let runtime = RunningRuntime::start().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let registrations = ProgramRegistrationRepository::new(runtime.pool.clone());
    let registration_id = ProgramRegistrationId::from_uuid(Uuid::now_v7());
    registrations
        .register_user(
            registration_id,
            ProgramRegistrationRequest {
                name: "large-retained-read".into(),
                revision: "1".into(),
                source: vec![],
                artifact: String::new(),
                grants: ProgramGrants::new([]),
            },
        )
        .await?;
    // JSON decimal byte arrays exceed 8 MiB for these retained payloads.
    for (case, input, result, input_extent, result_extent) in [
        (
            "one-digit result",
            vec![0, 255],
            vec![0; 5 * 1024 * 1024],
            ProgramByteExtent::Complete {},
            ProgramByteExtent::Truncated {
                total_bytes: 5 * 1024 * 1024,
            },
        ),
        (
            "three-digit result",
            vec![0, 255],
            vec![255; 3 * 1024 * 1024],
            ProgramByteExtent::Complete {},
            ProgramByteExtent::Truncated {
                total_bytes: 3 * 1024 * 1024,
            },
        ),
        (
            "oversized retained input",
            vec![255; 3 * 1024 * 1024],
            vec![255; 3],
            ProgramByteExtent::Truncated {
                total_bytes: 3 * 1024 * 1024,
            },
            ProgramByteExtent::Truncated { total_bytes: 3 },
        ),
    ] {
        let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
        let run = ProgramRunId::from_uuid(run_id.into_uuid());
        registrations
            .start_run(run, registration_id, &input)
            .await?;
        let journal = ProgramJournalRepository::new(runtime.pool.clone());
        journal
            .complete_if_tail(
                run,
                0,
                signalbox_domain::InlineFramePayload::new(result.clone()),
            )
            .await?
            .expect("retained success");
        connection
            .request(u64::MAX, ClientRequest::ReadProgramRun { run_id })
            .await?;
        let frame = response_within(&mut connection).await?;
        assert!(
            encode_server_line(&frame)?.len() <= MAX_FRAME_BYTES,
            "{case}"
        );
        let ServerMessage::ProgramRunRead { run: observed, .. } = frame.message() else {
            panic!("expected bounded read: {case}");
        };
        assert_eq!(observed.input_extent, input_extent, "{case}");
        assert_eq!(observed.input, input[..observed.input.len()], "{case}");
        let ProgramRunState::Succeeded {
            result: prefix,
            result_extent: extent,
        } = &observed.outcome
        else {
            panic!("expected retained success: {case}");
        };
        assert_eq!(*extent, result_extent, "{case}");
        assert_eq!(*prefix, result[..prefix.len()], "{case}");
        connection
            .request(u64::MAX, ClientRequest::ReadProgramRun { run_id })
            .await?;
        assert_eq!(
            response_within(&mut connection).await?,
            frame,
            "read retries retain the same prefix: {case}"
        );
        assert_eq!(
            journal
                .load(run)
                .await?
                .expect("retained journal")
                .result()
                .expect("retained result")
                .as_bytes(),
            result,
            "read does not truncate storage: {case}"
        );
    }
    drop(connection);
    runtime.stop().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL and a local Unix socket"]
async fn program_cancellation_of_a_large_entrypoint_result_replays_a_bounded_receipt()
-> Result<(), Box<dyn Error>> {
    use signalbox_persistence::program_cancellation::{
        self, ProgramCancellationOutcome, ProgramCancellationResult, ProgramTerminalState,
    };
    use signalbox_process_protocol::{
        MAX_FRAME_BYTES, ProgramByteExtent, ProgramRunTerminalState, encode_server_line,
    };
    let runtime = RunningRuntime::start_programs().await?;
    let mut connection = Connection::connect(runtime.socket()).await?;
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    // Five MiB of zeros expands past the eight-MiB JSON frame limit.
    let artifact = r#"import { defineProgram } from '@signalbox/program-sdk/v1';
        const codec = { decode(bytes) { return bytes; }, encode(value) { return value; } };
        export default defineProgram({ input: codec, output: codec, async run() { return new Uint8Array(5 * 1024 * 1024); } });"#;
    assert_eq!(
        program_request(
            &mut connection,
            ClientRequest::RegisterProgram {
                registration_id,
                registration: javascript_registration(artifact)
            }
        )
        .await?,
        ServerMessage::ProgramRegistered { registration_id }
    );
    assert_eq!(
        program_request(
            &mut connection,
            ClientRequest::StartProgramRun {
                run_id,
                registration_id,
                input: vec![]
            }
        )
        .await?,
        ServerMessage::ProgramRunStarted {
            run_id,
            registration_id
        }
    );
    let retained = program_result(&mut connection, run_id).await?;
    assert!(matches!(
        retained.outcome,
        ProgramRunState::Succeeded { .. }
    ));
    let command_id = command()?;
    let request = ClientRequest::CancelProgramRun { command_id, run_id };
    connection.request(1, request.clone()).await?;
    let first = response_within(&mut connection).await?;
    assert!(encode_server_line(&first)?.len() <= MAX_FRAME_BYTES);
    let ServerMessage::ProgramRunCancellationReceipt {
        command_id: observed_command,
        run_id: observed_run,
        outcome:
            ProgramRunCancellationOutcome::AlreadyTerminal(ProgramRunTerminalState::Succeeded {
                result,
                result_extent,
            }),
    } = first.message()
    else {
        panic!("expected successful cancellation receipt");
    };
    assert_eq!(*observed_command, command_id);
    assert_eq!(*observed_run, run_id);
    assert_eq!(
        *result_extent,
        ProgramByteExtent::Truncated {
            total_bytes: 5 * 1024 * 1024
        }
    );
    assert!(!result.is_empty());
    assert!(result.len() < 5 * 1024 * 1024);
    assert!(result.iter().all(|byte| *byte == 0));
    // Changing only the correlation ID must not change the stored command's projection.
    connection.request(u64::MAX, request).await?;
    let retry = response_within(&mut connection).await?;
    assert!(encode_server_line(&retry)?.len() <= MAX_FRAME_BYTES);
    assert_eq!(retry.message(), first.message());
    let stored = program_cancellation::cancel(
        &runtime.pool,
        program_cancellation::CancelProgramRun {
            command_id: signalbox_domain::DurableCommandId::from_uuid(command_id.into_uuid()),
            run_id: ProgramRunId::from_uuid(run_id.into_uuid()),
        },
    )
    .await?;
    assert_eq!(
        stored,
        ProgramCancellationResult::Recorded(ProgramCancellationOutcome::AlreadyTerminal(
            ProgramTerminalState::Succeeded(signalbox_domain::InlineFramePayload::new(
                vec![0; 5 * 1024 * 1024]
            ))
        ))
    );
    drop(connection);
    runtime.stop().await
}

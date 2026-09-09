use std::{
    error::Error,
    ffi::OsString,
    fs,
    io::{self, Cursor},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use signalbox_process_protocol::{
    BlobChunk, BoundChildAction, CanonicalBlobDigest, CanonicalU64, CanonicalUuid, ClientFrame,
    ClientRequest, CommandId, ContentFragment, ConversationImportFormat, ConversationImportSource,
    ConversationOriginFilter, ConversationSummary, DelegationMessageDirection, DelegationOutcome,
    DelegationPolicy, DelegationProvenance, DelegationReason, DelegationWaitMode,
    DescendantTerminationScope, EffectiveModelSettings, ErrorCode, ErrorDetail, FastMode,
    FrameEncodeError, GoalCommandRejection, GoalHistoryEvent, GoalLifecycleState,
    ImportedContentKind, ImportedSessionRelationship, ImportedSourceSpeaker, InputContent,
    InputDelivery, MAX_BLOB_CHUNK_BYTES, MAX_CONVERSATION_IMPORT_CHUNK_BYTES, MAX_FRAME_BYTES,
    ModelCallDisposition, ModelCallState, ModelSelection, ModelSettingSource, ModelSettingsOverlay,
    ModelSettingsPrecedence, ModelSettingsSnapshot, ProtocolVersion, ReasoningLevel,
    RejectionDetail, RequestId, ReviewConcernTerminalOutcome, ReviewExternalObjectKind,
    ReviewFindingEvent, ReviewFindingInput, ReviewFindingSnapshot, ReviewFindingStatus,
    ReviewJudgmentEffectTerminalOutcome, ReviewOrchestrationState, ReviewPassKind,
    ReviewPassLifecycle, ReviewPassSnapshot, ReviewPassTerminalOutcome, ReviewRunLifecycle,
    ReviewRunSnapshot, ReviewSeverity, ReviewWorkflow, RunnerConnectionHealth,
    RunnerPlacementRevision, RunnerProjection, RunnerProjectionSelector, RunnerProjectionState,
    RunnerSandboxProfile, RunnerStateTransitionState, ServerFrame, ServerMessage, SessionEvent,
    SessionPlacement, SettingOverlay, SystemPromptMember, SystemPromptText, ToolBatchState,
    ToolDecision, TurnState, UserInputContent, decode_client_line, encode_server_line,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    time::timeout,
};
use uuid::Uuid;

fn provider_default_model_settings() -> ModelSettingsSnapshot {
    ModelSettingsSnapshot {
        precedence: ModelSettingsPrecedence {
            per_call: ModelSettingsOverlay::inherit_all(),
            session: ModelSettingsOverlay::inherit_all(),
            profile: ModelSettingsOverlay::inherit_all(),
            global_default: ModelSettingsOverlay::inherit_all(),
        },
        effective: EffectiveModelSettings {
            reasoning_level: None,
            fast_mode: FastMode::Disabled,
            service_tier: None,
        },
        reasoning_source: None,
        fast_mode_source: None,
        service_tier_source: None,
        validated_for_selection_id: None,
    }
}

fn session_reasoning_model_settings(selection_id: CanonicalUuid) -> ModelSettingsSnapshot {
    let mut session = ModelSettingsOverlay::inherit_all();
    session.reasoning_level = SettingOverlay::Value(ReasoningLevel::High);
    ModelSettingsSnapshot {
        precedence: ModelSettingsPrecedence {
            per_call: ModelSettingsOverlay::inherit_all(),
            session,
            profile: ModelSettingsOverlay::inherit_all(),
            global_default: ModelSettingsOverlay::inherit_all(),
        },
        effective: EffectiveModelSettings {
            reasoning_level: Some(ReasoningLevel::High),
            fast_mode: FastMode::Disabled,
            service_tier: None,
        },
        reasoning_source: Some(ModelSettingSource::Session),
        fast_mode_source: None,
        service_tier_source: None,
        validated_for_selection_id: Some(selection_id),
    }
}

use super::{
    ClientDeploymentLimits, ConversationImportOutcome, ConversationsPageRequest,
    DelegationRejectionExpectation, DelegationRejectionOperation, GoalHistoryReplay,
    MAX_CONTENT_FRAGMENT_BYTES, MAX_INPUT_CONTENT_FRAME_BYTES, MAX_REVIEW_JSON_INPUT_BYTES,
    MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES, ModelSystemPromptChoice, PreparedBlobSource,
    ProcessClient, ReviewCommand, ReviewConcernsFile, ReviewFindingsFile,
    SessionMetadataPageRequest, SnapshotSelection, SubmitInputReceipt, ThroughPositionArgument,
    TurnTerminal, await_turn_terminal, blocker_recovery_snapshot_state, collect_import_paths,
    continue_imported, conversation_import_chunk_read_limit, conversations, create, decide,
    decode_goal_mutation_receipt, delegation_rejection_matches, descendant_scope, hash_blob_source,
    import_conversation_file, imported, model_call_recovery_transition, open_blob_source,
    open_scanned_import_source, placement_update_receipt_matches,
    placement_update_rejection_matches, queued_turn_recovery, queued_turn_runner_recovery,
    read_blob_chunk, read_blob_metadata, read_delegation_content_file, read_deployment_limits,
    read_goal_text_file, read_import_file, read_input, read_review_json_file,
    read_system_prompt_file, reconcile_turn, replace_session_model,
    replacement_receipt_settings_match, review, review_concern_state_is_coherent,
    review_finding_event_status, review_judgment_effect_state_is_coherent,
    review_judgment_plan_state_is_coherent, review_pass_completion_is_coherent,
    review_publication_state_is_coherent, review_repair_state_is_coherent, run, search,
    selected_turn_recovery_transition, session_recovery_transition, socket_path,
    source_fits_single_shot_import, stop_turn, submit_input, terminal_event_state,
    terminal_snapshot_selection, terminal_snapshot_state, tool_recovery_transition, upload_blob,
    validate_message_policy, validate_metadata_page_policy, validate_review_finding_count,
    validate_system_prompt_policy, write_blob_output,
};
use crate::{
    child_lifecycle_terminalization, error::ClientError, presentation::Output,
    transcript::TranscriptSnapshot,
};

/// The session a follower reads. Only a delegation event addressed to this
/// exact session terminalizes the follower's own turn.
const FOLLOWED_SESSION_IDENTITY: u128 = 0x5e5;

fn followed_session() -> CanonicalUuid {
    CanonicalUuid::from_uuid(Uuid::from_u128(FOLLOWED_SESSION_IDENTITY))
}

fn runner_projection(
    revision: u64,
    state: RunnerProjectionState,
    connection_health: Option<RunnerConnectionHealth>,
) -> RunnerProjection {
    let runner_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    RunnerProjection::try_new(
        RunnerProjectionSelector::Runner { runner_id },
        Some(runner_id),
        RunnerPlacementRevision::try_new(revision)
            .expect("the fixture placement revision is positive"),
        RunnerSandboxProfile::WorkspaceRestricted,
        None,
        None,
        None,
        connection_health,
        state,
    )
    .expect("the fixture runner projection is coherent")
}

fn delegation_rejection_expectation(
    operation: DelegationRejectionOperation,
) -> DelegationRejectionExpectation {
    DelegationRejectionExpectation {
        session: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        turn: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        tool_request: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        operation,
    }
}

/// spawn rejection evidence names the exact logical tool request.
#[test]
fn spawn_rejection_requires_exact_tool_request() {
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Spawn);
    let exact = RejectionDetail::DelegationSpawnConflict {
        tool_request_id: expected.tool_request,
    };
    let cross_wired = RejectionDetail::DelegationSpawnConflict {
        tool_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
    };

    assert!(delegation_rejection_matches(Some(exact), expected));
    assert!(!delegation_rejection_matches(Some(cross_wired), expected));
}

/// a spawn mutation cannot accept an await-family rejection.
#[test]
fn spawn_rejection_rejects_await_family() {
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Spawn);
    let await_rejection = RejectionDetail::DelegationAwaitConflict {
        tool_request_id: expected.tool_request,
    };

    assert!(!delegation_rejection_matches(
        Some(await_rejection),
        expected
    ));
}

/// a child identity collision names only daemon-minted state and
/// cannot authenticate which spawn mutation produced the rejection.
#[test]
fn spawn_rejects_uncorrelated_child_identity_collision() {
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Spawn);
    let uncorrelated = RejectionDetail::DelegatedChildIdentityCollision {
        child_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
    };

    assert!(!delegation_rejection_matches(Some(uncorrelated), expected));
}

/// common delegation rejection evidence repeats the exact
/// request-supplied session, turn, and logical request identities.
#[test]
fn await_rejection_requires_exact_request_tuple() {
    let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
        child,
        mode: DelegationWaitMode::Background,
    });
    let exact = RejectionDetail::DelegationRequestNotInTurn {
        session_id: expected.session,
        turn_id: expected.turn,
        tool_request_id: expected.tool_request,
    };
    let cross_wired = RejectionDetail::DelegationRequestNotInTurn {
        session_id: expected.session,
        turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
        tool_request_id: expected.tool_request,
    };

    assert!(delegation_rejection_matches(Some(exact), expected));
    assert!(!delegation_rejection_matches(Some(cross_wired), expected));
}

/// delegation-wide missing-identity rejections repeat the exact
/// request-supplied session and logical tool request identities.
#[test]
fn delegation_missing_identity_rejections_require_exact_request() {
    let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
    let other = CanonicalUuid::from_uuid(Uuid::from_u128(5));

    assert!(delegation_rejection_matches(
        Some(RejectionDetail::SessionNotFound {
            session_id: expected.session,
        }),
        expected
    ));
    assert!(!delegation_rejection_matches(
        Some(RejectionDetail::SessionNotFound { session_id: other }),
        expected
    ));
    assert!(delegation_rejection_matches(
        Some(RejectionDetail::ToolRequestNotFound {
            tool_request_id: expected.tool_request,
        }),
        expected
    ));
    assert!(!delegation_rejection_matches(
        Some(RejectionDetail::ToolRequestNotFound {
            tool_request_id: other,
        }),
        expected
    ));
    assert!(delegation_rejection_matches(
        Some(RejectionDetail::ToolRequestNotInSession {
            session_id: expected.session,
            tool_request_id: expected.tool_request,
        }),
        expected
    ));
    assert!(!delegation_rejection_matches(
        Some(RejectionDetail::ToolRequestNotInSession {
            session_id: expected.session,
            tool_request_id: other,
        }),
        expected
    ));
}

/// await missing-relationship evidence repeats both requested
/// endpoints.
#[test]
fn await_rejection_requires_exact_relationship_endpoints() {
    let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
        child,
        mode: DelegationWaitMode::Background,
    });
    let exact = RejectionDetail::DelegationRelationNotFound {
        session_id: expected.session,
        peer_session_id: child,
    };
    let cross_wired = RejectionDetail::DelegationRelationNotFound {
        session_id: expected.session,
        peer_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
    };

    assert!(delegation_rejection_matches(Some(exact), expected));
    assert!(!delegation_rejection_matches(Some(cross_wired), expected));
}

/// relationship event exhaustion does not carry enough evidence
/// to correlate an await mutation, so the response remains ambiguous.
#[test]
fn await_rejects_uncorrelated_event_ordinal_exhaustion() {
    let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
        child,
        mode: DelegationWaitMode::Background,
    });
    let uncorrelated = RejectionDetail::DelegationEventOrdinalExhausted {
        spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
        last: CanonicalU64::new(u64::MAX),
    };

    assert!(!delegation_rejection_matches(Some(uncorrelated), expected));
}

/// background-await delivery exhaustion names the requesting
/// parent as the result recipient.
#[test]
fn background_await_delivery_exhaustion_requires_parent_recipient() {
    let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
        child,
        mode: DelegationWaitMode::Background,
    });
    let exact = RejectionDetail::DelegationDeliverySequenceExhausted {
        recipient_session_id: expected.session,
        last: CanonicalU64::new(u64::MAX),
    };
    let cross_wired = RejectionDetail::DelegationDeliverySequenceExhausted {
        recipient_session_id: child,
        last: CanonicalU64::new(u64::MAX),
    };

    assert!(delegation_rejection_matches(Some(exact), expected));
    assert!(!delegation_rejection_matches(Some(cross_wired), expected));
}

/// a foreground await cannot report background-only delivery
/// sequence exhaustion.
#[test]
fn foreground_await_rejects_delivery_sequence_exhaustion() {
    let child = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Await {
        child,
        mode: DelegationWaitMode::Foreground,
    });
    let background_only = RejectionDetail::DelegationDeliverySequenceExhausted {
        recipient_session_id: expected.session,
        last: CanonicalU64::new(u64::MAX),
    };

    assert!(!delegation_rejection_matches(
        Some(background_only),
        expected
    ));
}

/// message missing-relationship evidence repeats both requested
/// endpoints.
#[test]
fn message_rejection_requires_exact_relationship_endpoints() {
    let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
    let exact = RejectionDetail::DelegationRelationNotFound {
        session_id: expected.session,
        peer_session_id: peer,
    };
    let cross_wired = RejectionDetail::DelegationRelationNotFound {
        session_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
        peer_session_id: peer,
    };

    assert!(delegation_rejection_matches(Some(exact), expected));
    assert!(!delegation_rejection_matches(Some(cross_wired), expected));
}

/// relationship event exhaustion cannot authenticate which peer
/// message mutation exhausted the shared relationship ordinal.
#[test]
fn message_rejects_uncorrelated_event_ordinal_exhaustion() {
    let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
    let uncorrelated = RejectionDetail::DelegationEventOrdinalExhausted {
        spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
        last: CanonicalU64::new(u64::MAX),
    };

    assert!(!delegation_rejection_matches(Some(uncorrelated), expected));
}

/// a message identity collision names only daemon-minted state
/// and cannot authenticate which message mutation produced the rejection.
#[test]
fn message_rejects_uncorrelated_identity_collision() {
    let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
    let uncorrelated = RejectionDetail::DelegationMessageIdentityCollision {
        message_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
    };

    assert!(!delegation_rejection_matches(Some(uncorrelated), expected));
}

/// message delivery exhaustion names the requested peer as its
/// recipient.
#[test]
fn message_delivery_exhaustion_requires_peer_recipient() {
    let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
    let exact = RejectionDetail::DelegationDeliverySequenceExhausted {
        recipient_session_id: peer,
        last: CanonicalU64::new(u64::MAX),
    };
    let cross_wired = RejectionDetail::DelegationDeliverySequenceExhausted {
        recipient_session_id: expected.session,
        last: CanonicalU64::new(u64::MAX),
    };

    assert!(delegation_rejection_matches(Some(exact), expected));
    assert!(!delegation_rejection_matches(Some(cross_wired), expected));
}

/// a message mutation cannot accept an await-family rejection.
#[test]
fn message_rejection_rejects_await_family() {
    let peer = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let expected = delegation_rejection_expectation(DelegationRejectionOperation::Message { peer });
    let await_rejection = RejectionDetail::DelegationAwaitConflict {
        tool_request_id: expected.tool_request,
    };

    assert!(!delegation_rejection_matches(
        Some(await_rejection),
        expected
    ));
}

async fn accept_request_and_reply(
    listener: &UnixListener,
    expected: &ClientRequest,
    response: ServerMessage,
) -> io::Result<()> {
    let (stream, _) = listener.accept().await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    let request = decode_client_line(&line).map_err(io::Error::other)?;
    assert_eq!(request.request(), expected);
    let frame = ServerFrame::try_new_for_version(request.version(), request.request_id(), response)
        .map_err(io::Error::other)?;
    writer
        .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
        .await
}

fn deployment_limits_message(limits: ClientDeploymentLimits) -> ServerMessage {
    ServerMessage::DeploymentLimits {
        max_message_utf8_bytes: limits
            .max_message_utf8_bytes
            .map(|value| CanonicalU64::new(u64::try_from(value).expect("fixture limit fits u64"))),
        max_system_prompt_utf8_bytes: limits
            .max_system_prompt_utf8_bytes
            .map(|value| CanonicalU64::new(u64::try_from(value).expect("fixture limit fits u64"))),
        terminal_input_channel_capacity: limits
            .terminal_input_channel_capacity
            .map(|value| CanonicalU64::new(u64::try_from(value).expect("fixture limit fits u64"))),
        min_metadata_page_size: limits.min_metadata_page_size.map(CanonicalU64::new),
        max_metadata_page_size: limits.max_metadata_page_size.map(CanonicalU64::new),
        max_review_findings_per_run: limits.max_review_findings_per_run.map(CanonicalU64::new),
    }
}

#[tokio::test]
async fn client_learns_the_exact_deployment_limits_over_the_connection()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let expected = ClientDeploymentLimits {
        max_message_utf8_bytes: Some(7),
        max_system_prompt_utf8_bytes: None,
        terminal_input_channel_capacity: Some(3),
        min_metadata_page_size: Some(2),
        max_metadata_page_size: None,
        max_review_findings_per_run: Some(5),
    };
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &ClientRequest::ReadDeploymentLimits {},
            deployment_limits_message(expected),
        )
        .await
    });
    let mut client = ProcessClient::new(socket);

    let observed = read_deployment_limits(&mut client).await?;

    assert_eq!(observed, expected);
    server.await??;
    Ok(())
}

#[test]
fn message_policy_rejects_above_finite_limit() {
    let limits = ClientDeploymentLimits {
        max_message_utf8_bytes: Some(3),
        ..ClientDeploymentLimits::unbounded()
    };
    assert!(validate_message_policy("four", Some(limits)).is_err());
}

#[test]
fn message_policy_admits_unbounded_input() {
    let limits = ClientDeploymentLimits::unbounded();
    assert!(validate_message_policy("four", Some(limits)).is_ok());
}

#[test]
fn system_prompt_policy_rejects_above_finite_limit() {
    let limits = ClientDeploymentLimits {
        max_system_prompt_utf8_bytes: Some(3),
        ..ClientDeploymentLimits::unbounded()
    };
    assert!(
        validate_system_prompt_policy(
            &SystemPromptText::try_new(String::from("four")).expect("valid prompt"),
            Some(limits)
        )
        .is_err()
    );
}

#[test]
fn system_prompt_policy_admits_unbounded_input() {
    let limits = ClientDeploymentLimits::unbounded();
    assert!(
        validate_system_prompt_policy(
            &SystemPromptText::try_new(String::from("four")).expect("valid prompt"),
            Some(limits)
        )
        .is_ok()
    );
}

#[test]
fn finding_count_policy_rejects_above_finite_limit() {
    let limits = ClientDeploymentLimits {
        max_review_findings_per_run: Some(3),
        ..ClientDeploymentLimits::unbounded()
    };
    assert!(validate_review_finding_count(4, Some(limits)).is_err());
}

#[test]
fn finding_count_policy_admits_unbounded_input() {
    let limits = ClientDeploymentLimits::unbounded();
    assert!(validate_review_finding_count(4, Some(limits)).is_ok());
}

#[test]
fn metadata_policy_rejects_outside_finite_range() {
    let limits = ClientDeploymentLimits {
        min_metadata_page_size: Some(2),
        max_metadata_page_size: Some(4),
        ..ClientDeploymentLimits::unbounded()
    };
    for size in [1, 5] {
        assert!(validate_metadata_page_policy(CanonicalU64::new(size), Some(limits)).is_err());
    }
}

#[test]
fn metadata_policy_admits_positive_unbounded_pages() {
    assert!(
        validate_metadata_page_policy(
            CanonicalU64::new(u64::MAX),
            Some(ClientDeploymentLimits::unbounded())
        )
        .is_ok()
    );
}

#[test]
fn finding_inventory_cannot_exceed_the_storage_seal() {
    let structural_maximum = signalbox_process_protocol::MAX_REVIEW_PRODUCED_FINDINGS;
    for maximum in [None, Some(structural_maximum as u64 + 1)] {
        let limits = ClientDeploymentLimits {
            max_review_findings_per_run: maximum,
            ..ClientDeploymentLimits::unbounded()
        };
        assert!(validate_review_finding_count(structural_maximum, Some(limits)).is_ok());
        assert!(validate_review_finding_count(structural_maximum + 1, Some(limits)).is_err());
    }
}

#[test]
fn escaped_standard_input_is_rejected_before_request_preparation() {
    for byte in [b'"', b'\\', 1] {
        let input = vec![byte; MAX_INPUT_CONTENT_FRAME_BYTES / 2 + 1];
        assert!(read_input(&mut Cursor::new(input)).is_err(), "byte {byte}");
    }
}

fn client_arguments(socket: &Path, command: &[&str]) -> Vec<OsString> {
    [OsString::from("--socket"), socket.as_os_str().to_owned()]
        .into_iter()
        .chain(command.iter().map(OsString::from))
        .collect()
}

#[test]
fn descendant_scope_follows_the_explicit_cli_choice() {
    assert_eq!(
        descendant_scope(false),
        DescendantTerminationScope::ParentAlone
    );
    assert_eq!(
        descendant_scope(true),
        DescendantTerminationScope::ParentAndDescendants
    );
}

#[test]
fn goal_history_replay_accepts_supersession_lineage() -> Result<(), ClientError> {
    let first_command = CommandId::try_from_uuid(Uuid::from_u128(11))
        .expect("fixture command identity is admitted");
    let supersede_command = CommandId::try_from_uuid(Uuid::from_u128(12))
        .expect("fixture command identity is admitted");
    let stop_command = CommandId::try_from_uuid(Uuid::from_u128(13))
        .expect("fixture command identity is admitted");
    let mut replay = GoalHistoryReplay::default();

    replay.apply(
        1,
        &GoalHistoryEvent::Commissioned {
            statement: String::from("first scope"),
            command_id: first_command,
        },
    )?;
    replay.apply(
        1,
        &GoalHistoryEvent::Superseded {
            replacement_statement: String::from("replacement scope"),
            command_id: supersede_command,
        },
    )?;
    replay.apply(
        2,
        &GoalHistoryEvent::UserStopped {
            command_id: stop_command,
            settling_turn_id: None,
            abandoned_actions: Some(CanonicalU64::new(0)),
        },
    )?;

    replay.validate_projection(2, "replacement scope", &GoalLifecycleState::UserStopped {})
}

#[test]
fn goal_history_replay_rejects_an_invalid_first_transition() {
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(14))
        .expect("fixture command identity is admitted");
    let mut replay = GoalHistoryReplay::default();

    let result = replay.apply(
        1,
        &GoalHistoryEvent::UserStopped {
            command_id,
            settling_turn_id: None,
            abandoned_actions: Some(CanonicalU64::new(0)),
        },
    );

    assert!(matches!(result, Err(ClientError::Protocol(_))));
}

#[test]
fn goal_history_replay_rejects_a_mismatched_current_projection() {
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(15))
        .expect("fixture command identity is admitted");
    let mut replay = GoalHistoryReplay::default();
    replay
        .apply(
            1,
            &GoalHistoryEvent::Commissioned {
                statement: String::from("commissioned scope"),
                command_id,
            },
        )
        .expect("the commissioning event is valid");

    let result =
        replay.validate_projection(1, "different projection", &GoalLifecycleState::Pursuing {});

    assert!(matches!(result, Err(ClientError::Protocol(_))));
}

#[test]
fn goal_mutation_receipt_rejects_a_cross_wired_session() {
    let selected_session = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let foreign_session = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let message = ServerMessage::GoalTransitionApplied {
        termination: None,
        session_id: foreign_session,
        event_ordinal: CanonicalU64::new(1),
        generation: CanonicalU64::new(1),
    };

    let error = decode_goal_mutation_receipt(selected_session, message)
        .expect_err("foreign session receipt is rejected");

    assert!(error.is_ambiguous_mutation());
}

#[test]
fn placement_update_receipt_requires_the_exact_successor_and_echoed_request() {
    let requested_session = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let foreign_session = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let expected_version = CanonicalU64::new(7);
    let successor_version = CanonicalU64::new(8);
    let requested_placement = SessionPlacement::Pathless {};
    let foreign_placement = SessionPlacement::try_scoped(String::from("projects.other.session"))
        .expect("fixture placement is admitted");

    assert!(placement_update_receipt_matches(
        requested_session,
        successor_version,
        &requested_placement,
        requested_session,
        expected_version,
        &requested_placement,
    ));
    assert!(!placement_update_receipt_matches(
        foreign_session,
        successor_version,
        &requested_placement,
        requested_session,
        expected_version,
        &requested_placement,
    ));
    assert!(!placement_update_receipt_matches(
        requested_session,
        expected_version,
        &requested_placement,
        requested_session,
        expected_version,
        &requested_placement,
    ));
    assert!(!placement_update_receipt_matches(
        requested_session,
        successor_version,
        &foreign_placement,
        requested_session,
        expected_version,
        &requested_placement,
    ));
}

#[test]
fn placement_update_rejection_requires_the_exact_request_evidence() {
    let requested_session = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let foreign_session = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let expected_version = CanonicalU64::new(u64::MAX);

    assert!(placement_update_rejection_matches(
        Some(RejectionDetail::SessionNotFound {
            session_id: requested_session,
        }),
        requested_session,
        expected_version,
    ));
    assert!(!placement_update_rejection_matches(
        Some(RejectionDetail::SessionNotFound {
            session_id: foreign_session,
        }),
        requested_session,
        expected_version,
    ));
    assert!(placement_update_rejection_matches(
        Some(RejectionDetail::SessionPlacementCurrentVersionMismatch {
            session_id: requested_session,
            expected_placement_version: expected_version,
            current_placement_version: CanonicalU64::new(3),
        }),
        requested_session,
        expected_version,
    ));
    assert!(!placement_update_rejection_matches(
        Some(RejectionDetail::SessionPlacementCurrentVersionMismatch {
            session_id: requested_session,
            expected_placement_version: CanonicalU64::new(3),
            current_placement_version: CanonicalU64::new(4),
        }),
        requested_session,
        expected_version,
    ));
    assert!(placement_update_rejection_matches(
        Some(RejectionDetail::SessionPlacementVersionExhausted {
            session_id: requested_session,
            current_placement_version: expected_version,
        }),
        requested_session,
        expected_version,
    ));
    assert!(!placement_update_rejection_matches(
        Some(RejectionDetail::GoalCommandRejected {
            session_id: requested_session,
            reason: GoalCommandRejection::SessionNotFound,
        }),
        requested_session,
        expected_version,
    ));
    assert!(!placement_update_rejection_matches(
        None,
        requested_session,
        expected_version,
    ));
}

#[tokio::test]
async fn goal_text_file_reads_the_exact_maximum() -> Result<(), Box<dyn Error>> {
    let file = tempfile::NamedTempFile::new()?;
    fs::write(file.path(), vec![b'g'; MAX_CONTENT_FRAGMENT_BYTES])?;

    let text = read_goal_text_file(file.path()).await?;

    assert_eq!(text.len(), MAX_CONTENT_FRAGMENT_BYTES);
    Ok(())
}

#[tokio::test]
async fn goal_text_file_rejects_content_beyond_the_maximum() -> Result<(), Box<dyn Error>> {
    let file = tempfile::NamedTempFile::new()?;
    fs::write(file.path(), vec![b'g'; MAX_CONTENT_FRAGMENT_BYTES + 1])?;

    let result = read_goal_text_file(file.path()).await;

    assert!(matches!(result, Err(ClientError::Input(_))));
    Ok(())
}

#[tokio::test]
async fn goal_text_file_error_retains_the_selected_path() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("missing-goal.txt");

    let error = read_goal_text_file(&path)
        .await
        .expect_err("the absent goal text file is rejected");

    assert!(error.to_string().contains(&path.display().to_string()));
    assert_eq!(
        error
            .source()
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::NotFound)
    );
    Ok(())
}

#[tokio::test]
async fn delegation_content_file_utf8_error_retains_path_and_source() -> Result<(), Box<dyn Error>>
{
    let file = tempfile::NamedTempFile::new()?;
    fs::write(file.path(), [0xff])?;

    let error = read_delegation_content_file(file.path())
        .await
        .expect_err("invalid UTF-8 delegation content is rejected");

    assert!(
        error
            .to_string()
            .contains(&file.path().display().to_string())
    );
    assert!(
        error
            .source()
            .and_then(|source| source.downcast_ref::<std::string::FromUtf8Error>())
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn review_findings_file_decodes_an_empty_complete_inventory()
-> Result<(), Box<dyn std::error::Error>> {
    let file = tempfile::NamedTempFile::new()?;
    fs::write(file.path(), br#"{"findings":[]}"#)?;

    let decoded: ReviewFindingsFile = read_review_json_file(file.path()).await?;

    assert!(decoded.findings.is_empty());
    Ok(())
}

#[tokio::test]
async fn review_concerns_file_decodes_its_exact_wrapper() -> Result<(), Box<dyn std::error::Error>>
{
    let file = tempfile::NamedTempFile::new()?;
    fs::write(
        file.path(),
        br#"{"concerns":[{"key":"correctness","template_name":"review.correctness"}]}"#,
    )?;

    let decoded: ReviewConcernsFile = read_review_json_file(file.path()).await?;

    assert_eq!(decoded.concerns.len(), 1);
    assert_eq!(decoded.concerns[0].key, "correctness");
    Ok(())
}

#[tokio::test]
async fn review_concerns_file_rejects_an_unknown_wrapper_member()
-> Result<(), Box<dyn std::error::Error>> {
    let file = tempfile::NamedTempFile::new()?;
    fs::write(
        file.path(),
        br#"{"concerns":[{"key":"correctness","template_name":"review.correctness"}],"future":true}"#,
    )?;

    let decoded = read_review_json_file::<ReviewConcernsFile>(file.path()).await;

    assert!(matches!(decoded, Err(ClientError::ReviewInputJson(_))));
    Ok(())
}

#[tokio::test]
async fn review_json_file_rejects_content_beyond_the_frame_bound()
-> Result<(), Box<dyn std::error::Error>> {
    let file = tempfile::NamedTempFile::new()?;
    fs::write(file.path(), vec![b'x'; MAX_REVIEW_JSON_INPUT_BYTES + 1])?;

    let decoded = read_review_json_file::<ReviewConcernsFile>(file.path()).await;

    assert!(matches!(decoded, Err(ClientError::ReviewInputExceedsFrame)));
    Ok(())
}

#[tokio::test]
async fn review_json_file_bound_reserves_request_envelope_headroom()
-> Result<(), Box<dyn std::error::Error>> {
    let file = tempfile::NamedTempFile::new()?;
    fs::write(file.path(), vec![b'x'; MAX_REVIEW_JSON_INPUT_BYTES])?;

    let decoded = read_review_json_file::<ReviewConcernsFile>(file.path()).await;

    const {
        assert!(MAX_REVIEW_JSON_INPUT_BYTES < signalbox_process_protocol::MAX_FRAME_BYTES);
    }
    assert!(!matches!(
        decoded,
        Err(ClientError::ReviewInputExceedsFrame)
    ));
    Ok(())
}

#[test]
fn concern_acknowledgement_correlates_the_submitted_outcome() {
    assert!(review_concern_state_is_coherent(
        ReviewConcernTerminalOutcome::Succeeded,
        ReviewOrchestrationState::AwaitingJudgment,
    ));
    assert!(!review_concern_state_is_coherent(
        ReviewConcernTerminalOutcome::Failed,
        ReviewOrchestrationState::AwaitingJudgment,
    ));
}

#[test]
fn successful_last_concern_can_close_an_incomplete_fanout() {
    assert!(review_concern_state_is_coherent(
        ReviewConcernTerminalOutcome::Succeeded,
        ReviewOrchestrationState::FanoutIncomplete,
    ));
}

#[test]
fn judgment_acknowledgements_correlate_the_submitted_facts() {
    assert!(review_judgment_plan_state_is_coherent(
        true,
        ReviewOrchestrationState::AwaitingRepair,
    ));
    assert!(!review_judgment_effect_state_is_coherent(
        ReviewJudgmentEffectTerminalOutcome::Blocked,
        ReviewOrchestrationState::AwaitingRepair,
    ));
}

#[test]
fn repair_acknowledgement_correlates_the_blocked_barrier() {
    assert!(review_repair_state_is_coherent(
        true,
        ReviewOrchestrationState::RepairIncomplete,
    ));
    assert!(!review_repair_state_is_coherent(
        true,
        ReviewOrchestrationState::AwaitingPublication,
    ));
}

#[test]
fn publication_acknowledgement_correlates_the_complete_inventory() {
    assert!(review_publication_state_is_coherent(
        true,
        ReviewOrchestrationState::Complete,
    ));
    assert!(!review_publication_state_is_coherent(
        false,
        ReviewOrchestrationState::Complete,
    ));
}

#[test]
fn review_pass_completion_response_requires_the_exact_terminal_state() {
    assert!(review_pass_completion_is_coherent(
        ReviewPassTerminalOutcome::Succeeded,
        ReviewPassLifecycle::Succeeded,
    ));
    assert!(!review_pass_completion_is_coherent(
        ReviewPassTerminalOutcome::Succeeded,
        ReviewPassLifecycle::Failed,
    ));
}

#[test]
fn review_finding_event_response_requires_the_derived_status() {
    let event = ReviewFindingEvent::Fixed {};

    assert_eq!(
        review_finding_event_status(&event),
        ReviewFindingStatus::Fixed
    );
}

#[test]
fn coherent_review_run_response_is_accepted() {
    let pass = review_pass_snapshot();
    let run = review_run_snapshot(Some(pass.pass_id));

    assert!(super::review_run_response_is_coherent(&run, Some(&pass)));
}

#[test]
fn review_run_response_rejects_a_missing_recorded_pass() {
    let recorded_pass = review_pass_snapshot();
    let run = review_run_snapshot(Some(recorded_pass.pass_id));

    assert!(!super::review_run_response_is_coherent(&run, None));
}

#[test]
fn review_run_response_rejects_cross_wired_pass_ancestry() {
    const FOREIGN_TARGET_IDENTITY: u128 = 4;

    let mut pass = review_pass_snapshot();
    let run = review_run_snapshot(Some(pass.pass_id));
    pass.target_id = CanonicalUuid::from_uuid(Uuid::from_u128(FOREIGN_TARGET_IDENTITY));

    assert!(!super::review_run_response_is_coherent(&run, Some(&pass)));
}

#[test]
fn empty_standard_input_is_rejected() {
    assert!(read_input(&mut Cursor::new(Vec::<u8>::new())).is_err());
}

#[test]
fn nul_in_standard_input_is_rejected() {
    assert!(read_input(&mut Cursor::new(b"before\0after".to_vec())).is_err());
}

#[test]
fn oversized_standard_input_is_rejected() {
    assert!(
        read_input(&mut Cursor::new(vec![
            b'a';
            MAX_INPUT_CONTENT_FRAME_BYTES + 1
        ]))
        .is_err()
    );
}

#[test]
fn exact_limit_standard_input_is_accepted() {
    let exact = vec![b'a'; MAX_INPUT_CONTENT_FRAME_BYTES];
    assert_eq!(
        read_input(&mut Cursor::new(exact.clone()))
            .ok()
            .map(|value| value.into_bytes()),
        Some(exact)
    );
}

#[test]
fn send_waits_while_automatic_model_call_recovery_owns_the_decision() {
    let state = TurnState::ActiveAwaitingModelCallRecovery {
        ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        automatic_reconciliation_attempts: CanonicalU64::new(0),
        operator_action_required: false,
    };

    assert!(matches!(terminal_snapshot_state(Some(&state)), Ok(None)));
}

#[test]
fn send_fails_when_model_call_recovery_requires_operator_action() {
    let state = TurnState::ActiveAwaitingModelCallRecovery {
        ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        automatic_reconciliation_attempts: CanonicalU64::new(5),
        operator_action_required: true,
    };

    assert!(matches!(
        terminal_snapshot_state(Some(&state)),
        Err(ClientError::TurnRecoveryRequired)
    ));
}

#[test]
fn send_waits_while_automatic_tool_recovery_owns_the_decision() {
    let state = TurnState::ActiveAwaitingToolRecovery {
        ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        recovery_tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        automatic_reconciliation_attempts: CanonicalU64::new(0),
        operator_action_required: false,
    };

    assert!(matches!(terminal_snapshot_state(Some(&state)), Ok(None)));
}

#[test]
fn send_fails_when_tool_recovery_requires_operator_action() {
    let state = TurnState::ActiveAwaitingToolRecovery {
        ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        recovery_tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        automatic_reconciliation_attempts: CanonicalU64::new(5),
        operator_action_required: true,
    };

    assert!(matches!(
        terminal_snapshot_state(Some(&state)),
        Err(ClientError::TurnRecoveryRequired)
    ));
}

#[test]
fn queued_send_waits_while_automatic_tool_recovery_owns_its_blocker() {
    let blocker = TurnState::ActiveAwaitingToolRecovery {
        ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        recovery_tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        automatic_reconciliation_attempts: CanonicalU64::new(0),
        operator_action_required: false,
    };

    assert!(matches!(blocker_recovery_snapshot_state(&blocker), Ok(())));
}

#[test]
fn queued_send_fails_when_its_tool_recovery_blocker_requires_operator_action() {
    let blocker = TurnState::ActiveAwaitingToolRecovery {
        ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        recovery_tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        automatic_reconciliation_attempts: CanonicalU64::new(5),
        operator_action_required: true,
    };

    assert!(matches!(
        blocker_recovery_snapshot_state(&blocker),
        Err(ClientError::TurnRecoveryRequired)
    ));
}

#[test]
fn credential_wait_keeps_send_and_queued_follow_nonterminal() {
    let state = TurnState::ActiveAwaitingCredentialAvailability {
        wait_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        cause: signalbox_process_protocol::CredentialAvailabilityWaitCause::Exhausted,
    };
    assert_eq!(
        terminal_snapshot_state(Some(&state)).expect("credential wait is readable"),
        None
    );
    assert!(blocker_recovery_snapshot_state(&state).is_ok());
}

#[test]
fn send_fails_explicitly_when_runner_recovery_is_required() {
    let state = TurnState::ActiveAwaitingRunnerRecovery {
        runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        placement_revision: signalbox_process_protocol::PositiveCanonicalU64::try_new(2)
            .expect("the fixture revision is positive"),
        tool_attempt_id: None,
    };
    let error = terminal_snapshot_state(Some(&state))
        .expect_err("runner recovery cannot be completed by the terminal");

    assert!(matches!(&error, ClientError::RunnerRecoveryRequired));
    assert_eq!(
        error.to_string(),
        "the submitted turn awaits lost-runner replacement or stop_turn before abandonment"
    );
}

#[test]
fn queued_send_fails_explicitly_when_its_active_blocker_awaits_runner_recovery() {
    let blocker = TurnState::ActiveAwaitingRunnerRecovery {
        runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        placement_revision: signalbox_process_protocol::PositiveCanonicalU64::try_new(2)
            .expect("the fixture revision is positive"),
        tool_attempt_id: None,
    };

    assert!(matches!(
        blocker_recovery_snapshot_state(&blocker),
        Err(ClientError::RunnerRecoveryRequired)
    ));
}

#[test]
fn send_classifies_cancelled_snapshot_truth() {
    let state = TurnState::Cancelled {
        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        terminal_model_call_id: None,
    };

    assert_eq!(
        terminal_snapshot_state(Some(&state)).expect("cancelled state is terminal protocol truth"),
        Some(TurnTerminal::Cancelled)
    );
}

#[test]
fn send_classifies_reconciliation_required_snapshot_truth() {
    let state = TurnState::ReconciliationRequired {
        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        terminal_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
    };

    assert_eq!(
        terminal_snapshot_state(Some(&state))
            .expect("reconciliation state is terminal protocol truth"),
        Some(TurnTerminal::ReconciliationRequired)
    );
}

#[tokio::test]
async fn queued_send_wait_uses_active_slot_not_acceptance_order_or_terminal_history()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let historical_terminal_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let current_blocker_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let queued_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            follow_request.request(),
            &ClientRequest::FollowSession { session_id }
        );
        let snapshot = |version, request_id, cursor, blocker_state| -> io::Result<Vec<u8>> {
            let frame = |message| {
                ServerFrame::try_new_for_version(version, request_id, message)
                    .map_err(io::Error::other)
            };
            let mut response =
                encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                    workspace_root_kind: None,
                    repository_watch: None,
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    runner: None,
                })?)
                .map_err(io::Error::other)?;
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id: historical_terminal_turn_id,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state: TurnState::ReconciliationRequired {
                        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                        terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
                        terminal_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(7)),
                    },
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id: queued_turn_id,
                    acceptance_position: CanonicalU64::new(3),
                    model_settings: None,
                    state: TurnState::Queued {
                        accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(10)),
                        content: UserInputContent::text(String::from("wait behind recovery")),
                    },
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id: current_blocker_turn_id,
                    acceptance_position: CanonicalU64::new(4),
                    model_settings: None,
                    state: blocker_state,
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                    model_call_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    turn_count: CanonicalU64::new(3),
                    entry_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            Ok(response)
        };
        let mut initial = snapshot(
            follow_request.version(),
            follow_request.request_id(),
            0,
            TurnState::ActiveRunning {
                current_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(8)),
                current_model_call: None,
            },
        )?;
        initial.extend_from_slice(
            &encode_server_line(
                &ServerFrame::try_new_for_version(
                    follow_request.version(),
                    follow_request.request_id(),
                    ServerMessage::SessionEvent {
                        cursor: CanonicalU64::new(1),
                        session_id,
                        event: SessionEvent::ModelCallTransition {
                            turn_id: current_blocker_turn_id,
                            model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
                            state: ModelCallState::Terminal {
                                disposition: ModelCallDisposition::Ambiguous,
                            },
                        },
                    },
                )
                .map_err(io::Error::other)?,
            )
            .map_err(io::Error::other)?,
        );
        writer.write_all(&initial).await?;

        let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
        let mut refresh_reader = BufReader::new(refresh_stream);
        let mut refresh_line = Vec::new();
        refresh_reader.read_until(b'\n', &mut refresh_line).await?;
        let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
        assert_eq!(
            refresh_request.request(),
            &ClientRequest::ReadTranscript { session_id }
        );
        let refreshed = snapshot(
            refresh_request.version(),
            refresh_request.request_id(),
            1,
            TurnState::ActiveAwaitingModelCallRecovery {
                ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(8)),
                recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
                automatic_reconciliation_attempts: CanonicalU64::new(0),
                operator_action_required: false,
            },
        )?;
        refresh_writer.write_all(&refreshed).await?;

        let (exhausted_stream, mut exhausted_writer) = listener.accept().await?.0.into_split();
        let mut exhausted_reader = BufReader::new(exhausted_stream);
        let mut exhausted_line = Vec::new();
        exhausted_reader
            .read_until(b'\n', &mut exhausted_line)
            .await?;
        let exhausted_request = decode_client_line(&exhausted_line).map_err(io::Error::other)?;
        assert_eq!(
            exhausted_request.request(),
            &ClientRequest::ReadTranscript { session_id }
        );
        let exhausted = snapshot(
            exhausted_request.version(),
            exhausted_request.request_id(),
            1,
            TurnState::ActiveAwaitingModelCallRecovery {
                ended_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(8)),
                recovery_model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
                automatic_reconciliation_attempts: CanonicalU64::new(5),
                operator_action_required: true,
            },
        )?;
        exhausted_writer.write_all(&exhausted).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let result = await_turn_terminal(&mut client, session_id, queued_turn_id).await;

    assert!(matches!(result, Err(ClientError::TurnRecoveryRequired)));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn selected_send_polls_after_an_automatic_recovery_transition() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let server = tokio::spawn(async move {
        let snapshot = |version, request_id, cursor, state| -> io::Result<Vec<u8>> {
            let frame = |message| {
                ServerFrame::try_new_for_version(version, request_id, message)
                    .map_err(io::Error::other)
            };
            let mut response =
                encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                    workspace_root_kind: None,
                    repository_watch: None,
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    runner: None,
                })?)
                .map_err(io::Error::other)?;
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state,
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                    model_call_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    turn_count: CanonicalU64::new(1),
                    entry_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            Ok(response)
        };

        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
        let mut initial = snapshot(
            follow_request.version(),
            follow_request.request_id(),
            0,
            TurnState::ActiveRunning {
                current_attempt_id: attempt_id,
                current_model_call: None,
            },
        )?;
        initial.extend_from_slice(
            &encode_server_line(
                &ServerFrame::try_new_for_version(
                    follow_request.version(),
                    follow_request.request_id(),
                    ServerMessage::SessionEvent {
                        cursor: CanonicalU64::new(1),
                        session_id,
                        event: SessionEvent::ModelCallTransition {
                            turn_id,
                            model_call_id,
                            state: ModelCallState::Terminal {
                                disposition: ModelCallDisposition::Ambiguous,
                            },
                        },
                    },
                )
                .map_err(io::Error::other)?,
            )
            .map_err(io::Error::other)?,
        );
        writer.write_all(&initial).await?;

        let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
        let mut refresh_reader = BufReader::new(refresh_stream);
        let mut refresh_line = Vec::new();
        refresh_reader.read_until(b'\n', &mut refresh_line).await?;
        let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
        refresh_writer
            .write_all(&snapshot(
                refresh_request.version(),
                refresh_request.request_id(),
                1,
                TurnState::ActiveAwaitingModelCallRecovery {
                    ended_attempt_id: attempt_id,
                    recovery_model_call_id: model_call_id,
                    automatic_reconciliation_attempts: CanonicalU64::new(0),
                    operator_action_required: false,
                },
            )?)
            .await?;

        let (poll_stream, mut poll_writer) = listener.accept().await?.0.into_split();
        let mut poll_reader = BufReader::new(poll_stream);
        let mut poll_line = Vec::new();
        poll_reader.read_until(b'\n', &mut poll_line).await?;
        let poll_request = decode_client_line(&poll_line).map_err(io::Error::other)?;
        poll_writer
            .write_all(&snapshot(
                poll_request.version(),
                poll_request.request_id(),
                1,
                TurnState::ReconciliationRequired {
                    terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                    terminal_attempt_id: attempt_id,
                    terminal_model_call_id: model_call_id,
                },
            )?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

    assert_eq!(terminal, TurnTerminal::ReconciliationRequired);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn selected_send_recovery_poll_is_not_postponed_by_follow_traffic()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let server = tokio::spawn(async move {
        let snapshot = |version, request_id, cursor, state| -> io::Result<Vec<u8>> {
            let frame = |message| {
                ServerFrame::try_new_for_version(version, request_id, message)
                    .map_err(io::Error::other)
            };
            let mut response =
                encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                    workspace_root_kind: None,
                    repository_watch: None,
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    runner: None,
                })?)
                .map_err(io::Error::other)?;
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state,
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                    model_call_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    turn_count: CanonicalU64::new(1),
                    entry_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            Ok(response)
        };

        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
        let mut initial = snapshot(
            follow_request.version(),
            follow_request.request_id(),
            0,
            TurnState::ActiveRunning {
                current_attempt_id: attempt_id,
                current_model_call: None,
            },
        )?;
        initial.extend_from_slice(
            &encode_server_line(
                &ServerFrame::try_new_for_version(
                    follow_request.version(),
                    follow_request.request_id(),
                    ServerMessage::SessionEvent {
                        cursor: CanonicalU64::new(1),
                        session_id,
                        event: SessionEvent::ModelCallTransition {
                            turn_id,
                            model_call_id,
                            state: ModelCallState::Terminal {
                                disposition: ModelCallDisposition::Ambiguous,
                            },
                        },
                    },
                )
                .map_err(io::Error::other)?,
            )
            .map_err(io::Error::other)?,
        );
        writer.write_all(&initial).await?;

        let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
        let mut refresh_reader = BufReader::new(refresh_stream);
        let mut refresh_line = Vec::new();
        refresh_reader.read_until(b'\n', &mut refresh_line).await?;
        let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
        refresh_writer
            .write_all(&snapshot(
                refresh_request.version(),
                refresh_request.request_id(),
                1,
                TurnState::ActiveAwaitingModelCallRecovery {
                    ended_attempt_id: attempt_id,
                    recovery_model_call_id: model_call_id,
                    automatic_reconciliation_attempts: CanonicalU64::new(0),
                    operator_action_required: false,
                },
            )?)
            .await?;

        let traffic = encode_server_line(
            &ServerFrame::try_new_for_version(
                follow_request.version(),
                follow_request.request_id(),
                ServerMessage::ProviderTextDelta {
                    session_id,
                    turn_id,
                    model_call_id,
                    part_index: CanonicalU64::new(0),
                    content: ContentFragment::try_new(String::from("busy follow traffic"))
                        .map_err(io::Error::other)?,
                },
            )
            .map_err(io::Error::other)?,
        )
        .map_err(io::Error::other)?;
        let mut traffic_interval = tokio::time::interval(Duration::from_millis(10));
        let poll_stream = timeout(Duration::from_secs(1), async {
            loop {
                tokio::select! {
                    accepted = listener.accept() => break accepted,
                    _ = traffic_interval.tick() => writer.write_all(&traffic).await?,
                }
            }
        })
        .await??
        .0;
        let (poll_stream, mut poll_writer) = poll_stream.into_split();
        let mut poll_reader = BufReader::new(poll_stream);
        let mut poll_line = Vec::new();
        poll_reader.read_until(b'\n', &mut poll_line).await?;
        let poll_request = decode_client_line(&poll_line).map_err(io::Error::other)?;
        poll_writer
            .write_all(&snapshot(
                poll_request.version(),
                poll_request.request_id(),
                1,
                TurnState::ReconciliationRequired {
                    terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                    terminal_attempt_id: attempt_id,
                    terminal_model_call_id: model_call_id,
                },
            )?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

    assert_eq!(terminal, TurnTerminal::ReconciliationRequired);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn selected_send_polls_after_an_automatic_tool_recovery_transition()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let tool_attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(6));
    let server = tokio::spawn(async move {
        let snapshot = |version, request_id, cursor, state| -> io::Result<Vec<u8>> {
            let frame = |message| {
                ServerFrame::try_new_for_version(version, request_id, message)
                    .map_err(io::Error::other)
            };
            let mut response =
                encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                    workspace_root_kind: None,
                    repository_watch: None,
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    runner: None,
                })?)
                .map_err(io::Error::other)?;
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state,
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                    model_call_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    turn_count: CanonicalU64::new(1),
                    entry_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            Ok(response)
        };

        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
        let mut initial = snapshot(
            follow_request.version(),
            follow_request.request_id(),
            0,
            TurnState::ActiveRunning {
                current_attempt_id: attempt_id,
                current_model_call: None,
            },
        )?;
        initial.extend_from_slice(
            &encode_server_line(
                &ServerFrame::try_new_for_version(
                    follow_request.version(),
                    follow_request.request_id(),
                    ServerMessage::SessionEvent {
                        cursor: CanonicalU64::new(1),
                        session_id,
                        event: SessionEvent::ToolBatchTransition {
                            turn_id,
                            model_call_id,
                            state: ToolBatchState::RecoveryRequired { tool_attempt_id },
                        },
                    },
                )
                .map_err(io::Error::other)?,
            )
            .map_err(io::Error::other)?,
        );
        writer.write_all(&initial).await?;

        let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
        let mut refresh_reader = BufReader::new(refresh_stream);
        let mut refresh_line = Vec::new();
        refresh_reader.read_until(b'\n', &mut refresh_line).await?;
        let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
        refresh_writer
            .write_all(&snapshot(
                refresh_request.version(),
                refresh_request.request_id(),
                1,
                TurnState::ActiveAwaitingToolRecovery {
                    ended_attempt_id: attempt_id,
                    recovery_tool_attempt_id: tool_attempt_id,
                    automatic_reconciliation_attempts: CanonicalU64::new(0),
                    operator_action_required: false,
                },
            )?)
            .await?;

        let (poll_stream, mut poll_writer) = listener.accept().await?.0.into_split();
        let mut poll_reader = BufReader::new(poll_stream);
        let mut poll_line = Vec::new();
        poll_reader.read_until(b'\n', &mut poll_line).await?;
        let poll_request = decode_client_line(&poll_line).map_err(io::Error::other)?;
        poll_writer
            .write_all(&snapshot(
                poll_request.version(),
                poll_request.request_id(),
                1,
                TurnState::ToolReconciliationRequired {
                    terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                    terminal_attempt_id: attempt_id,
                    terminal_tool_attempt_id: tool_attempt_id,
                },
            )?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

    assert_eq!(terminal, TurnTerminal::ReconciliationRequired);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn send_wait_continues_after_a_superseded_runner_loss_event() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let follow_request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            follow_request.request(),
            &ClientRequest::FollowSession { session_id }
        );
        let snapshot = |version, request_id, cursor| -> io::Result<Vec<u8>> {
            let frame = |message| {
                ServerFrame::try_new_for_version(version, request_id, message)
                    .map_err(io::Error::other)
            };
            let mut response =
                encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
                    workspace_root_kind: None,
                    repository_watch: None,
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    runner: None,
                })?)
                .map_err(io::Error::other)?;
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                    turn_id,
                    acceptance_position: CanonicalU64::new(1),
                    model_settings: None,
                    state: TurnState::ActiveRunning {
                        current_attempt_id: attempt_id,
                        current_model_call: None,
                    },
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                    model_call_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            response.extend_from_slice(
                &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                    session_id,
                    cursor: CanonicalU64::new(cursor),
                    turn_count: CanonicalU64::new(1),
                    entry_count: CanonicalU64::new(0),
                })?)
                .map_err(io::Error::other)?,
            );
            Ok(response)
        };
        let mut initial = snapshot(follow_request.version(), follow_request.request_id(), 0)?;
        initial.extend_from_slice(
            &encode_server_line(
                &ServerFrame::try_new_for_version(
                    follow_request.version(),
                    follow_request.request_id(),
                    ServerMessage::SessionEvent {
                        cursor: CanonicalU64::new(1),
                        session_id,
                        event: SessionEvent::RunnerStateTransition {
                            runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                            placement_revision: RunnerPlacementRevision::try_new(1)
                                .ok_or_else(|| io::Error::other("positive fixture revision"))?,
                            sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
                            working_directory: None,
                            state: RunnerStateTransitionState::RunnerLost,
                        },
                    },
                )
                .map_err(io::Error::other)?,
            )
            .map_err(io::Error::other)?,
        );
        writer.write_all(&initial).await?;

        let (refresh_stream, mut refresh_writer) = listener.accept().await?.0.into_split();
        let mut refresh_reader = BufReader::new(refresh_stream);
        let mut refresh_line = Vec::new();
        refresh_reader.read_until(b'\n', &mut refresh_line).await?;
        let refresh_request = decode_client_line(&refresh_line).map_err(io::Error::other)?;
        assert_eq!(
            refresh_request.request(),
            &ClientRequest::ReadTranscript { session_id }
        );
        refresh_writer
            .write_all(&snapshot(
                refresh_request.version(),
                refresh_request.request_id(),
                1,
            )?)
            .await?;
        writer
            .write_all(
                &encode_server_line(
                    &ServerFrame::try_new_for_version(
                        follow_request.version(),
                        follow_request.request_id(),
                        ServerMessage::SessionEvent {
                            cursor: CanonicalU64::new(2),
                            session_id,
                            event: SessionEvent::TurnCompleted {
                                turn_id,
                                model_call_id,
                                completion_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
                                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(7)),
                            },
                        },
                    )
                    .map_err(io::Error::other)?,
                )
                .map_err(io::Error::other)?,
            )
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

    assert_eq!(terminal, TurnTerminal::Completed);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn send_wait_ignores_streamed_text_until_the_durable_terminal_event()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::FollowSession { session_id }
        );
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let mut response = encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
            workspace_root_kind: None,
            repository_watch: None,
            session_id,
            cursor: CanonicalU64::new(0),
            runner: None,
        })?)
        .map_err(io::Error::other)?;
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                    content: UserInputContent::text(String::from("stream the reply")),
                },
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                model_call_count: CanonicalU64::new(0),
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                session_id,
                cursor: CanonicalU64::new(0),
                turn_count: CanonicalU64::new(1),
                entry_count: CanonicalU64::new(0),
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ProviderTextDelta {
                session_id,
                turn_id,
                model_call_id,
                part_index: CanonicalU64::new(0),
                content: ContentFragment::try_new(String::from("already [redacted]"))
                    .map_err(io::Error::other)?,
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::SessionEvent {
                cursor: CanonicalU64::new(1),
                session_id,
                event: SessionEvent::TurnCompleted {
                    turn_id,
                    model_call_id,
                    completion_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                    terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
                },
            })?)
            .map_err(io::Error::other)?,
        );
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let terminal = await_turn_terminal(&mut client, session_id, turn_id).await?;

    assert_eq!(terminal, TurnTerminal::Completed);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn send_wait_rejects_streamed_text_for_another_session() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let other_session_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::FollowSession { session_id }
        );
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let mut response = encode_server_line(&frame(ServerMessage::TranscriptSnapshotStart {
            workspace_root_kind: None,
            repository_watch: None,
            session_id,
            cursor: CanonicalU64::new(0),
            runner: None,
        })?)
        .map_err(io::Error::other)?;
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::TranscriptTurn {
                turn_id,
                acceptance_position: CanonicalU64::new(1),
                model_settings: None,
                state: TurnState::Queued {
                    accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                    content: UserInputContent::text(String::from("stream the reply")),
                },
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::TranscriptModelCallsEnd {
                model_call_count: CanonicalU64::new(0),
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::TranscriptSnapshotEnd {
                session_id,
                cursor: CanonicalU64::new(0),
                turn_count: CanonicalU64::new(1),
                entry_count: CanonicalU64::new(0),
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ProviderTextDelta {
                session_id: other_session_id,
                turn_id,
                model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
                part_index: CanonicalU64::new(0),
                content: ContentFragment::try_new(String::from("cross-wired text"))
                    .map_err(io::Error::other)?,
            })?)
            .map_err(io::Error::other)?,
        );
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let result = await_turn_terminal(&mut client, session_id, turn_id).await;

    assert!(matches!(
        result,
        Err(ClientError::Protocol(
            "follow returned an unexpected response"
        ))
    ));
    server.await??;
    Ok(())
}

#[test]
fn send_classifies_cancelled_event_for_its_turn() {
    let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let event = SessionEvent::TurnCancelled {
        turn_id: selected_turn,
        cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
    };

    assert_eq!(
        terminal_event_state(&event, selected_turn),
        Some(TurnTerminal::Cancelled)
    );
}

#[test]
fn send_classifies_reconciliation_required_event_for_its_turn() {
    let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let event = SessionEvent::TurnReconciliationRequired {
        turn_id: selected_turn,
        model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
    };

    assert_eq!(
        terminal_event_state(&event, selected_turn),
        Some(TurnTerminal::ReconciliationRequired)
    );
    assert!(session_recovery_transition(&event));
}

#[test]
fn cli_socket_path_must_be_absolute() {
    assert!(matches!(
        socket_path(Some(PathBuf::from("relative.sock")), None),
        Err(ClientError::Input(
            "the local process socket path must be absolute"
        ))
    ));
}

#[test]
fn environment_socket_path_must_be_absolute() {
    assert!(matches!(
        socket_path(None, Some(OsString::from("relative.sock"))),
        Err(ClientError::Input(
            "the local process socket path must be absolute"
        ))
    ));
}

#[test]
fn selected_turn_ambiguous_model_call_requests_recovery_reread() {
    let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let event = SessionEvent::ModelCallTransition {
        turn_id: selected_turn,
        model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        state: ModelCallState::Terminal {
            disposition: ModelCallDisposition::Ambiguous,
        },
    };

    assert!(model_call_recovery_transition(&event, selected_turn));
    assert!(session_recovery_transition(&event));
    assert!(!model_call_recovery_transition(
        &event,
        CanonicalUuid::from_uuid(Uuid::from_u128(3))
    ));
}

#[test]
fn selected_turn_tool_recovery_requests_authoritative_reread() {
    let selected_turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let event = SessionEvent::ToolBatchTransition {
        turn_id: selected_turn,
        model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        state: ToolBatchState::RecoveryRequired {
            tool_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        },
    };

    assert!(tool_recovery_transition(&event, selected_turn));
    assert!(session_recovery_transition(&event));
    assert!(!tool_recovery_transition(
        &event,
        CanonicalUuid::from_uuid(Uuid::from_u128(4))
    ));
}

#[test]
fn runner_loss_requests_authoritative_turn_reread() {
    let event = SessionEvent::RunnerStateTransition {
        runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        placement_revision: RunnerPlacementRevision::try_new(2)
            .expect("the fixture placement revision is positive"),
        sandbox_profile: RunnerSandboxProfile::WorkspaceRestricted,
        working_directory: None,
        state: RunnerStateTransitionState::RunnerLost,
    };

    assert!(selected_turn_recovery_transition(
        &event,
        CanonicalUuid::from_uuid(Uuid::from_u128(3))
    ));
}

#[test]
fn queued_send_stops_on_current_pre_pin_runner_loss_from_snapshot() {
    let projection = runner_projection(1, RunnerProjectionState::RunnerLostBeforePin, None);
    let result = queued_turn_runner_recovery(Some(&projection));

    assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
}

#[test]
fn selected_send_stops_when_its_authoritative_turn_is_still_queued_on_runner_loss() {
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let projection = runner_projection(1, RunnerProjectionState::RunnerLost, None);
    let mut snapshot = TranscriptSnapshot::from_messages_with_runner(
        1,
        Some(projection),
        [ServerMessage::TranscriptTurn {
            turn_id,
            acceptance_position: CanonicalU64::new(1),
            model_settings: None,
            state: TurnState::Queued {
                accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                content: UserInputContent::text(String::from("queued selected input")),
            },
        }],
    )
    .expect("the queued selected-turn fixture spools");
    let result = queued_turn_recovery(&mut snapshot, turn_id);

    assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
}

#[test]
fn queued_send_ignores_pre_pin_runner_loss_superseded_in_snapshot() {
    let projection = runner_projection(
        2,
        RunnerProjectionState::Pinned,
        Some(RunnerConnectionHealth::Connected),
    );
    let result = queued_turn_runner_recovery(Some(&projection));

    assert!(result.is_ok());
}

#[test]
fn queued_send_stops_on_current_pinned_runner_loss() {
    let projection = runner_projection(1, RunnerProjectionState::RunnerLost, None);
    let result = queued_turn_runner_recovery(Some(&projection));

    assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
}

/// orderly terminal runner shutdown blocks queued activation
/// before placement reconciliation catches up.
#[test]
fn queued_send_stops_on_current_runner_shutdown() {
    let projection = runner_projection(
        1,
        RunnerProjectionState::Pinned,
        Some(RunnerConnectionHealth::Shutdown),
    );
    let result = queued_turn_runner_recovery(Some(&projection));

    assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
}

/// terminal runner connection loss blocks queued activation
/// before placement reconciliation catches up.
#[test]
fn queued_send_stops_on_current_runner_connection_loss() {
    let projection = runner_projection(
        1,
        RunnerProjectionState::Pinned,
        Some(RunnerConnectionHealth::Lost),
    );
    let result = queued_turn_runner_recovery(Some(&projection));

    assert!(matches!(result, Err(ClientError::RunnerRecoveryRequired)));
}

#[test]
fn tool_batch_events_select_their_exact_material() {
    let turn = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let call = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let frontier = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    assert_eq!(
        terminal_snapshot_selection(
            &SessionEvent::ToolBatchTransition {
                turn_id: turn,
                model_call_id: call,
                state: ToolBatchState::Proposed {
                    frontier_id: frontier,
                },
            },
            followed_session()
        ),
        Some(SnapshotSelection::ToolBatchProposed {
            turn_id: turn,
            model_call_id: call,
        })
    );
    assert_eq!(
        terminal_snapshot_selection(
            &SessionEvent::ToolBatchTransition {
                turn_id: turn,
                model_call_id: call,
                state: ToolBatchState::ResultsProjected {
                    frontier_id: frontier,
                },
            },
            followed_session()
        ),
        Some(SnapshotSelection::ToolBatchResults {
            turn_id: turn,
            model_call_id: call,
        })
    );
}

#[test]
fn refused_terminal_event_requests_provider_compaction_call_material() {
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let model_call_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    assert_eq!(
        terminal_snapshot_selection(
            &SessionEvent::TurnRefused {
                turn_id,
                model_call_id,
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            },
            followed_session()
        ),
        Some(SnapshotSelection::Refused {
            turn_id,
            model_call_id,
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        })
    );
}

#[test]
fn cancellation_event_selects_its_exact_marker_for_reread() {
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));

    assert!(matches!(
        terminal_snapshot_selection(&SessionEvent::TurnCancelled {
            turn_id,
            cancellation_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        }, followed_session()),
        Some(SnapshotSelection::Cancelled {
            turn_id: selected,
            terminal_entry_id,
        }) if selected == turn_id && terminal_entry_id == CanonicalUuid::from_uuid(Uuid::from_u128(2))
    ));
}

#[test]
fn reconciliation_event_selects_no_semantic_material_for_reread() {
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));

    assert!(
        terminal_snapshot_selection(
            &SessionEvent::TurnReconciliationRequired {
                turn_id,
                model_call_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
                terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            },
            followed_session()
        )
        .is_none()
    );
}

#[test]
fn tool_reconciliation_event_selects_terminal_tool_results() {
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let tool_attempt_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let terminal_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));

    assert_eq!(
        terminal_snapshot_selection(
            &SessionEvent::TurnToolReconciliationRequired {
                turn_id,
                tool_attempt_id,
                terminal_frontier_id,
            },
            followed_session()
        ),
        Some(SnapshotSelection::ToolReconciliation {
            turn_id,
            tool_attempt_id,
            terminal_frontier_id,
        })
    );
}

#[test]
fn child_addressed_cascade_disposition_requests_an_authoritative_reread() {
    let event = SessionEvent::ChildLifecycleDisposition {
        spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        child_session_id: followed_session(),
        outcome: DelegationOutcome::Stopped,
        reason: DelegationReason::ParentStopped,
        provenance: DelegationProvenance::ParentGoalCommand {
            parent_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
            goal_generation: CanonicalU64::new(1),
            command_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    };

    assert!(child_lifecycle_terminalization(&event, followed_session()));
    assert_eq!(
        terminal_snapshot_selection(&event, followed_session()),
        Some(SnapshotSelection::All)
    );
}

#[test]
fn parent_addressed_cascade_disposition_requests_no_reread() {
    let event = SessionEvent::ChildLifecycleDisposition {
        spawning_request_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        child_session_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
        outcome: DelegationOutcome::Stopped,
        reason: DelegationReason::ParentStopped,
        provenance: DelegationProvenance::ParentGoalCommand {
            parent_session_id: followed_session(),
            goal_generation: CanonicalU64::new(1),
            command_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            descendant_scope: DescendantTerminationScope::ParentAndDescendants,
        },
    };

    assert!(!child_lifecycle_terminalization(&event, followed_session()));
    assert_eq!(
        terminal_snapshot_selection(&event, followed_session()),
        None
    );
}

#[tokio::test]
async fn invalid_send_input_fails_before_a_missing_socket_is_opened() {
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let exit = run(
        [
            OsString::from("--socket"),
            OsString::from("/does/not/exist"),
            OsString::from("send"),
            OsString::from("00000000-0000-0000-0000-000000000001"),
        ],
        None,
        &mut input,
        &mut output,
        &mut error,
    )
    .await;
    assert_eq!(exit, ExitCode::FAILURE);
    assert!(String::from_utf8_lossy(&error).contains("must not be empty"));
}

#[tokio::test]
async fn missing_import_source_fails_before_a_missing_socket_is_opened() {
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let exit = run(
        [
            OsString::from("--socket"),
            OsString::from("/does/not/exist/hub.sock"),
            OsString::from("import"),
            OsString::from("--format"),
            OsString::from("claude-code"),
            OsString::from("/does/not/exist/session.jsonl"),
        ],
        None,
        &mut input,
        &mut output,
        &mut error,
    )
    .await;

    assert_eq!(exit, ExitCode::FAILURE);
    assert!(output.is_empty());
    assert!(
        String::from_utf8_lossy(&error)
            .contains("conversation import source file could not be read")
    );
}

#[tokio::test]
async fn import_reader_rejects_source_beyond_its_single_frame_bound() -> Result<(), Box<dyn Error>>
{
    let source_file = tempfile::tempfile()?;
    let source_size = MAX_SINGLE_FRAME_IMPORT_SOURCE_BYTES + 1;
    source_file.set_len(u64::try_from(source_size)?)?;
    let source_file = tokio::fs::File::from_std(source_file);

    let error = read_import_file(source_file).await.unwrap_err();

    assert!(matches!(error, ClientError::SourceExceedsFrame));
    Ok(())
}

#[test]
fn import_transport_selects_single_shot_only_when_the_exact_frame_fits()
-> Result<(), Box<dyn Error>> {
    let small_source = b"{}\n";
    let oversized_source = vec![b'x'; MAX_FRAME_BYTES];
    let request_id = RequestId::try_new(1)?;

    assert!(source_fits_single_shot_import(
        ConversationImportFormat::CodexRolloutJsonlV1,
        small_source,
        request_id,
    )?);
    assert!(!source_fits_single_shot_import(
        ConversationImportFormat::CodexRolloutJsonlV1,
        &oversized_source,
        request_id,
    )?);
    Ok(())
}

#[test]
fn chunked_import_reads_at_most_one_byte_past_the_declared_size() {
    let declared_size_bytes = CanonicalU64::new(1);

    assert_eq!(
        conversation_import_chunk_read_limit(declared_size_bytes, 0),
        2
    );
    assert_eq!(
        conversation_import_chunk_read_limit(declared_size_bytes, 1),
        1
    );
    assert_eq!(
        conversation_import_chunk_read_limit(declared_size_bytes, 2),
        0
    );
    assert_eq!(
        conversation_import_chunk_read_limit(CanonicalU64::new(u64::MAX), u64::MAX),
        0
    );
}

#[track_caller]
fn assert_append_request(frame: &ClientFrame, expected_chunk: &[u8]) {
    assert_eq!(
        frame.request(),
        &ClientRequest::AppendConversationImport {
            chunk: ConversationImportSource::new(expected_chunk.to_vec()),
        }
    );
}

#[tokio::test]
async fn large_file_import_streams_exact_bounded_assembly_and_commits() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let source = vec![b'x'; MAX_FRAME_BYTES / 4 * 3 + 1];
    let source_path = directory.path().join("source.jsonl");
    fs::write(&source_path, &source)?;
    let source_file = tokio::fs::File::open(source_path).await?;
    let expected_source = source;
    let imported_conversation_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();

        reader.read_until(b'\n', &mut line).await?;
        assert!(line.len() <= MAX_FRAME_BYTES);
        let begin = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            begin.request(),
            &ClientRequest::BeginConversationImport {
                format: ConversationImportFormat::CodexRolloutJsonlV1,
                declared_size_bytes: CanonicalU64::new(
                    u64::try_from(expected_source.len()).map_err(io::Error::other)?,
                ),
            }
        );
        let begun = ServerFrame::try_new_for_version(
            begin.version(),
            begin.request_id(),
            ServerMessage::ConversationImportBegun {
                declared_size_bytes: CanonicalU64::new(
                    u64::try_from(expected_source.len()).map_err(io::Error::other)?,
                ),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&begun).map_err(io::Error::other)?)
            .await?;

        line.clear();
        reader.read_until(b'\n', &mut line).await?;
        assert!(line.len() <= MAX_FRAME_BYTES);
        let first_append = decode_client_line(&line).map_err(io::Error::other)?;
        assert_append_request(
            &first_append,
            &expected_source[..MAX_CONVERSATION_IMPORT_CHUNK_BYTES],
        );
        let first_appended = ServerFrame::try_new_for_version(
            first_append.version(),
            first_append.request_id(),
            ServerMessage::ConversationImportAppended {
                assembled_size_bytes: CanonicalU64::new(
                    u64::try_from(MAX_CONVERSATION_IMPORT_CHUNK_BYTES).map_err(io::Error::other)?,
                ),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&first_appended).map_err(io::Error::other)?)
            .await?;

        line.clear();
        reader.read_until(b'\n', &mut line).await?;
        assert!(line.len() <= MAX_FRAME_BYTES);
        let second_append = decode_client_line(&line).map_err(io::Error::other)?;
        assert_append_request(
            &second_append,
            &expected_source[MAX_CONVERSATION_IMPORT_CHUNK_BYTES..],
        );
        let second_appended = ServerFrame::try_new_for_version(
            second_append.version(),
            second_append.request_id(),
            ServerMessage::ConversationImportAppended {
                assembled_size_bytes: CanonicalU64::new(
                    u64::try_from(expected_source.len()).map_err(io::Error::other)?,
                ),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&second_appended).map_err(io::Error::other)?)
            .await?;

        line.clear();
        reader.read_until(b'\n', &mut line).await?;
        assert!(line.len() <= MAX_FRAME_BYTES);
        let commit = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            commit.request(),
            &ClientRequest::CommitConversationImport {}
        );
        let inserted = ServerFrame::try_new_for_version(
            commit.version(),
            commit.request_id(),
            ServerMessage::ConversationImportInserted {
                imported_conversation_id,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&inserted).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });
    let mut client = ProcessClient::new(socket);

    let outcome = import_conversation_file(
        &mut client,
        ConversationImportFormat::CodexRolloutJsonlV1,
        source_file,
    )
    .await?;

    assert_eq!(
        outcome,
        ConversationImportOutcome::Inserted(imported_conversation_id)
    );
    server.await??;
    Ok(())
}

/// a directory replaced after enumeration cannot redirect a queued candidate read through a
/// symbolic link.
#[tokio::test]
async fn scan_refuses_directory_symlink_replacement() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let queued_directory = root.path().join("queued");
    let retained_directory = root.path().join("retained");
    fs::create_dir(&queued_directory)?;
    fs::write(queued_directory.join("conversation.jsonl"), b"inside")?;
    fs::write(outside.path().join("conversation.jsonl"), b"outside")?;
    let scan = collect_import_paths(root.path())?;
    let candidate = scan
        .paths
        .first()
        .ok_or("fixture must select one candidate")?;
    let relative = candidate.relative.clone();
    fs::rename(&queued_directory, retained_directory)?;
    symlink(outside.path(), &queued_directory)?;

    let opened = open_scanned_import_source(&scan.root, &relative);

    assert!(matches!(opened, Err(ClientError::SourceFile(_))));
    Ok(())
}

/// an unreadable `--system-prompt-file` names the prompt file, not the unrelated
/// conversation-import source, in both its typed variant and its rendered diagnostic.
#[tokio::test]
async fn missing_system_prompt_file_reports_a_prompt_file_failure() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let absent = root.path().join("prompt.txt");

    let failure = read_system_prompt_file(&absent)
        .await
        .expect_err("an absent prompt file must fail");

    assert!(matches!(failure, ClientError::SystemPromptFile(_)));
    assert_eq!(
        failure.to_string(),
        "the system prompt file could not be read"
    );
    Ok(())
}

/// a regular candidate replaced after enumeration by a FIFO is rejected without waiting for a
/// writer.
#[cfg(not(target_vendor = "apple"))]
#[tokio::test]
async fn scan_refuses_fifo_replacement_without_blocking() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let candidate_path = root.path().join("conversation.jsonl");
    fs::write(&candidate_path, b"inside")?;
    let scan = collect_import_paths(root.path())?;
    let candidate = scan
        .paths
        .first()
        .ok_or("fixture must select one candidate")?;
    let relative = candidate.relative.clone();
    fs::remove_file(&candidate_path)?;
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &candidate_path,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;

    let opened = open_scanned_import_source(&scan.root, &relative);

    assert!(matches!(opened, Err(ClientError::SourceFile(_))));
    Ok(())
}

#[test]
fn blob_upload_missing_source_retains_path_and_os_failure() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let absent = root.path().join("absent.bin");

    let Err(failure) = open_blob_source(&absent) else {
        panic!("an absent blob source must fail")
    };
    let ClientError::BlobSourceFile { path, source } = failure else {
        panic!("an absent blob source must retain its path and OS failure")
    };

    assert_eq!(path, absent);
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
    Ok(())
}

#[test]
fn blob_upload_source_failure_display_names_path_and_cause() {
    let path = PathBuf::from("fixture.bin");
    let failure = ClientError::blob_source_file(
        &path,
        io::Error::new(io::ErrorKind::PermissionDenied, "fixture denied"),
    );

    expect_test::expect![[r#"
        the blob upload source file 'fixture.bin' could not be read: fixture denied"#]]
    .assert_eq(&failure.to_string());
}

/// opening a FIFO as an upload source is nonblocking and rejects
/// the descriptor before hashing.
#[cfg(not(target_vendor = "apple"))]
#[test]
fn blob_upload_rejects_fifo_source_without_blocking() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("blob.fifo");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &source,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;

    let opened = open_blob_source(&source);

    let Err(ClientError::BlobSourceFile { path, .. }) = opened else {
        panic!("a FIFO source must retain its rejected path")
    };
    assert_eq!(path, source);
    Ok(())
}

/// a regular seekable source is nonempty when its hash pass reads
/// bytes even if its advisory metadata length is zero.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn blob_upload_counts_bytes_from_the_hash_pass() -> Result<(), Box<dyn Error>> {
    let path = Path::new("/proc/version");
    let mut source = open_blob_source(path)?;
    let metadata_length = source.file.metadata().await?.len();

    let (_digest, observed_length) = hash_blob_source(&mut source.file, path).await?;

    assert_eq!(metadata_length, 0);
    assert!(observed_length.value() > 0);
    Ok(())
}

async fn reply_to_ambiguous_blob_upload(
    listener: &UnixListener,
    bytes: &[u8],
    digest: CanonicalBlobDigest,
    byte_length: CanonicalU64,
    code: ErrorCode,
) -> Result<(), io::Error> {
    let (stream, _) = listener.accept().await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();

    reader.read_until(b'\n', &mut line).await?;
    let begin = decode_client_line(&line).map_err(io::Error::other)?;
    assert_eq!(
        begin.request(),
        &ClientRequest::BeginBlobUpload {
            expected_digest: digest,
            expected_length_bytes: byte_length,
        }
    );
    let begun = ServerFrame::try_new_for_version(
        begin.version(),
        begin.request_id(),
        ServerMessage::BlobUploadBegun {
            expected_digest: digest,
            expected_length_bytes: byte_length,
        },
    )
    .map_err(io::Error::other)?;
    writer
        .write_all(&encode_server_line(&begun).map_err(io::Error::other)?)
        .await?;

    line.clear();
    reader.read_until(b'\n', &mut line).await?;
    let append = decode_client_line(&line).map_err(io::Error::other)?;
    assert_eq!(
        append.request(),
        &ClientRequest::AppendBlobUpload {
            chunk: BlobChunk::new(bytes.to_vec()),
        }
    );
    let appended = ServerFrame::try_new_for_version(
        append.version(),
        append.request_id(),
        ServerMessage::BlobUploadAppended {
            assembled_length_bytes: byte_length,
        },
    )
    .map_err(io::Error::other)?;
    writer
        .write_all(&encode_server_line(&appended).map_err(io::Error::other)?)
        .await?;

    line.clear();
    reader.read_until(b'\n', &mut line).await?;
    let commit = decode_client_line(&line).map_err(io::Error::other)?;
    assert_eq!(commit.request(), &ClientRequest::CommitBlobUpload {});
    let ambiguous = ServerFrame::try_new_for_version(
        commit.version(),
        commit.request_id(),
        ServerMessage::Error {
            code,
            message: String::from("blob publication outcome is ambiguous"),
            detail: ErrorDetail::none(),
        },
    )
    .map_err(io::Error::other)?;
    writer
        .write_all(&encode_server_line(&ambiguous).map_err(io::Error::other)?)
        .await
}

async fn reply_to_restarted_blob_upload(
    listener: &UnixListener,
    digest: CanonicalBlobDigest,
    byte_length: CanonicalU64,
) -> Result<(), io::Error> {
    let (stream, _) = listener.accept().await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    let begin = decode_client_line(&line).map_err(io::Error::other)?;
    assert_eq!(
        begin.request(),
        &ClientRequest::BeginBlobUpload {
            expected_digest: digest,
            expected_length_bytes: byte_length,
        }
    );
    let present = ServerFrame::try_new_for_version(
        begin.version(),
        begin.request_id(),
        ServerMessage::BlobUploadAlreadyPresent {
            digest,
            byte_length,
        },
    )
    .map_err(io::Error::other)?;
    writer
        .write_all(&encode_server_line(&present).map_err(io::Error::other)?)
        .await?;
    line.clear();
    assert_eq!(reader.read_until(b'\n', &mut line).await?, 0);
    Ok(())
}

async fn assert_ambiguous_blob_upload_restarts(code: ErrorCode) -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let source_path = directory.path().join("blob.bin");
    let bytes = b"ambiguously published bytes";
    fs::write(&source_path, bytes)?;
    let digest = CanonicalBlobDigest::from_digest(signalbox_domain::BlobDigest::digest(bytes));
    let byte_length = CanonicalU64::new(u64::try_from(bytes.len())?);
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        reply_to_ambiguous_blob_upload(&listener, bytes, digest, byte_length, code).await?;
        reply_to_restarted_blob_upload(&listener, digest, byte_length).await
    });
    let mut client = ProcessClient::new(socket);
    let source = open_blob_source(&source_path)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);

    upload_blob(&mut client, &mut output, source).await?;

    expect_test::expect![[r#"
        already_present digest=sha256:0e6161e59e9ca2ce9118def8a479a2a9696dc6352eca107b5a580069df4db7e2 byte_length=27
    "#]].assert_eq(&String::from_utf8(stdout)?);
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

/// an ambiguous catalog commit restarts the complete high-level
/// upload instead of retrying commit alone.
#[tokio::test]
async fn blob_upload_restarts_after_ambiguous_catalog_commit() -> Result<(), Box<dyn Error>> {
    assert_ambiguous_blob_upload_restarts(ErrorCode::CommitAmbiguous).await
}

/// an ambiguous remote publication restarts the complete
/// high-level upload instead of retrying commit alone.
#[tokio::test]
async fn blob_upload_restarts_after_ambiguous_publication() -> Result<(), Box<dyn Error>> {
    assert_ambiguous_blob_upload_restarts(ErrorCode::PublicationAmbiguous).await
}

/// an already-present receipt succeeds only after re-reading the
/// same descriptor and proving its identity is unchanged.
#[tokio::test]
async fn blob_upload_revalidates_source_before_deduplication() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let source_path = directory.path().join("blob.bin");
    let original = b"catalogued source bytes";
    let replacement = b"rewritten source bytes";
    fs::write(&source_path, original)?;
    let digest = CanonicalBlobDigest::from_digest(signalbox_domain::BlobDigest::digest(original));
    let byte_length = CanonicalU64::new(u64::try_from(original.len())?);
    let listener = UnixListener::bind(&socket)?;
    let rewritten_path = source_path.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let begin = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            begin.request(),
            &ClientRequest::BeginBlobUpload {
                expected_digest: digest,
                expected_length_bytes: byte_length,
            }
        );
        fs::write(rewritten_path, replacement)?;
        let present = ServerFrame::try_new_for_version(
            begin.version(),
            begin.request_id(),
            ServerMessage::BlobUploadAlreadyPresent {
                digest,
                byte_length,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&present).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });
    let mut client = ProcessClient::new(socket);
    let source = open_blob_source(&source_path)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);

    let failure = upload_blob(&mut client, &mut output, source)
        .await
        .expect_err("a rewritten source must not accept deduplication");

    assert!(matches!(
        failure,
        ClientError::Input("blob source changed after it was hashed")
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

/// the terminal client prehashes one descriptor, streams bounded
/// chunks in order, validates every echo, and reports the committed identity.
#[tokio::test]
async fn blob_upload_streams_the_exact_lifecycle() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let source_path = directory.path().join("blob.bin");
    let first_chunk = vec![b'a'; MAX_BLOB_CHUNK_BYTES];
    let final_chunk = b"terminal blob tail";
    let mut bytes = first_chunk.clone();
    bytes.extend_from_slice(final_chunk);
    fs::write(&source_path, &bytes)?;
    let digest = CanonicalBlobDigest::from_digest(signalbox_domain::BlobDigest::digest(&bytes));
    let byte_length = CanonicalU64::new(u64::try_from(bytes.len())?);
    let first_length = CanonicalU64::new(u64::try_from(first_chunk.len())?);
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();

        reader.read_until(b'\n', &mut line).await?;
        let begin = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            begin.request(),
            &ClientRequest::BeginBlobUpload {
                expected_digest: digest,
                expected_length_bytes: byte_length,
            }
        );
        let begun = ServerFrame::try_new_for_version(
            begin.version(),
            begin.request_id(),
            ServerMessage::BlobUploadBegun {
                expected_digest: digest,
                expected_length_bytes: byte_length,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&begun).map_err(io::Error::other)?)
            .await?;

        line.clear();
        reader.read_until(b'\n', &mut line).await?;
        let append = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            append.request(),
            &ClientRequest::AppendBlobUpload {
                chunk: BlobChunk::new(first_chunk),
            }
        );
        let appended = ServerFrame::try_new_for_version(
            append.version(),
            append.request_id(),
            ServerMessage::BlobUploadAppended {
                assembled_length_bytes: first_length,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&appended).map_err(io::Error::other)?)
            .await?;

        line.clear();
        reader.read_until(b'\n', &mut line).await?;
        let append = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            append.request(),
            &ClientRequest::AppendBlobUpload {
                chunk: BlobChunk::new(final_chunk.to_vec()),
            }
        );
        let appended = ServerFrame::try_new_for_version(
            append.version(),
            append.request_id(),
            ServerMessage::BlobUploadAppended {
                assembled_length_bytes: byte_length,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&appended).map_err(io::Error::other)?)
            .await?;

        line.clear();
        reader.read_until(b'\n', &mut line).await?;
        let commit = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(commit.request(), &ClientRequest::CommitBlobUpload {});
        let committed = ServerFrame::try_new_for_version(
            commit.version(),
            commit.request_id(),
            ServerMessage::BlobUploadCommitted {
                digest,
                byte_length,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&committed).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });
    let mut client = ProcessClient::new(socket);
    let file = tokio::fs::File::open(&source_path).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);

    upload_blob(
        &mut client,
        &mut output,
        PreparedBlobSource {
            path: source_path,
            file,
        },
    )
    .await?;

    expect_test::expect![[r#"
        committed digest=sha256:dc8dba98d0eeeb8521413d99301f4b1efe3a2eab5e514460aabbd3e9b9d5684e byte_length=4194322
    "#]].assert_eq(&String::from_utf8(stdout)?);
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

/// terminal metadata validates echoed identity and prints the
/// bounded catalog facts returned by the daemon.
#[tokio::test]
async fn blob_metadata_preserves_exact_wire_facts() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    let byte_length = CanonicalU64::new(9);
    let replica_count = CanonicalU64::new(1);
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let metadata = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            metadata.request(),
            &ClientRequest::ReadBlobMetadata { digest }
        );
        let metadata_response = ServerFrame::try_new_for_version(
            metadata.version(),
            metadata.request_id(),
            ServerMessage::BlobMetadata {
                digest,
                byte_length,
                replica_count,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&metadata_response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });
    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);

    read_blob_metadata(&mut client, &mut output, digest).await?;

    assert_eq!(
        String::from_utf8(stdout)?,
        format!(
            "digest={digest} byte_length={} replica_count={}\n",
            byte_length.value(),
            replica_count.value()
        )
    );
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

/// a terminal range read validates echoed identity and offset and
/// returns only the exact requested bytes for file delivery.
#[tokio::test]
async fn blob_read_returns_only_the_exact_range() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let digest = CanonicalBlobDigest::from_bytes([0xab; 32]);
    let offset_bytes = CanonicalU64::new(7);
    let bytes = vec![0, 255];
    let length_bytes = CanonicalU64::new(u64::try_from(bytes.len())?);
    let listener = UnixListener::bind(&socket)?;
    let expected_bytes = bytes.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let range = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            range.request(),
            &ClientRequest::ReadBlobChunk {
                digest,
                offset_bytes,
                length_bytes,
            }
        );
        let response = ServerFrame::try_new_for_version(
            range.version(),
            range.request_id(),
            ServerMessage::BlobChunkRead {
                digest,
                offset_bytes,
                bytes: BlobChunk::new(expected_bytes),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });
    let mut client = ProcessClient::new(socket);

    let observed = read_blob_chunk(&mut client, digest, offset_bytes, length_bytes).await?;

    assert_eq!(observed, bytes);
    server.await??;
    Ok(())
}

/// terminal range delivery creates one file containing exactly the
/// bounded bytes returned by the daemon.
#[tokio::test]
async fn blob_output_file_contains_exact_bytes() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("range.bin");
    let bytes = b"exact range bytes";

    write_blob_output(&output, bytes).await?;

    assert_eq!(fs::read(&output)?, bytes);
    Ok(())
}

#[tokio::test]
async fn blob_output_file_uses_private_mode() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("range.bin");

    write_blob_output(&output, b"range").await?;

    assert_eq!(fs::metadata(&output)?.permissions().mode() & 0o777, 0o600);
    Ok(())
}

#[tokio::test]
async fn blob_output_refuses_to_replace_an_existing_file() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("existing.bin");
    let existing = b"existing bytes";
    fs::write(&output, existing)?;

    let failure = write_blob_output(&output, b"replacement")
        .await
        .expect_err("blob range delivery must not replace an existing file");

    let ClientError::BlobOutputFile { path, source } = failure else {
        panic!("an output collision must retain its path and OS failure")
    };
    assert_eq!(path, output);
    assert_eq!(source.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&path)?, existing);
    Ok(())
}

#[tokio::test]
async fn search_rejects_a_page_that_exceeds_its_requested_bound() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ListSessionMetadata {
                required_tags: Vec::new(),
                title_contains: None,
                include_archived: false,
                page_size: CanonicalU64::new(1),
                after_session_id: None,
            }
        );
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let summary = |seed| ServerMessage::SessionMetadataSummary {
            session_id: CanonicalUuid::from_uuid(Uuid::from_u128(seed)),
            defaults_version: CanonicalU64::new(1),
            model_selection: ModelSelection::Direct {
                selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(9)),
            },
            dangerous_tool_auto_approval: false,
            title: None,
            tags: Vec::new(),
            archived: false,
            last_writer: None,
        };
        let mut response = Vec::new();
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::SessionMetadataPageStart {})?)
                .map_err(io::Error::other)?,
        );
        response
            .extend_from_slice(&encode_server_line(&frame(summary(1))?).map_err(io::Error::other)?);
        response
            .extend_from_slice(&encode_server_line(&frame(summary(2))?).map_err(io::Error::other)?);
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let result = search(
        &mut client,
        &mut output,
        SessionMetadataPageRequest {
            required_tags: Vec::new(),
            title_contains: None,
            include_archived: false,
            page_size: CanonicalU64::new(1),
            after_session_id: None,
        },
    )
    .await;

    assert!(matches!(
        result,
        Err(ClientError::Protocol(
            "session metadata page exceeded its requested bound"
        ))
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

/// the inspection read is the client's source of selectable positions, so a gap in the emitted
/// sequence is rejected before any row can suggest a position the daemon did not emit.
#[tokio::test]
async fn imported_rejects_noncontiguous_positions_before_writing_rows() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let imported_conversation_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ReadImportedConversation {
                imported_conversation_id,
            }
        );
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let entry = |position| ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(position),
            imported_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::from(
                100 - position,
            ))),
            source_speaker: ImportedSourceSpeaker::NotAttested {},
            content_kind: ImportedContentKind::SourceEvent,
            text_preview: None,
        };
        let mut response = Vec::new();
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ImportedConversationStart {
                imported_conversation_id,
            })?)
            .map_err(io::Error::other)?,
        );
        response
            .extend_from_slice(&encode_server_line(&frame(entry(1))?).map_err(io::Error::other)?);
        response
            .extend_from_slice(&encode_server_line(&frame(entry(3))?).map_err(io::Error::other)?);
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let result = imported(&mut client, &mut output, imported_conversation_id).await;

    assert!(matches!(
        result,
        Err(ClientError::Protocol(
            "imported entry positions were not the contiguous sequence from one"
        ))
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

/// an imported conversation's normalized entry sequence is nonempty, so an empty inventory
/// contradicts the record the daemon claims to be reading. The shared reader fails closed on it
/// rather than printing a conversation with no selectable position.
#[tokio::test]
async fn imported_rejects_an_empty_entry_inventory() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let imported_conversation_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ReadImportedConversation {
                imported_conversation_id,
            }
        );
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let mut response = Vec::new();
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ImportedConversationStart {
                imported_conversation_id,
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ImportedConversationEnd {
                imported_conversation_id,
                entry_count: CanonicalU64::new(0),
            })?)
            .map_err(io::Error::other)?,
        );
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let result = imported(&mut client, &mut output, imported_conversation_id).await;

    assert!(matches!(
        result,
        Err(ClientError::Protocol(
            "imported conversation reported no entries"
        ))
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

/// `latest` resolves against the imported conversation's own declared entry count and reaches
/// the wire as that concrete ordinal, so the durable command an exact replay reconstructs is
/// unchanged.
#[tokio::test]
async fn continue_resolves_latest_to_a_concrete_wire_position() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let imported_conversation_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(2))?;
    let selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ReadImportedConversation {
                imported_conversation_id,
            }
        );
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let entry = |position| ServerMessage::ImportedConversationEntry {
            position: CanonicalU64::new(position),
            imported_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(u128::from(
                100 - position,
            ))),
            source_speaker: ImportedSourceSpeaker::NotAttested {},
            content_kind: ImportedContentKind::SourceEvent,
            text_preview: None,
        };
        let mut response = Vec::new();
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ImportedConversationStart {
                imported_conversation_id,
            })?)
            .map_err(io::Error::other)?,
        );
        response
            .extend_from_slice(&encode_server_line(&frame(entry(1))?).map_err(io::Error::other)?);
        response
            .extend_from_slice(&encode_server_line(&frame(entry(2))?).map_err(io::Error::other)?);
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ImportedConversationEnd {
                imported_conversation_id,
                entry_count: CanonicalU64::new(2),
            })?)
            .map_err(io::Error::other)?,
        );
        writer.write_all(&response).await?;

        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::CreateSessionFromImportedFrontier {
                command_id,
                imported_conversation_id,
                through_position: CanonicalU64::new(2),
                relationship: ImportedSessionRelationship::Resume,
                initial_model_selection: ModelSelection::Direct { selection_id },
                model_settings: ModelSettingsOverlay::inherit_all(),
            }
        );
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::SessionCreated {
                session_id,
                model_settings: provider_default_model_settings(),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    continue_imported(
        &mut client,
        &mut output,
        imported_conversation_id,
        ThroughPositionArgument::Latest,
        ImportedSessionRelationship::Resume,
        ModelSelection::Direct { selection_id },
        Some(command_id),
    )
    .await?;

    assert_eq!(String::from_utf8(stdout)?, format!("{session_id}\n"));
    assert_eq!(String::from_utf8(stderr)?, "through_position=2\n");
    server.await??;
    Ok(())
}

#[tokio::test]
async fn conversations_rejects_a_page_that_exceeds_its_requested_bound()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ListConversations {
                title_contains: None,
                origin: ConversationOriginFilter::All,
                include_archived: false,
                page_size: CanonicalU64::new(1),
                after: None,
            }
        );
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let summary = |seed| ServerMessage::ConversationSummary {
            conversation: ConversationSummary::NativeSession {
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(seed)),
                title: None,
                archived: false,
                defaults_version: CanonicalU64::new(1),
            },
        };
        let mut response = Vec::new();
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ConversationPageStart {})?)
                .map_err(io::Error::other)?,
        );
        response
            .extend_from_slice(&encode_server_line(&frame(summary(1))?).map_err(io::Error::other)?);
        response
            .extend_from_slice(&encode_server_line(&frame(summary(2))?).map_err(io::Error::other)?);
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let result = conversations(
        &mut client,
        &mut output,
        ConversationsPageRequest {
            title_contains: None,
            origin: ConversationOriginFilter::All,
            include_archived: false,
            page_size: CanonicalU64::new(1),
            after: None,
        },
    )
    .await;

    assert!(matches!(
        result,
        Err(ClientError::Protocol(
            "conversation page exceeded its requested bound"
        ))
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn conversations_rejects_summaries_out_of_unified_cursor_order() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let mut response = Vec::new();
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ConversationPageStart {})?)
                .map_err(io::Error::other)?,
        );
        // The imported row shares identity value 1 with the native row
        // that follows it, so the pair inverts the native-before-imported
        // tiebreak of the unified order.
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ConversationSummary {
                conversation: ConversationSummary::ImportedConversation {
                    imported_conversation_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                    title: None,
                    entry_count: CanonicalU64::new(1),
                    source_format:
                        signalbox_process_protocol::ImportedConversationSourceFormat::CodexRolloutJsonlV1,
                },
            })?)
            .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ConversationSummary {
                conversation: ConversationSummary::NativeSession {
                    session_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
                    title: None,
                    archived: false,
                    defaults_version: CanonicalU64::new(1),
                },
            })?)
            .map_err(io::Error::other)?,
        );
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let result = conversations(
        &mut client,
        &mut output,
        ConversationsPageRequest {
            title_contains: None,
            origin: ConversationOriginFilter::All,
            include_archived: false,
            page_size: CanonicalU64::new(50),
            after: None,
        },
    )
    .await;

    assert!(matches!(
        result,
        Err(ClientError::Protocol(
            "conversation summaries were not strictly ordered"
        ))
    ));
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn review_records_an_empty_complete_finding_inventory() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let findings_file = directory.path().join("findings.json");
    fs::write(&findings_file, br#"{"findings":[]}"#)?;
    let listener = UnixListener::bind(&socket)?;
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))?;
    let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let pass_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let output_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::RecordReviewFindings {
                command_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                findings: Vec::new(),
            }
        );
        let frame = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::ReviewFindingsRecorded {
                run_id,
                pass_id,
                finding_count: CanonicalU64::new(0),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    review(
        &mut client,
        &mut output,
        ReviewCommand::RecordFindings {
            command_id: Some(command_id),
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            findings_file,
        },
        Some(ClientDeploymentLimits::unbounded()),
    )
    .await?;

    let expected_stdout = format!("run={run_id} pass={pass_id} findings=0 recorded\n");
    assert_eq!(String::from_utf8(stdout)?, expected_stdout);
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn review_reserves_an_external_publication_link() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))?;
    let finding_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let external_link_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ReserveReviewExternalLink {
                command_id,
                external_link_id,
                finding_id,
                provider: String::from("example-host"),
                object_kind: ReviewExternalObjectKind::ReviewComment,
            }
        );
        let frame = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::ReviewExternalLinkReserved { external_link_id },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    review(
        &mut client,
        &mut output,
        ReviewCommand::ReserveExternalLink {
            command_id: Some(command_id),
            external_link_id,
            finding_id,
            provider: String::from("example-host"),
            object_kind: ReviewExternalObjectKind::ReviewComment,
        },
        None,
    )
    .await?;

    assert_eq!(
        String::from_utf8(stdout)?,
        format!("external_link={external_link_id} reserved\n")
    );
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn review_attaches_an_external_publication_link() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(1))?;
    let external_link_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let pass_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
    let output_frontier_id = CanonicalUuid::from_uuid(Uuid::from_u128(6));
    let event_ordinal = CanonicalU64::new(1);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::AttachReviewExternalLink {
                command_id,
                external_link_id,
                run_id,
                pass_id,
                turn_id,
                output_frontier_id,
                external_object: String::from("provider-object-7"),
                event_ordinal,
            }
        );
        let frame = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::ReviewExternalLinkAttached {
                external_link_id,
                external_object: String::from("provider-object-7"),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    review(
        &mut client,
        &mut output,
        ReviewCommand::AttachExternalLink {
            command_id: Some(command_id),
            external_link_id,
            run_id,
            pass_id,
            turn_id,
            output_frontier_id,
            external_object: String::from("provider-object-7"),
            event_ordinal,
        },
        None,
    )
    .await?;

    assert_eq!(
        String::from_utf8(stdout)?,
        format!("external_link={external_link_id} attached\n")
    );
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn review_list_rejects_terminal_count_before_writing_items() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let finding_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ListReviewFindings { run_id }
        );
        let frame = |message| {
            ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
                .map_err(io::Error::other)
        };
        let finding = ReviewFindingSnapshot {
            target_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            run_id,
            producing_pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
            finding: ReviewFindingInput {
                finding_id,
                file_path: String::from("src/review.rs"),
                line_start: Some(CanonicalU64::new(11)),
                line_end: Some(CanonicalU64::new(14)),
                diff_side: None,
                title: String::from("Retain the exact edge"),
                body: String::from("The terminal count must authenticate the list."),
                severity: ReviewSeverity::High,
                is_real_confidence: CanonicalU64::new(9_000),
                severity_label_confidence: CanonicalU64::new(8_500),
                category: String::from("correctness"),
                recommended_fix: None,
            },
            status: ReviewFindingStatus::Open,
            event_count: CanonicalU64::new(0),
        };
        let mut response = Vec::new();
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ReviewFindingsStart { run_id })?)
                .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ReviewFindingItem { finding })?)
                .map_err(io::Error::other)?,
        );
        response.extend_from_slice(
            &encode_server_line(&frame(ServerMessage::ReviewFindingsEnd {
                finding_count: CanonicalU64::new(2),
            })?)
            .map_err(io::Error::other)?,
        );
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let error = review(
        &mut client,
        &mut output,
        ReviewCommand::ListFindings { run_id },
        Some(ClientDeploymentLimits::unbounded()),
    )
    .await
    .expect_err("the mismatched terminal count must reject the list");

    assert_eq!(
        error.to_string(),
        "the server violated the process protocol: review finding list sequence or count was invalid"
    );
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn review_list_rejects_structural_overflow_without_waiting_for_the_end()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let count = signalbox_process_protocol::MAX_REVIEW_PRODUCED_FINDINGS as u64 + 1;
    let (done, finished) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ListReviewFindings { run_id }
        );
        let response =
            review_finding_items_response(&request, run_id, count).map_err(io::Error::other)?;
        writer.write_all(&response).await?;
        // Keep the connection open without an end marker until the client rejects it.
        let _ = finished.await;
        Ok::<(), io::Error>(())
    });
    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        review(
            &mut client,
            &mut output,
            ReviewCommand::ListFindings { run_id },
            None,
        ),
    )
    .await?
    .expect_err("the shared structural limit must reject an unterminated oversized list");
    assert_eq!(
        error.to_string(),
        "the server violated the process protocol: review finding list exceeded its structural count limit"
    );
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    let _ = done.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn review_list_remains_readable_after_admission_limit_is_lowered()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let run_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ListReviewFindings { run_id }
        );
        let count = REVIEW_FINDING_LIMIT_FIXTURE + 1;
        let mut response =
            review_finding_items_response(&request, run_id, count).map_err(io::Error::other)?;
        let end = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::ReviewFindingsEnd {
                finding_count: CanonicalU64::new(count),
            },
        )
        .map_err(io::Error::other)?;
        response.extend_from_slice(&encode_server_line(&end).map_err(io::Error::other)?);
        writer.write_all(&response).await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    review(
        &mut client,
        &mut output,
        ReviewCommand::ListFindings { run_id },
        Some(ClientDeploymentLimits {
            max_review_findings_per_run: Some(REVIEW_FINDING_LIMIT_FIXTURE),
            ..ClientDeploymentLimits::unbounded()
        }),
    )
    .await?;

    assert!(!stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn create_connection_failure_is_definitely_uncommitted() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let mut client = ProcessClient::new(directory.path().join("missing.sock"));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);

    let result = create(
        &mut client,
        &mut output,
        ModelSelection::Direct {
            selection_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        },
        Some(CommandId::try_from_uuid(Uuid::from_u128(2))?),
        None,
        super::SessionPlacement::Pathless {},
    )
    .await;

    assert!(matches!(result, Err(ClientError::DaemonIo(_))));
    Ok(())
}

#[tokio::test]
async fn create_rejects_settings_for_another_direct_model() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let requested_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let returned_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(3))?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(4));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::CreateSession {
                command_id,
                initial_model_selection: ModelSelection::Direct {
                    selection_id: requested_selection_id,
                },
                model_settings: ModelSettingsOverlay::inherit_all(),
                system_prompt: SystemPromptMember::present(None),
                placement: SessionPlacement::Pathless {},
                lifecycle: signalbox_process_protocol::SessionLifecycleMembers::default(),
            }
        );
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::SessionCreated {
                session_id,
                model_settings: session_reasoning_model_settings(returned_selection_id),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let error = create(
        &mut client,
        &mut output,
        ModelSelection::Direct {
            selection_id: requested_selection_id,
        },
        Some(command_id),
        None,
        SessionPlacement::Pathless {},
    )
    .await
    .expect_err("creation must reject settings validated for another direct model");

    assert!(error.is_ambiguous_mutation());
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn model_replacement_preserves_the_session_settings_layer() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let prior_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let installed_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
    let expected_session_settings = session_reasoning_model_settings(prior_selection_id)
        .precedence
        .session;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let read = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            read.request(),
            &ClientRequest::ReadSessionDefaults {
                session_id,
                defaults_version: None,
            }
        );
        let read_response = ServerFrame::try_new_for_version(
            read.version(),
            read.request_id(),
            ServerMessage::SessionDefaults {
                session_id,
                defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: prior_selection_id,
                },
                model_settings: session_reasoning_model_settings(prior_selection_id),
                dangerous_tool_auto_approval: false,
                system_prompt: None,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&read_response).map_err(io::Error::other)?)
            .await?;

        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let replace = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            replace.request(),
            &ClientRequest::ReplaceSessionDefaults {
                command_id,
                session_id,
                expected_defaults_version: CanonicalU64::new(1),
                model_selection: ModelSelection::Direct {
                    selection_id: installed_selection_id,
                },
                model_settings: expected_session_settings,
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            }
        );
        let replace_response = ServerFrame::try_new_for_version(
            replace.version(),
            replace.request_id(),
            ServerMessage::SessionDefaultsReplaced {
                session_id,
                defaults_version: CanonicalU64::new(2),
                model_selection: ModelSelection::Direct {
                    selection_id: installed_selection_id,
                },
                model_settings: session_reasoning_model_settings(installed_selection_id),
                dangerous_tool_auto_approval: false,
                system_prompt: SystemPromptMember::present(None),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&replace_response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    replace_session_model(
        &mut client,
        &mut output,
        session_id,
        ModelSelection::Direct {
            selection_id: installed_selection_id,
        },
        Some(command_id),
        None,
        None,
        ModelSystemPromptChoice::Keep,
    )
    .await?;

    assert_eq!(
        String::from_utf8(stderr)?,
        "defaults_version=1\ndangerous_tool_auto_approval=disabled\n"
    );
    assert_eq!(
        String::from_utf8(stdout)?,
        format!("session={session_id} defaults_version=2 model={installed_selection_id}\n")
    );
    server.await??;
    Ok(())
}

#[test]
fn model_replacement_rejects_a_receipt_with_another_session_settings_layer() {
    let prior_selection_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let expected_session_settings = session_reasoning_model_settings(prior_selection_id)
        .precedence
        .session;
    let returned = provider_default_model_settings();

    assert!(!replacement_receipt_settings_match(
        expected_session_settings,
        &returned
    ));
}

#[tokio::test]
async fn submit_connection_failure_is_definitely_uncommitted() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let mut client = ProcessClient::new(directory.path().join("missing.sock"));

    let result = submit_input(
        &mut client,
        CommandId::try_from_uuid(Uuid::from_u128(1))?,
        CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        InputContent::new(String::from("queued content")),
        Some(CanonicalU64::new(1)),
        None,
    )
    .await;

    assert!(matches!(result, Err(ClientError::DaemonIo(_))));
    Ok(())
}

#[tokio::test]
async fn submit_input_rejects_stop_termination_metadata() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let content = InputContent::new(String::from("continue"));
    let expected_request = ClientRequest::SubmitInput {
        command_id,
        session_id,
        content: UserInputContent::text(content.clone().into_string()),
        expected_defaults_version: Some(CanonicalU64::new(1)),
        model_settings: ModelSettingsOverlay::inherit_all(),
        delivery: None,
    };
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &expected_request,
            ServerMessage::InputSubmitted {
                termination: Some(signalbox_process_protocol::TerminationReceipt {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    descendant_count: CanonicalU64::new(0),
                }),
                session_id,
                accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                acceptance_position: CanonicalU64::new(1),
                turn_id,
                model_settings: provider_default_model_settings(),
            },
        )
        .await
    });
    let mut client = ProcessClient::new(socket);
    let result = submit_input(
        &mut client,
        command_id,
        session_id,
        content,
        Some(CanonicalU64::new(1)),
        None,
    )
    .await;
    assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn reconcile_turn_rejects_stop_termination_metadata() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let content = InputContent::new(String::from("continue"));
    let expected_request = ClientRequest::ReconcileTurn {
        command_id,
        session_id,
        expected_active_turn_id: turn_id,
        content: UserInputContent::text(content.clone().into_string()),
        expected_defaults_version: CanonicalU64::new(1),
        model_settings: ModelSettingsOverlay::inherit_all(),
    };
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &expected_request,
            ServerMessage::InputSubmitted {
                termination: Some(signalbox_process_protocol::TerminationReceipt {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    descendant_count: CanonicalU64::new(0),
                }),
                session_id,
                accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                acceptance_position: CanonicalU64::new(1),
                turn_id,
                model_settings: provider_default_model_settings(),
            },
        )
        .await
    });
    let mut client = ProcessClient::new(socket);
    let result = reconcile_turn(
        &mut client,
        command_id,
        session_id,
        turn_id,
        content,
        CanonicalU64::new(1),
    )
    .await;
    assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn submit_input_releases_its_connection_after_acceptance() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
    let content = InputContent::new(String::from("queued content"));
    let expected_content = UserInputContent::text(content.clone().into_string());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::SubmitInput {
                command_id,
                session_id,
                content: expected_content,
                expected_defaults_version: Some(CanonicalU64::new(1)),
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: None,
            }
        );
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::InputSubmitted {
                termination: None,
                session_id,
                accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                acceptance_position: CanonicalU64::new(1),
                turn_id,
                model_settings: provider_default_model_settings(),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;

        let mut byte = [0_u8; 1];
        let read = timeout(Duration::from_secs(1), reader.read(&mut byte))
            .await
            .map_err(io::Error::other)??;
        assert_eq!(read, 0);
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let submitted_turn = submit_input(
        &mut client,
        command_id,
        session_id,
        content,
        Some(CanonicalU64::new(1)),
        None,
    )
    .await?;
    assert_eq!(submitted_turn, SubmitInputReceipt::Turn { turn_id });
    server.await??;
    Ok(())
}

/// the current client sends the configuration-free request and returns its typed
/// accepted-input/source-turn receipt.
#[tokio::test]
async fn current_client_uses_the_exact_steering_exchange() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let source_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let accepted_input_id = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(request.version(), ProtocolVersion::One);
        assert_eq!(
            request.request(),
            &ClientRequest::SubmitInput {
                command_id: CommandId::try_from_uuid(Uuid::from_u128(4))
                    .map_err(io::Error::other)?,
                session_id,
                content: UserInputContent::text(String::from("steering content")),
                expected_defaults_version: None,
                model_settings: ModelSettingsOverlay::inherit_all(),
                delivery: Some(InputDelivery::Steer {
                    expected_active_turn_id: source_turn_id,
                }),
            }
        );
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::SteeringSubmitted {
                session_id,
                accepted_input_id,
                acceptance_position: CanonicalU64::new(2),
                source_turn_id,
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let receipt = submit_input(
        &mut client,
        CommandId::try_from_uuid(Uuid::from_u128(4))?,
        session_id,
        InputContent::new(String::from("steering content")),
        None,
        Some(InputDelivery::Steer {
            expected_active_turn_id: source_turn_id,
        }),
    )
    .await?;
    assert_eq!(
        receipt,
        SubmitInputReceipt::Steering {
            accepted_input_id,
            acceptance_position: 2,
            source_turn_id,
        }
    );
    server.await??;
    Ok(())
}

/// the reconciliation verb names the exact parked turn on the
/// wire and returns the accepted successor turn.
#[tokio::test]
async fn reconcile_turn_names_the_parked_turn_and_returns_its_successor()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let parked_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let successor_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
    let defaults_version = CanonicalU64::new(1);
    let content = InputContent::new(String::from("continue after reconciliation"));
    let expected_content = UserInputContent::text(content.clone().into_string());
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(
            request.request(),
            &ClientRequest::ReconcileTurn {
                command_id,
                session_id,
                expected_active_turn_id: parked_turn_id,
                content: expected_content,
                expected_defaults_version: defaults_version,
                model_settings: ModelSettingsOverlay::inherit_all(),
            }
        );
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::InputSubmitted {
                termination: None,
                session_id,
                accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                acceptance_position: CanonicalU64::new(2),
                turn_id: successor_turn_id,
                model_settings: provider_default_model_settings(),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let accepted_successor = reconcile_turn(
        &mut client,
        command_id,
        session_id,
        parked_turn_id,
        content,
        defaults_version,
    )
    .await?;
    assert_eq!(accepted_successor, successor_turn_id);
    server.await??;
    Ok(())
}

/// the stop verb names the exact expected active turn on the
/// wire and returns the accepted successor turn.
#[tokio::test]
async fn stop_turn_names_the_active_turn_and_returns_its_successor() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let active_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let successor_turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(5));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
    let content = InputContent::new(String::from("continue after the stop"));
    let defaults_version = CanonicalU64::new(1);
    let selected_scope = DescendantTerminationScope::ParentAndDescendants;
    let expected_request = ClientRequest::StopTurn {
        command_id,
        session_id,
        expected_active_turn_id: active_turn_id,
        content: UserInputContent::text(content.clone().into_string()),
        expected_defaults_version: defaults_version,
        descendant_scope: selected_scope,
        model_settings: ModelSettingsOverlay::inherit_all(),
    };
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(request.request(), &expected_request);
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::InputSubmitted {
                termination: Some(signalbox_process_protocol::TerminationReceipt {
                    descendant_scope: selected_scope,
                    descendant_count: CanonicalU64::new(2),
                }),
                session_id,
                accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                acceptance_position: CanonicalU64::new(2),
                turn_id: successor_turn_id,
                model_settings: provider_default_model_settings(),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut client = ProcessClient::new(socket);
    let accepted_successor = stop_turn(
        &mut client,
        command_id,
        session_id,
        active_turn_id,
        content,
        defaults_version,
        selected_scope,
    )
    .await?;
    assert_eq!(accepted_successor.turn_id, successor_turn_id);
    server.await??;
    Ok(())
}

/// Override arming sends its own canonical command and accepts only the exact
/// overridden-request receipt.
#[tokio::test]
async fn override_validates_the_exact_recorded_receipt() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let command_id = CommandId::try_from_uuid(Uuid::from_u128(4))?;
    struct Case {
        receipt: ServerMessage,
        accepted: bool,
    }
    let cases = [
        Case {
            receipt: ServerMessage::ToolDenialOverridden { tool_request_id },
            accepted: true,
        },
        Case {
            receipt: ServerMessage::ToolDenialOverridden {
                tool_request_id: session_id,
            },
            accepted: false,
        },
        Case {
            receipt: ServerMessage::ToolRequestDecided {
                tool_request_id,
                decision: ToolDecision::Approve {},
            },
            accepted: false,
        },
    ];
    for case in cases {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::OverrideDeniedToolRequest {
                    command_id,
                    session_id,
                    tool_request_id,
                }
            );
            let response = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                case.receipt,
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
                .await?;
            Ok::<(), io::Error>(())
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr, false);
        let result = crate::override_denial(
            &mut ProcessClient::new(socket),
            &mut output,
            session_id,
            tool_request_id,
            Some(command_id),
        )
        .await;
        server.await??;

        assert_eq!(result.is_ok(), case.accepted, "{result:?}");
        assert_eq!(!stdout.is_empty(), case.accepted);
        assert_eq!(
            String::from_utf8(stderr)?.contains("one extra model round"),
            case.accepted
        );
    }
    Ok(())
}

#[tokio::test]
async fn decide_validates_the_exact_recorded_receipt() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert!(matches!(
            request.request(),
            ClientRequest::DecideToolRequest {
                session_id: requested_session,
                tool_request_id: requested_tool,
                decision: ToolDecision::Deny { reason },
                ..
            } if *requested_session == session_id
                && *requested_tool == tool_request_id
                && reason == "writes outside the workspace"
        ));
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::ToolRequestDecided {
                tool_request_id,
                decision: ToolDecision::Deny {
                    reason: String::from("writes outside the workspace"),
                },
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let mut client = ProcessClient::new(socket);
    decide(
        &mut client,
        &mut output,
        session_id,
        tool_request_id,
        Some(CommandId::try_from_uuid(Uuid::from_u128(4))?),
        ToolDecision::Deny {
            reason: String::from("writes outside the workspace"),
        },
    )
    .await?;
    server.await??;
    assert_eq!(
        String::from_utf8(stdout)?,
        format!("tool_request={tool_request_id} decision=deny\n")
    );
    assert_eq!(String::from_utf8(stderr)?, "");
    Ok(())
}

/// a receipt naming a different request or decision is a
/// protocol violation, never silently accepted.
#[tokio::test]
async fn decide_rejects_a_receipt_for_a_different_decision() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::ToolRequestDecided {
                tool_request_id,
                decision: ToolDecision::Approve {},
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let mut client = ProcessClient::new(socket);
    let result = decide(
        &mut client,
        &mut output,
        session_id,
        tool_request_id,
        Some(CommandId::try_from_uuid(Uuid::from_u128(4))?),
        ToolDecision::Deny {
            reason: String::from("writes outside the workspace"),
        },
    )
    .await;
    assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
    server.await??;
    Ok(())
}

/// `decide` accepts only its own receipt. A `tool_denial_overridden`
/// receipt names a distinct command — it proves a one-shot override was
/// recorded for a future re-proposal, never that this pending request was
/// decided — so naming the same request cannot make it stand in for one.
#[tokio::test]
async fn decide_rejects_a_denial_override_receipt() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let tool_request_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let server = tokio::spawn(async move {
        let (stream, mut writer) = listener.accept().await?.0.into_split();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        let response = ServerFrame::try_new_for_version(
            request.version(),
            request.request_id(),
            ServerMessage::ToolDenialOverridden { tool_request_id },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&response).map_err(io::Error::other)?)
            .await?;
        Ok::<(), io::Error>(())
    });

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    let mut client = ProcessClient::new(socket);
    let result = decide(
        &mut client,
        &mut output,
        session_id,
        tool_request_id,
        Some(CommandId::try_from_uuid(Uuid::from_u128(4))?),
        ToolDecision::Approve {},
    )
    .await;
    assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
    server.await??;
    assert_eq!(String::from_utf8(stdout)?, "");
    Ok(())
}

const DELEGATION_SESSION: &str = "00000000-0000-0000-0000-000000000001";
const DELEGATION_TURN: &str = "00000000-0000-0000-0000-000000000002";
const DELEGATION_SPAWN_REQUEST: &str = "00000000-0000-0000-0000-000000000003";
const DELEGATION_CHILD: &str = "00000000-0000-0000-0000-000000000004";
const DELEGATION_AWAIT_REQUEST: &str = "00000000-0000-0000-0000-000000000005";
const DELEGATION_CHILD_TURN: &str = "00000000-0000-0000-0000-000000000006";
const DELEGATION_MESSAGE_REQUEST: &str = "00000000-0000-0000-0000-000000000007";
const DELEGATION_MESSAGE: &str = "00000000-0000-0000-0000-000000000008";
const DELEGATION_BACKGROUND_AWAIT_REQUEST: &str = "00000000-0000-0000-0000-000000000009";
const DELEGATION_FOREIGN_PARENT: &str = "00000000-0000-0000-0000-00000000000a";

struct DelegationVerbResult {
    exit: ExitCode,
    stdout: String,
    stderr: String,
}

async fn run_delegation_verb(
    command: &[&str],
    expected: ClientRequest,
    response: ServerMessage,
) -> Result<DelegationVerbResult, Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let server =
        tokio::spawn(async move { accept_request_and_reply(&listener, &expected, response).await });
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = run(
        client_arguments(&socket, command),
        None,
        &mut input,
        &mut stdout,
        &mut stderr,
    )
    .await;
    server.await??;
    Ok(DelegationVerbResult {
        exit,
        stdout: String::from_utf8(stdout)?,
        stderr: String::from_utf8(stderr)?,
    })
}

#[tokio::test]
async fn delegation_spawn_encodes_request_and_renders_receipt() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
    let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
    let relationship = DelegationPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let result = run_delegation_verb(
        &[
            "session",
            "spawn",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_SPAWN_REQUEST,
            "--task",
            "inspect logs",
            "--bound",
            "--on-parent-stopped",
            "stop",
            "--on-parent-cancelled",
            "cancel",
        ],
        ClientRequest::SpawnSession {
            session_id,
            turn_id,
            tool_request_id: request_id,
            task: String::from("inspect logs"),
            relationship,
        },
        ServerMessage::SessionSpawned {
            tool_request_id: request_id,
            child_session_id: child_id,
            relationship,
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::SUCCESS);
    assert_eq!(
        result.stdout,
        format!(
            "spawn_request={request_id} child_session={child_id} relationship=bound on_parent_stopped=stop on_parent_cancelled=cancel\n"
        )
    );
    assert_eq!(result.stderr, "");
    Ok(())
}

#[tokio::test]
async fn delegation_spawn_rejects_the_parent_as_its_own_child() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
    let relationship = DelegationPolicy::Bound {
        on_parent_stopped: BoundChildAction::Stop,
        on_parent_cancelled: BoundChildAction::Cancel,
    };
    let result = run_delegation_verb(
        &[
            "session",
            "spawn",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_SPAWN_REQUEST,
            "--task",
            "inspect logs",
            "--bound",
            "--on-parent-stopped",
            "stop",
            "--on-parent-cancelled",
            "cancel",
        ],
        ClientRequest::SpawnSession {
            session_id,
            turn_id,
            tool_request_id: request_id,
            task: String::from("inspect logs"),
            relationship,
        },
        ServerMessage::SessionSpawned {
            tool_request_id: request_id,
            child_session_id: session_id,
            relationship,
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::FAILURE);
    assert_eq!(result.stdout, "");
    assert!(!result.stderr.is_empty());
    Ok(())
}

#[tokio::test]
async fn delegation_foreground_await_encodes_request_and_renders_result()
-> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_AWAIT_REQUEST)?);
    let spawn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
    let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
    let child_turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD_TURN)?);
    let result = run_delegation_verb(
        &[
            "session",
            "await",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_AWAIT_REQUEST,
            DELEGATION_CHILD,
            "--mode",
            "foreground",
        ],
        ClientRequest::AwaitSession {
            session_id,
            turn_id,
            tool_request_id: request_id,
            child_session_id: child_id,
            mode: DelegationWaitMode::Foreground,
        },
        ServerMessage::ChildResult {
            await_request_id: request_id,
            spawning_request_id: spawn_id,
            child_session_id: child_id,
            outcome: DelegationOutcome::Returned,
            content: Some(String::from("done\nnow")),
            reason: DelegationReason::ChildCompleted,
            provenance: DelegationProvenance::ChildTurn {
                child_session_id: child_id,
                child_turn_id,
            },
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::SUCCESS);
    assert_eq!(
        result.stdout,
        format!(
            "await_request={request_id} spawning_request={spawn_id} child_session={child_id} delivery=foreground outcome=returned reason=child_completed provenance=child_turn:{child_id}:{child_turn_id} content=done\\u{{a}}now\n"
        )
    );
    assert_eq!(result.stderr, "");
    Ok(())
}

#[tokio::test]
async fn delegation_foreground_await_rejects_another_parent_provenance()
-> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let foreign_parent = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_FOREIGN_PARENT)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_AWAIT_REQUEST)?);
    let spawn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
    let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
    let command_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE)?);
    let result = run_delegation_verb(
        &[
            "session",
            "await",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_AWAIT_REQUEST,
            DELEGATION_CHILD,
            "--mode",
            "foreground",
        ],
        ClientRequest::AwaitSession {
            session_id,
            turn_id,
            tool_request_id: request_id,
            child_session_id: child_id,
            mode: DelegationWaitMode::Foreground,
        },
        ServerMessage::ChildResult {
            await_request_id: request_id,
            spawning_request_id: spawn_id,
            child_session_id: child_id,
            outcome: DelegationOutcome::Stopped,
            content: None,
            reason: DelegationReason::ParentStopped,
            provenance: DelegationProvenance::ParentGoalCommand {
                parent_session_id: foreign_parent,
                goal_generation: CanonicalU64::new(1),
                command_id,
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::FAILURE);
    assert_eq!(result.stdout, "");
    assert!(!result.stderr.is_empty());
    Ok(())
}

#[tokio::test]
async fn delegation_background_await_encodes_request_and_renders_registration()
-> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id =
        CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_BACKGROUND_AWAIT_REQUEST)?);
    let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
    let result = run_delegation_verb(
        &[
            "session",
            "await",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_BACKGROUND_AWAIT_REQUEST,
            DELEGATION_CHILD,
            "--mode",
            "background",
        ],
        ClientRequest::AwaitSession {
            session_id,
            turn_id,
            tool_request_id: request_id,
            child_session_id: child_id,
            mode: DelegationWaitMode::Background,
        },
        ServerMessage::SessionAwaitRegistered {
            tool_request_id: request_id,
            child_session_id: child_id,
            mode: DelegationWaitMode::Background,
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::SUCCESS);
    assert_eq!(
        result.stdout,
        format!("await_request={request_id} child_session={child_id} mode=background\n")
    );
    assert_eq!(result.stderr, "");
    Ok(())
}

#[tokio::test]
async fn delegation_message_encodes_request_and_renders_receipt() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE_REQUEST)?);
    let child_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD)?);
    let message_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE)?);
    let result = run_delegation_verb(
        &[
            "session",
            "message",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_MESSAGE_REQUEST,
            DELEGATION_CHILD,
            "--content",
            "status ready",
        ],
        ClientRequest::SendSessionMessage {
            session_id,
            turn_id,
            tool_request_id: request_id,
            peer_session_id: child_id,
            content: String::from("status ready"),
        },
        ServerMessage::SessionMessageSent {
            tool_request_id: request_id,
            message_id,
            direction: DelegationMessageDirection::ParentToChild,
            ordinal: CanonicalU64::new(2),
            delivery_sequence: CanonicalU64::new(1),
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::SUCCESS);
    assert_eq!(
        result.stdout,
        format!(
            "message_request={request_id} peer_session={child_id} message={message_id} direction=parent_to_child ordinal=2 delivery_sequence=1\n"
        )
    );
    assert_eq!(result.stderr, "");
    Ok(())
}

#[tokio::test]
async fn delegation_background_await_rejects_self_relationship() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id =
        CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_BACKGROUND_AWAIT_REQUEST)?);
    let result = run_delegation_verb(
        &[
            "session",
            "await",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_BACKGROUND_AWAIT_REQUEST,
            DELEGATION_SESSION,
            "--mode",
            "background",
        ],
        ClientRequest::AwaitSession {
            session_id,
            turn_id,
            tool_request_id: request_id,
            child_session_id: session_id,
            mode: DelegationWaitMode::Background,
        },
        ServerMessage::SessionAwaitRegistered {
            tool_request_id: request_id,
            child_session_id: session_id,
            mode: DelegationWaitMode::Background,
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::FAILURE);
    assert_eq!(result.stdout, "");
    assert!(!result.stderr.is_empty());
    Ok(())
}

#[tokio::test]
async fn delegation_foreground_await_rejects_self_relationship() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_AWAIT_REQUEST)?);
    let spawn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SPAWN_REQUEST)?);
    let child_turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_CHILD_TURN)?);
    let result = run_delegation_verb(
        &[
            "session",
            "await",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_AWAIT_REQUEST,
            DELEGATION_SESSION,
            "--mode",
            "foreground",
        ],
        ClientRequest::AwaitSession {
            session_id,
            turn_id,
            tool_request_id: request_id,
            child_session_id: session_id,
            mode: DelegationWaitMode::Foreground,
        },
        ServerMessage::ChildResult {
            await_request_id: request_id,
            spawning_request_id: spawn_id,
            child_session_id: session_id,
            outcome: DelegationOutcome::Returned,
            content: Some(String::from("done")),
            reason: DelegationReason::ChildCompleted,
            provenance: DelegationProvenance::ChildTurn {
                child_session_id: session_id,
                child_turn_id,
            },
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::FAILURE);
    assert_eq!(result.stdout, "");
    assert!(!result.stderr.is_empty());
    Ok(())
}

#[tokio::test]
async fn delegation_message_rejects_self_peer() -> Result<(), Box<dyn Error>> {
    let session_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_SESSION)?);
    let turn_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_TURN)?);
    let request_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE_REQUEST)?);
    let message_id = CanonicalUuid::from_uuid(Uuid::parse_str(DELEGATION_MESSAGE)?);
    let result = run_delegation_verb(
        &[
            "session",
            "message",
            DELEGATION_SESSION,
            DELEGATION_TURN,
            DELEGATION_MESSAGE_REQUEST,
            DELEGATION_SESSION,
            "--content",
            "status ready",
        ],
        ClientRequest::SendSessionMessage {
            session_id,
            turn_id,
            tool_request_id: request_id,
            peer_session_id: session_id,
            content: String::from("status ready"),
        },
        ServerMessage::SessionMessageSent {
            tool_request_id: request_id,
            message_id,
            direction: DelegationMessageDirection::ParentToChild,
            ordinal: CanonicalU64::new(2),
            delivery_sequence: CanonicalU64::new(1),
        },
    )
    .await?;

    assert_eq!(result.exit, ExitCode::FAILURE);
    assert_eq!(result.stdout, "");
    assert!(!result.stderr.is_empty());
    Ok(())
}

fn review_finding_items_response(
    request: &ClientFrame,
    run_id: CanonicalUuid,
    count: u64,
) -> Result<Vec<u8>, FrameEncodeError> {
    const FIRST_FINDING_IDENTITY: u128 = 10;

    let frame = |message| {
        ServerFrame::try_new_for_version(request.version(), request.request_id(), message)
    };
    let mut response = encode_server_line(&frame(ServerMessage::ReviewFindingsStart { run_id })?)?;
    for offset in 0..count {
        let finding_id =
            CanonicalUuid::from_uuid(Uuid::from_u128(FIRST_FINDING_IDENTITY + u128::from(offset)));
        let finding = ReviewFindingSnapshot {
            target_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
            run_id,
            producing_pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
            finding: ReviewFindingInput {
                finding_id,
                file_path: String::from("src/review.rs"),
                line_start: None,
                line_end: None,
                diff_side: None,
                title: String::from("Read the durable finding"),
                body: String::from("Admission policy does not invalidate stored findings."),
                severity: ReviewSeverity::High,
                is_real_confidence: CanonicalU64::new(9_000),
                severity_label_confidence: CanonicalU64::new(8_500),
                category: String::from("availability"),
                recommended_fix: None,
            },
            status: ReviewFindingStatus::Open,
            event_count: CanonicalU64::new(0),
        };
        response.extend_from_slice(&encode_server_line(&frame(
            ServerMessage::ReviewFindingItem { finding },
        )?)?);
    }
    Ok(response)
}

const REVIEW_FINDING_LIMIT_FIXTURE: u64 = 3;

fn review_run_snapshot(pass_id: Option<CanonicalUuid>) -> ReviewRunSnapshot {
    ReviewRunSnapshot {
        target_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        run_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        workflow: ReviewWorkflow::ReadOnlyReview,
        policy_version: CanonicalU64::new(1),
        minimum_judge_confidence: CanonicalU64::new(8_000),
        minimum_publication_confidence: CanonicalU64::new(9_000),
        state: ReviewRunLifecycle::Queued,
        pass_id,
    }
}

fn review_pass_snapshot() -> ReviewPassSnapshot {
    ReviewPassSnapshot {
        pass_id: CanonicalUuid::from_uuid(Uuid::from_u128(3)),
        run_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        target_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        kind: ReviewPassKind::ReadOnlyReview,
        session_id: CanonicalUuid::from_uuid(Uuid::from_u128(5)),
        accepted_input_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
        origin_turn_id: CanonicalUuid::from_uuid(Uuid::from_u128(7)),
        state: ReviewPassLifecycle::Queued,
        turn_id: None,
        output_frontier_id: None,
    }
}

#[tokio::test]
async fn credential_clear_rejects_a_receipt_for_another_generation() -> Result<(), Box<dyn Error>> {
    use signalbox_process_protocol::{CredentialExclusionClearOutcome, CredentialExclusionTarget};
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let target = CredentialExclusionTarget::ProfileQuarantine {
        profile: "home".into(),
        record_generation: CanonicalU64::new(1),
    };
    let request_target = target.clone();
    let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &ClientRequest::ClearCredentialExclusion {
                command_id,
                target: request_target,
            },
            ServerMessage::CredentialExclusionCleared {
                target: CredentialExclusionTarget::ProfileQuarantine {
                    profile: "home".into(),
                    record_generation: CanonicalU64::new(2),
                },
                outcome: CredentialExclusionClearOutcome::Cleared,
            },
        )
        .await
    });
    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = crate::credential::credential(
        &mut client,
        &mut Output::new(&mut stdout, &mut stderr, false),
        crate::arguments::CredentialCommand::Clear {
            target,
            command_id: Some(command_id),
        },
    )
    .await;
    assert!(result.is_err());
    assert!(
        stdout.is_empty(),
        "a mismatched receipt must not be presented as cleared"
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn program_cancellation_rejects_a_receipt_for_another_run() -> Result<(), Box<dyn Error>> {
    use signalbox_process_protocol::ProgramRunCancellationOutcome;
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &ClientRequest::CancelProgramRun { command_id, run_id },
            ServerMessage::ProgramRunCancellationReceipt {
                command_id,
                run_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
                outcome: ProgramRunCancellationOutcome::NotFound {},
            },
        )
        .await
    });
    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = crate::program::execute(
        &mut client,
        &mut Output::new(&mut stdout, &mut stderr, false),
        crate::arguments::ProgramCommand::Cancel {
            run_id,
            command_id: Some(command_id),
        },
    )
    .await;
    assert!(matches!(result, Err(ClientError::AmbiguousMutation)));
    assert!(stdout.is_empty());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn pool_projection_rejects_a_foreign_policy_read() -> Result<(), Box<dyn Error>> {
    use signalbox_process_protocol::{CredentialPoolExclusion, CredentialPoolMemberEvidence};
    let session_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let turn_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let pool_policy_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let policy_members = vec![String::from("excluded-member")];
    let members = vec![CredentialPoolMemberEvidence {
        profile: policy_members[0].clone(),
        reset_at_unix_ms: None,
        exclusion: CredentialPoolExclusion::ProfileQuarantine {
            record_generation: signalbox_process_protocol::CanonicalU64::new(0),
        },
    }];
    for response in [
        ServerMessage::CredentialPoolPolicy {
            pool_policy_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
            policy_members: policy_members.clone(),
        },
        ServerMessage::CredentialPoolPolicy {
            pool_policy_id,
            policy_members: vec![String::from("foreign-member")],
        },
    ] {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            accept_request_and_reply(
                &listener,
                &ClientRequest::ReadCredentialPoolPolicy {
                    session_id,
                    turn_id,
                    pool_policy_id,
                },
                response,
            )
            .await
        });
        let mut client = ProcessClient::new(socket);
        assert!(matches!(
            crate::credential_pool::validate(
                &mut client,
                session_id,
                turn_id,
                pool_policy_id,
                &policy_members,
                &members
            )
            .await,
            Err(ClientError::Protocol(_))
        ));
        server.await??;
    }
    Ok(())
}

#[tokio::test]
async fn reload_configuration_prints_the_correlated_installed_sections()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("reload.sock");
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let frame = decode_client_line(&line).map_err(io::Error::other)?;
        let ClientRequest::ReloadConfiguration { command_id } = frame.request() else {
            return Err(io::Error::other("expected reload request"));
        };
        let receipt = ServerFrame::try_new_for_version(
            frame.version(),
            frame.request_id(),
            ServerMessage::ConfigurationReloaded {
                command_id: *command_id,
                reloaded_sections: signalbox_process_protocol::ReloadedSection::ALL.to_vec(),
            },
        )
        .map_err(io::Error::other)?;
        writer
            .write_all(&encode_server_line(&receipt).map_err(io::Error::other)?)
            .await
    });
    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr, false);
    crate::session::reload_configuration(&mut client, &mut output, None).await?;
    assert_eq!(
        String::from_utf8(stdout)?,
        "reloaded model_catalog session_templates repo_watch\n"
    );
    assert!(String::from_utf8(stderr)?.starts_with("command_id="));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn reload_configuration_reuses_the_supplied_command_id() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("reload.sock");
    let listener = UnixListener::bind(&socket)?;
    let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
    let expected = ClientRequest::ReloadConfiguration { command_id };
    let receipt = ServerMessage::ConfigurationReloaded {
        command_id,
        reloaded_sections: signalbox_process_protocol::ReloadedSection::ALL.to_vec(),
    };
    let server =
        tokio::spawn(async move { accept_request_and_reply(&listener, &expected, receipt).await });
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let identity = command_id.into_uuid().hyphenated().to_string();
    let exit = run(
        client_arguments(
            &socket,
            &["reload-configuration", "--command-id", &identity],
        ),
        None,
        &mut input,
        &mut stdout,
        &mut stderr,
    )
    .await;
    server.await??;
    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(
        String::from_utf8(stdout)?,
        "reloaded model_catalog session_templates repo_watch\n"
    );
    assert_eq!(
        String::from_utf8(stderr)?,
        format!("command_id={identity}\n")
    );
    Ok(())
}

#[test]
fn credential_wait_terminal_release_finishes_follow_as_failed() {
    let state = TurnState::FailedAfterCredentialWait {
        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        predecessor_model_call:
            signalbox_process_protocol::FailedTerminalModelCall::known_failed_with_cause(
                CanonicalUuid::from_uuid(Uuid::from_u128(3)),
                signalbox_process_protocol::FailedModelCallCause::QuotaExhausted,
            ),
    };
    assert_eq!(
        terminal_snapshot_state(Some(&state)).expect("terminal release is readable"),
        Some(TurnTerminal::Failed)
    );
}

#[tokio::test]
async fn credential_wait_failure_event_requires_its_terminal_snapshot_frontier()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let session_id = CanonicalUuid::from_uuid(Uuid::from_u128(1));
    let turn_id = CanonicalUuid::from_uuid(Uuid::from_u128(2));
    let frontier = CanonicalUuid::from_uuid(Uuid::from_u128(3));
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, mut writer) = listener.accept().await?.0.into_split();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let request = decode_client_line(&line).map_err(io::Error::other)?;
            assert_eq!(
                request.request(),
                &ClientRequest::ReadTranscript { session_id }
            );
            for message in [
                ServerMessage::TranscriptSnapshotStart {
 workspace_root_kind: None, session_id, cursor: CanonicalU64::new(1), runner: None, repository_watch: None },
                ServerMessage::TranscriptTurn {
                    turn_id, acceptance_position: CanonicalU64::new(1), model_settings: None,
                    state: TurnState::FailedAfterCredentialWait {
                        terminal_frontier_id: frontier,
                        terminal_attempt_id: CanonicalUuid::from_uuid(Uuid::from_u128(4)),
                        predecessor_model_call: signalbox_process_protocol::FailedTerminalModelCall::known_failed_with_cause(
                            CanonicalUuid::from_uuid(Uuid::from_u128(5)), signalbox_process_protocol::FailedModelCallCause::QuotaExhausted,
                        ),
                    },
                },
                ServerMessage::TranscriptModelCallsEnd { model_call_count: CanonicalU64::new(0) },
                ServerMessage::TranscriptSnapshotEnd { session_id, cursor: CanonicalU64::new(1), turn_count: CanonicalU64::new(1), entry_count: CanonicalU64::new(0) },
            ] {
                let frame = ServerFrame::try_new_for_version(request.version(), request.request_id(), message).map_err(io::Error::other)?;
                writer.write_all(&encode_server_line(&frame).map_err(io::Error::other)?).await?;
            }
        }
        Ok::<_, io::Error>(())
    });
    let mut client = ProcessClient::new(socket);
    let event = SessionEvent::TurnFailed {
        turn_id,
        failure_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
        terminal_frontier_id: frontier,
    };
    crate::credential_pool::validate_event(&mut client, session_id, &event).await?;
    let foreign_frontier = SessionEvent::TurnFailed {
        turn_id,
        failure_entry_id: CanonicalUuid::from_uuid(Uuid::from_u128(6)),
        terminal_frontier_id: CanonicalUuid::from_uuid(Uuid::from_u128(7)),
    };
    assert!(matches!(
        crate::credential_pool::validate_event(&mut client, session_id, &foreign_frontier).await,
        Err(ClientError::Protocol(_))
    ));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn program_cancellation_presents_the_retained_successful_result() -> Result<(), Box<dyn Error>>
{
    use signalbox_process_protocol::{ProgramRunCancellationOutcome, ProgramRunTerminalState};
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let command_id = CommandId::try_from_uuid(Uuid::now_v7())?;
    let retained = vec![0, 255, 128];
    let outcome =
        ProgramRunCancellationOutcome::AlreadyTerminal(ProgramRunTerminalState::Succeeded {
            result: retained.clone(),
            result_extent: signalbox_process_protocol::ProgramByteExtent::Complete {},
        });
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &ClientRequest::CancelProgramRun { command_id, run_id },
            ServerMessage::ProgramRunCancellationReceipt {
                command_id,
                run_id,
                outcome,
            },
        )
        .await
    });
    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    crate::program::execute(
        &mut client,
        &mut Output::new(&mut stdout, &mut stderr, false),
        crate::arguments::ProgramCommand::Cancel {
            run_id,
            command_id: Some(command_id),
        },
    )
    .await?;
    let text = String::from_utf8(stdout)?;
    let (_, json) = text.split_once(' ').expect("run and receipt");
    let value: serde_json::Value = serde_json::from_str(json)?;
    assert_eq!(
        value,
        serde_json::json!({"kind":"already_terminal","terminal_state":"succeeded","result":retained,"result_extent":{"kind":"complete"}})
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn program_read_presents_retained_input_and_exact_result() -> Result<(), Box<dyn Error>> {
    use signalbox_process_protocol::{ProgramRun, ProgramRunState};
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let run = ProgramRun {
        registration_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
        input: vec![0, 255],
        input_extent: signalbox_process_protocol::ProgramByteExtent::Complete {},
        outcome: ProgramRunState::Succeeded {
            result: vec![128, 0],
            result_extent: signalbox_process_protocol::ProgramByteExtent::Complete {},
        },
    };
    let expected = serde_json::to_value(&run)?;
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &ClientRequest::ReadProgramRun { run_id },
            ServerMessage::ProgramRunRead { run_id, run },
        )
        .await
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    crate::program::execute(
        &mut ProcessClient::new(socket),
        &mut Output::new(&mut stdout, &mut stderr, false),
        crate::arguments::ProgramCommand::Read { run_id },
    )
    .await?;
    let text = String::from_utf8(stdout)?;
    let (identity, json) = text.split_once(' ').expect("run and retained state");
    assert_eq!(identity, format!("run={run_id}"));
    assert_eq!(serde_json::from_str::<serde_json::Value>(json)?, expected);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn program_read_presents_typed_truncation_markers() -> Result<(), Box<dyn Error>> {
    use signalbox_process_protocol::{ProgramRun, ProgramRunState};
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let run = ProgramRun {
        registration_id: CanonicalUuid::from_uuid(Uuid::now_v7()),
        input: vec![0, 255],
        input_extent: signalbox_process_protocol::ProgramByteExtent::Truncated {
            total_bytes: 5000000,
        },
        outcome: ProgramRunState::Succeeded {
            result: vec![128, 0],
            result_extent: signalbox_process_protocol::ProgramByteExtent::Truncated {
                total_bytes: 5000000,
            },
        },
    };
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &ClientRequest::ReadProgramRun { run_id },
            ServerMessage::ProgramRunRead { run_id, run },
        )
        .await
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    crate::program::execute(
        &mut ProcessClient::new(socket),
        &mut Output::new(&mut stdout, &mut stderr, false),
        crate::arguments::ProgramCommand::Read { run_id },
    )
    .await?;
    let text = String::from_utf8(stdout)?;
    let (identity, json) = text.split_once(' ').expect("run and retained state");
    assert_eq!(identity, format!("run={run_id}"));
    let value: serde_json::Value = serde_json::from_str(json)?;
    assert_eq!(
        value["input_extent"],
        serde_json::json!({"kind": "truncated", "total_bytes": 5000000})
    );
    assert_eq!(
        value["outcome"]["result_extent"],
        serde_json::json!({"kind": "truncated", "total_bytes": 5000000})
    );
    assert_eq!(value["outcome"]["result"], serde_json::json!([128, 0]));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn program_register_reads_the_executable_and_grants_from_json() -> Result<(), Box<dyn Error>>
{
    use signalbox_process_protocol::{
        ProgramExecutableInput, ProgramGrant, ProgramRegistrationInput,
    };
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let registration_file = directory.path().join("registration.json");
    std::fs::write(&registration_file, br#"{"name":"clock","revision":"1","executable":{"kind":"native","entry":"clock","revision":"1"},"grants":["time"]}"#)?;
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &ClientRequest::RegisterProgram {
                registration_id,
                registration: ProgramRegistrationInput {
                    name: "clock".into(),
                    revision: "1".into(),
                    executable: ProgramExecutableInput::Native {
                        entry: "clock".into(),
                        revision: "1".into(),
                    },
                    grants: vec![ProgramGrant::Time],
                },
            },
            ServerMessage::ProgramRegistered { registration_id },
        )
        .await
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    crate::program::execute(
        &mut ProcessClient::new(socket),
        &mut Output::new(&mut stdout, &mut stderr, false),
        crate::arguments::ProgramCommand::Register {
            registration_id,
            registration: registration_file,
        },
    )
    .await?;
    assert!(String::from_utf8(stderr)?.contains(&registration_id.to_string()));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn program_start_sends_exact_file_bytes_and_rejects_another_registration_receipt()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("client.sock");
    let listener = UnixListener::bind(&socket)?;
    let input = directory.path().join("input.bin");
    let bytes = vec![0, 255, 128];
    std::fs::write(&input, &bytes)?;
    let run_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let registration_id = CanonicalUuid::from_uuid(Uuid::now_v7());
    let other_registration = CanonicalUuid::from_uuid(Uuid::now_v7());
    let server = tokio::spawn(async move {
        accept_request_and_reply(
            &listener,
            &ClientRequest::StartProgramRun {
                run_id,
                registration_id,
                input: bytes,
            },
            ServerMessage::ProgramRunStarted {
                run_id,
                registration_id: other_registration,
            },
        )
        .await
    });
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = crate::program::execute(
        &mut ProcessClient::new(socket),
        &mut Output::new(&mut stdout, &mut stderr, false),
        crate::arguments::ProgramCommand::Start {
            run_id,
            registration_id,
            input,
        },
    )
    .await;
    assert!(
        matches!(result, Err(ClientError::AmbiguousMutation)),
        "an uncorrelated mutation receipt must require recovery"
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn operator_status_counts_and_displays_supervision_and_repository_ingestion()
-> Result<(), Box<dyn Error>> {
    use signalbox_process_protocol::{
        OperatorStatusEndMessage, OperatorStatusMessage, OperatorStatusRepositoryIngestion,
    };
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("status.sock");
    let listener = UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let request = decode_client_line(&line).map_err(io::Error::other)?;
        assert_eq!(request.request(), &ClientRequest::ReadOperatorStatus {});
        for message in [
            OperatorStatusMessage::Start {},
            OperatorStatusMessage::SessionSupervision(Box::new(signalbox_process_protocol::OperatorStatusSessionSupervisionMessage {
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(144)),
                terminal: true,
                failure_class: signalbox_process_protocol::OperatorStatusSupervisionFailureClass::Corruption,
                cause_code: String::from("durable_state_corruption"),
            })),
            OperatorStatusMessage::RepositoryIngestion(Box::new(
                OperatorStatusRepositoryIngestion {
                    repository: "evidence/project".to_owned(),
                    last_successful_observation: None,
                    last_poll: None,
                    last_accepted_webhook: None,
                    events_recorded: CanonicalU64::new(7),
                },
            )),
            OperatorStatusMessage::End(Box::new(OperatorStatusEndMessage {
                session_supervision_count: CanonicalU64::new(1),
                repository_ingestion_count: CanonicalU64::new(1),
                lifecycle_week_count: CanonicalU64::new(0),
                lifecycle_deadline_violation_count: CanonicalU64::new(0),
            })),
        ] {
            let frame = ServerFrame::try_new_for_version(
                request.version(),
                request.request_id(),
                ServerMessage::OperatorStatus(Box::new(message)),
            )
            .map_err(io::Error::other)?;
            writer
                .write_all(&encode_server_line(&frame).map_err(io::Error::other)?)
                .await?;
        }
        Ok::<_, io::Error>(())
    });
    let mut client = ProcessClient::new(socket);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    crate::follow_status::status(
        &mut client,
        &mut Output::new(&mut stdout, &mut stderr, false),
    )
    .await?;
    server.await??;
    let rendered = String::from_utf8(stdout)?;
    let evidence = rendered
        .lines()
        .find_map(|line| line.strip_prefix("repository_ingestion "))
        .expect("ingestion row is displayed");
    let evidence: serde_json::Value = serde_json::from_str(evidence)?;
    assert_eq!(evidence["repository"], "evidence/project");
    assert_eq!(evidence["events_recorded"], "7");
    assert!(evidence["last_successful_observation"].is_null());
    let supervision = rendered
        .lines()
        .find_map(|line| line.strip_prefix("session_supervision "))
        .expect("terminal operator item is displayed");
    let supervision: serde_json::Value = serde_json::from_str(supervision)?;
    assert_eq!(supervision["terminal"], true);
    assert_eq!(supervision["failure_class"], "corruption");
    assert_eq!(supervision["cause_code"], "durable_state_corruption");
    assert!(rendered.contains("session_supervision=1"));
    Ok(())
}

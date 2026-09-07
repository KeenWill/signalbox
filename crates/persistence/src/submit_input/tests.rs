use std::io;

use super::*;

#[test]
fn turn_origin_dependency_order_handles_reverse_key_chains() {
    let session = Uuid::from_u128(1);
    let chain = (1..=512)
        .rev()
        .map(|turn| (session, Uuid::from_u128(turn)))
        .collect::<Vec<_>>();
    let relationships = chain
        .iter()
        .enumerate()
        .map(|(index, turn)| (*turn, index.checked_sub(1).map(|prior| chain[prior])))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        turn_origin_dependency_order(
            relationships
                .iter()
                .map(|(turn, predecessor)| (*turn, *predecessor)),
        ),
        Some(chain),
    );
}

#[test]
fn turn_origin_dependency_order_rejects_cycles() {
    let session = Uuid::from_u128(1);
    let first = (session, Uuid::from_u128(1));
    let second = (session, Uuid::from_u128(2));

    assert_eq!(
        turn_origin_dependency_order([(first, Some(second)), (second, Some(first))]),
        None,
    );
}

#[test]
fn lost_commit_response_is_typed_as_ambiguous() {
    let error = SubmitInputRepositoryError::from_commit_failure(sqlx::Error::Io(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "commit response was lost",
    )));

    assert!(matches!(
        error,
        SubmitInputRepositoryError::CommitAmbiguous(_)
    ));
}

#[test]
fn imported_conversation_database_failure_remains_retryable() {
    let error = map_imported_scheduling_error(
        crate::create_session_from_imported_frontier::ImportedSessionRepositoryError::ImportedConversation(
            crate::conversation_import::ImportedConversationRepositoryError::Database(
                sqlx::Error::PoolTimedOut,
            ),
        ),
    );

    assert!(matches!(
        error,
        SubmitInputRepositoryError::Database(sqlx::Error::PoolTimedOut)
    ));
}

#[test]
fn delegation_child_result_decoder_restores_exact_typed_outcome() {
    let child = Uuid::from_u128(0xd101);
    let turn = Uuid::from_u128(0xd102);
    let content = DelegationContent::try_new(String::from("checked result"))
        .expect("fixture content is valid");
    let outcome = DelegationOutcome::reconstitute(
        decode_delegation_outcome_kind("result_returned")
            .expect("fixture outcome kind is supported"),
        Some(content.clone()),
        decode_delegation_outcome_reason("child_completed").expect("fixture reason is supported"),
        decode_delegation_provenance("child_turn", child, Some(turn), None, None)
            .expect("fixture provenance is complete"),
    )
    .expect("fixture outcome is internally consistent");

    assert_eq!(outcome.kind(), DelegationOutcomeKind::ResultReturned);
    assert_eq!(outcome.content(), Some(&content));
    assert_eq!(
        outcome.reconstitution_provenance(),
        DelegationProvenanceReconstitutionInput::ChildTurn {
            session: session_id_from_uuid(child),
            turn: turn_id_from_uuid(turn),
        }
    );
}

#[test]
fn delegation_parent_result_decoder_restores_command_provenance() {
    let parent = Uuid::from_u128(0xd111);
    let turn = Uuid::from_u128(0xd112);
    let command = Uuid::from_u128(0xd113);
    let outcome = DelegationOutcome::reconstitute(
        decode_delegation_outcome_kind("continue_running")
            .expect("fixture outcome kind is supported"),
        None,
        decode_delegation_outcome_reason("parent_stopped_parent_and_descendants")
            .expect("fixture reason is supported"),
        decode_delegation_provenance(
            "parent_turn_command",
            parent,
            Some(turn),
            None,
            Some(command),
        )
        .expect("fixture provenance is complete"),
    )
    .expect("fixture outcome is internally consistent");

    assert_eq!(outcome.kind(), DelegationOutcomeKind::ContinueRunning);
    assert_eq!(outcome.content(), None);
    assert_eq!(
        outcome.reconstitution_provenance(),
        DelegationProvenanceReconstitutionInput::ParentTurnCommand {
            session: session_id_from_uuid(parent),
            turn: turn_id_from_uuid(turn),
            command: durable_command_id_from_uuid(command)
                .expect("fixture command identity is valid"),
        }
    );
}

#[test]
fn delegation_result_decoder_rejects_incomplete_parent_provenance() {
    let error = decode_delegation_provenance(
        "parent_goal_command",
        Uuid::from_u128(0xd121),
        None,
        None,
        Some(Uuid::from_u128(0xd122)),
    )
    .expect_err("parent goal provenance requires its generation");

    assert_eq!(
        error,
        SubmitInputCorruption::Inconsistent("delegation result provenance")
    );
}

#[test]
fn unsupported_model_setting_remains_a_caller_facing_repository_error() {
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(0x51));
    let unsupported = UnsupportedModelSetting::FastMode { selection };

    let error =
        map_model_settings_resolution_error(OriginModelSettingsError::Unsupported(unsupported));

    assert_unsupported_model_setting(error, &unsupported);
}

#[track_caller]
fn assert_unsupported_model_setting(
    error: SubmitInputRepositoryError,
    expected: &UnsupportedModelSetting,
) {
    let SubmitInputRepositoryError::UnsupportedModelSetting(actual) = error else {
        panic!("expected unsupported model setting, got {error}");
    };
    assert_eq!(&actual, expected);
}

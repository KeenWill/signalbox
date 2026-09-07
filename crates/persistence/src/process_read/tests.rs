use rust_decimal::Decimal;
use signalbox_domain::{SessionId, ToolRequestId, TurnId};
use sqlx::types::Uuid;

use super::{
    DecodedTurnOrigin, ProcessModelCallInputTokenSemantics, ProcessModelCallUsageProvenance,
    ProcessReadCorruption, decode_execution_lineage_tip, decode_tool_result_disposition,
    decode_transcript_turn_origin,
};

fn turn(value: u128) -> TurnId {
    TurnId::from_uuid(Uuid::from_u128(value))
}

/// acceptance order A, B, C may execute as A, C, B; the database lineage diagnostic selects B
/// as the one complete-chain tip.
#[test]
fn latest_tip_follows_execution_lineage() {
    let second = turn(2);

    assert_eq!(
        decode_execution_lineage_tip(3, 1, 3, 1, false, false, Some(second))
            .expect("the lineage is one complete chain"),
        Some(second)
    );
}

/// a branched persisted execution lineage cannot choose one
/// authoritative snapshot frontier and therefore fails closed.
#[test]
fn latest_frontier_rejects_branched_execution_lineage() {
    assert!(decode_execution_lineage_tip(3, 1, 3, 2, true, false, Some(turn(2))).is_err());
}

#[test]
fn delegated_transcript_origin_retains_exact_spawn_provenance() {
    let current_turn = turn(1);
    let spawning_request = Uuid::from_u128(2);
    let parent_session = Uuid::from_u128(3);
    let parent_turn = Uuid::from_u128(4);
    let content = String::from("delegated task");
    let decoded = decode_transcript_turn_origin(
        String::from("delegation"),
        None,
        None,
        None,
        None,
        None,
        Some(spawning_request),
        Some(parent_session),
        Some(parent_turn),
        Some(content.clone()),
        None,
        None,
        current_turn,
        1,
    )
    .expect("a complete delegated task origin is readable");
    let DecodedTurnOrigin::DelegatedTask {
        spawning_request: decoded_request,
        parent_session: decoded_session,
        parent_turn: decoded_turn,
        content: decoded_content,
    } = decoded
    else {
        panic!("the delegated fixture retains its origin family")
    };
    assert_eq!(decoded_request, ToolRequestId::from_uuid(spawning_request));
    assert_eq!(decoded_session, SessionId::from_uuid(parent_session));
    assert_eq!(decoded_turn, TurnId::from_uuid(parent_turn));
    assert_eq!(decoded_content, content);
}

#[test]
fn delegated_transcript_origin_rejects_missing_spawn_provenance() {
    let error = decode_transcript_turn_origin(
        String::from("delegation"),
        None,
        None,
        None,
        None,
        None,
        None,
        Some(Uuid::from_u128(3)),
        Some(Uuid::from_u128(4)),
        Some(String::from("delegated task")),
        None,
        None,
        turn(1),
        1,
    )
    .expect_err("delegated origin provenance is all-or-nothing");
    assert!(error.to_string().contains("turn origin correlation"));
}

#[test]
fn delegation_wake_origin_retains_exact_delivery_range() {
    let decoded = decode_transcript_turn_origin(
        String::from("delegation"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(Decimal::from(2)),
        Some(Decimal::from(4)),
        turn(1),
        2,
    )
    .expect("a complete delegation wake origin is readable");
    let DecodedTurnOrigin::DelegationWake {
        first_delivery_sequence,
        through_delivery_sequence,
    } = decoded
    else {
        panic!("the wake fixture retains its origin family")
    };
    assert_eq!(first_delivery_sequence, 2);
    assert_eq!(through_delivery_sequence, 4);
}

#[test]
fn delegation_wake_origin_rejects_reversed_delivery_range() {
    let error = decode_transcript_turn_origin(
        String::from("delegation"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(Decimal::from(4)),
        Some(Decimal::from(2)),
        turn(1),
        2,
    )
    .expect_err("a delegation wake range cannot run backward");
    assert!(error.to_string().contains("delegation wake delivery range"));
}

#[test]
fn model_call_usage_provenance_storage_mapping_is_closed() {
    assert_eq!(
        ProcessModelCallUsageProvenance::from_storage("reported"),
        Some(ProcessModelCallUsageProvenance::Reported)
    );
    assert_eq!(
        ProcessModelCallUsageProvenance::from_storage("estimated"),
        Some(ProcessModelCallUsageProvenance::Estimated)
    );
    assert_eq!(
        ProcessModelCallUsageProvenance::from_storage("inferred"),
        None
    );
}

#[test]
fn historical_model_call_input_semantics_remain_unknown() {
    assert_eq!(
        ProcessModelCallInputTokenSemantics::from_storage(None),
        None
    );
    assert_eq!(
        ProcessModelCallInputTokenSemantics::from_storage(Some(false)),
        Some(ProcessModelCallInputTokenSemantics::CacheExclusive)
    );
    assert_eq!(
        ProcessModelCallInputTokenSemantics::from_storage(Some(true)),
        Some(ProcessModelCallInputTokenSemantics::CacheInclusive)
    );
}

#[test]
fn tool_result_disposition_preserves_an_unsupported_spelling() {
    let unsupported = String::from("synthetic_future_disposition");

    assert_eq!(
        decode_tool_result_disposition(&unsupported),
        Err(ProcessReadCorruption::Unsupported {
            field: "terminal_disposition_kind",
            value: unsupported,
        })
    );
}

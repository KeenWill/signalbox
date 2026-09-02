//! PostgreSQL integration proof for lifecycle write-time stamps and the
//! mandatory typed terminal cause.

use std::collections::{BTreeMap, BTreeSet};

use crate::*;
use signalbox_persistence::{
    context_compaction::{
        ContextCompactionRepository, PrepareContextCompactionOutcome,
        PrepareContextCompactionRequest,
    },
    mapping::turn_terminal_cause_to_str,
};
use sqlx::types::time::OffsetDateTime;

/// One column's durable stamping contract: whether a row may omit it, and the
/// expression that fills it when a writer does not.
type StampingContract = (String, Option<String>);

async fn stamping_contracts(
    pool: &PgPool,
    columns: &[(&str, &str)],
) -> Result<BTreeMap<String, StampingContract>, sqlx::Error> {
    let tables: Vec<String> = columns.iter().map(|(table, _)| (*table).into()).collect();
    let names: Vec<String> = columns.iter().map(|(_, column)| (*column).into()).collect();
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT table_name, column_name, is_nullable, column_default
           FROM information_schema.columns
           JOIN unnest($1::text[], $2::text[]) AS wanted(table_name, column_name)
             USING (table_name, column_name)
          WHERE table_schema = 'public'",
    )
    .bind(&tables)
    .bind(&names)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(table, column, nullable, default)| {
            (format!("{table}.{column}"), (nullable, default))
        })
        .collect())
}

fn required_and_defaulted(columns: &[(&str, &str)]) -> BTreeMap<String, StampingContract> {
    columns
        .iter()
        .map(|(table, column)| {
            (
                format!("{table}.{column}"),
                (String::from("NO"), Some(String::from("clock_timestamp()"))),
            )
        })
        .collect()
}

/// Terminalizes one active turn by raw statement with an exact cause value,
/// supplying the complete failed-terminal payload so the row satisfies every
/// other constraint and only the cause rules can reject it.
async fn terminalize_with_cause(
    pool: &PgPool,
    turn: TurnId,
    cause: Option<&str>,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = starting_frontier_id,
                terminal_disposition_kind = 'failed',
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                recovery_model_call_id = NULL,
                active_tool_round_call_id = NULL,
                approval_tool_request_id = NULL,
                recovery_tool_attempt_id = NULL,
                terminal_attempt_id = NULL,
                terminal_model_call_id = NULL,
                terminal_tool_attempt_id = NULL,
                terminal_cause_kind = $2
          WHERE turn_id = $1",
    )
    .bind(turn.into_uuid())
    .bind(cause)
    .execute(pool)
    .await
}

/// Every spelling the closed database constraint admits, read back from the
/// constraint itself rather than restated by the reader.
async fn admitted_cause_spellings(pool: &PgPool) -> Result<BTreeSet<String>, sqlx::Error> {
    let spellings: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT spelling.captures[1]
           FROM pg_constraint AS closure
           CROSS JOIN LATERAL regexp_matches(
                    pg_get_constraintdef(closure.oid),
                    $$'([a-z_]+)'::text$$,
                    'g'
                ) AS spelling(captures)
          WHERE closure.conname = 'turn_lifecycle_terminal_cause_closed'",
    )
    .fetch_all(pool)
    .await?;
    Ok(spellings.into_iter().collect())
}

/// Every variant the domain vocabulary carries, encoded for storage.
fn encoded_cause_spellings() -> BTreeSet<String> {
    EVERY_TERMINAL_CAUSE
        .into_iter()
        .map(|cause| String::from(turn_terminal_cause_to_str(cause)))
        .collect()
}

const EVERY_TERMINAL_CAUSE: [TurnTerminalCause; 20] = [
    TurnTerminalCause::Completed,
    TurnTerminalCause::ModelRefusal,
    TurnTerminalCause::InterruptApplied,
    TurnTerminalCause::ModelCallAmbiguous,
    TurnTerminalCause::ToolAttemptAmbiguous,
    TurnTerminalCause::ModelCallFailed,
    TurnTerminalCause::ModelTargetUnavailable,
    TurnTerminalCause::AttachmentPreparationFailed,
    TurnTerminalCause::CapabilityPreparationFailed,
    TurnTerminalCause::ToolRoundLimitReached,
    TurnTerminalCause::ToolAttemptLost,
    TurnTerminalCause::CredentialPoolExhausted,
    TurnTerminalCause::HeadlessApprovalEscalation,
    TurnTerminalCause::AbandonedAtRestart,
    TurnTerminalCause::ContextHeadroomExhausted,
    TurnTerminalCause::ContextCompactionWall,
    TurnTerminalCause::ContextCompactionFailed,
    TurnTerminalCause::ReportedUsageContextCompactionExhausted,
    TurnTerminalCause::ReportedUsageContextStillExceeded,
    TurnTerminalCause::UnclassifiedFailure,
];

/// Every insert-time lifecycle stamp is required of the row and filled by the
/// column itself, so no present or future write path can commit an unstamped
/// lifecycle row.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn insert_time_lifecycle_stamps_are_required_and_column_filled() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let stamped = [
        ("outbox_event", "recorded_at"),
        ("delegation_outbox_event", "recorded_at"),
        ("session", "created_at"),
        ("turn_lifecycle", "recorded_at"),
        ("turn_attempt", "recorded_at"),
        ("model_call", "recorded_at"),
        ("tool_attempt", "recorded_at"),
        ("goal_event", "recorded_at"),
        ("compact_session_command", "requested_at"),
        ("context_compaction_model_call", "prepared_at"),
        ("context_compaction", "applied_at"),
    ];

    let observed = stamping_contracts(&pool, &stamped).await?;

    assert_eq!(observed, required_and_defaulted(&stamped));
    pool.close().await;
    drop(container);
    Ok(())
}

/// A committed session, turn, attempt, model call, and outbox event each carry
/// the instant their own row was written.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn committed_lifecycle_rows_carry_their_own_write_time() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let opened: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await?;

    let fixture = checkpoint_restart_model_call(&pool, 0x71a0, true).await?;

    let closed: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await?;
    let session_created: OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM session WHERE session_id = $1")
            .bind(fixture.session.into_uuid())
            .fetch_one(&pool)
            .await?;
    let turn_recorded: OffsetDateTime =
        sqlx::query_scalar("SELECT recorded_at FROM turn_lifecycle WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .fetch_one(&pool)
            .await?;
    let attempt_recorded: OffsetDateTime =
        sqlx::query_scalar("SELECT recorded_at FROM turn_attempt WHERE turn_attempt_id = $1")
            .bind(fixture.attempt.into_uuid())
            .fetch_one(&pool)
            .await?;
    let call_recorded: OffsetDateTime =
        sqlx::query_scalar("SELECT recorded_at FROM model_call WHERE model_call_id = $1")
            .bind(fixture.call.into_uuid())
            .fetch_one(&pool)
            .await?;
    let earliest_event: OffsetDateTime =
        sqlx::query_scalar("SELECT min(recorded_at) FROM outbox_event WHERE session_id = $1")
            .bind(fixture.session.into_uuid())
            .fetch_one(&pool)
            .await?;

    assert!(
        (opened..=closed).contains(&session_created),
        "session creation stamp {session_created} is outside the fixture window {opened}..={closed}"
    );
    assert!(
        (session_created..=closed).contains(&turn_recorded),
        "turn stamp {turn_recorded} does not follow its session's creation {session_created}"
    );
    assert!(
        (turn_recorded..=closed).contains(&attempt_recorded),
        "attempt stamp {attempt_recorded} does not follow its turn {turn_recorded}"
    );
    assert!(
        (attempt_recorded..=closed).contains(&call_recorded),
        "call stamp {call_recorded} does not follow its attempt {attempt_recorded}"
    );
    assert!(
        (opened..=closed).contains(&earliest_event),
        "outbox stamp {earliest_event} is outside the fixture window {opened}..={closed}"
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// The compaction command's acceptance time is its own durable column, not the
/// operational `durable_command.claimed_at` metadata, and each call transition
/// stamps when it happened.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn compaction_stamps_its_request_and_every_call_transition() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x71b0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let assistant = AssistantText::try_new(String::from("context before compaction"))
        .expect("fixture assistant text is admitted");
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![assistant],
        });
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                vec![SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    seed + 0x20,
                ))],
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x21)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x22)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let command = DurableCommandId::from_uuid(Uuid::from_u128(seed + 0x30));
    let call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0x31));
    let compaction = ContextCompactionId::from_uuid(Uuid::from_u128(seed + 0x32));
    let compaction_repository = ContextCompactionRepository::new(pool.clone());
    let prepared = compaction_repository
        .prepare(PrepareContextCompactionRequest {
            command,
            session: fixture.session,
            requested_through_position: Some(1),
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                Uuid::from_u128(seed + 6),
            )),
            input_includes_cache_tokens: true,
            credential_reference: String::from("compaction stamp fixture credential"),
            call,
            compaction,
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x33)),
            result_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x34)),
        })
        .await?;
    let PrepareContextCompactionOutcome::Prepared(prepared) = prepared else {
        panic!("the completed turn has a compactable frontier")
    };
    compaction_repository.authorize(&prepared).await?;
    compaction_repository
        .complete(
            &prepared,
            "retained context summary",
            ContextCompactionTokenUsage::unreported().with_input_tokens(Some(91)),
        )
        .await?;

    let (claimed_at, requested_at): (OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "SELECT durable_command.claimed_at, compact_session_command.requested_at
           FROM compact_session_command
           JOIN durable_command USING (command_id)
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_one(&pool)
    .await?;
    let (prepared_at, in_flight_at, terminal_at): (
        OffsetDateTime,
        Option<OffsetDateTime>,
        Option<OffsetDateTime>,
    ) = sqlx::query_as(
        "SELECT prepared_at, in_flight_at, terminal_at
           FROM context_compaction_model_call
          WHERE model_call_id = $1",
    )
    .bind(call.into_uuid())
    .fetch_one(&pool)
    .await?;
    let applied_at: OffsetDateTime = sqlx::query_scalar(
        "SELECT applied_at FROM context_compaction WHERE context_compaction_id = $1",
    )
    .bind(compaction.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert!(
        claimed_at < requested_at,
        "requested_at {requested_at} is not a stamp of its own: claim time is {claimed_at}"
    );
    assert!(
        prepared_at < in_flight_at.expect("an authorized compaction call went in flight"),
        "in-flight stamp does not follow the prepared stamp {prepared_at}"
    );
    assert!(
        in_flight_at < terminal_at,
        "terminal stamp does not follow the in-flight stamp"
    );
    assert!(
        terminal_at.expect("a completed compaction call is terminal") <= applied_at,
        "the application row predates the call that produced it"
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// A model call that ends failed classifies its turn's terminalization as the
/// call's own failure rather than leaving the cause unstated.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_failed_model_call_classifies_its_turn_terminalization() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x71c0;
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(&pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::KnownFailed);
    repository
        .apply_terminal_observation(
            fixture.session,
            observation,
            ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x20)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x21)),
            )),
            |_| panic!("the fixture has no pending steering to reclassify"),
        )
        .await?;

    let (disposition, cause): (String, String) = sqlx::query_as(
        "SELECT terminal_disposition_kind, terminal_cause_kind
           FROM turn_lifecycle
          WHERE turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(disposition, "failed");
    assert_eq!(
        cause,
        turn_terminal_cause_to_str(TurnTerminalCause::ModelCallFailed)
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// A turn cannot reach `terminal` without naming a cause.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_terminal_turn_without_a_cause_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x71d0, true).await?;

    let rejection = terminalize_with_cause(&pool, fixture.turn, None)
        .await
        .expect_err("a causeless terminalization is rejected");

    assert_eq!(
        rejection
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("turn_lifecycle_terminal_cause_required")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// A turn that has not reached `terminal` cannot carry a terminal cause, so the
/// column never states a reason for an ending that has not happened.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_nonterminal_turn_carrying_a_cause_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x71e0, true).await?;

    let rejection =
        sqlx::query("UPDATE turn_lifecycle SET terminal_cause_kind = $2 WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .bind(turn_terminal_cause_to_str(TurnTerminalCause::Completed))
            .execute(&pool)
            .await
            .expect_err("an active turn cannot state why it ended");

    assert_eq!(
        rejection
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("turn_lifecycle_terminal_cause_required")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// The stored cause vocabulary is closed, so a spelling the encoder cannot
/// produce cannot reach a durable row either.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_cause_spelling_outside_the_vocabulary_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x71f0, true).await?;

    let rejection = terminalize_with_cause(&pool, fixture.turn, Some("outside-closed-set"))
        .await
        .expect_err("an unknown cause spelling is rejected");

    assert_eq!(
        rejection
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("turn_lifecycle_terminal_cause_closed")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// The database's closed vocabulary and the encoder's vocabulary are the same
/// set, so a variant added to one cannot silently miss the other.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_stored_cause_vocabulary_matches_the_encoder() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;

    let admitted = admitted_cause_spellings(&pool).await?;

    assert_eq!(admitted, encoded_cause_spellings());
    pool.close().await;
    drop(container);
    Ok(())
}

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

/// One column's durable stamping contract: whether a row may omit it, and
/// whether the column itself fills it when a writer does not.
type StampingContract = (String, bool);

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
            (format!("{table}.{column}"), (nullable, default.is_some()))
        })
        .collect())
}

fn required_and_defaulted(columns: &[(&str, &str)]) -> BTreeMap<String, StampingContract> {
    columns
        .iter()
        .map(|(table, column)| (format!("{table}.{column}"), (String::from("NO"), true)))
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

/// Proves `EVERY_TERMINAL_CAUSE` still lists the whole vocabulary.
///
/// The match is exhaustive, so a variant added to `TurnTerminalCause` without
/// being added to the array above stops this file compiling. Without it the
/// comparison below would keep passing against the old subset while the new
/// cause was absent from storage — a tripwire that cannot trip.
const fn listed_position(cause: TurnTerminalCause) -> usize {
    match cause {
        TurnTerminalCause::Completed => 0,
        TurnTerminalCause::ModelRefusal => 1,
        TurnTerminalCause::InterruptApplied => 2,
        TurnTerminalCause::ModelCallAmbiguous => 3,
        TurnTerminalCause::ToolAttemptAmbiguous => 4,
        TurnTerminalCause::ModelCallFailed => 5,
        TurnTerminalCause::ModelTargetUnavailable => 6,
        TurnTerminalCause::AttachmentPreparationFailed => 7,
        TurnTerminalCause::CapabilityPreparationFailed => 8,
        TurnTerminalCause::ToolRoundLimitReached => 9,
        TurnTerminalCause::ToolAttemptLost => 10,
        TurnTerminalCause::CredentialPoolExhausted => 11,
        TurnTerminalCause::HeadlessApprovalEscalation => 12,
        TurnTerminalCause::AbandonedAtRestart => 13,
        TurnTerminalCause::WatchdogStaleTurn => 14,
        TurnTerminalCause::ContextHeadroomExhausted => 15,
        TurnTerminalCause::ContextCompactionWall => 16,
        TurnTerminalCause::ContextCompactionFailed => 17,
        TurnTerminalCause::ReportedUsageContextCompactionExhausted => 18,
        TurnTerminalCause::ReportedUsageContextStillExceeded => 19,
        TurnTerminalCause::UnclassifiedFailure => 20,
        TurnTerminalCause::GoalTurnIneligible => 21,
        TurnTerminalCause::SessionClosed => 22,
    }
}

/// Each listed variant's own position, so a variant given a fresh position by
/// the match above but never added to the array shows up as a gap.
fn listed_positions() -> Vec<usize> {
    EVERY_TERMINAL_CAUSE
        .into_iter()
        .map(listed_position)
        .collect()
}

/// Every variant the domain vocabulary carries, encoded for storage.
fn encoded_cause_spellings() -> BTreeSet<String> {
    EVERY_TERMINAL_CAUSE
        .into_iter()
        .map(|cause| String::from(turn_terminal_cause_to_str(cause)))
        .collect()
}

const EVERY_TERMINAL_CAUSE: [TurnTerminalCause; 23] = [
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
    TurnTerminalCause::WatchdogStaleTurn,
    TurnTerminalCause::ContextHeadroomExhausted,
    TurnTerminalCause::ContextCompactionWall,
    TurnTerminalCause::ContextCompactionFailed,
    TurnTerminalCause::ReportedUsageContextCompactionExhausted,
    TurnTerminalCause::ReportedUsageContextStillExceeded,
    TurnTerminalCause::UnclassifiedFailure,
    TurnTerminalCause::GoalTurnIneligible,
    TurnTerminalCause::SessionClosed,
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

/// One authorized compaction call and the identities its stamps hang off.
struct AuthorizedCompaction {
    repository: ContextCompactionRepository,
    prepared: Box<signalbox_persistence::context_compaction::PreparedContextCompaction>,
    command: DurableCommandId,
    call: ModelCallId,
    compaction: ContextCompactionId,
}

/// Compacts one session as far as `in_flight`: a completed turn gives the
/// session a compactable frontier, the command prepares the call, and
/// authorization moves it in flight.
async fn authorized_compaction_call(
    pool: &PgPool,
    seed: u128,
) -> Result<AuthorizedCompaction, Box<dyn Error>> {
    let (fixture, repository, authorized) = authorize_checkpointed_model_call(pool, seed).await?;
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
    Ok(AuthorizedCompaction {
        repository: compaction_repository,
        prepared,
        command,
        call,
        compaction,
    })
}

/// The compaction command's acceptance time is its own durable column, not the
/// operational `durable_command.claimed_at` metadata, and each call transition
/// stamps when it happened.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn compaction_stamps_its_request_and_every_call_transition() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let compaction = authorized_compaction_call(&pool, 0x71b0).await?;
    compaction
        .repository
        .complete(
            &compaction.prepared,
            "retained context summary",
            ContextCompactionTokenUsage::unreported().with_input_tokens(Some(91)),
        )
        .await?;
    let command = compaction.command;
    let call = compaction.call;

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
    .bind(compaction.compaction.into_uuid())
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

    // Both cause rules reject this row, and which one PostgreSQL reports is
    // not specified, so the assertion names neither in particular.
    let constraint = rejection
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint);
    assert!(
        matches!(
            constraint,
            Some(
                "turn_lifecycle_terminal_cause_required"
                    | "turn_lifecycle_terminal_cause_matches_disposition"
            )
        ),
        "rejected by {constraint:?}, which is not a terminal-cause rule"
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

    assert_eq!(
        listed_positions(),
        (0..EVERY_TERMINAL_CAUSE.len()).collect::<Vec<_>>()
    );
    assert_eq!(admitted, encoded_cause_spellings());
    pool.close().await;
    drop(container);
    Ok(())
}

/// A lifecycle row's write time does not move, so no later transition can
/// rewrite the instant every duration and queue wait is measured from.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_lifecycle_row_write_time_cannot_be_moved() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x7200, true).await?;

    let rejection =
        sqlx::query("UPDATE turn_lifecycle SET recorded_at = clock_timestamp() WHERE turn_id = $1")
            .bind(fixture.turn.into_uuid())
            .execute(&pool)
            .await
            .expect_err("a lifecycle write time cannot be restamped");

    assert_eq!(
        rejection
            .as_database_error()
            .map(sqlx::error::DatabaseError::message),
        Some("lifecycle row write time is immutable")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// Terminalizing a compaction call cannot erase the in-flight stamp it already
/// recorded, so the funnel interval survives the transition that ends it. The
/// state constraint alone admits this update: once the row is terminal its
/// `in_flight` clause is vacuous.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminalizing_a_compaction_call_cannot_erase_its_in_flight_stamp()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let compaction = authorized_compaction_call(&pool, 0x7210).await?;

    let rejection = sqlx::query(
        "UPDATE context_compaction_model_call
            SET state_kind = 'terminal',
                terminal_at = clock_timestamp(),
                terminal_disposition_kind = 'known_failed',
                in_flight_at = NULL
          WHERE model_call_id = $1",
    )
    .bind(compaction.call.into_uuid())
    .execute(&pool)
    .await
    .expect_err("terminalizing cannot erase the in-flight stamp");

    assert_eq!(
        rejection
            .as_database_error()
            .map(sqlx::error::DatabaseError::message),
        Some("compaction call in-flight time is write-once")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// A compaction call is inserted carrying only its preparation time, so no
/// writer can fabricate an authorization transition that never happened.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_compaction_call_cannot_be_inserted_already_in_flight() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x7220;
    let fixture = checkpoint_restart_model_call(&pool, seed, true).await?;

    let rejection = sqlx::query(
        "INSERT INTO context_compaction_model_call
            (model_call_id, session_id, direct_model_selection_id,
             resolved_provider_model_identity_id, source_frontier_id,
             credential_reference, state_kind, in_flight_at)
         VALUES ($1, $2, $3, $4, $5, 'fabricated', 'prepared', statement_timestamp())",
    )
    .bind(Uuid::from_u128(seed + 0x90))
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 5))
    .bind(Uuid::from_u128(seed + 6))
    .bind(Uuid::from_u128(seed + 11))
    .execute(&pool)
    .await
    .expect_err("a prepared compaction call cannot carry an in-flight stamp");

    assert_eq!(
        rejection
            .as_database_error()
            .map(sqlx::error::DatabaseError::message),
        Some("compaction call begins with only its preparation time")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// A turn cannot say two contradictory things about how it ended: the cause a
/// terminal turn records has to be one its own disposition admits.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_cause_its_disposition_does_not_admit_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x7230, true).await?;

    let rejection = terminalize_with_cause(
        &pool,
        fixture.turn,
        Some(turn_terminal_cause_to_str(TurnTerminalCause::Completed)),
    )
    .await
    .expect_err("a failed turn cannot record a completion cause");

    assert_eq!(
        rejection
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("turn_lifecycle_terminal_cause_matches_disposition")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

/// A terminal row cannot carry a cause with no disposition to admit it: the
/// map is total, so the check decides rather than evaluating null.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_cause_without_a_disposition_is_rejected() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = checkpoint_restart_model_call(&pool, 0x7240, true).await?;

    let rejection = sqlx::query(
        "UPDATE turn_lifecycle
            SET state_kind = 'terminal',
                terminal_frontier_id = starting_frontier_id,
                terminal_disposition_kind = NULL,
                active_phase_kind = NULL,
                current_attempt_id = NULL,
                terminal_cause_kind = $2
          WHERE turn_id = $1",
    )
    .bind(fixture.turn.into_uuid())
    .bind(turn_terminal_cause_to_str(
        TurnTerminalCause::ModelCallFailed,
    ))
    .execute(&pool)
    .await
    .expect_err("a cause needs a disposition that admits it");

    assert_eq!(
        rejection
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("turn_lifecycle_terminal_cause_matches_disposition")
    );
    pool.close().await;
    drop(container);
    Ok(())
}

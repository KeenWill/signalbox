//! What the outbox decoder admits, and the stall a row it cannot decode imposes.
//!
//! The outbox dispatcher is a singleton over a single global cursor: one
//! committed row it cannot decode is not a lost event for one session, it is a
//! cursor that never advances again for *any* session. Two separate lines are
//! held against that here.
//!
//! The first is a compile-time line. Every dispatched enum below is enumerated
//! by an exhaustive `match` with no wildcard arm, which *produces* the
//! inventory rather than sitting beside a hand-written one, so a new variant
//! stops this crate compiling until this file accounts for it.
//!
//! The second is a durable line, and it deliberately states no spellings of its
//! own. `docs/style.md` gives each closed discriminator written to PostgreSQL
//! one encoder and one decoder and forbids a second spelling table, so a test
//! that restated the decoder's literals could pass while the decoder itself was
//! mistyped — the exact regression this file exists to catch. Instead the
//! spellings come from the durable `CHECK` constraint in the live catalogue and
//! are fed through the *production* decoder, asserting that what storage admits
//! and what the decoder produces are the same closed set.
//!
//! The third is behavioral: `an_undecodable_committed_row_stalls_every_session`
//! records what the dispatcher does today when a committed row cannot be
//! decoded, including the effect on an unrelated session's committed event.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{collections::BTreeSet, error::Error, fmt};

use signalbox_domain::{
    ContextFrontierId, CreateSession, DelegationMessageId, DirectModelSelection, DurableCommandId,
    ModelCallId, ModelSelectionRequest, PreparedCreateSession, SessionConfigurationDefaults,
    SessionCreationCause, SessionCreationProvenance, SessionId, ToolAttemptId, ToolRequestId,
    TranscriptAncestry, TurnId,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    create_session::CreateSessionRepository,
    local_test_connection_options, migrate,
    outbox::{
        DispatchedBoundChildAction, DispatchedDelegationOutcome, DispatchedDelegationPolicy,
        DispatchedDelegationProvenance, DispatchedDelegationReason, DispatchedDelegationUpdate,
        DispatchedDelegationWaitMode, DispatchedDelegationWake, DispatchedModelCallDisposition,
        DispatchedModelCallState, DispatchedReconciliationOperation, DispatchedToolBatchState,
        OutboxCorruption, OutboxDeliveryDecision, OutboxDispatchError, OutboxDispatcher,
        decode_bound_action, decode_delegation_outcome, decode_delegation_reason, decode_wait_mode,
    },
};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_outbox_decode";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

const DELEGATION_UPDATES: &str = "delegation_update_outbox_event";

/// The session whose committed event is made undecodable.
const UNDECODABLE_SESSION: u128 = 0x0d01;
/// A second, wholly unrelated session, committed behind the undecodable row.
const FOLLOWING_SESSION: u128 = 0x0d02;
const UNDECODABLE_CREATE_COMMAND: u128 = 0x0d11;
const FOLLOWING_CREATE_COMMAND: u128 = 0x0d12;

/// Identities that only have to be distinct; no assertion reads their values.
const ARBITRARY_SPAWNING_REQUEST_SEED: u128 = 0x0e01;
const ARBITRARY_AWAITING_REQUEST_SEED: u128 = 0x0e02;
const ARBITRARY_CHILD_SESSION_SEED: u128 = 0x0e03;
const ARBITRARY_SENDER_SESSION_SEED: u128 = 0x0e04;
const ARBITRARY_RECIPIENT_SESSION_SEED: u128 = 0x0e05;
const ARBITRARY_MESSAGE_SEED: u128 = 0x0e06;
const ARBITRARY_TURN_SEED: u128 = 0x0e07;
const ARBITRARY_COMMAND_SEED: u128 = 0x0e08;
const ARBITRARY_CALL_SEED: u128 = 0x0e09;
const ARBITRARY_ATTEMPT_SEED: u128 = 0x0e0a;
const ARBITRARY_FRONTIER_SEED: u128 = 0x0e0b;
const ARBITRARY_ORDINAL: u64 = 1;

fn session(value: u128) -> SessionId {
    SessionId::from_uuid(Uuid::from_u128(value))
}

fn command(value: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(value))
}

fn tool_request(value: u128) -> ToolRequestId {
    ToolRequestId::from_uuid(Uuid::from_u128(value))
}

fn credential_pin() -> SessionCredentialPin {
    SessionCredentialPin::try_new(vec![SessionModelCredential::new(
        "test-model-family",
        "test-model-primary",
    )])
    .expect("test credential pin is valid")
}

fn creation(session_seed: u128, command_seed: u128) -> PreparedCreateSession {
    CreateSession::new(
        command(command_seed),
        SessionCreationProvenance::new(
            SessionCreationCause::UserInitiated,
            TranscriptAncestry::None,
        ),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(0x0a01)),
        )),
    )
    .prepare(session(session_seed))
    .expect("user-initiated creation without ancestry is preparable")
}

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_fsync_enabled()
        .with_tag(POSTGRES_IMAGE_TAG)
        .start()
        .await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let database_url =
        format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    Ok((container, pool))
}

// ---------------------------------------------------------------------------
// Inventories the compiler keeps complete
// ---------------------------------------------------------------------------
//
// Each inventory is produced by an exhaustive match naming its own successor,
// so it cannot fall behind the enum: adding a variant fails to compile here.
// None of them states a durable spelling — production defines that table in
// exactly one place, and the assertions below read the other side from the
// database rather than restating it here.

/// Every bound-child action, in an inventory the compiler keeps complete.
fn every_bound_child_action() -> Vec<DispatchedBoundChildAction> {
    let mut actions = Vec::new();
    let mut next = Some(DispatchedBoundChildAction::KeepRunning);
    while let Some(current) = next {
        next = match current {
            DispatchedBoundChildAction::KeepRunning => Some(DispatchedBoundChildAction::Stop),
            DispatchedBoundChildAction::Stop => Some(DispatchedBoundChildAction::Cancel),
            DispatchedBoundChildAction::Cancel => None,
        };
        actions.push(current);
    }
    actions
}

/// Every delegation wait mode, in an inventory the compiler keeps complete.
fn every_delegation_wait_mode() -> Vec<DispatchedDelegationWaitMode> {
    let mut modes = Vec::new();
    let mut next = Some(DispatchedDelegationWaitMode::Foreground);
    while let Some(current) = next {
        next = match current {
            DispatchedDelegationWaitMode::Foreground => {
                Some(DispatchedDelegationWaitMode::Background)
            }
            DispatchedDelegationWaitMode::Background => None,
        };
        modes.push(current);
    }
    modes
}

/// Every delegation outcome, in an inventory the compiler keeps complete.
fn every_delegation_outcome() -> Vec<DispatchedDelegationOutcome> {
    let mut outcomes = Vec::new();
    let mut next = Some(DispatchedDelegationOutcome::ResultReturned);
    while let Some(current) = next {
        next = match current {
            DispatchedDelegationOutcome::ResultReturned => {
                Some(DispatchedDelegationOutcome::ChildFailed)
            }
            DispatchedDelegationOutcome::ChildFailed => {
                Some(DispatchedDelegationOutcome::ChildStopped)
            }
            DispatchedDelegationOutcome::ChildStopped => {
                Some(DispatchedDelegationOutcome::ChildCancelled)
            }
            DispatchedDelegationOutcome::ChildCancelled => {
                Some(DispatchedDelegationOutcome::ContinueRunning)
            }
            DispatchedDelegationOutcome::ContinueRunning => {
                Some(DispatchedDelegationOutcome::AlreadyTerminal)
            }
            DispatchedDelegationOutcome::AlreadyTerminal => None,
        };
        outcomes.push(current);
    }
    outcomes
}

/// Every delegation reason, in an inventory the compiler keeps complete.
fn every_delegation_reason() -> Vec<DispatchedDelegationReason> {
    let mut reasons = Vec::new();
    let mut next = Some(DispatchedDelegationReason::ChildCompleted);
    while let Some(current) = next {
        next = match current {
            DispatchedDelegationReason::ChildCompleted => {
                Some(DispatchedDelegationReason::ChildExecutionFailed)
            }
            DispatchedDelegationReason::ChildExecutionFailed => {
                Some(DispatchedDelegationReason::ChildResultUnavailable)
            }
            DispatchedDelegationReason::ChildResultUnavailable => {
                Some(DispatchedDelegationReason::ChildCancelled)
            }
            DispatchedDelegationReason::ChildCancelled => {
                Some(DispatchedDelegationReason::ParentStoppedWithDescendants)
            }
            DispatchedDelegationReason::ParentStoppedWithDescendants => {
                Some(DispatchedDelegationReason::ParentCancelledWithDescendants)
            }
            DispatchedDelegationReason::ParentCancelledWithDescendants => None,
        };
        reasons.push(current);
    }
    reasons
}

/// Every delegation policy, in an inventory the compiler keeps complete.
fn every_delegation_policy() -> Vec<DispatchedDelegationPolicy> {
    let mut policies = Vec::new();
    let mut next = Some(DispatchedDelegationPolicy::Background);
    while let Some(current) = next {
        next = match current {
            DispatchedDelegationPolicy::Background => Some(DispatchedDelegationPolicy::Bound {
                on_parent_stopped: DispatchedBoundChildAction::Stop,
                on_parent_cancelled: DispatchedBoundChildAction::Cancel,
            }),
            DispatchedDelegationPolicy::Bound { .. } => None,
        };
        policies.push(current);
    }
    policies
}

/// Every delegation provenance, in an inventory the compiler keeps complete.
fn every_delegation_provenance() -> Vec<DispatchedDelegationProvenance> {
    let mut provenances = Vec::new();
    let mut next = Some(DispatchedDelegationProvenance::ChildTurn {
        session: session(ARBITRARY_CHILD_SESSION_SEED),
        turn: TurnId::from_uuid(Uuid::from_u128(ARBITRARY_TURN_SEED)),
    });
    while let Some(current) = next {
        next = match current {
            DispatchedDelegationProvenance::ChildTurn { .. } => {
                Some(DispatchedDelegationProvenance::ParentTurnCommand {
                    session: session(UNDECODABLE_SESSION),
                    turn: TurnId::from_uuid(Uuid::from_u128(ARBITRARY_TURN_SEED)),
                    command: command(ARBITRARY_COMMAND_SEED),
                })
            }
            DispatchedDelegationProvenance::ParentTurnCommand { .. } => {
                Some(DispatchedDelegationProvenance::ParentGoalCommand {
                    session: session(UNDECODABLE_SESSION),
                    goal_generation: ARBITRARY_ORDINAL,
                    command: command(ARBITRARY_COMMAND_SEED),
                })
            }
            DispatchedDelegationProvenance::ParentGoalCommand { .. } => None,
        };
        provenances.push(current);
    }
    provenances
}

/// Every delegation update, in an inventory the compiler keeps complete.
///
/// The variants carry relationship identities that only a fixture can supply,
/// so this lives here rather than on the enum.
fn every_delegation_update() -> Vec<DispatchedDelegationUpdate> {
    let mut updates = Vec::new();
    let mut next = Some(DispatchedDelegationUpdate::ChildSpawned {
        spawning_request: tool_request(ARBITRARY_SPAWNING_REQUEST_SEED),
        child: session(ARBITRARY_CHILD_SESSION_SEED),
        policy: DispatchedDelegationPolicy::Background,
    });
    while let Some(current) = next {
        next = match current {
            DispatchedDelegationUpdate::ChildSpawned { .. } => {
                Some(DispatchedDelegationUpdate::ChildWaiting {
                    spawning_request: tool_request(ARBITRARY_SPAWNING_REQUEST_SEED),
                    child: session(ARBITRARY_CHILD_SESSION_SEED),
                    awaiting_request: tool_request(ARBITRARY_AWAITING_REQUEST_SEED),
                    mode: DispatchedDelegationWaitMode::Foreground,
                })
            }
            DispatchedDelegationUpdate::ChildWaiting { .. } => {
                Some(DispatchedDelegationUpdate::ChildLifecycleDisposition {
                    spawning_request: tool_request(ARBITRARY_SPAWNING_REQUEST_SEED),
                    child: session(ARBITRARY_CHILD_SESSION_SEED),
                    event_ordinal: ARBITRARY_ORDINAL,
                    outcome: DispatchedDelegationOutcome::ChildStopped,
                    reason: DispatchedDelegationReason::ParentStoppedWithDescendants,
                    provenance: DispatchedDelegationProvenance::ParentTurnCommand {
                        session: session(UNDECODABLE_SESSION),
                        turn: TurnId::from_uuid(Uuid::from_u128(ARBITRARY_TURN_SEED)),
                        command: command(ARBITRARY_COMMAND_SEED),
                    },
                })
            }
            DispatchedDelegationUpdate::ChildLifecycleDisposition { .. } => {
                Some(DispatchedDelegationUpdate::ChildResult {
                    spawning_request: tool_request(ARBITRARY_SPAWNING_REQUEST_SEED),
                    child: session(ARBITRARY_CHILD_SESSION_SEED),
                    outcome: DispatchedDelegationOutcome::ResultReturned,
                    reason: DispatchedDelegationReason::ChildCompleted,
                    provenance: DispatchedDelegationProvenance::ChildTurn {
                        session: session(ARBITRARY_CHILD_SESSION_SEED),
                        turn: TurnId::from_uuid(Uuid::from_u128(ARBITRARY_TURN_SEED)),
                    },
                    content: Some("delegated result content".to_owned()),
                })
            }
            DispatchedDelegationUpdate::ChildResult { .. } => {
                Some(DispatchedDelegationUpdate::SessionMessage {
                    spawning_request: tool_request(ARBITRARY_SPAWNING_REQUEST_SEED),
                    message: DelegationMessageId::from_uuid(Uuid::from_u128(
                        ARBITRARY_MESSAGE_SEED,
                    )),
                    sender: session(ARBITRARY_SENDER_SESSION_SEED),
                    recipient: session(ARBITRARY_RECIPIENT_SESSION_SEED),
                    message_ordinal: ARBITRARY_ORDINAL,
                    delivery_sequence: ARBITRARY_ORDINAL,
                    content: "relationship message content".to_owned(),
                })
            }
            DispatchedDelegationUpdate::SessionMessage { .. } => None,
        };
        updates.push(current);
    }
    updates
}

/// Every delegation wake, in an inventory the compiler keeps complete.
fn every_delegation_wake() -> Vec<DispatchedDelegationWake> {
    let mut wakes = Vec::new();
    let mut next = Some(DispatchedDelegationWake::Result {
        spawning_request: tool_request(ARBITRARY_SPAWNING_REQUEST_SEED),
        awaiting_request: Some(tool_request(ARBITRARY_AWAITING_REQUEST_SEED)),
    });
    while let Some(current) = next {
        next = match current {
            DispatchedDelegationWake::Result { .. } => Some(DispatchedDelegationWake::Message {
                spawning_request: tool_request(ARBITRARY_SPAWNING_REQUEST_SEED),
                message: DelegationMessageId::from_uuid(Uuid::from_u128(ARBITRARY_MESSAGE_SEED)),
            }),
            DispatchedDelegationWake::Message { .. } => None,
        };
        wakes.push(current);
    }
    wakes
}

/// Every model-call disposition, in an inventory the compiler keeps complete.
fn every_model_call_disposition() -> Vec<DispatchedModelCallDisposition> {
    let mut dispositions = Vec::new();
    let mut next = Some(DispatchedModelCallDisposition::Completed);
    while let Some(current) = next {
        next = match current {
            DispatchedModelCallDisposition::Completed => {
                Some(DispatchedModelCallDisposition::KnownFailed)
            }
            DispatchedModelCallDisposition::KnownFailed => {
                Some(DispatchedModelCallDisposition::Refused)
            }
            DispatchedModelCallDisposition::Refused => {
                Some(DispatchedModelCallDisposition::Cancelled)
            }
            DispatchedModelCallDisposition::Cancelled => {
                Some(DispatchedModelCallDisposition::Ambiguous)
            }
            DispatchedModelCallDisposition::Ambiguous => None,
        };
        dispositions.push(current);
    }
    dispositions
}

/// Every model-call state, in an inventory the compiler keeps complete.
fn every_model_call_state() -> Vec<DispatchedModelCallState> {
    let mut states = Vec::new();
    let mut next = Some(DispatchedModelCallState::Prepared);
    while let Some(current) = next {
        next = match current {
            DispatchedModelCallState::Prepared => Some(DispatchedModelCallState::InFlight),
            DispatchedModelCallState::InFlight => {
                Some(DispatchedModelCallState::CancellationRequested)
            }
            DispatchedModelCallState::CancellationRequested => Some(
                DispatchedModelCallState::Terminal(DispatchedModelCallDisposition::Completed),
            ),
            DispatchedModelCallState::Terminal(_) => None,
        };
        states.push(current);
    }
    states
}

/// Every tool-batch state, in an inventory the compiler keeps complete.
fn every_tool_batch_state() -> Vec<DispatchedToolBatchState> {
    let mut states = Vec::new();
    let mut next = Some(DispatchedToolBatchState::Proposed {
        frontier: ContextFrontierId::from_uuid(Uuid::from_u128(ARBITRARY_FRONTIER_SEED)),
    });
    while let Some(current) = next {
        next = match current {
            DispatchedToolBatchState::Proposed { .. } => {
                Some(DispatchedToolBatchState::ResultsProjected {
                    frontier: ContextFrontierId::from_uuid(Uuid::from_u128(
                        ARBITRARY_FRONTIER_SEED,
                    )),
                })
            }
            DispatchedToolBatchState::ResultsProjected { .. } => {
                Some(DispatchedToolBatchState::RecoveryRequired {
                    attempt: ToolAttemptId::from_uuid(Uuid::from_u128(ARBITRARY_ATTEMPT_SEED)),
                })
            }
            DispatchedToolBatchState::RecoveryRequired { .. } => None,
        };
        states.push(current);
    }
    states
}

/// Every reconciliation operation, in an inventory the compiler keeps complete.
fn every_reconciliation_operation() -> Vec<DispatchedReconciliationOperation> {
    let mut operations = Vec::new();
    let mut next = Some(DispatchedReconciliationOperation::ModelCall(
        ModelCallId::from_uuid(Uuid::from_u128(ARBITRARY_CALL_SEED)),
    ));
    while let Some(current) = next {
        next = match current {
            DispatchedReconciliationOperation::ModelCall(_) => {
                Some(DispatchedReconciliationOperation::ToolAttempt(
                    ToolAttemptId::from_uuid(Uuid::from_u128(ARBITRARY_ATTEMPT_SEED)),
                ))
            }
            DispatchedReconciliationOperation::ToolAttempt(_) => None,
        };
        operations.push(current);
    }
    operations
}

// ---------------------------------------------------------------------------
// Durable CHECK constraints
// ---------------------------------------------------------------------------

/// The single-quoted text literals one `CHECK` definition admits.
///
/// PostgreSQL renders an `IN` list as `= ANY (ARRAY['a'::text, 'b'::text])`,
/// and none of the outbox spellings contain a quote, so the literals are
/// exactly the odd-indexed fragments of a split on `'`. The image tag is
/// pinned, so this rendering is deterministic.
fn admitted_text_literals(constraint_definition: &str) -> BTreeSet<String> {
    constraint_definition
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The spellings the single-column `CHECK` guarding one durable column admits.
///
/// Looked up by table and column rather than by constraint name: most of these
/// are inline column checks whose names PostgreSQL generates, and a generated
/// name is not a contract. Multi-column shape constraints are excluded, so this
/// reads the check that closes the column's own value set and nothing else.
async fn admitted_spellings(
    pool: &PgPool,
    table: &str,
    column: &str,
) -> Result<BTreeSet<String>, Box<dyn Error>> {
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(constraint_entry.oid)
           FROM pg_constraint AS constraint_entry
           JOIN pg_attribute AS column_entry
             ON column_entry.attrelid = constraint_entry.conrelid
            AND column_entry.attnum = ANY (constraint_entry.conkey)
          WHERE constraint_entry.conrelid = $1::regclass
            AND constraint_entry.contype = 'c'
            AND array_length(constraint_entry.conkey, 1) = 1
            AND column_entry.attname = $2",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await?;
    Ok(admitted_text_literals(&definition))
}

/// Asserts storage and the production decoder close over the same set.
///
/// Every spelling comes from the database and is decoded by the real decoder,
/// so neither side is restated here. A spelling storage admits that the decoder
/// rejects is the stall; a variant no admitted spelling reaches is a decoder
/// arm storage can never produce.
#[track_caller]
fn assert_storage_and_decoder_agree<Value: PartialEq + fmt::Debug>(
    admitted: &BTreeSet<String>,
    decode: fn(&str) -> Result<Value, OutboxCorruption>,
    inventory: &[Value],
    column: &str,
) {
    let decoded: Vec<Value> = admitted
        .iter()
        .map(|spelling| {
            decode(spelling).unwrap_or_else(|corruption| {
                panic!(
                    "durable {column} admits {spelling:?}, which the outbox decoder rejects \
                     ({corruption:?}); a committed row carrying it can never be decoded, and \
                     stalls the singleton outbox cursor for every session"
                )
            })
        })
        .collect();
    assert_eq!(
        decoded.len(),
        inventory.len(),
        "durable {column} admits {} spellings but the dispatched enum has {} variants",
        decoded.len(),
        inventory.len()
    );
    inventory.iter().for_each(|variant| {
        assert!(
            decoded.contains(variant),
            "no spelling durable {column} admits decodes to {variant:?}"
        );
    });
}

/// The literal extractor decides every constraint assertion below, so its own
/// behavior is pinned rather than trusted: a rendered `IN` list, a rendered
/// nullable list, and a definition naming no literal at all.
#[test]
fn admitted_text_literals_reads_a_rendered_check_definition() {
    assert_eq!(
        admitted_text_literals(
            "CHECK ((update_kind = ANY (ARRAY['child_spawned'::text, 'child_result'::text])))"
        ),
        BTreeSet::from(["child_spawned".to_owned(), "child_result".to_owned()])
    );
    assert_eq!(
        admitted_text_literals(
            "CHECK (((wait_mode IS NULL) OR (wait_mode = ANY (ARRAY['foreground'::text]))))"
        ),
        BTreeSet::from(["foreground".to_owned()])
    );
    assert_eq!(
        admitted_text_literals("CHECK ((storage_version = 1))"),
        BTreeSet::new()
    );
}

/// Families decoded from a row's shape rather than from one text column.
///
/// No single-argument decoder exists to drive these with the schema's own
/// literals, so they are enumerated only: adding a variant fails to compile in
/// the inventory above, which is what forces this file to be revisited. The
/// counts are stated so that a variant added *and* removed still trips here.
#[test]
fn row_decoded_families_are_enumerated() {
    assert_eq!(every_delegation_update().len(), 5);
    assert_eq!(every_delegation_wake().len(), 2);
    assert_eq!(every_delegation_policy().len(), 2);
    assert_eq!(every_delegation_provenance().len(), 3);
    assert_eq!(every_model_call_state().len(), 4);
    assert_eq!(every_model_call_disposition().len(), 5);
    assert_eq!(every_tool_batch_state().len(), 3);
    assert_eq!(every_reconciliation_operation().len(), 2);
}

/// Storage and the decoder close over the same delegation spellings.
///
/// This is the tripwire itself, for the four columns a single-argument decoder
/// closes. `outcome_kind` and `reason_kind` are the columns the
/// `child_lifecycle_disposition` and `child_result` arms read, which is where
/// the concern was raised: those events are written by production code, and a
/// spelling storage admits without a decoder arm stalls the global cursor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_storage_and_decoder_close_over_the_same_spellings() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;

    assert_storage_and_decoder_agree(
        &admitted_spellings(&pool, DELEGATION_UPDATES, "outcome_kind").await?,
        decode_delegation_outcome,
        &every_delegation_outcome(),
        "outcome_kind",
    );
    assert_storage_and_decoder_agree(
        &admitted_spellings(&pool, DELEGATION_UPDATES, "reason_kind").await?,
        decode_delegation_reason,
        &every_delegation_reason(),
        "reason_kind",
    );
    assert_storage_and_decoder_agree(
        &admitted_spellings(&pool, DELEGATION_UPDATES, "wait_mode").await?,
        decode_wait_mode,
        &every_delegation_wait_mode(),
        "wait_mode",
    );
    assert_storage_and_decoder_agree(
        &admitted_spellings(&pool, DELEGATION_UPDATES, "on_parent_stopped").await?,
        decode_bound_action,
        &every_bound_child_action(),
        "on_parent_stopped",
    );
    assert_storage_and_decoder_agree(
        &admitted_spellings(&pool, DELEGATION_UPDATES, "on_parent_cancelled").await?,
        decode_bound_action,
        &every_bound_child_action(),
        "on_parent_cancelled",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

// ---------------------------------------------------------------------------
// stall_pin
// ---------------------------------------------------------------------------

/// A committed row the dispatcher cannot decode stalls every session, today.
///
/// This pins current behavior rather than endorsing it. The dispatcher offers
/// exactly the next committed sequence and advances the singleton
/// `outbox_delivery_state` cursor only after the consumer accepts; a row that
/// fails to decode never reaches a consumer, so the cursor cannot move and no
/// later sequence — for any session — is ever offered. The assertions below
/// state that as a conjunction because it is one contract: the error repeats,
/// the cursor holds, and an unrelated session's already-committed event sits
/// behind it. Splitting them would let each half pass while the combined
/// behavior regressed.
///
/// The concern this documents: with delegation events flowing through the same
/// singleton cursor, an undecodable persisted variant is a system-wide stall,
/// not a per-session one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_undecodable_committed_row_stalls_every_session() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let sessions = CreateSessionRepository::new(pool.clone(), credential_pin());
    sessions
        .handle(creation(UNDECODABLE_SESSION, UNDECODABLE_CREATE_COMMAND))
        .await?;
    sessions
        .handle(creation(FOLLOWING_SESSION, FOLLOWING_CREATE_COMMAND))
        .await?;

    // Remove the typed record beneath the first committed header, leaving a
    // header the decoder must reject. Triggers are disabled around the
    // deletion because the schema exists precisely to make this unreachable
    // through supported writes.
    sqlx::query("ALTER TABLE session_created_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM session_created_outbox_event WHERE session_id = $1")
        .bind(Uuid::from_u128(UNDECODABLE_SESSION))
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_created_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert!(matches!(
        dispatcher
            .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
            .await,
        Err(OutboxDispatchError::Corruption(
            OutboxCorruption::MissingTypedRecord
        ))
    ));
    assert!(
        matches!(
            dispatcher
                .dispatch_next(|_| OutboxDeliveryDecision::Delivered)
                .await,
            Err(OutboxDispatchError::Corruption(
                OutboxCorruption::MissingTypedRecord
            ))
        ),
        "the undecodable row is offered again rather than skipped"
    );

    let delivered: rust_decimal::Decimal =
        sqlx::query_scalar("SELECT delivered_through FROM outbox_delivery_state WHERE singleton")
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        delivered,
        rust_decimal::Decimal::from(0_u64),
        "the durable delivered prefix never advances past a row that cannot be decoded"
    );

    let following: rust_decimal::Decimal =
        sqlx::query_scalar("SELECT event_sequence FROM outbox_event WHERE session_id = $1")
            .bind(Uuid::from_u128(FOLLOWING_SESSION))
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        following,
        rust_decimal::Decimal::from(2_u64),
        "an unrelated session's event is committed behind the undecodable row, and the stalled \
         cursor makes it permanently undeliverable"
    );

    pool.close().await;
    drop(container);
    Ok(())
}

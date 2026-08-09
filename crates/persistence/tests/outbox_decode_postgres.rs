//! Storage spellings the outbox decoder admits, and the stall one it cannot decode imposes.
//!
//! The outbox dispatcher is a singleton over a single global cursor: one
//! committed row it cannot decode is not a lost event for one session, it is a
//! cursor that never advances again for *any* session. The tests here hold two
//! separate lines against that.
//!
//! The first is a compile-time line. Every enum below is enumerated by an
//! exhaustive `match` with no wildcard arm, so a new dispatched variant stops
//! this crate compiling until it names the exact text storage carries for it —
//! and, for every column closed by a single-column `CHECK`, that constraint is
//! asserted to admit exactly the same set. A spelling that reaches storage
//! without a decoder arm is precisely the stall, so the two sides are pinned
//! against each other rather than trusted to stay aligned. The one column
//! closed jointly with another carries its spellings in a compile-enumerated
//! assertion instead, noted where it appears.
//!
//! The second is a behavioral line:
//! `an_undecodable_committed_row_stalls_every_session` records what the
//! dispatcher does today when a committed row cannot be decoded, including the
//! effect on an unrelated session's already-committed event.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{collections::BTreeSet, error::Error};

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
// Storage spellings, enumerated by the compiler
// ---------------------------------------------------------------------------
//
// Each `*_storage_name` below is the decoder's text contract restated as a
// total function. None carries a wildcard arm, so adding a dispatched variant
// fails to compile here until the new variant names the text storage carries —
// and each `every_*` inventory is produced *by* an exhaustive match rather than
// kept beside one, per `docs/style.md`, so a new variant also joins the set the
// durable `CHECK` assertions below compare against.

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

fn bound_child_action_storage_name(action: DispatchedBoundChildAction) -> &'static str {
    match action {
        DispatchedBoundChildAction::KeepRunning => "keep_running",
        DispatchedBoundChildAction::Stop => "stop",
        DispatchedBoundChildAction::Cancel => "cancel",
    }
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

fn delegation_wait_mode_storage_name(mode: DispatchedDelegationWaitMode) -> &'static str {
    match mode {
        DispatchedDelegationWaitMode::Foreground => "foreground",
        DispatchedDelegationWaitMode::Background => "background",
    }
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

fn delegation_outcome_storage_name(outcome: DispatchedDelegationOutcome) -> &'static str {
    match outcome {
        DispatchedDelegationOutcome::ResultReturned => "result_returned",
        DispatchedDelegationOutcome::ChildFailed => "child_failed",
        DispatchedDelegationOutcome::ChildStopped => "child_stopped",
        DispatchedDelegationOutcome::ChildCancelled => "child_cancelled",
        DispatchedDelegationOutcome::ContinueRunning => "continue_running",
        DispatchedDelegationOutcome::AlreadyTerminal => "already_terminal",
    }
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

fn delegation_reason_storage_name(reason: DispatchedDelegationReason) -> &'static str {
    match reason {
        DispatchedDelegationReason::ChildCompleted => "child_completed",
        DispatchedDelegationReason::ChildExecutionFailed => "child_execution_failed",
        DispatchedDelegationReason::ChildResultUnavailable => "child_result_unavailable",
        DispatchedDelegationReason::ChildCancelled => "child_cancelled",
        DispatchedDelegationReason::ParentStoppedWithDescendants => {
            "parent_stopped_parent_and_descendants"
        }
        DispatchedDelegationReason::ParentCancelledWithDescendants => {
            "parent_cancelled_parent_and_descendants"
        }
    }
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

fn delegation_policy_storage_name(policy: DispatchedDelegationPolicy) -> &'static str {
    match policy {
        DispatchedDelegationPolicy::Background => "background",
        DispatchedDelegationPolicy::Bound { .. } => "bound",
    }
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

fn delegation_provenance_storage_name(provenance: DispatchedDelegationProvenance) -> &'static str {
    match provenance {
        DispatchedDelegationProvenance::ChildTurn { .. } => "child_turn",
        DispatchedDelegationProvenance::ParentTurnCommand { .. } => "parent_turn_command",
        DispatchedDelegationProvenance::ParentGoalCommand { .. } => "parent_goal_command",
    }
}

/// Every delegation update, in an inventory the compiler keeps complete.
///
/// The variants carry relationship identities that only a fixture can supply,
/// so this lives here rather than on the enum, exactly as the repository's
/// other identity-carrying inventories do.
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

fn delegation_update_storage_name(update: &DispatchedDelegationUpdate) -> &'static str {
    match update {
        DispatchedDelegationUpdate::ChildSpawned { .. } => "child_spawned",
        DispatchedDelegationUpdate::ChildWaiting { .. } => "child_waiting",
        DispatchedDelegationUpdate::ChildLifecycleDisposition { .. } => {
            "child_lifecycle_disposition"
        }
        DispatchedDelegationUpdate::ChildResult { .. } => "child_result",
        DispatchedDelegationUpdate::SessionMessage { .. } => "session_message",
    }
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

fn delegation_wake_storage_name(wake: DispatchedDelegationWake) -> &'static str {
    match wake {
        DispatchedDelegationWake::Result { .. } => "result",
        DispatchedDelegationWake::Message { .. } => "message",
    }
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

fn model_call_disposition_storage_name(
    disposition: DispatchedModelCallDisposition,
) -> &'static str {
    match disposition {
        DispatchedModelCallDisposition::Completed => "completed",
        DispatchedModelCallDisposition::KnownFailed => "known_failed",
        DispatchedModelCallDisposition::Refused => "refused",
        DispatchedModelCallDisposition::Cancelled => "cancelled",
        DispatchedModelCallDisposition::Ambiguous => "ambiguous",
    }
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

fn model_call_state_storage_name(state: DispatchedModelCallState) -> &'static str {
    match state {
        DispatchedModelCallState::Prepared => "prepared",
        DispatchedModelCallState::InFlight => "in_flight",
        DispatchedModelCallState::CancellationRequested => "cancellation_requested",
        DispatchedModelCallState::Terminal(_) => "terminal",
    }
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

fn tool_batch_state_storage_name(state: DispatchedToolBatchState) -> &'static str {
    match state {
        DispatchedToolBatchState::Proposed { .. } => "proposed",
        DispatchedToolBatchState::ResultsProjected { .. } => "results_projected",
        DispatchedToolBatchState::RecoveryRequired { .. } => "recovery_required",
    }
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

#[track_caller]
fn assert_same_spellings(admitted: &BTreeSet<String>, decoded: &BTreeSet<String>, column: &str) {
    assert_eq!(
        admitted, decoded,
        "durable storage for {column} admits exactly the spellings the dispatched enum decodes; \
         a spelling on only one side either stalls the singleton outbox cursor or is unreachable"
    );
}

fn spellings<Value: Copy>(values: &[Value], name: fn(Value) -> &'static str) -> BTreeSet<String> {
    values.iter().map(|value| name(*value).to_owned()).collect()
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

/// Every dispatched delegation update names a distinct storage spelling.
///
/// The inventory is the compiler's: `every_delegation_update` is produced by an
/// exhaustive match, so a new variant fails to compile there, and this
/// assertion then fails until the new variant's spelling is stated.
#[test]
fn every_delegation_update_names_its_storage_spelling() {
    assert_eq!(
        every_delegation_update()
            .iter()
            .map(delegation_update_storage_name)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "child_spawned",
            "child_waiting",
            "child_lifecycle_disposition",
            "child_result",
            "session_message",
        ])
    );
}

/// The delegation families the tripwire names carry the spellings storage uses.
#[test]
fn every_delegation_component_names_its_storage_spelling() {
    assert_eq!(
        spellings(&every_delegation_wake(), delegation_wake_storage_name),
        BTreeSet::from(["result".to_owned(), "message".to_owned()])
    );
    assert_eq!(
        spellings(&every_delegation_policy(), delegation_policy_storage_name),
        BTreeSet::from(["background".to_owned(), "bound".to_owned()])
    );
    assert_eq!(
        spellings(&every_bound_child_action(), bound_child_action_storage_name),
        BTreeSet::from([
            "keep_running".to_owned(),
            "stop".to_owned(),
            "cancel".to_owned()
        ])
    );
    assert_eq!(
        spellings(
            &every_delegation_wait_mode(),
            delegation_wait_mode_storage_name
        ),
        BTreeSet::from(["foreground".to_owned(), "background".to_owned()])
    );
    assert_eq!(
        spellings(
            &every_delegation_provenance(),
            delegation_provenance_storage_name
        ),
        BTreeSet::from([
            "child_turn".to_owned(),
            "parent_turn_command".to_owned(),
            "parent_goal_command".to_owned(),
        ])
    );
}

/// The lifecycle families carry the spellings storage uses.
///
/// `terminal_disposition_kind` has no single-column check for the durable
/// assertion below to read, so this is the only place its spellings are stated.
#[test]
fn every_lifecycle_component_names_its_storage_spelling() {
    assert_eq!(
        spellings(&every_model_call_state(), model_call_state_storage_name),
        BTreeSet::from([
            "prepared".to_owned(),
            "in_flight".to_owned(),
            "cancellation_requested".to_owned(),
            "terminal".to_owned(),
        ])
    );
    assert_eq!(
        spellings(
            &every_model_call_disposition(),
            model_call_disposition_storage_name
        ),
        BTreeSet::from([
            "completed".to_owned(),
            "known_failed".to_owned(),
            "refused".to_owned(),
            "cancelled".to_owned(),
            "ambiguous".to_owned(),
        ])
    );
    assert_eq!(
        spellings(&every_tool_batch_state(), tool_batch_state_storage_name),
        BTreeSet::from([
            "proposed".to_owned(),
            "results_projected".to_owned(),
            "recovery_required".to_owned(),
        ])
    );
}

/// The reconciliation inventory is shape-decoded rather than text-decoded, so
/// it carries no spelling; it is enumerated here so a new ambiguous operation
/// still fails to compile until this file considers it.
#[test]
fn every_reconciliation_operation_is_enumerated() {
    assert_eq!(every_reconciliation_operation().len(), 2);
}

// ---------------------------------------------------------------------------
// Durable constraint agreement
// ---------------------------------------------------------------------------

/// The delegation update spellings storage admits are exactly those decoded.
///
/// This is the tripwire itself. `crates/persistence/src/outbox.rs` decodes
/// `update_kind` into `DispatchedDelegationUpdate`; a spelling the durable
/// `CHECK` admits without a decoder arm becomes `InvalidDelegationEvent` on a
/// committed row, which stalls the singleton cursor for every session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_update_storage_admits_exactly_the_decoded_spellings()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let updates = "delegation_update_outbox_event";

    assert_same_spellings(
        &admitted_spellings(&pool, updates, "update_kind").await?,
        &every_delegation_update()
            .iter()
            .map(|update| delegation_update_storage_name(update).to_owned())
            .collect(),
        "update_kind",
    );
    assert_same_spellings(
        &admitted_spellings(&pool, updates, "outcome_kind").await?,
        &spellings(&every_delegation_outcome(), delegation_outcome_storage_name),
        "outcome_kind",
    );
    assert_same_spellings(
        &admitted_spellings(&pool, updates, "reason_kind").await?,
        &spellings(&every_delegation_reason(), delegation_reason_storage_name),
        "reason_kind",
    );
    assert_same_spellings(
        &admitted_spellings(&pool, updates, "provenance_kind").await?,
        &spellings(
            &every_delegation_provenance(),
            delegation_provenance_storage_name,
        ),
        "provenance_kind",
    );
    assert_same_spellings(
        &admitted_spellings(&pool, updates, "wait_mode").await?,
        &spellings(
            &every_delegation_wait_mode(),
            delegation_wait_mode_storage_name,
        ),
        "wait_mode",
    );
    assert_same_spellings(
        &admitted_spellings(&pool, updates, "policy_kind").await?,
        &spellings(&every_delegation_policy(), delegation_policy_storage_name),
        "policy_kind",
    );
    assert_same_spellings(
        &admitted_spellings(&pool, updates, "on_parent_stopped").await?,
        &spellings(&every_bound_child_action(), bound_child_action_storage_name),
        "on_parent_stopped",
    );
    assert_same_spellings(
        &admitted_spellings(&pool, updates, "on_parent_cancelled").await?,
        &spellings(&every_bound_child_action(), bound_child_action_storage_name),
        "on_parent_cancelled",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The delegation wake subjects storage admits are exactly those decoded.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn delegation_wake_storage_admits_exactly_the_decoded_spellings() -> Result<(), Box<dyn Error>>
{
    let (container, pool) = migrated_postgres().await?;

    assert_same_spellings(
        &admitted_spellings(&pool, "delegation_wake_outbox_event", "subject_kind").await?,
        &spellings(&every_delegation_wake(), delegation_wake_storage_name),
        "subject_kind",
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// The model-call and tool-batch spellings storage admits are exactly those decoded.
///
/// Only the two columns closed by a single-column `CHECK` are asserted here.
/// `terminal_disposition_kind` is closed jointly with `call_state_kind` by a
/// multi-column shape constraint, so its admitted set cannot be read off one
/// column's definition; `every_model_call_disposition` still enumerates it, so
/// a new disposition fails to compile in this file regardless.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn lifecycle_transition_storage_admits_exactly_the_decoded_spellings()
-> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;

    assert_same_spellings(
        &admitted_spellings(
            &pool,
            "model_call_transition_outbox_event",
            "call_state_kind",
        )
        .await?,
        &spellings(&every_model_call_state(), model_call_state_storage_name),
        "call_state_kind",
    );
    assert_same_spellings(
        &admitted_spellings(
            &pool,
            "tool_batch_transition_outbox_event",
            "transition_kind",
        )
        .await?,
        &spellings(&every_tool_batch_state(), tool_batch_state_storage_name),
        "transition_kind",
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

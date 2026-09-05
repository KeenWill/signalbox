//! What the outbox decoder admits, and the stall a row it cannot decode imposes.
//!
//! Each outbox dispatcher advances one consumer's global cursor: one committed
//! row it cannot decode is not a lost event for one session, it stalls that
//! consumer for *every* session. Two separate lines are held against that here.
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
    SessionCreationCause, SessionCreationProvenance, SessionId, SessionOwnership, ToolAttemptId,
    ToolRequestId, TranscriptAncestry, TurnId,
};
use signalbox_persistence::{
    SessionCredentialPin, SessionModelCredential,
    create_session::CreateSessionRepository,
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options,
    mapping::{
        DelegationPolicyStorageKind, DelegationUpdateStorageKind, DelegationWakeStorageKind,
    },
    migrate,
    outbox::{
        DispatchedBoundChildAction, DispatchedDelegationOutcome, DispatchedDelegationPolicy,
        DispatchedDelegationProvenance, DispatchedDelegationReason, DispatchedDelegationUpdate,
        DispatchedDelegationWaitMode, DispatchedDelegationWake, DispatchedModelCallDisposition,
        DispatchedModelCallState, DispatchedOutboxEventKind, DispatchedReconciliationOperation,
        DispatchedSessionCreation, DispatchedToolBatchState, OutboxCorruption,
        OutboxDeliveryDecision, OutboxDispatchError, OutboxDispatcher, decode_bound_action,
        decode_delegation_outcome, decode_delegation_policy_kind, decode_delegation_reason,
        decode_delegation_update_kind, decode_delegation_wake_subject, decode_wait_mode,
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
const DELEGATION_WAKES: &str = "delegation_wake_outbox_event";

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
/// Only makes the creation fixture valid; no assertion reads its value.
const ARBITRARY_MODEL_SELECTION_SEED: u128 = 0x0a01;

/// One relationship identity per planted update kind. They must differ: the
/// table's partial unique indexes are indexes, not triggers, so they stay
/// enforced even while referential triggers are disabled.
const SPAWNED_REQUEST_SEED: u128 = 0x0f01;
const BOUND_SPAWN_REQUEST_SEED: u128 = 0x0f06;
const GOAL_LIFECYCLE_REQUEST_SEED: u128 = 0x0f07;
/// Distinct from the event ordinal so a column read in the wrong place shows.
const ARBITRARY_GOAL_GENERATION: u64 = 2;
const WAITING_REQUEST_SEED: u128 = 0x0f02;
const LIFECYCLE_REQUEST_SEED: u128 = 0x0f03;
const RESULT_REQUEST_SEED: u128 = 0x0f04;
const MESSAGE_REQUEST_SEED: u128 = 0x0f05;
const DISPATCH_SESSION: u128 = 0x0d03;
const DISPATCH_CREATE_COMMAND: u128 = 0x0d13;
const RESULT_CONTENT: &str = "delegated result content";
const MESSAGE_CONTENT: &str = "relationship message content";

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
        SessionCreationProvenance::new(SessionCreationCause::Interactive, TranscriptAncestry::None),
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(Uuid::from_u128(ARBITRARY_MODEL_SELECTION_SEED)),
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
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs_from_example()?)
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_labels(disposable_test_container_labels())
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

/// Every delegation update kind, in an inventory the compiler keeps complete.
fn every_delegation_update_kind() -> Vec<DelegationUpdateStorageKind> {
    let mut kinds = Vec::new();
    let mut next = Some(DelegationUpdateStorageKind::ChildSpawned);
    while let Some(current) = next {
        next = match current {
            DelegationUpdateStorageKind::ChildSpawned => {
                Some(DelegationUpdateStorageKind::ChildWaiting)
            }
            DelegationUpdateStorageKind::ChildWaiting => {
                Some(DelegationUpdateStorageKind::ChildLifecycleDisposition)
            }
            DelegationUpdateStorageKind::ChildLifecycleDisposition => {
                Some(DelegationUpdateStorageKind::ChildResult)
            }
            DelegationUpdateStorageKind::ChildResult => {
                Some(DelegationUpdateStorageKind::SessionMessage)
            }
            DelegationUpdateStorageKind::SessionMessage => None,
        };
        kinds.push(current);
    }
    kinds
}

/// Every delegation policy kind, in an inventory the compiler keeps complete.
fn every_delegation_policy_storage_kind() -> Vec<DelegationPolicyStorageKind> {
    let mut kinds = Vec::new();
    let mut next = Some(DelegationPolicyStorageKind::Background);
    while let Some(current) = next {
        next = match current {
            DelegationPolicyStorageKind::Background => Some(DelegationPolicyStorageKind::Bound),
            DelegationPolicyStorageKind::Bound => None,
        };
        kinds.push(current);
    }
    kinds
}

/// Every delegation wake subject, in an inventory the compiler keeps complete.
fn every_delegation_wake_subject() -> Vec<DelegationWakeStorageKind> {
    let mut subjects = Vec::new();
    let mut next = Some(DelegationWakeStorageKind::Result);
    while let Some(current) = next {
        next = match current {
            DelegationWakeStorageKind::Result => Some(DelegationWakeStorageKind::Message),
            DelegationWakeStorageKind::Message => None,
        };
        subjects.push(current);
    }
    subjects
}

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
            DispatchedDelegationProvenance::ParentGoalCommand { .. } => {
                Some(DispatchedDelegationProvenance::ParentLifecycleCommand {
                    session: session(UNDECODABLE_SESSION),
                    command: command(ARBITRARY_COMMAND_SEED),
                })
            }
            DispatchedDelegationProvenance::ParentLifecycleCommand { .. } => None,
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
    let definitions: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(constraint_entry.oid)
           FROM pg_constraint AS constraint_entry
           JOIN pg_attribute AS column_entry
             ON column_entry.attrelid = constraint_entry.conrelid
            AND column_entry.attnum = ANY (constraint_entry.conkey)
          WHERE constraint_entry.conrelid = $1::regclass
            AND constraint_entry.contype = 'c'
            AND array_length(constraint_entry.conkey, 1) = 1
            AND column_entry.attname = $2
          ORDER BY constraint_entry.conname",
    )
    .bind(table)
    .bind(column)
    .fetch_all(pool)
    .await?;
    // A second single-column check on the same column — a nonemptiness or
    // normalization guard, say — would make "the value set" ambiguous, and
    // picking either one silently would report a decoder mismatch that has
    // nothing to do with the admitted discriminators. Refuse to guess.
    let [definition] = definitions.as_slice() else {
        return Err(format!(
            "expected exactly one single-column CHECK closing {table}.{column}, found {}: \
             {definitions:?}; the value-set constraint can no longer be identified by shape \
             alone, so this lookup must name the one it means",
            definitions.len()
        )
        .into());
    };
    Ok(admitted_text_literals(definition))
}

/// Asserts storage and the production decoder close over the same set.
///
/// Every spelling comes from the database and is decoded by the real decoder,
/// so neither side is restated here. A spelling storage admits that the decoder
/// rejects is the stall; a variant no admitted spelling reaches is a decoder
/// arm storage can never produce.
///
/// This establishes *coverage*, not meaning: it cannot distinguish a
/// permutation of the closed set. Which spelling carries which meaning is
/// pinned pairwise by the `each_*_spelling_decodes_to_its_variant` tests, and
/// the two claims are deliberately kept apart — this one needs a database and
/// names no literal, those name literals and need none.
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
                     stalls each outbox consumer cursor for every session"
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
    // The storage discriminator is decoder-driven above; this is the dispatched
    // projection, which is still decoded from the row's shape.
    assert_eq!(every_delegation_policy().len(), 2);
    assert_eq!(every_delegation_provenance().len(), 4);
    assert_eq!(every_model_call_state().len(), 4);
    assert_eq!(every_model_call_disposition().len(), 5);
    assert_eq!(every_tool_batch_state().len(), 3);
    assert_eq!(every_reconciliation_operation().len(), 2);
}

// The set agreement above is blind to a permutation: swapping two decoder arms
// leaves the decoded set identical, so lifecycle events would report the wrong
// outcome with every assertion still green. These pin the pairing itself.
//
// The literal is spelled at the assertion because the test cares about this
// exact pairing, and the value it is compared against is produced by the real
// decoder rather than by a table standing in for it. A literal that drifts away
// from what storage admits fails the durable assertion below, so the two halves
// cannot both be satisfied by a wrong spelling.

/// Each durable update-kind spelling decodes to the kind it names.
///
/// This is the arm the tripwire analysis named: `child_result` and
/// `child_lifecycle_disposition` are written by production code, and mistyping
/// either routes a committed row to the fail-closed arm, which stalls the
/// selected consumer cursor for every session.
#[test]
fn each_delegation_update_kind_spelling_decodes_to_its_variant() {
    assert_eq!(
        decode_delegation_update_kind("child_spawned").unwrap(),
        DelegationUpdateStorageKind::ChildSpawned
    );
    assert_eq!(
        decode_delegation_update_kind("child_waiting").unwrap(),
        DelegationUpdateStorageKind::ChildWaiting
    );
    assert_eq!(
        decode_delegation_update_kind("child_lifecycle_disposition").unwrap(),
        DelegationUpdateStorageKind::ChildLifecycleDisposition
    );
    assert_eq!(
        decode_delegation_update_kind("child_result").unwrap(),
        DelegationUpdateStorageKind::ChildResult
    );
    assert_eq!(
        decode_delegation_update_kind("session_message").unwrap(),
        DelegationUpdateStorageKind::SessionMessage
    );
}

/// Each durable wake-subject spelling decodes to the subject it names.
#[test]
fn each_delegation_wake_subject_spelling_decodes_to_its_variant() {
    assert_eq!(
        decode_delegation_wake_subject("result").unwrap(),
        DelegationWakeStorageKind::Result
    );
    assert_eq!(
        decode_delegation_wake_subject("message").unwrap(),
        DelegationWakeStorageKind::Message
    );
}

/// Each durable outcome spelling decodes to the outcome it names.
#[test]
fn each_delegation_outcome_spelling_decodes_to_its_variant() {
    assert_eq!(
        decode_delegation_outcome("result_returned").unwrap(),
        DispatchedDelegationOutcome::ResultReturned
    );
    assert_eq!(
        decode_delegation_outcome("child_failed").unwrap(),
        DispatchedDelegationOutcome::ChildFailed
    );
    assert_eq!(
        decode_delegation_outcome("child_stopped").unwrap(),
        DispatchedDelegationOutcome::ChildStopped
    );
    assert_eq!(
        decode_delegation_outcome("child_cancelled").unwrap(),
        DispatchedDelegationOutcome::ChildCancelled
    );
    assert_eq!(
        decode_delegation_outcome("continue_running").unwrap(),
        DispatchedDelegationOutcome::ContinueRunning
    );
    assert_eq!(
        decode_delegation_outcome("already_terminal").unwrap(),
        DispatchedDelegationOutcome::AlreadyTerminal
    );
}

/// Each durable reason spelling decodes to the reason it names.
#[test]
fn each_delegation_reason_spelling_decodes_to_its_variant() {
    assert_eq!(
        decode_delegation_reason("child_completed").unwrap(),
        DispatchedDelegationReason::ChildCompleted
    );
    assert_eq!(
        decode_delegation_reason("child_execution_failed").unwrap(),
        DispatchedDelegationReason::ChildExecutionFailed
    );
    assert_eq!(
        decode_delegation_reason("child_result_unavailable").unwrap(),
        DispatchedDelegationReason::ChildResultUnavailable
    );
    assert_eq!(
        decode_delegation_reason("child_cancelled").unwrap(),
        DispatchedDelegationReason::ChildCancelled
    );
    assert_eq!(
        decode_delegation_reason("parent_stopped_parent_and_descendants").unwrap(),
        DispatchedDelegationReason::ParentStoppedWithDescendants
    );
    assert_eq!(
        decode_delegation_reason("parent_cancelled_parent_and_descendants").unwrap(),
        DispatchedDelegationReason::ParentCancelledWithDescendants
    );
}

/// Each durable wait-mode spelling decodes to the mode it names.
#[test]
fn each_delegation_wait_mode_spelling_decodes_to_its_variant() {
    assert_eq!(
        decode_wait_mode("foreground").unwrap(),
        DispatchedDelegationWaitMode::Foreground
    );
    assert_eq!(
        decode_wait_mode("background").unwrap(),
        DispatchedDelegationWaitMode::Background
    );
}

/// Each durable bound-action spelling decodes to the action it names.
#[test]
fn each_bound_child_action_spelling_decodes_to_its_variant() {
    assert_eq!(
        decode_bound_action("keep_running").unwrap(),
        DispatchedBoundChildAction::KeepRunning
    );
    assert_eq!(
        decode_bound_action("stop").unwrap(),
        DispatchedBoundChildAction::Stop
    );
    assert_eq!(
        decode_bound_action("cancel").unwrap(),
        DispatchedBoundChildAction::Cancel
    );
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
        &admitted_spellings(&pool, DELEGATION_UPDATES, "update_kind").await?,
        decode_delegation_update_kind,
        &every_delegation_update_kind(),
        "update_kind",
    );
    assert_storage_and_decoder_agree(
        &admitted_spellings(&pool, DELEGATION_WAKES, "subject_kind").await?,
        decode_delegation_wake_subject,
        &every_delegation_wake_subject(),
        "subject_kind",
    );
    assert_storage_and_decoder_agree(
        &admitted_spellings(&pool, DELEGATION_UPDATES, "policy_kind").await?,
        decode_delegation_policy_kind,
        &every_delegation_policy_storage_kind(),
        "policy_kind",
    );
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
/// exactly the next committed sequence and advances its `outbox_consumer_cursor`
/// cursor only after the consumer accepts; a row that
/// fails to decode never reaches a consumer, so the cursor cannot move and no
/// later sequence — for any session — is ever offered. The assertions below
/// state that as a conjunction because it is one contract: the error repeats,
/// the cursor holds, and an unrelated session's already-committed event sits
/// behind it. Splitting them would let each half pass while the combined
/// behavior regressed.
///
/// The concern this documents: with delegation events flowing through the same
/// selected consumer cursor, an undecodable persisted variant is a consumer-wide stall,
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
        sqlx::query_scalar("SELECT delivered_through FROM outbox_consumer_cursor WHERE consumer_name = 'process_protocol'")
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

// ---------------------------------------------------------------------------
// Row dispatch for every admitted update kind
// ---------------------------------------------------------------------------
//
// The spelling assertions above prove the decoder routes each admitted
// `update_kind`, but not that the arm it routes to reads the right columns.
// These plant one committed row per kind and dispatch it through the real
// `OutboxDispatcher`, asserting the exact decoded variant.
//
// Referential triggers are disabled around the inserts. The relationship rows
// these updates point at are not what the decoder reads, and building them
// honestly costs roughly 700 lines across ~35 tables — the relation and its
// update are bidirectionally coupled by deferred constraint triggers, so
// neither can be inserted without the other. What is *not* disabled is what
// matters: `delegation_update_subject_shape` and
// `delegation_update_provenance_shape` are CHECK constraints, and the five
// per-kind unique indexes are indexes. Neither is a trigger, so both stay
// enforced and every row below is exactly the shape storage admits. The
// sequence allocator on the header table is likewise left enabled, so
// `event_sequence` is assigned the way production assigns it.

/// The identities one planted delegation update names.
///
/// The planters below took a run of adjacent `Uuid` arguments whose roles were
/// legible only from the helper's signature, which is exactly the cross-wiring
/// hazard the fixture rules warn about. Naming them here means a call site
/// reads its roles, and the one knob a planted kind actually varies — the
/// relationship identity — is turned by [`RelationshipFacts::spawned_by`].
#[derive(Clone, Copy)]
struct RelationshipFacts {
    parent: Uuid,
    spawning: Uuid,
    child: Uuid,
    awaiting: Uuid,
    turn: Uuid,
    command: Uuid,
    message: Uuid,
    sender: Uuid,
}

impl RelationshipFacts {
    /// Returns the same relationship under a different spawning identity.
    ///
    /// Each planted kind needs its own: the table's per-kind unique indexes are
    /// indexes rather than triggers, so they stay enforced here.
    const fn spawned_by(self, spawning: Uuid) -> Self {
        Self { spawning, ..self }
    }
}

// Each planter writes its header and typed record in one statement so the
// deferred "header requires a typed record" constraint is satisfied at commit,
// and each states only the columns its kind admits — the omitted ones must be
// NULL, and `delegation_update_subject_shape` still enforces that.

/// Plants a spawn update whose child outlives parent state changes.
async fn plant_child_spawned_background(
    pool: &PgPool,
    facts: RelationshipFacts,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id, policy_kind,
             delegation_event_ordinal, delegation_event_kind)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'child_spawned', $2, $3, 'background', 1, 'spawned'
           FROM header",
    )
    .bind(facts.parent)
    .bind(facts.spawning)
    .bind(facts.child)
    .execute(pool)
    .await?;
    Ok(())
}

/// Plants a spawn update whose child follows the two parent-state actions.
///
/// The stopped and cancelled actions differ so that an arm reading one column
/// for the other is visible in the decoded variant.
async fn plant_child_spawned_bound(
    pool: &PgPool,
    facts: RelationshipFacts,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id, policy_kind,
             on_parent_stopped, on_parent_cancelled,
             delegation_event_ordinal, delegation_event_kind)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'child_spawned', $2, $3, 'bound', 'stop', 'cancel', 1, 'spawned'
           FROM header",
    )
    .bind(facts.parent)
    .bind(facts.spawning)
    .bind(facts.child)
    .execute(pool)
    .await?;
    Ok(())
}

/// Plants a foreground wait registration.
async fn plant_child_waiting(
    pool: &PgPool,
    facts: RelationshipFacts,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             awaiting_tool_request_id, wait_mode)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'child_waiting', $2, $3, $4, 'foreground'
           FROM header",
    )
    .bind(facts.parent)
    .bind(facts.spawning)
    .bind(facts.child)
    .bind(facts.awaiting)
    .execute(pool)
    .await?;
    Ok(())
}

/// Plants a parent-stop lifecycle disposition with command provenance.
async fn plant_child_lifecycle_disposition(
    pool: &PgPool,
    facts: RelationshipFacts,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             delegation_event_ordinal, delegation_event_kind, outcome_kind, reason_kind,
             provenance_kind, provenance_session_id, provenance_turn_id,
             provenance_command_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'child_lifecycle_disposition', $2, $3, 1, 'outcome_recorded',
                'child_stopped', 'parent_stopped_parent_and_descendants',
                'parent_turn_command', $1, $4, $5
           FROM header",
    )
    .bind(facts.parent)
    .bind(facts.spawning)
    .bind(facts.child)
    .bind(facts.turn)
    .bind(facts.command)
    .execute(pool)
    .await?;
    Ok(())
}

/// Plants a parent-stop lifecycle disposition with goal-command provenance.
///
/// The third provenance shape: it names a goal generation instead of a turn,
/// and `delegation_update_provenance_shape` requires the turn column to be NULL
/// for it, so a decoder arm reading the wrong column cannot satisfy both.
async fn plant_child_lifecycle_goal_disposition(
    pool: &PgPool,
    facts: RelationshipFacts,
    goal_generation: u64,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             delegation_event_ordinal, delegation_event_kind, outcome_kind, reason_kind,
             provenance_kind, provenance_session_id, provenance_goal_generation,
             provenance_command_id)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'child_lifecycle_disposition', $2, $3, 1, 'outcome_recorded',
                'child_cancelled', 'parent_cancelled_parent_and_descendants',
                'parent_goal_command', $1, $4, $5
           FROM header",
    )
    .bind(facts.parent)
    .bind(facts.spawning)
    .bind(facts.child)
    .bind(rust_decimal::Decimal::from(goal_generation))
    .bind(facts.command)
    .execute(pool)
    .await?;
    Ok(())
}

/// Plants a returned child result with child-turn provenance.
async fn plant_child_result(
    pool: &PgPool,
    facts: RelationshipFacts,
    content: &str,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id, outcome_kind,
             reason_kind, provenance_kind, provenance_session_id, provenance_turn_id,
             result_spawning_request_id, content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'child_result', $2, $3, 'result_returned', 'child_completed',
                'child_turn', $3, $4, $2, $5
           FROM header",
    )
    .bind(facts.parent)
    .bind(facts.spawning)
    .bind(facts.child)
    .bind(facts.turn)
    .bind(content)
    .execute(pool)
    .await?;
    Ok(())
}

/// Plants a relationship message addressed to the owning session.
async fn plant_session_message(
    pool: &PgPool,
    facts: RelationshipFacts,
    content: &str,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "WITH header AS (
            INSERT INTO delegation_outbox_event(event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, $1)
            RETURNING event_sequence, event_kind, storage_version, session_id
         )
         INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, message_id, sender_session_id,
             recipient_session_id, message_ordinal, content_text)
         SELECT event_sequence, event_kind, storage_version, session_id,
                'session_message', $2, $3, $4, $1, 1, $5
           FROM header",
    )
    .bind(facts.parent)
    .bind(facts.spawning)
    .bind(facts.message)
    .bind(facts.sender)
    .bind(content)
    .execute(pool)
    .await?;
    Ok(())
}

/// Offers the next committed event and returns the decoded record.
async fn dispatch_next_kind(
    dispatcher: &OutboxDispatcher,
) -> Result<DispatchedOutboxEventKind, Box<dyn Error>> {
    let mut captured = None;
    let outcome = dispatcher
        .dispatch_next(|event| {
            captured = Some(event.kind().clone());
            OutboxDeliveryDecision::Delivered
        })
        .await?;
    captured.ok_or_else(|| format!("dispatcher offered no event, reporting {outcome:?}").into())
}

/// Every admitted update kind dispatches to the variant it names.
///
/// This is the end the tripwire analysis pointed at: `child_result` and
/// `child_lifecycle_disposition` are written by production code, and an arm
/// that cannot decode its own committed row stalls its consumer cursor for
/// every session rather than losing one event.
///
/// Both spawn policies are planted, because `background` and `bound` take
/// different arms of the same decode and only the bound arm reads the two
/// parent-state action columns.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn every_admitted_update_kind_dispatches_to_its_variant() -> Result<(), Box<dyn Error>> {
    let (container, pool) = migrated_postgres().await?;
    let parent = Uuid::from_u128(DISPATCH_SESSION);
    CreateSessionRepository::new(pool.clone(), credential_pin())
        .handle(creation(DISPATCH_SESSION, DISPATCH_CREATE_COMMAND))
        .await?;

    sqlx::query("ALTER TABLE delegation_update_outbox_event DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE session_message_delivery DISABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let facts = RelationshipFacts {
        parent,
        spawning: Uuid::from_u128(SPAWNED_REQUEST_SEED),
        child: Uuid::from_u128(ARBITRARY_CHILD_SESSION_SEED),
        awaiting: Uuid::from_u128(ARBITRARY_AWAITING_REQUEST_SEED),
        turn: Uuid::from_u128(ARBITRARY_TURN_SEED),
        command: Uuid::from_u128(ARBITRARY_COMMAND_SEED),
        message: Uuid::from_u128(ARBITRARY_MESSAGE_SEED),
        sender: Uuid::from_u128(ARBITRARY_SENDER_SESSION_SEED),
    };
    let bound = facts.spawned_by(Uuid::from_u128(BOUND_SPAWN_REQUEST_SEED));
    let waiting = facts.spawned_by(Uuid::from_u128(WAITING_REQUEST_SEED));
    let lifecycle = facts.spawned_by(Uuid::from_u128(LIFECYCLE_REQUEST_SEED));
    let goal_lifecycle = facts.spawned_by(Uuid::from_u128(GOAL_LIFECYCLE_REQUEST_SEED));
    let result = facts.spawned_by(Uuid::from_u128(RESULT_REQUEST_SEED));
    let messaged = facts.spawned_by(Uuid::from_u128(MESSAGE_REQUEST_SEED));

    plant_child_spawned_background(&pool, facts).await?;
    plant_child_spawned_bound(&pool, bound).await?;
    plant_child_waiting(&pool, waiting).await?;
    plant_child_lifecycle_disposition(&pool, lifecycle).await?;
    plant_child_lifecycle_goal_disposition(&pool, goal_lifecycle, ARBITRARY_GOAL_GENERATION)
        .await?;
    plant_child_result(&pool, result, RESULT_CONTENT).await?;
    plant_session_message(&pool, messaged, MESSAGE_CONTENT).await?;
    // The decoder reads `delivery_sequence` through a join, so the message kind
    // needs its delivery row; the other kinds read only their own columns.
    sqlx::query(
        "INSERT INTO session_message_delivery
            (message_id, spawning_tool_request_id, recipient_session_id,
             delivery_sequence, delivery_kind)
         VALUES ($1, $2, $3, 1, 'message')",
    )
    .bind(messaged.message)
    .bind(messaged.spawning)
    .bind(parent)
    .execute(&pool)
    .await?;

    sqlx::query("ALTER TABLE session_message_delivery ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;
    sqlx::query("ALTER TABLE delegation_update_outbox_event ENABLE TRIGGER ALL")
        .execute(&pool)
        .await?;

    let dispatcher = OutboxDispatcher::new(pool.clone());
    assert_eq!(
        dispatch_next_kind(&dispatcher).await?,
        DispatchedOutboxEventKind::SessionCreated(DispatchedSessionCreation {
            cause: SessionCreationCause::Interactive,
            ownership: SessionOwnership::Unmonitored,
        }),
        "the session's own creation event is committed ahead of the planted updates"
    );
    assert_eq!(
        dispatch_next_kind(&dispatcher).await?,
        DispatchedOutboxEventKind::DelegationUpdate(DispatchedDelegationUpdate::ChildSpawned {
            spawning_request: ToolRequestId::from_uuid(facts.spawning),
            child: SessionId::from_uuid(facts.child),
            policy: DispatchedDelegationPolicy::Background,
        })
    );
    assert_eq!(
        dispatch_next_kind(&dispatcher).await?,
        DispatchedOutboxEventKind::DelegationUpdate(DispatchedDelegationUpdate::ChildSpawned {
            spawning_request: ToolRequestId::from_uuid(bound.spawning),
            child: SessionId::from_uuid(bound.child),
            policy: DispatchedDelegationPolicy::Bound {
                on_parent_stopped: DispatchedBoundChildAction::Stop,
                on_parent_cancelled: DispatchedBoundChildAction::Cancel,
            },
        })
    );
    assert_eq!(
        dispatch_next_kind(&dispatcher).await?,
        DispatchedOutboxEventKind::DelegationUpdate(DispatchedDelegationUpdate::ChildWaiting {
            spawning_request: ToolRequestId::from_uuid(waiting.spawning),
            child: SessionId::from_uuid(waiting.child),
            awaiting_request: ToolRequestId::from_uuid(waiting.awaiting),
            mode: DispatchedDelegationWaitMode::Foreground,
        })
    );
    assert_eq!(
        dispatch_next_kind(&dispatcher).await?,
        DispatchedOutboxEventKind::DelegationUpdate(
            DispatchedDelegationUpdate::ChildLifecycleDisposition {
                spawning_request: ToolRequestId::from_uuid(lifecycle.spawning),
                child: SessionId::from_uuid(lifecycle.child),
                event_ordinal: ARBITRARY_ORDINAL,
                outcome: DispatchedDelegationOutcome::ChildStopped,
                reason: DispatchedDelegationReason::ParentStoppedWithDescendants,
                provenance: DispatchedDelegationProvenance::ParentTurnCommand {
                    session: SessionId::from_uuid(parent),
                    turn: TurnId::from_uuid(lifecycle.turn),
                    command: DurableCommandId::from_uuid(lifecycle.command),
                },
            }
        )
    );
    assert_eq!(
        dispatch_next_kind(&dispatcher).await?,
        DispatchedOutboxEventKind::DelegationUpdate(
            DispatchedDelegationUpdate::ChildLifecycleDisposition {
                spawning_request: ToolRequestId::from_uuid(goal_lifecycle.spawning),
                child: SessionId::from_uuid(goal_lifecycle.child),
                event_ordinal: ARBITRARY_ORDINAL,
                outcome: DispatchedDelegationOutcome::ChildCancelled,
                reason: DispatchedDelegationReason::ParentCancelledWithDescendants,
                provenance: DispatchedDelegationProvenance::ParentGoalCommand {
                    session: SessionId::from_uuid(parent),
                    goal_generation: ARBITRARY_GOAL_GENERATION,
                    command: DurableCommandId::from_uuid(goal_lifecycle.command),
                },
            }
        )
    );
    assert_eq!(
        dispatch_next_kind(&dispatcher).await?,
        DispatchedOutboxEventKind::DelegationUpdate(DispatchedDelegationUpdate::ChildResult {
            spawning_request: ToolRequestId::from_uuid(result.spawning),
            child: SessionId::from_uuid(result.child),
            outcome: DispatchedDelegationOutcome::ResultReturned,
            reason: DispatchedDelegationReason::ChildCompleted,
            provenance: DispatchedDelegationProvenance::ChildTurn {
                session: SessionId::from_uuid(result.child),
                turn: TurnId::from_uuid(result.turn),
            },
            content: Some(RESULT_CONTENT.to_owned()),
        })
    );
    assert_eq!(
        dispatch_next_kind(&dispatcher).await?,
        DispatchedOutboxEventKind::DelegationUpdate(DispatchedDelegationUpdate::SessionMessage {
            spawning_request: ToolRequestId::from_uuid(messaged.spawning),
            message: DelegationMessageId::from_uuid(messaged.message),
            sender: SessionId::from_uuid(messaged.sender),
            recipient: SessionId::from_uuid(parent),
            message_ordinal: ARBITRARY_ORDINAL,
            delivery_sequence: ARBITRARY_ORDINAL,
            content: MESSAGE_CONTENT.to_owned(),
        })
    );

    pool.close().await;
    drop(container);
    Ok(())
}

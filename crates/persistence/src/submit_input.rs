//! Atomic PostgreSQL persistence and replay for durable input acceptance.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{SubmitInputOutcome, SubmitInputTransaction};
use signalbox_domain::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueuePriority, AcceptedInputSchedulingProjection,
    AcceptedInputSchedulingReconstitutionFailure, AcceptedInputSchedulingReconstitutionInput,
    AcceptedInputStartingLineage, AcceptedInputTurnSchedulingRecord,
    AcceptedInputTurnSchedulingRecordState, ActiveTurnSchedulingReconstitutionInput, Actor,
    AppliedInterruptCommandResult, AssistantText, CancellationStopDisposition,
    CancelledModelCallTurnIdentities, CancelledTurnExecutionReconstitutionInput,
    ConsumedSteeringReconstitutionInput, ContextFrontierId, DeliveryRequest, DirectModelSelection,
    DurableCommandId, FailedTurnExecutionReconstitutionInput, FrozenAliasDefinition,
    FrozenModelSelection, ModelAlias, ModelCallDisposition, ModelCallId, ModelCallInterruptOutcome,
    ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelCallTerminalOutcome,
    ModelSelectionOverride, ModelSelectionRequest, NonEmptyUnicodeTextFailure, OriginConfiguration,
    PerInputConfigurationChoices, PinnedProviderTargetReconstitutionInput, PreparedSubmitInput,
    ProviderModelIdentity, ReconstitutedSubmitInput, ResolvedContextFrontierReconstitutionInput,
    ResolvedProviderTarget, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload as InitialSemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, Session,
    SessionAcceptanceTailEntryReconstitutionInput, SessionAcceptanceTailReconstitutionInput,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SessionInputPosition, SteeringBinding, SteeringReclassificationReason, SubmitInput,
    SubmitInputAppliedResult, SubmitInputPreparationFailure, SubmitInputReconstitutionFailure,
    SubmitInputReconstitutionInput, SubmitInputRejectedResult, SubmitInputResult,
    SubmitInputTerminalSourceReconstitutionInput, SubmitInputTurnOriginReconstitutionInput,
    TerminalAttemptEndReconstitutionInput, ToolRequestId, TurnAttemptId, TurnId,
    UnstoppedAttemptDisposition, UserContent,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CommandKind, RegistryCorruption, RegistryInspectionError, SUBMIT_INPUT_KIND,
    },
    mapping::{
        PositiveOrdinalMappingError, accepted_input_id_from_uuid, accepted_input_id_to_uuid,
        defaults_version_from_numeric, defaults_version_to_numeric, durable_command_id_from_uuid,
        durable_command_id_to_uuid, input_position_from_numeric, input_position_to_numeric,
        session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
    },
    model_execution::{
        ModelCallRepositoryError, attach_interrupt_reclassification_candidates,
        attach_recovery_interrupt_reclassification_candidates, persist_stop_requested,
        persist_terminal_outcome, require_live_execution_for_restart,
    },
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
};

const STORAGE_VERSION: i16 = 1;
const APPLIED: &str = "applied";
const REJECTED: &str = "rejected";

pub(crate) type StoredTurnOriginKey = (Uuid, Uuid);

#[derive(Clone, Copy)]
struct StoredTurnOriginLink {
    command_id: DurableCommandId,
    kind: StoredTurnOriginKind,
    accepted_input: AcceptedInputId,
    queue_order: AcceptedInputQueueOrder,
}

#[derive(Clone, Copy)]
enum StoredTurnOriginKind {
    Direct {
        predecessor: Option<StoredTurnOriginKey>,
    },
    Reclassified {
        source: StoredTurnOriginKey,
        source_disposition: StoredTerminalTurnDisposition,
    },
}

impl StoredTurnOriginKind {
    const fn dependency(self) -> Option<StoredTurnOriginKey> {
        match self {
            Self::Direct { predecessor } => predecessor,
            Self::Reclassified { source, .. } => Some(source),
        }
    }
}

#[derive(Clone, Copy)]
enum StoredTerminalTurnDisposition {
    Completed,
    Refused,
    Failed,
    Cancelled {
        interrupt_command: DurableCommandId,
    },
    ReconciliationRequired {
        interrupt_command: DurableCommandId,
        ambiguous_call: ModelCallId,
    },
}

impl StoredTerminalTurnDisposition {
    const fn unstopped_domain(self) -> Option<signalbox_domain::TurnDisposition> {
        match self {
            Self::Completed => Some(signalbox_domain::TurnDisposition::Completed),
            Self::Refused => Some(signalbox_domain::TurnDisposition::Refused),
            Self::Failed => Some(signalbox_domain::TurnDisposition::Failed),
            Self::Cancelled { .. } | Self::ReconciliationRequired { .. } => None,
        }
    }
}

fn turn_origin_dependency_order(
    relationships: impl IntoIterator<Item = (StoredTurnOriginKey, Option<StoredTurnOriginKey>)>,
) -> Option<Vec<StoredTurnOriginKey>> {
    let mut ready = VecDeque::new();
    let mut dependents: BTreeMap<StoredTurnOriginKey, Vec<StoredTurnOriginKey>> = BTreeMap::new();
    let mut relationship_count = 0;
    for (turn, predecessor) in relationships {
        relationship_count += 1;
        if let Some(predecessor) = predecessor {
            dependents.entry(predecessor).or_default().push(turn);
        } else {
            ready.push_back(turn);
        }
    }

    let mut ordered = Vec::with_capacity(relationship_count);
    while let Some(turn) = ready.pop_front() {
        ordered.push(turn);
        if let Some(newly_ready) = dependents.remove(&turn) {
            ready.extend(newly_ready);
        }
    }
    (ordered.len() == relationship_count).then_some(ordered)
}

#[cfg(test)]
mod tests {
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
}

/// The committed outcome of handling one canonical input submission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputHandlingOutcome {
    /// First handling or equal replay returns the complete recorded result.
    Recorded(SubmitInputResult),
    /// The identifier already names another kind or structural payload.
    ConflictingReuse {
        /// The owner-global identifier whose earlier meaning is retained.
        command_id: DurableCommandId,
    },
}

/// A durable shape that cannot reconstruct one complete input handling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// Typed records or variant-specific fields disagree.
    Inconsistent(&'static str),
    /// A stored positive ordinal cannot construct the domain value.
    InvalidOrdinal {
        /// The ordinal-bearing field.
        field: &'static str,
        /// Why its numeric representation is invalid.
        reason: PositiveOrdinalMappingError,
    },
    /// Exact stored text cannot construct baseline user content.
    InvalidContent {
        /// The content-bearing field.
        field: &'static str,
        /// Why the exact stored text is outside the baseline.
        failure: NonEmptyUnicodeTextFailure,
    },
    /// The current session projection required for first handling is invalid.
    CurrentSession(SessionCorruption),
    /// Checked stored values fail domain-owned receipt correlation.
    Domain(SubmitInputReconstitutionFailure),
    /// Complete scheduling facts fail domain-owned aggregate reconstruction.
    Scheduling(AcceptedInputSchedulingReconstitutionFailure),
}

impl fmt::Display for SubmitInputCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing durable SubmitInput {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported SubmitInput {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent SubmitInput {relationship}")
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(formatter, "invalid SubmitInput {field}: {reason}")
            }
            Self::InvalidContent { field, failure } => {
                write!(formatter, "invalid SubmitInput {field}: {failure:?}")
            }
            Self::CurrentSession(error) => {
                write!(formatter, "SubmitInput current Session is invalid: {error}")
            }
            Self::Domain(failure) => {
                write!(
                    formatter,
                    "SubmitInput domain reconstitution failed: {failure:?}"
                )
            }
            Self::Scheduling(failure) => {
                write!(
                    formatter,
                    "SubmitInput scheduling reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for SubmitInputCorruption {}

/// A database failure, wrong purpose-specific load, or integrity failure.
#[derive(Debug)]
pub enum SubmitInputRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database(sqlx::Error),
    /// A purpose-specific load named a valid command of another admitted kind.
    DifferentCommandKind {
        /// The owner-global identifier that names another kind.
        command_id: DurableCommandId,
    },
    /// A generated accepted-input candidate reused the active turn's origin.
    AcceptedInputIdentityCollision {
        /// The unclaimed durable command.
        command_id: DurableCommandId,
        /// The authoritative active turn.
        active_turn: TurnId,
        /// The colliding accepted-input candidate and active origin.
        accepted_input: AcceptedInputId,
    },
    /// Durable records cannot reconstruct the requested domain value.
    Corruption(SubmitInputCorruption),
    /// The active turn's model-execution aggregate could not apply or persist
    /// the correlated stop transition.
    ModelExecution(Box<ModelCallRepositoryError>),
}

impl fmt::Display for SubmitInputRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "SubmitInput database failure: {error}"),
            Self::DifferentCommandKind { command_id } => {
                write!(
                    formatter,
                    "durable command {command_id:?} does not name SubmitInput"
                )
            }
            Self::AcceptedInputIdentityCollision {
                command_id,
                active_turn,
                accepted_input,
            } => write!(
                formatter,
                "SubmitInput command {command_id:?} proposed accepted input {accepted_input:?}, which is already the origin of active turn {active_turn:?}"
            ),
            Self::Corruption(error) => error.fmt(formatter),
            Self::ModelExecution(error) => {
                write!(formatter, "SubmitInput model execution failed: {error}")
            }
        }
    }
}

impl Error for SubmitInputRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::DifferentCommandKind { .. } | Self::AcceptedInputIdentityCollision { .. } => None,
            Self::Corruption(error) => Some(error),
            Self::ModelExecution(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for SubmitInputRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SubmitInputCorruption> for SubmitInputRepositoryError {
    fn from(error: SubmitInputCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<ModelCallRepositoryError> for SubmitInputRepositoryError {
    fn from(error: ModelCallRepositoryError) -> Self {
        Self::ModelExecution(Box::new(error))
    }
}

enum TransactionDecision {
    Commit(SubmitInputHandlingOutcome),
    Rollback(SubmitInputHandlingOutcome),
}

struct PreparedAgainstLockedState {
    prepared: PreparedSubmitInput,
    scheduling: Option<AcceptedInputSchedulingProjection>,
}

/// PostgreSQL implementation of atomic durable input acceptance.
#[derive(Clone, Debug)]
pub struct SubmitInputRepository {
    pool: PgPool,
}

impl SubmitInputRepository {
    /// Uses the supplied pool for atomic handling and fail-closed loads.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Handles an unseen command or resolves its immutable recorded meaning.
    ///
    /// Registry inspection or claim is always first. An unseen command then
    /// locks the session and its current-defaults pointer before reading
    /// state, serializes position assignment on the session row, and commits
    /// the typed terminal result with all applied effects.
    pub async fn handle_with_candidates<NextTurn>(
        &self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<SubmitInputHandlingOutcome, SubmitInputRepositoryError>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let decision = handle_in_transaction(
            &mut transaction,
            command,
            accepted_input,
            turn,
            cancellation_identities,
            next_reclassified_turn,
        )
        .await;

        match decision {
            Ok(TransactionDecision::Commit(outcome)) => {
                transaction.commit().await?;
                Ok(outcome)
            }
            Ok(TransactionDecision::Rollback(outcome)) => {
                transaction.rollback().await?;
                Ok(outcome)
            }
            Err(error) => {
                if let Err(rollback_error) = transaction.rollback().await {
                    return Err(rollback_error.into());
                }
                Err(error)
            }
        }
    }

    /// Loads one complete handling, or `None` only for an unseen identifier.
    pub async fn load(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::SubmitInput) => {
                load_from_connection(&mut connection, command_id).await
            }
            Some(CommandKind::CreateSession | CommandKind::ReplaceSessionDefaults) => {
                Err(Self::wrong_kind(command_id))
            }
        }
    }

    fn wrong_kind(command_id: DurableCommandId) -> SubmitInputRepositoryError {
        SubmitInputRepositoryError::DifferentCommandKind { command_id }
    }
}

impl SubmitInputTransaction for SubmitInputRepository {
    type Error = SubmitInputRepositoryError;

    async fn handle<NextTurn>(
        &mut self,
        command: SubmitInput,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        cancellation_identities: CancelledModelCallTurnIdentities,
        next_reclassified_turn: NextTurn,
    ) -> Result<SubmitInputOutcome, Self::Error>
    where
        NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
    {
        let outcome = SubmitInputRepository::handle_with_candidates(
            self,
            command,
            accepted_input,
            turn,
            cancellation_identities,
            next_reclassified_turn,
        )
        .await?;

        Ok(match outcome {
            SubmitInputHandlingOutcome::Recorded(result) => SubmitInputOutcome::Recorded(result),
            SubmitInputHandlingOutcome::ConflictingReuse { command_id } => {
                SubmitInputOutcome::ConflictingReuse { command_id }
            }
        })
    }
}

async fn handle_in_transaction<NextTurn>(
    connection: &mut PgConnection,
    command: SubmitInput,
    accepted_input: AcceptedInputId,
    turn: Option<TurnId>,
    cancellation_identities: CancelledModelCallTurnIdentities,
    mut next_reclassified_turn: NextTurn,
) -> Result<TransactionDecision, SubmitInputRepositoryError>
where
    NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
{
    let command_id = command.command_id();
    match inspect_registry(connection, command_id).await? {
        Some(CommandKind::SubmitInput) => {
            return Ok(TransactionDecision::Rollback(existing_outcome(
                &command,
                require_recorded(connection, command_id).await?,
            )));
        }
        Some(CommandKind::CreateSession | CommandKind::ReplaceSessionDefaults) => {
            return Ok(TransactionDecision::Rollback(
                SubmitInputHandlingOutcome::ConflictingReuse { command_id },
            ));
        }
        None => {}
    }

    let claimed = sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, $2, $3, transaction_timestamp())
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .bind(SUBMIT_INPUT_KIND)
    .bind(STORAGE_VERSION)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        == 1;

    if !claimed {
        return match inspect_registry(connection, command_id).await? {
            Some(CommandKind::SubmitInput) => Ok(TransactionDecision::Rollback(existing_outcome(
                &command,
                require_recorded(connection, command_id).await?,
            ))),
            Some(CommandKind::CreateSession | CommandKind::ReplaceSessionDefaults) => {
                Ok(TransactionDecision::Rollback(
                    SubmitInputHandlingOutcome::ConflictingReuse { command_id },
                ))
            }
            None => Err(SubmitInputCorruption::Inconsistent("winner claim disappeared").into()),
        };
    }

    let PreparedAgainstLockedState {
        prepared,
        scheduling,
    } = prepare_against_locked_state(connection, command, accepted_input, turn).await?;
    let recorded = prepared.result().clone();
    let interrupt = match prepared.result() {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) => {
            origin.applied_interrupt().copied()
        }
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        | SubmitInputResult::Rejected(_) => None,
    };
    let interrupt_outcome = if let Some(interrupt) = interrupt {
        let recovery_wait = scheduling
            .as_ref()
            .and_then(AcceptedInputSchedulingProjection::active_turn_execution)
            .is_some_and(|active| {
                matches!(
                    active.phase(),
                    signalbox_domain::ActiveTurnPhase::AwaitingRecoveryDecision {
                        applied_interrupt: None,
                        ..
                    }
                )
            });
        if recovery_wait {
            let scheduling = scheduling.ok_or(SubmitInputCorruption::Inconsistent(
                "applied interrupt lacks active scheduling state",
            ))?;
            let active_turn =
                scheduling
                    .active_turn_execution()
                    .ok_or(SubmitInputCorruption::Inconsistent(
                        "applied interrupt lacks active turn execution",
                    ))?;
            let identities = attach_recovery_interrupt_reclassification_candidates(
                cancellation_identities,
                &active_turn,
                &mut next_reclassified_turn,
            )?;
            Some(ModelCallInterruptOutcome::ReconciliationRequired(
                scheduling
                    .apply_interrupt_to_model_call_recovery(interrupt, identities.into_ambiguous())
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent(
                            "applied interrupt does not match model-call recovery wait",
                        )
                    })?,
            ))
        } else {
            let execution =
                require_live_execution_for_restart(connection, interrupt.session()).await?;
            let identities = attach_interrupt_reclassification_candidates(
                cancellation_identities,
                &execution,
                &mut next_reclassified_turn,
            )?;
            Some(
                execution
                    .apply_interrupt(interrupt, identities)
                    .map_err(|_| {
                        SubmitInputCorruption::Inconsistent(
                            "applied interrupt does not match active model execution",
                        )
                    })?,
            )
        }
    } else {
        None
    };
    insert_prepared(connection, prepared).await?;
    match interrupt_outcome {
        Some(ModelCallInterruptOutcome::Cancelled(cancelled)) => {
            persist_terminal_outcome(connection, &ModelCallTerminalOutcome::Cancelled(cancelled))
                .await?;
        }
        Some(ModelCallInterruptOutcome::CancellationRequested(stopped)) => {
            persist_stop_requested(connection, &stopped).await?;
        }
        Some(ModelCallInterruptOutcome::ReconciliationRequired(reconciliation)) => {
            persist_terminal_outcome(
                connection,
                &ModelCallTerminalOutcome::ReconciliationRequired(reconciliation),
            )
            .await?;
        }
        None => {}
    }
    Ok(TransactionDecision::Commit(
        SubmitInputHandlingOutcome::Recorded(recorded),
    ))
}

async fn require_recorded(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<ReconstitutedSubmitInput, SubmitInputRepositoryError> {
    load_from_connection(connection, command_id)
        .await?
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("registry entry disappeared").into())
}

pub(crate) async fn require_recorded_batch(
    connection: &mut PgConnection,
    command_ids: &[DurableCommandId],
) -> Result<BTreeMap<DurableCommandId, ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
    let requested = command_ids
        .iter()
        .copied()
        .map(|command_id| (durable_command_id_to_uuid(command_id), command_id))
        .collect::<BTreeMap<_, _>>();
    let requested_uuids = requested.keys().copied().collect::<Vec<_>>();
    let rows = load_complete_rows(connection, &requested_uuids).await?;
    let mut rows_by_command = BTreeMap::new();
    let mut related_turns = BTreeSet::new();
    for row in rows {
        let command_uuid: Uuid = required(&row, "registry_command_id")?;
        if !requested.contains_key(&command_uuid) {
            return Err(
                SubmitInputCorruption::Inconsistent("unexpected batched command identity").into(),
            );
        }
        if let Some(related_turn) = related_turn_origin_key(&row)? {
            related_turns.insert(related_turn);
        }
        if rows_by_command.insert(command_uuid, row).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate batched command row").into(),
            );
        }
    }
    if rows_by_command.len() != requested.len() {
        return Err(SubmitInputCorruption::Missing("batched origin command").into());
    }

    let related_origins = load_turn_origin_graph(connection, &related_turns).await?;
    let mut recorded = BTreeMap::new();
    for (command_uuid, command_id) in requested {
        let row = rows_by_command
            .remove(&command_uuid)
            .ok_or(SubmitInputCorruption::Missing("batched origin command"))?;
        let related_turn_origin = related_turn_origin_key(&row)?
            .map(|key| {
                related_origins
                    .get(&key)
                    .cloned()
                    .ok_or(SubmitInputCorruption::Missing("related turn origin"))
            })
            .transpose()?;
        let existing_interrupt = load_existing_interrupt(connection, &row).await?;
        let reconstructed =
            decode_complete(row, command_id, related_turn_origin, existing_interrupt)?;
        if recorded.insert(command_id, reconstructed).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate batched command row").into(),
            );
        }
    }
    Ok(recorded)
}

fn existing_outcome(
    command: &SubmitInput,
    recorded: ReconstitutedSubmitInput,
) -> SubmitInputHandlingOutcome {
    if command == recorded.command() {
        SubmitInputHandlingOutcome::Recorded(recorded.result().clone())
    } else {
        SubmitInputHandlingOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    }
}

async fn prepare_against_locked_state(
    connection: &mut PgConnection,
    command: SubmitInput,
    accepted_input: AcceptedInputId,
    turn: Option<TurnId>,
) -> Result<PreparedAgainstLockedState, SubmitInputRepositoryError> {
    // Lock-mode constraint: this session-row lock must use the no-key-update
    // mode, not PostgreSQL's strongest row-lock mode. Submit orders the session row before the
    // scheduler row and current-defaults pointer row, while a concurrent
    // defaults replacement holds the pointer row (its compare-and-set) when its
    // `session_defaults_version` insert requests `FOR KEY SHARE` on this
    // session row through the non-deferrable session foreign key.
    // The stronger mode conflicts with `FOR KEY SHARE` and closes that lock-order
    // cycle into a deadlock (40P01); `FOR NO KEY UPDATE` does not conflict
    // with referential-integrity `KEY SHARE` locks while remaining
    // self-exclusive, so per-session position assignment stays serialized.
    let session_exists = sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SESSION)
        .bind(session_id_to_uuid(command.session()))
        .fetch_optional(&mut *connection)
        .await?
        .is_some();
    if !session_exists {
        return Ok(PreparedAgainstLockedState {
            prepared: command.prepare_session_not_found(),
            scheduling: None,
        });
    }

    let scheduler_exists =
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SCHEDULER)
            .bind(session_id_to_uuid(command.session()))
            .fetch_optional(&mut *connection)
            .await?
            .is_some();
    if !scheduler_exists {
        return Err(
            SubmitInputCorruption::CurrentSession(SessionCorruption::Missing("scheduler row"))
                .into(),
        );
    }

    let pointer_exists =
        sqlx::query_scalar::<_, Decimal>(crate::lock_inventory::SUBMIT_INPUT_DEFAULTS)
            .bind(session_id_to_uuid(command.session()))
            .fetch_optional(&mut *connection)
            .await?
            .is_some();
    if !pointer_exists {
        return Err(
            SubmitInputCorruption::CurrentSession(SessionCorruption::Missing(
                "current defaults pointer",
            ))
            .into(),
        );
    }

    let session = match load_session_from_connection(connection, command.session()).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(SubmitInputCorruption::Inconsistent("locked session disappeared").into());
        }
        Err(SessionRepositoryError::Database(error)) => return Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            return Err(SubmitInputCorruption::CurrentSession(error).into());
        }
    };

    let scheduling = load_scheduling_projection(connection, session.clone()).await?;
    let active_turn_id = scheduling.active_turn().map(|active| active.turn());
    let prepared = if active_turn_id.is_some() {
        command.prepare_with_active_turn(&scheduling, accepted_input, turn, |_| None)
    } else {
        let previous_position = sqlx::query_scalar::<_, Decimal>(
            "SELECT acceptance_position
               FROM accepted_input
              WHERE session_id = $1
              ORDER BY acceptance_position DESC
              LIMIT 1",
        )
        .bind(session_id_to_uuid(command.session()))
        .fetch_optional(&mut *connection)
        .await?
        .map(|value| {
            input_position_from_numeric(value).map_err(|reason| {
                SubmitInputRepositoryError::Corruption(SubmitInputCorruption::InvalidOrdinal {
                    field: "previous acceptance_position",
                    reason,
                })
            })
        })
        .transpose()?;
        command.prepare_when_no_active_turn(
            &session,
            accepted_input,
            turn,
            previous_position,
            |_| None,
        )
    };

    prepared
        .map(|prepared| PreparedAgainstLockedState {
            prepared,
            scheduling: Some(scheduling),
        })
        .map_err(|error| match error.failure() {
            SubmitInputPreparationFailure::SessionMismatch { .. } => {
                SubmitInputCorruption::Inconsistent("current session ownership").into()
            }
            SubmitInputPreparationFailure::TurnCandidateMismatch => {
                SubmitInputCorruption::Inconsistent("delivery turn candidate").into()
            }
            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                active_turn,
                accepted_input,
            } => SubmitInputRepositoryError::AcceptedInputIdentityCollision {
                command_id: error.command().command_id(),
                active_turn,
                accepted_input,
            },
            SubmitInputPreparationFailure::ActiveTurnProjectionMissing => {
                SubmitInputCorruption::Inconsistent("selected active scheduling state").into()
            }
            SubmitInputPreparationFailure::InterruptQueueOrderInvalid => {
                SubmitInputCorruption::Inconsistent("interrupt queue order").into()
            }
        })
}

pub(crate) async fn load_scheduling_projection(
    connection: &mut PgConnection,
    session: Session,
) -> Result<AcceptedInputSchedulingProjection, SubmitInputRepositoryError> {
    let session_id = session.id();
    let (queue_count, lifecycle_count): (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*)
               FROM queued_input_origin
              WHERE session_id = $1),
            (SELECT count(*)
               FROM turn_lifecycle
              WHERE session_id = $1)",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_one(&mut *connection)
    .await?;
    if queue_count != lifecycle_count {
        return Err(
            SubmitInputCorruption::Inconsistent("complete scheduling turn inventory").into(),
        );
    }

    let rows = sqlx::query(
        "SELECT
            queued.turn_id AS queued_turn_id,
            queued.accepted_input_id AS queued_accepted_input_id,
            queued.session_id AS queued_session_id,
            queued.acceptance_position AS queued_position,
            queued.priority_kind,
            queued.interrupt_predecessor_turn_id,
            accepted.accepting_command_id,
            accepted.accepted_input_id,
            accepted.session_id AS accepted_session_id,
            accepted.disposition_kind,
            accepted.origin_turn_id,
            accepted.expected_active_turn_id AS accepted_source_turn_id,
            queued.source_configuration_turn_id,
            (
                queued.defaults_version IS NULL
                AND queued.requested_model_kind IS NULL
                AND queued.requested_direct_model_selection_id IS NULL
                AND queued.requested_model_alias_id IS NULL
                AND queued.frozen_model_kind IS NULL
                AND queued.frozen_direct_model_selection_id IS NULL
                AND queued.frozen_model_alias_id IS NULL
                AND queued.frozen_alias_selected_direct_id IS NULL
                AND queued.model_parameters IS NULL
                AND queued.known_provider_failure_retry IS NULL
                AND queued.model_fallback IS NULL
            ) AS queued_configuration_values_absent,
            queued.defaults_version AS queued_defaults_version,
            queued.requested_model_kind,
            queued.requested_direct_model_selection_id,
            queued.requested_model_alias_id,
            queued.frozen_model_kind,
            queued.frozen_direct_model_selection_id,
            queued.frozen_model_alias_id,
            queued.frozen_alias_selected_direct_id,
            queued.model_parameters,
            queued.known_provider_failure_retry,
            queued.model_fallback,
            turn.turn_id AS lifecycle_turn_id,
            turn.session_id AS lifecycle_session_id,
            turn.state_kind AS lifecycle_state_kind,
            turn.start_lineage_kind,
            turn.immediate_predecessor_turn_id,
            turn.starting_frontier_id,
            turn.terminal_frontier_id,
            turn.active_phase_kind,
            turn.current_attempt_id,
            turn.pinned_provider_model_identity_id,
            turn.recovery_model_call_id,
            turn.terminal_attempt_id,
            turn.terminal_model_call_id,
            turn.terminal_disposition_kind,
            (
                SELECT call.model_call_id
                  FROM model_call AS call
                 WHERE call.turn_id = turn.turn_id
                   AND call.session_id = turn.session_id
                   AND call.state_kind = 'cancellation_requested'
            ) AS stop_requested_model_call_id,
            attempt.turn_attempt_id,
            attempt.turn_id AS attempt_turn_id,
            attempt.session_id AS attempt_session_id,
            attempt.continued_from_attempt_id,
            attempt.state_kind AS attempt_state_kind,
            attempt.interrupt_command_id,
            attempt.interrupt_predecessor_turn_id AS attempt_interrupt_predecessor_turn_id,
            attempt.end_variant,
            attempt.end_disposition
         FROM queued_input_origin AS queued
         LEFT JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = queued.accepted_input_id
         LEFT JOIN turn_lifecycle AS turn
           ON turn.turn_id = queued.turn_id
         LEFT JOIN turn_attempt AS attempt
           ON attempt.turn_attempt_id = COALESCE(
                turn.current_attempt_id,
                turn.terminal_attempt_id
              )
        WHERE queued.session_id = $1
        ORDER BY queued.acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut accepting_commands = Vec::with_capacity(rows.len());
    for row in &rows {
        let command_uuid: Uuid = required(row, "accepting_command_id")?;
        accepting_commands.push(
            durable_command_id_from_uuid(command_uuid)
                .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?,
        );
    }
    let recorded_commands = require_recorded_batch(connection, &accepting_commands).await?;

    let mut turns = Vec::with_capacity(rows.len());
    let mut turn_configurations = BTreeMap::<TurnId, OriginConfiguration>::new();
    let mut pinned_target_identities = BTreeMap::new();
    let mut required_frontiers = BTreeSet::new();
    let mut required_model_calls = BTreeSet::new();
    for (row, accepting_command) in rows.into_iter().zip(accepting_commands) {
        let queued_turn = turn_id_from_uuid(required(&row, "queued_turn_id")?);
        let queued_accepted =
            accepted_input_id_from_uuid(required(&row, "queued_accepted_input_id")?);
        let queued_session = session_id_from_uuid(required(&row, "queued_session_id")?);
        let queued_position = decode_position(&row, "queued_position")?;
        let queued_order = match required::<String>(&row, "priority_kind")?.as_str() {
            "ordinary" => {
                if row
                    .try_get::<Option<Uuid>, _>("interrupt_predecessor_turn_id")?
                    .is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("ordinary queue priority").into(),
                    );
                }
                AcceptedInputQueueOrder::ordinary(queued_position)
            }
            "interrupt_immediately_after" => {
                let predecessor =
                    turn_id_from_uuid(required(&row, "interrupt_predecessor_turn_id")?);
                AcceptedInputQueueOrder::interrupt_immediately_after(queued_position, predecessor)
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "queue priority kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };

        let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
        let accepted_session = session_id_from_uuid(required(&row, "accepted_session_id")?);
        let disposition_kind: String = required(&row, "disposition_kind")?;
        let origin_turn = turn_id_from_uuid(required(&row, "origin_turn_id")?);
        let accepted_source_turn: Option<Uuid> = row.try_get("accepted_source_turn_id")?;

        let lifecycle_turn = turn_id_from_uuid(required(&row, "lifecycle_turn_id")?);
        let lifecycle_session = session_id_from_uuid(required(&row, "lifecycle_session_id")?);
        if queued_accepted != accepted_input
            || queued_turn != origin_turn
            || lifecycle_turn != queued_turn
        {
            return Err(SubmitInputCorruption::Inconsistent(
                "scheduling turn identity correlation",
            )
            .into());
        }

        let recorded = recorded_commands
            .get(&accepting_command)
            .ok_or(SubmitInputCorruption::Missing("batched origin receipt"))?;
        let (accepted_lifecycle, origin_delivery, origin_configuration, binding) =
            match (disposition_kind.as_str(), recorded.result()) {
                (
                    "origin_of",
                    SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)),
                ) if applied.accepted_input() == accepted_input
                    && applied.session() == accepted_session
                    && applied.turn() == queued_turn
                    && accepted_source_turn
                        == accepted_origin_source_turn(recorded.command().delivery())
                            .map(TurnId::into_uuid) =>
                {
                    (
                        AcceptedInputLifecycle::new(
                            accepted_input,
                            AcceptedInputDisposition::OriginOf(origin_turn),
                        ),
                        recorded.command().delivery(),
                        applied.origin_configuration().clone(),
                        None,
                    )
                }
                (
                    "reclassified_as_turn_origin",
                    SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(applied)),
                ) if applied.accepted_input() == accepted_input
                    && applied.session() == accepted_session
                    && applied.binding().source_turn().into_uuid()
                        == accepted_source_turn.ok_or(SubmitInputCorruption::Missing(
                            "reclassified source turn",
                        ))? =>
                {
                    let source_turn = applied.binding().source_turn();
                    let source_configuration =
                        turn_configurations.get(&source_turn).cloned().ok_or(
                            SubmitInputCorruption::Missing("reclassified source configuration"),
                        )?;
                    (
                        AcceptedInputLifecycle::new(
                            accepted_input,
                            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                                turn: origin_turn,
                                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
                            },
                        ),
                        recorded.command().delivery(),
                        source_configuration,
                        Some(applied.binding()),
                    )
                }
                ("origin_of" | "reclassified_as_turn_origin", _) => {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "scheduling origin command result",
                    )
                    .into());
                }
                (value, _) => {
                    return Err(SubmitInputCorruption::Unsupported {
                        field: "scheduling accepted-input disposition_kind",
                        value: value.to_owned(),
                    }
                    .into());
                }
            };
        match binding {
            Some(binding) => require_stored_inherited_configuration(&row, binding.source_turn())?,
            None => require_stored_origin_configuration(&row, &origin_configuration)?,
        }
        if turn_configurations
            .insert(queued_turn, origin_configuration.clone())
            .is_some()
        {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate scheduling configuration").into(),
            );
        }

        let state_kind: String = required(&row, "lifecycle_state_kind")?;
        let lineage_kind: Option<String> = row.try_get("start_lineage_kind")?;
        let predecessor: Option<Uuid> = row.try_get("immediate_predecessor_turn_id")?;
        let starting_frontier: Option<Uuid> = row.try_get("starting_frontier_id")?;
        let terminal_frontier: Option<Uuid> = row.try_get("terminal_frontier_id")?;
        let active_phase: Option<String> = row.try_get("active_phase_kind")?;
        let current_attempt: Option<Uuid> = row.try_get("current_attempt_id")?;
        let pinned_target: Option<Uuid> = row.try_get("pinned_provider_model_identity_id")?;
        let recovery_model_call: Option<Uuid> = row.try_get("recovery_model_call_id")?;
        let terminal_attempt: Option<Uuid> = row.try_get("terminal_attempt_id")?;
        let terminal_model_call: Option<Uuid> = row.try_get("terminal_model_call_id")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let state = match state_kind.as_str() {
            "queued" => {
                if lineage_kind.is_some()
                    || predecessor.is_some()
                    || starting_frontier.is_some()
                    || terminal_frontier.is_some()
                    || active_phase.is_some()
                    || current_attempt.is_some()
                    || recovery_model_call.is_some()
                    || terminal_attempt.is_some()
                    || terminal_model_call.is_some()
                    || terminal_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("queued scheduling lifecycle").into(),
                    );
                }
                AcceptedInputTurnSchedulingRecordState::Queued
            }
            "active" => {
                if terminal_frontier.is_some()
                    || terminal_attempt.is_some()
                    || terminal_model_call.is_some()
                    || terminal_disposition.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("active scheduling lifecycle").into(),
                    );
                }
                let attempt_id = TurnAttemptId::from_uuid(
                    current_attempt.ok_or(SubmitInputCorruption::Missing("current_attempt_id"))?,
                );
                let stored_attempt_id =
                    TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                let attempt_session = session_id_from_uuid(required(&row, "attempt_session_id")?);
                let continued_from: Option<Uuid> = row.try_get("continued_from_attempt_id")?;
                let attempt_state: String = required(&row, "attempt_state_kind")?;
                let end_variant: Option<String> = row.try_get("end_variant")?;
                let end_disposition: Option<String> = row.try_get("end_disposition")?;
                if stored_attempt_id != attempt_id
                    || attempt_turn != lifecycle_turn
                    || attempt_session != lifecycle_session
                    || continued_from.is_some()
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("active current attempt").into(),
                    );
                }
                let phase = match active_phase.as_deref() {
                    Some("running") if recovery_model_call.is_none() => {
                        if end_variant.is_some() || end_disposition.is_some() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "active live attempt end",
                            )
                            .into());
                        }
                        match attempt_state.as_str() {
                            "prepared" => ActiveTurnSchedulingReconstitutionInput::prepared(
                                lifecycle_turn,
                                attempt_id,
                            ),
                            "running" => ActiveTurnSchedulingReconstitutionInput::running(
                                lifecycle_turn,
                                attempt_id,
                            ),
                            "stop_requested" => {
                                let call = required::<Uuid>(&row, "stop_requested_model_call_id")?;
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                required_model_calls.insert(call);
                                ActiveTurnSchedulingReconstitutionInput::stop_requested(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(call),
                                    interrupt,
                                )
                            }
                            value => {
                                return Err(SubmitInputCorruption::Unsupported {
                                    field: "active attempt state_kind",
                                    value: value.to_owned(),
                                }
                                .into());
                            }
                        }
                    }
                    Some("awaiting_model_call_recovery") => {
                        let recovery_call = recovery_model_call
                            .ok_or(SubmitInputCorruption::Missing("recovery_model_call_id"))?;
                        if attempt_state != "ended" {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "model-call recovery attempt end",
                            )
                            .into());
                        }
                        required_model_calls.insert(recovery_call);
                        match (end_variant.as_deref(), end_disposition.as_deref()) {
                            (Some("without_stop"), Some("ambiguous")) => ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery(
                                lifecycle_turn,
                                attempt_id,
                                ModelCallId::from_uuid(recovery_call),
                            ),
                            (Some("without_stop"), Some("lost")) => ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_restart(
                                lifecycle_turn,
                                attempt_id,
                                ModelCallId::from_uuid(recovery_call),
                            ),
                            (Some("after_cancellation"), Some("ambiguous")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(recovery_call),
                                    interrupt,
                                )
                            }
                            (Some("after_cancellation"), Some("lost")) => {
                                let interrupt = require_applied_interrupt_from_attempt(
                                    &row,
                                    lifecycle_turn,
                                    &recorded_commands,
                                )?;
                                ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_cancellation_restart(
                                    lifecycle_turn,
                                    attempt_id,
                                    ModelCallId::from_uuid(recovery_call),
                                    interrupt,
                                )
                            }
                            (None, _) | (_, None) => {
                                return Err(SubmitInputCorruption::Missing(
                                    "model-call recovery attempt end",
                                )
                                .into());
                            }
                            (Some(value), Some(_)) => {
                                return Err(SubmitInputCorruption::Unsupported {
                                    field: "model-call recovery attempt end_variant",
                                    value: value.to_owned(),
                                }
                                .into());
                            }
                        }
                    }
                    Some(value) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "active phase kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                    None => {
                        return Err(SubmitInputCorruption::Missing("active_phase_kind").into());
                    }
                };
                let starting_frontier = starting_frontier
                    .ok_or(SubmitInputCorruption::Missing("starting_frontier_id"))?;
                required_frontiers.insert(starting_frontier);
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: decode_starting_lineage(lineage_kind, predecessor)?,
                    starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                    phase,
                }
            }
            "terminal" => {
                if active_phase.is_some()
                    || current_attempt.is_some()
                    || recovery_model_call.is_some()
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "terminal scheduling lifecycle",
                    )
                    .into());
                }
                let starting_frontier = starting_frontier
                    .ok_or(SubmitInputCorruption::Missing("starting_frontier_id"))?;
                let terminal_frontier = terminal_frontier
                    .ok_or(SubmitInputCorruption::Missing("terminal_frontier_id"))?;
                required_frontiers.insert(starting_frontier);
                required_frontiers.insert(terminal_frontier);
                let starting_lineage = decode_starting_lineage(lineage_kind, predecessor)?;
                match terminal_disposition.as_deref() {
                    Some("failed") => {
                        let terminal_execution = match (terminal_attempt, terminal_model_call) {
                            (None, None) => None,
                            (Some(terminal_attempt), terminal_call) => {
                                let stored_attempt_id =
                                    TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                                let attempt_turn =
                                    turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                                let attempt_session =
                                    session_id_from_uuid(required(&row, "attempt_session_id")?);
                                let continued_from: Option<Uuid> =
                                    row.try_get("continued_from_attempt_id")?;
                                let attempt_state: String = required(&row, "attempt_state_kind")?;
                                let end_variant: Option<String> = row.try_get("end_variant")?;
                                let end_disposition: Option<String> =
                                    row.try_get("end_disposition")?;
                                if stored_attempt_id.into_uuid() != terminal_attempt
                                    || attempt_turn != lifecycle_turn
                                    || attempt_session != lifecycle_session
                                    || continued_from.is_some()
                                    || attempt_state != "ended"
                                {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "failed terminal attempt",
                                    )
                                    .into());
                                }
                                let ended_call = terminal_call.map(|call| {
                                    required_model_calls.insert(call);
                                    ModelCallId::from_uuid(call)
                                });
                                Some(
                                    match (
                                        end_variant.as_deref(),
                                        end_disposition.as_deref(),
                                        ended_call,
                                    ) {
                                        (
                                            Some("without_stop"),
                                            Some("known_failure"),
                                            Some(call),
                                        ) => FailedTurnExecutionReconstitutionInput::with_call(
                                            lifecycle_turn,
                                            stored_attempt_id,
                                            UnstoppedAttemptDisposition::KnownFailure,
                                            call,
                                        ),
                                        (Some("without_stop"), Some("known_failure"), None) => {
                                            FailedTurnExecutionReconstitutionInput::attempt_only(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                UnstoppedAttemptDisposition::KnownFailure,
                                            )
                                        }
                                        (Some("without_stop"), Some("lost"), Some(call)) => {
                                            FailedTurnExecutionReconstitutionInput::with_call(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                UnstoppedAttemptDisposition::Lost,
                                                call,
                                            )
                                        }
                                        (Some("without_stop"), Some("lost"), None) => {
                                            FailedTurnExecutionReconstitutionInput::attempt_only(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                UnstoppedAttemptDisposition::Lost,
                                            )
                                        }
                                        (
                                            Some("after_cancellation"),
                                            Some("known_failure"),
                                            ended_call,
                                        ) => {
                                            let interrupt = require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?;
                                            match ended_call {
                                            Some(call) => FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::KnownFailure,
                                                interrupt,
                                                call,
                                            ),
                                            None => FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::KnownFailure,
                                                interrupt,
                                            ),
                                        }
                                        }
                                        (Some("after_cancellation"), Some("lost"), ended_call) => {
                                            let interrupt = require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?;
                                            match ended_call {
                                            Some(call) => FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::Lost,
                                                interrupt,
                                                call,
                                            ),
                                            None => FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                                                lifecycle_turn,
                                                stored_attempt_id,
                                                CancellationStopDisposition::Lost,
                                                interrupt,
                                            ),
                                        }
                                        }
                                        _ => {
                                            return Err(SubmitInputCorruption::Inconsistent(
                                                "failed terminal attempt disposition",
                                            )
                                            .into());
                                        }
                                    },
                                )
                            }
                            (None, Some(_)) => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "failed terminal call without attempt",
                                )
                                .into());
                            }
                        };
                        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                            starting_lineage,
                            starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                            terminal_execution,
                            terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                        }
                    }
                    Some("cancelled") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let continued_from: Option<Uuid> =
                            row.try_get("continued_from_attempt_id")?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || continued_from.is_some()
                            || attempt_state != "ended"
                            || end_variant.as_deref() != Some("after_cancellation")
                            || end_disposition.as_deref() != Some("cancelled")
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "cancelled terminal attempt",
                            )
                            .into());
                        }
                        let interrupt = require_applied_interrupt_from_attempt(
                            &row,
                            lifecycle_turn,
                            &recorded_commands,
                        )?;
                        let ended_call = terminal_model_call.map(ModelCallId::from_uuid);
                        if let Some(call) = terminal_model_call {
                            required_model_calls.insert(call);
                        }
                        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                            starting_lineage,
                            starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                            terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                                lifecycle_turn,
                                stored_attempt_id,
                                TerminalAttemptEndReconstitutionInput::after_cancellation(
                                    CancellationStopDisposition::Cancelled,
                                    interrupt,
                                ),
                                ended_call,
                                interrupt,
                            ),
                            terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                        }
                    }
                    Some("reconciliation_required") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let terminal_call = terminal_model_call
                            .ok_or(SubmitInputCorruption::Missing("terminal_model_call_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let continued_from: Option<Uuid> =
                            row.try_get("continued_from_attempt_id")?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || continued_from.is_some()
                            || attempt_state != "ended"
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "reconciliation terminal attempt",
                            )
                            .into());
                        }
                        let interrupt = match end_variant.as_deref() {
                            Some("after_cancellation") => require_applied_interrupt_from_attempt(
                                &row,
                                lifecycle_turn,
                                &recorded_commands,
                            )?,
                            Some("without_stop") => require_applied_interrupt_for_turn(
                                lifecycle_turn,
                                &recorded_commands,
                            )?,
                            Some(value) => {
                                return Err(SubmitInputCorruption::Unsupported {
                                    field: "reconciliation attempt end_variant",
                                    value: value.to_owned(),
                                }
                                .into());
                            }
                            None => {
                                return Err(SubmitInputCorruption::Missing(
                                    "reconciliation attempt end_variant",
                                )
                                .into());
                            }
                        };
                        let reconciling_attempt_end =
                            match (end_variant.as_deref(), end_disposition.as_deref()) {
                                (Some("without_stop"), Some("ambiguous")) => {
                                    TerminalAttemptEndReconstitutionInput::without_stop(
                                        UnstoppedAttemptDisposition::Ambiguous,
                                    )
                                }
                                (Some("without_stop"), Some("lost")) => {
                                    TerminalAttemptEndReconstitutionInput::without_stop(
                                        UnstoppedAttemptDisposition::Lost,
                                    )
                                }
                                (Some("after_cancellation"), Some("ambiguous")) => {
                                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                                        CancellationStopDisposition::Ambiguous,
                                        interrupt,
                                    )
                                }
                                (Some("after_cancellation"), Some("lost")) => {
                                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                                        CancellationStopDisposition::Lost,
                                        interrupt,
                                    )
                                }
                                _ => {
                                    return Err(SubmitInputCorruption::Inconsistent(
                                        "reconciliation terminal attempt disposition",
                                    )
                                    .into());
                                }
                            };
                        required_model_calls.insert(terminal_call);
                        AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                            starting_lineage,
                            starting_frontier: ContextFrontierId::from_uuid(starting_frontier),
                            reconciling_attempt: stored_attempt_id,
                            reconciling_attempt_end,
                            ambiguous_call: ModelCallId::from_uuid(terminal_call),
                            interrupt,
                            terminal_frontier: ContextFrontierId::from_uuid(terminal_frontier),
                        }
                    }
                    Some("completed" | "refused") => {
                        let terminal_attempt = terminal_attempt
                            .ok_or(SubmitInputCorruption::Missing("terminal_attempt_id"))?;
                        let terminal_call = terminal_model_call
                            .ok_or(SubmitInputCorruption::Missing("terminal_model_call_id"))?;
                        let stored_attempt_id =
                            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?);
                        let attempt_turn = turn_id_from_uuid(required(&row, "attempt_turn_id")?);
                        let attempt_session =
                            session_id_from_uuid(required(&row, "attempt_session_id")?);
                        let continued_from: Option<Uuid> =
                            row.try_get("continued_from_attempt_id")?;
                        let attempt_state: String = required(&row, "attempt_state_kind")?;
                        let end_variant: Option<String> = row.try_get("end_variant")?;
                        let end_disposition: Option<String> = row.try_get("end_disposition")?;
                        if stored_attempt_id.into_uuid() != terminal_attempt
                            || attempt_turn != lifecycle_turn
                            || attempt_session != lifecycle_session
                            || continued_from.is_some()
                            || attempt_state != "ended"
                        {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "terminal model-call attempt",
                            )
                            .into());
                        }
                        required_model_calls.insert(terminal_call);
                        match terminal_disposition.as_deref() {
                            Some("completed") => {
                                let completing_attempt_end = match (
                                    end_variant.as_deref(),
                                    end_disposition.as_deref(),
                                ) {
                                    (Some("without_stop"), Some("turn_completed")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::TurnCompleted,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::Lost,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("turn_completed")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::TurnCompleted,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::Lost,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    _ => {
                                        return Err(SubmitInputCorruption::Inconsistent(
                                            "terminal model-call disposition",
                                        )
                                        .into());
                                    }
                                };
                                AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(
                                        starting_frontier,
                                    ),
                                    completing_attempt: stored_attempt_id,
                                    completing_attempt_end,
                                    completing_call: ModelCallId::from_uuid(terminal_call),
                                    terminal_frontier: ContextFrontierId::from_uuid(
                                        terminal_frontier,
                                    ),
                                }
                            }
                            Some("refused") => {
                                let refusing_attempt_end = match (
                                    end_variant.as_deref(),
                                    end_disposition.as_deref(),
                                ) {
                                    (Some("without_stop"), Some("turn_refused")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::TurnRefused,
                                        )
                                    }
                                    (Some("without_stop"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::without_stop(
                                            UnstoppedAttemptDisposition::Lost,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("turn_refused")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::TurnRefused,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    (Some("after_cancellation"), Some("lost")) => {
                                        TerminalAttemptEndReconstitutionInput::after_cancellation(
                                            CancellationStopDisposition::Lost,
                                            require_applied_interrupt_from_attempt(
                                                &row,
                                                lifecycle_turn,
                                                &recorded_commands,
                                            )?,
                                        )
                                    }
                                    _ => {
                                        return Err(SubmitInputCorruption::Inconsistent(
                                            "terminal model-call disposition",
                                        )
                                        .into());
                                    }
                                };
                                AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                                    starting_lineage,
                                    starting_frontier: ContextFrontierId::from_uuid(
                                        starting_frontier,
                                    ),
                                    refusing_attempt: stored_attempt_id,
                                    refusing_attempt_end,
                                    refusing_call: ModelCallId::from_uuid(terminal_call),
                                    terminal_frontier: ContextFrontierId::from_uuid(
                                        terminal_frontier,
                                    ),
                                }
                            }
                            _ => {
                                return Err(SubmitInputCorruption::Inconsistent(
                                    "terminal model-call disposition",
                                )
                                .into());
                            }
                        }
                    }
                    Some(value) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "terminal disposition kind",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                    None => {
                        return Err(
                            SubmitInputCorruption::Missing("terminal_disposition_kind").into()
                        );
                    }
                }
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "turn lifecycle state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };

        if let Some(identity) = pinned_target
            && pinned_target_identities
                .insert(queued_turn, identity)
                .is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("duplicate turn target pin").into());
        }

        let record = match binding {
            Some(binding) => AcceptedInputTurnSchedulingRecord::reclassified(
                lifecycle_session,
                lifecycle_turn,
                accepted_session,
                accepted_lifecycle,
                queued_session,
                queued_turn,
                AcceptedInputQueueOrder::ordinary(queued_position),
                origin_delivery,
                binding,
                origin_configuration,
                state,
            ),
            None => AcceptedInputTurnSchedulingRecord::new(
                lifecycle_session,
                lifecycle_turn,
                accepted_session,
                accepted_lifecycle,
                queued_session,
                queued_turn,
                queued_order,
                origin_delivery,
                origin_configuration,
                state,
            ),
        };
        turns.push(record);
    }

    let active_acceptance_tail =
        load_active_acceptance_tail(connection, session_id, &turns).await?;

    let consumed_steering_rows = sqlx::query(
        "SELECT session_id, accepted_input_id, acceptance_position, expected_active_turn_id,
                consuming_model_call_id
           FROM accepted_input
          WHERE session_id = $1
            AND disposition_kind = 'consumed_as_steering'
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut consumed_steering = Vec::with_capacity(consumed_steering_rows.len());
    for row in consumed_steering_rows {
        let call = ModelCallId::from_uuid(required(&row, "consuming_model_call_id")?);
        required_model_calls.insert(call.into_uuid());
        consumed_steering.push(ConsumedSteeringReconstitutionInput::new(
            session_id_from_uuid(required(&row, "session_id")?),
            AcceptedInputLifecycle::new(
                accepted_input_id_from_uuid(required(&row, "accepted_input_id")?),
                AcceptedInputDisposition::ConsumedAsSteering { call },
            ),
            decode_position(&row, "acceptance_position")?,
            turn_id_from_uuid(required(&row, "expected_active_turn_id")?),
        ));
    }

    let required_model_call_ids = required_model_calls.iter().copied().collect::<Vec<_>>();
    let model_call_rows = sqlx::query(
        "SELECT
            model_call_id,
            turn_id,
            session_id,
            turn_attempt_id,
            selection_kind,
            direct_model_selection_id,
            frozen_model_alias_id,
            frozen_alias_selected_direct_id,
            resolved_provider_model_identity_id,
            context_frontier_id,
            state_kind,
            terminal_disposition_kind
           FROM model_call
          WHERE session_id = $1
            AND model_call_id = ANY($2)
          ORDER BY model_call_id",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_model_call_ids)
    .fetch_all(&mut *connection)
    .await?;
    let mut model_calls = Vec::with_capacity(model_call_rows.len());
    let mut pinned_targets = Vec::with_capacity(model_call_rows.len());
    let mut loaded_pinned_turns = BTreeSet::new();
    let mut loaded_model_calls = BTreeSet::new();
    for row in model_call_rows {
        let call_uuid: Uuid = required(&row, "model_call_id")?;
        if !loaded_model_calls.insert(call_uuid) {
            return Err(SubmitInputCorruption::Inconsistent("duplicate model call").into());
        }
        let frontier_uuid: Uuid = required(&row, "context_frontier_id")?;
        let turn_uuid: Uuid = required(&row, "turn_id")?;
        let turn = turn_id_from_uuid(turn_uuid);
        let pinned_identity = pinned_target_identities
            .get(&turn)
            .copied()
            .ok_or(SubmitInputCorruption::Missing("model call turn target pin"))?;
        if loaded_pinned_turns.insert(turn) {
            pinned_targets.push(PinnedProviderTargetReconstitutionInput::new(
                turn,
                ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(pinned_identity)),
            ));
        }
        required_frontiers.insert(frontier_uuid);
        let state_kind: String = required(&row, "state_kind")?;
        let terminal_disposition: Option<String> = row.try_get("terminal_disposition_kind")?;
        let state = match (state_kind.as_str(), terminal_disposition.as_deref()) {
            ("prepared", None) => ModelCallReconstitutionState::Prepared,
            ("in_flight", None) => ModelCallReconstitutionState::InFlight,
            ("cancellation_requested", None) => ModelCallReconstitutionState::CancellationRequested,
            ("terminal", Some(disposition)) => {
                ModelCallReconstitutionState::Terminal(decode_model_call_disposition(disposition)?)
            }
            ("prepared" | "in_flight" | "cancellation_requested" | "terminal", _) => {
                return Err(SubmitInputCorruption::Inconsistent("model call state payload").into());
            }
            (value, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "model call state_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        model_calls.push(ModelCallReconstitutionInput::new(
            ModelCallId::from_uuid(call_uuid),
            turn,
            TurnAttemptId::from_uuid(required(&row, "turn_attempt_id")?),
            decode_frozen_model(
                required(&row, "selection_kind")?,
                row.try_get("direct_model_selection_id")?,
                row.try_get("frozen_model_alias_id")?,
                row.try_get("frozen_alias_selected_direct_id")?,
            )?,
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(required(
                &row,
                "resolved_provider_model_identity_id",
            )?)),
            ContextFrontierId::from_uuid(frontier_uuid),
            state,
        ));
    }
    if loaded_model_calls != required_model_calls {
        return Err(SubmitInputCorruption::Missing("scheduling model call").into());
    }

    let required_frontier_ids = required_frontiers.iter().copied().collect::<Vec<_>>();
    let frontier_rows = sqlx::query(
        "SELECT context_frontier_id, member_count
           FROM context_frontier
          WHERE owning_session_id = $1
            AND context_frontier_id = ANY($2)
          ORDER BY context_frontier_id",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_frontier_ids)
    .fetch_all(&mut *connection)
    .await?;
    let member_rows = sqlx::query(
        "SELECT
            context_frontier_id,
            member_position,
            source_session_id,
            semantic_entry_id
           FROM context_frontier_member
          WHERE owning_session_id = $1
            AND context_frontier_id = ANY($2)
          ORDER BY context_frontier_id, member_position",
    )
    .bind(session_id_to_uuid(session_id))
    .bind(&required_frontier_ids)
    .fetch_all(&mut *connection)
    .await?;
    let mut members_by_frontier =
        BTreeMap::<Uuid, Vec<(Decimal, SessionId, SemanticTranscriptEntryId)>>::new();
    let mut required_semantic_entries = BTreeSet::new();
    for member_row in member_rows {
        let source_session = required(&member_row, "source_session_id")?;
        let semantic_entry = required(&member_row, "semantic_entry_id")?;
        required_semantic_entries.insert((source_session, semantic_entry));
        members_by_frontier
            .entry(required(&member_row, "context_frontier_id")?)
            .or_default()
            .push((
                required(&member_row, "member_position")?,
                session_id_from_uuid(source_session),
                SemanticTranscriptEntryId::from_uuid(semantic_entry),
            ));
    }

    let semantic_source_sessions = required_semantic_entries
        .iter()
        .map(|(source_session, _)| *source_session)
        .collect::<Vec<_>>();
    let semantic_entry_ids = required_semantic_entries
        .iter()
        .map(|(_, semantic_entry)| *semantic_entry)
        .collect::<Vec<_>>();
    let semantic_rows = sqlx::query(
        "SELECT
            source_session_id,
            semantic_entry_id,
            payload_kind,
            origin_accepted_input_id,
            steering_source_turn_id,
            failed_turn_id,
            cancelled_turn_id,
            assistant_text_value,
            producing_model_call_id,
            assistant_tool_request_id,
            completed_turn_id
         FROM semantic_transcript_entry
        WHERE source_session_id = $3
           OR (source_session_id, semantic_entry_id) IN (
            SELECT required.source_session_id, required.semantic_entry_id
              FROM UNNEST($1::uuid[], $2::uuid[])
                AS required(source_session_id, semantic_entry_id)
        )
        ORDER BY source_session_id, semantic_entry_id",
    )
    .bind(&semantic_source_sessions)
    .bind(&semantic_entry_ids)
    .bind(session_id_to_uuid(session_id))
    .fetch_all(&mut *connection)
    .await?;
    let mut semantic_entries = Vec::with_capacity(semantic_rows.len());
    let mut loaded_semantic_entries = BTreeSet::new();
    for row in semantic_rows {
        let source_session_uuid: Uuid = required(&row, "source_session_id")?;
        let entry_uuid: Uuid = required(&row, "semantic_entry_id")?;
        if !loaded_semantic_entries.insert((source_session_uuid, entry_uuid)) {
            return Err(SubmitInputCorruption::Inconsistent("duplicate semantic entry").into());
        }
        let source_session = session_id_from_uuid(source_session_uuid);
        let entry = SemanticTranscriptEntryId::from_uuid(entry_uuid);
        let payload_kind: String = required(&row, "payload_kind")?;
        let origin: Option<Uuid> = row.try_get("origin_accepted_input_id")?;
        let steering_source_turn: Option<Uuid> = row.try_get("steering_source_turn_id")?;
        let failed_turn: Option<Uuid> = row.try_get("failed_turn_id")?;
        let cancelled_turn: Option<Uuid> = row.try_get("cancelled_turn_id")?;
        let assistant_text: Option<String> = row.try_get("assistant_text_value")?;
        let producing_call: Option<Uuid> = row.try_get("producing_model_call_id")?;
        let tool_request: Option<Uuid> = row.try_get("assistant_tool_request_id")?;
        let completed_turn: Option<Uuid> = row.try_get("completed_turn_id")?;
        let payload = match (
            payload_kind.as_str(),
            origin,
            steering_source_turn,
            failed_turn,
            cancelled_turn,
            assistant_text,
            producing_call,
            tool_request,
            completed_turn,
        ) {
            ("origin_accepted_input", Some(origin), None, None, None, None, None, None, None) => {
                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: accepted_input_id_from_uuid(origin),
                }
            }
            (
                "steering_accepted_input",
                Some(accepted_input),
                Some(source_turn),
                None,
                None,
                None,
                None,
                None,
                None,
            ) => InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: accepted_input_id_from_uuid(accepted_input),
                source_turn: turn_id_from_uuid(source_turn),
            },
            ("turn_failed", None, None, Some(turn), None, None, None, None, None) => {
                InitialSemanticTranscriptEntryPayload::TurnFailed {
                    turn: turn_id_from_uuid(turn),
                }
            }
            ("turn_cancelled", None, None, None, Some(turn), None, None, None, None) => {
                InitialSemanticTranscriptEntryPayload::TurnCancelled {
                    turn: turn_id_from_uuid(turn),
                }
            }
            ("assistant_text", None, None, None, None, Some(text), Some(call), None, None) => {
                InitialSemanticTranscriptEntryPayload::AssistantText {
                    producing_call: ModelCallId::from_uuid(call),
                    value: AssistantText::try_new(text).map_err(|error| {
                        SubmitInputCorruption::InvalidContent {
                            field: "assistant_text_value",
                            failure: error.failure(),
                        }
                    })?,
                }
            }
            (
                "assistant_tool_use",
                None,
                None,
                None,
                None,
                None,
                Some(call),
                Some(request),
                None,
            ) => InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: ModelCallId::from_uuid(call),
                request: ToolRequestId::from_uuid(request),
            },
            ("turn_completed", None, None, None, None, None, None, None, Some(turn)) => {
                InitialSemanticTranscriptEntryPayload::TurnCompleted {
                    turn: turn_id_from_uuid(turn),
                }
            }
            (
                "origin_accepted_input"
                | "steering_accepted_input"
                | "turn_failed"
                | "turn_cancelled"
                | "assistant_text"
                | "assistant_tool_use"
                | "turn_completed",
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
            ) => {
                return Err(SubmitInputCorruption::Inconsistent("semantic entry payload").into());
            }
            (value, _, _, _, _, _, _, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "semantic entry payload_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
            entry,
            source_session,
            payload,
        ));
    }
    if !required_semantic_entries.is_subset(&loaded_semantic_entries) {
        return Err(SubmitInputCorruption::Missing("context frontier semantic entry").into());
    }

    let mut snapshots = Vec::with_capacity(frontier_rows.len());
    for frontier_row in frontier_rows {
        let frontier_uuid: Uuid = required(&frontier_row, "context_frontier_id")?;
        let declared_count: Decimal = required(&frontier_row, "member_count")?;
        let member_rows = members_by_frontier
            .remove(&frontier_uuid)
            .unwrap_or_default();
        #[expect(
            clippy::expect_used,
            reason = "temporary ledger site: PostgreSQL result cardinality cannot exceed the stored u64 bound on supported targets; typed conversion is commissioned by the 2026-07-20 audit"
        )]
        let actual_count = u64::try_from(member_rows.len())
            .expect("PostgreSQL result cardinality fits the u64 schema bound");
        if declared_count != Decimal::from(actual_count) {
            return Err(SubmitInputCorruption::Inconsistent(
                "context frontier declared membership",
            )
            .into());
        }
        let mut members = Vec::with_capacity(member_rows.len());
        for (index, (position, source_session, semantic_entry)) in
            member_rows.into_iter().enumerate()
        {
            #[expect(
                clippy::expect_used,
                reason = "temporary ledger site: PostgreSQL result cardinality cannot exceed the stored u64 bound on supported targets; typed conversion is commissioned by the 2026-07-20 audit"
            )]
            let expected_position = u64::try_from(index + 1)
                .expect("PostgreSQL result cardinality fits the u64 schema bound");
            if position != Decimal::from(expected_position) {
                return Err(SubmitInputCorruption::Inconsistent(
                    "context frontier contiguous membership",
                )
                .into());
            }
            members.push(SemanticTranscriptEntryRef::from_source(
                source_session,
                semantic_entry,
            ));
        }
        snapshots.push(ResolvedContextFrontierReconstitutionInput::new(
            session_id,
            ContextFrontierId::from_uuid(frontier_uuid),
            members,
        ));
    }
    if !members_by_frontier.is_empty() {
        return Err(
            SubmitInputCorruption::Inconsistent("context frontier member without header").into(),
        );
    }
    if snapshots.len() != required_frontiers.len() {
        return Err(SubmitInputCorruption::Missing("scheduling context frontier").into());
    }

    AcceptedInputSchedulingReconstitutionInput::new(
        session,
        turns,
        semantic_entries,
        snapshots,
        active_acceptance_tail,
    )
    .with_model_call_facts(pinned_targets, model_calls)
    .with_consumed_steering_facts(consumed_steering)
    .reconstitute()
    .map_err(|error| {
        let (_, failure) = error.into_parts();
        SubmitInputCorruption::Scheduling(failure).into()
    })
}

fn require_applied_interrupt_from_attempt(
    row: &PgRow,
    owning_turn: TurnId,
    recorded_commands: &BTreeMap<DurableCommandId, ReconstitutedSubmitInput>,
) -> Result<AppliedInterruptCommandResult, SubmitInputRepositoryError> {
    let command = durable_command_id_from_uuid(required(row, "interrupt_command_id")?)
        .map_err(|_| SubmitInputCorruption::Inconsistent("interrupt command identity"))?;
    let predecessor = turn_id_from_uuid(required(row, "attempt_interrupt_predecessor_turn_id")?);
    if predecessor != owning_turn {
        return Err(SubmitInputCorruption::Inconsistent("attempt interrupt predecessor").into());
    }
    let receipt = recorded_commands
        .get(&command)
        .ok_or(SubmitInputCorruption::Missing("applied interrupt command"))?;
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        return Err(
            SubmitInputCorruption::Inconsistent("interrupt command was not applied").into(),
        );
    };
    origin
        .applied_interrupt()
        .copied()
        .filter(|interrupt| {
            interrupt.proof().command() == command && interrupt.proof().predecessor() == owning_turn
        })
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("attempt interrupt authority").into())
}

fn require_applied_interrupt_for_turn(
    owning_turn: TurnId,
    recorded_commands: &BTreeMap<DurableCommandId, ReconstitutedSubmitInput>,
) -> Result<AppliedInterruptCommandResult, SubmitInputRepositoryError> {
    let mut matches = recorded_commands.values().filter_map(|receipt| {
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) =
            receipt.result()
        else {
            return None;
        };
        origin
            .applied_interrupt()
            .copied()
            .filter(|interrupt| interrupt.proof().predecessor() == owning_turn)
    });
    let interrupt = matches
        .next()
        .ok_or(SubmitInputCorruption::Missing("applied interrupt command"))?;
    if matches.next().is_some() {
        return Err(
            SubmitInputCorruption::Inconsistent("multiple applied interrupt commands").into(),
        );
    }
    Ok(interrupt)
}

fn accepted_origin_source_turn(delivery: DeliveryRequest) -> Option<TurnId> {
    match delivery {
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            ..
        }
        | DeliveryRequest::Interrupt {
            expected_active_turn,
            ..
        } => Some(expected_active_turn),
        DeliveryRequest::StartWhenNoActiveTurn { .. } | DeliveryRequest::NextSafePoint { .. } => {
            None
        }
    }
}

fn require_stored_origin_configuration(
    row: &PgRow,
    expected: &OriginConfiguration,
) -> Result<(), SubmitInputRepositoryError> {
    let source: Option<Uuid> = row.try_get("source_configuration_turn_id")?;
    if source.is_some() {
        return Err(
            SubmitInputCorruption::Inconsistent("explicit configuration source reference").into(),
        );
    }
    require_spelling(row, "model_parameters", "provider_defaults")?;
    require_spelling(row, "known_provider_failure_retry", "disabled")?;
    require_spelling(row, "model_fallback", "disabled")?;
    let defaults_version = decode_defaults_version(row, "queued_defaults_version")?;
    let requested = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "scheduling requested model",
    )?;
    let frozen = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;
    if defaults_version != expected.session_defaults_version()
        || requested != expected.requested().model()
        || frozen != *expected.effective().model()
    {
        return Err(SubmitInputCorruption::Inconsistent("scheduling origin configuration").into());
    }
    Ok(())
}

fn require_stored_inherited_configuration(
    row: &PgRow,
    expected_source: TurnId,
) -> Result<(), SubmitInputRepositoryError> {
    let source: Option<Uuid> = row.try_get("source_configuration_turn_id")?;
    let values_absent: bool = required(row, "queued_configuration_values_absent")?;
    if source != Some(expected_source.into_uuid()) || !values_absent {
        return Err(
            SubmitInputCorruption::Inconsistent("inherited configuration provenance").into(),
        );
    }
    Ok(())
}

async fn load_active_acceptance_tail(
    connection: &mut PgConnection,
    session: SessionId,
    turns: &[AcceptedInputTurnSchedulingRecord],
) -> Result<Option<SessionAcceptanceTailReconstitutionInput>, SubmitInputRepositoryError> {
    let Some(active) = turns.iter().find(|record| {
        matches!(
            record.state(),
            AcceptedInputTurnSchedulingRecordState::Active { .. }
        )
    }) else {
        return Ok(None);
    };

    let rows = sqlx::query(
        "SELECT
            accepted_input_id,
            session_id,
            acceptance_position,
            disposition_kind,
            origin_turn_id,
            consuming_model_call_id,
            delivery_kind,
            expected_active_turn_id,
            expected_defaults_version,
            model_override_kind,
            replacement_model_kind,
            replacement_direct_model_selection_id,
            replacement_model_alias_id
           FROM accepted_input
          WHERE session_id = $1
            AND acceptance_position >= $2
          ORDER BY acceptance_position",
    )
    .bind(session_id_to_uuid(session))
    .bind(input_position_to_numeric(
        active.order().acceptance_position(),
    ))
    .fetch_all(&mut *connection)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let accepted_input = accepted_input_id_from_uuid(required(&row, "accepted_input_id")?);
        let entry_session = session_id_from_uuid(required(&row, "session_id")?);
        let position = decode_position(&row, "acceptance_position")?;
        let expected_active_turn: Option<Uuid> = row.try_get("expected_active_turn_id")?;
        let delivery = decode_delivery(
            required(&row, "delivery_kind")?,
            expected_active_turn,
            row.try_get("expected_defaults_version")?,
            row.try_get("model_override_kind")?,
            row.try_get("replacement_model_kind")?,
            row.try_get("replacement_direct_model_selection_id")?,
            row.try_get("replacement_model_alias_id")?,
            "active acceptance-tail delivery",
        )?;
        let disposition_kind: String = required(&row, "disposition_kind")?;
        let origin_turn: Option<Uuid> = row.try_get("origin_turn_id")?;
        let consuming_call: Option<Uuid> = row.try_get("consuming_model_call_id")?;
        let disposition = match (
            disposition_kind.as_str(),
            origin_turn,
            consuming_call,
            delivery,
        ) {
            ("origin_of", Some(origin), None, _) => {
                AcceptedInputDisposition::OriginOf(turn_id_from_uuid(origin))
            }
            (
                "pending_steering",
                None,
                None,
                DeliveryRequest::NextSafePoint {
                    expected_active_turn,
                },
            ) => AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(expected_active_turn),
            },
            (
                "reclassified_as_turn_origin",
                Some(origin),
                None,
                DeliveryRequest::NextSafePoint { .. },
            ) => AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: turn_id_from_uuid(origin),
                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
            ("consumed_as_steering", None, Some(call), DeliveryRequest::NextSafePoint { .. }) => {
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: ModelCallId::from_uuid(call),
                }
            }
            (
                "origin_of"
                | "pending_steering"
                | "reclassified_as_turn_origin"
                | "consumed_as_steering",
                _,
                _,
                _,
            ) => {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active acceptance-tail disposition",
                )
                .into());
            }
            (value, _, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "active acceptance-tail disposition_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        entries.push(SessionAcceptanceTailEntryReconstitutionInput::new(
            entry_session,
            AcceptedInputLifecycle::new(accepted_input, disposition),
            position,
            delivery,
        ));
    }

    let observed_last_position = entries
        .last()
        .map(SessionAcceptanceTailEntryReconstitutionInput::position)
        .ok_or(SubmitInputCorruption::Missing(
            "active acceptance-tail origin",
        ))?;
    Ok(Some(SessionAcceptanceTailReconstitutionInput::new(
        session,
        active.accepted_input().id(),
        observed_last_position,
        entries,
    )))
}

fn decode_starting_lineage(
    kind: Option<String>,
    predecessor: Option<Uuid>,
) -> Result<AcceptedInputStartingLineage, SubmitInputRepositoryError> {
    match (kind.as_deref(), predecessor) {
        (Some("first_in_session"), None) => Ok(AcceptedInputStartingLineage::FirstInSession),
        (Some("after"), Some(predecessor)) => Ok(AcceptedInputStartingLineage::After {
            immediate_predecessor: turn_id_from_uuid(predecessor),
        }),
        (Some("first_in_session" | "after"), _) | (None, _) => {
            Err(SubmitInputCorruption::Inconsistent("starting lineage").into())
        }
        (Some(value), _) => Err(SubmitInputCorruption::Unsupported {
            field: "start_lineage_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn insert_prepared(
    connection: &mut PgConnection,
    prepared: PreparedSubmitInput,
) -> Result<(), SubmitInputRepositoryError> {
    let command = prepared.command();
    let actor = encode_actor(command.actor());
    let delivery = encode_delivery(command.delivery());
    let result = encode_result(prepared.result(), command.delivery());

    sqlx::query(
        "INSERT INTO submit_input_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             content_kind, content_text,
             delivery_kind, expected_active_turn_id, expected_defaults_version,
             model_override_kind, replacement_model_kind,
             replacement_direct_model_selection_id, replacement_model_alias_id,
             result_kind, rejection_kind, result_session_id,
             result_accepted_input_id, result_turn_id,
             result_actual_active_turn_id,
             result_expected_active_turn_id, result_expected_defaults_version,
             result_current_defaults_version, result_unknown_alias_id,
             result_selected_defaults_version, result_last_position,
             result_existing_interrupt_command_id)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
             $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25,
             $26, $27, $28, $29)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(SUBMIT_INPUT_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(actor.kind)
    .bind(actor.turn)
    .bind(actor.tool_request)
    .bind("text")
    .bind(command.content().text().as_str())
    .bind(delivery.kind)
    .bind(delivery.expected_active_turn)
    .bind(delivery.expected_defaults_version)
    .bind(delivery.model_override_kind)
    .bind(delivery.replacement.kind)
    .bind(delivery.replacement.direct)
    .bind(delivery.replacement.alias)
    .bind(result.kind)
    .bind(result.rejection_kind)
    .bind(session_id_to_uuid(result.session))
    .bind(result.accepted_input)
    .bind(result.turn)
    .bind(result.actual_active_turn)
    .bind(result.expected_active_turn)
    .bind(result.expected_defaults_version)
    .bind(result.current_defaults_version)
    .bind(result.unknown_alias)
    .bind(result.selected_defaults_version)
    .bind(result.last_position)
    .bind(result.existing_interrupt_command)
    .execute(&mut *connection)
    .await?;

    if let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
        prepared.result()
    {
        let origin = applied.origin_configuration();
        let requested = encode_selection(origin.requested().model());
        let frozen = encode_frozen_model(origin.effective().model());
        let position = applied.acceptance_position();
        let (priority_kind, interrupt_predecessor) = match applied.queue_order().priority() {
            AcceptedInputQueuePriority::Ordinary => ("ordinary", None),
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor } => (
                "interrupt_immediately_after",
                Some(turn_id_to_uuid(predecessor)),
            ),
        };

        sqlx::query(
            "INSERT INTO accepted_input
                (accepted_input_id, accepting_command_id, session_id,
                 content_kind, content_text, delivery_kind,
                 expected_active_turn_id, expected_defaults_version,
                 model_override_kind, replacement_model_kind,
                 replacement_direct_model_selection_id, replacement_model_alias_id,
                 acceptance_position, disposition_kind, origin_turn_id)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15)",
        )
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(durable_command_id_to_uuid(command.command_id()))
        .bind(session_id_to_uuid(applied.session()))
        .bind("text")
        .bind(command.content().text().as_str())
        .bind(delivery.kind)
        .bind(delivery.expected_active_turn)
        .bind(delivery.expected_defaults_version)
        .bind(delivery.model_override_kind)
        .bind(delivery.replacement.kind)
        .bind(delivery.replacement.direct)
        .bind(delivery.replacement.alias)
        .bind(input_position_to_numeric(position))
        .bind("origin_of")
        .bind(turn_id_to_uuid(applied.turn()))
        .execute(&mut *connection)
        .await?;

        sqlx::query(
            "INSERT INTO queued_input_origin
                (turn_id, accepted_input_id, session_id, acceptance_position,
                 priority_kind, defaults_version,
                 interrupt_predecessor_turn_id,
                 requested_model_kind, requested_direct_model_selection_id,
                 requested_model_alias_id, frozen_model_kind,
                 frozen_direct_model_selection_id, frozen_model_alias_id,
                 frozen_alias_selected_direct_id, model_parameters,
                 known_provider_failure_retry, model_fallback)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15, $16, $17)",
        )
        .bind(turn_id_to_uuid(applied.turn()))
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(input_position_to_numeric(position))
        .bind(priority_kind)
        .bind(defaults_version_to_numeric(
            origin.session_defaults_version(),
        ))
        .bind(interrupt_predecessor)
        .bind(requested.kind)
        .bind(requested.direct)
        .bind(requested.alias)
        .bind(frozen.kind)
        .bind(frozen.direct)
        .bind(frozen.alias)
        .bind(frozen.alias_selected)
        .bind("provider_defaults")
        .bind("disabled")
        .bind("disabled")
        .execute(&mut *connection)
        .await?;

        sqlx::query(
            "INSERT INTO turn_lifecycle
                (turn_id, session_id, origin_accepted_input_id,
                 acceptance_position, state_kind)
             VALUES ($1, $2, $3, $4, 'queued')",
        )
        .bind(turn_id_to_uuid(applied.turn()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(input_position_to_numeric(position))
        .execute(&mut *connection)
        .await?;
    }

    if let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(applied)) =
        prepared.result()
    {
        sqlx::query(
            "INSERT INTO accepted_input
                (accepted_input_id, accepting_command_id, session_id,
                 content_kind, content_text, delivery_kind,
                 expected_active_turn_id, expected_defaults_version,
                 model_override_kind, replacement_model_kind,
                 replacement_direct_model_selection_id, replacement_model_alias_id,
                 acceptance_position, disposition_kind, origin_turn_id)
             VALUES
                ($1, $2, $3, 'text', $4, 'next_safe_point',
                 $5, NULL, NULL, NULL, NULL, NULL, $6, 'pending_steering', NULL)",
        )
        .bind(accepted_input_id_to_uuid(applied.accepted_input()))
        .bind(durable_command_id_to_uuid(command.command_id()))
        .bind(session_id_to_uuid(applied.session()))
        .bind(command.content().text().as_str())
        .bind(turn_id_to_uuid(applied.binding().source_turn()))
        .bind(input_position_to_numeric(applied.acceptance_position()))
        .execute(&mut *connection)
        .await?;
    }

    Ok(())
}

struct EncodedActor {
    kind: &'static str,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
}

fn encode_actor(actor: Actor) -> EncodedActor {
    match actor {
        Actor::Owner => EncodedActor {
            kind: "owner",
            turn: None,
            tool_request: None,
        },
        Actor::Model { turn } => EncodedActor {
            kind: "model",
            turn: Some(turn.into_uuid()),
            tool_request: None,
        },
        Actor::Recovery => EncodedActor {
            kind: "recovery",
            turn: None,
            tool_request: None,
        },
        Actor::Tool { request } => EncodedActor {
            kind: "tool",
            turn: None,
            tool_request: Some(request.into_uuid()),
        },
    }
}

#[derive(Clone, Copy)]
struct EncodedSelection {
    kind: Option<&'static str>,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
}

impl EncodedSelection {
    const fn absent() -> Self {
        Self {
            kind: None,
            direct: None,
            alias: None,
        }
    }
}

fn encode_selection(selection: ModelSelectionRequest) -> EncodedSelection {
    match selection {
        ModelSelectionRequest::Direct(selection) => EncodedSelection {
            kind: Some("direct"),
            direct: Some(selection.into_uuid()),
            alias: None,
        },
        ModelSelectionRequest::Alias(alias) => EncodedSelection {
            kind: Some("alias"),
            direct: None,
            alias: Some(alias.into_uuid()),
        },
    }
}

struct EncodedFrozenModel {
    kind: &'static str,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
}

fn encode_frozen_model(model: &FrozenModelSelection) -> EncodedFrozenModel {
    match model {
        FrozenModelSelection::Direct(selection) => EncodedFrozenModel {
            kind: "direct",
            direct: Some(selection.into_uuid()),
            alias: None,
            alias_selected: None,
        },
        FrozenModelSelection::FrozenAlias { alias, definition } => EncodedFrozenModel {
            kind: "frozen_alias",
            direct: None,
            alias: Some(alias.into_uuid()),
            alias_selected: Some(definition.selected().into_uuid()),
        },
    }
}

#[derive(Clone, Copy)]
struct EncodedDelivery {
    kind: &'static str,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<&'static str>,
    replacement: EncodedSelection,
}

fn encode_delivery(delivery: DeliveryRequest) -> EncodedDelivery {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration } => {
            encode_configured_delivery("start_when_no_active_turn", None, configuration)
        }
        DeliveryRequest::Interrupt {
            expected_active_turn,
            configuration,
        } => encode_configured_delivery(
            "interrupt",
            Some(expected_active_turn.into_uuid()),
            configuration,
        ),
        DeliveryRequest::NextSafePoint {
            expected_active_turn,
        } => EncodedDelivery {
            kind: "next_safe_point",
            expected_active_turn: Some(expected_active_turn.into_uuid()),
            expected_defaults_version: None,
            model_override_kind: None,
            replacement: EncodedSelection::absent(),
        },
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration,
        } => encode_configured_delivery(
            "after_current_turn",
            Some(expected_active_turn.into_uuid()),
            configuration,
        ),
    }
}

fn encode_configured_delivery(
    kind: &'static str,
    expected_active_turn: Option<Uuid>,
    configuration: PerInputConfigurationChoices,
) -> EncodedDelivery {
    let (model_override_kind, replacement) = match configuration.model() {
        ModelSelectionOverride::UseSessionDefault => {
            ("use_session_default", EncodedSelection::absent())
        }
        ModelSelectionOverride::ReplaceWith(selection) => {
            ("replace_with", encode_selection(selection))
        }
    };
    EncodedDelivery {
        kind,
        expected_active_turn,
        expected_defaults_version: Some(defaults_version_to_numeric(
            configuration.expected_session_defaults_version(),
        )),
        model_override_kind: Some(model_override_kind),
        replacement,
    }
}

struct EncodedResult {
    kind: &'static str,
    rejection_kind: Option<&'static str>,
    session: SessionId,
    accepted_input: Option<Uuid>,
    turn: Option<Uuid>,
    actual_active_turn: Option<Uuid>,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    current_defaults_version: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults_version: Option<Decimal>,
    last_position: Option<Decimal>,
    existing_interrupt_command: Option<Uuid>,
}

fn encode_result(result: &SubmitInputResult, delivery: DeliveryRequest) -> EncodedResult {
    match result {
        SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(result)) => EncodedResult {
            kind: APPLIED,
            rejection_kind: None,
            session: result.session(),
            accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
            turn: Some(turn_id_to_uuid(result.turn())),
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
        },
        SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(result)) => {
            EncodedResult {
                kind: APPLIED,
                rejection_kind: None,
                session: result.session(),
                accepted_input: Some(accepted_input_id_to_uuid(result.accepted_input())),
                turn: None,
                actual_active_turn: Some(turn_id_to_uuid(result.binding().source_turn())),
                expected_active_turn: None,
                expected_defaults_version: None,
                current_defaults_version: None,
                unknown_alias: None,
                selected_defaults_version: None,
                last_position: None,
                existing_interrupt_command: None,
            }
        }
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
            session,
            active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("active_turn_present"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
            session,
            expected_active_turn,
            actual_active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("active_turn_mismatch"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*actual_active_turn)),
            expected_active_turn: Some(turn_id_to_uuid(*expected_active_turn)),
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound { session }) => {
            EncodedResult {
                kind: REJECTED,
                rejection_kind: Some("session_not_found"),
                session: *session,
                accepted_input: None,
                turn: None,
                actual_active_turn: None,
                expected_active_turn: None,
                expected_defaults_version: None,
                current_defaults_version: None,
                unknown_alias: None,
                selected_defaults_version: None,
                last_position: None,
                existing_interrupt_command: None,
            }
        }
        SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
            session,
            expected_active_turn,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("no_active_turn"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: Some(turn_id_to_uuid(*expected_active_turn)),
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session,
                expected,
                current,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("session_defaults_version_mismatch"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: Some(defaults_version_to_numeric(*expected)),
            current_defaults_version: Some(defaults_version_to_numeric(*current)),
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
            session,
            alias,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("unknown_model_alias"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: Some(alias.into_uuid()),
            selected_defaults_version: configured_defaults_version(delivery)
                .map(defaults_version_to_numeric),
            last_position: None,
            existing_interrupt_command: None,
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::AcceptancePositionExhausted {
            session,
            last,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("acceptance_position_exhausted"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: None,
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: Some(input_position_to_numeric(*last)),
            existing_interrupt_command: None,
        },
        SubmitInputResult::Rejected(
            SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                session,
                active_turn,
                existing_command,
            },
        ) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("safe_point_unavailable_while_stopping"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: Some(durable_command_id_to_uuid(*existing_command)),
        },
        SubmitInputResult::Rejected(SubmitInputRejectedResult::InterruptAlreadyApplied {
            session,
            active_turn,
            existing_command,
        }) => EncodedResult {
            kind: REJECTED,
            rejection_kind: Some("interrupt_already_applied"),
            session: *session,
            accepted_input: None,
            turn: None,
            actual_active_turn: Some(turn_id_to_uuid(*active_turn)),
            expected_active_turn: None,
            expected_defaults_version: None,
            current_defaults_version: None,
            unknown_alias: None,
            selected_defaults_version: None,
            last_position: None,
            existing_interrupt_command: Some(durable_command_id_to_uuid(*existing_command)),
        },
    }
}

fn configured_defaults_version(
    delivery: DeliveryRequest,
) -> Option<SessionConfigurationDefaultsVersion> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => {
            Some(configuration.expected_session_defaults_version())
        }
        DeliveryRequest::NextSafePoint { .. } => None,
    }
}

async fn load_complete_rows(
    connection: &mut PgConnection,
    command_ids: &[Uuid],
) -> Result<Vec<PgRow>, SubmitInputRepositoryError> {
    let rows = sqlx::query(
        "SELECT
            registry.command_id AS registry_command_id,
            registry.command_kind AS registry_kind,
            registry.storage_version AS registry_version,
            typed.command_id AS typed_command_id,
            typed.command_kind AS typed_kind,
            typed.storage_version AS typed_version,
            typed.session_id AS command_session_id,
            typed.actor_kind,
            typed.actor_turn_id,
            typed.actor_tool_request_id,
            typed.content_kind AS command_content_kind,
            typed.content_text AS command_content_text,
            typed.delivery_kind AS command_delivery_kind,
            typed.expected_active_turn_id AS command_expected_active_turn_id,
            typed.expected_defaults_version AS command_expected_defaults_version,
            typed.model_override_kind AS command_model_override_kind,
            typed.replacement_model_kind AS command_replacement_model_kind,
            typed.replacement_direct_model_selection_id AS command_replacement_direct_id,
            typed.replacement_model_alias_id AS command_replacement_alias_id,
            typed.result_kind,
            typed.rejection_kind,
            typed.result_session_id,
            typed.result_accepted_input_id,
            typed.result_turn_id,
            typed.result_actual_active_turn_id,
            typed.result_expected_active_turn_id,
            typed.result_expected_defaults_version,
            typed.result_current_defaults_version,
            typed.result_unknown_alias_id,
            typed.result_selected_defaults_version,
            typed.result_last_position,
            typed.result_existing_interrupt_command_id,
            accepted.accepting_command_id,
            accepted.accepted_input_id,
            accepted.session_id AS accepted_session_id,
            accepted.content_kind AS accepted_content_kind,
            accepted.content_text AS accepted_content_text,
            accepted.delivery_kind AS accepted_delivery_kind,
            accepted.expected_active_turn_id AS accepted_expected_active_turn_id,
            accepted.expected_defaults_version AS accepted_expected_defaults_version,
            accepted.model_override_kind AS accepted_model_override_kind,
            accepted.replacement_model_kind AS accepted_replacement_model_kind,
            accepted.replacement_direct_model_selection_id AS accepted_replacement_direct_id,
            accepted.replacement_model_alias_id AS accepted_replacement_alias_id,
            accepted.acceptance_position AS accepted_position,
            accepted.disposition_kind,
            accepted.origin_turn_id,
            queued.turn_id AS queued_turn_id,
            queued.accepted_input_id AS queued_accepted_input_id,
            queued.session_id AS queued_session_id,
            queued.acceptance_position AS queued_position,
            queued.priority_kind,
            queued.interrupt_predecessor_turn_id,
            queued.defaults_version AS queued_defaults_version,
            queued.requested_model_kind,
            queued.requested_direct_model_selection_id,
            queued.requested_model_alias_id,
            queued.frozen_model_kind,
            queued.frozen_direct_model_selection_id,
            queued.frozen_model_alias_id,
            queued.frozen_alias_selected_direct_id,
            queued.model_parameters,
            queued.known_provider_failure_retry,
            queued.model_fallback,
            defaults.session_id AS defaults_session_id,
            defaults.version AS defaults_version,
            defaults.model_selection_kind AS defaults_model_kind,
            defaults.direct_model_selection_id AS defaults_direct_id,
            defaults.model_alias_id AS defaults_alias_id,
            (
                SELECT count(*)
                  FROM accepted_input AS effect
                 WHERE effect.accepting_command_id = typed.command_id
            ) AS accepted_effect_count,
            (
                SELECT count(*)
                  FROM queued_input_origin AS effect_queue
                  JOIN accepted_input AS effect_input
                    ON effect_input.accepted_input_id = effect_queue.accepted_input_id
                 WHERE effect_input.accepting_command_id = typed.command_id
            ) AS queued_effect_count
         FROM durable_command AS registry
         LEFT JOIN submit_input_command AS typed
           ON typed.command_id = registry.command_id
         LEFT JOIN accepted_input AS accepted
           ON accepted.accepted_input_id = typed.result_accepted_input_id
         LEFT JOIN queued_input_origin AS queued
           ON queued.accepted_input_id = accepted.accepted_input_id
         LEFT JOIN session_defaults_version AS defaults
           ON defaults.session_id = typed.result_session_id
          AND defaults.version = COALESCE(
                queued.defaults_version,
                typed.result_selected_defaults_version
              )
         WHERE registry.command_id = ANY($1)",
    )
    .bind(command_ids)
    .fetch_all(&mut *connection)
    .await?;

    Ok(rows)
}

async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedSubmitInput>, SubmitInputRepositoryError> {
    let command_uuid = durable_command_id_to_uuid(command_id);
    let mut rows = load_complete_rows(connection, &[command_uuid]).await?;
    let Some(row) = rows.pop() else {
        return Ok(None);
    };
    if !rows.is_empty() {
        return Err(SubmitInputCorruption::Inconsistent("duplicate complete command rows").into());
    }
    let related_turn_origin = load_related_turn_origin(connection, &row).await?;
    let existing_interrupt = load_existing_interrupt(connection, &row).await?;
    decode_complete(row, command_id, related_turn_origin, existing_interrupt).map(Some)
}

async fn load_existing_interrupt(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<Option<AppliedInterruptCommandResult>, SubmitInputRepositoryError> {
    let Some(command_uuid) =
        row.try_get::<Option<Uuid>, _>("result_existing_interrupt_command_id")?
    else {
        return Ok(None);
    };
    let command = durable_command_id_from_uuid(command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("existing interrupt command identity"))?;
    let mut rows = load_complete_rows(connection, &[command_uuid]).await?;
    let interrupt_row = rows
        .pop()
        .ok_or(SubmitInputCorruption::Missing("existing interrupt command"))?;
    if !rows.is_empty() {
        return Err(
            SubmitInputCorruption::Inconsistent("duplicate existing interrupt command").into(),
        );
    }
    let predecessor_origin = load_related_turn_origin(connection, &interrupt_row).await?;
    let receipt = decode_complete(interrupt_row, command, predecessor_origin, None)?;
    let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) = receipt.result()
    else {
        return Err(
            SubmitInputCorruption::Inconsistent("existing interrupt was not applied").into(),
        );
    };
    origin
        .applied_interrupt()
        .copied()
        .map(Some)
        .ok_or_else(|| SubmitInputCorruption::Inconsistent("existing interrupt authority").into())
}

async fn load_related_turn_origin(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<Option<SubmitInputTurnOriginReconstitutionInput>, SubmitInputRepositoryError> {
    let Some(key) = related_turn_origin_key(row)? else {
        return Ok(None);
    };
    let mut origins = load_turn_origin_graph(connection, &BTreeSet::from([key])).await?;
    origins
        .remove(&key)
        .map(Some)
        .ok_or_else(|| SubmitInputCorruption::Missing("related turn origin").into())
}

fn related_turn_origin_key(
    row: &PgRow,
) -> Result<Option<StoredTurnOriginKey>, SubmitInputRepositoryError> {
    let result_kind: Option<String> = row.try_get("result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let delivery_kind: Option<String> = row.try_get("command_delivery_kind")?;
    let source_turn = match (
        result_kind.as_deref(),
        rejection_kind.as_deref(),
        delivery_kind.as_deref(),
    ) {
        (Some(APPLIED), None, Some("interrupt" | "after_current_turn" | "next_safe_point")) => {
            required(row, "command_expected_active_turn_id")?
        }
        (
            Some(REJECTED),
            Some(
                "active_turn_present"
                | "active_turn_mismatch"
                | "safe_point_unavailable_while_stopping"
                | "interrupt_already_applied",
            ),
            _,
        ) => required(row, "result_actual_active_turn_id")?,
        (
            Some(REJECTED),
            Some(
                "session_defaults_version_mismatch"
                | "unknown_model_alias"
                | "acceptance_position_exhausted",
            ),
            Some("interrupt" | "after_current_turn" | "next_safe_point"),
        ) => required(row, "command_expected_active_turn_id")?,
        _ => return Ok(None),
    };
    Ok(Some((required(row, "result_session_id")?, source_turn)))
}

pub(crate) async fn load_turn_origin_graph(
    connection: &mut PgConnection,
    roots: &BTreeSet<StoredTurnOriginKey>,
) -> Result<
    BTreeMap<StoredTurnOriginKey, SubmitInputTurnOriginReconstitutionInput>,
    SubmitInputRepositoryError,
> {
    if roots.is_empty() {
        return Ok(BTreeMap::new());
    }

    let source_sessions = roots
        .iter()
        .map(|(session, _)| *session)
        .collect::<Vec<_>>();
    let source_turns = roots.iter().map(|(_, turn)| *turn).collect::<Vec<_>>();
    let link_rows = sqlx::query(
        "WITH RECURSIVE origin_turn(session_id, turn_id) AS (
            SELECT root.session_id, root.turn_id
              FROM UNNEST($1::uuid[], $2::uuid[]) AS root(session_id, turn_id)
            UNION
            SELECT
                current.session_id,
                CASE accepted.disposition_kind
                    WHEN 'reclassified_as_turn_origin'
                        THEN accepted.expected_active_turn_id
                    ELSE command.expected_active_turn_id
                END
              FROM origin_turn AS current
              JOIN turn_lifecycle AS turn
                ON turn.turn_id = current.turn_id
               AND turn.session_id = current.session_id
              JOIN queued_input_origin AS queued
                ON queued.turn_id = turn.turn_id
               AND queued.session_id = turn.session_id
               AND queued.accepted_input_id = turn.origin_accepted_input_id
              JOIN accepted_input AS accepted
                ON accepted.accepted_input_id = queued.accepted_input_id
               AND accepted.session_id = turn.session_id
               AND accepted.origin_turn_id = turn.turn_id
               AND accepted.disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
              JOIN submit_input_command AS command
                ON command.command_id = accepted.accepting_command_id
             WHERE (
                    accepted.disposition_kind = 'reclassified_as_turn_origin'
                    AND accepted.expected_active_turn_id IS NOT NULL
               ) OR (
                    accepted.disposition_kind = 'origin_of'
                    AND command.delivery_kind IN (
                        'interrupt',
                        'after_current_turn'
                    )
                    AND command.expected_active_turn_id IS NOT NULL
               )
        )
        SELECT
            current.session_id AS origin_session_id,
            current.turn_id AS origin_turn_id,
            accepted.accepting_command_id AS origin_command_id,
            accepted.accepted_input_id AS origin_accepted_input_id,
            accepted.disposition_kind AS origin_disposition_kind,
            accepted.expected_active_turn_id AS reclassified_source_turn_id,
            queued.acceptance_position AS origin_acceptance_position,
            queued.priority_kind AS origin_priority_kind,
            queued.interrupt_predecessor_turn_id AS origin_interrupt_predecessor_turn_id,
            command.delivery_kind AS origin_delivery_kind,
            command.expected_active_turn_id AS origin_predecessor_turn_id,
            source.state_kind AS source_state_kind,
            source.terminal_disposition_kind AS source_terminal_disposition_kind,
            source.terminal_model_call_id AS source_terminal_model_call_id,
            source_attempt.interrupt_command_id AS source_interrupt_command_id
          FROM origin_turn AS current
          JOIN turn_lifecycle AS turn
            ON turn.turn_id = current.turn_id
           AND turn.session_id = current.session_id
          JOIN queued_input_origin AS queued
            ON queued.turn_id = turn.turn_id
           AND queued.session_id = turn.session_id
           AND queued.accepted_input_id = turn.origin_accepted_input_id
          JOIN accepted_input AS accepted
            ON accepted.accepted_input_id = queued.accepted_input_id
           AND accepted.session_id = turn.session_id
           AND accepted.origin_turn_id = turn.turn_id
           AND accepted.disposition_kind IN (
                'origin_of',
                'reclassified_as_turn_origin'
           )
          JOIN submit_input_command AS command
            ON command.command_id = accepted.accepting_command_id
          LEFT JOIN turn_lifecycle AS source
            ON source.turn_id = accepted.expected_active_turn_id
           AND source.session_id = accepted.session_id
          LEFT JOIN turn_attempt AS source_attempt
            ON source_attempt.turn_attempt_id = source.terminal_attempt_id
           AND source_attempt.turn_id = source.turn_id
           AND source_attempt.session_id = source.session_id
         ORDER BY current.session_id, current.turn_id",
    )
    .bind(&source_sessions)
    .bind(&source_turns)
    .fetch_all(&mut *connection)
    .await?;

    let mut links = BTreeMap::new();
    let mut commands = BTreeMap::new();
    for row in link_rows {
        let key = (
            required(&row, "origin_session_id")?,
            required(&row, "origin_turn_id")?,
        );
        let command_uuid: Uuid = required(&row, "origin_command_id")?;
        let command_id = durable_command_id_from_uuid(command_uuid)
            .map_err(|_| SubmitInputCorruption::Inconsistent("turn origin command identity"))?;
        let accepted_input =
            accepted_input_id_from_uuid(required(&row, "origin_accepted_input_id")?);
        let queue_position = decode_position(&row, "origin_acceptance_position")?;
        let interrupt_predecessor: Option<Uuid> =
            row.try_get("origin_interrupt_predecessor_turn_id")?;
        let queue_order = match required::<String>(&row, "origin_priority_kind")?.as_str() {
            "ordinary" if interrupt_predecessor.is_none() => {
                AcceptedInputQueueOrder::ordinary(queue_position)
            }
            "interrupt_immediately_after" => AcceptedInputQueueOrder::interrupt_immediately_after(
                queue_position,
                turn_id_from_uuid(interrupt_predecessor.ok_or(SubmitInputCorruption::Missing(
                    "origin_interrupt_predecessor_turn_id",
                ))?),
            ),
            "ordinary" => {
                return Err(SubmitInputCorruption::Inconsistent("ordinary origin priority").into());
            }
            value => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "origin_priority_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        let disposition_kind: String = required(&row, "origin_disposition_kind")?;
        let delivery_kind: String = required(&row, "origin_delivery_kind")?;
        let predecessor_turn: Option<Uuid> = row.try_get("origin_predecessor_turn_id")?;
        let reclassified_source: Option<Uuid> = row.try_get("reclassified_source_turn_id")?;
        let source_state: Option<String> = row.try_get("source_state_kind")?;
        let source_disposition: Option<String> = row.try_get("source_terminal_disposition_kind")?;
        let kind = match (
            disposition_kind.as_str(),
            delivery_kind.as_str(),
            predecessor_turn,
            reclassified_source,
        ) {
            ("origin_of", "start_when_no_active_turn", None, None) => {
                StoredTurnOriginKind::Direct { predecessor: None }
            }
            ("origin_of", "after_current_turn", Some(turn), Some(source)) if turn == source => {
                StoredTurnOriginKind::Direct {
                    predecessor: Some((key.0, turn)),
                }
            }
            ("origin_of", "interrupt", Some(turn), Some(source))
                if turn == source && interrupt_predecessor == Some(turn) =>
            {
                StoredTurnOriginKind::Direct {
                    predecessor: Some((key.0, turn)),
                }
            }
            ("reclassified_as_turn_origin", "next_safe_point", Some(source), Some(binding))
                if source == binding && source_state.as_deref() == Some("terminal") =>
            {
                let source_disposition = match source_disposition.as_deref() {
                    Some("completed") => StoredTerminalTurnDisposition::Completed,
                    Some("refused") => StoredTerminalTurnDisposition::Refused,
                    Some("failed") => StoredTerminalTurnDisposition::Failed,
                    Some("cancelled") => {
                        let command = durable_command_id_from_uuid(required(
                            &row,
                            "source_interrupt_command_id",
                        )?)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "cancelled source interrupt command",
                            )
                        })?;
                        StoredTerminalTurnDisposition::Cancelled {
                            interrupt_command: command,
                        }
                    }
                    Some("reconciliation_required") => {
                        let command = durable_command_id_from_uuid(required(
                            &row,
                            "source_interrupt_command_id",
                        )?)
                        .map_err(|_| {
                            SubmitInputCorruption::Inconsistent(
                                "reconciliation source interrupt command",
                            )
                        })?;
                        StoredTerminalTurnDisposition::ReconciliationRequired {
                            interrupt_command: command,
                            ambiguous_call: ModelCallId::from_uuid(required(
                                &row,
                                "source_terminal_model_call_id",
                            )?),
                        }
                    }
                    Some(value) => {
                        return Err(SubmitInputCorruption::Unsupported {
                            field: "reclassified source terminal disposition",
                            value: value.to_owned(),
                        }
                        .into());
                    }
                    None => {
                        return Err(SubmitInputCorruption::Missing(
                            "reclassified source terminal disposition",
                        )
                        .into());
                    }
                };
                StoredTurnOriginKind::Reclassified {
                    source: (key.0, source),
                    source_disposition,
                }
            }
            ("origin_of" | "reclassified_as_turn_origin", _, _, _) => {
                return Err(
                    SubmitInputCorruption::Inconsistent("turn origin predecessor shape").into(),
                );
            }
            (value, _, _, _) => {
                return Err(SubmitInputCorruption::Unsupported {
                    field: "turn origin accepted-input disposition_kind",
                    value: value.to_owned(),
                }
                .into());
            }
        };
        if links
            .insert(
                key,
                StoredTurnOriginLink {
                    command_id,
                    kind,
                    accepted_input,
                    queue_order,
                },
            )
            .is_some()
        {
            return Err(SubmitInputCorruption::Inconsistent("duplicate turn origin").into());
        }
        if commands.insert(command_uuid, key).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("turn origin command reused by turns").into(),
            );
        }
    }

    for root in roots {
        if !links.contains_key(root) {
            return Err(SubmitInputCorruption::Missing("related turn origin").into());
        }
    }
    for link in links.values() {
        if let Some(dependency) = link.kind.dependency()
            && !links.contains_key(&dependency)
        {
            return Err(SubmitInputCorruption::Missing("turn origin predecessor").into());
        }
    }

    let command_uuids = commands.keys().copied().collect::<Vec<_>>();
    let complete_rows = load_complete_rows(connection, &command_uuids).await?;
    let mut rows_by_command = BTreeMap::new();
    for row in complete_rows {
        let command_uuid: Uuid = required(&row, "registry_command_id")?;
        if !commands.contains_key(&command_uuid) {
            return Err(
                SubmitInputCorruption::Inconsistent("unexpected turn origin command").into(),
            );
        }
        if rows_by_command.insert(command_uuid, row).is_some() {
            return Err(
                SubmitInputCorruption::Inconsistent("duplicate turn origin command rows").into(),
            );
        }
    }
    if rows_by_command.len() != commands.len() {
        return Err(SubmitInputCorruption::Missing("turn origin command").into());
    }

    let decode_order = turn_origin_dependency_order(
        links
            .iter()
            .map(|(key, link)| (*key, link.kind.dependency())),
    )
    .ok_or(SubmitInputCorruption::Inconsistent(
        "turn origin predecessor cycle",
    ))?;
    let mut decoded = BTreeMap::new();
    for ready in decode_order {
        #[expect(
            clippy::expect_used,
            reason = "temporary ledger site: the dependency-order output is derived from these exact remaining links; typed conversion is commissioned by the 2026-07-20 audit"
        )]
        let link = links
            .remove(&ready)
            .expect("the selected turn origin link remains present");
        let command_uuid = durable_command_id_to_uuid(link.command_id);
        let row = rows_by_command
            .remove(&command_uuid)
            .ok_or(SubmitInputCorruption::Missing("turn origin command"))?;
        let dependency = link
            .kind
            .dependency()
            .map(|key| {
                decoded
                    .get(&key)
                    .cloned()
                    .ok_or(SubmitInputCorruption::Missing("turn origin predecessor"))
            })
            .transpose()?;
        let receipt = decode_complete(row, link.command_id, dependency.clone(), None)?;
        let reconstructed = match link.kind {
            StoredTurnOriginKind::Direct { .. } => {
                let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
                    receipt.result()
                else {
                    return Err(
                        SubmitInputCorruption::Inconsistent("turn origin command result").into(),
                    );
                };
                if session_id_to_uuid(applied.session()) != ready.0
                    || turn_id_to_uuid(applied.turn()) != ready.1
                {
                    return Err(
                        SubmitInputCorruption::Inconsistent("turn origin correlation").into(),
                    );
                }
                SubmitInputTurnOriginReconstitutionInput::new(
                    receipt,
                    AcceptedInputLifecycle::new(
                        link.accepted_input,
                        AcceptedInputDisposition::OriginOf(turn_id_from_uuid(ready.1)),
                    ),
                    link.accepted_input,
                    session_id_from_uuid(ready.0),
                    turn_id_from_uuid(ready.1),
                    link.queue_order,
                )
            }
            StoredTurnOriginKind::Reclassified {
                source,
                source_disposition,
            } => {
                let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(applied)) =
                    receipt.result()
                else {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "reclassified origin command result",
                    )
                    .into());
                };
                if session_id_to_uuid(applied.session()) != ready.0
                    || applied.accepted_input() != link.accepted_input
                    || applied.binding().source_turn() != turn_id_from_uuid(source.1)
                {
                    return Err(SubmitInputCorruption::Inconsistent(
                        "reclassified origin correlation",
                    )
                    .into());
                }
                let source_origin = dependency
                    .ok_or(SubmitInputCorruption::Missing("reclassified source origin"))?;
                let source_turn = turn_id_from_uuid(source.1);
                let source_terminal = match source_disposition {
                    StoredTerminalTurnDisposition::Completed
                    | StoredTerminalTurnDisposition::Refused
                    | StoredTerminalTurnDisposition::Failed => {
                        SubmitInputTerminalSourceReconstitutionInput::new(
                            source_origin.clone(),
                            source_turn,
                            source_disposition.unstopped_domain().ok_or(
                                SubmitInputCorruption::Inconsistent("terminal source disposition"),
                            )?,
                        )
                    }
                    StoredTerminalTurnDisposition::Cancelled { interrupt_command } => {
                        let interrupt_uuid = durable_command_id_to_uuid(interrupt_command);
                        let mut interrupt_rows =
                            load_complete_rows(connection, &[interrupt_uuid]).await?;
                        let interrupt_row = interrupt_rows.pop().ok_or(
                            SubmitInputCorruption::Missing("cancelled source interrupt command"),
                        )?;
                        if !interrupt_rows.is_empty() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "duplicate cancelled source interrupt command",
                            )
                            .into());
                        }
                        let interrupt_receipt = decode_complete(
                            interrupt_row,
                            interrupt_command,
                            Some(source_origin.clone()),
                            None,
                        )?;
                        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                            interrupt_origin,
                        )) = interrupt_receipt.result()
                        else {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "cancelled source interrupt result",
                            )
                            .into());
                        };
                        SubmitInputTerminalSourceReconstitutionInput::new(
                            source_origin.clone(),
                            source_turn,
                            signalbox_domain::TurnDisposition::Cancelled {
                                cause: interrupt_origin
                                    .applied_interrupt()
                                    .ok_or(SubmitInputCorruption::Inconsistent(
                                        "cancelled source interrupt authority",
                                    ))?
                                    .proof(),
                            },
                        )
                    }
                    StoredTerminalTurnDisposition::ReconciliationRequired {
                        interrupt_command,
                        ambiguous_call,
                    } => {
                        let interrupt_uuid = durable_command_id_to_uuid(interrupt_command);
                        let mut interrupt_rows =
                            load_complete_rows(connection, &[interrupt_uuid]).await?;
                        let interrupt_row =
                            interrupt_rows.pop().ok_or(SubmitInputCorruption::Missing(
                                "reconciliation source interrupt command",
                            ))?;
                        if !interrupt_rows.is_empty() {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "duplicate reconciliation source interrupt command",
                            )
                            .into());
                        }
                        let interrupt_receipt = decode_complete(
                            interrupt_row,
                            interrupt_command,
                            Some(source_origin.clone()),
                            None,
                        )?;
                        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                            interrupt_origin,
                        )) = interrupt_receipt.result()
                        else {
                            return Err(SubmitInputCorruption::Inconsistent(
                                "reconciliation source interrupt result",
                            )
                            .into());
                        };
                        SubmitInputTerminalSourceReconstitutionInput::
                            interrupted_model_call_reconciliation(
                                source_origin.clone(),
                                source_turn,
                                ambiguous_call,
                                interrupt_origin
                                    .applied_interrupt()
                                    .ok_or(SubmitInputCorruption::Inconsistent(
                                        "reconciliation source interrupt authority",
                                    ))?
                                    .proof(),
                            )
                    }
                };
                SubmitInputTurnOriginReconstitutionInput::reclassified(
                    receipt,
                    AcceptedInputLifecycle::new(
                        link.accepted_input,
                        AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                            turn: turn_id_from_uuid(ready.1),
                            reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
                        },
                    ),
                    link.accepted_input,
                    session_id_from_uuid(ready.0),
                    turn_id_from_uuid(ready.1),
                    link.queue_order,
                    source_terminal,
                )
            }
        };
        decoded.insert(ready, reconstructed);
    }
    debug_assert!(links.is_empty());

    Ok(decoded)
}

fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
    related_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    existing_interrupt: Option<AppliedInterruptCommandResult>,
) -> Result<ReconstitutedSubmitInput, SubmitInputRepositoryError> {
    require_spelling(&row, "registry_kind", SUBMIT_INPUT_KIND)?;
    require_version(&row, "registry_version", STORAGE_VERSION)?;
    let typed_id: Uuid = required(&row, "typed_command_id")?;
    if typed_id != durable_command_id_to_uuid(command_id) {
        return Err(SubmitInputCorruption::Inconsistent("typed command identity").into());
    }
    require_spelling(&row, "typed_kind", SUBMIT_INPUT_KIND)?;
    require_version(&row, "typed_version", STORAGE_VERSION)?;

    // Decode-level checks reject unknown or malformed actor spellings here;
    // comparing the decoded actor against the canonical command's actor is
    // domain-owned semantics and happens inside reconstitution.
    let actor = decode_actor(
        required(&row, "actor_kind")?,
        row.try_get("actor_turn_id")?,
        row.try_get("actor_tool_request_id")?,
    )?;
    let command = SubmitInput::new(
        command_id,
        session_id_from_uuid(required(&row, "command_session_id")?),
        decode_content(
            required(&row, "command_content_kind")?,
            required(&row, "command_content_text")?,
            "command content",
        )?,
        decode_delivery(
            required(&row, "command_delivery_kind")?,
            row.try_get("command_expected_active_turn_id")?,
            row.try_get("command_expected_defaults_version")?,
            row.try_get("command_model_override_kind")?,
            row.try_get("command_replacement_model_kind")?,
            row.try_get("command_replacement_direct_id")?,
            row.try_get("command_replacement_alias_id")?,
            "command delivery",
        )?,
    );

    let result_kind: String = required(&row, "result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);
    let result_accepted: Option<Uuid> = row.try_get("result_accepted_input_id")?;
    let result_turn: Option<Uuid> = row.try_get("result_turn_id")?;
    let result_actual_turn: Option<Uuid> = row.try_get("result_actual_active_turn_id")?;
    let result_expected_turn: Option<Uuid> = row.try_get("result_expected_active_turn_id")?;
    let result_expected_defaults: Option<Decimal> =
        row.try_get("result_expected_defaults_version")?;
    let result_current_defaults: Option<Decimal> =
        row.try_get("result_current_defaults_version")?;
    let result_unknown_alias: Option<Uuid> = row.try_get("result_unknown_alias_id")?;
    let result_selected_defaults: Option<Decimal> =
        row.try_get("result_selected_defaults_version")?;
    let result_last_position: Option<Decimal> = row.try_get("result_last_position")?;
    let result_existing_interrupt: Option<Uuid> =
        row.try_get("result_existing_interrupt_command_id")?;
    let accepted_effect_count: i64 = required(&row, "accepted_effect_count")?;
    let queued_effect_count: i64 = required(&row, "queued_effect_count")?;

    let input = match (result_kind.as_str(), rejection_kind.as_deref()) {
        (APPLIED, None) => {
            if result_expected_turn.is_some()
                || result_expected_defaults.is_some()
                || result_current_defaults.is_some()
                || result_unknown_alias.is_some()
                || result_selected_defaults.is_some()
                || result_last_position.is_some()
                || result_existing_interrupt.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("applied result fields").into());
            }
            if accepted_effect_count != 1 {
                return Err(
                    SubmitInputCorruption::Inconsistent("applied effect cardinality").into(),
                );
            }
            let result_accepted = accepted_input_id_from_uuid(
                result_accepted
                    .ok_or(SubmitInputCorruption::Missing("result_accepted_input_id"))?,
            );
            match (result_turn, result_actual_turn) {
                (Some(result_turn), None) if queued_effect_count == 1 => {
                    decode_applied_turn_origin(
                        &row,
                        command,
                        actor,
                        result_session,
                        result_accepted,
                        turn_id_from_uuid(result_turn),
                        related_turn_origin,
                    )?
                }
                (None, Some(source_turn)) if queued_effect_count <= 1 => {
                    let source_turn_origin = related_turn_origin.ok_or(
                        SubmitInputCorruption::Missing("pending steering source turn origin"),
                    )?;
                    decode_applied_pending_steering(
                        &row,
                        command,
                        actor,
                        result_session,
                        result_accepted,
                        turn_id_from_uuid(source_turn),
                        source_turn_origin,
                    )?
                }
                _ => {
                    return Err(
                        SubmitInputCorruption::Inconsistent("applied variant correlation").into(),
                    );
                }
            }
        }
        (REJECTED, Some(kind)) => {
            if accepted_effect_count != 0 || queued_effect_count != 0 {
                return Err(
                    SubmitInputCorruption::Inconsistent("rejected command has effects").into(),
                );
            }
            if result_accepted.is_some() || result_turn.is_some() {
                return Err(
                    SubmitInputCorruption::Inconsistent("rejected applied identities").into(),
                );
            }
            decode_rejected(
                &row,
                command,
                actor,
                result_session,
                related_turn_origin,
                kind,
                result_actual_turn,
                result_expected_turn,
                result_expected_defaults,
                result_current_defaults,
                result_unknown_alias,
                result_selected_defaults,
                result_last_position,
                result_existing_interrupt,
                existing_interrupt,
            )?
        }
        (APPLIED, Some(_)) | (REJECTED, None) => {
            return Err(SubmitInputCorruption::Inconsistent("terminal result shape").into());
        }
        (value, _) => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "result_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };

    input
        .reconstitute()
        .map_err(|error| SubmitInputCorruption::Domain(error.failure()).into())
}

fn decode_applied_turn_origin(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_turn: TurnId,
    predecessor_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    let accepting_command_uuid: Uuid = required(row, "accepting_command_id")?;
    let accepting_command = durable_command_id_from_uuid(accepting_command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?;
    let accepted_input = accepted_input_id_from_uuid(required(row, "accepted_input_id")?);
    let accepted_session = session_id_from_uuid(required(row, "accepted_session_id")?);
    let accepted_content = decode_content(
        required(row, "accepted_content_kind")?,
        required(row, "accepted_content_text")?,
        "accepted content",
    )?;
    let accepted_delivery = decode_delivery(
        required(row, "accepted_delivery_kind")?,
        row.try_get("accepted_expected_active_turn_id")?,
        row.try_get("accepted_expected_defaults_version")?,
        row.try_get("accepted_model_override_kind")?,
        row.try_get("accepted_replacement_model_kind")?,
        row.try_get("accepted_replacement_direct_id")?,
        row.try_get("accepted_replacement_alias_id")?,
        "accepted delivery",
    )?;
    let accepted_position = decode_position(row, "accepted_position")?;
    require_spelling(row, "disposition_kind", "origin_of")?;
    let accepted_origin_turn = turn_id_from_uuid(required(row, "origin_turn_id")?);

    let queued_turn = turn_id_from_uuid(required(row, "queued_turn_id")?);
    let queued_accepted = accepted_input_id_from_uuid(required(row, "queued_accepted_input_id")?);
    if queued_accepted != accepted_input {
        return Err(SubmitInputCorruption::Inconsistent("queued accepted input").into());
    }
    let queued_session = session_id_from_uuid(required(row, "queued_session_id")?);
    let queued_position = decode_position(row, "queued_position")?;
    let queue_order = match required::<String>(row, "priority_kind")?.as_str() {
        "ordinary" => {
            if row
                .try_get::<Option<Uuid>, _>("interrupt_predecessor_turn_id")?
                .is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent("ordinary queue priority").into());
            }
            AcceptedInputQueueOrder::ordinary(queued_position)
        }
        "interrupt_immediately_after" => AcceptedInputQueueOrder::interrupt_immediately_after(
            queued_position,
            turn_id_from_uuid(required(row, "interrupt_predecessor_turn_id")?),
        ),
        value => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "priority_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };
    require_spelling(row, "model_parameters", "provider_defaults")?;
    require_spelling(row, "known_provider_failure_retry", "disabled")?;
    require_spelling(row, "model_fallback", "disabled")?;
    let defaults_version = decode_defaults_version(row, "queued_defaults_version")?;

    let defaults_session = session_id_from_uuid(required(row, "defaults_session_id")?);
    let joined_defaults_version = decode_defaults_version(row, "defaults_version")?;
    if joined_defaults_version != defaults_version {
        return Err(SubmitInputCorruption::Inconsistent("selected defaults version").into());
    }
    let defaults = decode_defaults(
        required(row, "defaults_model_kind")?,
        row.try_get("defaults_direct_id")?,
        row.try_get("defaults_alias_id")?,
        "selected defaults",
    )?;
    let stored_requested_model = decode_model_selection(
        required(row, "requested_model_kind")?,
        row.try_get("requested_direct_model_selection_id")?,
        row.try_get("requested_model_alias_id")?,
        "requested model",
    )?;
    let stored_frozen_model = decode_frozen_model(
        required(row, "frozen_model_kind")?,
        row.try_get("frozen_direct_model_selection_id")?,
        row.try_get("frozen_model_alias_id")?,
        row.try_get("frozen_alias_selected_direct_id")?,
    )?;

    Ok(SubmitInputReconstitutionInput::applied_turn_origin(
        command,
        stored_actor,
        result_session,
        result_accepted_input,
        result_turn,
        predecessor_origin,
        accepting_command,
        accepted_input,
        accepted_session,
        accepted_content,
        accepted_delivery,
        accepted_position,
        AcceptedInputDisposition::OriginOf(accepted_origin_turn),
        queued_session,
        queued_turn,
        queue_order,
        defaults_session,
        defaults_version,
        defaults,
        stored_requested_model,
        stored_frozen_model,
    ))
}

fn decode_applied_pending_steering(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_source_turn: TurnId,
    source_turn_origin: SubmitInputTurnOriginReconstitutionInput,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    let accepting_command_uuid: Uuid = required(row, "accepting_command_id")?;
    let accepting_command = durable_command_id_from_uuid(accepting_command_uuid)
        .map_err(|_| SubmitInputCorruption::Inconsistent("accepting command identity"))?;
    let accepted_input = accepted_input_id_from_uuid(required(row, "accepted_input_id")?);
    let accepted_session = session_id_from_uuid(required(row, "accepted_session_id")?);
    let accepted_content = decode_content(
        required(row, "accepted_content_kind")?,
        required(row, "accepted_content_text")?,
        "accepted content",
    )?;
    let accepted_delivery = decode_delivery(
        required(row, "accepted_delivery_kind")?,
        row.try_get("accepted_expected_active_turn_id")?,
        row.try_get("accepted_expected_defaults_version")?,
        row.try_get("accepted_model_override_kind")?,
        row.try_get("accepted_replacement_model_kind")?,
        row.try_get("accepted_replacement_direct_id")?,
        row.try_get("accepted_replacement_alias_id")?,
        "accepted delivery",
    )?;
    let accepted_position = decode_position(row, "accepted_position")?;

    Ok(SubmitInputReconstitutionInput::applied_pending_steering(
        command,
        stored_actor,
        result_session,
        result_accepted_input,
        result_source_turn,
        source_turn_origin,
        accepting_command,
        accepted_input,
        accepted_session,
        accepted_content,
        accepted_delivery,
        accepted_position,
    ))
}

#[allow(clippy::too_many_arguments)]
fn decode_rejected(
    row: &PgRow,
    command: SubmitInput,
    stored_actor: Actor,
    result_session: SessionId,
    active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    rejection_kind: &str,
    actual_turn: Option<Uuid>,
    expected_turn: Option<Uuid>,
    expected_defaults: Option<Decimal>,
    current_defaults: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults: Option<Decimal>,
    last_position: Option<Decimal>,
    existing_interrupt_command: Option<Uuid>,
    existing_interrupt: Option<AppliedInterruptCommandResult>,
) -> Result<SubmitInputReconstitutionInput, SubmitInputRepositoryError> {
    if !matches!(
        rejection_kind,
        "safe_point_unavailable_while_stopping" | "interrupt_already_applied"
    ) && (existing_interrupt_command.is_some() || existing_interrupt.is_some())
    {
        return Err(
            SubmitInputCorruption::Inconsistent("unexpected existing interrupt result").into(),
        );
    }
    match rejection_kind {
        "session_not_found" => {
            require_all_absent(
                actual_turn,
                expected_turn,
                expected_defaults,
                current_defaults,
                unknown_alias,
                selected_defaults,
                last_position,
                "session-not-found result fields",
            )?;
            Ok(SubmitInputReconstitutionInput::rejected_session_not_found(
                command,
                stored_actor,
                result_session,
            ))
        }
        "no_active_turn" => {
            if actual_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("no-active-turn result fields").into(),
                );
            }
            Ok(SubmitInputReconstitutionInput::rejected_no_active_turn(
                command,
                stored_actor,
                result_session,
                turn_id_from_uuid(expected_turn.ok_or(SubmitInputCorruption::Missing(
                    "result_expected_active_turn_id",
                ))?),
            ))
        }
        "active_turn_present" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active-turn-present result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_active_turn_present(
                    command,
                    stored_actor,
                    result_session,
                    turn_id_from_uuid(actual_turn.ok_or(SubmitInputCorruption::Missing(
                        "result_actual_active_turn_id",
                    ))?),
                    active_turn_origin
                        .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                ),
            )
        }
        "active_turn_mismatch" => {
            if expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "active-turn-mismatch result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                    command,
                    stored_actor,
                    result_session,
                    turn_id_from_uuid(expected_turn.ok_or(SubmitInputCorruption::Missing(
                        "result_expected_active_turn_id",
                    ))?),
                    turn_id_from_uuid(actual_turn.ok_or(SubmitInputCorruption::Missing(
                        "result_actual_active_turn_id",
                    ))?),
                    active_turn_origin
                        .ok_or(SubmitInputCorruption::Missing("actual turn origin"))?,
                ),
            )
        }
        "session_defaults_version_mismatch" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("defaults-mismatch result fields").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                    command,
                    stored_actor,
                    result_session,
                    decode_optional_defaults_version(
                        expected_defaults,
                        "result_expected_defaults_version",
                    )?
                    .ok_or(SubmitInputCorruption::Missing(
                        "result_expected_defaults_version",
                    ))?,
                    decode_optional_defaults_version(
                        current_defaults,
                        "result_current_defaults_version",
                    )?
                    .ok_or(SubmitInputCorruption::Missing(
                        "result_current_defaults_version",
                    ))?,
                    active_turn_origin,
                ),
            )
        }
        "unknown_model_alias" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || last_position.is_some()
            {
                return Err(
                    SubmitInputCorruption::Inconsistent("unknown-alias result fields").into(),
                );
            }
            let selected = decode_optional_defaults_version(
                selected_defaults,
                "result_selected_defaults_version",
            )?
            .ok_or(SubmitInputCorruption::Missing(
                "result_selected_defaults_version",
            ))?;
            let defaults_session = session_id_from_uuid(required(row, "defaults_session_id")?);
            let defaults_version = decode_defaults_version(row, "defaults_version")?;
            let defaults = decode_defaults(
                required(row, "defaults_model_kind")?,
                row.try_get("defaults_direct_id")?,
                row.try_get("defaults_alias_id")?,
                "selected defaults",
            )?;
            if selected != defaults_version {
                return Err(
                    SubmitInputCorruption::Inconsistent("unknown-alias defaults version").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                    command,
                    stored_actor,
                    result_session,
                    ModelAlias::from_uuid(
                        unknown_alias
                            .ok_or(SubmitInputCorruption::Missing("result_unknown_alias_id"))?,
                    ),
                    defaults_session,
                    defaults_version,
                    defaults,
                    active_turn_origin,
                ),
            )
        }
        "acceptance_position_exhausted" => {
            if actual_turn.is_some()
                || expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "position-exhausted result fields",
                )
                .into());
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                    command,
                    stored_actor,
                    result_session,
                    decode_optional_position(last_position, "result_last_position")?
                        .ok_or(SubmitInputCorruption::Missing("result_last_position"))?,
                    active_turn_origin,
                ),
            )
        }
        "safe_point_unavailable_while_stopping" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "stopping safe-point result fields",
                )
                .into());
            }
            let active_turn = turn_id_from_uuid(actual_turn.ok_or(
                SubmitInputCorruption::Missing("result_actual_active_turn_id"),
            )?);
            let stored_command = durable_command_id_from_uuid(existing_interrupt_command.ok_or(
                SubmitInputCorruption::Missing("result_existing_interrupt_command_id"),
            )?)
            .map_err(|_| {
                SubmitInputCorruption::Inconsistent("existing interrupt command identity")
            })?;
            let interrupt = existing_interrupt.ok_or(SubmitInputCorruption::Missing(
                "existing interrupt authority",
            ))?;
            if stored_command != interrupt.proof().command() {
                return Err(
                    SubmitInputCorruption::Inconsistent("existing interrupt command").into(),
                );
            }
            Ok(
                SubmitInputReconstitutionInput::rejected_safe_point_unavailable_while_stopping(
                    command,
                    stored_actor,
                    result_session,
                    active_turn,
                    active_turn_origin
                        .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                    interrupt,
                ),
            )
        }
        "interrupt_already_applied" => {
            if expected_turn.is_some()
                || expected_defaults.is_some()
                || current_defaults.is_some()
                || unknown_alias.is_some()
                || selected_defaults.is_some()
                || last_position.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(
                    "already-applied interrupt result fields",
                )
                .into());
            }
            let active_turn = turn_id_from_uuid(actual_turn.ok_or(
                SubmitInputCorruption::Missing("result_actual_active_turn_id"),
            )?);
            let stored_command = durable_command_id_from_uuid(existing_interrupt_command.ok_or(
                SubmitInputCorruption::Missing("result_existing_interrupt_command_id"),
            )?)
            .map_err(|_| {
                SubmitInputCorruption::Inconsistent("existing interrupt command identity")
            })?;
            let interrupt = existing_interrupt.ok_or(SubmitInputCorruption::Missing(
                "existing interrupt authority",
            ))?;
            Ok(
                SubmitInputReconstitutionInput::rejected_interrupt_already_applied(
                    command,
                    stored_actor,
                    result_session,
                    active_turn,
                    stored_command,
                    active_turn_origin
                        .ok_or(SubmitInputCorruption::Missing("active turn origin"))?,
                    interrupt,
                ),
            )
        }
        value => Err(SubmitInputCorruption::Unsupported {
            field: "rejection_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SubmitInputRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| SubmitInputCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &str,
) -> Result<(), SubmitInputRepositoryError> {
    let actual: String = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(SubmitInputCorruption::Unsupported {
            field,
            value: actual,
        }
        .into())
    }
}

fn require_version(
    row: &PgRow,
    field: &'static str,
    expected: i16,
) -> Result<(), SubmitInputRepositoryError> {
    let actual: i16 = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(SubmitInputCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

#[allow(clippy::too_many_arguments)]
fn require_all_absent(
    actual_turn: Option<Uuid>,
    expected_turn: Option<Uuid>,
    expected_defaults: Option<Decimal>,
    current_defaults: Option<Decimal>,
    unknown_alias: Option<Uuid>,
    selected_defaults: Option<Decimal>,
    last_position: Option<Decimal>,
    relationship: &'static str,
) -> Result<(), SubmitInputRepositoryError> {
    if actual_turn.is_none()
        && expected_turn.is_none()
        && expected_defaults.is_none()
        && current_defaults.is_none()
        && unknown_alias.is_none()
        && selected_defaults.is_none()
        && last_position.is_none()
    {
        Ok(())
    } else {
        Err(SubmitInputCorruption::Inconsistent(relationship).into())
    }
}

fn decode_actor(
    kind: String,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
) -> Result<Actor, SubmitInputRepositoryError> {
    match (kind.as_str(), turn, tool_request) {
        ("owner", None, None) => Ok(Actor::Owner),
        ("model", Some(turn), None) => Ok(Actor::Model {
            turn: TurnId::from_uuid(turn),
        }),
        ("recovery", None, None) => Ok(Actor::Recovery),
        ("tool", None, Some(request)) => Ok(Actor::Tool {
            request: ToolRequestId::from_uuid(request),
        }),
        ("owner" | "model" | "recovery" | "tool", _, _) => {
            Err(SubmitInputCorruption::Inconsistent("actor fields").into())
        }
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "actor_kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_content(
    kind: String,
    text: String,
    field: &'static str,
) -> Result<UserContent, SubmitInputRepositoryError> {
    if kind != "text" {
        return Err(SubmitInputCorruption::Unsupported { field, value: kind }.into());
    }
    UserContent::try_text(text).map_err(|error| {
        SubmitInputCorruption::InvalidContent {
            field,
            failure: error.failure(),
        }
        .into()
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_delivery(
    kind: String,
    expected_active_turn: Option<Uuid>,
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<String>,
    replacement_model_kind: Option<String>,
    replacement_direct: Option<Uuid>,
    replacement_alias: Option<Uuid>,
    field: &'static str,
) -> Result<DeliveryRequest, SubmitInputRepositoryError> {
    match kind.as_str() {
        "start_when_no_active_turn" => {
            if expected_active_turn.is_some() {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            Ok(DeliveryRequest::StartWhenNoActiveTurn {
                configuration: decode_configuration(
                    expected_defaults_version,
                    model_override_kind,
                    replacement_model_kind,
                    replacement_direct,
                    replacement_alias,
                    field,
                )?,
            })
        }
        "interrupt" | "after_current_turn" => {
            let turn = TurnId::from_uuid(
                expected_active_turn
                    .ok_or(SubmitInputCorruption::Missing("expected_active_turn_id"))?,
            );
            let configuration = decode_configuration(
                expected_defaults_version,
                model_override_kind,
                replacement_model_kind,
                replacement_direct,
                replacement_alias,
                field,
            )?;
            if kind == "interrupt" {
                Ok(DeliveryRequest::Interrupt {
                    expected_active_turn: turn,
                    configuration,
                })
            } else {
                Ok(DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: turn,
                    configuration,
                })
            }
        }
        "next_safe_point" => {
            if expected_defaults_version.is_some()
                || model_override_kind.is_some()
                || replacement_model_kind.is_some()
                || replacement_direct.is_some()
                || replacement_alias.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            Ok(DeliveryRequest::NextSafePoint {
                expected_active_turn: TurnId::from_uuid(
                    expected_active_turn
                        .ok_or(SubmitInputCorruption::Missing("expected_active_turn_id"))?,
                ),
            })
        }
        _ => Err(SubmitInputCorruption::Unsupported { field, value: kind }.into()),
    }
}

fn decode_configuration(
    expected_defaults_version: Option<Decimal>,
    model_override_kind: Option<String>,
    replacement_model_kind: Option<String>,
    replacement_direct: Option<Uuid>,
    replacement_alias: Option<Uuid>,
    field: &'static str,
) -> Result<PerInputConfigurationChoices, SubmitInputRepositoryError> {
    let expected =
        decode_optional_defaults_version(expected_defaults_version, "expected_defaults_version")?
            .ok_or(SubmitInputCorruption::Missing("expected_defaults_version"))?;
    let model = match model_override_kind.as_deref() {
        Some("use_session_default") => {
            if replacement_model_kind.is_some()
                || replacement_direct.is_some()
                || replacement_alias.is_some()
            {
                return Err(SubmitInputCorruption::Inconsistent(field).into());
            }
            ModelSelectionOverride::UseSessionDefault
        }
        Some("replace_with") => ModelSelectionOverride::ReplaceWith(decode_model_selection(
            replacement_model_kind
                .ok_or(SubmitInputCorruption::Missing("replacement_model_kind"))?,
            replacement_direct,
            replacement_alias,
            "replacement model",
        )?),
        Some(value) => {
            return Err(SubmitInputCorruption::Unsupported {
                field: "model_override_kind",
                value: value.to_owned(),
            }
            .into());
        }
        None => return Err(SubmitInputCorruption::Missing("model_override_kind").into()),
    };
    Ok(PerInputConfigurationChoices::new(expected, model))
}

fn decode_defaults_version(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, SubmitInputRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_optional_defaults_version(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<SessionConfigurationDefaultsVersion>, SubmitInputRepositoryError> {
    value
        .map(|value| {
            defaults_version_from_numeric(value)
                .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
        })
        .transpose()
}

fn decode_position(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionInputPosition, SubmitInputRepositoryError> {
    let value: Decimal = required(row, field)?;
    input_position_from_numeric(value)
        .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_optional_position(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<SessionInputPosition>, SubmitInputRepositoryError> {
    value
        .map(|value| {
            input_position_from_numeric(value)
                .map_err(|reason| SubmitInputCorruption::InvalidOrdinal { field, reason }.into())
        })
        .transpose()
}

fn decode_defaults(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    field: &'static str,
) -> Result<SessionConfigurationDefaults, SubmitInputRepositoryError> {
    Ok(SessionConfigurationDefaults::new(decode_model_selection(
        kind, direct, alias, field,
    )?))
}

fn decode_model_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    field: &'static str,
) -> Result<ModelSelectionRequest, SubmitInputRepositoryError> {
    match (kind.as_str(), direct, alias) {
        ("direct", Some(selection), None) => Ok(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("alias", None, Some(alias)) => {
            Ok(ModelSelectionRequest::Alias(ModelAlias::from_uuid(alias)))
        }
        ("direct" | "alias", _, _) => Err(SubmitInputCorruption::Inconsistent(field).into()),
        _ => Err(SubmitInputCorruption::Unsupported { field, value: kind }.into()),
    }
}

fn decode_frozen_model(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    alias_selected: Option<Uuid>,
) -> Result<FrozenModelSelection, SubmitInputRepositoryError> {
    match (kind.as_str(), direct, alias, alias_selected) {
        ("direct", Some(selection), None, None) => Ok(FrozenModelSelection::Direct(
            DirectModelSelection::from_uuid(selection),
        )),
        ("frozen_alias", None, Some(alias), Some(selected)) => {
            Ok(FrozenModelSelection::FrozenAlias {
                alias: ModelAlias::from_uuid(alias),
                definition: FrozenAliasDefinition::selecting(DirectModelSelection::from_uuid(
                    selected,
                )),
            })
        }
        ("direct" | "frozen_alias", _, _, _) => {
            Err(SubmitInputCorruption::Inconsistent("frozen model").into())
        }
        _ => Err(SubmitInputCorruption::Unsupported {
            field: "frozen_model_kind",
            value: kind,
        }
        .into()),
    }
}

fn decode_model_call_disposition(
    value: &str,
) -> Result<ModelCallDisposition, SubmitInputRepositoryError> {
    match value {
        "completed" => Ok(ModelCallDisposition::Completed),
        "known_failed" => Ok(ModelCallDisposition::KnownFailed),
        "refused" => Ok(ModelCallDisposition::Refused),
        "cancelled" => Ok(ModelCallDisposition::Cancelled),
        "ambiguous" => Ok(ModelCallDisposition::Ambiguous),
        value => Err(SubmitInputCorruption::Unsupported {
            field: "model call terminal_disposition_kind",
            value: value.to_owned(),
        }
        .into()),
    }
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, SubmitInputRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(map_registry_error)
}

fn map_registry_error(error: RegistryInspectionError) -> SubmitInputRepositoryError {
    match error {
        RegistryInspectionError::Database(error) => error.into(),
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
            SubmitInputCorruption::Unsupported {
                field: "registry_kind",
                value,
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(value)) => {
            SubmitInputCorruption::Unsupported {
                field: "registry_version",
                value: value.to_string(),
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
            SubmitInputCorruption::Missing("typed_command_id").into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
            SubmitInputCorruption::Inconsistent("typed command family").into()
        }
    }
}

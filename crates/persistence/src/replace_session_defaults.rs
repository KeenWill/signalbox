//! Atomic persistence and replay for session-defaults replacement.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_application::{
    PromptMemberStatement, ReplaceSessionDefaultsOutcome, ReplaceSessionDefaultsTransaction,
};
use signalbox_domain::{
    DirectModelSelection, DurableCommandId, ModelAlias, ModelSelectionRequest,
    PreparedReplaceSessionDefaults, ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaults,
    ReplaceSessionDefaultsAppliedResult, ReplaceSessionDefaultsReconstitutionFailure,
    ReplaceSessionDefaultsReconstitutionInput, ReplaceSessionDefaultsRejectedResult,
    ReplaceSessionDefaultsResult, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SessionModelSettingsChanged,
    VersionedSessionConfigurationDefaults,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CommandKind, REPLACE_SESSION_DEFAULTS_KIND, RegistryCorruption,
        RegistryInspectionError,
    },
    lock_inventory,
    mapping::{
        PositiveOrdinalMappingError, dangerous_tool_auto_approval_from_str,
        dangerous_tool_auto_approval_to_str, defaults_version_from_numeric,
        defaults_version_to_numeric, durable_command_id_to_uuid,
        model_change_adjustments_from_json, model_settings_from_json,
        model_settings_overlay_from_json, model_settings_overlay_to_json, model_settings_to_json,
        session_id_from_uuid, session_id_to_uuid,
    },
    session::{SessionCorruption, SessionRepositoryError, load_session_from_connection},
};

const STORAGE_VERSION: i16 = 4;
pub(crate) const MODEL_SETTINGS_FROM_STORAGE_VERSION: i16 = 4;
const APPLIED: &str = "applied";
const REJECTED: &str = "rejected";
const SESSION_NOT_FOUND: &str = "session_not_found";
const CURRENT_VERSION_MISMATCH: &str = "current_version_mismatch";
const VERSION_EXHAUSTED: &str = "version_exhausted";

/// The committed outcome of handling one defaults-replacement command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplaceSessionDefaultsHandlingOutcome {
    /// First handling or equal replay returns the recorded application.
    Applied(ReplaceSessionDefaultsAppliedResult),
    /// First handling or equal replay returns the recorded rejection.
    Rejected(ReplaceSessionDefaultsRejectedResult),
    /// The identifier already has a structurally different user-global use.
    ConflictingReuse {
        /// The user-global identifier whose existing meaning is retained.
        command_id: DurableCommandId,
    },
    /// An unstated prompt member met a prompted current epoch under the CAS
    /// lock; the whole transaction rolled back and nothing — not even the
    /// command identity — was recorded.
    PromptRequiresStatedMember,
}

/// Result of admitting a replacement only when its expected version is stale.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplaceSessionDefaultsRejectionOnlyOutcome {
    /// The atomic boundary recorded or replayed the command's terminal result.
    Handled(ReplaceSessionDefaultsHandlingOutcome),
    /// The expected version was current under the session pointer lock, so
    /// the command claim was rolled back without applying the placeholder.
    CurrentVersionMatched,
}

/// A durable shape that cannot reconstruct one recorded replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplaceSessionDefaultsCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// Typed record relationships or variant fields disagree.
    Inconsistent(&'static str),
    /// A stored positive ordinal cannot construct a domain version.
    InvalidOrdinal {
        /// The ordinal-bearing record field.
        field: &'static str,
        /// Why the numeric value is outside the domain.
        reason: PositiveOrdinalMappingError,
    },
    /// The current session projection is incomplete or invalid.
    CurrentSession(SessionCorruption),
    /// Complete checked receipt values fail domain-owned correlation.
    Domain(ReplaceSessionDefaultsReconstitutionFailure),
}

impl fmt::Display for ReplaceSessionDefaultsCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => {
                write!(formatter, "missing durable ReplaceSessionDefaults {field}")
            }
            Self::Unsupported { field, value } => {
                write!(
                    formatter,
                    "unsupported ReplaceSessionDefaults {field}: {value}"
                )
            }
            Self::Inconsistent(relationship) => {
                write!(
                    formatter,
                    "inconsistent ReplaceSessionDefaults {relationship}"
                )
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(
                    formatter,
                    "invalid ReplaceSessionDefaults {field}: {reason}"
                )
            }
            Self::CurrentSession(error) => {
                write!(
                    formatter,
                    "ReplaceSessionDefaults current Session is invalid: {error}"
                )
            }
            Self::Domain(failure) => write!(
                formatter,
                "ReplaceSessionDefaults domain reconstitution failed: {failure:?}"
            ),
        }
    }
}

impl Error for ReplaceSessionDefaultsCorruption {}

/// A database failure, wrong purpose-specific load, or integrity failure.
#[derive(Debug)]
pub enum ReplaceSessionDefaultsRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database {
        /// The underlying SQLx failure.
        source: sqlx::Error,
        /// Whether the failure occurred while awaiting commit.
        commit_ambiguous: bool,
    },
    /// A purpose-specific load named a valid command of another admitted kind.
    DifferentCommandKind {
        /// The user-global identifier that names another kind.
        command_id: DurableCommandId,
    },
    /// Durable records cannot reconstruct the requested domain value.
    Corruption(ReplaceSessionDefaultsCorruption),
}

impl fmt::Display for ReplaceSessionDefaultsRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(
                    formatter,
                    "ReplaceSessionDefaults database failure: {source}"
                )
            }
            Self::DifferentCommandKind { command_id } => write!(
                formatter,
                "durable command {command_id:?} does not name ReplaceSessionDefaults"
            ),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ReplaceSessionDefaultsRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::DifferentCommandKind { .. } => None,
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ReplaceSessionDefaultsRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::from_database(error, false)
    }
}

impl ReplaceSessionDefaultsRepositoryError {
    fn from_database(source: sqlx::Error, commit_ambiguous: bool) -> Self {
        Self::Database {
            source,
            commit_ambiguous,
        }
    }

    fn from_commit_failure(source: sqlx::Error) -> Self {
        let commit_ambiguous = crate::commit_failure_is_ambiguous(&source);
        Self::from_database(source, commit_ambiguous)
    }
}

impl From<ReplaceSessionDefaultsCorruption> for ReplaceSessionDefaultsRepositoryError {
    fn from(error: ReplaceSessionDefaultsCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of atomic defaults replacement.
#[derive(Clone, Debug)]
pub struct ReplaceSessionDefaultsRepository {
    pool: PgPool,
}

impl ReplaceSessionDefaultsRepository {
    /// Uses the supplied pool for atomic handling and complete receipt loads.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claims and handles an unseen command, or resolves its recorded meaning.
    ///
    /// User-global command lookup is the first durable read. Mutable session
    /// state is consulted only for an unseen identifier.
    pub async fn handle(
        &self,
        command: ReplaceSessionDefaults,
    ) -> Result<ReplaceSessionDefaultsHandlingOutcome, ReplaceSessionDefaultsRepositoryError> {
        self.handle_where_prompt_member(command, PromptMemberStatement::Stated)
            .await
    }

    /// Handles one replacement whose caller may be unable to state the
    /// system-prompt member.
    ///
    /// For an `Unstated` member the prompted-current-epoch check runs after
    /// the pointer compare-and-set, under that row lock and against the
    /// immutable expected epoch, so no concurrent replacement can interleave;
    /// refusal rolls back the complete transaction including the command
    /// claim (docs/spec/process-protocol.md).
    pub async fn handle_where_prompt_member(
        &self,
        command: ReplaceSessionDefaults,
        prompt_member: PromptMemberStatement,
    ) -> Result<ReplaceSessionDefaultsHandlingOutcome, ReplaceSessionDefaultsRepositoryError> {
        match self
            .handle_conditionally(command, prompt_member, false)
            .await?
        {
            ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(outcome) => Ok(outcome),
            ReplaceSessionDefaultsRejectionOnlyOutcome::CurrentVersionMatched => {
                Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "unconditional replacement was not handled",
                )
                .into())
            }
        }
    }

    /// Records a stale-version result atomically but never applies the supplied
    /// replacement if its expected version has become current.
    pub async fn handle_rejection_only_where_prompt_member(
        &self,
        command: ReplaceSessionDefaults,
        prompt_member: PromptMemberStatement,
    ) -> Result<ReplaceSessionDefaultsRejectionOnlyOutcome, ReplaceSessionDefaultsRepositoryError>
    {
        self.handle_conditionally(command, prompt_member, true)
            .await
    }

    async fn handle_conditionally(
        &self,
        command: ReplaceSessionDefaults,
        prompt_member: PromptMemberStatement,
        rejection_only: bool,
    ) -> Result<ReplaceSessionDefaultsRejectionOnlyOutcome, ReplaceSessionDefaultsRepositoryError>
    {
        let command_id = command.command_id();
        let mut transaction = self.pool.begin().await?;

        match inspect_registry(&mut transaction, command_id).await? {
            Some(CommandKind::ReplaceSessionDefaults) => {
                let recorded = load_from_connection(&mut transaction, command_id)
                    .await?
                    .ok_or(ReplaceSessionDefaultsCorruption::Inconsistent(
                        "registry entry disappeared",
                    ))?;
                let outcome = existing_outcome(&command, &recorded);
                transaction.rollback().await?;
                return Ok(ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(outcome));
            }
            Some(CommandKind::CreateSession | CommandKind::CreateSessionFromImportedFrontier) => {
                transaction.rollback().await?;
                return Ok(ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(
                    ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id },
                ));
            }
            Some(
                CommandKind::ReplaceSessionMetadata
                | CommandKind::SubmitInput
                | CommandKind::DecideToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::PromotePendingRunner
                | CommandKind::AbandonLostRunner
                | CommandKind::ReplaceLostRunner,
            ) => {
                transaction.rollback().await?;
                return Ok(ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(
                    ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id },
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
        .bind(REPLACE_SESSION_DEFAULTS_KIND)
        .bind(STORAGE_VERSION)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        if !claimed {
            let outcome = match inspect_registry(&mut transaction, command_id).await? {
                Some(CommandKind::ReplaceSessionDefaults) => {
                    let recorded = load_from_connection(&mut transaction, command_id)
                        .await?
                        .ok_or(ReplaceSessionDefaultsCorruption::Inconsistent(
                            "winner claim disappeared",
                        ))?;
                    existing_outcome(&command, &recorded)
                }
                Some(
                    CommandKind::CreateSession
                    | CommandKind::CreateSessionFromImportedFrontier
                    | CommandKind::ReplaceSessionMetadata
                    | CommandKind::SubmitInput
                    | CommandKind::DecideToolRequest
                    | CommandKind::ReviewWorkflow
                    | CommandKind::ReviewOrchestration
                    | CommandKind::CompactSession
                    | CommandKind::Goal
                    | CommandKind::UpdateSessionPlacement
                    | CommandKind::RegisterWorkspace
                    | CommandKind::MintGitRemote
                    | CommandKind::WithdrawGitRemote
                    | CommandKind::PromotePendingRunner
                    | CommandKind::AbandonLostRunner
                    | CommandKind::ReplaceLostRunner,
                ) => ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id },
                None => {
                    return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                        "winner claim disappeared",
                    )
                    .into());
                }
            };
            transaction.rollback().await?;
            return Ok(ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(outcome));
        }

        lock_current_defaults_pointer(&mut transaction, command.session()).await?;
        let (prepared, mut prior_defaults) =
            prepare_against_current(&mut transaction, command.clone()).await?;
        if rejection_only && matches!(prepared.result(), ReplaceSessionDefaultsResult::Applied(_)) {
            transaction.rollback().await?;
            return Ok(ReplaceSessionDefaultsRejectionOnlyOutcome::CurrentVersionMatched);
        }
        let prepared = match prepared.result() {
            ReplaceSessionDefaultsResult::Applied(applied) => {
                let updated = sqlx::query(
                    "UPDATE session_current_defaults
                     SET current_version = $3
                     WHERE session_id = $1
                       AND current_version = $2",
                )
                .bind(session_id_to_uuid(command.session()))
                .bind(defaults_version_to_numeric(
                    command.expected_current_version(),
                ))
                .bind(defaults_version_to_numeric(applied.installed().version()))
                .execute(&mut *transaction)
                .await?
                .rows_affected();

                if updated == 1 {
                    if prompt_member == PromptMemberStatement::Unstated
                        && expected_epoch_carries_prompt(
                            &mut transaction,
                            command.session(),
                            command.expected_current_version(),
                        )
                        .await?
                    {
                        transaction.rollback().await?;
                        return Ok(ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(
                            ReplaceSessionDefaultsHandlingOutcome::PromptRequiresStatedMember,
                        ));
                    }
                    insert_defaults_version(&mut transaction, applied).await?;
                    prepared.clone()
                } else if updated == 0 {
                    let (rederived, _) = prepare_against_current(&mut transaction, command).await?;
                    if matches!(rederived.result(), ReplaceSessionDefaultsResult::Applied(_)) {
                        transaction.rollback().await?;
                        return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                            "pointer compare-and-set lost without a version change",
                        )
                        .into());
                    }
                    prior_defaults = None;
                    rederived
                } else {
                    transaction.rollback().await?;
                    return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                        "pointer compare-and-set affected multiple rows",
                    )
                    .into());
                }
            }
            ReplaceSessionDefaultsResult::Rejected(_) => prepared.clone(),
        };

        let outcome = result_outcome(prepared.result());
        insert_typed_record(&mut transaction, &prepared).await?;
        insert_model_settings_event(&mut transaction, &prepared, prior_defaults.as_ref()).await?;
        transaction
            .commit()
            .await
            .map_err(ReplaceSessionDefaultsRepositoryError::from_commit_failure)?;
        Ok(ReplaceSessionDefaultsRejectionOnlyOutcome::Handled(outcome))
    }

    /// Loads one complete replacement receipt, or `None` only for an unseen ID.
    pub async fn load(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedReplaceSessionDefaults>, ReplaceSessionDefaultsRepositoryError>
    {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::ReplaceSessionDefaults) => {
                load_from_connection(&mut connection, command_id).await
            }
            Some(CommandKind::CreateSession | CommandKind::CreateSessionFromImportedFrontier) => {
                Err(ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { command_id })
            }
            Some(
                CommandKind::ReplaceSessionMetadata
                | CommandKind::SubmitInput
                | CommandKind::DecideToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::PromotePendingRunner
                | CommandKind::AbandonLostRunner
                | CommandKind::ReplaceLostRunner,
            ) => Err(ReplaceSessionDefaultsRepositoryError::DifferentCommandKind { command_id }),
        }
    }
}

impl ReplaceSessionDefaultsTransaction for ReplaceSessionDefaultsRepository {
    type Error = ReplaceSessionDefaultsRepositoryError;

    async fn handle(
        &mut self,
        command: ReplaceSessionDefaults,
        prompt_member: PromptMemberStatement,
    ) -> Result<ReplaceSessionDefaultsOutcome, Self::Error> {
        let outcome = ReplaceSessionDefaultsRepository::handle_where_prompt_member(
            self,
            command,
            prompt_member,
        )
        .await?;

        Ok(match outcome {
            ReplaceSessionDefaultsHandlingOutcome::Applied(result) => {
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Applied(
                    result,
                ))
            }
            ReplaceSessionDefaultsHandlingOutcome::Rejected(result) => {
                ReplaceSessionDefaultsOutcome::Recorded(ReplaceSessionDefaultsResult::Rejected(
                    result,
                ))
            }
            ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse { command_id } => {
                ReplaceSessionDefaultsOutcome::ConflictingReuse { command_id }
            }
            ReplaceSessionDefaultsHandlingOutcome::PromptRequiresStatedMember => {
                ReplaceSessionDefaultsOutcome::PromptRequiresStatedMember
            }
        })
    }
}

/// Serializes preparation with every replacement that can move this session's
/// current-defaults pointer. An absent row is left for domain preparation to
/// classify as session-not-found or durable corruption.
async fn lock_current_defaults_pointer(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let _: Option<Decimal> = sqlx::query_scalar(lock_inventory::REPLACE_SESSION_DEFAULTS_CURRENT)
        .bind(session_id_to_uuid(session))
        .fetch_optional(&mut *connection)
        .await?;
    Ok(())
}

/// Reads whether the immutable expected epoch carries a prompt.
///
/// Runs after the pointer compare-and-set in the same transaction, so the
/// pointer row lock serializes this read against concurrent replacements.
async fn expected_epoch_carries_prompt(
    connection: &mut PgConnection,
    session: SessionId,
    expected: SessionConfigurationDefaultsVersion,
) -> Result<bool, ReplaceSessionDefaultsRepositoryError> {
    let carried: Option<bool> = sqlx::query_scalar(
        "SELECT system_prompt IS NOT NULL
           FROM session_defaults_version
          WHERE session_id = $1
            AND version = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(defaults_version_to_numeric(expected))
    .fetch_optional(&mut *connection)
    .await?;
    carried.ok_or_else(|| {
        ReplaceSessionDefaultsCorruption::Inconsistent("expected epoch disappeared under lock")
            .into()
    })
}

async fn prepare_against_current(
    connection: &mut PgConnection,
    command: ReplaceSessionDefaults,
) -> Result<
    (
        PreparedReplaceSessionDefaults,
        Option<VersionedSessionConfigurationDefaults>,
    ),
    ReplaceSessionDefaultsRepositoryError,
> {
    match load_session_from_connection(connection, command.session()).await {
        Ok(Some(session)) => {
            let prior_defaults = session.current_configuration_defaults().clone();
            let prepared = command.prepare_against(&session).map_err(|_| {
                ReplaceSessionDefaultsCorruption::Inconsistent("current session ownership")
            })?;
            Ok((prepared, Some(prior_defaults)))
        }
        Ok(None) => Ok((command.prepare_session_not_found(), None)),
        Err(SessionRepositoryError::Database(error)) => Err(error.into()),
        Err(SessionRepositoryError::Corruption(error)) => {
            Err(ReplaceSessionDefaultsCorruption::CurrentSession(error).into())
        }
    }
}

fn existing_outcome(
    command: &ReplaceSessionDefaults,
    recorded: &ReconstitutedReplaceSessionDefaults,
) -> ReplaceSessionDefaultsHandlingOutcome {
    if command == recorded.command() {
        result_outcome(recorded.result())
    } else {
        ReplaceSessionDefaultsHandlingOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    }
}

fn result_outcome(result: &ReplaceSessionDefaultsResult) -> ReplaceSessionDefaultsHandlingOutcome {
    match result.clone() {
        ReplaceSessionDefaultsResult::Applied(result) => {
            ReplaceSessionDefaultsHandlingOutcome::Applied(result)
        }
        ReplaceSessionDefaultsResult::Rejected(result) => {
            ReplaceSessionDefaultsHandlingOutcome::Rejected(result)
        }
    }
}

async fn insert_defaults_version(
    connection: &mut PgConnection,
    applied: &ReplaceSessionDefaultsAppliedResult,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let installed = applied.installed();
    let selection = encode_selection(installed.defaults().model());
    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             dangerous_tool_auto_approval, system_prompt, model_settings)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(session_id_to_uuid(applied.session()))
    .bind(defaults_version_to_numeric(installed.version()))
    .bind(selection.kind)
    .bind(selection.direct)
    .bind(selection.alias)
    .bind(dangerous_tool_auto_approval_to_str(
        installed.defaults().dangerous_tool_auto_approval(),
    ))
    .bind(
        installed
            .defaults()
            .system_prompt()
            .map(signalbox_domain::SessionSystemPrompt::as_str),
    )
    .bind(model_settings_to_json(
        installed.defaults().model_settings(),
    ))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_typed_record(
    connection: &mut PgConnection,
    prepared: &PreparedReplaceSessionDefaults,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let command = prepared.command();
    let selection = encode_selection(command.replacement().model());
    let encoded_result = encode_result(prepared.result());

    sqlx::query(
        "INSERT INTO replace_session_defaults_command
            (command_id, command_kind, storage_version, session_id,
             expected_current_version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             dangerous_tool_auto_approval, system_prompt,
             replacement_model_settings, caller_model_settings, result_kind,
             rejection_kind, result_session_id,
             result_installed_version, result_expected_version,
             result_current_version)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(REPLACE_SESSION_DEFAULTS_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(defaults_version_to_numeric(
        command.expected_current_version(),
    ))
    .bind(selection.kind)
    .bind(selection.direct)
    .bind(selection.alias)
    .bind(dangerous_tool_auto_approval_to_str(
        command.replacement().dangerous_tool_auto_approval(),
    ))
    .bind(
        command
            .replacement()
            .system_prompt()
            .map(signalbox_domain::SessionSystemPrompt::as_str),
    )
    .bind(model_settings_to_json(
        command.replacement().model_settings(),
    ))
    .bind(model_settings_overlay_to_json(
        command.caller_model_settings(),
    ))
    .bind(encoded_result.result_kind)
    .bind(encoded_result.rejection_kind)
    .bind(session_id_to_uuid(encoded_result.session))
    .bind(encoded_result.installed_version)
    .bind(encoded_result.expected_version)
    .bind(encoded_result.current_version)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_model_settings_event(
    connection: &mut PgConnection,
    prepared: &PreparedReplaceSessionDefaults,
    prior: Option<&VersionedSessionConfigurationDefaults>,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let ReplaceSessionDefaultsResult::Applied(applied) = prepared.result() else {
        return Ok(());
    };
    let Some(prior) = prior else {
        return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
            "applied settings event prior defaults",
        )
        .into());
    };
    let command = prepared.command();
    let Some(event) = SessionModelSettingsChanged::try_new(
        command.session(),
        command.command_id(),
        prior.version(),
        applied.installed().version(),
        prior.defaults().model(),
        applied.installed().defaults().model(),
        prior.defaults().model_settings(),
        applied.installed().defaults().model_settings(),
        command.caller_model_settings(),
        command.model_settings_adjustments().to_vec(),
    ) else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO session_model_settings_changed
            (session_id, command_id, prior_defaults_version,
             installed_defaults_version, prior_model_settings,
             installed_model_settings, caller_model_settings, adjustments)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(session_id_to_uuid(event.session()))
    .bind(durable_command_id_to_uuid(event.command_id()))
    .bind(defaults_version_to_numeric(event.prior_defaults_version()))
    .bind(defaults_version_to_numeric(
        event.installed_defaults_version(),
    ))
    .bind(model_settings_to_json(event.prior_settings()))
    .bind(model_settings_to_json(event.installed_settings()))
    .bind(model_settings_overlay_to_json(event.caller_override()))
    .bind(crate::mapping::model_change_adjustments_to_json(
        event.adjustments(),
    ))
    .execute(&mut *connection)
    .await?;
    crate::outbox::append(
        connection,
        crate::outbox::OutboxEvent::SessionModelSettingsChanged {
            session: event.session(),
            installed_defaults_version: event.installed_defaults_version(),
        },
    )
    .await?;
    Ok(())
}

struct EncodedSelection {
    kind: &'static str,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
}

fn encode_selection(selection: ModelSelectionRequest) -> EncodedSelection {
    match selection {
        ModelSelectionRequest::Direct(value) => EncodedSelection {
            kind: "direct",
            direct: Some(value.into_uuid()),
            alias: None,
        },
        ModelSelectionRequest::Alias(value) => EncodedSelection {
            kind: "alias",
            direct: None,
            alias: Some(value.into_uuid()),
        },
    }
}

struct EncodedResult {
    result_kind: &'static str,
    rejection_kind: Option<&'static str>,
    session: signalbox_domain::SessionId,
    installed_version: Option<Decimal>,
    expected_version: Option<Decimal>,
    current_version: Option<Decimal>,
}

fn encode_result(result: &ReplaceSessionDefaultsResult) -> EncodedResult {
    match result.clone() {
        ReplaceSessionDefaultsResult::Applied(result) => EncodedResult {
            result_kind: APPLIED,
            rejection_kind: None,
            session: result.session(),
            installed_version: Some(defaults_version_to_numeric(result.installed().version())),
            expected_version: None,
            current_version: None,
        },
        ReplaceSessionDefaultsResult::Rejected(
            ReplaceSessionDefaultsRejectedResult::SessionNotFound(result),
        ) => EncodedResult {
            result_kind: REJECTED,
            rejection_kind: Some(SESSION_NOT_FOUND),
            session: result.session(),
            installed_version: None,
            expected_version: None,
            current_version: None,
        },
        ReplaceSessionDefaultsResult::Rejected(
            ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(result),
        ) => EncodedResult {
            result_kind: REJECTED,
            rejection_kind: Some(CURRENT_VERSION_MISMATCH),
            session: result.session(),
            installed_version: None,
            expected_version: Some(defaults_version_to_numeric(result.expected())),
            current_version: Some(defaults_version_to_numeric(result.current())),
        },
        ReplaceSessionDefaultsResult::Rejected(
            ReplaceSessionDefaultsRejectedResult::VersionExhausted(result),
        ) => EncodedResult {
            result_kind: REJECTED,
            rejection_kind: Some(VERSION_EXHAUSTED),
            session: result.session(),
            installed_version: None,
            expected_version: None,
            current_version: Some(defaults_version_to_numeric(result.current())),
        },
    }
}

async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedReplaceSessionDefaults>, ReplaceSessionDefaultsRepositoryError> {
    let row = sqlx::query(
        "SELECT
            command.command_kind AS registry_kind,
            command.storage_version AS registry_version,
            typed.command_id AS typed_command_id,
            typed.command_kind AS typed_kind,
            typed.storage_version AS typed_version,
            typed.session_id,
            typed.expected_current_version,
            typed.model_selection_kind AS command_model_kind,
            typed.direct_model_selection_id AS command_direct_id,
            typed.model_alias_id AS command_alias_id,
            typed.dangerous_tool_auto_approval AS command_tool_auto_approval,
            typed.system_prompt AS command_system_prompt,
            typed.replacement_model_settings AS command_model_settings,
            typed.caller_model_settings,
            typed.result_kind,
            typed.rejection_kind,
            typed.result_session_id,
            typed.result_installed_version,
            typed.result_expected_version,
            typed.result_current_version,
            installed.session_id AS installed_session_id,
            installed.version AS installed_version,
            installed.model_selection_kind AS installed_model_kind,
            installed.direct_model_selection_id AS installed_direct_id,
            installed.model_alias_id AS installed_alias_id,
            installed.dangerous_tool_auto_approval AS installed_tool_auto_approval,
            installed.system_prompt AS installed_system_prompt,
            installed.model_settings AS installed_model_settings,
            prior.session_id AS prior_session_id,
            prior.version AS prior_version,
            prior.model_selection_kind AS prior_model_kind,
            prior.direct_model_selection_id AS prior_direct_id,
            prior.model_alias_id AS prior_alias_id,
            prior.dangerous_tool_auto_approval AS prior_tool_auto_approval,
            prior.system_prompt AS prior_system_prompt,
            prior.model_settings AS prior_model_settings,
            settings_event.command_id AS settings_event_command_id,
            settings_event.prior_model_settings AS settings_event_prior_model_settings,
            settings_event.installed_model_settings AS settings_event_installed_model_settings,
            settings_event.caller_model_settings AS settings_event_caller_model_settings,
            settings_event.adjustments AS model_settings_adjustments
         FROM durable_command AS command
         LEFT JOIN replace_session_defaults_command AS typed
           ON typed.command_id = command.command_id
         LEFT JOIN session_defaults_version AS installed
          ON installed.session_id = typed.result_session_id
         AND installed.version = typed.result_installed_version
         LEFT JOIN session_defaults_version AS prior
           ON prior.session_id = typed.session_id
          AND prior.version = typed.expected_current_version
         LEFT JOIN session_model_settings_changed AS settings_event
           ON settings_event.command_id = typed.command_id
         WHERE command.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;

    row.map(|row| decode_complete(row, command_id)).transpose()
}

fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
) -> Result<ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaultsRepositoryError> {
    require_spelling(&row, "registry_kind", REPLACE_SESSION_DEFAULTS_KIND)?;
    let registry_version = require_supported_version(&row, "registry_version")?;
    let _: Uuid = required(&row, "typed_command_id")?;
    require_spelling(&row, "typed_kind", REPLACE_SESSION_DEFAULTS_KIND)?;
    let typed_version = require_supported_version(&row, "typed_version")?;
    if registry_version != typed_version {
        return Err(
            ReplaceSessionDefaultsCorruption::Inconsistent("command storage version").into(),
        );
    }

    let settings_event_command: Option<Uuid> = row.try_get("settings_event_command_id")?;
    let settings_event_prior: Option<Value> = row.try_get("settings_event_prior_model_settings")?;
    let settings_event_installed: Option<Value> =
        row.try_get("settings_event_installed_model_settings")?;
    let settings_event_caller: Option<Value> =
        row.try_get("settings_event_caller_model_settings")?;
    let settings_event_adjustments: Option<Value> = row.try_get("model_settings_adjustments")?;
    let (settings_event_values, adjustments) = match (
        settings_event_command,
        settings_event_prior,
        settings_event_installed,
        settings_event_caller,
        settings_event_adjustments,
    ) {
        (None, None, None, None, None) => (None, Vec::new()),
        (Some(event_command), Some(prior), Some(installed), Some(caller), Some(adjustments))
            if event_command == durable_command_id_to_uuid(command_id) =>
        {
            let adjustments = model_change_adjustments_from_json(adjustments).map_err(|_| {
                ReplaceSessionDefaultsCorruption::Inconsistent("settings adjustments")
            })?;
            (Some((prior, installed, caller)), adjustments)
        }
        _ => {
            return Err(
                ReplaceSessionDefaultsCorruption::Inconsistent("settings event shape").into(),
            );
        }
    };
    let command_caller_model_settings: Value = required(&row, "caller_model_settings")?;
    let caller_model_settings =
        model_settings_overlay_from_json(command_caller_model_settings.clone())
            .map_err(|_| ReplaceSessionDefaultsCorruption::Inconsistent("caller model settings"))?;
    if typed_version < MODEL_SETTINGS_FROM_STORAGE_VERSION
        && caller_model_settings != signalbox_domain::ModelSettingsOverlay::inherit_all()
    {
        return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
            "storage version without caller model settings",
        )
        .into());
    }
    let command = ReplaceSessionDefaults::with_model_settings_adjustments(
        command_id,
        session_id_from_uuid(required(&row, "session_id")?),
        decode_ordinal(&row, "expected_current_version")?,
        decode_selection(
            required(&row, "command_model_kind")?,
            row.try_get("command_direct_id")?,
            row.try_get("command_alias_id")?,
            StoredConfigurationFields {
                dangerous_tool_auto_approval: required(&row, "command_tool_auto_approval")?,
                system_prompt: row.try_get("command_system_prompt")?,
                model_settings: required(&row, "command_model_settings")?,
                storage_version: typed_version,
            },
            "command model selection",
        )?,
        caller_model_settings,
        adjustments,
    );
    let result_kind: String = required(&row, "result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);
    let installed: Option<Decimal> = row.try_get("result_installed_version")?;
    let expected: Option<Decimal> = row.try_get("result_expected_version")?;
    let current: Option<Decimal> = row.try_get("result_current_version")?;

    let input = match (result_kind.as_str(), rejection_kind.as_deref()) {
        (APPLIED, None) => {
            if expected.is_some() || current.is_some() {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "applied result fields",
                )
                .into());
            }
            let result_version = decode_optional_ordinal(installed, "result_installed_version")?
                .ok_or(ReplaceSessionDefaultsCorruption::Missing(
                    "result_installed_version",
                ))?;
            let installed_session = session_id_from_uuid(required(&row, "installed_session_id")?);
            let installed_version = decode_ordinal(&row, "installed_version")?;
            let installed_model_settings: Value = required(&row, "installed_model_settings")?;
            let installed_defaults = decode_selection(
                required(&row, "installed_model_kind")?,
                row.try_get("installed_direct_id")?,
                row.try_get("installed_alias_id")?,
                StoredConfigurationFields {
                    dangerous_tool_auto_approval: required(&row, "installed_tool_auto_approval")?,
                    system_prompt: row.try_get("installed_system_prompt")?,
                    model_settings: installed_model_settings.clone(),
                    storage_version: typed_version,
                },
                "installed model selection",
            )?;
            let prior_session = session_id_from_uuid(required(&row, "prior_session_id")?);
            let prior_version = decode_ordinal(&row, "prior_version")?;
            if prior_session != command.session()
                || prior_version != command.expected_current_version()
            {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "settings event prior defaults identity",
                )
                .into());
            }
            let prior_model_settings: Value = required(&row, "prior_model_settings")?;
            let prior_defaults = decode_selection(
                required(&row, "prior_model_kind")?,
                row.try_get("prior_direct_id")?,
                row.try_get("prior_alias_id")?,
                StoredConfigurationFields {
                    dangerous_tool_auto_approval: required(&row, "prior_tool_auto_approval")?,
                    system_prompt: row.try_get("prior_system_prompt")?,
                    model_settings: prior_model_settings.clone(),
                    storage_version: typed_version,
                },
                "prior model selection",
            )?;
            if let Some((event_prior, event_installed, event_caller)) = &settings_event_values
                && (event_prior != &prior_model_settings
                    || event_installed != &installed_model_settings
                    || event_caller != &command_caller_model_settings)
            {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "settings event evidence",
                )
                .into());
            }
            let records_settings_change = prior_defaults.model() != installed_defaults.model()
                || prior_defaults.model_settings() != installed_defaults.model_settings();
            let requires_settings_event =
                typed_version >= MODEL_SETTINGS_FROM_STORAGE_VERSION && records_settings_change;
            if settings_event_values.is_some() != requires_settings_event {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "settings change evidence",
                )
                .into());
            }
            ReplaceSessionDefaultsReconstitutionInput::applied(
                command,
                result_session,
                result_version,
                installed_session,
                installed_version,
                installed_defaults,
            )
        }
        (REJECTED, Some(SESSION_NOT_FOUND)) => {
            require_absent_result_versions(installed, expected, current)?;
            ReplaceSessionDefaultsReconstitutionInput::rejected_session_not_found(
                command,
                result_session,
            )
        }
        (REJECTED, Some(CURRENT_VERSION_MISMATCH)) => {
            if installed.is_some() {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "mismatch installed version",
                )
                .into());
            }
            let result_expected = decode_optional_ordinal(expected, "result_expected_version")?
                .ok_or(ReplaceSessionDefaultsCorruption::Missing(
                    "result_expected_version",
                ))?;
            let result_current = decode_optional_ordinal(current, "result_current_version")?
                .ok_or(ReplaceSessionDefaultsCorruption::Missing(
                    "result_current_version",
                ))?;
            ReplaceSessionDefaultsReconstitutionInput::rejected_current_version_mismatch(
                command,
                result_session,
                result_expected,
                result_current,
            )
        }
        (REJECTED, Some(VERSION_EXHAUSTED)) => {
            if installed.is_some() || expected.is_some() {
                return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
                    "exhaustion result fields",
                )
                .into());
            }
            let result_current = decode_optional_ordinal(current, "result_current_version")?
                .ok_or(ReplaceSessionDefaultsCorruption::Missing(
                    "result_current_version",
                ))?;
            ReplaceSessionDefaultsReconstitutionInput::rejected_version_exhausted(
                command,
                result_session,
                result_current,
            )
        }
        (APPLIED, Some(_)) | (REJECTED, None) => {
            return Err(
                ReplaceSessionDefaultsCorruption::Inconsistent("terminal result shape").into(),
            );
        }
        (REJECTED, Some(value)) => {
            return Err(ReplaceSessionDefaultsCorruption::Unsupported {
                field: "rejection_kind",
                value: value.to_owned(),
            }
            .into());
        }
        (value, _) => {
            return Err(ReplaceSessionDefaultsCorruption::Unsupported {
                field: "result_kind",
                value: value.to_owned(),
            }
            .into());
        }
    };

    input
        .reconstitute()
        .map_err(|error| ReplaceSessionDefaultsCorruption::Domain(error.failure()).into())
}

fn require_absent_result_versions(
    installed: Option<Decimal>,
    expected: Option<Decimal>,
    current: Option<Decimal>,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    if installed.is_none() && expected.is_none() && current.is_none() {
        Ok(())
    } else {
        Err(
            ReplaceSessionDefaultsCorruption::Inconsistent("session-not-found result fields")
                .into(),
        )
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, ReplaceSessionDefaultsRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ReplaceSessionDefaultsCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &str,
) -> Result<(), ReplaceSessionDefaultsRepositoryError> {
    let actual: String = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ReplaceSessionDefaultsCorruption::Unsupported {
            field,
            value: actual,
        }
        .into())
    }
}

fn require_supported_version(
    row: &PgRow,
    field: &'static str,
) -> Result<i16, ReplaceSessionDefaultsRepositoryError> {
    let actual: i16 = required(row, field)?;
    if matches!(actual, 1..=4) {
        Ok(actual)
    } else {
        Err(ReplaceSessionDefaultsCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

fn decode_ordinal(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, ReplaceSessionDefaultsRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| ReplaceSessionDefaultsCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_optional_ordinal(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<SessionConfigurationDefaultsVersion>, ReplaceSessionDefaultsRepositoryError> {
    value
        .map(|value| {
            defaults_version_from_numeric(value).map_err(|reason| {
                ReplaceSessionDefaultsCorruption::InvalidOrdinal { field, reason }.into()
            })
        })
        .transpose()
}

struct StoredConfigurationFields {
    dangerous_tool_auto_approval: String,
    system_prompt: Option<String>,
    model_settings: Value,
    storage_version: i16,
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    stored: StoredConfigurationFields,
    field: &'static str,
) -> Result<SessionConfigurationDefaults, ReplaceSessionDefaultsRepositoryError> {
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => ModelSelectionRequest::Alias(ModelAlias::from_uuid(value)),
        ("direct" | "alias", _, _) => {
            return Err(ReplaceSessionDefaultsCorruption::Inconsistent(field).into());
        }
        _ => {
            return Err(
                ReplaceSessionDefaultsCorruption::Unsupported { field, value: kind }.into(),
            );
        }
    };
    let dangerous_tool_auto_approval = dangerous_tool_auto_approval_from_str(
        &stored.dangerous_tool_auto_approval,
    )
    .ok_or_else(|| {
        ReplaceSessionDefaultsRepositoryError::from(ReplaceSessionDefaultsCorruption::Unsupported {
            field: "dangerous tool auto approval",
            value: stored.dangerous_tool_auto_approval,
        })
    })?;
    if stored.storage_version == 1
        && dangerous_tool_auto_approval != signalbox_domain::DangerousToolAutoApproval::Disabled
    {
        return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
            "version-one dangerous tool auto approval",
        )
        .into());
    }
    if stored.storage_version <= 2 && stored.system_prompt.is_some() {
        return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
            "pre-version-three system prompt",
        )
        .into());
    }
    let system_prompt = stored
        .system_prompt
        .map(|value| {
            signalbox_domain::SessionSystemPrompt::try_new(value).map_err(|_| {
                ReplaceSessionDefaultsCorruption::Inconsistent("system prompt admission")
            })
        })
        .transpose()?;
    let model_settings = model_settings_from_json(stored.model_settings)
        .map_err(|_| ReplaceSessionDefaultsCorruption::Inconsistent("model settings"))?;
    if stored.storage_version < MODEL_SETTINGS_FROM_STORAGE_VERSION
        && model_settings != signalbox_domain::ValidatedModelSettings::provider_defaults()
    {
        return Err(ReplaceSessionDefaultsCorruption::Inconsistent(
            "storage version without model settings",
        )
        .into());
    }
    SessionConfigurationDefaults::complete_with_model_settings(
        model,
        dangerous_tool_auto_approval,
        system_prompt,
        model_settings,
    )
    .ok_or_else(|| {
        ReplaceSessionDefaultsRepositoryError::from(ReplaceSessionDefaultsCorruption::Inconsistent(
            "model settings validation selection",
        ))
    })
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, ReplaceSessionDefaultsRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(map_registry_error)
}

fn map_registry_error(error: RegistryInspectionError) -> ReplaceSessionDefaultsRepositoryError {
    match error {
        RegistryInspectionError::Database(error) => error.into(),
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
            ReplaceSessionDefaultsCorruption::Unsupported {
                field: "registry_kind",
                value,
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(value)) => {
            ReplaceSessionDefaultsCorruption::Unsupported {
                field: "registry_version",
                value: value.to_string(),
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
            ReplaceSessionDefaultsCorruption::Missing("typed_command_id").into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
            ReplaceSessionDefaultsCorruption::Inconsistent("typed command family").into()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::ReplaceSessionDefaultsRepositoryError;

    #[test]
    fn lost_commit_response_is_typed_as_ambiguous() {
        let error = ReplaceSessionDefaultsRepositoryError::from_commit_failure(sqlx::Error::Io(
            io::Error::new(io::ErrorKind::ConnectionReset, "commit response was lost"),
        ));

        assert!(matches!(
            error,
            ReplaceSessionDefaultsRepositoryError::Database {
                commit_ambiguous: true,
                ..
            }
        ));
    }
}

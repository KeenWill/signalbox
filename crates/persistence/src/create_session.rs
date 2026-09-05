//! Atomic persistence and replay for the admitted `CreateSession` slice.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_application::{CreateSessionOutcome, CreateSessionTransaction};
use signalbox_domain::{
    CommandPrincipal, CommissionedDispatchId, CreateSessionAppliedResult,
    CreateSessionReconstitutionFailure, CreateSessionReconstitutionInput, DirectModelSelection,
    DispatchingModule, DurableCommandId, ModelAlias, ModelSelectionRequest, ModuleDispatch,
    PreparedCreateSession, ReconstitutedSessionCreation, RepoWatchDispatchId,
    RootPlacementGlobalReadIntent, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionOwnership, SessionPlacement, SessionPlacementEventKind, SessionPlacementPath,
    SessionPlacementVersion, SessionTemplateContentDigest, SessionTemplateName,
    SessionTemplateProvenance, StartGate, TranscriptAncestry, VersionedSessionPlacement,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::command_registry::{
    self, CommandKind, RegistryCorruption, RegistryInspectionError,
    create_session_storage_version_is_supported,
};
use crate::mapping::{
    PositiveOrdinalMappingError, dangerous_tool_auto_approval_from_str,
    dangerous_tool_auto_approval_to_str, defaults_version_from_numeric,
    defaults_version_to_numeric, durable_command_id_to_uuid, finish_condition_columns,
    finish_condition_from_columns, model_settings_from_json, model_settings_to_json,
    session_creation_cause_to_str, session_id_from_uuid, session_id_to_uuid,
    session_placement_event_kind_from_str, session_placement_event_kind_to_str,
};
use crate::outbox;

const COMMAND_KIND: &str = "create_session";
const WRITTEN_STORAGE_VERSION: i16 = 7;
const DANGEROUS_TOOL_AUTO_APPROVAL_FROM_STORAGE_VERSION: i16 = 2;
const SYSTEM_PROMPT_FROM_STORAGE_VERSION: i16 = 3;
const TEMPLATE_PROVENANCE_FROM_STORAGE_VERSION: i16 = 4;
const PLACEMENT_FROM_STORAGE_VERSION: i16 = 6;
pub(crate) const MODEL_SETTINGS_FROM_STORAGE_VERSION: i16 = 7;
const NO_ANCESTRY: &str = "none";
const APPLIED: &str = "applied";

/// The committed outcome of handling one prepared creation command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateSessionHandlingOutcome {
    /// First handling or equal replay returns the recorded applied result.
    Applied(CreateSessionAppliedResult),
    /// The identifier is already bound to a structurally different payload.
    ConflictingReuse {
        /// The user-global identifier whose earlier meaning is retained.
        command_id: DurableCommandId,
    },
}

/// A durable shape that cannot reconstruct the admitted domain value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CreateSessionCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// A typed record relationship disagrees with another durable record.
    Inconsistent(&'static str),
    /// A stored positive ordinal cannot construct the domain value.
    InvalidOrdinal {
        /// The ordinal-bearing record field.
        field: &'static str,
        /// Why the numeric value is outside the domain.
        reason: PositiveOrdinalMappingError,
    },
    /// Complete checked values fail domain-owned correlation.
    Domain(CreateSessionReconstitutionFailure),
}

impl fmt::Display for CreateSessionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing durable CreateSession {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported CreateSession {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent CreateSession {relationship}")
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(formatter, "invalid CreateSession {field}: {reason}")
            }
            Self::Domain(failure) => {
                write!(
                    formatter,
                    "CreateSession domain reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for CreateSessionCorruption {}

/// A database failure or a fail-closed durable-shape failure.
#[derive(Debug)]
pub enum CreateSessionRepositoryError {
    /// PostgreSQL failed before any commit could have succeeded.
    Database(sqlx::Error),
    /// PostgreSQL obscured whether the requested commit succeeded.
    CommitAmbiguous(sqlx::Error),
    /// A purpose-specific load named a valid command of another admitted kind.
    DifferentCommandKind {
        /// The user-global identifier that names another kind.
        command_id: DurableCommandId,
    },
    /// Committed or transaction-visible records cannot reconstruct the domain.
    Corruption(CreateSessionCorruption),
}

impl fmt::Display for CreateSessionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "CreateSession database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "CreateSession commit outcome is ambiguous: {error}"
                )
            }
            Self::DifferentCommandKind { command_id } => {
                write!(
                    formatter,
                    "durable command {command_id:?} does not name CreateSession"
                )
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for CreateSessionRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::DifferentCommandKind { .. } => None,
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for CreateSessionRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<CreateSessionCorruption> for CreateSessionRepositoryError {
    fn from(error: CreateSessionCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl CreateSessionRepositoryError {
    fn from_commit_failure(error: sqlx::Error) -> Self {
        if crate::commit_failure_is_ambiguous(&error) {
            Self::CommitAmbiguous(error)
        } else {
            Self::Database(error)
        }
    }
}

/// PostgreSQL implementation of the initial session-creation boundary.
#[derive(Clone, Debug)]
pub struct CreateSessionRepository {
    pool: PgPool,
    credential_pin: crate::SessionCredentialPin,
}

impl CreateSessionRepository {
    /// Uses the supplied pool for atomic handling and complete loads.
    pub fn new(pool: PgPool, credential_pin: crate::SessionCredentialPin) -> Self {
        Self {
            pool,
            credential_pin,
        }
    }

    /// Claims and applies a new command, or resolves replay from the winner.
    ///
    /// Lookup by user-global command identity is the first durable operation.
    /// All first-handling records commit together; no returned applied result
    /// precedes commit.
    pub async fn handle(
        &self,
        prepared: PreparedCreateSession,
    ) -> Result<CreateSessionHandlingOutcome, CreateSessionRepositoryError> {
        let command_id = prepared.command().command_id();
        let mut transaction = self.pool.begin().await?;

        match inspect_registry(&mut transaction, command_id).await? {
            Some(CommandKind::CreateSession) => {
                if commissioned_dispatch_claims(&mut transaction, command_id).await? {
                    transaction.rollback().await?;
                    return Ok(CreateSessionHandlingOutcome::ConflictingReuse { command_id });
                }
                let recorded = load_from_connection(&mut transaction, command_id)
                    .await?
                    .ok_or(CreateSessionCorruption::Inconsistent(
                        "registry entry disappeared",
                    ))?;
                let outcome = existing_outcome(&prepared, &recorded);
                transaction.rollback().await?;
                return Ok(outcome);
            }
            Some(
                CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata,
            ) => {
                transaction.rollback().await?;
                return Ok(CreateSessionHandlingOutcome::ConflictingReuse { command_id });
            }
            Some(
                CommandKind::SubmitInput
                | CommandKind::DecideToolRequest
                | CommandKind::OverrideDeniedToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle,
            ) => {
                transaction.rollback().await?;
                return Ok(CreateSessionHandlingOutcome::ConflictingReuse { command_id });
            }
            None => {}
        }

        let issuer =
            crate::command_registry::issuer_columns(signalbox_domain::CommandPrincipal::Operator);
        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at,
                 issuer_kind, issuer_module)
             VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command_id))
        .bind(COMMAND_KIND)
        .bind(WRITTEN_STORAGE_VERSION)
        .bind(issuer.0)
        .bind(issuer.1)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        if !claimed {
            let outcome = match inspect_registry(&mut transaction, command_id).await? {
                Some(CommandKind::CreateSession) => {
                    if commissioned_dispatch_claims(&mut transaction, command_id).await? {
                        CreateSessionHandlingOutcome::ConflictingReuse { command_id }
                    } else {
                        let recorded = load_from_connection(&mut transaction, command_id)
                            .await?
                            .ok_or(CreateSessionCorruption::Inconsistent(
                                "winner claim disappeared",
                            ))?;
                        existing_outcome(&prepared, &recorded)
                    }
                }
                Some(
                    CommandKind::CreateSessionFromImportedFrontier
                    | CommandKind::ReplaceSessionDefaults
                    | CommandKind::ReplaceSessionMetadata
                    | CommandKind::SubmitInput
                    | CommandKind::DecideToolRequest
                    | CommandKind::OverrideDeniedToolRequest
                    | CommandKind::ReviewWorkflow
                    | CommandKind::ReviewOrchestration
                    | CommandKind::CompactSession
                    | CommandKind::Goal
                    | CommandKind::UpdateSessionPlacement
                    | CommandKind::RegisterWorkspace
                    | CommandKind::MintGitRemote
                    | CommandKind::WithdrawGitRemote
                    | CommandKind::SessionLifecycle,
                ) => CreateSessionHandlingOutcome::ConflictingReuse { command_id },
                None => {
                    return Err(
                        CreateSessionCorruption::Inconsistent("winner claim disappeared").into(),
                    );
                }
            };
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let result = prepared.applied_result();
        if let Err(error) = insert_prepared(&mut transaction, prepared, &self.credential_pin).await
        {
            transaction.rollback().await?;
            return Err(error);
        }

        transaction
            .commit()
            .await
            .map_err(CreateSessionRepositoryError::from_commit_failure)?;
        Ok(CreateSessionHandlingOutcome::Applied(result))
    }

    /// Loads one complete claimed creation, or `None` only for an unseen ID.
    pub async fn load(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedSessionCreation>, CreateSessionRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::CreateSession) => {
                // A claim a committed commissioned dispatch also holds names
                // the commission wire operation, not an ordinary creation, so
                // every ordinary-create replay probe refuses it the same way
                // it refuses any other command kind.
                if commissioned_dispatch_claims(&mut connection, command_id).await? {
                    return Err(CreateSessionRepositoryError::DifferentCommandKind { command_id });
                }
                load_from_connection(&mut connection, command_id).await
            }
            Some(
                CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata
                | CommandKind::SubmitInput
                | CommandKind::DecideToolRequest
                | CommandKind::OverrideDeniedToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle,
            ) => Err(CreateSessionRepositoryError::DifferentCommandKind { command_id }),
        }
    }
}

impl CreateSessionTransaction for CreateSessionRepository {
    type Error = CreateSessionRepositoryError;

    async fn handle(
        &mut self,
        prepared: PreparedCreateSession,
    ) -> Result<CreateSessionOutcome, Self::Error> {
        let outcome = CreateSessionRepository::handle(self, prepared).await?;

        Ok(match outcome {
            CreateSessionHandlingOutcome::Applied(result) => CreateSessionOutcome::Applied(result),
            CreateSessionHandlingOutcome::ConflictingReuse { command_id } => {
                CreateSessionOutcome::ConflictingReuse { command_id }
            }
        })
    }
}

fn existing_outcome(
    prepared: &PreparedCreateSession,
    recorded: &ReconstitutedSessionCreation,
) -> CreateSessionHandlingOutcome {
    if prepared.command() == recorded.command() {
        CreateSessionHandlingOutcome::Applied(recorded.applied_result())
    } else {
        CreateSessionHandlingOutcome::ConflictingReuse {
            command_id: prepared.command().command_id(),
        }
    }
}

/// Reports whether a committed commissioned dispatch claims this identity.
///
/// The ordinary create-session wire operation and the commission wire
/// operation carry different intent, so an ordinary create retried against a
/// commission's command identity must refuse rather than replay the created
/// session as its own.
async fn commissioned_dispatch_claims(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<bool, CreateSessionRepositoryError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM commissioned_dispatch WHERE create_command_id = $1
        )",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_one(connection)
    .await?)
}

/// Claims one create-session command identity, reporting whether this
/// transaction won it.
///
/// A `false` return means another committed claim holds the identity; the
/// caller decides whether that is a replay, a conflicting reuse, or
/// corruption.
pub(crate) async fn claim_create_session_command(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
    principal: CommandPrincipal,
) -> Result<bool, CreateSessionRepositoryError> {
    let issuer = crate::command_registry::issuer_columns(principal);
    Ok(sqlx::query(
        "INSERT INTO durable_command
            (command_id, command_kind, storage_version, claimed_at,
             issuer_kind, issuer_module)
         VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .bind(COMMAND_KIND)
    .bind(WRITTEN_STORAGE_VERSION)
    .bind(issuer.0)
    .bind(issuer.1)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        == 1)
}

pub(crate) async fn insert_prepared(
    connection: &mut PgConnection,
    prepared: PreparedCreateSession,
    credential_pin: &crate::SessionCredentialPin,
) -> Result<(), CreateSessionRepositoryError> {
    let command = prepared.command();
    let session = prepared.session();
    let defaults = session.configuration_defaults();
    let stored_selection = encode_selection(defaults.defaults().model());

    let cause = command.provenance().cause();
    let (dispatching_module, dispatch_ref) = encode_module_dispatch(cause);
    sqlx::query(
        "INSERT INTO session
            (session_id, creation_cause, ancestry_kind,
             template_name, template_content_digest,
             dispatching_module, dispatch_ref)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(session_creation_cause_to_str(&cause))
    .bind(NO_ANCESTRY)
    .bind(
        session
            .template_provenance()
            .map(|value| value.name().as_str()),
    )
    .bind(
        session
            .template_provenance()
            .map(|value| value.content_digest().as_bytes().to_vec()),
    )
    .bind(dispatching_module)
    .bind(dispatch_ref)
    .execute(&mut *connection)
    .await?;
    crate::session_lifecycle::insert_created(
        connection,
        session.id(),
        &cause,
        command.ownership(),
        command.start_gate(),
        command.finish_condition(),
    )
    .await?;

    let (placement_path, root_intent) = encode_placement(session.placement().placement());
    sqlx::query(
        "INSERT INTO session_placement_event
            (session_id, version, prior_version, event_kind, placement_path,
             root_global_read_intent, provenance_command_id, recorded_at)
         VALUES ($1, 1, NULL, $2, $3, $4, $5, transaction_timestamp())",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(session_placement_event_kind_to_str(
        SessionPlacementEventKind::Created,
    ))
    .bind(placement_path)
    .bind(root_intent)
    .bind(durable_command_id_to_uuid(command.command_id()))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO session_current_placement (session_id, current_version)
         VALUES ($1, 1)",
    )
    .bind(session_id_to_uuid(session.id()))
    .execute(&mut *connection)
    .await?;

    crate::session_credentials::insert_initial_session_credential_event(
        connection,
        session_id_to_uuid(session.id()),
        durable_command_id_to_uuid(command.command_id()),
        "create_session",
        credential_pin,
    )
    .await?;

    sqlx::query(
        "INSERT INTO session_scheduler (session_id)
         VALUES ($1)",
    )
    .bind(session_id_to_uuid(session.id()))
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id,
             dangerous_tool_auto_approval, system_prompt, model_settings)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(defaults_version_to_numeric(defaults.version()))
    .bind(stored_selection.kind)
    .bind(stored_selection.direct)
    .bind(stored_selection.alias)
    .bind(dangerous_tool_auto_approval_to_str(
        defaults.defaults().dangerous_tool_auto_approval(),
    ))
    .bind(
        defaults
            .defaults()
            .system_prompt()
            .map(signalbox_domain::SessionSystemPrompt::as_str),
    )
    .bind(model_settings_to_json(defaults.defaults().model_settings()))
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, $2)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(defaults_version_to_numeric(defaults.version()))
    .execute(&mut *connection)
    .await?;

    insert_command_record(connection, command, prepared.applied_result()).await?;

    outbox::append(
        connection,
        outbox::OutboxEvent::SessionCreated {
            session: session.id(),
            cause,
            ownership: command.ownership(),
        },
    )
    .await?;

    Ok(())
}

/// Writes an applied creation's typed command record and `session_created`
/// receipt.
async fn insert_command_record(
    connection: &mut PgConnection,
    command: &signalbox_domain::CreateSession,
    applied: CreateSessionAppliedResult,
) -> Result<(), CreateSessionRepositoryError> {
    let cause = command.provenance().cause();
    let (dispatching_module, dispatch_ref) = encode_module_dispatch(cause);
    let (placement_path, root_intent) = encode_placement(command.placement());
    let command_selection = encode_selection(command.initial_configuration_defaults().model());
    let (finish_kind, finish_statement) = finish_condition_columns(command.finish_condition());
    let created_session = session_id_to_uuid(applied.session());
    sqlx::query(
        "INSERT INTO create_session_command
            (command_id, command_kind, storage_version,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             dangerous_tool_auto_approval, system_prompt, model_settings,
             template_name, template_content_digest,
             placement_path, root_global_read_intent,
             result_kind, created_session_id,
             dispatching_module, dispatch_ref,
             start_gate, ownership, finish_condition_kind, finish_condition)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18,
                 $19, $20, $21, $22, $23, $24)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(COMMAND_KIND)
    .bind(WRITTEN_STORAGE_VERSION)
    .bind(session_creation_cause_to_str(&cause))
    .bind(NO_ANCESTRY)
    .bind(defaults_version_to_numeric(
        SessionConfigurationDefaultsVersion::first(),
    ))
    .bind(command_selection.kind)
    .bind(command_selection.direct)
    .bind(command_selection.alias)
    .bind(dangerous_tool_auto_approval_to_str(
        command
            .initial_configuration_defaults()
            .dangerous_tool_auto_approval(),
    ))
    .bind(
        command
            .initial_configuration_defaults()
            .system_prompt()
            .map(signalbox_domain::SessionSystemPrompt::as_str),
    )
    .bind(model_settings_to_json(
        command.initial_configuration_defaults().model_settings(),
    ))
    .bind(
        command
            .template_provenance()
            .map(|value| value.name().as_str()),
    )
    .bind(
        command
            .template_provenance()
            .map(|value| value.content_digest().as_bytes().to_vec()),
    )
    .bind(placement_path)
    .bind(root_intent)
    .bind(APPLIED)
    .bind(created_session)
    .bind(dispatching_module)
    .bind(dispatch_ref)
    .bind(start_gate_to_str(command.start_gate()))
    .bind(ownership_to_str(command.ownership()))
    .bind(finish_kind)
    .bind(finish_statement)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

const fn start_gate_to_str(value: StartGate) -> &'static str {
    match value {
        StartGate::Open => "open",
        StartGate::Held => "held",
    }
}

fn start_gate_from_str(value: &str) -> Option<StartGate> {
    match value {
        "open" => Some(StartGate::Open),
        "held" => Some(StartGate::Held),
        _ => None,
    }
}

const fn ownership_to_str(value: SessionOwnership) -> &'static str {
    match value {
        SessionOwnership::Owned => "owned",
        SessionOwnership::Unmonitored => "unmonitored",
    }
}

fn ownership_from_str(value: &str) -> Option<SessionOwnership> {
    match value {
        "owned" => Some(SessionOwnership::Owned),
        "unmonitored" => Some(SessionOwnership::Unmonitored),
        _ => None,
    }
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

async fn load_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedSessionCreation>, CreateSessionRepositoryError> {
    let row = sqlx::query(
        "SELECT
            d.command_kind AS registry_kind,
            d.storage_version AS registry_version,
            c.command_id AS typed_command_id,
            c.command_kind AS typed_kind,
            c.storage_version AS typed_version,
            c.creation_cause AS command_cause,
            c.ancestry_kind AS command_ancestry,
            c.dispatching_module AS command_dispatching_module,
            c.dispatch_ref AS command_dispatch_ref,
            c.initial_defaults_version,
            c.model_selection_kind AS command_model_kind,
            c.direct_model_selection_id AS command_direct_id,
            c.model_alias_id AS command_alias_id,
            c.dangerous_tool_auto_approval AS command_tool_auto_approval,
            c.system_prompt AS command_system_prompt,
            c.model_settings AS command_model_settings,
            c.template_name AS command_template_name,
            c.template_content_digest AS command_template_digest,
            c.placement_path AS command_placement_path,
            c.root_global_read_intent AS command_root_intent,
            c.result_kind,
            c.start_gate,
            c.ownership,
            c.finish_condition_kind,
            c.finish_condition,
            c.created_session_id AS result_session_id,
            s.session_id AS stored_session_id,
            s.creation_cause AS stored_cause,
            s.ancestry_kind AS stored_ancestry,
            s.spawning_tool_request_id AS stored_spawning_request_id,
            s.dispatching_module AS stored_dispatching_module,
            s.dispatch_ref AS stored_dispatch_ref,
            s.template_name AS stored_template_name,
            s.template_content_digest AS stored_template_digest,
            v.session_id AS defaults_session_id,
            v.version AS stored_defaults_version,
            v.model_selection_kind AS stored_model_kind,
            v.direct_model_selection_id AS stored_direct_id,
            v.model_alias_id AS stored_alias_id,
            v.dangerous_tool_auto_approval AS stored_tool_auto_approval,
            v.system_prompt AS stored_system_prompt,
            v.model_settings AS stored_model_settings
            ,pe.version AS stored_placement_version
            ,pe.prior_version AS stored_placement_prior_version
            ,pe.event_kind AS stored_placement_event_kind
            ,pe.placement_path AS stored_placement_path
            ,pe.root_global_read_intent AS stored_root_intent
            ,placement_head.current_version AS current_placement_head_version
            ,current_placement.version AS current_placement_event_version
            ,EXISTS (
                SELECT 1
                  FROM session_placement_event AS later_placement
                 WHERE later_placement.session_id = placement_head.session_id
                   AND later_placement.version > placement_head.current_version
             ) AS current_placement_later_event_exists
         FROM durable_command AS d
         LEFT JOIN create_session_command AS c
           ON c.command_id = d.command_id
         LEFT JOIN session AS s
           ON s.session_id = c.created_session_id
         LEFT JOIN session_defaults_version AS v
           ON v.session_id = c.created_session_id
          AND v.version = c.initial_defaults_version
         LEFT JOIN session_placement_event AS pe
           ON pe.session_id = c.created_session_id
          AND pe.version = 1
          AND pe.provenance_command_id = c.command_id
         LEFT JOIN session_current_placement AS placement_head
           ON placement_head.session_id = c.created_session_id
         LEFT JOIN session_placement_event AS current_placement
           ON current_placement.session_id = placement_head.session_id
          AND current_placement.version = placement_head.current_version
         WHERE d.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;

    row.map(|row| decode_complete(row, command_id)).transpose()
}

fn decode_complete(
    row: PgRow,
    command_id: DurableCommandId,
) -> Result<ReconstitutedSessionCreation, CreateSessionRepositoryError> {
    require_spelling(&row, "registry_kind", COMMAND_KIND)?;
    let registry_version = require_supported_version(&row, "registry_version")?;
    let _: Uuid = required(&row, "typed_command_id")?;
    require_spelling(&row, "typed_kind", COMMAND_KIND)?;
    let typed_version = require_supported_version(&row, "typed_version")?;
    if registry_version != typed_version {
        return Err(CreateSessionCorruption::Inconsistent("command storage version").into());
    }
    let command_provenance = decode_provenance(
        required(&row, "command_cause")?,
        required(&row, "command_ancestry")?,
        None,
        row.try_get("command_dispatching_module")?,
        row.try_get("command_dispatch_ref")?,
    )?;
    let initial_version = decode_ordinal(&row, "initial_defaults_version")?;
    if initial_version != SessionConfigurationDefaultsVersion::first() {
        return Err(
            CreateSessionCorruption::Inconsistent("command initial defaults version").into(),
        );
    }
    let command_model_settings: Value = required(&row, "command_model_settings")?;
    let command_defaults = decode_selection(
        required(&row, "command_model_kind")?,
        row.try_get("command_direct_id")?,
        row.try_get("command_alias_id")?,
        StoredConfigurationFields {
            dangerous_tool_auto_approval: required(&row, "command_tool_auto_approval")?,
            system_prompt: row.try_get("command_system_prompt")?,
            model_settings: command_model_settings,
            storage_version: typed_version,
        },
        "command model selection",
    )?;
    let command_template_provenance = decode_template_provenance(
        row.try_get("command_template_name")?,
        row.try_get("command_template_digest")?,
        "command template provenance",
    )?;
    let command_placement = decode_placement(
        row.try_get("command_placement_path")?,
        required(&row, "command_root_intent")?,
        typed_version,
    )?;
    if !storage_version_supports_template_provenance(typed_version)
        && command_template_provenance.is_some()
    {
        return Err(
            CreateSessionCorruption::Inconsistent("pre-version-four template provenance").into(),
        );
    }
    if command_template_provenance.is_some() && command_defaults.system_prompt().is_none() {
        return Err(CreateSessionCorruption::Inconsistent(
            "template creation without system prompt",
        )
        .into());
    }
    let command = match command_template_provenance {
        Some(template_provenance) => {
            signalbox_domain::CreateSession::new_from_template_with_placement(
                command_id,
                command_provenance,
                template_provenance,
                command_defaults,
                command_placement,
            )
        }
        None => signalbox_domain::CreateSession::new_with_placement(
            command_id,
            command_provenance,
            command_defaults,
            command_placement,
        ),
    };
    let start_gate: String = required(&row, "start_gate")?;
    let ownership: String = required(&row, "ownership")?;
    let finish_condition = finish_condition_from_columns(
        row.try_get("finish_condition_kind")?,
        row.try_get("finish_condition")?,
    )
    .map_err(CreateSessionCorruption::Inconsistent)?;
    let command = command.with_lifecycle(
        start_gate_from_str(&start_gate).ok_or(CreateSessionCorruption::Unsupported {
            field: "start_gate",
            value: start_gate,
        })?,
        ownership_from_str(&ownership).ok_or(CreateSessionCorruption::Unsupported {
            field: "ownership",
            value: ownership,
        })?,
        finish_condition,
    );
    let result_kind: String = required(&row, "result_kind")?;
    if result_kind != APPLIED {
        return Err(CreateSessionCorruption::Unsupported {
            field: "result_kind",
            value: result_kind,
        }
        .into());
    }
    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);

    let stored_session_uuid: Uuid = required(&row, "stored_session_id")?;
    let stored_session = session_id_from_uuid(stored_session_uuid);
    let stored_provenance = decode_provenance(
        required(&row, "stored_cause")?,
        required(&row, "stored_ancestry")?,
        row.try_get("stored_spawning_request_id")?,
        row.try_get("stored_dispatching_module")?,
        row.try_get("stored_dispatch_ref")?,
    )?;
    let stored_template_provenance = decode_template_provenance(
        row.try_get("stored_template_name")?,
        row.try_get("stored_template_digest")?,
        "stored template provenance",
    )?;
    if !storage_version_supports_template_provenance(typed_version)
        && stored_template_provenance.is_some()
    {
        return Err(
            CreateSessionCorruption::Inconsistent("pre-version-four template provenance").into(),
        );
    }
    let defaults_session: Uuid = required(&row, "defaults_session_id")?;
    if defaults_session != stored_session_uuid {
        return Err(CreateSessionCorruption::Inconsistent("session/defaults ownership").into());
    }
    let stored_version = decode_ordinal(&row, "stored_defaults_version")?;
    let stored_model_settings: Value = required(&row, "stored_model_settings")?;
    let stored_defaults = decode_selection(
        required(&row, "stored_model_kind")?,
        row.try_get("stored_direct_id")?,
        row.try_get("stored_alias_id")?,
        StoredConfigurationFields {
            dangerous_tool_auto_approval: required(&row, "stored_tool_auto_approval")?,
            system_prompt: row.try_get("stored_system_prompt")?,
            model_settings: stored_model_settings,
            storage_version: typed_version,
        },
        "stored model selection",
    )?;
    let stored_placement_version = placement_version_from_numeric(
        required(&row, "stored_placement_version")?,
        "stored placement version",
    )?;
    let stored_placement_prior: Option<Decimal> = row.try_get("stored_placement_prior_version")?;
    let stored_placement_event_kind_spelling: String =
        required(&row, "stored_placement_event_kind")?;
    let stored_placement_event_kind = session_placement_event_kind_from_str(
        &stored_placement_event_kind_spelling,
    )
    .ok_or(CreateSessionCorruption::Unsupported {
        field: "stored placement event kind",
        value: stored_placement_event_kind_spelling,
    })?;
    if stored_placement_version != SessionPlacementVersion::INITIAL
        || stored_placement_prior.is_some()
        || stored_placement_event_kind != SessionPlacementEventKind::Created
    {
        return Err(CreateSessionCorruption::Inconsistent("initial placement effect").into());
    }
    let stored_placement = decode_placement(
        row.try_get("stored_placement_path")?,
        required(&row, "stored_root_intent")?,
        PLACEMENT_FROM_STORAGE_VERSION,
    )?;
    let placement_head = placement_version_from_numeric(
        required(&row, "current_placement_head_version")?,
        "current placement head version",
    )?;
    let current_placement_event = placement_version_from_numeric(
        required(&row, "current_placement_event_version")?,
        "current placement event version",
    )?;
    if placement_head != current_placement_event {
        return Err(CreateSessionCorruption::Inconsistent("current placement head event").into());
    }
    let history_head_state =
        crate::session_placement::PlacementHistoryHeadState::from_later_event_exists(required(
            &row,
            "current_placement_later_event_exists",
        )?);
    match history_head_state {
        crate::session_placement::PlacementHistoryHeadState::MatchesLatestEvent => {}
        crate::session_placement::PlacementHistoryHeadState::BehindLaterEvent => {
            return Err(CreateSessionCorruption::Inconsistent(
                "session placement head behind event history",
            )
            .into());
        }
    }

    CreateSessionReconstitutionInput::new_with_template_and_placement(
        command,
        result_session,
        stored_session,
        stored_provenance,
        stored_template_provenance,
        session_id_from_uuid(defaults_session),
        stored_version,
        stored_defaults,
        VersionedSessionPlacement::reconstitute(stored_placement_version, stored_placement),
    )
    .reconstitute()
    .map_err(|error| CreateSessionCorruption::Domain(error.failure()).into())
}

fn encode_placement(placement: &SessionPlacement) -> (Option<&str>, bool) {
    (
        placement.path().map(SessionPlacementPath::as_str),
        placement.records_root_global_read_intent(),
    )
}

fn decode_placement(
    path: Option<String>,
    root_intent: bool,
    storage_version: i16,
) -> Result<SessionPlacement, CreateSessionRepositoryError> {
    if storage_version < PLACEMENT_FROM_STORAGE_VERSION {
        return if path.is_none() && !root_intent {
            Ok(SessionPlacement::pathless())
        } else {
            Err(CreateSessionCorruption::Inconsistent("pre-version-six placement").into())
        };
    }
    let Some(path) = path else {
        return if root_intent {
            Err(CreateSessionCorruption::Inconsistent("pathless root intent").into())
        } else {
            Ok(SessionPlacement::pathless())
        };
    };
    let path = SessionPlacementPath::try_new(path)
        .map_err(|_| CreateSessionCorruption::Inconsistent("placement path"))?;
    if root_intent {
        SessionPlacement::root_global_read(path, RootPlacementGlobalReadIntent::Acknowledged)
            .map_err(|_| CreateSessionCorruption::Inconsistent("root placement intent").into())
    } else {
        SessionPlacement::scoped(path)
            .map_err(|_| CreateSessionCorruption::Inconsistent("scoped placement").into())
    }
}

fn placement_version_from_numeric(
    value: Decimal,
    field: &'static str,
) -> Result<SessionPlacementVersion, CreateSessionRepositoryError> {
    let value = u64::try_from(value).map_err(|_| CreateSessionCorruption::Inconsistent(field))?;
    SessionPlacementVersion::try_from_u64(value)
        .ok_or_else(|| CreateSessionCorruption::Inconsistent(field).into())
}

fn decode_template_provenance(
    name: Option<String>,
    digest: Option<Vec<u8>>,
    relationship: &'static str,
) -> Result<Option<SessionTemplateProvenance>, CreateSessionRepositoryError> {
    match (name, digest) {
        (None, None) => Ok(None),
        (Some(name), Some(digest)) => {
            let name = SessionTemplateName::try_new(name)
                .map_err(|_| CreateSessionCorruption::Inconsistent(relationship))?;
            let digest: [u8; 32] = digest
                .try_into()
                .map_err(|_| CreateSessionCorruption::Inconsistent(relationship))?;
            Ok(Some(SessionTemplateProvenance::new(
                name,
                SessionTemplateContentDigest::from_bytes(digest),
            )))
        }
        (None, Some(_)) | (Some(_), None) => {
            Err(CreateSessionCorruption::Inconsistent(relationship).into())
        }
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, CreateSessionRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| CreateSessionCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &str,
) -> Result<(), CreateSessionRepositoryError> {
    let actual: String = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(CreateSessionCorruption::Unsupported {
            field,
            value: actual,
        }
        .into())
    }
}

fn require_supported_version(
    row: &PgRow,
    field: &'static str,
) -> Result<i16, CreateSessionRepositoryError> {
    let actual: i16 = required(row, field)?;
    if create_session_storage_version_is_supported(actual) {
        Ok(actual)
    } else {
        Err(CreateSessionCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

fn decode_ordinal(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, CreateSessionRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| CreateSessionCorruption::InvalidOrdinal { field, reason }.into())
}

/// Encodes the module and dispatch a module-dispatched creation names.
fn encode_module_dispatch(cause: SessionCreationCause) -> (Option<&'static str>, Option<Uuid>) {
    match cause {
        SessionCreationCause::ModuleDispatched { dispatch } => (
            Some(crate::mapping::dispatching_module_to_str(dispatch.module())),
            Some(module_dispatch_reference(dispatch)),
        ),
        SessionCreationCause::Interactive | SessionCreationCause::Delegated { .. } => (None, None),
    }
}

fn module_dispatch_reference(dispatch: ModuleDispatch) -> Uuid {
    match dispatch {
        ModuleDispatch::RepositoryWatch { dispatch } => dispatch.into_uuid(),
        ModuleDispatch::Commissioned { dispatch } => dispatch.into_uuid(),
    }
}

fn decode_provenance(
    cause: String,
    ancestry: String,
    spawning_request: Option<Uuid>,
    dispatching_module: Option<String>,
    dispatch_ref: Option<Uuid>,
) -> Result<SessionCreationProvenance, CreateSessionRepositoryError> {
    if ancestry != NO_ANCESTRY {
        return Err(CreateSessionCorruption::Unsupported {
            field: "ancestry kind",
            value: ancestry,
        }
        .into());
    }
    if spawning_request.is_some() {
        return Err(CreateSessionCorruption::Inconsistent("creation cause provenance").into());
    }
    match (cause.as_str(), dispatching_module, dispatch_ref) {
        ("interactive", None, None) => Ok(SessionCreationProvenance::new(
            SessionCreationCause::Interactive,
            TranscriptAncestry::None,
        )),
        ("module_dispatched", Some(module), Some(dispatch)) => {
            decode_module_dispatch(&module, dispatch)
                .map(SessionCreationProvenance::module_dispatched)
        }
        ("interactive" | "module_dispatched", _, _) => {
            Err(CreateSessionCorruption::Inconsistent("creation cause provenance").into())
        }
        _ => Err(CreateSessionCorruption::Unsupported {
            field: "creation cause",
            value: cause,
        }
        .into()),
    }
}

/// Rebuilds the exact dispatch a module-dispatched creation names.
fn decode_module_dispatch(
    module: &str,
    dispatch: Uuid,
) -> Result<ModuleDispatch, CreateSessionRepositoryError> {
    match crate::mapping::dispatching_module_from_str(module) {
        Some(DispatchingModule::RepositoryWatch) => Ok(ModuleDispatch::RepositoryWatch {
            dispatch: RepoWatchDispatchId::from_uuid(dispatch),
        }),
        Some(DispatchingModule::CommissionedDispatch) => Ok(ModuleDispatch::Commissioned {
            dispatch: CommissionedDispatchId::from_uuid(dispatch),
        }),
        None => Err(CreateSessionCorruption::Unsupported {
            field: "dispatching module",
            value: String::from(module),
        }
        .into()),
    }
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
) -> Result<SessionConfigurationDefaults, CreateSessionRepositoryError> {
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => ModelSelectionRequest::Alias(ModelAlias::from_uuid(value)),
        ("direct" | "alias", _, _) => {
            return Err(CreateSessionCorruption::Inconsistent(field).into());
        }
        _ => {
            return Err(CreateSessionCorruption::Unsupported { field, value: kind }.into());
        }
    };
    let dangerous_tool_auto_approval = dangerous_tool_auto_approval_from_str(
        &stored.dangerous_tool_auto_approval,
    )
    .ok_or_else(|| {
        CreateSessionRepositoryError::from(CreateSessionCorruption::Unsupported {
            field: "dangerous tool auto approval",
            value: stored.dangerous_tool_auto_approval,
        })
    })?;
    if stored.storage_version < DANGEROUS_TOOL_AUTO_APPROVAL_FROM_STORAGE_VERSION
        && dangerous_tool_auto_approval != signalbox_domain::DangerousToolAutoApproval::Disabled
    {
        return Err(CreateSessionCorruption::Inconsistent(
            "storage version without dangerous tool auto approval",
        )
        .into());
    }
    if stored.storage_version < SYSTEM_PROMPT_FROM_STORAGE_VERSION && stored.system_prompt.is_some()
    {
        return Err(
            CreateSessionCorruption::Inconsistent("storage version without system prompt").into(),
        );
    }
    let system_prompt = stored
        .system_prompt
        .map(|value| {
            signalbox_domain::SessionSystemPrompt::try_new(value)
                .map_err(|_| CreateSessionCorruption::Inconsistent("system prompt admission"))
        })
        .transpose()?;
    let model_settings = model_settings_from_json(stored.model_settings)
        .map_err(|_| CreateSessionCorruption::Inconsistent("model settings"))?;
    if stored.storage_version < MODEL_SETTINGS_FROM_STORAGE_VERSION
        && model_settings != signalbox_domain::ValidatedModelSettings::provider_defaults()
    {
        return Err(CreateSessionCorruption::Inconsistent(
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
        CreateSessionRepositoryError::from(CreateSessionCorruption::Inconsistent(
            "model settings validation selection",
        ))
    })
}

const fn storage_version_supports_template_provenance(storage_version: i16) -> bool {
    storage_version >= TEMPLATE_PROVENANCE_FROM_STORAGE_VERSION
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, CreateSessionRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(map_registry_error)
}

fn map_registry_error(error: RegistryInspectionError) -> CreateSessionRepositoryError {
    match error {
        RegistryInspectionError::Database(error) => error.into(),
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
            CreateSessionCorruption::Unsupported {
                field: "registry_kind",
                value,
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(value)) => {
            CreateSessionCorruption::Unsupported {
                field: "registry_version",
                value: value.to_string(),
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
            CreateSessionCorruption::Missing("typed_command_id").into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
            CreateSessionCorruption::Inconsistent("typed command family").into()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use signalbox_domain::SessionCreationCause;
    use sqlx::types::Uuid;

    use super::{
        CreateSessionCorruption, CreateSessionRepositoryError, NO_ANCESTRY,
        WRITTEN_STORAGE_VERSION, decode_provenance, storage_version_supports_template_provenance,
    };
    use crate::mapping::session_creation_cause_to_str;

    fn corruption(error: CreateSessionRepositoryError) -> CreateSessionCorruption {
        let CreateSessionRepositoryError::Corruption(corruption) = error else {
            panic!("the mapping failure is durable corruption")
        };
        corruption
    }

    /// S01 / INV-003: the ordinary creation reader cannot silently discard a
    /// delegated spawning identity from an interactive session row.
    #[test]
    fn s01_inv003_interactive_creation_rejects_spawning_request() {
        let error = decode_provenance(
            String::from(session_creation_cause_to_str(
                &SessionCreationCause::Interactive,
            )),
            String::from(NO_ANCESTRY),
            Some(Uuid::from_u128(1)),
            None,
            None,
        )
        .expect_err("interactive creation cannot carry a spawning request");

        assert_eq!(
            corruption(error),
            CreateSessionCorruption::Inconsistent("creation cause provenance")
        );
    }

    /// Version four rows carrying template provenance remain valid after the
    /// writer advances to a later storage version.
    #[test]
    fn writer_version_does_not_reinterpret_existing_template_provenance() {
        const EXISTING_TEMPLATE_PROVENANCE_STORAGE_VERSION: i16 = 4;
        const NEXT_WRITTEN_STORAGE_VERSION: i16 = WRITTEN_STORAGE_VERSION + 1;

        assert!(storage_version_supports_template_provenance(
            EXISTING_TEMPLATE_PROVENANCE_STORAGE_VERSION
        ));
        assert!(storage_version_supports_template_provenance(
            NEXT_WRITTEN_STORAGE_VERSION
        ));
    }

    #[test]
    fn lost_commit_response_is_typed_as_ambiguous() {
        let error = CreateSessionRepositoryError::from_commit_failure(sqlx::Error::Io(
            io::Error::new(io::ErrorKind::ConnectionReset, "commit response was lost"),
        ));

        assert!(matches!(
            error,
            CreateSessionRepositoryError::CommitAmbiguous(_)
        ));
    }
}

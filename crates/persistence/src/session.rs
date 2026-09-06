//! PostgreSQL loading for the current long-lived [`Session`] aggregate.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use serde_json::Value;
use signalbox_application::SessionReader;
use signalbox_domain::{
    CommissionedDispatchId, DirectModelSelection, DispatchingModule, ModelAlias,
    ModelSelectionRequest, ModuleDispatch, RepoWatchDispatchId, Session,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
    SessionCreationProvenance, SessionId, SessionPlacementEventKind, SessionPlacementVersion,
    SessionReconstitutionFailure, SessionReconstitutionInput, SessionTemplateContentDigest,
    SessionTemplateName, SessionTemplateProvenance, TranscriptAncestry, VersionedSessionPlacement,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::create_session_from_imported_frontier::{
    self, ImportedSessionCorruption, ImportedSessionRepositoryError,
};
use crate::mapping::{
    PositiveOrdinalMappingError, SessionCreationCauseStorageKind,
    dangerous_tool_auto_approval_from_str, defaults_version_from_numeric, model_settings_from_json,
    session_creation_cause_from_str, session_id_from_uuid, session_id_to_uuid,
    session_placement_event_kind_from_str, tool_request_id_from_uuid,
};

const NO_ANCESTRY: &str = "none";

/// A durable shape that cannot reconstruct one complete current session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator has no admitted storage mapping.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// A discriminator and its variant-specific fields disagree.
    Inconsistent(&'static str),
    /// A stored defaults version cannot construct the positive domain ordinal.
    InvalidOrdinal {
        /// The ordinal-bearing record field.
        field: &'static str,
        /// Why the numeric value is outside the domain.
        reason: PositiveOrdinalMappingError,
    },
    /// Complete checked values fail domain-owned aggregate correlation.
    Domain(SessionReconstitutionFailure),
    /// Imported ancestry or its immutable seed projection is corrupt.
    Imported(ImportedSessionCorruption),
}

impl fmt::Display for SessionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing durable Session {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported Session {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent Session {relationship}")
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(formatter, "invalid Session {field}: {reason}")
            }
            Self::Domain(failure) => {
                write!(
                    formatter,
                    "Session domain reconstitution failed: {failure:?}"
                )
            }
            Self::Imported(error) => error.fmt(formatter),
        }
    }
}

impl Error for SessionCorruption {}

/// A database failure or fail-closed current-session shape failure.
#[derive(Debug)]
pub enum SessionRepositoryError {
    /// PostgreSQL could not complete the load.
    Database(sqlx::Error),
    /// Durable records cannot reconstruct the requested session.
    Corruption(SessionCorruption),
}

impl fmt::Display for SessionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "Session database failure: {error}"),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for SessionRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for SessionRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SessionCorruption> for SessionRepositoryError {
    fn from(error: SessionCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of the current-session load boundary.
#[derive(Clone, Debug)]
pub struct SessionRepository {
    pool: PgPool,
}

impl SessionRepository {
    /// Uses the supplied pool for database-consistent current-session loads.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Loads one complete current session, or `None` only when its session row
    /// is absent from the statement snapshot.
    ///
    /// The query is driven by `session` and left-joins the authoritative
    /// current-defaults pointer and exactly the immutable defaults row selected
    /// by that pointer. Imported ancestry additionally joins its one-to-one
    /// seed record and seed-frontier header as a constant-size proof. Native
    /// template provenance and every selected defaults epoch additionally
    /// correlate the command storage version that introduced their durable
    /// fields. The selected placement is then checked against its complete
    /// authenticated event-and-receipt chain on the same connection.
    /// It intentionally loads no imported aggregate, frontier membership,
    /// semantic entry, turn history, or unselected defaults version.
    pub async fn load_session(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<Session>, SessionRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_session_from_connection(&mut connection, requested_session).await
    }
}

impl SessionReader for SessionRepository {
    type Error = SessionRepositoryError;

    async fn load_session(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<Session>, Self::Error> {
        SessionRepository::load_session(self, requested_session).await
    }
}

pub(crate) async fn load_session_from_connection(
    connection: &mut PgConnection,
    requested_session: SessionId,
) -> Result<Option<Session>, SessionRepositoryError> {
    let rows = sqlx::query(
        "SELECT
            s.session_id AS stored_session_id,
            s.creation_cause AS stored_cause,
            s.ancestry_kind AS stored_ancestry,
            s.spawning_tool_request_id AS stored_spawning_request_id,
            s.dispatching_module AS stored_dispatching_module,
            s.dispatch_ref AS stored_dispatch_ref,
            s.template_name AS stored_template_name,
            s.template_content_digest AS stored_template_digest,
            creation.storage_version AS create_storage_version,
            creation.model_settings AS create_command_model_settings,
            imported_creation.storage_version AS imported_create_storage_version,
            imported_creation.model_settings AS imported_create_command_model_settings,
            s.imported_conversation_id AS stored_conversation_id,
            s.imported_frontier_entry_id AS stored_frontier_entry_id,
            s.imported_frontier_position AS stored_frontier_position,
            s.imported_relationship_kind AS stored_relationship_kind,
            p.session_id AS current_defaults_session_id,
            p.current_version,
            v.session_id AS selected_defaults_session_id,
            v.version AS selected_defaults_version,
            v.model_selection_kind,
            v.direct_model_selection_id,
            v.model_alias_id,
            v.dangerous_tool_auto_approval,
            v.system_prompt,
            v.model_settings,
            defaults_replacement.storage_version AS replace_storage_version,
            defaults_replacement.replacement_model_settings
                AS replace_command_model_settings,
            seed.session_id AS seed_session_id,
            seed.seed_context_frontier_id,
            seed_frontier.owning_session_id AS seed_frontier_session_id,
            seed_frontier.context_frontier_id AS seed_frontier_id,
            seed_frontier.member_count AS seed_frontier_member_count
            ,placement_head.session_id AS current_placement_session_id
            ,placement_head.current_version AS current_placement_head_version
            ,placement.session_id AS current_placement_event_session_id
            ,placement.version AS current_placement_event_version
            ,placement.prior_version AS current_placement_prior_version
            ,placement.event_kind AS current_placement_event_kind
            ,placement.placement_path AS current_placement_path
            ,placement.root_global_read_intent AS current_placement_root_intent
            ,placement_native_creation.command_id AS current_native_creation_command_id
            ,placement_imported_creation.command_id AS current_imported_creation_command_id
            ,placement_update.command_id AS current_placement_update_command_id
            ,EXISTS (
                SELECT 1
                  FROM session_placement_event AS later_placement
                 WHERE later_placement.session_id = placement_head.session_id
                   AND later_placement.version > placement_head.current_version
             ) AS current_placement_later_event_exists
         FROM session AS s
         LEFT JOIN session_current_defaults AS p
           ON p.session_id = s.session_id
         LEFT JOIN session_defaults_version AS v
           ON v.session_id = p.session_id
          AND v.version = p.current_version
         LEFT JOIN create_session_command AS creation
           ON creation.created_session_id = s.session_id
         LEFT JOIN create_session_from_imported_frontier_command AS imported_creation
           ON imported_creation.created_session_id = s.session_id
         LEFT JOIN replace_session_defaults_command AS defaults_replacement
           ON defaults_replacement.result_session_id = v.session_id
          AND defaults_replacement.result_installed_version = v.version
          AND defaults_replacement.result_kind = 'applied'
         LEFT JOIN imported_session_seed AS seed
           ON seed.session_id = s.session_id
         LEFT JOIN context_frontier AS seed_frontier
           ON seed_frontier.owning_session_id = seed.session_id
          AND seed_frontier.context_frontier_id =
                  seed.seed_context_frontier_id
         LEFT JOIN session_current_placement AS placement_head
           ON placement_head.session_id = s.session_id
         LEFT JOIN session_placement_event AS placement
           ON placement.session_id = placement_head.session_id
          AND placement.version = placement_head.current_version
         LEFT JOIN create_session_command AS placement_native_creation
           ON placement_native_creation.command_id = placement.provenance_command_id
          AND placement_native_creation.created_session_id = placement.session_id
          AND placement_native_creation.result_kind = 'applied'
          AND placement_native_creation.placement_path
                IS NOT DISTINCT FROM placement.placement_path
          AND placement_native_creation.root_global_read_intent =
                placement.root_global_read_intent
         LEFT JOIN create_session_from_imported_frontier_command
                   AS placement_imported_creation
           ON placement_imported_creation.command_id = placement.provenance_command_id
          AND placement_imported_creation.created_session_id = placement.session_id
          AND placement_imported_creation.result_kind = 'applied'
          AND placement.placement_path IS NULL
          AND NOT placement.root_global_read_intent
         LEFT JOIN update_session_placement_command AS placement_update
           ON placement_update.command_id = placement.provenance_command_id
          AND placement_update.session_id = placement.session_id
          AND placement_update.result_kind = 'applied'
          AND placement_update.rejection_kind IS NULL
          AND placement_update.result_version = placement.version
          AND placement_update.result_current_version IS NULL
          AND placement_update.expected_version = placement.prior_version
          AND placement_update.replacement_path
                IS NOT DISTINCT FROM placement.placement_path
          AND placement_update.root_global_read_intent =
                placement.root_global_read_intent
         WHERE s.session_id = $1",
    )
    .bind(session_id_to_uuid(requested_session))
    .fetch_all(&mut *connection)
    .await?;

    let mut rows = rows.into_iter();
    let Some(row) = rows.next() else {
        return Ok(None);
    };
    if rows.next().is_some() {
        return Err(
            SessionCorruption::Inconsistent("current session projection cardinality").into(),
        );
    }

    let history_head_state =
        crate::session_placement::PlacementHistoryHeadState::from_later_event_exists(required(
            &row,
            "current_placement_later_event_exists",
        )?);
    let session = decode_complete(row, requested_session)?;
    crate::session_placement::authenticate_loaded_current(
        connection,
        requested_session,
        session.current_placement().clone(),
        history_head_state,
    )
    .await
    .map_err(map_placement_error)?;
    Ok(Some(session))
}

fn decode_complete(
    row: PgRow,
    requested_session: SessionId,
) -> Result<Session, SessionRepositoryError> {
    let ancestry: String = required(&row, "stored_ancestry")?;
    let settings_authentication = authenticate_defaults_settings_version(&row, &ancestry);
    if ancestry == "imported_conversation" {
        validate_imported_creation_provenance(
            required(&row, "stored_cause")?,
            row.try_get("stored_spawning_request_id")?,
            row.try_get("stored_dispatching_module")?,
            row.try_get("stored_dispatch_ref")?,
        )?;
        if row
            .try_get::<Option<String>, _>("stored_template_name")?
            .is_some()
            || row
                .try_get::<Option<Vec<u8>>, _>("stored_template_digest")?
                .is_some()
        {
            return Err(SessionCorruption::Inconsistent(
                "imported session has template provenance",
            )
            .into());
        }
        let placement =
            decode_current_placement(&row, PlacementCreationFamily::ImportedConversation)?;
        let session = create_session_from_imported_frontier::reconstitute_bounded_current(
            requested_session,
            row,
            placement.current_session,
            placement.current_version,
            placement.event_session,
            placement.placement,
        )
        .map_err(map_imported_error)?;
        settings_authentication?;
        return Ok(session);
    }
    if row.try_get::<Option<Uuid>, _>("seed_session_id")?.is_some() {
        return Err(
            SessionCorruption::Inconsistent("non-imported session has an imported seed").into(),
        );
    }
    let stored_session = session_id_from_uuid(required(&row, "stored_session_id")?);
    let provenance = decode_provenance(
        required(&row, "stored_cause")?,
        ancestry,
        row.try_get("stored_spawning_request_id")?,
        row.try_get("stored_dispatching_module")?,
        row.try_get("stored_dispatch_ref")?,
    )?;
    let template_provenance = decode_template_provenance(
        row.try_get("stored_template_name")?,
        row.try_get("stored_template_digest")?,
    )?;
    if template_provenance.is_some()
        && !matches!(
            row.try_get::<Option<i16>, _>("create_storage_version")?,
            Some(version) if version >= 4
        )
    {
        return Err(SessionCorruption::Inconsistent("pre-version-four template provenance").into());
    }
    let current_defaults_session =
        session_id_from_uuid(required(&row, "current_defaults_session_id")?);
    let current_defaults_version = decode_ordinal(&row, "current_version")?;
    let defaults_session = session_id_from_uuid(required(&row, "selected_defaults_session_id")?);
    let defaults_version = decode_ordinal(&row, "selected_defaults_version")?;
    let defaults = decode_selection(
        required(&row, "model_selection_kind")?,
        row.try_get("direct_model_selection_id")?,
        row.try_get("model_alias_id")?,
        required(&row, "dangerous_tool_auto_approval")?,
        row.try_get("system_prompt")?,
        required(&row, "model_settings")?,
    )?;
    let placement = decode_current_placement(&row, PlacementCreationFamily::Native)?;

    let session = SessionReconstitutionInput::new_with_template_and_placement(
        requested_session,
        stored_session,
        provenance,
        template_provenance,
        current_defaults_session,
        current_defaults_version,
        defaults_session,
        defaults_version,
        defaults,
        signalbox_domain::SessionPlacementReconstitutionFacts {
            current_pointer_session: placement.current_session,
            current_pointer_version: placement.current_version,
            selected_event_session: placement.event_session,
            selected_event: placement.placement,
        },
    )
    .reconstitute()
    .map_err(|error| SessionCorruption::Domain(error.failure()))?;
    settings_authentication?;
    Ok(session)
}

fn authenticate_defaults_settings_version(
    row: &PgRow,
    ancestry: &str,
) -> Result<(), SessionRepositoryError> {
    let defaults_version = decode_ordinal(row, "selected_defaults_version")?;
    let stored_model_settings: Value = required(row, "model_settings")?;
    let model_settings = model_settings_from_json(stored_model_settings.clone())
        .map_err(|_| SessionCorruption::Inconsistent("model settings"))?;
    if defaults_version != SessionConfigurationDefaultsVersion::first()
        && model_settings == signalbox_domain::ValidatedModelSettings::provider_defaults()
        && row
            .try_get::<Option<i16>, _>("replace_storage_version")?
            .is_none()
    {
        return Ok(());
    }
    let (storage_version, settings_cutover, command_settings_field): (i16, i16, &'static str) =
        if defaults_version == SessionConfigurationDefaultsVersion::first() {
            match ancestry {
                "none" => (
                    required(row, "create_storage_version")?,
                    crate::create_session::MODEL_SETTINGS_FROM_STORAGE_VERSION,
                    "create_command_model_settings",
                ),
                "imported_conversation" => (
                    required(row, "imported_create_storage_version")?,
                    create_session_from_imported_frontier::MODEL_SETTINGS_FROM_STORAGE_VERSION,
                    "imported_create_command_model_settings",
                ),
                _ => return Ok(()),
            }
        } else {
            (
                required(row, "replace_storage_version")?,
                crate::replace_session_defaults::MODEL_SETTINGS_FROM_STORAGE_VERSION,
                "replace_command_model_settings",
            )
        };
    let command_model_settings: Value = required(row, command_settings_field)?;
    if storage_version < settings_cutover {
        if model_settings != signalbox_domain::ValidatedModelSettings::provider_defaults() {
            return Err(SessionCorruption::Inconsistent(
                "defaults storage version without model settings",
            )
            .into());
        }
    } else if command_model_settings != stored_model_settings {
        return Err(SessionCorruption::Inconsistent(
            "defaults model settings disagree with command",
        )
        .into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PlacementCreationFamily {
    Native,
    ImportedConversation,
}

struct DecodedCurrentPlacement {
    current_session: SessionId,
    current_version: SessionPlacementVersion,
    event_session: SessionId,
    placement: VersionedSessionPlacement,
}

fn decode_current_placement(
    row: &PgRow,
    creation_family: PlacementCreationFamily,
) -> Result<DecodedCurrentPlacement, SessionRepositoryError> {
    let current_session = session_id_from_uuid(required(row, "current_placement_session_id")?);
    let current_version =
        crate::session_placement::decode_version(required(row, "current_placement_head_version")?)
            .map_err(|_| SessionCorruption::Inconsistent("current placement head version"))?;
    let event_session = session_id_from_uuid(required(row, "current_placement_event_session_id")?);
    let event_version =
        crate::session_placement::decode_version(required(row, "current_placement_event_version")?)
            .map_err(|_| SessionCorruption::Inconsistent("current placement version"))?;
    let prior = row
        .try_get::<Option<Decimal>, _>("current_placement_prior_version")?
        .map(crate::session_placement::decode_version)
        .transpose()
        .map_err(|_| SessionCorruption::Inconsistent("current placement prior version"))?;
    let event_kind_spelling: String = required(row, "current_placement_event_kind")?;
    let event_kind =
        session_placement_event_kind_from_str(&event_kind_spelling).ok_or_else(|| {
            SessionRepositoryError::from(SessionCorruption::Unsupported {
                field: "current placement event kind",
                value: event_kind_spelling,
            })
        })?;
    let native_creation: Option<Uuid> = row.try_get("current_native_creation_command_id")?;
    let imported_creation: Option<Uuid> = row.try_get("current_imported_creation_command_id")?;
    let update: Option<Uuid> = row.try_get("current_placement_update_command_id")?;
    let receipt_is_valid = match event_kind {
        SessionPlacementEventKind::Created => {
            event_version == SessionPlacementVersion::INITIAL
                && prior.is_none()
                && update.is_none()
                && match creation_family {
                    PlacementCreationFamily::Native => {
                        native_creation.is_some() && imported_creation.is_none()
                    }
                    PlacementCreationFamily::ImportedConversation => {
                        imported_creation.is_some() && native_creation.is_none()
                    }
                }
        }
        SessionPlacementEventKind::Updated => {
            prior.is_some()
                && update.is_some()
                && native_creation.is_none()
                && imported_creation.is_none()
        }
    };
    if !receipt_is_valid {
        return Err(SessionCorruption::Inconsistent("current placement provenance receipt").into());
    }
    let placement = crate::session_placement::decode_placement(
        row.try_get("current_placement_path")?,
        required(row, "current_placement_root_intent")?,
    )
    .map_err(|_| SessionCorruption::Inconsistent("current placement"))?;
    Ok(DecodedCurrentPlacement {
        current_session,
        current_version,
        event_session,
        placement: VersionedSessionPlacement::reconstitute(event_version, placement),
    })
}

fn decode_template_provenance(
    name: Option<String>,
    digest: Option<Vec<u8>>,
) -> Result<Option<SessionTemplateProvenance>, SessionRepositoryError> {
    const RELATIONSHIP: &str = "session template provenance";
    match (name, digest) {
        (None, None) => Ok(None),
        (Some(name), Some(digest)) => {
            let name = SessionTemplateName::try_new(name)
                .map_err(|_| SessionCorruption::Inconsistent(RELATIONSHIP))?;
            let digest: [u8; 32] = digest
                .try_into()
                .map_err(|_| SessionCorruption::Inconsistent(RELATIONSHIP))?;
            Ok(Some(SessionTemplateProvenance::new(
                name,
                SessionTemplateContentDigest::from_bytes(digest),
            )))
        }
        (None, Some(_)) | (Some(_), None) => {
            Err(SessionCorruption::Inconsistent(RELATIONSHIP).into())
        }
    }
}

fn map_imported_error(error: ImportedSessionRepositoryError) -> SessionRepositoryError {
    match error {
        ImportedSessionRepositoryError::Database(error) => SessionRepositoryError::Database(error),
        ImportedSessionRepositoryError::Corruption(error) => {
            SessionRepositoryError::Corruption(SessionCorruption::Imported(error))
        }
        ImportedSessionRepositoryError::CommitAmbiguous(_) => {
            SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
                "imported load reported an impossible commit ambiguity",
            ))
        }
        ImportedSessionRepositoryError::DifferentCommandKind { .. } => {
            SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
                "imported session lookup reached another command family",
            ))
        }
        ImportedSessionRepositoryError::Preparation(_) => {
            SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
                "imported load reported an impossible preparation failure",
            ))
        }
        ImportedSessionRepositoryError::IdentityCollision(_) => {
            SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
                "imported load reported an impossible identity collision",
            ))
        }
        ImportedSessionRepositoryError::ImportedConversation(
            crate::conversation_import::ImportedConversationRepositoryError::Database(error),
        ) => SessionRepositoryError::Database(error),
        ImportedSessionRepositoryError::ImportedConversation(_) => {
            SessionRepositoryError::Corruption(SessionCorruption::Inconsistent(
                "imported conversation load failed",
            ))
        }
    }
}

fn map_placement_error(
    error: crate::session_placement::SessionPlacementRepositoryError,
) -> SessionRepositoryError {
    use crate::session_placement::SessionPlacementRepositoryError;

    match error {
        SessionPlacementRepositoryError::Database(error)
        | SessionPlacementRepositoryError::CommitAmbiguous(error) => {
            SessionRepositoryError::Database(error)
        }
        SessionPlacementRepositoryError::InvalidCommandId => {
            SessionCorruption::Inconsistent("current placement command identity").into()
        }
        SessionPlacementRepositoryError::Corruption(reason) => {
            SessionCorruption::Inconsistent(reason).into()
        }
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SessionRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| SessionCorruption::Missing(field).into())
}

fn decode_ordinal(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, SessionRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| SessionCorruption::InvalidOrdinal { field, reason }.into())
}

fn decode_provenance(
    cause: String,
    ancestry: String,
    spawning_request: Option<Uuid>,
    dispatching_module: Option<String>,
    dispatch_ref: Option<Uuid>,
) -> Result<SessionCreationProvenance, SessionRepositoryError> {
    if ancestry != NO_ANCESTRY {
        return Err(SessionCorruption::Unsupported {
            field: "ancestry kind",
            value: ancestry,
        }
        .into());
    }
    let Some(cause_kind) = session_creation_cause_from_str(&cause) else {
        return Err(SessionCorruption::Unsupported {
            field: "creation cause",
            value: cause,
        }
        .into());
    };
    match (
        cause_kind,
        spawning_request,
        dispatching_module,
        dispatch_ref,
    ) {
        (SessionCreationCauseStorageKind::Interactive, None, None, None) => {
            Ok(SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ))
        }
        (SessionCreationCauseStorageKind::Delegated, Some(request), None, None) => Ok(
            SessionCreationProvenance::delegated(tool_request_id_from_uuid(request)),
        ),
        (SessionCreationCauseStorageKind::ModuleDispatched, None, Some(module), Some(dispatch)) => {
            decode_module_dispatch(&module, dispatch)
                .map(SessionCreationProvenance::module_dispatched)
        }
        (
            SessionCreationCauseStorageKind::Interactive
            | SessionCreationCauseStorageKind::Delegated
            | SessionCreationCauseStorageKind::ModuleDispatched,
            _,
            _,
            _,
        ) => Err(SessionCorruption::Inconsistent("creation cause provenance").into()),
    }
}

/// Rebuilds the exact dispatch a module-dispatched creation names.
///
/// The module spelling selects which identity kind the reference is, so a
/// dispatch identity never silently changes hands between modules.
fn decode_module_dispatch(
    module: &str,
    dispatch: Uuid,
) -> Result<ModuleDispatch, SessionRepositoryError> {
    match crate::mapping::dispatching_module_from_str(module) {
        Some(DispatchingModule::RepositoryWatch) => Ok(ModuleDispatch::RepositoryWatch {
            dispatch: RepoWatchDispatchId::from_uuid(dispatch),
        }),
        Some(DispatchingModule::CommissionedDispatch) => Ok(ModuleDispatch::Commissioned {
            dispatch: CommissionedDispatchId::from_uuid(dispatch),
        }),
        None => Err(SessionCorruption::Unsupported {
            field: "dispatching module",
            value: String::from(module),
        }
        .into()),
    }
}

fn validate_imported_creation_provenance(
    cause: String,
    spawning_request: Option<Uuid>,
    dispatching_module: Option<String>,
    dispatch_ref: Option<Uuid>,
) -> Result<(), SessionRepositoryError> {
    let Some(cause_kind) = session_creation_cause_from_str(&cause) else {
        return Err(SessionCorruption::Unsupported {
            field: "creation cause",
            value: cause,
        }
        .into());
    };
    match (
        cause_kind,
        spawning_request,
        dispatching_module,
        dispatch_ref,
    ) {
        (SessionCreationCauseStorageKind::Interactive, None, None, None) => Ok(()),
        (
            SessionCreationCauseStorageKind::Interactive
            | SessionCreationCauseStorageKind::Delegated
            | SessionCreationCauseStorageKind::ModuleDispatched,
            _,
            _,
            _,
        ) => Err(SessionCorruption::Inconsistent("creation cause provenance").into()),
    }
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    dangerous_tool_auto_approval: String,
    system_prompt: Option<String>,
    model_settings: Value,
) -> Result<SessionConfigurationDefaults, SessionRepositoryError> {
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => ModelSelectionRequest::Alias(ModelAlias::from_uuid(value)),
        ("direct" | "alias", _, _) => {
            return Err(SessionCorruption::Inconsistent("model selection").into());
        }
        _ => {
            return Err(SessionCorruption::Unsupported {
                field: "model selection kind",
                value: kind,
            }
            .into());
        }
    };
    let dangerous_tool_auto_approval =
        dangerous_tool_auto_approval_from_str(&dangerous_tool_auto_approval).ok_or_else(|| {
            SessionRepositoryError::from(SessionCorruption::Unsupported {
                field: "dangerous tool auto approval",
                value: dangerous_tool_auto_approval,
            })
        })?;
    let system_prompt = system_prompt
        .map(|value| {
            signalbox_domain::SessionSystemPrompt::try_new(value)
                .map_err(|_| SessionCorruption::Inconsistent("system prompt admission"))
        })
        .transpose()?;
    let model_settings = model_settings_from_json(model_settings)
        .map_err(|_| SessionCorruption::Inconsistent("model settings"))?;
    SessionConfigurationDefaults::complete_with_model_settings(
        model,
        dangerous_tool_auto_approval,
        system_prompt,
        model_settings,
    )
    .ok_or_else(|| {
        SessionRepositoryError::from(SessionCorruption::Inconsistent(
            "model settings validation selection",
        ))
    })
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{SessionCreationCause, TranscriptAncestry};
    use sqlx::types::Uuid;

    use super::{
        NO_ANCESTRY, SessionCorruption, SessionRepositoryError, decode_provenance,
        map_imported_error, validate_imported_creation_provenance,
    };
    use crate::{
        conversation_import::ImportedConversationRepositoryError,
        create_session_from_imported_frontier::ImportedSessionRepositoryError,
        mapping::session_creation_cause_to_str,
    };

    const NON_NONE_ANCESTRY: &str = "single_source";

    fn spawning_request() -> Uuid {
        Uuid::from_u128(1)
    }

    fn corruption(error: SessionRepositoryError) -> SessionCorruption {
        let SessionRepositoryError::Corruption(corruption) = error else {
            panic!("the mapping failure is durable corruption")
        };
        corruption
    }

    #[test]
    fn imported_conversation_database_failure_remains_retryable() {
        let error = map_imported_error(ImportedSessionRepositoryError::ImportedConversation(
            ImportedConversationRepositoryError::Database(sqlx::Error::PoolClosed),
        ));

        assert!(matches!(
            error,
            SessionRepositoryError::Database(sqlx::Error::PoolClosed)
        ));
    }

    /// S18: the durable delegated spelling retains its exact request.
    #[test]
    fn s18_delegated_provenance_decodes_exactly() {
        let request = spawning_request();
        let provenance = decode_provenance(
            String::from(session_creation_cause_to_str(
                &SessionCreationCause::Delegated {
                    spawning_request: signalbox_domain::ToolRequestId::from_uuid(request),
                },
            )),
            String::from(NO_ANCESTRY),
            Some(request),
            None,
            None,
        )
        .expect("the complete delegated storage shape decodes");

        assert_eq!(
            provenance.cause(),
            SessionCreationCause::Delegated {
                spawning_request: signalbox_domain::ToolRequestId::from_uuid(request),
            }
        );
        assert_eq!(provenance.ancestry(), TranscriptAncestry::None);
    }

    /// S18: delegated storage cannot omit its spawning request.
    #[test]
    fn s18_delegated_provenance_requires_spawning_request() {
        let delegated = SessionCreationCause::Delegated {
            spawning_request: signalbox_domain::ToolRequestId::from_uuid(spawning_request()),
        };
        let error = decode_provenance(
            String::from(session_creation_cause_to_str(&delegated)),
            String::from(NO_ANCESTRY),
            None,
            None,
            None,
        )
        .expect_err("delegated provenance without its request is corrupt");

        assert_eq!(
            corruption(error),
            SessionCorruption::Inconsistent("creation cause provenance")
        );
    }

    /// S01: interactive storage cannot claim a spawning request.
    #[test]
    fn s01_interactive_provenance_rejects_spawning_request() {
        let error = decode_provenance(
            String::from(session_creation_cause_to_str(
                &SessionCreationCause::Interactive,
            )),
            String::from(NO_ANCESTRY),
            Some(spawning_request()),
            None,
            None,
        )
        .expect_err("interactive provenance cannot carry delegated authority");

        assert_eq!(
            corruption(error),
            SessionCorruption::Inconsistent("creation cause provenance")
        );
    }

    /// S28: an imported interactive row cannot silently discard
    /// a contradictory delegated spawning identity.
    #[test]
    fn s28_imported_provenance_rejects_spawning_request() {
        let error = validate_imported_creation_provenance(
            String::from(session_creation_cause_to_str(
                &SessionCreationCause::Interactive,
            )),
            Some(spawning_request()),
            None,
            None,
        )
        .expect_err("imported interactive provenance cannot carry a spawning request");

        assert_eq!(
            corruption(error),
            SessionCorruption::Inconsistent("creation cause provenance")
        );
    }

    /// S18: delegated creation cannot acquire transcript ancestry.
    #[test]
    fn s18_delegated_provenance_rejects_non_none_ancestry() {
        let error =
            decode_provenance(
                String::from(session_creation_cause_to_str(
                    &SessionCreationCause::Delegated {
                        spawning_request: signalbox_domain::ToolRequestId::from_uuid(
                            spawning_request(),
                        ),
                    },
                )),
                String::from(NON_NONE_ANCESTRY),
                Some(spawning_request()),
                None,
                None,
            )
            .expect_err("delegated provenance cannot inherit transcript ancestry");

        assert_eq!(
            corruption(error),
            SessionCorruption::Unsupported {
                field: "ancestry kind",
                value: String::from(NON_NONE_ANCESTRY),
            }
        );
    }
}

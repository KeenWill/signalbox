//! Append-only session-placement history and explicit update replay.

use std::{collections::BTreeMap, error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{UpdateSessionPlacementOutcome, UpdateSessionPlacementTransaction};
use signalbox_domain::{
    DurableCommandId, RootPlacementGlobalReadIntent, SessionId, SessionPlacement,
    SessionPlacementEvent, SessionPlacementEventKind, SessionPlacementPath,
    SessionPlacementVersion, UpdateSessionPlacement, UpdateSessionPlacementApplied,
    UpdateSessionPlacementRejection, UpdateSessionPlacementResult, VersionedSessionPlacement,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};

use crate::command_registry::{
    self, CommandKind, RegistryCorruption, RegistryInspectionError, UPDATE_SESSION_PLACEMENT_KIND,
};
use crate::lock_inventory;
use crate::mapping::{
    SessionPlacementRejectionStorageKind, SessionPlacementResultStorageKind,
    durable_command_id_from_uuid, durable_command_id_to_uuid, session_id_from_uuid,
    session_id_to_uuid, session_placement_event_kind_from_str, session_placement_event_kind_to_str,
    session_placement_rejection_from_str, session_placement_rejection_to_str,
    session_placement_result_kind_from_str, session_placement_result_kind_to_str,
};

const STORAGE_VERSION: i16 = 1;
const AUTHENTICATION_PAGE_SIZE: i64 = 64;

/// First handling/equal replay or conflicting durable-command reuse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionPlacementRepositoryOutcome {
    Recorded(UpdateSessionPlacementResult),
    ConflictingReuse { command_id: DurableCommandId },
}

/// Database or fail-closed placement-history failure.
#[derive(Debug)]
pub enum SessionPlacementRepositoryError {
    /// The user-global durable command identity is a reserved sentinel.
    InvalidCommandId,
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    Corruption(&'static str),
}

impl fmt::Display for SessionPlacementRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommandId => {
                formatter.write_str("session placement command identity is reserved")
            }
            Self::Database(error) => {
                write!(formatter, "session placement database failure: {error}")
            }
            Self::CommitAmbiguous(error) => {
                write!(formatter, "session placement commit is ambiguous: {error}")
            }
            Self::Corruption(reason) => {
                write!(formatter, "session placement storage is corrupt: {reason}")
            }
        }
    }
}

impl Error for SessionPlacementRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::InvalidCommandId | Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for SessionPlacementRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// PostgreSQL placement history/update adapter.
#[derive(Clone, Debug)]
pub struct SessionPlacementRepository {
    pool: PgPool,
}

impl SessionPlacementRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claims, applies, and records one command, or resolves its replay.
    pub async fn handle(
        &self,
        command: UpdateSessionPlacement,
    ) -> Result<SessionPlacementRepositoryOutcome, SessionPlacementRepositoryError> {
        let command_id = command.command_id();
        if command_id.as_uuid().is_nil() || command_id.as_uuid().is_max() {
            return Err(SessionPlacementRepositoryError::InvalidCommandId);
        }
        let mut transaction = self.pool.begin().await?;
        match inspect_registry(&mut transaction, command_id).await? {
            Some(CommandKind::UpdateSessionPlacement) => {
                let (recorded_command, result) =
                    load_record(&mut transaction, command_id).await?.ok_or(
                        SessionPlacementRepositoryError::Corruption("typed record missing"),
                    )?;
                transaction.rollback().await?;
                return Ok(if recorded_command == command {
                    SessionPlacementRepositoryOutcome::Recorded(result)
                } else {
                    SessionPlacementRepositoryOutcome::ConflictingReuse { command_id }
                });
            }
            Some(
                CommandKind::CreateSession
                | CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::ReplaceSessionMetadata
                | CommandKind::SubmitInput
                | CommandKind::DecideToolRequest
                | CommandKind::OverrideDeniedToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle,
            ) => {
                transaction.rollback().await?;
                return Ok(SessionPlacementRepositoryOutcome::ConflictingReuse { command_id });
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
        .bind(UPDATE_SESSION_PLACEMENT_KIND)
        .bind(STORAGE_VERSION)
        .bind(issuer.0)
        .bind(issuer.1)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            let outcome = match inspect_registry(&mut transaction, command_id).await? {
                Some(CommandKind::UpdateSessionPlacement) => {
                    let (recorded_command, result) =
                        load_record(&mut transaction, command_id).await?.ok_or(
                            SessionPlacementRepositoryError::Corruption("winner record missing"),
                        )?;
                    if recorded_command == command {
                        SessionPlacementRepositoryOutcome::Recorded(result)
                    } else {
                        SessionPlacementRepositoryOutcome::ConflictingReuse { command_id }
                    }
                }
                Some(
                    CommandKind::CreateSession
                    | CommandKind::CreateSessionFromImportedFrontier
                    | CommandKind::ReplaceSessionDefaults
                    | CommandKind::ReplaceSessionMetadata
                    | CommandKind::SubmitInput
                    | CommandKind::DecideToolRequest
                    | CommandKind::OverrideDeniedToolRequest
                    | CommandKind::ReviewWorkflow
                    | CommandKind::ReviewOrchestration
                    | CommandKind::CompactSession
                    | CommandKind::Goal
                    | CommandKind::RegisterWorkspace
                    | CommandKind::MintGitRemote
                    | CommandKind::WithdrawGitRemote
                    | CommandKind::SessionLifecycle,
                ) => SessionPlacementRepositoryOutcome::ConflictingReuse { command_id },
                None => {
                    return Err(SessionPlacementRepositoryError::Corruption(
                        "winner claim missing",
                    ));
                }
            };
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let current = load_current_for_update(&mut transaction, command.session()).await?;
        let result = match current {
            None => UpdateSessionPlacementResult::Rejected(
                UpdateSessionPlacementRejection::session_not_found(&command),
            ),
            Some(current) if current.version() != command.expected_version() => {
                UpdateSessionPlacementResult::Rejected(
                    UpdateSessionPlacementRejection::current_version_mismatch(
                        &command,
                        current.version(),
                    )
                    .ok_or(SessionPlacementRepositoryError::Corruption(
                        "mismatch rejection evidence",
                    ))?,
                )
            }
            Some(current) => match SessionPlacementEvent::updated(
                command.session(),
                current.version(),
                command.replacement().clone(),
                command.command_id(),
            ) {
                Some(event) => {
                    insert_event(&mut transaction, &event).await?;
                    let updated = sqlx::query(
                        "UPDATE session_current_placement
                         SET current_version = $3
                         WHERE session_id = $1 AND current_version = $2",
                    )
                    .bind(session_id_to_uuid(command.session()))
                    .bind(version_to_numeric(current.version()))
                    .bind(version_to_numeric(event.placement().version()))
                    .execute(&mut *transaction)
                    .await?
                    .rows_affected();
                    if updated != 1 {
                        return Err(SessionPlacementRepositoryError::Corruption(
                            "placement head CAS",
                        ));
                    }
                    UpdateSessionPlacementResult::Applied(
                        UpdateSessionPlacementApplied::try_new(&command, event).ok_or(
                            SessionPlacementRepositoryError::Corruption("applied result evidence"),
                        )?,
                    )
                }
                None => UpdateSessionPlacementResult::Rejected(
                    UpdateSessionPlacementRejection::version_exhausted(&command, current.version())
                        .ok_or(SessionPlacementRepositoryError::Corruption(
                            "version exhaustion evidence",
                        ))?,
                ),
            },
        };
        insert_command_record(&mut transaction, &command, &result).await?;
        transaction.commit().await.map_err(|error| {
            if crate::commit_failure_is_ambiguous(&error) {
                SessionPlacementRepositoryError::CommitAmbiguous(error)
            } else {
                SessionPlacementRepositoryError::Database(error)
            }
        })?;
        Ok(SessionPlacementRepositoryOutcome::Recorded(result))
    }

    /// Loads the current immutable placement event for one session.
    pub async fn load_current(
        &self,
        session: SessionId,
    ) -> Result<Option<VersionedSessionPlacement>, SessionPlacementRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        load_current(&mut connection, session).await
    }
}

impl UpdateSessionPlacementTransaction for SessionPlacementRepository {
    type Error = SessionPlacementRepositoryError;

    async fn handle(
        &mut self,
        command: UpdateSessionPlacement,
    ) -> Result<UpdateSessionPlacementOutcome, Self::Error> {
        Ok(
            match SessionPlacementRepository::handle(self, command).await? {
                SessionPlacementRepositoryOutcome::Recorded(result) => {
                    UpdateSessionPlacementOutcome::Recorded(result)
                }
                SessionPlacementRepositoryOutcome::ConflictingReuse { command_id } => {
                    UpdateSessionPlacementOutcome::ConflictingReuse { command_id }
                }
            },
        )
    }
}

pub(crate) async fn load_current(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<VersionedSessionPlacement>, SessionPlacementRepositoryError> {
    let row = sqlx::query(
        "SELECT session.ancestry_kind,
                head.session_id AS head_session_id,
                event.session_id AS event_session_id,
                event.version, event.prior_version, event.event_kind,
                event.placement_path, event.root_global_read_intent,
                native_registry.command_id AS native_creation_command_id,
                imported_registry.command_id AS imported_creation_command_id,
                placement_update_registry.command_id AS placement_update_command_id,
                EXISTS (
                    SELECT 1
                      FROM session_placement_event AS later_event
                     WHERE later_event.session_id = head.session_id
                       AND later_event.version > head.current_version
                ) AS later_event_exists
           FROM session
           LEFT JOIN session_current_placement AS head
             ON head.session_id = session.session_id
           LEFT JOIN session_placement_event AS event
             ON event.session_id = head.session_id
            AND event.version = head.current_version
           LEFT JOIN create_session_command AS native_creation
             ON native_creation.command_id = event.provenance_command_id
            AND native_creation.created_session_id = event.session_id
            AND native_creation.command_kind = 'create_session'
            AND native_creation.storage_version IN (1, 2, 3, 4, 6, 7)
            AND (native_creation.storage_version IN (6, 7)
                 OR (native_creation.storage_version IN (1, 2, 3, 4)
                     AND event.placement_path IS NULL
                     AND NOT event.root_global_read_intent))
            AND native_creation.result_kind = 'applied'
            AND native_creation.placement_path IS NOT DISTINCT FROM event.placement_path
            AND native_creation.root_global_read_intent = event.root_global_read_intent
           LEFT JOIN durable_command AS native_registry
             ON native_registry.command_id = native_creation.command_id
            AND native_registry.command_kind = native_creation.command_kind
            AND native_registry.storage_version = native_creation.storage_version
           LEFT JOIN create_session_from_imported_frontier_command AS imported_creation
             ON imported_creation.command_id = event.provenance_command_id
            AND imported_creation.created_session_id = event.session_id
            AND imported_creation.command_kind = 'create_session_from_imported_frontier'
            AND imported_creation.storage_version IN (1, 2, 3, 5)
            AND imported_creation.result_kind = 'applied'
            AND event.placement_path IS NULL
            AND NOT event.root_global_read_intent
           LEFT JOIN durable_command AS imported_registry
             ON imported_registry.command_id = imported_creation.command_id
            AND imported_registry.command_kind = imported_creation.command_kind
            AND imported_registry.storage_version = imported_creation.storage_version
           LEFT JOIN update_session_placement_command AS placement_update
             ON placement_update.command_id = event.provenance_command_id
            AND placement_update.session_id = event.session_id
            AND placement_update.command_kind = 'update_session_placement'
            AND placement_update.storage_version = 1
            AND placement_update.result_kind = 'applied'
            AND placement_update.rejection_kind IS NULL
            AND placement_update.result_version = event.version
            AND placement_update.result_current_version IS NULL
            AND placement_update.expected_version = event.prior_version
            AND placement_update.replacement_path IS NOT DISTINCT FROM event.placement_path
            AND placement_update.root_global_read_intent = event.root_global_read_intent
           LEFT JOIN durable_command AS placement_update_registry
             ON placement_update_registry.command_id = placement_update.command_id
            AND placement_update_registry.command_kind = placement_update.command_kind
            AND placement_update_registry.storage_version = placement_update.storage_version
          WHERE session.session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let head: Option<sqlx::types::Uuid> = row.try_get("head_session_id")?;
    if head.is_none() {
        return Err(SessionPlacementRepositoryError::Corruption(
            "session placement head missing",
        ));
    }
    let event: Option<sqlx::types::Uuid> = row.try_get("event_session_id")?;
    if event.is_none() {
        return Err(SessionPlacementRepositoryError::Corruption(
            "session placement event missing",
        ));
    }
    let history_head_state =
        PlacementHistoryHeadState::from_later_event_exists(row.try_get("later_event_exists")?);
    let current = decode_authenticated_placement(row)?;
    authenticate_loaded_current(connection, session, current, history_head_state)
        .await
        .map(Some)
}

/// Loads and authenticates the complete current placement histories for one
/// bounded session-summary page in bounded event pages.
pub(crate) async fn load_current_batch(
    connection: &mut PgConnection,
    sessions: &[SessionId],
) -> Result<BTreeMap<sqlx::types::Uuid, VersionedSessionPlacement>, SessionPlacementRepositoryError>
{
    if sessions.is_empty() {
        return Ok(BTreeMap::new());
    }
    let session_ids = sessions
        .iter()
        .map(|session| session_id_to_uuid(*session))
        .collect::<Vec<_>>();
    let mut after_session = None;
    let mut after_version = Decimal::ZERO;
    let mut placements = BTreeMap::new();
    let mut current_session = None;
    let mut current_head = None;
    let mut authenticated = None;
    loop {
        let rows = sqlx::query(
            "SELECT session_row.session_id AS batch_session_id,
                head.current_version AS head_current_version,
                event.session_id AS event_session_id,
                session_row.ancestry_kind,
                event.version, event.prior_version, event.event_kind,
                event.placement_path, event.root_global_read_intent,
                native_registry.command_id AS native_creation_command_id,
                imported_registry.command_id AS imported_creation_command_id,
                placement_update_registry.command_id AS placement_update_command_id
           FROM session AS session_row
           LEFT JOIN session_current_placement AS head
             ON head.session_id = session_row.session_id
           LEFT JOIN session_placement_event AS event
             ON event.session_id = session_row.session_id
           LEFT JOIN create_session_command AS native_creation
             ON native_creation.command_id = event.provenance_command_id
            AND native_creation.created_session_id = event.session_id
            AND native_creation.command_kind = 'create_session'
            AND native_creation.storage_version IN (1, 2, 3, 4, 6, 7)
            AND (native_creation.storage_version IN (6, 7)
                 OR (native_creation.storage_version IN (1, 2, 3, 4)
                     AND event.placement_path IS NULL
                     AND NOT event.root_global_read_intent))
            AND native_creation.result_kind = 'applied'
            AND native_creation.placement_path IS NOT DISTINCT FROM event.placement_path
            AND native_creation.root_global_read_intent = event.root_global_read_intent
           LEFT JOIN durable_command AS native_registry
             ON native_registry.command_id = native_creation.command_id
            AND native_registry.command_kind = native_creation.command_kind
            AND native_registry.storage_version = native_creation.storage_version
           LEFT JOIN create_session_from_imported_frontier_command AS imported_creation
             ON imported_creation.command_id = event.provenance_command_id
            AND imported_creation.created_session_id = event.session_id
            AND imported_creation.command_kind = 'create_session_from_imported_frontier'
            AND imported_creation.storage_version IN (1, 2, 3, 5)
            AND imported_creation.result_kind = 'applied'
            AND event.placement_path IS NULL
            AND NOT event.root_global_read_intent
           LEFT JOIN durable_command AS imported_registry
             ON imported_registry.command_id = imported_creation.command_id
            AND imported_registry.command_kind = imported_creation.command_kind
            AND imported_registry.storage_version = imported_creation.storage_version
           LEFT JOIN update_session_placement_command AS placement_update
             ON placement_update.command_id = event.provenance_command_id
            AND placement_update.session_id = event.session_id
            AND placement_update.command_kind = 'update_session_placement'
            AND placement_update.storage_version = 1
            AND placement_update.result_kind = 'applied'
            AND placement_update.rejection_kind IS NULL
            AND placement_update.result_version = event.version
            AND placement_update.result_current_version IS NULL
            AND placement_update.expected_version = event.prior_version
            AND placement_update.replacement_path IS NOT DISTINCT FROM event.placement_path
            AND placement_update.root_global_read_intent = event.root_global_read_intent
           LEFT JOIN durable_command AS placement_update_registry
             ON placement_update_registry.command_id = placement_update.command_id
            AND placement_update_registry.command_kind = placement_update.command_kind
            AND placement_update_registry.storage_version = placement_update.storage_version
          WHERE session_row.session_id = ANY($1::uuid[])
            AND (
                    $2::uuid IS NULL
                    OR session_row.session_id > $2
                    OR (session_row.session_id = $2 AND event.version > $3)
                )
          ORDER BY session_row.session_id, event.version
          LIMIT $4",
        )
        .bind(&session_ids)
        .bind(after_session)
        .bind(after_version)
        .bind(AUTHENTICATION_PAGE_SIZE)
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let session: sqlx::types::Uuid = row.try_get("batch_session_id")?;
            let head: Option<Decimal> = row.try_get("head_current_version")?;
            if current_session != Some(session) {
                finish_batched_current(
                    &mut placements,
                    current_session,
                    current_head,
                    authenticated.take(),
                )?;
                current_session = Some(session);
                current_head = head;
            } else if current_head != head {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "session placement head changed within snapshot",
                ));
            }
            if row
                .try_get::<Option<sqlx::types::Uuid>, _>("event_session_id")?
                .is_none()
            {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "session placement event missing",
                ));
            }
            let stored_version: Decimal = row.try_get("version")?;
            after_session = Some(session);
            after_version = stored_version;
            let placement = decode_authenticated_placement(row)?;
            let expected_version = authenticated.as_ref().map_or(
                Some(SessionPlacementVersion::INITIAL),
                |predecessor: &VersionedSessionPlacement| predecessor.version().next(),
            );
            if expected_version != Some(placement.version()) {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "session placement predecessor chain",
                ));
            }
            authenticated = Some(placement);
        }
    }
    finish_batched_current(
        &mut placements,
        current_session,
        current_head,
        authenticated,
    )?;
    if placements.len() != session_ids.len() {
        return Err(SessionPlacementRepositoryError::Corruption(
            "session placement batch incomplete",
        ));
    }
    Ok(placements)
}

fn finish_batched_current(
    placements: &mut BTreeMap<sqlx::types::Uuid, VersionedSessionPlacement>,
    session: Option<sqlx::types::Uuid>,
    head: Option<Decimal>,
    authenticated: Option<VersionedSessionPlacement>,
) -> Result<(), SessionPlacementRepositoryError> {
    let Some(session) = session else {
        return Ok(());
    };
    let head = head.ok_or(SessionPlacementRepositoryError::Corruption(
        "session placement head missing",
    ))?;
    let head = decode_version(head)?;
    let authenticated = authenticated.ok_or(SessionPlacementRepositoryError::Corruption(
        "session placement predecessor chain",
    ))?;
    if authenticated.version() < head {
        return Err(SessionPlacementRepositoryError::Corruption(
            "session placement predecessor chain",
        ));
    }
    if authenticated.version() > head {
        return Err(SessionPlacementRepositoryError::Corruption(
            "session placement head behind event history",
        ));
    }
    if placements.insert(session, authenticated).is_some() {
        return Err(SessionPlacementRepositoryError::Corruption(
            "duplicate session placement batch row",
        ));
    }
    Ok(())
}

pub(crate) async fn load_authenticated_version(
    connection: &mut PgConnection,
    session: SessionId,
    version: SessionPlacementVersion,
) -> Result<Option<VersionedSessionPlacement>, SessionPlacementRepositoryError> {
    let mut authenticated: Option<VersionedSessionPlacement> = None;
    loop {
        let after_version = authenticated.as_ref().map_or(Decimal::ZERO, |placement| {
            version_to_numeric(placement.version())
        });
        let rows = sqlx::query(
            "SELECT session_row.ancestry_kind,
                event.version, event.prior_version, event.event_kind,
                event.placement_path, event.root_global_read_intent,
                native_registry.command_id AS native_creation_command_id,
                imported_registry.command_id AS imported_creation_command_id,
                placement_update_registry.command_id AS placement_update_command_id
           FROM session AS session_row
           JOIN session_placement_event AS event
             ON event.session_id = session_row.session_id
            AND event.version <= $2
           LEFT JOIN create_session_command AS native_creation
             ON native_creation.command_id = event.provenance_command_id
            AND native_creation.created_session_id = event.session_id
            AND native_creation.command_kind = 'create_session'
            AND native_creation.storage_version IN (1, 2, 3, 4, 6, 7)
            AND (native_creation.storage_version IN (6, 7)
                 OR (native_creation.storage_version IN (1, 2, 3, 4)
                     AND event.placement_path IS NULL
                     AND NOT event.root_global_read_intent))
            AND native_creation.result_kind = 'applied'
            AND native_creation.placement_path IS NOT DISTINCT FROM event.placement_path
            AND native_creation.root_global_read_intent = event.root_global_read_intent
           LEFT JOIN durable_command AS native_registry
             ON native_registry.command_id = native_creation.command_id
            AND native_registry.command_kind = native_creation.command_kind
            AND native_registry.storage_version = native_creation.storage_version
           LEFT JOIN create_session_from_imported_frontier_command AS imported_creation
             ON imported_creation.command_id = event.provenance_command_id
            AND imported_creation.created_session_id = event.session_id
            AND imported_creation.command_kind = 'create_session_from_imported_frontier'
            AND imported_creation.storage_version IN (1, 2, 3, 5)
            AND imported_creation.result_kind = 'applied'
            AND event.placement_path IS NULL
            AND NOT event.root_global_read_intent
           LEFT JOIN durable_command AS imported_registry
             ON imported_registry.command_id = imported_creation.command_id
            AND imported_registry.command_kind = imported_creation.command_kind
            AND imported_registry.storage_version = imported_creation.storage_version
           LEFT JOIN update_session_placement_command AS placement_update
             ON placement_update.command_id = event.provenance_command_id
            AND placement_update.session_id = event.session_id
            AND placement_update.command_kind = 'update_session_placement'
            AND placement_update.storage_version = 1
            AND placement_update.result_kind = 'applied'
            AND placement_update.rejection_kind IS NULL
            AND placement_update.result_version = event.version
            AND placement_update.result_current_version IS NULL
            AND placement_update.expected_version = event.prior_version
            AND placement_update.replacement_path IS NOT DISTINCT FROM event.placement_path
            AND placement_update.root_global_read_intent = event.root_global_read_intent
           LEFT JOIN durable_command AS placement_update_registry
             ON placement_update_registry.command_id = placement_update.command_id
            AND placement_update_registry.command_kind = placement_update.command_kind
            AND placement_update_registry.storage_version = placement_update.storage_version
          WHERE session_row.session_id = $1
            AND event.version <= $2
            AND event.version > $3
          ORDER BY event.version
          LIMIT $4",
        )
        .bind(session_id_to_uuid(session))
        .bind(version_to_numeric(version))
        .bind(after_version)
        .bind(AUTHENTICATION_PAGE_SIZE)
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let placement = decode_authenticated_placement(row)?;
            let expected_version = authenticated
                .as_ref()
                .map_or(Some(SessionPlacementVersion::INITIAL), |predecessor| {
                    predecessor.version().next()
                });
            if expected_version != Some(placement.version()) {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "session placement predecessor chain",
                ));
            }
            authenticated = Some(placement);
        }
    }
    match authenticated {
        Some(placement) if placement.version() == version => Ok(Some(placement)),
        _ => Ok(None),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlacementHistoryHeadState {
    MatchesLatestEvent,
    BehindLaterEvent,
}

impl PlacementHistoryHeadState {
    pub(crate) fn from_later_event_exists(later_event_exists: bool) -> Self {
        if later_event_exists {
            Self::BehindLaterEvent
        } else {
            Self::MatchesLatestEvent
        }
    }
}

pub(crate) async fn authenticate_loaded_current(
    connection: &mut PgConnection,
    session: SessionId,
    current: VersionedSessionPlacement,
    history_head_state: PlacementHistoryHeadState,
) -> Result<VersionedSessionPlacement, SessionPlacementRepositoryError> {
    let authenticated = load_authenticated_version(connection, session, current.version())
        .await?
        .ok_or(SessionPlacementRepositoryError::Corruption(
            "session placement predecessor chain",
        ))?;
    if authenticated != current {
        return Err(SessionPlacementRepositoryError::Corruption(
            "session placement predecessor chain",
        ));
    }
    match history_head_state {
        PlacementHistoryHeadState::MatchesLatestEvent => Ok(authenticated),
        PlacementHistoryHeadState::BehindLaterEvent => {
            Err(SessionPlacementRepositoryError::Corruption(
                "session placement head behind event history",
            ))
        }
    }
}

async fn load_current_for_update(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<VersionedSessionPlacement>, SessionPlacementRepositoryError> {
    let row = sqlx::query(lock_inventory::UPDATE_SESSION_PLACEMENT_HEAD)
        .bind(session_id_to_uuid(session))
        .fetch_optional(&mut *connection)
        .await?;
    if let Some(row) = row {
        let current = decode_authenticated_placement(row)?;
        let history_head_state =
            load_history_head_state(connection, session, current.version()).await?;
        return authenticate_loaded_current(connection, session, current, history_head_state)
            .await
            .map(Some);
    }
    missing_head_result(connection, session).await
}

async fn load_history_head_state(
    connection: &mut PgConnection,
    session: SessionId,
    version: SessionPlacementVersion,
) -> Result<PlacementHistoryHeadState, sqlx::Error> {
    let later_event_exists = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM session_placement_event
             WHERE session_id = $1 AND version > $2
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(version_to_numeric(version))
    .fetch_one(&mut *connection)
    .await?;
    Ok(PlacementHistoryHeadState::from_later_event_exists(
        later_event_exists,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlacementCreationFamily {
    Native,
    ImportedConversation,
}

fn decode_authenticated_placement(
    row: PgRow,
) -> Result<VersionedSessionPlacement, SessionPlacementRepositoryError> {
    let version = decode_version(row.try_get("version")?)?;
    let prior = row
        .try_get::<Option<Decimal>, _>("prior_version")?
        .map(decode_version)
        .transpose()?;
    let event_kind =
        session_placement_event_kind_from_str(row.try_get::<String, _>("event_kind")?.as_str())
            .ok_or(SessionPlacementRepositoryError::Corruption(
                "current placement event kind",
            ))?;
    let ancestry: String = row.try_get("ancestry_kind")?;
    let creation_family = match ancestry.as_str() {
        "none" => PlacementCreationFamily::Native,
        "imported_conversation" => PlacementCreationFamily::ImportedConversation,
        _ => {
            return Err(SessionPlacementRepositoryError::Corruption(
                "session ancestry kind",
            ));
        }
    };
    let native_creation =
        decode_receipt_command_identity(row.try_get("native_creation_command_id")?)?;
    let imported_creation =
        decode_receipt_command_identity(row.try_get("imported_creation_command_id")?)?;
    let update = decode_receipt_command_identity(row.try_get("placement_update_command_id")?)?;
    let receipt_is_valid = match event_kind {
        SessionPlacementEventKind::Created => {
            version == SessionPlacementVersion::INITIAL
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
            prior.is_some_and(|prior| prior.next() == Some(version))
                && update.is_some()
                && native_creation.is_none()
                && imported_creation.is_none()
        }
    };
    if !receipt_is_valid {
        return Err(SessionPlacementRepositoryError::Corruption(
            "session placement provenance receipt",
        ));
    }
    let placement = decode_placement(
        row.try_get("placement_path")?,
        row.try_get("root_global_read_intent")?,
    )?;
    Ok(VersionedSessionPlacement::reconstitute(version, placement))
}

fn decode_receipt_command_identity(
    stored: Option<sqlx::types::Uuid>,
) -> Result<Option<DurableCommandId>, SessionPlacementRepositoryError> {
    stored
        .map(|command| {
            durable_command_id_from_uuid(command).map_err(|_| {
                SessionPlacementRepositoryError::Corruption(
                    "session placement provenance command identity",
                )
            })
        })
        .transpose()
}

async fn missing_head_result(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<VersionedSessionPlacement>, SessionPlacementRepositoryError> {
    let session_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM session WHERE session_id = $1)")
            .bind(session_id_to_uuid(session))
            .fetch_one(&mut *connection)
            .await?;
    if !session_exists {
        return Ok(None);
    }
    let row = sqlx::query(lock_inventory::UPDATE_SESSION_PLACEMENT_HEAD)
        .bind(session_id_to_uuid(session))
        .fetch_optional(&mut *connection)
        .await?;
    match row {
        Some(row) => {
            let current = decode_authenticated_placement(row)?;
            let history_head_state =
                load_history_head_state(connection, session, current.version()).await?;
            authenticate_loaded_current(connection, session, current, history_head_state)
                .await
                .map(Some)
        }
        None => Err(SessionPlacementRepositoryError::Corruption(
            "session placement head missing",
        )),
    }
}

async fn insert_event(
    connection: &mut PgConnection,
    event: &SessionPlacementEvent,
) -> Result<(), SessionPlacementRepositoryError> {
    let (path, root_intent) = encode_placement(event.placement().placement());
    sqlx::query(
        "INSERT INTO session_placement_event
            (session_id, version, prior_version, event_kind, placement_path,
             root_global_read_intent, provenance_command_id, recorded_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, transaction_timestamp())",
    )
    .bind(session_id_to_uuid(event.session()))
    .bind(version_to_numeric(event.placement().version()))
    .bind(event.prior_version().map(version_to_numeric))
    .bind(session_placement_event_kind_to_str(event.kind()))
    .bind(path)
    .bind(root_intent)
    .bind(durable_command_id_to_uuid(event.command_id()))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_command_record(
    connection: &mut PgConnection,
    command: &UpdateSessionPlacement,
    result: &UpdateSessionPlacementResult,
) -> Result<(), SessionPlacementRepositoryError> {
    let (path, root_intent) = encode_placement(command.replacement());
    let (result_kind, rejection_kind, result_version, current_version) = match result {
        UpdateSessionPlacementResult::Applied(applied) => (
            session_placement_result_kind_to_str(SessionPlacementResultStorageKind::Applied),
            None,
            Some(version_to_numeric(applied.event().placement().version())),
            None,
        ),
        UpdateSessionPlacementResult::Rejected(rejection) => {
            let current = rejection.current_version().map(version_to_numeric);
            (
                session_placement_result_kind_to_str(SessionPlacementResultStorageKind::Rejected),
                Some(session_placement_rejection_to_str(rejection.kind())),
                None,
                current,
            )
        }
    };
    sqlx::query(
        "INSERT INTO update_session_placement_command
            (command_id, command_kind, storage_version, session_id, expected_version,
             replacement_path, root_global_read_intent, result_kind, rejection_kind,
             result_version, result_current_version)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(UPDATE_SESSION_PLACEMENT_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(version_to_numeric(command.expected_version()))
    .bind(path)
    .bind(root_intent)
    .bind(result_kind)
    .bind(rejection_kind)
    .bind(result_version)
    .bind(current_version)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn load_record(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<
    Option<(UpdateSessionPlacement, UpdateSessionPlacementResult)>,
    SessionPlacementRepositoryError,
> {
    let row = sqlx::query(
        "SELECT command.*, head.current_version AS current_placement_version,
                event.prior_version, event.version AS result_event_version,
                event.event_kind AS result_event_kind,
                event.placement_path AS result_path,
                event.root_global_read_intent AS result_root_intent,
                event.provenance_command_id AS result_provenance_command_id,
                EXISTS (
                    SELECT 1
                      FROM session_placement_event AS later_event
                     WHERE later_event.session_id = head.session_id
                       AND later_event.version > head.current_version
                ) AS current_placement_later_event_exists
           FROM update_session_placement_command AS command
           LEFT JOIN session_current_placement AS head
             ON head.session_id = command.session_id
           LEFT JOIN session_placement_event AS event
             ON event.session_id = command.session_id
            AND event.version = command.result_version
          WHERE command.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let session = session_id_from_uuid(row.try_get("session_id")?);
    let current_head_version = row
        .try_get::<Option<Decimal>, _>("current_placement_version")?
        .map(decode_version)
        .transpose()?;
    let authenticated_head_version = match current_head_version {
        Some(version) => load_authenticated_version(connection, session, version)
            .await?
            .map(|placement| placement.version()),
        None => None,
    };
    if authenticated_head_version != current_head_version {
        return Err(SessionPlacementRepositoryError::Corruption(
            "session placement head event",
        ));
    }
    let history_head_state = PlacementHistoryHeadState::from_later_event_exists(
        row.try_get("current_placement_later_event_exists")?,
    );
    match history_head_state {
        PlacementHistoryHeadState::MatchesLatestEvent => {}
        PlacementHistoryHeadState::BehindLaterEvent => {
            return Err(SessionPlacementRepositoryError::Corruption(
                "session placement head behind event history",
            ));
        }
    }
    let authenticated_result_version = match row.try_get::<Option<Decimal>, _>("result_version")? {
        Some(version) => {
            let version = decode_version(version)?;
            load_authenticated_version(connection, session, version)
                .await?
                .map(|placement| placement.version())
        }
        None => None,
    };
    let authenticated_rejection_version =
        match row.try_get::<Option<Decimal>, _>("result_current_version")? {
            Some(version) => {
                let version = decode_version(version)?;
                load_authenticated_version(connection, session, version)
                    .await?
                    .map(|placement| placement.version())
            }
            None => None,
        };
    decode_record(
        row,
        command_id,
        authenticated_result_version,
        authenticated_rejection_version,
    )
    .map(Some)
}

fn decode_record(
    row: PgRow,
    command_id: DurableCommandId,
    authenticated_result_version: Option<SessionPlacementVersion>,
    authenticated_rejection_version: Option<SessionPlacementVersion>,
) -> Result<(UpdateSessionPlacement, UpdateSessionPlacementResult), SessionPlacementRepositoryError>
{
    validate_typed_header(
        row.try_get::<String, _>("command_kind")?.as_str(),
        row.try_get("storage_version")?,
    )?;
    let session = session_id_from_uuid(row.try_get("session_id")?);
    let expected = decode_version(row.try_get("expected_version")?)?;
    let replacement = decode_placement(
        row.try_get("replacement_path")?,
        row.try_get("root_global_read_intent")?,
    )?;
    let command = UpdateSessionPlacement::new(command_id, session, expected, replacement);
    let result_kind =
        session_placement_result_kind_from_str(row.try_get::<String, _>("result_kind")?.as_str())
            .ok_or(SessionPlacementRepositoryError::Corruption("result kind"))?;
    let rejection = match row.try_get::<Option<String>, _>("rejection_kind")? {
        Some(value) => Some(session_placement_rejection_from_str(&value).ok_or(
            SessionPlacementRepositoryError::Corruption("rejection kind"),
        )?),
        None => None,
    };
    let result_version: Option<Decimal> = row.try_get("result_version")?;
    let result_current_version: Option<Decimal> = row.try_get("result_current_version")?;
    validate_terminal_field_shape(
        result_kind,
        rejection,
        TerminalFieldPresence {
            result_version: result_version.is_some(),
            current_version: result_current_version.is_some(),
        }
        .shape(),
    )?;
    let result = match (result_kind, rejection) {
        (SessionPlacementResultStorageKind::Applied, None) => {
            let event_kind = session_placement_event_kind_from_str(
                required::<String>(&row, "result_event_kind")?.as_str(),
            )
            .ok_or(SessionPlacementRepositoryError::Corruption(
                "result event kind",
            ))?;
            if event_kind != SessionPlacementEventKind::Updated {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied event kind",
                ));
            }
            let provenance: sqlx::types::Uuid = required(&row, "result_provenance_command_id")?;
            if provenance != durable_command_id_to_uuid(command_id) {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied event provenance",
                ));
            }
            let prior = decode_version(required(&row, "prior_version")?)?;
            if prior != expected {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied prior version",
                ));
            }
            let placement = decode_placement(
                row.try_get("result_path")?,
                required(&row, "result_root_intent")?,
            )?;
            if &placement != command.replacement() {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied replacement",
                ));
            }
            let recorded_result_version = decode_version(result_version.ok_or(
                SessionPlacementRepositoryError::Corruption("result version"),
            )?)?;
            let event_result_version = decode_version(required(&row, "result_event_version")?)?;
            let current_placement_version =
                decode_version(required(&row, "current_placement_version")?)?;
            if current_placement_version.as_u64() < event_result_version.as_u64() {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied event not reached by placement head",
                ));
            }
            if recorded_result_version != event_result_version {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied result version",
                ));
            }
            if authenticated_result_version != Some(recorded_result_version) {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied placement event",
                ));
            }
            let event = SessionPlacementEvent::updated(session, prior, placement, command_id)
                .ok_or(SessionPlacementRepositoryError::Corruption(
                    "applied version exhausted",
                ))?;
            if event.placement().version() != event_result_version {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied event version",
                ));
            }
            UpdateSessionPlacementResult::Applied(
                UpdateSessionPlacementApplied::try_new(&command, event).ok_or(
                    SessionPlacementRepositoryError::Corruption("applied result evidence"),
                )?,
            )
        }
        (
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
        ) => UpdateSessionPlacementResult::Rejected(
            UpdateSessionPlacementRejection::session_not_found(&command),
        ),
        (
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::CurrentVersionMismatch),
        ) => {
            let current = decode_version(result_current_version.ok_or(
                SessionPlacementRepositoryError::Corruption("result current version"),
            )?)?;
            if current == expected {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "mismatch rejection version",
                ));
            }
            if authenticated_rejection_version != Some(current) {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "rejection current placement event",
                ));
            }
            require_rejection_version_reached(&row, current)?;
            UpdateSessionPlacementResult::Rejected(
                UpdateSessionPlacementRejection::current_version_mismatch(&command, current)
                    .ok_or(SessionPlacementRepositoryError::Corruption(
                        "mismatch rejection evidence",
                    ))?,
            )
        }
        (
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::VersionExhausted),
        ) => {
            let current = decode_version(result_current_version.ok_or(
                SessionPlacementRepositoryError::Corruption("result current version"),
            )?)?;
            if expected.as_u64() != u64::MAX || current.as_u64() != u64::MAX {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "version exhaustion ordinal",
                ));
            }
            if authenticated_rejection_version != Some(current) {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "rejection current placement event",
                ));
            }
            require_rejection_version_reached(&row, current)?;
            UpdateSessionPlacementResult::Rejected(
                UpdateSessionPlacementRejection::version_exhausted(&command, current).ok_or(
                    SessionPlacementRepositoryError::Corruption("version exhaustion evidence"),
                )?,
            )
        }
        (
            SessionPlacementResultStorageKind::Applied,
            Some(
                SessionPlacementRejectionStorageKind::SessionNotFound
                | SessionPlacementRejectionStorageKind::CurrentVersionMismatch
                | SessionPlacementRejectionStorageKind::VersionExhausted,
            ),
        )
        | (SessionPlacementResultStorageKind::Rejected, None) => {
            return Err(SessionPlacementRepositoryError::Corruption(
                "terminal result shape",
            ));
        }
    };
    Ok((command, result))
}

fn require_rejection_version_reached(
    row: &PgRow,
    reported_version: SessionPlacementVersion,
) -> Result<(), SessionPlacementRepositoryError> {
    let current_placement_version = decode_version(required(row, "current_placement_version")?)?;
    if current_placement_version.as_u64() < reported_version.as_u64() {
        return Err(SessionPlacementRepositoryError::Corruption(
            "rejection event not reached by placement head",
        ));
    }
    Ok(())
}

fn validate_typed_header(
    command_kind: &str,
    storage_version: i16,
) -> Result<(), SessionPlacementRepositoryError> {
    if command_kind != UPDATE_SESSION_PLACEMENT_KIND {
        return Err(SessionPlacementRepositoryError::Corruption(
            "typed command kind",
        ));
    }
    if storage_version != STORAGE_VERSION {
        return Err(SessionPlacementRepositoryError::Corruption(
            "typed command storage version",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalFieldShape {
    Neither,
    ResultVersion,
    CurrentVersion,
    Both,
}

struct TerminalFieldPresence {
    result_version: bool,
    current_version: bool,
}

impl TerminalFieldPresence {
    const fn shape(self) -> TerminalFieldShape {
        match (self.result_version, self.current_version) {
            (false, false) => TerminalFieldShape::Neither,
            (true, false) => TerminalFieldShape::ResultVersion,
            (false, true) => TerminalFieldShape::CurrentVersion,
            (true, true) => TerminalFieldShape::Both,
        }
    }
}

fn validate_terminal_field_shape(
    result_kind: SessionPlacementResultStorageKind,
    rejection: Option<SessionPlacementRejectionStorageKind>,
    fields: TerminalFieldShape,
) -> Result<(), SessionPlacementRepositoryError> {
    let valid = match (result_kind, rejection, fields) {
        (SessionPlacementResultStorageKind::Applied, None, TerminalFieldShape::ResultVersion) => {
            true
        }
        (
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
            TerminalFieldShape::Neither,
        ) => true,
        (
            SessionPlacementResultStorageKind::Rejected,
            Some(
                SessionPlacementRejectionStorageKind::CurrentVersionMismatch
                | SessionPlacementRejectionStorageKind::VersionExhausted,
            ),
            TerminalFieldShape::CurrentVersion,
        ) => true,
        (
            SessionPlacementResultStorageKind::Applied,
            None,
            TerminalFieldShape::Neither
            | TerminalFieldShape::CurrentVersion
            | TerminalFieldShape::Both,
        )
        | (
            SessionPlacementResultStorageKind::Applied,
            Some(
                SessionPlacementRejectionStorageKind::SessionNotFound
                | SessionPlacementRejectionStorageKind::CurrentVersionMismatch
                | SessionPlacementRejectionStorageKind::VersionExhausted,
            ),
            TerminalFieldShape::Neither
            | TerminalFieldShape::ResultVersion
            | TerminalFieldShape::CurrentVersion
            | TerminalFieldShape::Both,
        )
        | (
            SessionPlacementResultStorageKind::Rejected,
            None,
            TerminalFieldShape::Neither
            | TerminalFieldShape::ResultVersion
            | TerminalFieldShape::CurrentVersion
            | TerminalFieldShape::Both,
        )
        | (
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
            TerminalFieldShape::ResultVersion
            | TerminalFieldShape::CurrentVersion
            | TerminalFieldShape::Both,
        )
        | (
            SessionPlacementResultStorageKind::Rejected,
            Some(
                SessionPlacementRejectionStorageKind::CurrentVersionMismatch
                | SessionPlacementRejectionStorageKind::VersionExhausted,
            ),
            TerminalFieldShape::Neither
            | TerminalFieldShape::ResultVersion
            | TerminalFieldShape::Both,
        ) => false,
    };
    if valid {
        Ok(())
    } else {
        Err(SessionPlacementRepositoryError::Corruption(
            "terminal result fields",
        ))
    }
}

pub(crate) fn encode_placement(placement: &SessionPlacement) -> (Option<&str>, bool) {
    (
        placement.path().map(SessionPlacementPath::as_str),
        placement.records_root_global_read_intent(),
    )
}

pub(crate) fn decode_placement(
    path: Option<String>,
    root_intent: bool,
) -> Result<SessionPlacement, SessionPlacementRepositoryError> {
    let Some(path) = path else {
        return if root_intent {
            Err(SessionPlacementRepositoryError::Corruption(
                "pathless root intent",
            ))
        } else {
            Ok(SessionPlacement::pathless())
        };
    };
    let path = SessionPlacementPath::try_new(path)
        .map_err(|_| SessionPlacementRepositoryError::Corruption("invalid placement path"))?;
    if root_intent {
        SessionPlacement::root_global_read(path, RootPlacementGlobalReadIntent::Acknowledged)
            .map_err(|_| SessionPlacementRepositoryError::Corruption("invalid root placement"))
    } else {
        SessionPlacement::scoped(path)
            .map_err(|_| SessionPlacementRepositoryError::Corruption("implicit root placement"))
    }
}

fn version_to_numeric(version: SessionPlacementVersion) -> Decimal {
    Decimal::from(version.as_u64())
}

pub(crate) fn decode_version(
    value: Decimal,
) -> Result<SessionPlacementVersion, SessionPlacementRepositoryError> {
    let value = u64::try_from(value)
        .map_err(|_| SessionPlacementRepositoryError::Corruption("invalid placement version"))?;
    SessionPlacementVersion::try_from_u64(value).ok_or(SessionPlacementRepositoryError::Corruption(
        "zero placement version",
    ))
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SessionPlacementRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or(SessionPlacementRepositoryError::Corruption(field))
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, SessionPlacementRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => {
                SessionPlacementRepositoryError::Database(error)
            }
            RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(_)) => {
                SessionPlacementRepositoryError::Corruption("registry kind")
            }
            RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(_)) => {
                SessionPlacementRepositoryError::Corruption("registry version")
            }
            RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
                SessionPlacementRepositoryError::Corruption("registry typed record")
            }
            RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
                SessionPlacementRepositoryError::Corruption("registry record conflict")
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn assert_terminal_field_corruption(result: Result<(), SessionPlacementRepositoryError>) {
        let Err(SessionPlacementRepositoryError::Corruption(reason)) = result else {
            panic!("fixture shape must fail with typed corruption")
        };
        assert_eq!(reason, "terminal result fields");
    }

    #[track_caller]
    fn assert_header_corruption(
        result: Result<(), SessionPlacementRepositoryError>,
        expected_reason: &'static str,
    ) {
        let Err(SessionPlacementRepositoryError::Corruption(reason)) = result else {
            panic!("fixture header must fail with typed corruption")
        };
        assert_eq!(reason, expected_reason);
    }

    #[track_caller]
    fn assert_provenance_command_identity_corruption(stored: sqlx::types::Uuid) {
        let Err(SessionPlacementRepositoryError::Corruption(reason)) =
            decode_receipt_command_identity(Some(stored))
        else {
            panic!("sentinel provenance must fail with typed corruption")
        };
        assert_eq!(reason, "session placement provenance command identity");
    }

    #[test]
    fn replay_terminal_shapes_reject_every_stray_result_field() {
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Applied,
            None,
            TerminalFieldShape::Both,
        ));
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
            TerminalFieldShape::ResultVersion,
        ));
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
            TerminalFieldShape::CurrentVersion,
        ));
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
            TerminalFieldShape::Both,
        ));
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::CurrentVersionMismatch),
            TerminalFieldShape::Both,
        ));
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::VersionExhausted),
            TerminalFieldShape::Both,
        ));
    }

    #[test]
    fn replay_rejects_each_inconsistent_typed_command_header() {
        assert_header_corruption(
            validate_typed_header("create_session", STORAGE_VERSION),
            "typed command kind",
        );
        assert_header_corruption(
            validate_typed_header(UPDATE_SESSION_PLACEMENT_KIND, STORAGE_VERSION + 1),
            "typed command storage version",
        );
    }

    #[test]
    fn placement_history_rejects_each_sentinel_command_provenance() {
        assert_provenance_command_identity_corruption(sqlx::types::Uuid::nil());
        assert_provenance_command_identity_corruption(sqlx::types::Uuid::max());
    }
}

//! Append-only session-placement history and explicit update replay.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    DurableCommandId, RootPlacementGlobalReadIntent, SessionId, SessionPlacement,
    SessionPlacementEvent, SessionPlacementEventKind, SessionPlacementPath,
    SessionPlacementVersion, UpdateSessionPlacement, UpdateSessionPlacementRejection,
    UpdateSessionPlacementResult, VersionedSessionPlacement,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};

use crate::command_registry::{
    self, CommandKind, RegistryCorruption, RegistryInspectionError, UPDATE_SESSION_PLACEMENT_KIND,
};
use crate::lock_inventory;
use crate::mapping::{
    SessionPlacementRejectionStorageKind, SessionPlacementResultStorageKind,
    durable_command_id_to_uuid, session_id_from_uuid, session_id_to_uuid,
    session_placement_event_kind_from_str, session_placement_event_kind_to_str,
    session_placement_rejection_from_str, session_placement_rejection_to_str,
    session_placement_result_kind_from_str, session_placement_result_kind_to_str,
};

const STORAGE_VERSION: i16 = 1;

/// First handling/equal replay or conflicting durable-command reuse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionPlacementRepositoryOutcome {
    Recorded(UpdateSessionPlacementResult),
    ConflictingReuse { command_id: DurableCommandId },
}

/// Database or fail-closed placement-history failure.
#[derive(Debug)]
pub enum SessionPlacementRepositoryError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    Corruption(&'static str),
}

impl fmt::Display for SessionPlacementRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            Self::Corruption(_) => None,
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
            Some(_) => {
                transaction.rollback().await?;
                return Ok(SessionPlacementRepositoryOutcome::ConflictingReuse { command_id });
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
        .bind(UPDATE_SESSION_PLACEMENT_KIND)
        .bind(STORAGE_VERSION)
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
                Some(_) => SessionPlacementRepositoryOutcome::ConflictingReuse { command_id },
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
                UpdateSessionPlacementRejection::SessionNotFound {
                    session: command.session(),
                },
            ),
            Some(current) if current.version() != command.expected_version() => {
                UpdateSessionPlacementResult::Rejected(
                    UpdateSessionPlacementRejection::CurrentVersionMismatch {
                        session: command.session(),
                        expected: command.expected_version(),
                        current: current.version(),
                    },
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
                    UpdateSessionPlacementResult::Applied(event)
                }
                None => UpdateSessionPlacementResult::Rejected(
                    UpdateSessionPlacementRejection::VersionExhausted {
                        session: command.session(),
                        current: current.version(),
                    },
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

pub(crate) async fn load_current(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<VersionedSessionPlacement>, SessionPlacementRepositoryError> {
    let row = sqlx::query(
        "SELECT head.session_id AS head_session_id,
                event.session_id AS event_session_id,
                event.version, event.placement_path, event.root_global_read_intent
           FROM session
           LEFT JOIN session_current_placement AS head
             ON head.session_id = session.session_id
           LEFT JOIN session_placement_event AS event
             ON event.session_id = head.session_id
            AND event.version = head.current_version
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
    decode_versioned_placement(row).map(Some)
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
        return decode_authenticated_locked_placement(row).map(Some);
    }
    missing_head_result(connection, session).await
}

fn decode_authenticated_locked_placement(
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
    let native_creation: Option<sqlx::types::Uuid> = row.try_get("native_creation_command_id")?;
    let imported_creation: Option<sqlx::types::Uuid> =
        row.try_get("imported_creation_command_id")?;
    let update: Option<sqlx::types::Uuid> = row.try_get("placement_update_command_id")?;
    let receipt_is_valid = match event_kind {
        SessionPlacementEventKind::Created => {
            version == SessionPlacementVersion::INITIAL
                && prior.is_none()
                && update.is_none()
                && (native_creation.is_some() != imported_creation.is_some())
        }
        SessionPlacementEventKind::Updated => {
            prior.is_some()
                && update.is_some()
                && native_creation.is_none()
                && imported_creation.is_none()
        }
    };
    if !receipt_is_valid {
        return Err(SessionPlacementRepositoryError::Corruption(
            "current placement provenance receipt",
        ));
    }
    let placement = decode_placement(
        row.try_get("placement_path")?,
        row.try_get("root_global_read_intent")?,
    )?;
    Ok(VersionedSessionPlacement::reconstitute(version, placement))
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
    if session_exists {
        Err(SessionPlacementRepositoryError::Corruption(
            "session placement head missing",
        ))
    } else {
        Ok(None)
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
        UpdateSessionPlacementResult::Applied(event) => (
            session_placement_result_kind_to_str(SessionPlacementResultStorageKind::Applied),
            None,
            Some(version_to_numeric(event.placement().version())),
            None,
        ),
        UpdateSessionPlacementResult::Rejected(rejection) => {
            let current = match rejection {
                UpdateSessionPlacementRejection::SessionNotFound { .. } => None,
                UpdateSessionPlacementRejection::CurrentVersionMismatch { current, .. }
                | UpdateSessionPlacementRejection::VersionExhausted { current, .. } => {
                    Some(version_to_numeric(*current))
                }
            };
            (
                session_placement_result_kind_to_str(SessionPlacementResultStorageKind::Rejected),
                Some(session_placement_rejection_to_str(rejection)),
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
        "SELECT command.*, event.prior_version, event.version AS result_event_version,
                event.event_kind AS result_event_kind,
                event.placement_path AS result_path,
                event.root_global_read_intent AS result_root_intent,
                event.provenance_command_id AS result_provenance_command_id
           FROM update_session_placement_command AS command
           LEFT JOIN session_placement_event AS event
             ON event.session_id = command.session_id
            AND event.version = command.result_version
          WHERE command.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| decode_record(row, command_id)).transpose()
}

fn decode_record(
    row: PgRow,
    command_id: DurableCommandId,
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
        result_version.is_some(),
        result_current_version.is_some(),
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
            if recorded_result_version != event_result_version {
                return Err(SessionPlacementRepositoryError::Corruption(
                    "applied result version",
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
            UpdateSessionPlacementResult::Applied(event)
        }
        (
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
        ) => UpdateSessionPlacementResult::Rejected(
            UpdateSessionPlacementRejection::SessionNotFound { session },
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
            UpdateSessionPlacementResult::Rejected(
                UpdateSessionPlacementRejection::CurrentVersionMismatch {
                    session,
                    expected,
                    current,
                },
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
            UpdateSessionPlacementResult::Rejected(
                UpdateSessionPlacementRejection::VersionExhausted { session, current },
            )
        }
        _ => {
            return Err(SessionPlacementRepositoryError::Corruption(
                "terminal result shape",
            ));
        }
    };
    Ok((command, result))
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

fn validate_terminal_field_shape(
    result_kind: SessionPlacementResultStorageKind,
    rejection: Option<SessionPlacementRejectionStorageKind>,
    has_result_version: bool,
    has_current_version: bool,
) -> Result<(), SessionPlacementRepositoryError> {
    let valid = match (result_kind, rejection) {
        (SessionPlacementResultStorageKind::Applied, None) => {
            has_result_version && !has_current_version
        }
        (
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
        ) => !has_result_version && !has_current_version,
        (
            SessionPlacementResultStorageKind::Rejected,
            Some(
                SessionPlacementRejectionStorageKind::CurrentVersionMismatch
                | SessionPlacementRejectionStorageKind::VersionExhausted,
            ),
        ) => !has_result_version && has_current_version,
        _ => false,
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

fn decode_versioned_placement(
    row: PgRow,
) -> Result<VersionedSessionPlacement, SessionPlacementRepositoryError> {
    Ok(VersionedSessionPlacement::reconstitute(
        decode_version(row.try_get("version")?)?,
        decode_placement(
            row.try_get("placement_path")?,
            row.try_get("root_global_read_intent")?,
        )?,
    ))
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

    fn assert_terminal_field_corruption(result: Result<(), SessionPlacementRepositoryError>) {
        let Err(SessionPlacementRepositoryError::Corruption(reason)) = result else {
            panic!("fixture shape must fail with typed corruption")
        };
        assert_eq!(reason, "terminal result fields");
    }

    fn assert_header_corruption(
        result: Result<(), SessionPlacementRepositoryError>,
        expected_reason: &'static str,
    ) {
        let Err(SessionPlacementRepositoryError::Corruption(reason)) = result else {
            panic!("fixture header must fail with typed corruption")
        };
        assert_eq!(reason, expected_reason);
    }

    #[test]
    fn replay_terminal_shapes_reject_every_stray_result_field() {
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Applied,
            None,
            true,
            true,
        ));
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::SessionNotFound),
            true,
            false,
        ));
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::CurrentVersionMismatch),
            true,
            true,
        ));
        assert_terminal_field_corruption(validate_terminal_field_shape(
            SessionPlacementResultStorageKind::Rejected,
            Some(SessionPlacementRejectionStorageKind::VersionExhausted),
            true,
            true,
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
}

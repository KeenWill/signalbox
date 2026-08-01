//! Append-only session-placement history and explicit update replay.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    DurableCommandId, RootPlacementGlobalReadIntent, SessionId, SessionPlacement,
    SessionPlacementEvent, SessionPlacementPath, SessionPlacementVersion, UpdateSessionPlacement,
    UpdateSessionPlacementRejection, UpdateSessionPlacementResult, VersionedSessionPlacement,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};

use crate::command_registry::{
    self, CommandKind, RegistryCorruption, RegistryInspectionError, UPDATE_SESSION_PLACEMENT_KIND,
};
use crate::lock_inventory;
use crate::mapping::{durable_command_id_to_uuid, session_id_from_uuid, session_id_to_uuid};

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
        "SELECT event.version, event.placement_path, event.root_global_read_intent
           FROM session_current_placement AS head
           JOIN session_placement_event AS event
             ON event.session_id = head.session_id
            AND event.version = head.current_version
          WHERE head.session_id = $1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(decode_versioned_placement).transpose()
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
        return decode_versioned_placement(row).map(Some);
    }
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
         VALUES ($1, $2, $3, 'updated', $4, $5, $6, transaction_timestamp())",
    )
    .bind(session_id_to_uuid(event.session()))
    .bind(version_to_numeric(event.placement().version()))
    .bind(event.prior_version().map(version_to_numeric))
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
            "applied",
            None,
            Some(version_to_numeric(event.placement().version())),
            None,
        ),
        UpdateSessionPlacementResult::Rejected(
            UpdateSessionPlacementRejection::SessionNotFound { .. },
        ) => ("rejected", Some("session_not_found"), None, None),
        UpdateSessionPlacementResult::Rejected(
            UpdateSessionPlacementRejection::CurrentVersionMismatch { current, .. },
        ) => (
            "rejected",
            Some("current_version_mismatch"),
            None,
            Some(version_to_numeric(*current)),
        ),
        UpdateSessionPlacementResult::Rejected(
            UpdateSessionPlacementRejection::VersionExhausted { current, .. },
        ) => (
            "rejected",
            Some("version_exhausted"),
            None,
            Some(version_to_numeric(*current)),
        ),
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
        "SELECT command.*, event.prior_version, event.placement_path AS result_path,
                event.root_global_read_intent AS result_root_intent
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
    let session = session_id_from_uuid(row.try_get("session_id")?);
    let expected = decode_version(row.try_get("expected_version")?)?;
    let replacement = decode_placement(
        row.try_get("replacement_path")?,
        row.try_get("root_global_read_intent")?,
    )?;
    let command = UpdateSessionPlacement::new(command_id, session, expected, replacement);
    let result_kind: String = row.try_get("result_kind")?;
    let rejection: Option<String> = row.try_get("rejection_kind")?;
    let result = match (result_kind.as_str(), rejection.as_deref()) {
        ("applied", None) => {
            let prior = decode_version(required(&row, "prior_version")?)?;
            let placement = decode_placement(
                row.try_get("result_path")?,
                required(&row, "result_root_intent")?,
            )?;
            let event = SessionPlacementEvent::updated(session, prior, placement, command_id)
                .ok_or(SessionPlacementRepositoryError::Corruption(
                    "applied version exhausted",
                ))?;
            UpdateSessionPlacementResult::Applied(event)
        }
        ("rejected", Some("session_not_found")) => UpdateSessionPlacementResult::Rejected(
            UpdateSessionPlacementRejection::SessionNotFound { session },
        ),
        ("rejected", Some("current_version_mismatch")) => UpdateSessionPlacementResult::Rejected(
            UpdateSessionPlacementRejection::CurrentVersionMismatch {
                session,
                expected,
                current: decode_version(required(&row, "result_current_version")?)?,
            },
        ),
        ("rejected", Some("version_exhausted")) => UpdateSessionPlacementResult::Rejected(
            UpdateSessionPlacementRejection::VersionExhausted {
                session,
                current: decode_version(required(&row, "result_current_version")?)?,
            },
        ),
        _ => {
            return Err(SessionPlacementRepositoryError::Corruption(
                "terminal result shape",
            ));
        }
    };
    Ok((command, result))
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
            Ok(SessionPlacement::Pathless)
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

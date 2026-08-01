//! Current session-placement reconstitution.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    RootPlacementGlobalReadIntent, SessionId, SessionPlacement, SessionPlacementPath,
    SessionPlacementVersion, VersionedSessionPlacement,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};

use crate::mapping::session_id_to_uuid;

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

/// PostgreSQL current-placement adapter.
#[derive(Clone, Debug)]
pub struct SessionPlacementRepository {
    pool: PgPool,
}

impl SessionPlacementRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
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

pub(crate) fn decode_version(
    value: Decimal,
) -> Result<SessionPlacementVersion, SessionPlacementRepositoryError> {
    let value = u64::try_from(value)
        .map_err(|_| SessionPlacementRepositoryError::Corruption("invalid placement version"))?;
    SessionPlacementVersion::try_from_u64(value).ok_or(SessionPlacementRepositoryError::Corruption(
        "zero placement version",
    ))
}

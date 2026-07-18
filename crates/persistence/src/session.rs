//! PostgreSQL loading for the current long-lived [`Session`] aggregate.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::SessionReader;
use signalbox_domain::{
    DirectModelSelection, ModelAlias, ModelSelectionRequest, Session, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SessionReconstitutionFailure, SessionReconstitutionInput, TranscriptAncestry,
};
use sqlx::{PgPool, Row, postgres::PgRow, types::Uuid};

use crate::mapping::{
    PositiveOrdinalMappingError, defaults_version_from_numeric, session_id_from_uuid,
    session_id_to_uuid,
};

const OWNER_INITIATED: &str = "owner_initiated";
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
    /// by that pointer. It intentionally loads no creation receipt, transcript,
    /// turn, command history, or unselected defaults version.
    pub async fn load_session(
        &self,
        requested_session: SessionId,
    ) -> Result<Option<Session>, SessionRepositoryError> {
        let row = sqlx::query(
            "SELECT
                s.session_id AS stored_session_id,
                s.creation_cause,
                s.ancestry_kind,
                p.session_id AS current_defaults_session_id,
                p.current_version,
                v.session_id AS selected_defaults_session_id,
                v.version AS selected_defaults_version,
                v.model_selection_kind,
                v.direct_model_selection_id,
                v.model_alias_id
             FROM session AS s
             LEFT JOIN session_current_defaults AS p
               ON p.session_id = s.session_id
             LEFT JOIN session_defaults_version AS v
               ON v.session_id = p.session_id
              AND v.version = p.current_version
             WHERE s.session_id = $1",
        )
        .bind(session_id_to_uuid(requested_session))
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| decode_complete(row, requested_session))
            .transpose()
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

fn decode_complete(
    row: PgRow,
    requested_session: SessionId,
) -> Result<Session, SessionRepositoryError> {
    let stored_session = session_id_from_uuid(required(&row, "stored_session_id")?);
    let provenance = decode_provenance(
        required(&row, "creation_cause")?,
        required(&row, "ancestry_kind")?,
    )?;
    let current_defaults_session =
        session_id_from_uuid(required(&row, "current_defaults_session_id")?);
    let current_defaults_version = decode_ordinal(&row, "current_version")?;
    let defaults_session = session_id_from_uuid(required(&row, "selected_defaults_session_id")?);
    let defaults_version = decode_ordinal(&row, "selected_defaults_version")?;
    let defaults = decode_selection(
        required(&row, "model_selection_kind")?,
        row.try_get("direct_model_selection_id")?,
        row.try_get("model_alias_id")?,
    )?;

    SessionReconstitutionInput::new(
        requested_session,
        stored_session,
        provenance,
        current_defaults_session,
        current_defaults_version,
        defaults_session,
        defaults_version,
        defaults,
    )
    .reconstitute()
    .map_err(|error| SessionCorruption::Domain(error.failure()).into())
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
) -> Result<SessionCreationProvenance, SessionRepositoryError> {
    // The current migration admits only the baseline storage spellings. This
    // adapter-level representation check does not narrow the domain seam:
    // `SessionReconstitutionInput` continues to accept any future provenance
    // variant once its owning migration supplies a checked mapping.
    if cause != OWNER_INITIATED {
        return Err(SessionCorruption::Unsupported {
            field: "creation cause",
            value: cause,
        }
        .into());
    }
    if ancestry != NO_ANCESTRY {
        return Err(SessionCorruption::Unsupported {
            field: "ancestry kind",
            value: ancestry,
        }
        .into());
    }
    Ok(SessionCreationProvenance::new(
        SessionCreationCause::OwnerInitiated,
        TranscriptAncestry::None,
    ))
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
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
    Ok(SessionConfigurationDefaults::new(model))
}

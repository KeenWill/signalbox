//! Shared session-placement storage decoding.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    RootPlacementGlobalReadIntent, SessionPlacement, SessionPlacementPath, SessionPlacementVersion,
};
/// Database or fail-closed placement-history failure.
#[derive(Debug)]
pub enum SessionPlacementRepositoryError {
    Database(sqlx::Error),
    Corruption(&'static str),
}

impl fmt::Display for SessionPlacementRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "session placement database failure: {error}")
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
            Self::Database(error) => Some(error),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for SessionPlacementRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
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

pub(crate) fn decode_version(
    value: Decimal,
) -> Result<SessionPlacementVersion, SessionPlacementRepositoryError> {
    let value = u64::try_from(value)
        .map_err(|_| SessionPlacementRepositoryError::Corruption("invalid placement version"))?;
    SessionPlacementVersion::try_from_u64(value).ok_or(SessionPlacementRepositoryError::Corruption(
        "zero placement version",
    ))
}

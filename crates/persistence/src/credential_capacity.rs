//! Latest provider capacity evidence per configured credential reference.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use signalbox_domain::{ModelCallId, ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use sqlx::{PgConnection, Row, types::time::OffsetDateTime};

use crate::model_execution::{ModelCallCorruption, ModelCallRepositoryError};

#[derive(Serialize, Deserialize)]
struct WindowRecord {
    remaining_percent: i64,
    window_duration: Option<Duration>,
    resets_at: Option<i128>,
}

fn unix_nanos(instant: SystemTime) -> i128 {
    match instant.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    }
}

fn reset_instant(nanos: i128) -> Result<SystemTime, ModelCallRepositoryError> {
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map(SystemTime::from)
        .map_err(|_| ModelCallCorruption::Inconsistent("credential capacity reset instant").into())
}

pub(crate) async fn retain_call_rate_limits(
    connection: &mut PgConnection,
    call: ModelCallId,
    snapshot: &ProviderRateLimitSnapshot,
) -> Result<(), ModelCallRepositoryError> {
    let windows = snapshot
        .windows()
        .iter()
        .map(|window| WindowRecord {
            remaining_percent: *window.remaining_percent(),
            window_duration: *window.window_duration(),
            resets_at: window.resets_at().map(unix_nanos),
        })
        .collect::<Vec<_>>();
    let windows = serde_json::to_string(&windows)
        .map_err(|_| ModelCallCorruption::Inconsistent("credential capacity window encoding"))?;
    sqlx::query(
        "INSERT INTO credential_rate_limit_snapshot
            (credential_reference, observation_model_call_id, observed_at, windows)
         SELECT credential_reference, model_call_id, $2, $3::jsonb
           FROM model_call WHERE model_call_id = $1
         ON CONFLICT (credential_reference) DO UPDATE
             SET observation_model_call_id = EXCLUDED.observation_model_call_id,
                 observed_at = EXCLUDED.observed_at,
                 windows = EXCLUDED.windows
           WHERE EXCLUDED.observed_at > credential_rate_limit_snapshot.observed_at",
    )
    .bind(call.into_uuid())
    .bind(OffsetDateTime::from(*snapshot.observed_at()))
    .bind(windows)
    .execute(connection)
    .await?;
    Ok(())
}

/// Loads the most recently observed windows for a non-secret profile reference.
pub async fn load_credential_rate_limits(
    connection: &mut PgConnection,
    credential_reference: &str,
) -> Result<Option<ProviderRateLimitSnapshot>, ModelCallRepositoryError> {
    let Some(row) = sqlx::query(
        "SELECT observed_at, windows::text FROM credential_rate_limit_snapshot
          WHERE credential_reference = $1",
    )
    .bind(credential_reference)
    .fetch_optional(connection)
    .await?
    else {
        return Ok(None);
    };
    let records: Vec<WindowRecord> = serde_json::from_str(row.try_get("windows")?)
        .map_err(|_| ModelCallCorruption::Inconsistent("credential capacity window decoding"))?;
    let windows = records
        .into_iter()
        .map(|window| {
            Ok(ProviderRateLimitWindow::new(
                window.remaining_percent,
                window.window_duration,
                window.resets_at.map(reset_instant).transpose()?,
            ))
        })
        .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
    Ok(Some(ProviderRateLimitSnapshot::new(
        SystemTime::from(row.try_get::<OffsetDateTime, _>("observed_at")?),
        windows,
    )))
}

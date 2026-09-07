//! Latest provider capacity evidence per configured credential reference.

use std::time::{Duration, SystemTime};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use signalbox_domain::{ModelCallId, ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use sqlx::{PgConnection, Row};

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

fn decode_instant(nanos: i128) -> Result<SystemTime, ModelCallRepositoryError> {
    let invalid = || ModelCallCorruption::Inconsistent("credential capacity instant");
    let magnitude = nanos.unsigned_abs();
    let nanos_per_second = Duration::from_secs(1).as_nanos();
    let seconds = u64::try_from(magnitude / nanos_per_second).map_err(|_| invalid())?;
    let subsecond_nanos = (magnitude % nanos_per_second) as u32;
    let duration = Duration::new(seconds, subsecond_nanos);
    let instant = if nanos >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(duration)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(duration)
    };
    instant.ok_or_else(|| invalid().into())
}

pub(crate) async fn retain_call_rate_limits(
    connection: &mut PgConnection,
    call: ModelCallId,
    snapshot: &ProviderRateLimitSnapshot,
) -> Result<bool, ModelCallRepositoryError> {
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
    let result = sqlx::query(
        "INSERT INTO credential_rate_limit_snapshot
            (credential_reference, observation_model_call_id, observed_at_nanos, windows)
         SELECT credential_reference, model_call_id, $2, $3::jsonb
           FROM model_call WHERE model_call_id = $1
         ON CONFLICT (credential_reference) DO UPDATE
             SET observation_model_call_id = EXCLUDED.observation_model_call_id,
                 observed_at_nanos = EXCLUDED.observed_at_nanos,
                 windows = EXCLUDED.windows
           WHERE EXCLUDED.observed_at_nanos > credential_rate_limit_snapshot.observed_at_nanos",
    )
    .bind(call.into_uuid())
    .bind(Decimal::from_i128_with_scale(
        unix_nanos(*snapshot.observed_at()),
        0,
    ))
    .bind(windows)
    .execute(connection)
    .await?;
    Ok(result.rows_affected() != 0)
}

/// Loads the most recently observed windows for a non-secret profile reference.
pub async fn load_credential_rate_limits(
    connection: &mut PgConnection,
    credential_reference: &str,
) -> Result<Option<ProviderRateLimitSnapshot>, ModelCallRepositoryError> {
    let Some(row) = sqlx::query(
        "SELECT observed_at_nanos, windows::text FROM credential_rate_limit_snapshot
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
                window.resets_at.map(decode_instant).transpose()?,
            ))
        })
        .collect::<Result<Vec<_>, ModelCallRepositoryError>>()?;
    Ok(Some(ProviderRateLimitSnapshot::new(
        decode_instant(row.try_get::<Decimal, _>("observed_at_nanos")?.mantissa())?,
        windows,
    )))
}

#[cfg(test)]
mod tests {
    use super::{decode_instant, unix_nanos};
    use std::time::{Duration, SystemTime};

    #[test]
    fn capacity_instants_retain_signed_nanoseconds() {
        let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_nanos(111);
        let after_epoch = SystemTime::UNIX_EPOCH + Duration::from_nanos(222);
        assert_eq!(unix_nanos(before_epoch), -111);
        assert_eq!(decode_instant(-111).ok(), Some(before_epoch));
        assert_eq!(unix_nanos(after_epoch), 222);
        assert_eq!(decode_instant(222).ok(), Some(after_epoch));
    }
}

//! PostgreSQL adapter for bounded aggregate and individual-call usage reads.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    UsageAggregateGroup, UsageAggregateKey, UsageAggregateReport, UsageAggregateTokenAxes,
    UsageCallCursor, UsageCallEvidence, UsageCallKind, UsageCallOrder, UsageCallPage,
    UsageCallQuery, UsageInputTokenSemantics, UsageProvenance, UsageQuery, UsageReader,
    UsageTimestampMicros, UsageTokenAxes, UsageTokenCoverage, UsageTokenPresence,
    max_usage_aggregate_calls, max_usage_aggregate_groups, max_usage_credential_profile_utf8_bytes,
};
use signalbox_domain::{
    ModelCallId, ProviderModelIdentity, ResolvedProviderTarget, SessionId, TurnId,
};
use sqlx::{PgPool, Row, postgres::PgRow, types::time::OffsetDateTime};
use uuid::Uuid;

use crate::mapping::{
    usage_call_kind_from_str, usage_call_kind_to_str, usage_provenance_from_str,
    usage_provenance_to_str,
};

// Canonical references are unbounded, so neither query may copy a
// reconstructed reference into per-call rows. Both resolve the oversized
// mapping through the bounded profile label and transfer a reference only when
// it fits the configured-profile ceiling: a longer reference can never name a
// configured profile, so it derives no cost and is reported as over-ceiling
// instead of being materialized. The aggregate resolves each emitted group's
// reference exactly once, after grouping and the group ceiling.
const AGGREGATE_SQL: &str = "
WITH candidate_calls AS MATERIALIZED (
    SELECT *
      FROM web_usage_call_projection
     WHERE ($1::timestamptz IS NULL OR recorded_at >= $1)
       AND ($2::timestamptz IS NULL OR recorded_at < $2)
       AND ($3::uuid IS NULL OR session_id = $3)
       AND ($4::uuid IS NULL OR turn_id = $4)
       AND ($5::uuid IS NULL OR resolved_provider_model_identity_id = $5)
       AND ($6::text IS NULL OR usage_provenance_kind = $6)
       AND ($7::text IS NULL OR call_kind = $7)
     ORDER BY recorded_at DESC, model_call_id DESC
     LIMIT $8
), bounded_calls AS (
    SELECT *
      FROM candidate_calls
     ORDER BY recorded_at DESC, model_call_id DESC
     LIMIT $9
), bounded_state AS (
    SELECT count(*) > $9 AS calls_truncated FROM candidate_calls
), grouped AS (
    SELECT call_kind, resolved_provider_model_identity_id,
           credential_profile_label,
           usage_provenance_kind, usage_input_includes_cache_tokens,
           input_tokens IS NOT NULL AS has_input,
           output_tokens IS NOT NULL AS has_output,
           cache_creation_input_tokens IS NOT NULL AS has_cache_creation,
           cache_read_input_tokens IS NOT NULL AS has_cache_read,
           count(*) AS call_count,
           sum(input_tokens) AS input_tokens,
           sum(output_tokens) AS output_tokens,
           sum(cache_creation_input_tokens) AS cache_creation_input_tokens,
           sum(cache_read_input_tokens) AS cache_read_input_tokens,
           usage_input_includes_cache_tokens IS NOT NULL
           AND bool_and(
               usage_input_includes_cache_tokens IS DISTINCT FROM true
               OR input_tokens IS NULL
               OR cache_creation_input_tokens IS NULL
               OR cache_read_input_tokens IS NULL
               OR input_tokens >= cache_creation_input_tokens + cache_read_input_tokens
           ) AS cache_normalization_safe,
           bounded_state.calls_truncated
      FROM bounded_calls
     CROSS JOIN bounded_state
     GROUP BY call_kind, resolved_provider_model_identity_id,
              credential_profile_label, usage_provenance_kind,
              usage_input_includes_cache_tokens,
              input_tokens IS NOT NULL, output_tokens IS NOT NULL,
              cache_creation_input_tokens IS NOT NULL,
              cache_read_input_tokens IS NOT NULL, bounded_state.calls_truncated
     ORDER BY call_kind, resolved_provider_model_identity_id,
              credential_profile_label, usage_provenance_kind,
              usage_input_includes_cache_tokens NULLS FIRST,
              has_input, has_output, has_cache_creation, has_cache_read
     LIMIT $10
)
SELECT call_kind, resolved_provider_model_identity_id,
       credential_profile_label AS web_profile,
       usage_provenance_kind, usage_input_includes_cache_tokens,
       has_input, has_output, has_cache_creation, has_cache_read,
       call_count, input_tokens, output_tokens,
       cache_creation_input_tokens, cache_read_input_tokens,
       cache_normalization_safe, calls_truncated,
       CASE
           WHEN credential_profile_label LIKE 'exact:%'
               THEN substring(credential_profile_label FROM 7)
           WHEN octet_length(oversized_profile.exact_reference) <= 256
               THEN oversized_profile.exact_reference
           ELSE NULL
       END AS credential_reference,
       COALESCE(octet_length(oversized_profile.exact_reference) > 256, false)
           AS credential_reference_over_ceiling
  FROM grouped
  LEFT JOIN web_usage_oversized_profile_identity AS oversized_profile
    ON credential_profile_label NOT LIKE 'exact:%'
   AND oversized_profile.profile_id::text = substring(credential_profile_label FROM 8)
 ORDER BY call_kind, resolved_provider_model_identity_id,
          credential_profile_label, usage_provenance_kind,
          usage_input_includes_cache_tokens NULLS FIRST,
          has_input, has_output, has_cache_creation, has_cache_read";

const CALLS_NEWEST_SQL: &str = "
SELECT model_call_id, call_kind, session_id, turn_id,
       resolved_provider_model_identity_id,
       CASE
           WHEN credential_profile_label LIKE 'exact:%'
               THEN substring(credential_profile_label FROM 7)
           WHEN octet_length(oversized_profile.exact_reference) <= 256
               THEN oversized_profile.exact_reference
           ELSE NULL
       END AS credential_reference,
       COALESCE(octet_length(oversized_profile.exact_reference) > 256, false)
           AS credential_reference_over_ceiling,
       credential_profile_label AS web_profile,
       usage_provenance_kind, usage_input_includes_cache_tokens,
       input_tokens, output_tokens,
       cache_creation_input_tokens, cache_read_input_tokens, recorded_at
  FROM web_usage_call_projection AS usage_call
  LEFT JOIN web_usage_oversized_profile_identity AS oversized_profile
    ON credential_profile_label NOT LIKE 'exact:%'
   AND oversized_profile.profile_id::text = substring(credential_profile_label FROM 8)
 WHERE ($1::timestamptz IS NULL OR recorded_at >= $1)
   AND ($2::timestamptz IS NULL OR recorded_at < $2)
   AND ($3::uuid IS NULL OR session_id = $3)
   AND ($4::uuid IS NULL OR turn_id = $4)
   AND ($5::uuid IS NULL OR resolved_provider_model_identity_id = $5)
   AND ($6::text IS NULL OR usage_provenance_kind = $6)
   AND ($7::text IS NULL OR call_kind = $7)
   AND (
       $8::timestamptz IS NULL
       OR recorded_at < $8
       OR (recorded_at = $8 AND model_call_id < $9)
   )
 ORDER BY recorded_at DESC, model_call_id DESC
 LIMIT $10";

/// Integrity failure in the dedicated usage projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageProjectionCorruption {
    /// A required projected field was absent or malformed.
    Invalid(&'static str),
    /// A closed stored discriminator was unsupported.
    Unsupported {
        /// Projection field carrying the unsupported spelling.
        field: &'static str,
        /// Exact unsupported spelling.
        value: String,
    },
}

impl fmt::Display for UsageProjectionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => write!(formatter, "invalid usage projection {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported usage projection {field}: {value}")
            }
        }
    }
}

impl Error for UsageProjectionCorruption {}

/// Database or fail-closed usage-projection failure.
#[derive(Debug)]
pub enum UsageRepositoryError {
    /// PostgreSQL query failure.
    Database(sqlx::Error),
    /// Projection row violated the application representation.
    Corruption(UsageProjectionCorruption),
}

impl fmt::Display for UsageRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "usage database failure: {error}"),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for UsageRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for UsageRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<UsageProjectionCorruption> for UsageRepositoryError {
    fn from(error: UsageProjectionCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of dedicated usage aggregate and detail reads.
#[derive(Clone, Debug)]
pub struct UsageRepository {
    pool: PgPool,
}

impl UsageRepository {
    /// Uses the supplied pool for indexed bounded reads.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Reads at most the hard aggregate-group ceiling plus one truncation row.
    pub async fn aggregate(
        &self,
        query: UsageQuery,
    ) -> Result<UsageAggregateReport, UsageRepositoryError> {
        let filters = QueryBindings::new(query)?;
        let rows = sqlx::query(AGGREGATE_SQL)
            .bind(filters.from)
            .bind(filters.to)
            .bind(filters.session)
            .bind(filters.turn)
            .bind(filters.model)
            .bind(filters.provenance)
            .bind(filters.call_kind)
            .bind(i64::from(max_usage_aggregate_calls()) + 1)
            .bind(i64::from(max_usage_aggregate_calls()))
            .bind(i64::from(max_usage_aggregate_groups()) + 1)
            .fetch_all(&self.pool)
            .await?;
        let limit = usize::from(max_usage_aggregate_groups());
        let source_calls_truncated = rows
            .first()
            .map(|row| row.try_get::<bool, _>("calls_truncated"))
            .transpose()?
            .unwrap_or(false);
        let truncated = rows.len() > limit || source_calls_truncated;
        let groups = rows
            .into_iter()
            .take(limit)
            .map(decode_aggregate)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(UsageAggregateReport { groups, truncated })
    }

    /// Reads one strict terminal-time/UUID keyset page.
    pub async fn calls(
        &self,
        query: UsageCallQuery,
    ) -> Result<UsageCallPage, UsageRepositoryError> {
        let filters = QueryBindings::new(query.scope)?;
        let cursor_time = query
            .after
            .map(|cursor| timestamp_to_offset(cursor.recorded_at))
            .transpose()?;
        let cursor_call = query.after.map(|cursor| cursor.call.into_uuid());
        let rows = match query.order {
            UsageCallOrder::NewestFirst => sqlx::query(CALLS_NEWEST_SQL),
        }
        .bind(filters.from)
        .bind(filters.to)
        .bind(filters.session)
        .bind(filters.turn)
        .bind(filters.model)
        .bind(filters.provenance)
        .bind(filters.call_kind)
        .bind(cursor_time)
        .bind(cursor_call)
        .bind(i64::from(query.limit.get()) + 1)
        .fetch_all(&self.pool)
        .await?;
        decode_call_page(rows, usize::from(query.limit.get()))
    }
}

impl UsageReader for UsageRepository {
    type Error = UsageRepositoryError;

    async fn aggregate(&self, query: UsageQuery) -> Result<UsageAggregateReport, Self::Error> {
        self.aggregate(query).await
    }

    async fn calls(&self, query: UsageCallQuery) -> Result<UsageCallPage, Self::Error> {
        self.calls(query).await
    }
}

struct QueryBindings {
    from: Option<OffsetDateTime>,
    to: Option<OffsetDateTime>,
    session: Option<Uuid>,
    turn: Option<Uuid>,
    model: Option<Uuid>,
    provenance: Option<&'static str>,
    call_kind: Option<&'static str>,
}

impl QueryBindings {
    fn new(query: UsageQuery) -> Result<Self, UsageProjectionCorruption> {
        Ok(Self {
            from: query
                .time
                .from_inclusive()
                .map(timestamp_to_offset)
                .transpose()?,
            to: query
                .time
                .to_exclusive()
                .map(timestamp_to_offset)
                .transpose()?,
            session: query.selection.session.map(SessionId::into_uuid),
            turn: query.selection.turn.map(TurnId::into_uuid),
            model: query
                .selection
                .model
                .map(|model| model.identity().into_uuid()),
            provenance: query.selection.provenance.map(usage_provenance_to_str),
            call_kind: query.selection.call_kind.map(usage_call_kind_to_str),
        })
    }
}

fn timestamp_to_offset(
    timestamp: UsageTimestampMicros,
) -> Result<OffsetDateTime, UsageProjectionCorruption> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp.get()) * 1_000)
        .map_err(|_| UsageProjectionCorruption::Invalid("timestamp"))
}

/// Reports whether one application timestamp can cross this adapter boundary.
#[must_use]
pub fn usage_timestamp_is_representable(timestamp: UsageTimestampMicros) -> bool {
    timestamp_to_offset(timestamp).is_ok()
}

fn decode_call_page(rows: Vec<PgRow>, limit: usize) -> Result<UsageCallPage, UsageRepositoryError> {
    let has_more = rows.len() > limit;
    let mut calls = rows
        .into_iter()
        .take(limit)
        .map(decode_call)
        .collect::<Result<Vec<_>, _>>()?;
    let next = if has_more {
        calls.last().map(|call| UsageCallCursor {
            recorded_at: call.recorded_at,
            call: call.call,
        })
    } else {
        None
    };
    Ok(UsageCallPage {
        calls: std::mem::take(&mut calls),
        next,
    })
}

fn decode_call(row: PgRow) -> Result<UsageCallEvidence, UsageRepositoryError> {
    let web_profile: String = row.try_get("web_profile")?;
    if web_profile.is_empty() || web_profile.len() > 256 {
        return Err(UsageProjectionCorruption::Invalid("web profile").into());
    }
    let credential_profile = decode_credential_reference(&row)?;
    Ok(UsageCallEvidence {
        call_kind: decode_call_kind(row.try_get("call_kind")?)?,
        call: ModelCallId::from_uuid(row.try_get("model_call_id")?),
        session: SessionId::from_uuid(row.try_get("session_id")?),
        turn: row
            .try_get::<Option<Uuid>, _>("turn_id")?
            .map(TurnId::from_uuid),
        model: decode_model(&row)?,
        web_profile,
        credential_profile,
        provenance: decode_provenance(row.try_get("usage_provenance_kind")?)?,
        input_semantics: decode_input_semantics(row.try_get("usage_input_includes_cache_tokens")?),
        tokens: decode_tokens(&row)?,
        recorded_at: decode_timestamp(row.try_get("recorded_at")?)?,
    })
}

fn decode_aggregate(row: PgRow) -> Result<UsageAggregateGroup, UsageRepositoryError> {
    let web_profile: String = row.try_get("web_profile")?;
    if web_profile.is_empty() || web_profile.len() > 256 {
        return Err(UsageProjectionCorruption::Invalid("web profile").into());
    }
    let credential_profile = decode_credential_reference(&row)?;
    let call_count = u64::try_from(row.try_get::<i64, _>("call_count")?)
        .map_err(|_| UsageProjectionCorruption::Invalid("call count"))?;
    if call_count == 0 {
        return Err(UsageProjectionCorruption::Invalid("call count").into());
    }
    Ok(UsageAggregateGroup {
        key: UsageAggregateKey {
            call_kind: decode_call_kind(row.try_get("call_kind")?)?,
            model: decode_model(&row)?,
            web_profile,
            credential_profile,
            provenance: decode_provenance(row.try_get("usage_provenance_kind")?)?,
            input_semantics: decode_input_semantics(
                row.try_get("usage_input_includes_cache_tokens")?,
            ),
            coverage: UsageTokenCoverage {
                input: decode_presence(row.try_get("has_input")?),
                output: decode_presence(row.try_get("has_output")?),
                cache_creation_input: decode_presence(row.try_get("has_cache_creation")?),
                cache_read_input: decode_presence(row.try_get("has_cache_read")?),
            },
        },
        call_count,
        tokens: decode_aggregate_tokens(&row)?,
        cache_normalization_safe: row.try_get("cache_normalization_safe")?,
    })
}

/// Decodes the ceiling-bounded credential reference used for cost derivation.
///
/// The queries reconstruct a reference only when it fits
/// [`max_usage_credential_profile_utf8_bytes`]; a longer canonical reference
/// can never name a configured profile, so it is reported as over-ceiling
/// (`None`) instead of being copied out of the mapping. A null reference
/// without the over-ceiling marker means the projected label points at no
/// mapping row, which is corruption and fails closed.
fn decode_credential_reference(row: &PgRow) -> Result<Option<String>, UsageRepositoryError> {
    match row.try_get::<Option<String>, _>("credential_reference")? {
        Some(reference)
            if !reference.is_empty()
                && reference.len() <= usize::from(max_usage_credential_profile_utf8_bytes()) =>
        {
            Ok(Some(reference))
        }
        Some(_) => Err(UsageProjectionCorruption::Invalid("credential profile").into()),
        None => {
            if row.try_get::<bool, _>("credential_reference_over_ceiling")? {
                Ok(None)
            } else {
                Err(UsageProjectionCorruption::Invalid("credential profile").into())
            }
        }
    }
}

fn decode_model(row: &PgRow) -> Result<ResolvedProviderTarget, sqlx::Error> {
    Ok(ResolvedProviderTarget::naming(
        ProviderModelIdentity::from_uuid(row.try_get("resolved_provider_model_identity_id")?),
    ))
}

fn decode_tokens(row: &PgRow) -> Result<UsageTokenAxes, UsageRepositoryError> {
    Ok(UsageTokenAxes {
        input: decode_optional_u64(row.try_get("input_tokens")?, "input tokens")?,
        output: decode_optional_u64(row.try_get("output_tokens")?, "output tokens")?,
        cache_creation_input: decode_optional_u64(
            row.try_get("cache_creation_input_tokens")?,
            "cache creation input tokens",
        )?,
        cache_read_input: decode_optional_u64(
            row.try_get("cache_read_input_tokens")?,
            "cache read input tokens",
        )?,
    })
}

fn decode_aggregate_tokens(row: &PgRow) -> Result<UsageAggregateTokenAxes, UsageRepositoryError> {
    Ok(UsageAggregateTokenAxes {
        input: decode_optional_u128(row.try_get("input_tokens")?, "input tokens")?,
        output: decode_optional_u128(row.try_get("output_tokens")?, "output tokens")?,
        cache_creation_input: decode_optional_u128(
            row.try_get("cache_creation_input_tokens")?,
            "cache creation input tokens",
        )?,
        cache_read_input: decode_optional_u128(
            row.try_get("cache_read_input_tokens")?,
            "cache read input tokens",
        )?,
    })
}

fn decode_optional_u128(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<u128>, UsageProjectionCorruption> {
    value
        .map(|value| {
            if value.fract().is_zero() && !value.is_sign_negative() {
                u128::try_from(value).map_err(|_| UsageProjectionCorruption::Invalid(field))
            } else {
                Err(UsageProjectionCorruption::Invalid(field))
            }
        })
        .transpose()
}

const fn decode_presence(value: bool) -> UsageTokenPresence {
    if value {
        UsageTokenPresence::Present
    } else {
        UsageTokenPresence::Absent
    }
}

fn decode_optional_u64(
    value: Option<Decimal>,
    field: &'static str,
) -> Result<Option<u64>, UsageProjectionCorruption> {
    value
        .map(|value| {
            if value.fract().is_zero() && !value.is_sign_negative() {
                u64::try_from(value).map_err(|_| UsageProjectionCorruption::Invalid(field))
            } else {
                Err(UsageProjectionCorruption::Invalid(field))
            }
        })
        .transpose()
}

fn decode_timestamp(
    value: OffsetDateTime,
) -> Result<UsageTimestampMicros, UsageProjectionCorruption> {
    let nanos = value.unix_timestamp_nanos();
    if nanos < 0 || nanos % 1_000 != 0 {
        return Err(UsageProjectionCorruption::Invalid("timestamp"));
    }
    let micros = u64::try_from(nanos / 1_000)
        .map_err(|_| UsageProjectionCorruption::Invalid("timestamp"))?;
    UsageTimestampMicros::new(micros).map_err(|_| UsageProjectionCorruption::Invalid("timestamp"))
}

fn decode_call_kind(value: String) -> Result<UsageCallKind, UsageProjectionCorruption> {
    usage_call_kind_from_str(&value).ok_or(UsageProjectionCorruption::Unsupported {
        field: "call kind",
        value,
    })
}

fn decode_provenance(value: String) -> Result<UsageProvenance, UsageProjectionCorruption> {
    usage_provenance_from_str(&value).ok_or(UsageProjectionCorruption::Unsupported {
        field: "usage provenance",
        value,
    })
}

const fn decode_input_semantics(value: Option<bool>) -> UsageInputTokenSemantics {
    match value {
        None => UsageInputTokenSemantics::Unknown,
        Some(false) => UsageInputTokenSemantics::CacheExclusive,
        Some(true) => UsageInputTokenSemantics::CacheInclusive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_queries_retain_raw_and_bounded_profile_projections() {
        assert!(AGGREGATE_SQL.contains("oversized_profile.exact_reference"));
        assert!(AGGREGATE_SQL.contains("substring(credential_profile_label FROM 7)"));
        assert!(AGGREGATE_SQL.contains("credential_profile_label AS web_profile"));
        assert!(CALLS_NEWEST_SQL.contains("oversized_profile.exact_reference"));
        assert!(CALLS_NEWEST_SQL.contains("substring(credential_profile_label FROM 7)"));
        assert!(CALLS_NEWEST_SQL.contains("credential_profile_label AS web_profile"));
    }

    #[test]
    fn usage_queries_bound_reconstructed_references_to_the_profile_ceiling() {
        let ceiling_guard = format!(
            "octet_length(oversized_profile.exact_reference) <= {}",
            max_usage_credential_profile_utf8_bytes()
        );
        let over_ceiling_marker = format!(
            "COALESCE(octet_length(oversized_profile.exact_reference) > {}, false)",
            max_usage_credential_profile_utf8_bytes()
        );

        assert!(AGGREGATE_SQL.contains(&ceiling_guard));
        assert!(AGGREGATE_SQL.contains(&over_ceiling_marker));
        assert!(CALLS_NEWEST_SQL.contains(&ceiling_guard));
        assert!(CALLS_NEWEST_SQL.contains(&over_ceiling_marker));
    }

    #[test]
    fn usage_aggregate_resolves_references_only_after_grouping() {
        let (grouping, reference_join) = AGGREGATE_SQL
            .split_once("GROUP BY")
            .expect("the aggregate query groups bounded calls");

        assert!(!grouping.contains("oversized_profile"));
        assert!(reference_join.contains("LEFT JOIN web_usage_oversized_profile_identity"));
    }

    #[test]
    fn timestamp_conversion_preserves_exact_microseconds() {
        let timestamp =
            UsageTimestampMicros::new(1_777_777_777_123_456).expect("fixture timestamp fits");
        let decoded =
            decode_timestamp(timestamp_to_offset(timestamp).expect("fixture timestamp converts"))
                .expect("fixture timestamp decodes");

        assert_eq!(decoded, timestamp);
    }

    #[test]
    fn timestamp_representability_admits_the_application_maximum() {
        let timestamp = UsageTimestampMicros::new(253_402_300_799_999_999)
            .expect("the application timestamp maximum is admitted");

        assert!(usage_timestamp_is_representable(timestamp));
        assert_eq!(
            decode_timestamp(timestamp_to_offset(timestamp).expect("maximum timestamp converts"))
                .expect("maximum timestamp decodes"),
            timestamp
        );
    }
}

//! PostgreSQL adapter for bounded aggregate and individual-call usage reads.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    UsageAggregateCompleteness, UsageAggregateGroup, UsageAggregateKey, UsageAggregateReport,
    UsageAggregateTokenAxes, UsageCacheNormalization, UsageCallCursor, UsageCallEvidence,
    UsageCallKind, UsageCallOrder, UsageCallPage, UsageCallQuery, UsageCallScope,
    UsageInputTokenSemantics, UsageProvenance, UsageQuery, UsageReader, UsageTimestampMicros,
    UsageTokenAxes, UsageTokenCoverage, UsageTokenPresence, max_usage_aggregate_calls,
    max_usage_aggregate_groups, max_usage_credential_profile_utf8_bytes,
};
use signalbox_domain::{
    ModelCallId, ProviderModelIdentity, ResolvedProviderTarget, SessionId, TurnId,
};
use sqlx::{
    PgPool, Row,
    postgres::{PgArguments, PgRow},
    types::time::OffsetDateTime,
};
use uuid::Uuid;

use crate::mapping::{
    usage_call_kind_from_str, usage_call_kind_to_str, usage_provenance_from_str,
    usage_provenance_to_str,
};

// Each statement is assembled with only the selected dimensions' predicates,
// so every selection shape gets its own cached prepared statement and even a
// generic plan sees exactly the conjunction its ordered index serves; no
// `$n IS NULL OR` disjunction survives to defeat index selection. A turn
// belongs to exactly one session, so when a selection supplies both, the
// projection predicate stays turn-led and the session filter collapses to one
// bounded `turn_lifecycle` unique probe (an uncorrelated one-time filter): a
// matched pair reads exactly the turn scope and a mismatched pair is proven
// empty without scanning either dimension's history.
#[derive(Clone, Copy)]
enum StatementBind {
    Time(OffsetDateTime),
    Id(Uuid),
    Label(&'static str),
    Count(i64),
}

struct StatementBuilder {
    predicates: Vec<String>,
    binds: Vec<StatementBind>,
}

impl StatementBuilder {
    const fn new() -> Self {
        Self {
            predicates: Vec::new(),
            binds: Vec::new(),
        }
    }

    fn bind(&mut self, value: StatementBind) -> usize {
        self.binds.push(value);
        self.binds.len()
    }

    fn predicate(&mut self, clause: String) {
        self.predicates.push(clause);
    }

    fn where_clause(&self) -> String {
        if self.predicates.is_empty() {
            "true".to_owned()
        } else {
            self.predicates.join("\n       AND ")
        }
    }

    // The assembled text is built solely from static SQL fragments and
    // sequential parameter numbers; every caller-influenced value is a bound
    // parameter, so no caller-provided SQL text reaches the statement.
    fn query(&self, sql: String) -> sqlx::query::Query<'static, sqlx::Postgres, PgArguments> {
        let mut statement = sqlx::query(sqlx::AssertSqlSafe(sql));
        for bind in &self.binds {
            statement = match *bind {
                StatementBind::Time(value) => statement.bind(value),
                StatementBind::Id(value) => statement.bind(value),
                StatementBind::Label(value) => statement.bind(value),
                StatementBind::Count(value) => statement.bind(value),
            };
        }
        statement
    }
}

fn selection_statement(filters: &QueryBindings) -> StatementBuilder {
    let mut builder = StatementBuilder::new();
    if let Some(from) = filters.from {
        let parameter = builder.bind(StatementBind::Time(from));
        builder.predicate(format!("recorded_at >= ${parameter}"));
    }
    if let Some(to) = filters.to {
        let parameter = builder.bind(StatementBind::Time(to));
        builder.predicate(format!("recorded_at < ${parameter}"));
    }
    match (filters.turn, filters.session) {
        (Some(turn), Some(session)) => {
            let turn_parameter = builder.bind(StatementBind::Id(turn));
            builder.predicate(format!("turn_id = ${turn_parameter}"));
            let session_parameter = builder.bind(StatementBind::Id(session));
            builder.predicate(format!(
                "EXISTS (SELECT 1 FROM turn_lifecycle \
                 WHERE turn_id = ${turn_parameter} AND session_id = ${session_parameter})"
            ));
        }
        (Some(turn), None) => {
            let parameter = builder.bind(StatementBind::Id(turn));
            builder.predicate(format!("turn_id = ${parameter}"));
        }
        (None, Some(session)) => {
            let parameter = builder.bind(StatementBind::Id(session));
            builder.predicate(format!("session_id = ${parameter}"));
        }
        (None, None) => {}
    }
    if let Some(model) = filters.model {
        let parameter = builder.bind(StatementBind::Id(model));
        builder.predicate(format!(
            "resolved_provider_model_identity_id = ${parameter}"
        ));
    }
    if let Some(provenance) = filters.provenance {
        let parameter = builder.bind(StatementBind::Label(provenance));
        builder.predicate(format!("usage_provenance_kind = ${parameter}"));
    }
    if let Some(call_kind) = filters.call_kind {
        let parameter = builder.bind(StatementBind::Label(call_kind));
        builder.predicate(format!("call_kind = ${parameter}"));
    }
    builder
}

fn aggregate_sql(
    where_clause: &str,
    source_probe: usize,
    source_limit: usize,
    group_probe: usize,
) -> String {
    format!(
        "
WITH candidate_calls AS MATERIALIZED (
    SELECT *
      FROM web_usage_call_projection
     WHERE {where_clause}
     ORDER BY recorded_at DESC, model_call_id DESC
     LIMIT ${source_probe}
), bounded_calls AS (
    SELECT *
      FROM candidate_calls
     ORDER BY recorded_at DESC, model_call_id DESC
     LIMIT ${source_limit}
), bounded_state AS (
    SELECT count(*) > ${source_limit} AS calls_truncated FROM candidate_calls
)
SELECT call_kind, resolved_provider_model_identity_id,
       credential_profile_label AS credential_reference,
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
           OR (
               input_tokens IS NOT NULL
               AND cache_creation_input_tokens IS NOT NULL
               AND cache_read_input_tokens IS NOT NULL
               AND input_tokens >= cache_creation_input_tokens + cache_read_input_tokens
           )
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
 ORDER BY call_kind, resolved_provider_model_identity_id, credential_profile_label,
          usage_provenance_kind, usage_input_includes_cache_tokens NULLS FIRST,
          input_tokens IS NOT NULL, output_tokens IS NOT NULL,
          cache_creation_input_tokens IS NOT NULL,
          cache_read_input_tokens IS NOT NULL
 LIMIT ${group_probe}"
    )
}

// Selection specialization matches `aggregate_sql` above; the strict keyset
// cursor predicate is appended as a selection predicate when present.
fn calls_newest_sql(where_clause: &str, page_probe: usize) -> String {
    format!(
        "
SELECT model_call_id, call_kind, session_id, turn_id,
       resolved_provider_model_identity_id,
       credential_profile_label AS credential_reference,
       usage_provenance_kind, usage_input_includes_cache_tokens,
       input_tokens, output_tokens,
       cache_creation_input_tokens, cache_read_input_tokens, recorded_at
  FROM web_usage_call_projection
 WHERE {where_clause}
 ORDER BY recorded_at DESC, model_call_id DESC
 LIMIT ${page_probe}"
    )
}

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
        let mut builder = selection_statement(&filters);
        let where_clause = builder.where_clause();
        let source_probe = builder.bind(StatementBind::Count(
            i64::from(max_usage_aggregate_calls()) + 1,
        ));
        let source_limit =
            builder.bind(StatementBind::Count(i64::from(max_usage_aggregate_calls())));
        let group_probe = builder.bind(StatementBind::Count(
            i64::from(max_usage_aggregate_groups()) + 1,
        ));
        let sql = aggregate_sql(&where_clause, source_probe, source_limit, group_probe);
        let rows = builder.query(sql).fetch_all(&self.pool).await?;
        let limit = usize::from(max_usage_aggregate_groups());
        let source_calls_truncated = rows
            .first()
            .map(|row| row.try_get::<bool, _>("calls_truncated"))
            .transpose()?
            .unwrap_or(false);
        let completeness = if rows.len() > limit || source_calls_truncated {
            UsageAggregateCompleteness::Truncated
        } else {
            UsageAggregateCompleteness::Complete
        };
        let groups = rows
            .into_iter()
            .take(limit)
            .map(decode_aggregate)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(UsageAggregateReport {
            groups,
            completeness,
        })
    }

    /// Reads one strict terminal-time/UUID keyset page.
    pub async fn calls(
        &self,
        query: UsageCallQuery,
    ) -> Result<UsageCallPage, UsageRepositoryError> {
        let UsageCallOrder::NewestFirst = query.order;
        let filters = QueryBindings::new(query.scope)?;
        let mut builder = selection_statement(&filters);
        if let Some(cursor) = query.after {
            let time_parameter = builder.bind(StatementBind::Time(timestamp_to_offset(
                cursor.recorded_at,
            )?));
            let call_parameter = builder.bind(StatementBind::Id(cursor.call.into_uuid()));
            builder.predicate(format!(
                "(recorded_at < ${time_parameter} \
                 OR (recorded_at = ${time_parameter} AND model_call_id < ${call_parameter}))"
            ));
        }
        let where_clause = builder.where_clause();
        let page_probe = builder.bind(StatementBind::Count(i64::from(query.limit.get()) + 1));
        let sql = calls_newest_sql(&where_clause, page_probe);
        let rows = builder.query(sql).fetch_all(&self.pool).await?;
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
    let credential_profile: String = row.try_get("credential_reference")?;
    if credential_profile.is_empty()
        || credential_profile.len() > usize::from(max_usage_credential_profile_utf8_bytes())
    {
        return Err(UsageProjectionCorruption::Invalid("credential profile").into());
    }
    let call_kind = decode_call_kind(row.try_get("call_kind")?)?;
    let turn = row
        .try_get::<Option<Uuid>, _>("turn_id")?
        .map(TurnId::from_uuid);
    let scope = match (call_kind, turn) {
        (UsageCallKind::ModelCall, Some(turn)) => UsageCallScope::ModelCall(turn),
        (UsageCallKind::ApprovalJudge, Some(turn)) => UsageCallScope::ApprovalJudge(turn),
        (UsageCallKind::ContextCompaction, None) => UsageCallScope::ContextCompaction,
        (UsageCallKind::ModelCall | UsageCallKind::ApprovalJudge, None)
        | (UsageCallKind::ContextCompaction, Some(_)) => {
            return Err(UsageProjectionCorruption::Invalid("turn correlation").into());
        }
    };
    Ok(UsageCallEvidence {
        scope,
        call: ModelCallId::from_uuid(row.try_get("model_call_id")?),
        session: SessionId::from_uuid(row.try_get("session_id")?),
        model: decode_model(&row)?,
        credential_profile,
        provenance: decode_provenance(row.try_get("usage_provenance_kind")?)?,
        input_semantics: decode_input_semantics(row.try_get("usage_input_includes_cache_tokens")?),
        tokens: decode_tokens(&row)?,
        recorded_at: decode_timestamp(row.try_get("recorded_at")?)?,
    })
}

fn decode_aggregate(row: PgRow) -> Result<UsageAggregateGroup, UsageRepositoryError> {
    let credential_profile: String = row.try_get("credential_reference")?;
    if credential_profile.is_empty()
        || credential_profile.len() > usize::from(max_usage_credential_profile_utf8_bytes())
    {
        return Err(UsageProjectionCorruption::Invalid("credential profile").into());
    }
    let call_count = u64::try_from(row.try_get::<i64, _>("call_count")?)
        .map_err(|_| UsageProjectionCorruption::Invalid("call count"))?;
    Ok(UsageAggregateGroup {
        key: UsageAggregateKey {
            call_kind: decode_call_kind(row.try_get("call_kind")?)?,
            model: decode_model(&row)?,
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
        cache_normalization: decode_cache_normalization(row.try_get("cache_normalization_safe")?),
    })
}

const fn decode_cache_normalization(safe: bool) -> UsageCacheNormalization {
    if safe {
        UsageCacheNormalization::Safe
    } else {
        UsageCacheNormalization::Unsafe
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
    fn timestamp_conversion_preserves_exact_microseconds() {
        let timestamp =
            UsageTimestampMicros::new(1_777_777_777_123_456).expect("fixture timestamp fits");
        let decoded =
            decode_timestamp(timestamp_to_offset(timestamp).expect("fixture timestamp converts"))
                .expect("fixture timestamp decodes");

        assert_eq!(decoded, timestamp);
    }

    fn selection_fixture() -> QueryBindings {
        QueryBindings {
            from: None,
            to: None,
            session: Some(Uuid::from_u128(0xA1)),
            turn: Some(Uuid::from_u128(0xB2)),
            model: None,
            provenance: None,
            call_kind: None,
        }
    }

    #[test]
    fn unselected_shape_reduces_to_a_constant_predicate() {
        let filters = QueryBindings {
            session: None,
            turn: None,
            ..selection_fixture()
        };

        assert_eq!(selection_statement(&filters).where_clause(), "true");
    }

    #[test]
    fn session_only_shape_carries_exactly_the_session_predicate() {
        let filters = QueryBindings {
            turn: None,
            ..selection_fixture()
        };

        assert_eq!(
            selection_statement(&filters).where_clause(),
            "session_id = $1"
        );
    }

    #[test]
    fn combined_session_and_turn_shape_stays_turn_led_with_an_ownership_probe() {
        let filters = selection_fixture();

        assert_eq!(
            selection_statement(&filters).where_clause(),
            "turn_id = $1\n       AND EXISTS (SELECT 1 FROM turn_lifecycle \
             WHERE turn_id = $1 AND session_id = $2)"
        );
    }
}

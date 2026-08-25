//! Bounded usage-evidence query vocabulary.
//!
//! Canonical token evidence remains durable in model-call records. This module
//! owns the product query shape used by dedicated projections, including the
//! dimensions that must remain separate before a configured cost is derived.

use std::{fmt, future::Future};

use signalbox_domain::{ModelCallId, ResolvedProviderTarget, SessionId, TurnId};

/// Maximum individual calls returned by one detail page.
#[must_use]
pub const fn max_usage_call_page_items() -> u16 {
    100
}

/// Maximum compatibility groups returned by one aggregate read.
///
/// This hard safety ceiling protects response memory and serialization latency.
#[must_use]
pub const fn max_usage_aggregate_groups() -> u16 {
    256
}

/// Maximum terminal calls consumed by one aggregate read.
///
/// This hard safety ceiling protects PostgreSQL work and request latency from
/// growing with the lifetime projection.
#[must_use]
pub const fn max_usage_aggregate_calls() -> u16 {
    10_000
}

/// Maximum UTF-8 bytes retained for one credential-profile dimension.
///
/// This hard safety ceiling keeps bounded pages from carrying unbounded copied
/// text and matches credential-catalog admission.
#[must_use]
pub const fn max_usage_credential_profile_utf8_bytes() -> u16 {
    256
}

/// Invalid microsecond timestamp supplied at an application boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageTimestampError {
    /// Rejected microseconds from the Unix epoch.
    pub rejected_micros: u64,
}

impl fmt::Display for UsageTimestampError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "usage timestamp {} microseconds exceeds the supported range",
            self.rejected_micros
        )
    }
}

impl std::error::Error for UsageTimestampError {}

/// Exact nonnegative microseconds from the Unix epoch.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UsageTimestampMicros(u64);

impl UsageTimestampMicros {
    /// Admits values representable by PostgreSQL adapter timestamp conversion.
    pub const fn new(value: u64) -> Result<Self, UsageTimestampError> {
        // 9999-12-31T23:59:59.999999Z, the shared PostgreSQL/time boundary.
        if value > 253_402_300_799_999_999 {
            Err(UsageTimestampError {
                rejected_micros: value,
            })
        } else {
            Ok(Self(value))
        }
    }

    /// Returns exact microseconds from the Unix epoch.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Invalid half-open usage time range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageTimeRangeError {
    /// Rejected inclusive lower boundary, in microseconds from the Unix epoch.
    pub from_inclusive_micros: u64,
    /// Rejected exclusive upper boundary, in microseconds from the Unix epoch.
    pub to_exclusive_micros: u64,
}

impl fmt::Display for UsageTimeRangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "usage time range [{}, {}) microseconds is empty or reversed",
            self.from_inclusive_micros, self.to_exclusive_micros
        )
    }
}

impl std::error::Error for UsageTimeRangeError {}

/// Inclusive lower usage-time boundary.
///
/// This newtype exists so the two same-typed boundaries of
/// [`UsageTimeRange::new`] cannot be transposed silently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageTimeFromInclusive(pub UsageTimestampMicros);

/// Exclusive upper usage-time boundary.
///
/// This newtype exists so the two same-typed boundaries of
/// [`UsageTimeRange::new`] cannot be transposed silently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageTimeToExclusive(pub UsageTimestampMicros);

/// Optional half-open terminal-evidence time range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageTimeRange {
    from_inclusive: Option<UsageTimestampMicros>,
    to_exclusive: Option<UsageTimestampMicros>,
}

impl UsageTimeRange {
    /// Constructs an unbounded range.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            from_inclusive: None,
            to_exclusive: None,
        }
    }

    /// Admits a nonempty half-open range.
    pub const fn new(
        from_inclusive: Option<UsageTimeFromInclusive>,
        to_exclusive: Option<UsageTimeToExclusive>,
    ) -> Result<Self, UsageTimeRangeError> {
        if let (Some(UsageTimeFromInclusive(from)), Some(UsageTimeToExclusive(to))) =
            (from_inclusive, to_exclusive)
            && from.0 >= to.0
        {
            return Err(UsageTimeRangeError {
                from_inclusive_micros: from.0,
                to_exclusive_micros: to.0,
            });
        }
        Ok(Self {
            from_inclusive: match from_inclusive {
                Some(UsageTimeFromInclusive(from)) => Some(from),
                None => None,
            },
            to_exclusive: match to_exclusive {
                Some(UsageTimeToExclusive(to)) => Some(to),
                None => None,
            },
        })
    }

    /// Returns the inclusive lower boundary.
    #[must_use]
    pub const fn from_inclusive(self) -> Option<UsageTimestampMicros> {
        self.from_inclusive
    }

    /// Returns the exclusive upper boundary.
    #[must_use]
    pub const fn to_exclusive(self) -> Option<UsageTimestampMicros> {
        self.to_exclusive
    }
}

/// Closed physical call class represented by canonical usage evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UsageCallKind {
    /// An ordinary transcript-producing model call.
    ModelCall,
    /// A delegated tool-approval judge call.
    ApprovalJudge,
    /// A session-level context-summary production call.
    ContextCompaction,
}

/// Closed provenance of token evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UsageProvenance {
    /// Counts reported by the provider or adapter.
    Reported,
    /// Counts produced by an explicit estimator.
    Estimated,
}

/// Meaning of the input-token axis for one provider target.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UsageInputTokenSemantics {
    /// Historical evidence predates a durable adapter meaning.
    Unknown,
    /// Input excludes separately reported cache axes.
    CacheExclusive,
    /// Input includes separately reported cache axes.
    CacheInclusive,
}

/// Typed presence state for one independently optional token axis.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UsageTokenPresence {
    /// No count was reported for this axis.
    Absent,
    /// A count, including zero, was reported for this axis.
    Present,
}

/// Presence of every independently optional token axis.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UsageTokenCoverage {
    /// Input-token presence.
    pub input: UsageTokenPresence,
    /// Output-token presence.
    pub output: UsageTokenPresence,
    /// Cache-creation input-token presence.
    pub cache_creation_input: UsageTokenPresence,
    /// Cache-read input-token presence.
    pub cache_read_input: UsageTokenPresence,
}

/// Independently optional token-axis values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageTokenAxes {
    /// Input-token count, under the named semantics.
    pub input: Option<u64>,
    /// Output-token count.
    pub output: Option<u64>,
    /// Cache-creation input-token count.
    pub cache_creation_input: Option<u64>,
    /// Cache-read input-token count.
    pub cache_read_input: Option<u64>,
}

impl UsageTokenAxes {
    /// Returns the exact presence shape without replacing absence by zero.
    #[must_use]
    pub const fn coverage(self) -> UsageTokenCoverage {
        UsageTokenCoverage {
            input: token_presence(self.input),
            output: token_presence(self.output),
            cache_creation_input: token_presence(self.cache_creation_input),
            cache_read_input: token_presence(self.cache_read_input),
        }
    }
}

const fn token_presence(value: Option<u64>) -> UsageTokenPresence {
    if value.is_some() {
        UsageTokenPresence::Present
    } else {
        UsageTokenPresence::Absent
    }
}

/// Aggregate token-axis sums widened beyond one physical call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageAggregateTokenAxes {
    /// Input-token sum, under the named semantics.
    pub input: Option<u128>,
    /// Output-token sum.
    pub output: Option<u128>,
    /// Cache-creation input-token sum.
    pub cache_creation_input: Option<u128>,
    /// Cache-read input-token sum.
    pub cache_read_input: Option<u128>,
}

/// Optional exact filters shared by aggregate and detail reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageSelection {
    /// Exact session, or every session.
    pub session: Option<SessionId>,
    /// Exact turn, or every turn in the selected session scope. A turn belongs
    /// to exactly one session, so combining a turn with a session it does not
    /// belong to selects nothing, proven by one bounded ownership probe.
    pub turn: Option<TurnId>,
    /// Exact resolved provider/model target.
    pub model: Option<ResolvedProviderTarget>,
    /// Exact evidence provenance.
    pub provenance: Option<UsageProvenance>,
    /// Exact physical call class.
    pub call_kind: Option<UsageCallKind>,
}

impl UsageSelection {
    /// Selects every canonical call.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            session: None,
            turn: None,
            model: None,
            provenance: None,
            call_kind: None,
        }
    }
}

/// Shared aggregate/detail query scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageQuery {
    /// Terminal-evidence time range.
    pub time: UsageTimeRange,
    /// Optional exact dimensions.
    pub selection: UsageSelection,
}

/// Rejection of an individual-call page size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageCallPageLimitError {
    /// Rejected page size.
    pub rejected_items: u16,
}

impl fmt::Display for UsageCallPageLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "usage call page size {} is outside 1..={}",
            self.rejected_items,
            max_usage_call_page_items()
        )
    }
}

impl std::error::Error for UsageCallPageLimitError {}

/// Validated item ceiling for one individual-call page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageCallPageLimit(u16);

impl UsageCallPageLimit {
    /// Admits one through the application hard ceiling.
    pub const fn new(value: u16) -> Result<Self, UsageCallPageLimitError> {
        if value == 0 || value > max_usage_call_page_items() {
            Err(UsageCallPageLimitError {
                rejected_items: value,
            })
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the admitted page size.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Stable direction for terminal-time detail traversal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCallOrder {
    /// Newest terminal evidence first.
    NewestFirst,
}

/// Strict detail-page keyset boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageCallCursor {
    /// Exact terminal-evidence timestamp.
    pub recorded_at: UsageTimestampMicros,
    /// UUID tiebreaker for calls sharing one terminal statement timestamp.
    pub call: ModelCallId,
}

/// Bounded individual-call request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageCallQuery {
    /// Shared selection and time scope.
    pub scope: UsageQuery,
    /// Stable traversal direction.
    pub order: UsageCallOrder,
    /// Maximum returned calls.
    pub limit: UsageCallPageLimit,
    /// Optional strict keyset boundary.
    pub after: Option<UsageCallCursor>,
}

/// One canonical terminal model-call usage record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageCallEvidence {
    /// Ordinary, approval-judge, or context-compaction call.
    pub call_kind: UsageCallKind,
    /// Exact physical call identity.
    pub call: ModelCallId,
    /// Owning session.
    pub session: SessionId,
    /// Owning turn, absent for session-level context compaction.
    pub turn: Option<TurnId>,
    /// Resolved provider/model target.
    pub model: ResolvedProviderTarget,
    /// Bounded non-secret projection label for the credential profile, not the
    /// canonical credential reference: references of at most 250 UTF-8 bytes
    /// appear as `exact:<reference>`, longer ones as a stable projection-owned
    /// `mapped:<id>` identity. Strip the `exact:` discriminator before any
    /// exact credential-catalog lookup; a `mapped:` label resolves only through
    /// the projection's oversized-reference mapping.
    pub credential_profile: String,
    /// Reported or estimated provenance.
    pub provenance: UsageProvenance,
    /// Meaning of the input-token axis.
    pub input_semantics: UsageInputTokenSemantics,
    /// Independently optional token axes.
    pub tokens: UsageTokenAxes,
    /// Terminal-evidence time.
    pub recorded_at: UsageTimestampMicros,
}

/// One bounded detail page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageCallPage {
    /// Calls in the requested stable order.
    pub calls: Vec<UsageCallEvidence>,
    /// Strict continuation when another call exists.
    pub next: Option<UsageCallCursor>,
}

/// Compatibility key that prevents unsafe aggregate summation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageAggregateKey {
    /// Ordinary, approval-judge, or context-compaction call.
    pub call_kind: UsageCallKind,
    /// Resolved provider/model target.
    pub model: ResolvedProviderTarget,
    /// Bounded non-secret credential-profile projection label used as the cost
    /// dimension; see [`UsageCallEvidence::credential_profile`] for the
    /// `exact:`/`mapped:` label forms and their distinction from the canonical
    /// credential reference.
    pub credential_profile: String,
    /// Reported or estimated provenance.
    pub provenance: UsageProvenance,
    /// Meaning of the input-token axis.
    pub input_semantics: UsageInputTokenSemantics,
    /// Exact token-axis presence shape.
    pub coverage: UsageTokenCoverage,
}

/// Whether cache-inclusive input can be normalized without underflow.
///
/// Safety here does not assert that later rate arithmetic is representable or
/// equivalent to checked per-call cost derivation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCacheNormalization {
    /// Normalizing cache-inclusive input would underflow or lacks the cache
    /// axes it needs, so a consumer must not subtract cache counts from input.
    Unsafe,
    /// Every call in the group carries the cache axes and input at least their
    /// sum, so cache-inclusive input can be normalized without underflow.
    Safe,
}

/// Whether an aggregate result covers every matching call and group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageAggregateCompleteness {
    /// Every matching source call and compatibility group is represented.
    Complete,
    /// The source-call or compatibility-group hard ceiling truncated the
    /// aggregate result.
    Truncated,
}

/// One aggregate over calls with fully compatible evidence dimensions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageAggregateGroup {
    /// Dimensions retained for safe cost derivation and presentation.
    pub key: UsageAggregateKey,
    /// Exact number of calls represented by this group.
    pub call_count: u64,
    /// Sums only for axes present on every call in the group.
    pub tokens: UsageAggregateTokenAxes,
    /// Whether cache-inclusive input can be normalized without underflow.
    pub cache_normalization: UsageCacheNormalization,
}

/// Bounded aggregate result with explicit truncation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageAggregateReport {
    /// Compatibility-preserving groups.
    pub groups: Vec<UsageAggregateGroup>,
    /// Whether either hard safety ceiling truncated the aggregate result.
    pub completeness: UsageAggregateCompleteness,
}

/// Dedicated aggregate and detail read boundary for canonical usage evidence.
pub trait UsageReader {
    /// Adapter-specific infrastructure or integrity failure.
    type Error;

    /// Reads bounded compatibility-preserving aggregate groups.
    fn aggregate(
        &self,
        query: UsageQuery,
    ) -> impl Future<Output = Result<UsageAggregateReport, Self::Error>> + Send;

    /// Reads one strict page of individual terminal calls.
    fn calls(
        &self,
        query: UsageCallQuery,
    ) -> impl Future<Output = Result<UsageCallPage, Self::Error>> + Send;
}

/// Coordinates dedicated usage reads without transcript materialization.
#[derive(Debug)]
pub struct UsageService<Reader> {
    reader: Reader,
}

impl<Reader> UsageService<Reader> {
    /// Wraps one dedicated usage adapter.
    #[must_use]
    pub const fn new(reader: Reader) -> Self {
        Self { reader }
    }
}

impl<Reader: UsageReader> UsageService<Reader> {
    /// Reads bounded aggregate groups.
    pub async fn aggregate(
        &self,
        query: UsageQuery,
    ) -> Result<UsageAggregateReport, Reader::Error> {
        self.reader.aggregate(query).await
    }

    /// Reads one bounded individual-call page.
    pub async fn calls(&self, query: UsageCallQuery) -> Result<UsageCallPage, Reader::Error> {
        self.reader.calls(query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_call_page_limit_rejects_values_outside_the_hard_bounds() {
        assert_eq!(
            UsageCallPageLimit::new(0),
            Err(UsageCallPageLimitError { rejected_items: 0 })
        );
        assert_eq!(
            UsageCallPageLimit::new(max_usage_call_page_items() + 1),
            Err(UsageCallPageLimitError {
                rejected_items: max_usage_call_page_items() + 1,
            })
        );
    }

    #[test]
    fn usage_timestamp_rejects_values_outside_the_postgres_adapter_range() {
        assert_eq!(
            UsageTimestampMicros::new(253_402_300_800_000_000),
            Err(UsageTimestampError {
                rejected_micros: 253_402_300_800_000_000,
            })
        );
    }

    #[test]
    fn usage_time_range_rejects_an_empty_half_open_interval() {
        assert_eq!(
            UsageTimeRange::new(
                Some(UsageTimeFromInclusive(
                    UsageTimestampMicros::new(8).expect("fixture timestamp fits")
                )),
                Some(UsageTimeToExclusive(
                    UsageTimestampMicros::new(8).expect("fixture timestamp fits")
                )),
            ),
            Err(UsageTimeRangeError {
                from_inclusive_micros: 8,
                to_exclusive_micros: 8,
            })
        );
    }

    #[test]
    fn token_coverage_preserves_missing_axes() {
        let tokens = UsageTokenAxes {
            input: Some(0),
            output: None,
            cache_creation_input: Some(7),
            cache_read_input: None,
        };

        assert_eq!(
            tokens.coverage(),
            UsageTokenCoverage {
                input: UsageTokenPresence::Present,
                output: UsageTokenPresence::Absent,
                cache_creation_input: UsageTokenPresence::Present,
                cache_read_input: UsageTokenPresence::Absent,
            }
        );
    }
}

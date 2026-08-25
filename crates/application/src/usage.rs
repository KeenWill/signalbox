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

/// Rejection of a credential-profile projection label.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCredentialProfileLabelError {
    /// The label was empty.
    Empty,
    /// The label exceeded the bounded projection size.
    Oversized {
        /// Rejected UTF-8 byte length.
        rejected_utf8_bytes: usize,
    },
    /// The label carried neither the `exact:` nor the `mapped:` discriminator
    /// with a nonempty tail.
    UndiscriminatedForm,
}

impl fmt::Display for UsageCredentialProfileLabelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "credential-profile label is empty"),
            Self::Oversized {
                rejected_utf8_bytes,
            } => write!(
                formatter,
                "credential-profile label carries {rejected_utf8_bytes} UTF-8 bytes, over the \
                 {} ceiling",
                max_usage_credential_profile_utf8_bytes()
            ),
            Self::UndiscriminatedForm => write!(
                formatter,
                "credential-profile label carries neither the `exact:` nor the `mapped:` \
                 discriminator with a nonempty tail"
            ),
        }
    }
}

impl std::error::Error for UsageCredentialProfileLabelError {}

/// Bounded non-secret projection label for a credential profile, not the
/// canonical credential reference: references of at most 250 UTF-8 bytes
/// appear as `exact:<reference>`, longer ones as a stable projection-owned
/// `mapped:<id>` identity. Strip the `exact:` discriminator before any exact
/// credential-catalog lookup; a `mapped:` label resolves only through the
/// projection's oversized-reference mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageCredentialProfileLabel(String);

impl UsageCredentialProfileLabel {
    /// Accepts only a nonempty, bounded, discriminated projection label.
    pub fn new(label: String) -> Result<Self, UsageCredentialProfileLabelError> {
        if label.is_empty() {
            return Err(UsageCredentialProfileLabelError::Empty);
        }
        if label.len() > usize::from(max_usage_credential_profile_utf8_bytes()) {
            return Err(UsageCredentialProfileLabelError::Oversized {
                rejected_utf8_bytes: label.len(),
            });
        }
        let discriminated = label
            .strip_prefix("exact:")
            .or_else(|| label.strip_prefix("mapped:"))
            .is_some_and(|tail| !tail.is_empty());
        if !discriminated {
            return Err(UsageCredentialProfileLabelError::UndiscriminatedForm);
        }
        Ok(Self(label))
    }

    /// Returns the label text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owned label text.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
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

/// Turn correlation fused with the physical call class, so a session-level
/// context-compaction call cannot carry a turn and a turn-owned call cannot
/// lack one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCallScope {
    /// An ordinary transcript-producing model call within its owning turn.
    ModelCall(TurnId),
    /// A delegated tool-approval judge call within its owning turn.
    ApprovalJudge(TurnId),
    /// A session-level context-summary production call with no turn identity.
    ContextCompaction,
}

impl UsageCallScope {
    /// Physical call class of this scope.
    #[must_use]
    pub const fn call_kind(self) -> UsageCallKind {
        match self {
            Self::ModelCall(_) => UsageCallKind::ModelCall,
            Self::ApprovalJudge(_) => UsageCallKind::ApprovalJudge,
            Self::ContextCompaction => UsageCallKind::ContextCompaction,
        }
    }

    /// Owning turn, absent exactly for session-level context compaction.
    #[must_use]
    pub const fn turn(self) -> Option<TurnId> {
        match self {
            Self::ModelCall(turn) | Self::ApprovalJudge(turn) => Some(turn),
            Self::ContextCompaction => None,
        }
    }
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
    /// Turn-correlated call scope: a turn-owned ordinary or approval-judge
    /// call, or session-level context compaction without a turn.
    pub scope: UsageCallScope,
    /// Exact physical call identity.
    pub call: ModelCallId,
    /// Owning session.
    pub session: SessionId,
    /// Resolved provider/model target.
    pub model: ResolvedProviderTarget,
    /// Bounded non-secret credential-profile projection label.
    pub credential_profile: UsageCredentialProfileLabel,
    /// Reported or estimated provenance.
    pub provenance: UsageProvenance,
    /// Meaning of the input-token axis.
    pub input_semantics: UsageInputTokenSemantics,
    /// Independently optional token axes.
    pub tokens: UsageTokenAxes,
    /// Terminal-evidence time.
    pub recorded_at: UsageTimestampMicros,
}

/// Whether more matching calls exist behind a detail page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCallPageContinuation {
    /// The page ends the matching evidence.
    Exhausted,
    /// More matching calls exist strictly after the page's last call.
    HasMore,
}

/// A detail page that violated its construction bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCallPageError {
    /// The page carried more calls than its requested limit.
    Overflow {
        /// Calls the reader tried to return.
        returned_calls: usize,
        /// Requested page ceiling.
        limit_items: u16,
    },
    /// The page claimed more matching calls behind it while returning none, so
    /// no last call exists to anchor the continuation cursor.
    DanglingContinuation,
    /// The page's calls are not in strictly newest-first
    /// `(recorded_at, call)` order, so a derived cursor would skip or repeat
    /// evidence.
    Misordered {
        /// Position of the first call not strictly older than its predecessor.
        position: usize,
    },
}

impl fmt::Display for UsageCallPageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow {
                returned_calls,
                limit_items,
            } => write!(
                formatter,
                "usage detail page carries {returned_calls} calls, over its requested limit of \
                 {limit_items}"
            ),
            Self::DanglingContinuation => write!(
                formatter,
                "usage detail page claims more matching calls but returns none to anchor the \
                 continuation cursor"
            ),
            Self::Misordered { position } => write!(
                formatter,
                "usage detail page call at position {position} is not strictly older than its \
                 predecessor"
            ),
        }
    }
}

impl std::error::Error for UsageCallPageError {}

/// One bounded detail page: no larger than its requested limit, with its
/// continuation cursor derived from its own last returned call, by
/// construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageCallPage {
    calls: Vec<UsageCallEvidence>,
    next: Option<UsageCallCursor>,
}

impl UsageCallPage {
    /// Accepts only a strictly newest-first page within the requested limit,
    /// deriving the continuation cursor from the last returned call.
    pub fn new(
        calls: Vec<UsageCallEvidence>,
        continuation: UsageCallPageContinuation,
        limit: UsageCallPageLimit,
    ) -> Result<Self, UsageCallPageError> {
        if calls.len() > usize::from(limit.get()) {
            return Err(UsageCallPageError::Overflow {
                returned_calls: calls.len(),
                limit_items: limit.get(),
            });
        }
        let misordered = calls.windows(2).position(|pair| {
            (pair[1].recorded_at, pair[1].call.into_uuid())
                >= (pair[0].recorded_at, pair[0].call.into_uuid())
        });
        if let Some(offset) = misordered {
            return Err(UsageCallPageError::Misordered {
                position: offset + 1,
            });
        }
        let next = match continuation {
            UsageCallPageContinuation::Exhausted => None,
            UsageCallPageContinuation::HasMore => {
                let Some(last) = calls.last() else {
                    return Err(UsageCallPageError::DanglingContinuation);
                };
                Some(UsageCallCursor {
                    recorded_at: last.recorded_at,
                    call: last.call,
                })
            }
        };
        Ok(Self { calls, next })
    }

    /// Calls in the requested stable order.
    #[must_use]
    pub fn calls(&self) -> &[UsageCallEvidence] {
        &self.calls
    }

    /// Strict continuation at the last returned call when another call exists.
    #[must_use]
    pub const fn next(&self) -> Option<UsageCallCursor> {
        self.next
    }
}

/// Compatibility key that prevents unsafe aggregate summation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageAggregateKey {
    /// Ordinary, approval-judge, or context-compaction call.
    pub call_kind: UsageCallKind,
    /// Resolved provider/model target.
    pub model: ResolvedProviderTarget,
    /// Bounded non-secret credential-profile projection label used as the cost
    /// dimension.
    pub credential_profile: UsageCredentialProfileLabel,
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
    key: UsageAggregateKey,
    call_count: u64,
    tokens: UsageAggregateTokenAxes,
    cache_normalization: UsageCacheNormalization,
}

/// One independently optional token axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageTokenAxis {
    /// The input-token axis.
    Input,
    /// The output-token axis.
    Output,
    /// The cache-creation input-token axis.
    CacheCreationInput,
    /// The cache-read input-token axis.
    CacheReadInput,
}

/// An aggregate group that violated its construction consistency rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageAggregateGroupError {
    /// A sum contradicts its declared presence coverage.
    Coverage {
        /// Axis whose sum and declared presence disagree.
        axis: UsageTokenAxis,
        /// Presence the key declares for that axis.
        declared: UsageTokenPresence,
    },
    /// The cache-normalization state contradicts the group's input-token
    /// semantics and sums.
    NormalizationClaim {
        /// Claimed normalization state.
        claimed: UsageCacheNormalization,
        /// Input-token semantics the key declares.
        input_semantics: UsageInputTokenSemantics,
    },
}

impl fmt::Display for UsageAggregateGroupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coverage { axis, declared } => write!(
                formatter,
                "aggregate {axis:?} sum contradicts its declared {declared:?} coverage"
            ),
            Self::NormalizationClaim {
                claimed,
                input_semantics,
            } => write!(
                formatter,
                "aggregate {claimed:?} cache normalization contradicts its {input_semantics:?} \
                 input-token semantics and sums"
            ),
        }
    }
}

impl std::error::Error for UsageAggregateGroupError {}

const fn coverage_agrees(sum: Option<u128>, declared: UsageTokenPresence) -> bool {
    matches!(
        (sum, declared),
        (Some(_), UsageTokenPresence::Present) | (None, UsageTokenPresence::Absent)
    )
}

/// Whether the normalization claim is consistent with the declared semantics
/// and sums: unknown semantics are never safe, cache-exclusive input is always
/// safe, and a cache-inclusive safety claim requires every cache axis present
/// with input at least their sum. A cache-inclusive `Unsafe` claim is always
/// admissible because per-call underflow is not derivable from group sums.
const fn normalization_claim_consistent(
    input_semantics: UsageInputTokenSemantics,
    tokens: UsageAggregateTokenAxes,
    claimed: UsageCacheNormalization,
) -> bool {
    match (input_semantics, claimed) {
        (UsageInputTokenSemantics::Unknown, UsageCacheNormalization::Unsafe)
        | (UsageInputTokenSemantics::CacheExclusive, UsageCacheNormalization::Safe)
        | (UsageInputTokenSemantics::CacheInclusive, UsageCacheNormalization::Unsafe) => true,
        (UsageInputTokenSemantics::Unknown, UsageCacheNormalization::Safe)
        | (UsageInputTokenSemantics::CacheExclusive, UsageCacheNormalization::Unsafe) => false,
        (UsageInputTokenSemantics::CacheInclusive, UsageCacheNormalization::Safe) => {
            match (
                tokens.input,
                tokens.cache_creation_input,
                tokens.cache_read_input,
            ) {
                (Some(input), Some(cache_creation), Some(cache_read)) => {
                    // An unrepresentable cache-axis total can never certify
                    // safety; checked addition rejects it instead of wrapping.
                    match cache_creation.checked_add(cache_read) {
                        Some(cache_total) => input >= cache_total,
                        None => false,
                    }
                }
                (None, _, _) | (_, None, _) | (_, _, None) => false,
            }
        }
    }
}

impl UsageAggregateGroup {
    /// Accepts only sums that agree with the key's declared presence coverage
    /// and a normalization state consistent with the semantics and sums.
    pub fn new(
        key: UsageAggregateKey,
        call_count: u64,
        tokens: UsageAggregateTokenAxes,
        cache_normalization: UsageCacheNormalization,
    ) -> Result<Self, UsageAggregateGroupError> {
        if !coverage_agrees(tokens.input, key.coverage.input) {
            return Err(UsageAggregateGroupError::Coverage {
                axis: UsageTokenAxis::Input,
                declared: key.coverage.input,
            });
        }
        if !coverage_agrees(tokens.output, key.coverage.output) {
            return Err(UsageAggregateGroupError::Coverage {
                axis: UsageTokenAxis::Output,
                declared: key.coverage.output,
            });
        }
        if !coverage_agrees(
            tokens.cache_creation_input,
            key.coverage.cache_creation_input,
        ) {
            return Err(UsageAggregateGroupError::Coverage {
                axis: UsageTokenAxis::CacheCreationInput,
                declared: key.coverage.cache_creation_input,
            });
        }
        if !coverage_agrees(tokens.cache_read_input, key.coverage.cache_read_input) {
            return Err(UsageAggregateGroupError::Coverage {
                axis: UsageTokenAxis::CacheReadInput,
                declared: key.coverage.cache_read_input,
            });
        }
        if !normalization_claim_consistent(key.input_semantics, tokens, cache_normalization) {
            return Err(UsageAggregateGroupError::NormalizationClaim {
                claimed: cache_normalization,
                input_semantics: key.input_semantics,
            });
        }
        Ok(Self {
            key,
            call_count,
            tokens,
            cache_normalization,
        })
    }

    /// Dimensions retained for safe cost derivation and presentation.
    #[must_use]
    pub const fn key(&self) -> &UsageAggregateKey {
        &self.key
    }

    /// Exact number of calls represented by this group.
    #[must_use]
    pub const fn call_count(&self) -> u64 {
        self.call_count
    }

    /// Sums only for axes present on every call in the group, agreeing with
    /// the key's declared coverage by construction.
    #[must_use]
    pub const fn tokens(&self) -> UsageAggregateTokenAxes {
        self.tokens
    }

    /// Whether cache-inclusive input can be normalized without underflow.
    #[must_use]
    pub const fn cache_normalization(&self) -> UsageCacheNormalization {
        self.cache_normalization
    }
}

/// An aggregate result that exceeded a hard aggregate ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageAggregateReportError {
    /// The result carried more groups than the hard group ceiling.
    GroupOverflow {
        /// Groups the reader tried to return.
        returned_groups: usize,
    },
    /// The result's groups together represent more source calls than one
    /// aggregate read may consume.
    SourceCallOverflow {
        /// Source calls the groups claim to represent.
        represented_calls: u128,
    },
}

impl fmt::Display for UsageAggregateReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GroupOverflow { returned_groups } => write!(
                formatter,
                "usage aggregate carries {returned_groups} groups, over the {} ceiling",
                max_usage_aggregate_groups()
            ),
            Self::SourceCallOverflow { represented_calls } => write!(
                formatter,
                "usage aggregate represents {represented_calls} source calls, over the {} ceiling",
                max_usage_aggregate_calls()
            ),
        }
    }
}

impl std::error::Error for UsageAggregateReportError {}

/// Bounded aggregate result with explicit truncation, within the hard group
/// and source-call ceilings by construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageAggregateReport {
    groups: Vec<UsageAggregateGroup>,
    completeness: UsageAggregateCompleteness,
}

impl UsageAggregateReport {
    /// Accepts only a result within the hard group and source-call ceilings.
    pub fn new(
        groups: Vec<UsageAggregateGroup>,
        completeness: UsageAggregateCompleteness,
    ) -> Result<Self, UsageAggregateReportError> {
        if groups.len() > usize::from(max_usage_aggregate_groups()) {
            return Err(UsageAggregateReportError::GroupOverflow {
                returned_groups: groups.len(),
            });
        }
        // At most 256 u64 counts, so the u128 sum cannot overflow.
        let represented_calls: u128 = groups
            .iter()
            .map(|group| u128::from(group.call_count()))
            .sum();
        if represented_calls > u128::from(max_usage_aggregate_calls()) {
            return Err(UsageAggregateReportError::SourceCallOverflow { represented_calls });
        }
        Ok(Self {
            groups,
            completeness,
        })
    }

    /// Compatibility-preserving groups.
    #[must_use]
    pub fn groups(&self) -> &[UsageAggregateGroup] {
        &self.groups
    }

    /// Whether either hard safety ceiling truncated the aggregate result.
    #[must_use]
    pub const fn completeness(&self) -> UsageAggregateCompleteness {
        self.completeness
    }
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
    use signalbox_domain::ProviderModelIdentity;

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
    fn credential_profile_label_rejects_empty_oversized_and_undiscriminated_text() {
        assert_eq!(
            UsageCredentialProfileLabel::new(String::new()),
            Err(UsageCredentialProfileLabelError::Empty)
        );
        assert_eq!(
            UsageCredentialProfileLabel::new(format!(
                "exact:{}",
                "a".repeat(usize::from(max_usage_credential_profile_utf8_bytes()))
            )),
            Err(UsageCredentialProfileLabelError::Oversized {
                rejected_utf8_bytes: usize::from(max_usage_credential_profile_utf8_bytes()) + 6,
            })
        );
        assert_eq!(
            UsageCredentialProfileLabel::new("profile-one".to_owned()),
            Err(UsageCredentialProfileLabelError::UndiscriminatedForm)
        );
        assert_eq!(
            UsageCredentialProfileLabel::new("mapped:".to_owned()),
            Err(UsageCredentialProfileLabelError::UndiscriminatedForm)
        );
    }

    #[test]
    fn credential_profile_label_accepts_both_discriminated_forms() {
        let exact = UsageCredentialProfileLabel::new("exact:profile-one".to_owned())
            .expect("fixture exact label is bounded and discriminated");
        let mapped = UsageCredentialProfileLabel::new("mapped:7".to_owned())
            .expect("fixture mapped label is bounded and discriminated");

        assert_eq!(exact.as_str(), "exact:profile-one");
        assert_eq!(mapped.into_string(), "mapped:7");
    }

    fn aggregate_key_fixture() -> UsageAggregateKey {
        UsageAggregateKey {
            call_kind: UsageCallKind::ModelCall,
            model: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                uuid::Uuid::from_u128(0xC3),
            )),
            credential_profile: UsageCredentialProfileLabel::new("exact:profile-one".to_owned())
                .expect("fixture label is bounded and discriminated"),
            provenance: UsageProvenance::Reported,
            input_semantics: UsageInputTokenSemantics::CacheExclusive,
            coverage: UsageTokenCoverage {
                input: UsageTokenPresence::Present,
                output: UsageTokenPresence::Absent,
                cache_creation_input: UsageTokenPresence::Absent,
                cache_read_input: UsageTokenPresence::Absent,
            },
        }
    }

    #[test]
    fn aggregate_group_rejects_a_sum_contradicting_declared_coverage() {
        assert_eq!(
            UsageAggregateGroup::new(
                aggregate_key_fixture(),
                1,
                UsageAggregateTokenAxes {
                    input: None,
                    output: Some(9),
                    cache_creation_input: None,
                    cache_read_input: None,
                },
                UsageCacheNormalization::Safe,
            ),
            Err(UsageAggregateGroupError::Coverage {
                axis: UsageTokenAxis::Input,
                declared: UsageTokenPresence::Present,
            })
        );
    }

    #[test]
    fn aggregate_group_accepts_sums_agreeing_with_declared_coverage() {
        let group = UsageAggregateGroup::new(
            aggregate_key_fixture(),
            2,
            UsageAggregateTokenAxes {
                input: Some(28),
                output: None,
                cache_creation_input: None,
                cache_read_input: None,
            },
            UsageCacheNormalization::Safe,
        )
        .expect("fixture sums agree with declared coverage");

        assert_eq!(group.call_count(), 2);
        assert_eq!(group.tokens().input, Some(28));
        assert_eq!(group.cache_normalization(), UsageCacheNormalization::Safe);
        assert_eq!(group.key(), &aggregate_key_fixture());
    }

    #[test]
    fn aggregate_report_rejects_results_over_the_group_ceiling() {
        let group = UsageAggregateGroup::new(
            aggregate_key_fixture(),
            1,
            UsageAggregateTokenAxes {
                input: Some(11),
                output: None,
                cache_creation_input: None,
                cache_read_input: None,
            },
            UsageCacheNormalization::Safe,
        )
        .expect("fixture sums agree with declared coverage");
        let over_ceiling = usize::from(max_usage_aggregate_groups()) + 1;

        assert_eq!(
            UsageAggregateReport::new(
                vec![group; over_ceiling],
                UsageAggregateCompleteness::Truncated,
            ),
            Err(UsageAggregateReportError::GroupOverflow {
                returned_groups: over_ceiling,
            })
        );
    }

    #[test]
    fn aggregate_report_rejects_results_over_the_source_call_ceiling() {
        let group = UsageAggregateGroup::new(
            aggregate_key_fixture(),
            u64::from(max_usage_aggregate_calls()) + 1,
            UsageAggregateTokenAxes {
                input: Some(11),
                output: None,
                cache_creation_input: None,
                cache_read_input: None,
            },
            UsageCacheNormalization::Safe,
        )
        .expect("fixture sums agree with declared coverage");

        assert_eq!(
            UsageAggregateReport::new(vec![group], UsageAggregateCompleteness::Truncated),
            Err(UsageAggregateReportError::SourceCallOverflow {
                represented_calls: u128::from(max_usage_aggregate_calls()) + 1,
            })
        );
    }

    #[test]
    fn aggregate_group_rejects_an_underflowing_cache_inclusive_safety_claim() {
        let mut key = aggregate_key_fixture();
        key.input_semantics = UsageInputTokenSemantics::CacheInclusive;
        key.coverage = UsageTokenCoverage {
            input: UsageTokenPresence::Present,
            output: UsageTokenPresence::Absent,
            cache_creation_input: UsageTokenPresence::Present,
            cache_read_input: UsageTokenPresence::Present,
        };

        assert_eq!(
            UsageAggregateGroup::new(
                key,
                1,
                UsageAggregateTokenAxes {
                    input: Some(1),
                    output: None,
                    cache_creation_input: Some(2),
                    cache_read_input: Some(0),
                },
                UsageCacheNormalization::Safe,
            ),
            Err(UsageAggregateGroupError::NormalizationClaim {
                claimed: UsageCacheNormalization::Safe,
                input_semantics: UsageInputTokenSemantics::CacheInclusive,
            })
        );
    }

    #[test]
    fn aggregate_group_rejects_a_safety_claim_under_unknown_semantics() {
        let mut key = aggregate_key_fixture();
        key.input_semantics = UsageInputTokenSemantics::Unknown;

        assert_eq!(
            UsageAggregateGroup::new(
                key,
                1,
                UsageAggregateTokenAxes {
                    input: Some(11),
                    output: None,
                    cache_creation_input: None,
                    cache_read_input: None,
                },
                UsageCacheNormalization::Safe,
            ),
            Err(UsageAggregateGroupError::NormalizationClaim {
                claimed: UsageCacheNormalization::Safe,
                input_semantics: UsageInputTokenSemantics::Unknown,
            })
        );
    }

    fn call_evidence_fixture() -> UsageCallEvidence {
        UsageCallEvidence {
            scope: UsageCallScope::ContextCompaction,
            call: ModelCallId::from_uuid(uuid::Uuid::from_u128(0xD4)),
            session: SessionId::from_uuid(uuid::Uuid::from_u128(0xD5)),
            model: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                uuid::Uuid::from_u128(0xD6),
            )),
            credential_profile: UsageCredentialProfileLabel::new("exact:profile-one".to_owned())
                .expect("fixture label is bounded and discriminated"),
            provenance: UsageProvenance::Reported,
            input_semantics: UsageInputTokenSemantics::CacheInclusive,
            tokens: UsageTokenAxes {
                input: Some(17),
                output: Some(5),
                cache_creation_input: Some(0),
                cache_read_input: Some(0),
            },
            recorded_at: UsageTimestampMicros::new(1_777_777_777_000_000)
                .expect("fixture timestamp fits"),
        }
    }

    #[test]
    fn call_page_derives_its_continuation_cursor_from_the_last_returned_call() {
        let evidence = call_evidence_fixture();
        let limit = UsageCallPageLimit::new(1).expect("fixture page limit fits");
        let page = UsageCallPage::new(
            vec![evidence.clone()],
            UsageCallPageContinuation::HasMore,
            limit,
        )
        .expect("fixture page is within its limit and anchors its cursor");

        assert_eq!(
            page.next(),
            Some(UsageCallCursor {
                recorded_at: evidence.recorded_at,
                call: evidence.call,
            })
        );
    }

    #[test]
    fn aggregate_group_rejects_a_safety_claim_with_an_unrepresentable_cache_total() {
        let mut key = aggregate_key_fixture();
        key.input_semantics = UsageInputTokenSemantics::CacheInclusive;
        key.coverage = UsageTokenCoverage {
            input: UsageTokenPresence::Present,
            output: UsageTokenPresence::Absent,
            cache_creation_input: UsageTokenPresence::Present,
            cache_read_input: UsageTokenPresence::Present,
        };

        assert_eq!(
            UsageAggregateGroup::new(
                key,
                1,
                UsageAggregateTokenAxes {
                    input: Some(0),
                    output: None,
                    cache_creation_input: Some(u128::MAX),
                    cache_read_input: Some(1),
                },
                UsageCacheNormalization::Safe,
            ),
            Err(UsageAggregateGroupError::NormalizationClaim {
                claimed: UsageCacheNormalization::Safe,
                input_semantics: UsageInputTokenSemantics::CacheInclusive,
            })
        );
    }

    #[test]
    fn call_page_rejects_calls_out_of_newest_first_order() {
        let older = call_evidence_fixture();
        let mut newer = call_evidence_fixture();
        newer.call = ModelCallId::from_uuid(uuid::Uuid::from_u128(0xD7));
        newer.recorded_at =
            UsageTimestampMicros::new(older.recorded_at.get() + 1).expect("fixture timestamp fits");
        let limit = UsageCallPageLimit::new(2).expect("fixture page limit fits");

        assert_eq!(
            UsageCallPage::new(
                vec![older, newer],
                UsageCallPageContinuation::Exhausted,
                limit,
            ),
            Err(UsageCallPageError::Misordered { position: 1 })
        );
    }

    #[test]
    fn call_page_rejects_a_continuation_claim_without_a_last_call() {
        let limit = UsageCallPageLimit::new(1).expect("fixture page limit fits");

        assert_eq!(
            UsageCallPage::new(Vec::new(), UsageCallPageContinuation::HasMore, limit),
            Err(UsageCallPageError::DanglingContinuation)
        );
    }

    #[test]
    fn call_page_rejects_more_calls_than_the_requested_limit() {
        let evidence = UsageCallEvidence {
            scope: UsageCallScope::ContextCompaction,
            call: ModelCallId::from_uuid(uuid::Uuid::from_u128(0xD4)),
            session: SessionId::from_uuid(uuid::Uuid::from_u128(0xD5)),
            model: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                uuid::Uuid::from_u128(0xD6),
            )),
            credential_profile: UsageCredentialProfileLabel::new("exact:profile-one".to_owned())
                .expect("fixture label is bounded and discriminated"),
            provenance: UsageProvenance::Reported,
            input_semantics: UsageInputTokenSemantics::CacheInclusive,
            tokens: UsageTokenAxes {
                input: Some(17),
                output: Some(5),
                cache_creation_input: Some(0),
                cache_read_input: Some(0),
            },
            recorded_at: UsageTimestampMicros::new(1_777_777_777_000_000)
                .expect("fixture timestamp fits"),
        };
        let limit = UsageCallPageLimit::new(1).expect("fixture page limit fits");

        assert_eq!(
            UsageCallPage::new(
                vec![evidence.clone(), evidence],
                UsageCallPageContinuation::Exhausted,
                limit,
            ),
            Err(UsageCallPageError::Overflow {
                returned_calls: 2,
                limit_items: 1,
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

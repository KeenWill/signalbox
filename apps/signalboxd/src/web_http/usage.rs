use super::{
    BillingKind, Deserialize, HubModelConfiguration, IntoResponse, Json, ModelCallId,
    ModelCallInputUsage, ProcessModelCallInputTokenSemantics, ProviderModelIdentity, Query,
    QueryRejection, ResolvedProviderTarget, Response, SessionId, State, StatusCode, TurnId,
    UsageAggregateCompleteness, UsageAggregateGroup, UsageAggregateTokenAxes,
    UsageCacheNormalization, UsageCallCursor, UsageCallEvidence, UsageCallKind, UsageCallOrder,
    UsageCallPageLimit, UsageCallQuery, UsageInputTokenSemantics, UsageProvenance, UsageQuery,
    UsageRepositoryError, UsageSelection, UsageTimeFromInclusive, UsageTimeRange,
    UsageTimeToExclusive, UsageTimestampMicros, UsageTokenAxes, UsageTokenPresence, WebApiState,
    WebDollarAmount, WebNullableU64, WebNullableU128, WebSessionId, WebUsageAggregateGroup,
    WebUsageAggregateTokenAxes, WebUsageCall, WebUsageCallCount, WebUsageCallCursor,
    WebUsageCallKind, WebUsageCallPage, WebUsageCost, WebUsageCostLabel,
    WebUsageCostUnavailableReason, WebUsageInputSemantics, WebUsageProvenance, WebUsageRateVersion,
    WebUsageSummary, WebUsageTimestampMicros, WebUsageTokenAxes, WebUsageTokenCoverage,
    application_error, web_uuid,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UsageSummaryHttpQuery {
    from_micros: Option<String>,
    to_micros: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    model_id: Option<String>,
    provenance: Option<String>,
    call_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UsageCallsHttpQuery {
    from_micros: Option<String>,
    to_micros: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    model_id: Option<String>,
    provenance: Option<String>,
    call_kind: Option<String>,
    order: String,
    max_items: String,
    after_recorded_at_micros: Option<String>,
    after_call_id: Option<String>,
}

pub(super) async fn usage_summary(
    State(state): State<WebApiState>,
    query: Result<Query<UsageSummaryHttpQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_usage_query(),
    };
    let Some(query) = parse_usage_query(
        query.from_micros,
        query.to_micros,
        query.session_id,
        query.turn_id,
        query.model_id,
        query.provenance,
        query.call_kind,
    ) else {
        return invalid_usage_query();
    };
    let (Some(repository), Some(configuration)) = (state.usage, state.model_configuration) else {
        return usage_unavailable();
    };
    // The aggregate read holds one pooled connection for its grouped scan, so
    // it is a snapshot reader on the same footing as the attention snapshot
    // and the lexical page: it draws its permit from the daemon-wide budget
    // that reserves pool connections for mutations and outbox work. Admitting
    // it after the query and repository checks keeps a malformed or
    // unconfigured request from spending a permit.
    let Some(budget) = state.snapshot_reader_budget else {
        return usage_projection_failed();
    };
    let Ok(_permit) = budget.acquire().await else {
        return usage_projection_failed();
    };
    match repository.aggregate(query).await {
        Ok(report) => Json(WebUsageSummary {
            groups: report
                .groups()
                .iter()
                .map(|group| usage_aggregate_dto(group, &configuration))
                .collect(),
            truncated: report.completeness() == UsageAggregateCompleteness::Truncated,
        })
        .into_response(),
        Err(error) => usage_repository_error(error),
    }
}

pub(super) async fn usage_calls(
    State(state): State<WebApiState>,
    query: Result<Query<UsageCallsHttpQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_usage_query(),
    };
    let scope = parse_usage_query(
        query.from_micros,
        query.to_micros,
        query.session_id,
        query.turn_id,
        query.model_id,
        query.provenance,
        query.call_kind,
    );
    let order = match query.order.as_str() {
        "newest" => Some(UsageCallOrder::NewestFirst),
        _ => None,
    };
    let limit = query
        .max_items
        .parse::<u16>()
        .ok()
        .and_then(|value| UsageCallPageLimit::new(value).ok());
    let after = match (query.after_recorded_at_micros, query.after_call_id) {
        (None, None) => Some(None),
        (Some(recorded_at), Some(call)) => parse_usage_timestamp(&recorded_at)
            .zip(parse_model_call_id(&call))
            .map(|(recorded_at, call)| Some(UsageCallCursor { recorded_at, call })),
        _ => None,
    };
    let (Some(scope), Some(order), Some(limit), Some(after)) = (scope, order, limit, after) else {
        return invalid_usage_query();
    };
    let (Some(repository), Some(configuration)) = (state.usage, state.model_configuration) else {
        return usage_unavailable();
    };
    // Same footing as the aggregate read above: one pooled connection for the
    // keyset page, drawn from the shared snapshot-reader budget after the
    // request and repository checks.
    let Some(budget) = state.snapshot_reader_budget else {
        return usage_projection_failed();
    };
    let Ok(_permit) = budget.acquire().await else {
        return usage_projection_failed();
    };
    match repository
        .calls(UsageCallQuery {
            scope,
            order,
            limit,
            after,
        })
        .await
    {
        Ok(page) => Json(WebUsageCallPage {
            calls: page
                .calls()
                .iter()
                .map(|call| usage_call_dto(call, &configuration))
                .collect(),
            continuation: page.next().map(|cursor| WebUsageCallCursor {
                recorded_at_micros: WebUsageTimestampMicros::from_application(
                    cursor.recorded_at.get(),
                ),
                call_id: web_uuid(cursor.call.into_uuid()),
            }),
        })
        .into_response(),
        Err(error) => usage_repository_error(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_usage_query(
    from_micros: Option<String>,
    to_micros: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    model_id: Option<String>,
    provenance: Option<String>,
    call_kind: Option<String>,
) -> Option<UsageQuery> {
    let from_inclusive = parse_optional(from_micros, parse_usage_timestamp)?;
    let to_exclusive = parse_optional(to_micros, parse_usage_timestamp)?;
    let time = UsageTimeRange::new(
        from_inclusive.map(UsageTimeFromInclusive),
        to_exclusive.map(UsageTimeToExclusive),
    )
    .ok()?;
    let selection = UsageSelection {
        session: parse_optional(session_id, |value| {
            uuid::Uuid::parse_str(value).ok().map(SessionId::from_uuid)
        })?,
        turn: parse_optional(turn_id, |value| {
            uuid::Uuid::parse_str(value).ok().map(TurnId::from_uuid)
        })?,
        model: parse_optional(model_id, |value| {
            uuid::Uuid::parse_str(value).ok().map(|identity| {
                ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(identity))
            })
        })?,
        provenance: parse_optional(provenance, parse_usage_provenance)?,
        call_kind: parse_optional(call_kind, parse_usage_call_kind)?,
    };
    Some(UsageQuery { time, selection })
}

fn parse_optional<T>(
    value: Option<String>,
    parser: impl FnOnce(&str) -> Option<T>,
) -> Option<Option<T>> {
    match value {
        None => Some(None),
        Some(value) => parser(&value).map(Some),
    }
}

fn parse_usage_timestamp(value: &str) -> Option<UsageTimestampMicros> {
    UsageTimestampMicros::new(value.parse().ok()?).ok()
}

fn parse_model_call_id(value: &str) -> Option<ModelCallId> {
    uuid::Uuid::parse_str(value)
        .ok()
        .map(ModelCallId::from_uuid)
}

fn parse_usage_provenance(value: &str) -> Option<UsageProvenance> {
    match value {
        "reported" => Some(UsageProvenance::Reported),
        "estimated" => Some(UsageProvenance::Estimated),
        _ => None,
    }
}

fn parse_usage_call_kind(value: &str) -> Option<UsageCallKind> {
    match value {
        "model_call" => Some(UsageCallKind::ModelCall),
        "approval_judge" => Some(UsageCallKind::ApprovalJudge),
        "context_compaction" => Some(UsageCallKind::ContextCompaction),
        _ => None,
    }
}

fn invalid_usage_query() -> Response {
    application_error(
        StatusCode::BAD_REQUEST,
        "invalid_usage_query",
        "usage parameters are malformed or outside the contract bounds",
    )
}

fn usage_unavailable() -> Response {
    application_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "usage_projection_unavailable",
        "usage projection or configured rates are not available",
    )
}

fn usage_projection_failed() -> Response {
    application_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "usage_projection_failed",
        "the durable usage projection could not be read",
    )
}

fn usage_repository_error(error: UsageRepositoryError) -> Response {
    let failure_class = match &error {
        UsageRepositoryError::Database(_) => "infrastructure",
        UsageRepositoryError::Corruption(_) => "fail_closed_corruption",
    };
    tracing::error!(failure_class, cause = %error, "usage projection read failed");
    usage_projection_failed()
}

fn usage_aggregate_dto(
    group: &UsageAggregateGroup,
    configuration: &HubModelConfiguration,
) -> WebUsageAggregateGroup {
    WebUsageAggregateGroup {
        call_kind: usage_call_kind_dto(group.key().call_kind),
        model_id: web_uuid(group.key().model.identity().into_uuid()),
        profile_id: signalbox_web_contract::WebUsageProfileId::from_bounded(
            group.key().credential_profile.as_str().to_owned(),
        ),
        provenance: usage_provenance_dto(group.key().provenance),
        input_semantics: usage_input_semantics_dto(group.key().input_semantics),
        coverage: WebUsageTokenCoverage {
            input: group.key().coverage.input == UsageTokenPresence::Present,
            output: group.key().coverage.output == UsageTokenPresence::Present,
            cache_creation_input: group.key().coverage.cache_creation_input
                == UsageTokenPresence::Present,
            cache_read_input: group.key().coverage.cache_read_input == UsageTokenPresence::Present,
        },
        call_count: WebUsageCallCount::from_positive(group.call_count()),
        tokens: usage_aggregate_tokens_dto(group.tokens()),
        cost: usage_aggregate_cost_dto(configuration, group),
    }
}

fn usage_call_dto(call: &UsageCallEvidence, configuration: &HubModelConfiguration) -> WebUsageCall {
    WebUsageCall {
        call_kind: usage_call_kind_dto(call.scope.call_kind()),
        call_id: web_uuid(call.call.into_uuid()),
        session_id: WebSessionId::from_uuid_bytes(*call.session.into_uuid().as_bytes()),
        turn_id: call.scope.turn().map(|turn| web_uuid(turn.into_uuid())),
        model_id: web_uuid(call.model.identity().into_uuid()),
        profile_id: signalbox_web_contract::WebUsageProfileId::from_bounded(
            call.credential_profile.as_str().to_owned(),
        ),
        provenance: usage_provenance_dto(call.provenance),
        input_semantics: usage_input_semantics_dto(call.input_semantics),
        tokens: usage_tokens_dto(call.tokens),
        recorded_at_micros: WebUsageTimestampMicros::from_application(call.recorded_at.get()),
        cost: usage_cost_dto(
            configuration,
            call.model,
            call.credential_reference.as_deref(),
            call.input_semantics,
            call.tokens,
            true,
        ),
    }
}

pub(super) fn usage_cost_dto(
    configuration: &HubModelConfiguration,
    model: ResolvedProviderTarget,
    credential_profile: Option<&str>,
    input_semantics: UsageInputTokenSemantics,
    tokens: UsageTokenAxes,
    cost_derivation_safe: bool,
) -> WebUsageCost {
    let unavailable = |reason| WebUsageCost::Unavailable { reason };
    if tokens.coverage()
        == (signalbox_application::UsageTokenCoverage {
            input: UsageTokenPresence::Absent,
            output: UsageTokenPresence::Absent,
            cache_creation_input: UsageTokenPresence::Absent,
            cache_read_input: UsageTokenPresence::Absent,
        })
    {
        return unavailable(WebUsageCostUnavailableReason::NoTokenEvidence);
    }
    let semantics = match input_semantics {
        UsageInputTokenSemantics::Unknown => {
            return unavailable(WebUsageCostUnavailableReason::UnknownInputSemantics);
        }
        UsageInputTokenSemantics::CacheExclusive => {
            ProcessModelCallInputTokenSemantics::CacheExclusive
        }
        UsageInputTokenSemantics::CacheInclusive => {
            if tokens.output.is_none()
                && tokens.cache_creation_input.is_none()
                && tokens.cache_read_input.is_none()
            {
                return unavailable(WebUsageCostUnavailableReason::IncompleteCacheAxes);
            }
            if tokens.input.is_some_and(|input| {
                tokens
                    .cache_creation_input
                    .zip(tokens.cache_read_input)
                    .is_some_and(|(creation, read)| {
                        creation.checked_add(read).is_none_or(|cache| input < cache)
                    })
            }) {
                return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
            }
            ProcessModelCallInputTokenSemantics::CacheInclusive
        }
    };
    if !cost_derivation_safe {
        return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
    }
    let Some(credential_profile) = credential_profile else {
        return unavailable(WebUsageCostUnavailableReason::ConfigurationUnavailable);
    };
    let Some(cost) = configuration.derive_model_call_cost(
        model,
        credential_profile,
        ModelCallInputUsage::from_persisted(tokens.input, Some(semantics)),
        tokens.output,
        tokens.cache_creation_input,
        tokens.cache_read_input,
    ) else {
        return unavailable(WebUsageCostUnavailableReason::ConfigurationUnavailable);
    };
    WebUsageCost::Derived {
        amount_usd: WebDollarAmount::from_derived(cost.amount_usd().normalize().to_string()),
        rate_version: WebUsageRateVersion::from_configured(cost.rate_version().to_owned()),
        label: match cost.billing_kind() {
            BillingKind::ApiMetered => WebUsageCostLabel::Real,
            BillingKind::Subscription => WebUsageCostLabel::MeteredEquivalent,
        },
    }
}

pub(super) fn usage_aggregate_cost_dto(
    configuration: &HubModelConfiguration,
    group: &UsageAggregateGroup,
) -> WebUsageCost {
    let unavailable = |reason| WebUsageCost::Unavailable { reason };
    let tokens = group.tokens();
    if tokens.input.is_none()
        && tokens.output.is_none()
        && tokens.cache_creation_input.is_none()
        && tokens.cache_read_input.is_none()
    {
        return unavailable(WebUsageCostUnavailableReason::NoTokenEvidence);
    }
    let semantics = match group.key().input_semantics {
        UsageInputTokenSemantics::Unknown => {
            return unavailable(WebUsageCostUnavailableReason::UnknownInputSemantics);
        }
        UsageInputTokenSemantics::CacheExclusive => {
            ProcessModelCallInputTokenSemantics::CacheExclusive
        }
        UsageInputTokenSemantics::CacheInclusive => {
            if tokens.output.is_none()
                && tokens.cache_creation_input.is_none()
                && tokens.cache_read_input.is_none()
            {
                return unavailable(WebUsageCostUnavailableReason::IncompleteCacheAxes);
            }
            if tokens
                .cache_creation_input
                .zip(tokens.cache_read_input)
                .is_some_and(|(creation, read)| {
                    creation
                        .checked_add(read)
                        .is_none_or(|cache| tokens.input.is_some_and(|input| input < cache))
                })
            {
                return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
            }
            ProcessModelCallInputTokenSemantics::CacheInclusive
        }
    };
    // `Unsafe` conflates two distinct states: a constituent call whose cache
    // breakdown contradicts its input total, and a group that never reported
    // the cache axes normalization would need. Only the first contradicts the
    // evidence. When the group's coverage lacks an axis, normalization is
    // merely incomplete, and the independently reported axes stay priceable
    // exactly as they do on the individual-call path.
    if group.key().input_semantics == UsageInputTokenSemantics::CacheInclusive
        && group.cache_normalization() == UsageCacheNormalization::Unsafe
        && tokens.input.is_some()
        && tokens.cache_creation_input.is_some()
        && tokens.cache_read_input.is_some()
    {
        return unavailable(WebUsageCostUnavailableReason::InvalidCacheBreakdown);
    }
    let Some(credential_reference) = group.key().credential_reference.as_deref() else {
        return unavailable(WebUsageCostUnavailableReason::ConfigurationUnavailable);
    };
    let Some(cost) = configuration.derive_usage_aggregate_cost(
        group.key().model,
        credential_reference,
        semantics,
        [
            tokens.input,
            tokens.output,
            tokens.cache_creation_input,
            tokens.cache_read_input,
        ],
    ) else {
        return unavailable(WebUsageCostUnavailableReason::ConfigurationUnavailable);
    };
    WebUsageCost::Derived {
        amount_usd: WebDollarAmount::from_derived(cost.amount_usd().normalize().to_string()),
        rate_version: WebUsageRateVersion::from_configured(cost.rate_version().to_owned()),
        label: match cost.billing_kind() {
            BillingKind::ApiMetered => WebUsageCostLabel::Real,
            BillingKind::Subscription => WebUsageCostLabel::MeteredEquivalent,
        },
    }
}

const fn usage_call_kind_dto(kind: UsageCallKind) -> WebUsageCallKind {
    match kind {
        UsageCallKind::ModelCall => WebUsageCallKind::ModelCall,
        UsageCallKind::ApprovalJudge => WebUsageCallKind::ApprovalJudge,
        UsageCallKind::ContextCompaction => WebUsageCallKind::ContextCompaction,
    }
}

const fn usage_provenance_dto(provenance: UsageProvenance) -> WebUsageProvenance {
    match provenance {
        UsageProvenance::Reported => WebUsageProvenance::Reported,
        UsageProvenance::Estimated => WebUsageProvenance::Estimated,
    }
}

const fn usage_input_semantics_dto(semantics: UsageInputTokenSemantics) -> WebUsageInputSemantics {
    match semantics {
        UsageInputTokenSemantics::Unknown => WebUsageInputSemantics::Unknown,
        UsageInputTokenSemantics::CacheExclusive => WebUsageInputSemantics::CacheExclusive,
        UsageInputTokenSemantics::CacheInclusive => WebUsageInputSemantics::CacheInclusive,
    }
}

fn usage_tokens_dto(tokens: UsageTokenAxes) -> WebUsageTokenAxes {
    WebUsageTokenAxes {
        input: WebNullableU64::from_option(tokens.input),
        output: WebNullableU64::from_option(tokens.output),
        cache_creation_input: WebNullableU64::from_option(tokens.cache_creation_input),
        cache_read_input: WebNullableU64::from_option(tokens.cache_read_input),
    }
}

fn usage_aggregate_tokens_dto(tokens: UsageAggregateTokenAxes) -> WebUsageAggregateTokenAxes {
    WebUsageAggregateTokenAxes {
        input: WebNullableU128::from_option(tokens.input),
        output: WebNullableU128::from_option(tokens.output),
        cache_creation_input: WebNullableU128::from_option(tokens.cache_creation_input),
        cache_read_input: WebNullableU128::from_option(tokens.cache_read_input),
    }
}

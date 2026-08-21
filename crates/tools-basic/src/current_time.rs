//! The first compiled hub-local tool: deterministic current-time lookup.

use std::{error::Error, fmt, future::Future, time::SystemTime};

use jiff::{
    Timestamp,
    fmt::strtime,
    tz::{self, TimeZone},
};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};

use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};
use signalbox_tool_schema_derive::ToolSchema;

pub const CURRENT_TIME_NAME: &str = "current_time";
const INVALID_ARGUMENTS_DETAIL: &str = "expected an object with one optional IANA timezone string";
const CLOCK_OUT_OF_RANGE_DETAIL: &str = "current time is outside the supported range";
const OFFSET_NOT_RFC3339_DETAIL: &str =
    "selected time zone offset cannot be represented as RFC 3339";
const RFC_3339_SECONDS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%:z";

/// Injected source of the instant observed by `current_time`.
pub trait CurrentTimeClock: Send + Sync {
    /// Returns one wall-clock instant without applying a time zone.
    fn now(&self) -> SystemTime;
}

impl<Clock> CurrentTimeClock for Clock
where
    Clock: Fn() -> SystemTime + Send + Sync,
{
    fn now(&self) -> SystemTime {
        self()
    }
}

/// Production wall-clock source.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCurrentTimeClock;

impl CurrentTimeClock for SystemCurrentTimeClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A static `current_time` declaration could not be compiled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CurrentTimeToolConstructionError {
    /// The static tool name was rejected.
    Name,
    /// The static JSON Schema was rejected.
    Schema,
    /// A static sanitized error detail was rejected.
    ErrorDetail,
    /// The one-entry catalog unexpectedly reported a duplicate.
    Duplicate,
}

impl fmt::Display for CurrentTimeToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name => formatter.write_str("current_time static name is invalid"),
            Self::Schema => formatter.write_str("current_time static schema is invalid"),
            Self::ErrorDetail => formatter.write_str("current_time static error detail is invalid"),
            Self::Duplicate => formatter.write_str("current_time catalog is duplicated"),
        }
    }
}

impl Error for CurrentTimeToolConstructionError {}

/// Compiled catalog entry and matching executor for `current_time`.
///
/// Effect posture: `EffectFree`. Execution observes the injected clock and
/// time-zone database only and performs no externally visible write.
#[derive(Clone, Debug)]
pub struct CurrentTimeTool<Clock> {
    catalog: CompiledToolCatalog,
    executor: CurrentTimeExecutor<Clock>,
}

impl<Clock> ToolContract for CurrentTimeTool<Clock> {
    type Arguments = CurrentTimeArguments;
    const NAME: &'static str = CURRENT_TIME_NAME;
    const DESCRIPTION: &'static str =
        "Returns the current time in UTC or an optional IANA time zone.";
}

impl<Clock> CurrentTimeTool<Clock> {
    /// Compiles immutable metadata and validation around one injected clock.
    pub fn try_new(clock: Clock) -> Result<Self, CurrentTimeToolConstructionError> {
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| CurrentTimeToolConstructionError::ErrorDetail)?;
        let clock_out_of_range_detail =
            ToolExecutionErrorDetail::try_new(String::from(CLOCK_OUT_OF_RANGE_DETAIL))
                .map_err(|_| CurrentTimeToolConstructionError::ErrorDetail)?;
        let offset_not_rfc3339_detail =
            ToolExecutionErrorDetail::try_new(String::from(OFFSET_NOT_RFC3339_DETAIL))
                .map_err(|_| CurrentTimeToolConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<Self>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => CurrentTimeToolConstructionError::Name,
            ToolContractCompileError::Schema => CurrentTimeToolConstructionError::Schema,
        })?;
        let compiled = CompiledTool::new(
            definition,
            CurrentTimeArgumentValidator {
                detail: invalid_arguments_detail,
            },
        );
        let catalog = CompiledToolCatalog::try_new([compiled])
            .map_err(|_| CurrentTimeToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: CurrentTimeExecutor {
                clock,
                clock_out_of_range_detail,
                offset_not_rfc3339_detail,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, CurrentTimeExecutor<Clock>) {
        (self.catalog, self.executor)
    }
}

#[derive(Clone, Debug)]
struct CurrentTimeArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for CurrentTimeArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        resolve_arguments(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// Daemon-local executor backed by an injected clock.
///
/// Effect posture: `EffectFree`. Execution observes the injected clock and
/// time-zone database only and performs no externally visible write.
#[derive(Clone, Debug)]
pub struct CurrentTimeExecutor<Clock> {
    clock: Clock,
    clock_out_of_range_detail: ToolExecutionErrorDetail,
    offset_not_rfc3339_detail: ToolExecutionErrorDetail,
}

/// A checked catalog/executor assumption failed inside `current_time`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CurrentTimeExecutorError {
    /// The executor could not reproduce catalog argument validation.
    ArgumentValidationDrift,
    /// The static RFC 3339 formatting operation failed.
    TimestampFormatting,
    /// Compact JSON result encoding unexpectedly failed.
    ResultEncoding,
}

impl fmt::Display for CurrentTimeExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArgumentValidationDrift => {
                formatter.write_str("current_time argument validation drifted")
            }
            Self::TimestampFormatting => {
                formatter.write_str("current_time timestamp formatting failed")
            }
            Self::ResultEncoding => formatter.write_str("current_time result encoding failed"),
        }
    }
}

impl Error for CurrentTimeExecutorError {}

impl ClassifyOperatorFailure for CurrentTimeExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl<Clock> ToolExecutor for CurrentTimeExecutor<Clock>
where
    Clock: CurrentTimeClock,
{
    type Error = CurrentTimeExecutorError;

    fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> impl Future<Output = Result<CorrelatedToolExecutorEvidence, Self::Error>> + Send {
        let evidence = current_time_evidence(
            self.clock.now(),
            invocation.request().arguments(),
            &self.clock_out_of_range_detail,
            &self.offset_not_rfc3339_detail,
        );
        async move { evidence.map(|evidence| invocation.bind(evidence)) }
    }
}

/// Typed `current_time` argument shape; decoder and rendered schema share it.
#[derive(Debug, serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
pub struct CurrentTimeArguments {
    #[serde(default, deserialize_with = "present_time_zone_only")]
    #[tool_schema(
        description = "Optional IANA time-zone name; defaults to UTC.",
        with = String
    )]
    timezone: Option<IanaTimeZone>,
}

/// Decodes a present `timezone` member, rejecting explicit `null`: the
/// declared schema types the member as a plain string, and omitting the
/// member is the one way to select the UTC default.
fn present_time_zone_only<'de, D>(deserializer: D) -> Result<Option<IanaTimeZone>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// One recognized IANA zone or link identifier resolved against the installed
/// time-zone database.
///
/// Admission is the contract the executor relies on: auxiliary TZif paths
/// (`localtime`, `posixrules`, `posix/…`, `right/…`) and the unknown-zone
/// sentinel are not IANA identifiers and never construct this value.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(try_from = "String")]
struct IanaTimeZone {
    name: String,
    time_zone: TimeZone,
}

/// A supplied time-zone value is not one recognized IANA identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnrecognizedIanaTimeZone;

impl fmt::Display for UnrecognizedIanaTimeZone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("not a recognized IANA time-zone name")
    }
}

impl TryFrom<String> for IanaTimeZone {
    type Error = UnrecognizedIanaTimeZone;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let database = tz::db();
        if matches!(name.as_str(), "localtime" | "posixrules")
            || !database
                .available()
                .any(|available| available.as_str() == name)
        {
            return Err(UnrecognizedIanaTimeZone);
        }
        let time_zone = database.get(&name).map_err(|_| UnrecognizedIanaTimeZone)?;
        if time_zone.is_unknown() {
            return Err(UnrecognizedIanaTimeZone);
        }
        Ok(Self { name, time_zone })
    }
}

struct ResolvedArguments {
    time_zone: TimeZone,
    selected_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidCurrentTimeArguments;

fn resolve_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<ResolvedArguments, InvalidCurrentTimeArguments> {
    let decoded: CurrentTimeArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidCurrentTimeArguments)?;
    Ok(match decoded.timezone {
        Some(zone) => ResolvedArguments {
            time_zone: zone.time_zone,
            selected_name: zone.name,
        },
        None => ResolvedArguments {
            time_zone: TimeZone::UTC,
            selected_name: String::from("UTC"),
        },
    })
}

fn current_time_evidence(
    now: SystemTime,
    arguments: &NormalizedToolArguments,
    clock_out_of_range_detail: &ToolExecutionErrorDetail,
    offset_not_rfc3339_detail: &ToolExecutionErrorDetail,
) -> Result<ToolExecutorEvidence, CurrentTimeExecutorError> {
    let resolved = resolve_arguments(arguments)
        .map_err(|_| CurrentTimeExecutorError::ArgumentValidationDrift)?;
    let timestamp = match Timestamp::try_from(now) {
        Ok(timestamp) => timestamp,
        Err(_error) => {
            return Ok(ToolExecutorEvidence::KnownFailed {
                detail: Some(clock_out_of_range_detail.clone()),
            });
        }
    };
    let zoned = timestamp.to_zoned(resolved.time_zone);
    if !(0..=9999).contains(&zoned.year()) {
        return Ok(ToolExecutorEvidence::KnownFailed {
            detail: Some(clock_out_of_range_detail.clone()),
        });
    }
    if zoned.offset().seconds() % 60 != 0 {
        return Ok(ToolExecutorEvidence::KnownFailed {
            detail: Some(offset_not_rfc3339_detail.clone()),
        });
    }
    let datetime = strtime::format(RFC_3339_SECONDS_FORMAT, &zoned)
        .map_err(|_| CurrentTimeExecutorError::TimestampFormatting)?;
    let result = serde_json::to_string(&serde_json::json!({
        "datetime": datetime,
        "timezone": resolved.selected_name,
    }))
    .map_err(|_| CurrentTimeExecutorError::ResultEncoding)?;
    Ok(ToolExecutorEvidence::CompletedText(result))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use signalbox_application::{
        CorrelatedDurableChildWait, InProcessToolDispatchGate, PrepareToolContinuationOutcome,
        RetainedToolAttemptObservationStatus, ToolAttemptAuthorizationStatus, ToolCatalog,
        ToolCatalogValidationFailure, ToolContinuationIdentities, ToolCrashClosureIdentities,
        ToolDefinition, ToolExecutionService, ToolExecutionServiceOutcome,
        ToolExecutionTransaction, ToolInputSchema, UuidV7ToolLoopIdGenerator,
    };
    use signalbox_domain::{
        AcceptedInputId, ContextFrontierId, CorrelatedToolAttemptObservation, CurrentToolAttempt,
        EndedToolAttempt, ModelCallId, NormalizedToolArguments,
        ResolvedContextFrontierReconstitutionInput, SemanticTranscriptEntryId, SessionId,
        ToolApprovalResolutionReconstitutionInput, ToolAttemptCrashOutcome,
        ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState,
        ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolDispatchGeneration,
        ToolExecutionError, ToolName, ToolRequestOrdinal, ToolRequestReconstitutionInput,
        TurnAttemptId, TurnId,
    };
    use uuid::Uuid;

    const SHIPPED_CURRENT_TIME_SCHEMA: &str = r#"{
        "type": "object",
        "properties": {
            "timezone": {
                "type": "string",
                "description": "Optional IANA time-zone name; defaults to UTC."
            }
        },
        "additionalProperties": false
    }"#;

    fn shipped_current_time_schema() -> ToolInputSchema {
        ToolInputSchema::try_new(String::from(SHIPPED_CURRENT_TIME_SCHEMA))
            .expect("shipped current_time schema remains admitted")
    }

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn clock_failure_detail() -> ToolExecutionErrorDetail {
        ToolExecutionErrorDetail::try_new(String::from(CLOCK_OUT_OF_RANGE_DETAIL))
            .expect("static fixture detail is valid")
    }

    fn offset_failure_detail() -> ToolExecutionErrorDetail {
        ToolExecutionErrorDetail::try_new(String::from(OFFSET_NOT_RFC3339_DETAIL))
            .expect("static fixture detail is valid")
    }

    #[track_caller]
    fn assert_auxiliary_zoneinfo_path_rejected(
        catalog: &CompiledToolCatalog,
        definition: &ToolDefinition,
        timezone: &str,
    ) {
        assert!(
            matches!(
                catalog.validate_arguments(
                    definition.name(),
                    &arguments(&format!(r#"{{"timezone":"{timezone}"}}"#))
                ),
                Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
            ),
            "auxiliary zoneinfo path {timezone} must not enter the IANA contract"
        );
    }

    fn prepared_current_time_batch(arguments: &str) -> signalbox_domain::ToolBatch {
        let session = SessionId::from_uuid(Uuid::from_u128(1));
        let turn = TurnId::from_uuid(Uuid::from_u128(2));
        let producing_call = ModelCallId::from_uuid(Uuid::from_u128(3));
        let request = ToolRequestReconstitutionInput::new(
            signalbox_domain::ToolRequestId::from_uuid(Uuid::from_u128(4)),
            session,
            turn,
            producing_call,
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from(CURRENT_TIME_NAME)).expect("fixture name is valid"),
            self::arguments(arguments),
        )
        .into_request();
        let turn_attempt = TurnAttemptId::from_uuid(Uuid::from_u128(5));
        let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
            .reconstitute()
            .expect("policy approval fixture is valid");
        let attempt = ToolAttemptReconstitutionInput::new(
            signalbox_domain::ToolAttemptId::from_uuid(Uuid::from_u128(6)),
            request.id(),
            session,
            turn,
            turn_attempt,
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Prepared,
        )
        .reconstitute()
        .expect("prepared attempt fixture is valid");
        let frontier = ResolvedContextFrontierReconstitutionInput::new(
            session,
            ContextFrontierId::from_uuid(Uuid::from_u128(7)),
            Vec::new(),
        )
        .reconstitute()
        .expect("empty frontier fixture is valid");

        ToolBatchReconstitutionInput::new(
            session,
            turn,
            producing_call,
            frontier,
            vec![request],
            vec![approval],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::Executing { turn_attempt },
        )
        .reconstitute()
        .expect("current_time batch fixture is valid")
    }

    struct ExecutorFixtureTransaction {
        batch: signalbox_domain::ToolBatch,
    }

    impl ToolExecutionTransaction for ExecutorFixtureTransaction {
        type Error = CurrentTimeExecutorError;

        async fn resume_child_wait(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: TurnAttemptId,
        ) -> Result<bool, Self::Error> {
            panic!("current-time fixtures never contain a delegated child wait")
        }

        async fn reread_durable_child_wait(
            &mut self,
            _wait: CorrelatedDurableChildWait,
        ) -> Result<bool, Self::Error> {
            panic!("current-time fixtures never park on a delegated child wait")
        }

        async fn reread_durable_completion(
            &mut self,
            _correlation: signalbox_domain::ToolAttemptDispatchCorrelation,
        ) -> Result<bool, Self::Error> {
            panic!("current-time fixtures never report a durable completion")
        }

        async fn load_active_batch(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
        ) -> Result<Option<signalbox_domain::ToolBatch>, Self::Error> {
            Ok(Some(self.batch.clone()))
        }

        async fn prepare_next_attempt(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: signalbox_domain::ToolAttemptId,
            _effect_class: ToolEffectClass,
        ) -> Result<Option<CurrentToolAttempt>, Self::Error> {
            panic!("fixture begins with one prepared attempt")
        }

        async fn authorize_attempt(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            attempt: signalbox_domain::ToolAttemptId,
            _preauthorization: signalbox_application::ToolPreauthorization,
        ) -> Result<signalbox_application::ToolAttemptAuthorizationOutcome, Self::Error> {
            self.batch
                .authorize_dispatch(attempt)
                .map(Box::new)
                .map(signalbox_application::ToolAttemptAuthorizationOutcome::Authorized)
                .map_err(|_| CurrentTimeExecutorError::ArgumentValidationDrift)
        }

        async fn reread_ambiguous_authorization(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: signalbox_domain::ToolAttemptId,
        ) -> Result<ToolAttemptAuthorizationStatus, Self::Error> {
            panic!("fixture authorization is unambiguous")
        }

        async fn commit_preflight_error(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: signalbox_domain::ToolAttemptId,
            _error: ToolExecutionError,
        ) -> Result<EndedToolAttempt, Self::Error> {
            panic!("fixture arguments pass preflight")
        }

        async fn commit_observation(
            &mut self,
            observation: CorrelatedToolAttemptObservation,
        ) -> Result<EndedToolAttempt, Self::Error> {
            self.batch
                .authorize_attempt(observation.correlation().attempt())
                .map_err(|_| CurrentTimeExecutorError::ArgumentValidationDrift)?
                .into_parts()
                .0
                .apply_terminal_observation(observation)
                .map_err(|_| CurrentTimeExecutorError::ArgumentValidationDrift)
        }

        async fn reread_observation(
            &mut self,
            _observation: &CorrelatedToolAttemptObservation,
        ) -> Result<RetainedToolAttemptObservationStatus, Self::Error> {
            Ok(RetainedToolAttemptObservationStatus::Pending)
        }

        async fn classify_crash_loss<NextTurn>(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _attempt: signalbox_domain::ToolAttemptId,
            _identities: ToolCrashClosureIdentities,
            _next_turn: NextTurn,
        ) -> Result<ToolAttemptCrashOutcome, Self::Error>
        where
            NextTurn: FnMut(AcceptedInputId) -> TurnId + Send,
        {
            panic!("fixture attempt is not a prior-process crash")
        }

        async fn prepare_continuation<NextSteering>(
            &mut self,
            _session: SessionId,
            _turn: TurnId,
            _producing_call: ModelCallId,
            _identities: ToolContinuationIdentities,
            _next_steering: NextSteering,
        ) -> Result<PrepareToolContinuationOutcome, Self::Error>
        where
            NextSteering: FnMut(AcceptedInputId) -> (SemanticTranscriptEntryId, TurnId) + Send,
        {
            panic!("fixture has one prepared attempt")
        }
    }

    /// S15: the first compiled declaration is exactly auto-approved and
    /// effect-free.
    #[test]
    fn s15_current_time_definition_carries_exact_policy() {
        let (catalog, _executor) = CurrentTimeTool::try_new(|| SystemTime::UNIX_EPOCH)
            .expect("static current_time tool compiles")
            .into_parts();
        let definitions = catalog.definitions();
        let [definition] = definitions.as_ref() else {
            panic!("current_time is the one compiled definition")
        };

        assert_eq!(definition.name().as_str(), CURRENT_TIME_NAME);
        assert_eq!(definition.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(definition.effect_class(), ToolEffectClass::EffectFree);
    }

    /// S15: the derived schema remains byte-identical to the canonical
    /// artifact produced from the hand-written schema that shipped before
    /// derivation.
    #[test]
    fn s15_current_time_derived_schema_is_byte_identical_to_shipped_schema() {
        let (catalog, _executor) = CurrentTimeTool::try_new(|| SystemTime::UNIX_EPOCH)
            .expect("static current_time tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];
        let shipped = shipped_current_time_schema();
        let schema: serde_json::Value = serde_json::from_str(definition.input_schema().as_str())
            .expect("registry schema is valid JSON");

        expect_test::expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "timezone": {
                  "description": "Optional IANA time-zone name; defaults to UTC.",
                  "type": "string"
                }
              },
              "type": "object"
            }"#]]
        .assert_eq(&format!("{schema:#}"));
        assert_eq!(definition.input_schema().as_str(), shipped.as_str());
        assert_eq!(
            <CurrentTimeArguments as signalbox_tool_contract::ToolSchema>::schema().to_string(),
            shipped.as_str()
        );
    }

    /// S15: an explicit `timezone: null` is not the omitted-member default;
    /// the declared plain-string schema and the decoder reject it together.
    #[test]
    fn s15_current_time_rejects_explicit_null_timezone() {
        let (catalog, _executor) = CurrentTimeTool::try_new(|| SystemTime::UNIX_EPOCH)
            .expect("static current_time tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(definition.name(), &arguments(r#"{"timezone":null}"#)),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// S15: the declaration schema accepts the empty object and rejects
    /// unexpected fields.
    #[test]
    fn s15_current_time_schema_rejects_unexpected_fields() {
        let (catalog, _executor) = CurrentTimeTool::try_new(|| SystemTime::UNIX_EPOCH)
            .expect("static current_time tool compiles")
            .into_parts();
        let definitions = catalog.definitions();
        let [definition] = definitions.as_ref() else {
            panic!("current_time is the one compiled definition")
        };

        assert_eq!(
            catalog.validate_arguments(definition.name(), &arguments("{}")),
            Ok(())
        );
        assert!(matches!(
            catalog.validate_arguments(definition.name(), &arguments(r#"{"unexpected":true}"#)),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// S15: executor dispatch observes its injected clock exactly once instead
    /// of consulting ambient wall-clock state.
    #[tokio::test]
    async fn s15_current_time_executor_uses_only_the_injected_clock() {
        let clock_reads = Arc::new(AtomicUsize::new(0));
        let observed_reads = Arc::clone(&clock_reads);
        let clock = move || {
            observed_reads.fetch_add(1, Ordering::SeqCst);
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1)
        };
        let (catalog, executor) = CurrentTimeTool::try_new(clock)
            .expect("static current_time tool compiles")
            .into_parts();
        let batch = prepared_current_time_batch(r#"{"timezone":"UTC"}"#);
        let mut service = ToolExecutionService::new(
            UuidV7ToolLoopIdGenerator,
            ExecutorFixtureTransaction {
                batch: batch.clone(),
            },
            catalog,
            executor,
            InProcessToolDispatchGate::default(),
        );

        let outcome = service
            .execute(batch.session(), batch.turn())
            .await
            .expect("executor evidence commits");
        let ToolExecutionServiceOutcome::ObservationCommitted(ended) = outcome else {
            panic!("prepared current_time dispatch must commit one observation")
        };

        assert_eq!(clock_reads.load(Ordering::SeqCst), 1);
        assert!(matches!(
            ended.end(),
            signalbox_domain::ToolAttemptEnd::Completed {
                result: signalbox_domain::ToolResultContent::Text(result)
            } if result.as_str()
                == r#"{"datetime":"1970-01-01T00:00:01+00:00","timezone":"UTC"}"#
        ));
    }

    /// S15: an omitted timezone resolves to the exact UTC default.
    #[test]
    fn s15_current_time_defaults_to_utc() {
        let resolved = resolve_arguments(&arguments("{}"))
            .expect("the empty object selects the default timezone");

        assert_eq!(resolved.selected_name, "UTC");
    }

    /// S15: successful output is the exact compact JSON contract.
    #[test]
    fn s15_current_time_result_encoding_is_exact_and_compact() {
        let evidence = current_time_evidence(
            SystemTime::UNIX_EPOCH,
            &arguments(r#"{"timezone":"UTC"}"#),
            &clock_failure_detail(),
            &offset_failure_detail(),
        )
        .expect("valid UTC execution returns evidence");

        assert_eq!(
            evidence,
            ToolExecutorEvidence::CompletedText(String::from(
                r#"{"datetime":"1970-01-01T00:00:00+00:00","timezone":"UTC"}"#
            ))
        );
    }

    /// S15: negative civil years cannot enter the RFC 3339 success shape.
    #[test]
    fn s15_current_time_rejects_negative_rfc3339_year() {
        let before_year_zero =
            SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(62_198_841_600);
        let evidence = current_time_evidence(
            before_year_zero,
            &arguments(r#"{"timezone":"UTC"}"#),
            &clock_failure_detail(),
            &offset_failure_detail(),
        )
        .expect("a representable negative year returns typed evidence");

        assert_eq!(
            evidence,
            ToolExecutorEvidence::KnownFailed {
                detail: Some(clock_failure_detail()),
            }
        );
    }

    /// S15: IANA lookup preserves a recognized alias and applies the zone's
    /// offset to the injected instant.
    #[test]
    fn s15_current_time_preserves_selected_iana_alias() {
        let evidence = current_time_evidence(
            SystemTime::UNIX_EPOCH,
            &arguments(r#"{"timezone":"US/Eastern"}"#),
            &clock_failure_detail(),
            &offset_failure_detail(),
        )
        .expect("valid IANA execution returns evidence");

        assert_eq!(
            evidence,
            ToolExecutorEvidence::CompletedText(String::from(
                r#"{"datetime":"1969-12-31T19:00:00-05:00","timezone":"US/Eastern"}"#
            ))
        );
    }

    /// S15: unknown IANA names fail catalog validation before execution.
    #[test]
    fn s15_current_time_rejects_unknown_iana_zone() {
        let (catalog, _executor) = CurrentTimeTool::try_new(|| SystemTime::UNIX_EPOCH)
            .expect("static current_time tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"timezone":"Mars/Olympus_Mons"}"#)
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// S15: Jiff's non-IANA unknown-zone sentinel is not a valid tool argument.
    #[test]
    fn s15_current_time_rejects_unknown_zone_sentinel() {
        let (catalog, _executor) = CurrentTimeTool::try_new(|| SystemTime::UNIX_EPOCH)
            .expect("static current_time tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"timezone":"Etc/Unknown"}"#)
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// S15: auxiliary TZif paths are not IANA zone or link identifiers.
    #[test]
    fn s15_current_time_rejects_auxiliary_zoneinfo_paths() {
        let (catalog, _executor) = CurrentTimeTool::try_new(|| SystemTime::UNIX_EPOCH)
            .expect("static current_time tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert_auxiliary_zoneinfo_path_rejected(&catalog, definition, "localtime");
        assert_auxiliary_zoneinfo_path_rejected(&catalog, definition, "posixrules");
        assert_auxiliary_zoneinfo_path_rejected(&catalog, definition, "posix/UTC");
        assert_auxiliary_zoneinfo_path_rejected(&catalog, definition, "right/UTC");
    }

    /// S15: a present timezone must be a string.
    #[test]
    fn s15_current_time_rejects_non_string_timezone() {
        let (catalog, _executor) = CurrentTimeTool::try_new(|| SystemTime::UNIX_EPOCH)
            .expect("static current_time tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(definition.name(), &arguments(r#"{"timezone":false}"#)),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// S15: an injected instant outside the supported civil-time range is a
    /// typed known failure rather than executor infrastructure failure.
    #[test]
    fn s15_current_time_reports_out_of_range_clock_as_known_failure() {
        let outside_jiff_range =
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(253_402_300_800);

        assert_eq!(
            current_time_evidence(
                outside_jiff_range,
                &arguments("{}"),
                &clock_failure_detail(),
                &offset_failure_detail(),
            )
            .expect("clock range is represented as executor evidence"),
            ToolExecutorEvidence::KnownFailed {
                detail: Some(clock_failure_detail()),
            }
        );
    }

    /// S15: a historical sub-minute IANA offset is a typed known failure
    /// rather than a minute-truncated timestamp for another instant.
    #[test]
    fn s15_current_time_rejects_offset_rfc3339_cannot_represent() {
        let start_of_1900_utc = SystemTime::UNIX_EPOCH
            .checked_sub(std::time::Duration::from_secs(2_208_988_800))
            .expect("fixture instant is representable");

        assert_eq!(
            current_time_evidence(
                start_of_1900_utc,
                &arguments(r#"{"timezone":"Europe/Paris"}"#),
                &clock_failure_detail(),
                &offset_failure_detail(),
            )
            .expect("offset range is represented as executor evidence"),
            ToolExecutorEvidence::KnownFailed {
                detail: Some(offset_failure_detail()),
            }
        );
    }
}

//! The first compiled hub-local tool: deterministic current-time lookup.

use std::{error::Error, fmt, future::Future, time::SystemTime};

use jiff::{Timestamp, fmt::strtime, tz::TimeZone};
use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence, ToolInputSchema,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolName,
    ToolPermissionDefault,
};

const CURRENT_TIME_NAME: &str = "current_time";
const CURRENT_TIME_DESCRIPTION: &str =
    "Returns the current time in UTC or an optional IANA time zone.";
const CURRENT_TIME_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "timezone": {
            "type": "string",
            "description": "Optional IANA time-zone name; defaults to UTC."
        }
    },
    "additionalProperties": false
}"#;
const INVALID_ARGUMENTS_DETAIL: &str = "expected an object with one optional IANA timezone string";
const CLOCK_OUT_OF_RANGE_DETAIL: &str = "current time is outside the supported range";
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
#[derive(Clone, Debug)]
pub struct CurrentTimeTool<Clock> {
    catalog: CompiledToolCatalog,
    executor: CurrentTimeExecutor<Clock>,
}

impl<Clock> CurrentTimeTool<Clock> {
    /// Compiles immutable metadata and validation around one injected clock.
    pub fn try_new(clock: Clock) -> Result<Self, CurrentTimeToolConstructionError> {
        let name = ToolName::try_new(String::from(CURRENT_TIME_NAME))
            .map_err(|_| CurrentTimeToolConstructionError::Name)?;
        let schema = ToolInputSchema::try_new(String::from(CURRENT_TIME_SCHEMA))
            .map_err(|_| CurrentTimeToolConstructionError::Schema)?;
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| CurrentTimeToolConstructionError::ErrorDetail)?;
        let clock_out_of_range_detail =
            ToolExecutionErrorDetail::try_new(String::from(CLOCK_OUT_OF_RANGE_DETAIL))
                .map_err(|_| CurrentTimeToolConstructionError::ErrorDetail)?;
        let definition = ToolDefinition::new(
            name,
            String::from(CURRENT_TIME_DESCRIPTION),
            schema,
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        );
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

/// Hub-local executor backed by an injected clock.
#[derive(Clone, Debug)]
pub struct CurrentTimeExecutor<Clock> {
    clock: Clock,
    clock_out_of_range_detail: ToolExecutionErrorDetail,
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
        );
        async move { evidence.map(|evidence| invocation.bind(evidence)) }
    }
}

struct ResolvedArguments {
    time_zone: TimeZone,
    canonical_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidCurrentTimeArguments;

fn resolve_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<ResolvedArguments, InvalidCurrentTimeArguments> {
    let serde_json::Value::Object(mut object) =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidCurrentTimeArguments)?
    else {
        return Err(InvalidCurrentTimeArguments);
    };
    if object.keys().any(|key| key != "timezone") {
        return Err(InvalidCurrentTimeArguments);
    }
    let Some(value) = object.remove("timezone") else {
        return Ok(ResolvedArguments {
            time_zone: TimeZone::UTC,
            canonical_name: String::from("UTC"),
        });
    };
    let serde_json::Value::String(name) = value else {
        return Err(InvalidCurrentTimeArguments);
    };
    let time_zone = TimeZone::get(&name).map_err(|_| InvalidCurrentTimeArguments)?;
    let canonical_name = time_zone
        .iana_name()
        .ok_or(InvalidCurrentTimeArguments)?
        .to_owned();
    Ok(ResolvedArguments {
        time_zone,
        canonical_name,
    })
}

fn current_time_evidence(
    now: SystemTime,
    arguments: &NormalizedToolArguments,
    clock_out_of_range_detail: &ToolExecutionErrorDetail,
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
    let datetime = strtime::format(RFC_3339_SECONDS_FORMAT, &zoned)
        .map_err(|_| CurrentTimeExecutorError::TimestampFormatting)?;
    let result = serde_json::to_string(&serde_json::json!({
        "datetime": datetime,
        "timezone": resolved.canonical_name,
    }))
    .map_err(|_| CurrentTimeExecutorError::ResultEncoding)?;
    Ok(ToolExecutorEvidence::CompletedText(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
    use signalbox_domain::NormalizedToolArguments;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    fn clock_failure_detail() -> ToolExecutionErrorDetail {
        ToolExecutionErrorDetail::try_new(String::from(CLOCK_OUT_OF_RANGE_DETAIL))
            .expect("static fixture detail is valid")
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

    /// S15 / INV-024: the result uses only the injected instant, defaults to
    /// UTC, and emits the exact compact whole-second contract.
    #[test]
    fn s15_inv024_current_time_uses_injected_instant_and_defaults_to_utc() {
        let evidence = current_time_evidence(
            SystemTime::UNIX_EPOCH,
            &arguments("{}"),
            &clock_failure_detail(),
        )
        .expect("valid UTC execution returns evidence");

        assert_eq!(
            evidence,
            ToolExecutorEvidence::CompletedText(String::from(
                r#"{"datetime":"1970-01-01T00:00:00+00:00","timezone":"UTC"}"#
            ))
        );
    }

    /// S15 / INV-024: IANA lookup canonicalizes the selected name and applies
    /// the zone's offset to the injected instant.
    #[test]
    fn s15_inv024_current_time_applies_canonical_iana_zone() {
        let evidence = current_time_evidence(
            SystemTime::UNIX_EPOCH,
            &arguments(r#"{"timezone":"america/new_york"}"#),
            &clock_failure_detail(),
        )
        .expect("valid IANA execution returns evidence");

        assert_eq!(
            evidence,
            ToolExecutorEvidence::CompletedText(String::from(
                r#"{"datetime":"1969-12-31T19:00:00-05:00","timezone":"America/New_York"}"#
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
            )
            .expect("clock range is represented as executor evidence"),
            ToolExecutorEvidence::KnownFailed {
                detail: Some(clock_failure_detail()),
            }
        );
    }
}

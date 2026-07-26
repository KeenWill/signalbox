//! Daemon-local conformance echo tool.

use std::{error::Error, fmt, future::Future};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolDefinition, ToolExecutionInvocation,
    ToolExecutor, ToolExecutorEvidence, ToolInputSchema,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolName,
    ToolPermissionDefault,
};

pub(crate) const ECHO_NAME: &str = "echo";
const ECHO_DESCRIPTION: &str = "Returns the supplied text unchanged in a compact JSON object.";
const ECHO_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "text": {
            "type": "string",
            "description": "Exact text to return."
        }
    },
    "required": ["text"],
    "additionalProperties": false
}"#;
const INVALID_ARGUMENTS_DETAIL: &str = "expected an object containing exactly one text string";

/// A static `echo` declaration could not be compiled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EchoToolConstructionError {
    /// The static name was rejected.
    Name,
    /// The static schema was rejected.
    Schema,
    /// The static sanitized error detail was rejected.
    ErrorDetail,
    /// The one-entry catalog unexpectedly reported a duplicate.
    Duplicate,
}

impl fmt::Display for EchoToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "echo static name is invalid",
            Self::Schema => "echo static schema is invalid",
            Self::ErrorDetail => "echo static error detail is invalid",
            Self::Duplicate => "echo catalog is duplicated",
        })
    }
}

impl Error for EchoToolConstructionError {}

/// Compiled catalog entry and matching executor for `echo`.
///
/// Effect posture: `EffectFree`. Execution observes no external state and
/// returns only a checked projection of the invocation arguments.
#[derive(Clone, Debug)]
pub struct EchoTool {
    catalog: CompiledToolCatalog,
    executor: EchoExecutor,
}

impl EchoTool {
    /// Compiles the immutable declaration and typed argument validator.
    pub fn try_new() -> Result<Self, EchoToolConstructionError> {
        let name = ToolName::try_new(String::from(ECHO_NAME))
            .map_err(|_| EchoToolConstructionError::Name)?;
        let schema = ToolInputSchema::try_new(String::from(ECHO_SCHEMA))
            .map_err(|_| EchoToolConstructionError::Schema)?;
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| EchoToolConstructionError::ErrorDetail)?;
        let definition = ToolDefinition::new(
            name,
            String::from(ECHO_DESCRIPTION),
            schema,
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        );
        let compiled = CompiledTool::new(
            definition,
            EchoArgumentValidator {
                detail: invalid_arguments_detail,
            },
        );
        let catalog = CompiledToolCatalog::try_new([compiled])
            .map_err(|_| EchoToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: EchoExecutor,
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, EchoExecutor) {
        (self.catalog, self.executor)
    }
}

#[derive(Clone, Debug)]
struct EchoArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for EchoArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

/// Daemon-local echo executor.
///
/// Effect posture: `EffectFree`. Execution observes no external state and
/// returns only a checked projection of the invocation arguments.
#[derive(Clone, Copy, Debug)]
pub struct EchoExecutor;

/// A checked catalog/executor assumption failed inside `echo`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EchoExecutorError;

impl fmt::Display for EchoExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("echo argument validation drifted")
    }
}

impl Error for EchoExecutorError {}

impl ClassifyOperatorFailure for EchoExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl ToolExecutor for EchoExecutor {
    type Error = EchoExecutorError;

    fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> impl Future<Output = Result<CorrelatedToolExecutorEvidence, Self::Error>> + Send {
        let evidence = echo_evidence(invocation.request().arguments());
        async move { evidence.map(|evidence| invocation.bind(evidence)) }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidEchoArguments;

fn decode_arguments(arguments: &NormalizedToolArguments) -> Result<(), InvalidEchoArguments> {
    let serde_json::Value::Object(object) =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidEchoArguments)?
    else {
        return Err(InvalidEchoArguments);
    };
    if object.len() != 1 {
        return Err(InvalidEchoArguments);
    }
    object
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or(InvalidEchoArguments)?;
    Ok(())
}

fn echo_evidence(
    arguments: &NormalizedToolArguments,
) -> Result<ToolExecutorEvidence, EchoExecutorError> {
    decode_arguments(arguments).map_err(|_| EchoExecutorError)?;
    Ok(ToolExecutorEvidence::CompletedText(
        arguments.as_str().to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};

    use super::*;

    fn arguments(value: &str) -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(value.to_owned())
            .expect("fixture arguments are admitted")
    }

    /// The conformance declaration is auto-approved and effect-free.
    #[test]
    fn echo_definition_carries_exact_policy() {
        let (catalog, _executor) = EchoTool::try_new()
            .expect("static echo tool compiles")
            .into_parts();
        let definitions = catalog.definitions();
        let [definition] = definitions.as_ref() else {
            panic!("echo is the one compiled definition")
        };

        assert_eq!(definition.name().as_str(), ECHO_NAME);
        assert_eq!(definition.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(definition.effect_class(), ToolEffectClass::EffectFree);
    }

    /// Typed decoding accepts one exact text field.
    #[test]
    fn echo_typed_decode_accepts_exact_text_shape() {
        let (catalog, _executor) = EchoTool::try_new()
            .expect("static echo tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert_eq!(
            catalog.validate_arguments(definition.name(), &arguments(r#"{"text":"hello"}"#),),
            Ok(())
        );
    }

    /// Typed decoding rejects an unexpected field.
    #[test]
    fn echo_typed_decode_rejects_unexpected_field() {
        let (catalog, _executor) = EchoTool::try_new()
            .expect("static echo tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];

        assert!(matches!(
            catalog.validate_arguments(
                definition.name(),
                &arguments(r#"{"extra":1,"text":"hello"}"#),
            ),
            Err(ToolCatalogValidationFailure::InvalidArguments { detail: Some(_) })
        ));
    }

    /// Success returns the canonical compact argument object unchanged.
    #[test]
    fn echo_result_is_exact_compact_json() {
        let supplied = arguments(r#"{ "text" : "hello" }"#);

        let evidence = echo_evidence(&supplied).expect("validated echo arguments execute");

        assert_eq!(
            evidence,
            ToolExecutorEvidence::CompletedText(supplied.as_str().to_owned())
        );
    }
}

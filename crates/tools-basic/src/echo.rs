//! Daemon-local conformance echo tool.

use std::{error::Error, fmt, future::Future};

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

pub const ECHO_NAME: &str = "echo";
const INVALID_ARGUMENTS_DETAIL: &str = "expected an object containing exactly one text string";

/// Typed `echo` argument shape; decoder and rendered schema share it.
#[derive(Debug, serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
pub struct EchoArguments {
    #[tool_schema(description = "Exact text to return.")]
    #[expect(dead_code, reason = "execution returns the canonical argument text")]
    text: String,
}

impl ToolContract for EchoTool {
    type Arguments = EchoArguments;
    const NAME: &'static str = ECHO_NAME;
    const DESCRIPTION: &'static str =
        "Returns the supplied text unchanged in a compact JSON object.";
}

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
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| EchoToolConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<Self>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => EchoToolConstructionError::Name,
            ToolContractCompileError::Schema => EchoToolConstructionError::Schema,
        })?;
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
    serde_json::from_str::<EchoArguments>(arguments.as_str())
        .map(|_| ())
        .map_err(|_| InvalidEchoArguments)
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
    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure, ToolInputSchema};

    use super::*;

    const SHIPPED_ECHO_SCHEMA: &str = r#"{
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

    fn shipped_echo_schema() -> ToolInputSchema {
        ToolInputSchema::try_new(String::from(SHIPPED_ECHO_SCHEMA))
            .expect("shipped echo schema remains admitted")
    }

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

    /// The derived schema remains byte-identical to the canonical artifact
    /// produced from the hand-written schema that shipped before derivation.
    #[test]
    fn echo_derived_schema_is_byte_identical_to_shipped_schema() {
        let (catalog, _executor) = EchoTool::try_new()
            .expect("static echo tool compiles")
            .into_parts();
        let definition = &catalog.definitions()[0];
        let shipped = shipped_echo_schema();
        let schema: serde_json::Value = serde_json::from_str(definition.input_schema().as_str())
            .expect("registry schema is valid JSON");

        expect_test::expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "text": {
                  "description": "Exact text to return.",
                  "type": "string"
                }
              },
              "required": [
                "text"
              ],
              "type": "object"
            }"#]]
        .assert_eq(&format!("{schema:#}"));
        assert_eq!(definition.input_schema().as_str(), shipped.as_str());
        assert_eq!(
            <EchoArguments as signalbox_tool_contract::ToolSchema>::schema().to_string(),
            shipped.as_str()
        );
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

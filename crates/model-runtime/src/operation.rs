//! The one explicitly authorized model operation.

use crate::credential::CredentialReference;
use crate::message::ConversationMessage;
use crate::output::StructuredOutputContract;
use crate::settings::ModelSettings;
use crate::target::{RequestedTarget, ResolvedTarget};
use crate::tool::{ToolDefinition, ToolName};

/// Why an operation cannot be translated into one provider interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelOperationValidationError {
    /// Two ordinary tools use the same proposal/dispatch name.
    DuplicateToolName {
        /// The repeated ordinary-tool name.
        name: ToolName,
    },
    /// An ordinary tool uses the name reserved for structured output.
    OutputContractToolNameCollision {
        /// The name used by both declarations.
        name: ToolName,
    },
    /// A named tool choice does not match any declared tool.
    UndeclaredToolChoice {
        /// The name the choice requires.
        name: ToolName,
    },
}

impl std::fmt::Display for ModelOperationValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateToolName { name } => {
                write!(
                    f,
                    "ordinary tool name `{}` is declared more than once",
                    name.as_str()
                )
            }
            Self::UndeclaredToolChoice { name } => {
                write!(
                    f,
                    "tool choice names `{}`, which no declared tool carries",
                    name.as_str()
                )
            }
            Self::OutputContractToolNameCollision { name } => write!(
                f,
                "structured-output name `{}` is also declared as an ordinary tool",
                name.as_str()
            ),
        }
    }
}

impl std::error::Error for ModelOperationValidationError {}

/// Whether the caller wants the exchange delivered as one buffered response
/// or as a provider event stream.
///
/// Either way the adapter performs at most one provider interaction and
/// reports the same terminal-evidence vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// One buffered response body.
    Buffered,
    /// The provider's event stream, surfaced as observations.
    Streamed,
}

/// How the provider may choose among the declared tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides whether to call a declared tool.
    Automatic,
    /// The model must call some declared tool.
    AnyTool,
    /// The model must call the named tool.
    Named(ToolName),
}

/// One explicitly authorized model operation.
///
/// The correlation value `C` is the caller's durable operation identity
/// (Signalbox's `ModelCallId`, or any other caller identity), opaque to this
/// layer. Adapters thread it onto every observation and the terminal report
/// (docs/spec/model-call-execution.md: a runtime-generated identity is never
/// authoritative correlation).
///
/// One operation authorizes at most one provider interaction. Nothing in
/// this layer turns one operation into two requests; a retry or continuation
/// is a new operation with a new caller identity.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOperation<C> {
    /// The caller's durable identity for this operation, threaded onto every
    /// observation and evidence record.
    pub correlation: C,
    /// The non-secret credential reference pinned with this operation.
    /// Adapters resolve its current value during preparation of each
    /// physical request through [`crate::CredentialAccess`]
    /// (docs/spec/runtime-substrate.md).
    pub credential_reference: CredentialReference,
    /// The caller's original model selection, for provenance.
    pub requested_target: RequestedTarget,
    /// The exact hub-resolved model identifier this operation must use.
    pub resolved_target: ResolvedTarget,
    /// System instructions, when the caller supplies any.
    pub system: Option<String>,
    /// Conversation history, oldest first.
    pub messages: Vec<ConversationMessage>,
    /// Sampling and limit settings.
    pub settings: ModelSettings,
    /// Tools the model may propose calling.
    pub tools: Vec<ToolDefinition>,
    /// How the provider may choose among the declared tools; ignored by
    /// adapters when `tools` is empty and no output contract is set.
    pub tool_choice: ToolChoice,
    /// A structured-output contract the response must satisfy, when the
    /// caller demands typed output.
    pub output_contract: Option<StructuredOutputContract>,
    /// Buffered or streamed delivery.
    pub delivery: DeliveryMode,
}

impl<C> ModelOperation<C> {
    /// An operation carrying the required facts; optional facts start empty
    /// (no system prompt, no tools, automatic tool choice, no output
    /// contract, buffered delivery).
    pub fn new(
        correlation: C,
        credential_reference: CredentialReference,
        requested_target: RequestedTarget,
        resolved_target: ResolvedTarget,
        messages: Vec<ConversationMessage>,
        settings: ModelSettings,
    ) -> Self {
        Self {
            correlation,
            credential_reference,
            requested_target,
            resolved_target,
            system: None,
            messages,
            settings,
            tools: Vec::new(),
            tool_choice: ToolChoice::Automatic,
            output_contract: None,
            delivery: DeliveryMode::Buffered,
        }
    }

    /// Checks provider-neutral declaration rules before an adapter sends the
    /// operation.
    ///
    /// A structured-output contract reserves its name: otherwise a returned
    /// proposal with that name cannot be distinguished from an ordinary tool
    /// call. Provider adapters call this during preparation and report a
    /// failure without crossing the request boundary when it returns an error.
    pub fn validate(&self) -> Result<(), ModelOperationValidationError> {
        let mut names = std::collections::HashSet::with_capacity(self.tools.len());
        for tool in &self.tools {
            if !names.insert(&tool.name) {
                return Err(ModelOperationValidationError::DuplicateToolName {
                    name: tool.name.clone(),
                });
            }
        }
        if let ToolChoice::Named(name) = &self.tool_choice
            && !self.tools.iter().any(|tool| &tool.name == name)
        {
            // An impossible choice is a locally knowable declaration error;
            // sending it would only surface as a post-boundary provider
            // rejection.
            return Err(ModelOperationValidationError::UndeclaredToolChoice { name: name.clone() });
        }
        let Some(contract) = &self.output_contract else {
            return Ok(());
        };
        if self.tools.iter().any(|tool| tool.name == contract.name) {
            return Err(
                ModelOperationValidationError::OutputContractToolNameCollision {
                    name: contract.name.clone(),
                },
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::{ModelOperation, ModelOperationValidationError};
    use crate::credential::CredentialReference;
    use crate::message::ConversationMessage;
    use crate::output::StructuredOutputContract;
    use crate::settings::ModelSettings;
    use crate::target::{RequestedTarget, ResolvedTarget};
    use crate::tool::ToolDefinition;

    #[allow(dead_code)]
    #[derive(JsonSchema)]
    struct Arguments {
        value: String,
    }

    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct Output {
        value: String,
    }

    fn operation() -> ModelOperation<()> {
        ModelOperation::new(
            (),
            CredentialReference::new("credential"),
            RequestedTarget::new("requested"),
            ResolvedTarget::new("resolved"),
            Vec::<ConversationMessage>::new(),
            ModelSettings::new(128),
        )
    }

    #[test]
    fn structured_output_name_is_reserved_from_ordinary_tools() {
        let contract = StructuredOutputContract::of_type::<Output>("result", "result");
        let colliding_name = contract.name.clone();
        let mut operation = operation();
        operation.tools = vec![ToolDefinition::of_type::<Arguments>(
            colliding_name.as_str(),
            "ordinary tool",
        )];
        operation.output_contract = Some(contract);

        let error = operation
            .validate()
            .expect_err("collision must be rejected");

        assert_eq!(
            error,
            ModelOperationValidationError::OutputContractToolNameCollision {
                name: colliding_name,
            }
        );
    }

    #[test]
    fn a_named_choice_for_an_undeclared_tool_is_rejected() {
        let mut operation = operation();
        operation.tools = vec![ToolDefinition::of_type::<Arguments>(
            "ordinary",
            "ordinary tool",
        )];
        operation.tool_choice = crate::ToolChoice::Named(crate::ToolName::new("missing"));

        let error = operation
            .validate()
            .expect_err("an impossible choice must fail before any send");

        assert_eq!(
            error,
            ModelOperationValidationError::UndeclaredToolChoice {
                name: crate::ToolName::new("missing"),
            }
        );
    }

    #[test]
    fn distinct_tool_and_structured_output_names_are_valid() {
        let mut operation = operation();
        operation.tools = vec![ToolDefinition::of_type::<Arguments>(
            "ordinary",
            "ordinary tool",
        )];
        operation.output_contract = Some(StructuredOutputContract::of_type::<Output>(
            "result", "result",
        ));

        assert_eq!(operation.validate(), Ok(()));
    }

    #[test]
    fn duplicate_ordinary_tool_names_are_rejected() {
        let duplicate_name = "ordinary";
        let mut operation = operation();
        operation.tools = vec![
            ToolDefinition::of_type::<Arguments>(duplicate_name, "first declaration"),
            ToolDefinition::of_type::<Arguments>(duplicate_name, "second declaration"),
        ];

        let error = operation
            .validate()
            .expect_err("duplicate ordinary names must be rejected");

        assert_eq!(
            error,
            ModelOperationValidationError::DuplicateToolName {
                name: crate::ToolName::new(duplicate_name),
            }
        );
    }
}

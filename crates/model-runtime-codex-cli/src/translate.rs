//! Stateless rendering of one model operation into Codex stdin.

use serde_json::{Value, json};
use signalbox_model_runtime::{
    ConversationMessage, ConversationRole, MessagePart, ModelOperation, PreparationDefect,
    PreparationFailure, ToolChoice,
};

pub(crate) struct TranslatedOperation {
    pub(crate) prompt: Vec<u8>,
    pub(crate) declared_tools: Vec<String>,
    pub(crate) output_contract_name: Option<String>,
    pub(crate) tool_requirement: ToolRequirement,
}

#[derive(Clone)]
pub(crate) enum ToolRequirement {
    Optional,
    Any,
    Named(String),
}

pub(crate) fn translate<C>(
    operation: &ModelOperation<C>,
) -> Result<TranslatedOperation, TranslationError> {
    operation.validate().map_err(|error| {
        TranslationError::Failure(PreparationFailure::UnsupportedOperation {
            detail: error.to_string(),
        })
    })?;
    validate_settings(operation)?;

    let messages = operation
        .messages
        .iter()
        .map(render_message)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = operation
        .tools
        .iter()
        .map(|tool| {
            let input_schema = parse_object_schema(
                tool.input_schema.get(),
                &format!("tool `{}` input schema", tool.name.as_str()),
            )?;
            Ok(json!({
                "name": tool.name.as_str(),
                "description": tool.description,
                "input_schema": input_schema,
            }))
        })
        .collect::<Result<Vec<_>, TranslationError>>()?;
    let output_contract = operation
        .output_contract
        .as_ref()
        .map(|contract| {
            let schema = parse_object_schema(
                contract.schema.get(),
                &format!("structured output `{}` schema", contract.name.as_str()),
            )?;
            Ok(json!({
                "name": contract.name.as_str(),
                "description": contract.description,
                "schema": schema,
            }))
        })
        .transpose()?;

    let effective_tool_choice = if let Some(contract) = &operation.output_contract {
        ToolChoice::Named(contract.name.clone())
    } else if operation.tools.is_empty() {
        ToolChoice::Automatic
    } else {
        operation.tool_choice.clone()
    };
    let tool_choice = match &effective_tool_choice {
        ToolChoice::Automatic => json!({"kind": "automatic"}),
        ToolChoice::AnyTool => json!({"kind": "any_tool"}),
        ToolChoice::Named(name) => json!({"kind": "named", "name": name.as_str()}),
    };
    let tool_requirement = match effective_tool_choice {
        ToolChoice::Automatic => ToolRequirement::Optional,
        ToolChoice::AnyTool => ToolRequirement::Any,
        ToolChoice::Named(name) => ToolRequirement::Named(name.as_str().to_string()),
    };
    let request = json!({
        "system": operation.system,
        "messages": messages,
        "settings": {
            "max_output_tokens": operation.settings.max_output_tokens,
            "temperature": operation.settings.temperature,
            "top_p": operation.settings.top_p,
            "stop_sequences": operation.settings.stop_sequences,
        },
        "tools": tools,
        "tool_choice": tool_choice,
        "structured_output": output_contract,
    });
    let request_json = serde_json::to_string(&request).map_err(|error| {
        TranslationError::Defect(PreparationDefect::SerializationFailed {
            detail: error.to_string(),
        })
    })?;
    let prompt = format!(
        "Act only as the model for the following stateless request. Do not use shell, \
         file, web, MCP, or collaboration tools. The complete ordered context is in \
         the JSON below. Return exactly the response envelope required by the supplied \
         output schema. `outcome` is `refused` only for a safety refusal. For ordinary \
         completion put response text in `text`. Propose caller-declared tools only in \
         `tool_calls`, preserving their argument values as JSON objects. If \
         `structured_output` is present, return exactly one tool call bearing its name \
         and the contracted value as `arguments`. Honor `tool_choice` and the stated \
         generation settings.\n\n{request_json}\n"
    )
    .into_bytes();

    Ok(TranslatedOperation {
        prompt,
        declared_tools: operation
            .tools
            .iter()
            .map(|tool| tool.name.as_str().to_string())
            .collect(),
        output_contract_name: operation
            .output_contract
            .as_ref()
            .map(|contract| contract.name.as_str().to_string()),
        tool_requirement,
    })
}

fn render_message(message: &ConversationMessage) -> Result<Value, TranslationError> {
    let role = match message.role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
    };
    let parts = message
        .parts
        .iter()
        .map(render_part)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"role": role, "parts": parts}))
}

fn render_part(part: &MessagePart) -> Result<Value, TranslationError> {
    match part {
        MessagePart::Text(text) => Ok(json!({"type": "text", "text": text})),
        MessagePart::ToolCall(call) => Ok(json!({
            "type": "tool_call",
            "id": call.id.as_str(),
            "name": call.name.as_str(),
            "arguments": parse_raw_json(&call.arguments_json)?,
        })),
        MessagePart::ToolResult(result) => Ok(json!({
            "type": "tool_result",
            "tool_call_id": result.tool_call_id.as_str(),
            "content": result.content,
            "is_error": result.is_error,
        })),
        MessagePart::Thinking { text, signature } => Ok(json!({
            "type": "thinking",
            "text": text,
            "signature": signature,
        })),
        MessagePart::RedactedThinking { data } => {
            Ok(json!({"type": "redacted_thinking", "data": data}))
        }
    }
}

fn parse_raw_json(raw: &str) -> Result<Value, TranslationError> {
    serde_json::from_str(raw).map_err(|error| {
        TranslationError::Defect(PreparationDefect::SerializationFailed {
            detail: error.to_string(),
        })
    })
}

fn parse_object_schema(raw: &str, subject: &str) -> Result<Value, TranslationError> {
    let schema = parse_raw_json(raw)?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: format!("{subject} must describe an object at its root"),
            },
        ));
    }
    Ok(schema)
}

fn validate_settings<C>(operation: &ModelOperation<C>) -> Result<(), TranslationError> {
    if operation.settings.max_output_tokens == 0 {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "max_output_tokens must be at least 1".to_string(),
            },
        ));
    }
    if let Some(value) = operation.settings.temperature
        && !(0.0..=2.0).contains(&value)
    {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "temperature must be a finite number from 0 through 2".to_string(),
            },
        ));
    }
    if let Some(value) = operation.settings.top_p
        && !(0.0..=1.0).contains(&value)
    {
        return Err(TranslationError::Failure(
            PreparationFailure::UnsupportedOperation {
                detail: "top_p must be a finite number from 0 through 1".to_string(),
            },
        ));
    }
    Ok(())
}

pub(crate) enum TranslationError {
    Failure(PreparationFailure),
    Defect(PreparationDefect),
}

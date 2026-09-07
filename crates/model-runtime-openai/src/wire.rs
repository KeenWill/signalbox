//! Responses request, response, and SSE payload types for `POST /v1/responses`.
//! Additive fields are tolerated; item and event kinds are checked by decoders.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

#[derive(Debug, Serialize)]
pub(crate) struct CreateResponse {
    pub model: String,
    pub input: Vec<WireInputItem>,
    pub max_output_tokens: u32,
    pub store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<WireReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireFunctionTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    pub stream: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireReasoning {
    pub effort: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WireInputItem {
    Message {
        role: &'static str,
        content: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct WireFunctionTool {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: String,
    pub description: String,
    pub parameters: Box<RawValue>,
    pub strict: bool,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "ResponseEnvelope")]
pub(crate) struct Response {
    pub id: Option<String>,
    pub object: Option<String>,
    pub model: Option<String>,
    pub status: Option<String>,
    pub incomplete_details: Option<IncompleteDetails>,
    pub output: Option<Box<RawValue>>,
    pub usage: Option<WireUsage>,
    pub error: Option<ResponseError>,
}

#[derive(Deserialize)]
struct ResponseEnvelope {
    id: Option<String>,
    object: Option<String>,
    model: Option<String>,
    status: Option<String>,
    incomplete_details: Option<Box<RawValue>>,
    output: Option<Box<RawValue>>,
    usage: Option<Box<RawValue>>,
    error: Option<ResponseError>,
}

impl TryFrom<ResponseEnvelope> for Response {
    type Error = serde_json::Error;

    fn try_from(envelope: ResponseEnvelope) -> Result<Self, Self::Error> {
        let failed = envelope.status.as_deref() == Some("failed") && envelope.error.is_some();
        let usage = envelope
            .usage
            .map(|raw| serde_json::from_str(raw.get()))
            .transpose();
        let usage = if failed {
            usage.unwrap_or(None)
        } else {
            usage?
        };
        let incomplete_details = if failed {
            None
        } else {
            envelope
                .incomplete_details
                .map(|raw| serde_json::from_str(raw.get()))
                .transpose()?
        };
        Ok(Self {
            id: envelope.id,
            object: envelope.object,
            model: envelope.model,
            status: envelope.status,
            incomplete_details,
            output: envelope.output,
            usage,
            error: envelope.error,
        })
    }
}

impl Response {
    pub(crate) fn output_items(&self) -> Result<Option<Vec<Box<RawValue>>>, serde_json::Error> {
        self.output
            .as_ref()
            .map(|raw| serde_json::from_str(raw.get()))
            .transpose()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct IncompleteDetails {
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireOutputItem {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Option<String>,
    pub status: Option<String>,
    pub role: Option<String>,
    pub content: Option<Vec<WireContent>>,
    pub call_id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WireContent {
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    #[serde(other)]
    Unknown,
}

impl WireContent {
    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::OutputText { text } => Some(text),
            Self::Refusal { refusal } => Some(refusal),
            Self::Unknown => None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub input_tokens_details: Option<InputTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InputTokensDetails {
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

/// The accepted-response and bare SSE error shape has no HTTP error type.
#[derive(Debug, Deserialize)]
pub(crate) struct ResponseError {
    pub code: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorEnvelope {
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireError {
    pub message: Option<String>,
    #[serde(rename = "type")]
    pub error_type: Option<String>,
    #[serde(default)]
    pub code: Option<serde_json::Value>,
}

impl WireError {
    /// The native code as text, when one was carried.
    pub(crate) fn code_text(&self) -> Option<String> {
        match &self.code {
            Some(serde_json::Value::String(code)) => Some(code.clone()),
            Some(serde_json::Value::Number(code)) => Some(code.to_string()),
            _ => None,
        }
    }

    /// Retains the native error material verbatim as neutral evidence.
    pub(crate) fn into_native_facts(self) -> signalbox_model_runtime::NativeErrorFacts {
        let code = self.code_text();
        signalbox_model_runtime::NativeErrorFacts {
            error_token: self.error_type,
            error_code: code,
            message: self.message,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseEvent {
    #[serde(rename = "type")]
    pub kind: String,
    pub response: Option<Response>,
    pub output_index: Option<u32>,
    pub content_index: Option<u32>,
    pub part: Option<WireContent>,
    pub item_id: Option<String>,
    pub item: Option<Box<RawValue>>,
    pub delta: Option<String>,
    pub code: Option<String>,
    pub message: Option<String>,
}

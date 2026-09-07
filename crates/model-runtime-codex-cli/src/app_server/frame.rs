use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) enum KnownError {
    ContextWindowExceeded,
    SessionBudgetExceeded,
    UsageLimitExceeded,
    RateLimitExceeded,
    ServerOverloaded,
    CyberPolicy,
    MisalignmentPolicyViolation,
    HttpConnectionFailed {
        #[serde(rename = "httpStatusCode")]
        http_status_code: Option<u16>,
    },
    ResponseStreamConnectionFailed {
        #[serde(rename = "httpStatusCode")]
        http_status_code: Option<u16>,
    },
    InternalServerError,
    Unauthorized,
    BadRequest,
    ThreadRollbackFailed,
    SandboxError,
    ResponseStreamDisconnected {
        #[serde(rename = "httpStatusCode")]
        http_status_code: Option<u16>,
    },
    ResponseTooManyFailedAttempts {
        #[serde(rename = "httpStatusCode")]
        http_status_code: Option<u16>,
    },
    ActiveTurnNotSteerable {
        #[serde(rename = "turnKind")]
        turn_kind: String,
    },
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CodexErrorInfo {
    Known(KnownError),
    Unknown { tag: String },
}

// The schema guard checks the known shapes at the consumed field and admits
// the decoder's unknown-tag fallback.
#[cfg(test)]
impl schemars::JsonSchema for CodexErrorInfo {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CodexErrorInfo".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = <KnownError as schemars::JsonSchema>::json_schema(generator);
        schema.insert("x-codex-unknown-tags".into(), true.into());
        schema
    }
}

impl<'de> Deserialize<'de> for CodexErrorInfo {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let tag = match &value {
            Value::String(tag) => tag.clone(),
            Value::Object(fields) if fields.len() == 1 => fields
                .keys()
                .next()
                .cloned()
                .ok_or_else(|| de::Error::custom("codexErrorInfo has no variant"))?,
            _ => return Err(de::Error::custom("codexErrorInfo must name one variant")),
        };
        match serde_json::from_value(value) {
            Ok(known) => Ok(Self::Known(known)),
            Err(error) if Self::known_tag(&tag) => Err(de::Error::custom(error)),
            Err(_) => Ok(Self::Unknown { tag }),
        }
    }
}

impl CodexErrorInfo {
    pub(crate) fn known_tag(tag: &str) -> bool {
        matches!(
            tag,
            "contextWindowExceeded"
                | "sessionBudgetExceeded"
                | "usageLimitExceeded"
                | "rateLimitExceeded"
                | "serverOverloaded"
                | "cyberPolicy"
                | "misalignmentPolicyViolation"
                | "httpConnectionFailed"
                | "responseStreamConnectionFailed"
                | "internalServerError"
                | "unauthorized"
                | "badRequest"
                | "threadRollbackFailed"
                | "sandboxError"
                | "responseStreamDisconnected"
                | "responseTooManyFailedAttempts"
                | "activeTurnNotSteerable"
                | "other"
        )
    }

    pub(crate) fn tag(&self) -> &str {
        match self {
            Self::Known(known) => match known {
                KnownError::ContextWindowExceeded => "contextWindowExceeded",
                KnownError::SessionBudgetExceeded => "sessionBudgetExceeded",
                KnownError::UsageLimitExceeded => "usageLimitExceeded",
                KnownError::RateLimitExceeded => "rateLimitExceeded",
                KnownError::ServerOverloaded => "serverOverloaded",
                KnownError::CyberPolicy => "cyberPolicy",
                KnownError::MisalignmentPolicyViolation => "misalignmentPolicyViolation",
                KnownError::HttpConnectionFailed { .. } => "httpConnectionFailed",
                KnownError::ResponseStreamConnectionFailed { .. } => {
                    "responseStreamConnectionFailed"
                }
                KnownError::InternalServerError => "internalServerError",
                KnownError::Unauthorized => "unauthorized",
                KnownError::BadRequest => "badRequest",
                KnownError::ThreadRollbackFailed => "threadRollbackFailed",
                KnownError::SandboxError => "sandboxError",
                KnownError::ResponseStreamDisconnected { .. } => "responseStreamDisconnected",
                KnownError::ResponseTooManyFailedAttempts { .. } => "responseTooManyFailedAttempts",
                KnownError::ActiveTurnNotSteerable { .. } => "activeTurnNotSteerable",
                KnownError::Other => "other",
            },
            Self::Unknown { tag } => tag,
        }
    }

    pub(crate) fn http_status(&self) -> Option<u16> {
        match self {
            Self::Known(
                KnownError::HttpConnectionFailed { http_status_code }
                | KnownError::ResponseStreamConnectionFailed { http_status_code }
                | KnownError::ResponseStreamDisconnected { http_status_code }
                | KnownError::ResponseTooManyFailedAttempts { http_status_code },
            ) => *http_status_code,
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct TurnError {
    pub(crate) message: String,
    pub(crate) codex_error_info: Option<CodexErrorInfo>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) enum TurnStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct Turn {
    pub(crate) id: String,
    pub(crate) status: TurnStatus,
    pub(crate) error: Option<TurnError>,
    #[cfg_attr(test, schemars(schema_with = "consumed_turn_items_schema"))]
    pub(crate) items: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct TurnCompleted {
    pub(crate) thread_id: String,
    pub(crate) turn: Turn,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct ErrorNotification {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) error: TurnError,
    pub(crate) will_retry: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageBreakdown {
    pub(crate) input_tokens: Option<i64>,
    pub(crate) cached_input_tokens: Option<i64>,
    #[serde(default)]
    pub(crate) cache_write_input_tokens: Option<i64>,
    pub(crate) output_tokens: Option<i64>,
    pub(crate) reasoning_output_tokens: Option<i64>,
    pub(crate) total_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ThreadTokenUsage {
    pub(crate) total: UsageBreakdown,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TokenUsageUpdated {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) token_usage: ThreadTokenUsage,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ItemTextDelta {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) item_id: String,
    pub(crate) delta: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ItemNotification {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) item: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct AgentMessage {
    pub(crate) id: String,
    pub(crate) text: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ThreadStartResponse {
    pub(crate) thread: Thread,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Thread {
    pub(crate) id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TurnStartResponse {
    pub(crate) turn: Turn,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct RpcError {
    pub(crate) code: i64,
    pub(crate) data: Option<Value>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadOptions {
    pub(crate) model: String,
    pub(crate) cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) service_tier: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnInput {
    pub(crate) input: Vec<TextInput>,
    pub(crate) output_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct TextInput {
    #[serde(rename = "type")]
    pub(crate) kind: TextInputKind,
    pub(crate) text: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TextInputKind {
    Text,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct AccountRateLimitsUpdated {
    pub(crate) rate_limits: RateLimits,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct RateLimits {
    pub(crate) primary: Option<RateLimitWindow>,
    pub(crate) secondary: Option<RateLimitWindow>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct RateLimitWindow {
    pub(crate) used_percent: f64,
    pub(crate) resets_at: Option<i64>,
}

impl RateLimits {
    pub(crate) fn merge(&mut self, update: Self) {
        if update.primary.is_some() {
            self.primary = update.primary;
        }
        if update.secondary.is_some() {
            self.secondary = update.secondary;
        }
    }

    pub(crate) fn retry_after(&self, now: std::time::SystemTime) -> Option<std::time::Duration> {
        let latest = [self.primary.as_ref(), self.secondary.as_ref()]
            .into_iter()
            .flatten()
            .filter(|window| window.used_percent >= 100.0)
            .filter_map(|window| window.resets_at)
            .max()?;
        let reset = std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(
            u64::try_from(latest).unwrap_or(0),
        ))?;
        Some(
            reset
                .duration_since(now)
                .unwrap_or(std::time::Duration::ZERO),
        )
    }
}

// Turn items are retained as JSON, then agent messages are decoded separately.
#[cfg(test)]
fn consumed_turn_items_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = <Vec<AgentMessage> as schemars::JsonSchema>::json_schema(generator);
    schema.insert("x-codex-agent-items".into(), true.into());
    schema
}

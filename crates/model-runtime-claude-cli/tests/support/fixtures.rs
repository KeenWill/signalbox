//! Synthetic values shared by the offline fake CLI and assertions.

pub const SESSION_ID: &str = "session-synthetic-1";
pub const MESSAGE_ID: &str = "message-synthetic-1";
pub const OTHER_MESSAGE_ID: &str = "message-synthetic-2";
pub const MODEL: &str = "claude-synthetic-model";
pub const ANSWER: &str = "synthetic completion";
pub const SAFE_CREDENTIAL_PREFIX: &str = "API";
pub const REFUSAL: &str = "synthetic refusal";
pub const SENSITIVE_TEXT: &str = "Authorization: synthetic-credential-value";
pub const FRAGMENTED_SECRET_PREFIX: &str = "api_";
pub const FRAGMENTED_SECRET_CONTINUATION: &str = "key=synthetic-fragment";
pub const FRAGMENTED_SECRET: &str = "api_key=synthetic-fragment";
pub const TOOL_NAME: &str = "synthetic_lookup";
pub const OTHER_TOOL_NAME: &str = "synthetic_other";
pub const TOOL_ID: &str = "toolu_synthetic_1";
pub const OTHER_TOOL_ID: &str = "toolu_synthetic_2";
pub const TOOL_ARGUMENTS: &str = r#"{"subject":"synthetic"}"#;
pub const INPUT_TOKENS: u64 = 11;
pub const OUTPUT_TOKENS: u64 = 7;
pub const CACHE_CREATION_TOKENS: u64 = 2;
pub const CACHE_READ_TOKENS: u64 = 3;

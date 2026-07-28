//! Synthetic values shared by the offline fake CLI and assertions.

pub const SESSION_ID: &str = "session-synthetic-1";
pub const MESSAGE_ID: &str = "message-synthetic-1";
pub const MODEL: &str = "claude-synthetic-model";
pub const ANSWER: &str = "synthetic completion";
pub const REFUSAL: &str = "synthetic refusal";
pub const SENSITIVE_TEXT: &str = "Authorization: synthetic-credential-value";
pub const TOOL_NAME: &str = "synthetic_lookup";
pub const TOOL_ID: &str = "toolu_synthetic_1";
pub const TOOL_ARGUMENTS: &str = r#"{"subject":"synthetic"}"#;
pub const INPUT_TOKENS: u64 = 11;
pub const OUTPUT_TOKENS: u64 = 7;
pub const CACHE_CREATION_TOKENS: u64 = 2;
pub const CACHE_READ_TOKENS: u64 = 3;

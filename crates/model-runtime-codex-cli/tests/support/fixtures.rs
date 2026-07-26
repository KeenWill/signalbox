//! Values shared by the scripted executable and its process-level assertions.

pub const THREAD_ID: &str = "thread-offline-1";
pub const BUFFERED_ANSWER: &str = "buffered answer";
pub const STREAMED_ANSWER: &str = "streamed answer";
pub const REASONING_TEXT: &str = "considering";
pub const REFUSAL_TEXT: &str = "request refused";
pub const TOOL_NAME: &str = "lookup";
pub const TOOL_ARGUMENTS: &str = r#"{ "city" : "Oslo", "limit": 3 }"#;
pub const STRUCTURED_ACCEPTED: bool = true;
pub const INPUT_TOKENS: u64 = 11;
pub const CACHE_READ_INPUT_TOKENS: u64 = 2;
pub const CACHE_CREATION_INPUT_TOKENS: u64 = 1;
pub const OUTPUT_TOKENS: u64 = 7;
pub const SENSITIVE_OUTPUT_TOKEN: &str = "sk-sensitive-output";
pub const SENSITIVE_REFRESH_TOKEN: &str = "sensitive-refresh";
pub const SENSITIVE_STDERR_TOKEN: &str = "sensitive-stderr";
pub const SENSITIVE_TOOL_ID_ONE: &str = "sk-sensitive-call-one";
pub const SENSITIVE_TOOL_ID_TWO: &str = "API_KEY=sensitive-call-two";
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const REDACTED_TOOL_ID_ONE: &str = "codex-redacted-call-1";
#[allow(
    dead_code,
    reason = "process-only expected values share this module with the fake executable"
)]
pub const REDACTED_TOOL_ID_TWO: &str = "codex-redacted-call-2";

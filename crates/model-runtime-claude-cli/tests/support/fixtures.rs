//! Synthetic values shared by the offline fake CLI and assertions.
#![allow(
    dead_code,
    reason = "shared fixture accessors are consumed by different test targets"
)]

pub const SESSION_ID: &str = "session-synthetic-1";
pub const OTHER_SESSION_ID: &str = "session-synthetic-2";
pub const MESSAGE_ID: &str = "message-synthetic-1";
pub const OTHER_MESSAGE_ID: &str = "message-synthetic-2";
pub const MODEL: &str = "claude-synthetic-model";
pub const RESOLVED_MODEL: &str = "claude-synthetic-model-resolved";
pub const OTHER_RESOLVED_MODEL: &str = "claude-synthetic-model-other";
pub const CREDENTIAL_PREFIX_RESOLVED_MODEL: &str = "claude-synthetic-model-resolved-api_";
pub const MODEL_MARKER_HELD_WORD: &str = "key";
pub const MODEL_MARKER_RELEASED_TAIL: &str = " is a poor name for it.";
pub const MODEL_CREDENTIAL_PREFIX: &str = "api_";
pub const MODEL_CREDENTIAL_CONTINUATION: &str = "key=synthetic-model-continuation";
pub const MODEL_RECONSTRUCTED_CREDENTIAL: &str = "api_key=synthetic-model-continuation";
pub const CREDENTIAL_SHAPED_SESSION_ID: &str = "api_key=synthetic-session-secret";
pub const CREDENTIAL_SHAPED_MODEL: &str = "secret=synthetic-model-secret";
pub const ANSWER: &str = "synthetic completion";
pub const FILE_DELIVERED_CREDENTIAL: &str = "violet-circuit-synthetic-value:";
pub const SAFE_CREDENTIAL_PREFIX: &str = "API";
pub const REFUSAL: &str = "synthetic refusal";
pub const SENSITIVE_TEXT: &str = "Authorization: synthetic-credential-value";
pub const FRAGMENTED_SECRET_PREFIX: &str = "api_";
pub const FRAGMENTED_SECRET_CONTINUATION: &str = "key=synthetic-fragment";
pub const FRAGMENTED_SECRET: &str = "api_key=synthetic-fragment";
pub const CONTROL_SEQUENCE: &str = "\u{1b}[0m";
pub const CONTROL_SEQUENCE_SECRET: &str = "synthetic-control-secret";
pub const CONTROL_SEQUENCE_SECRET_CONTINUATION: &str = "key=synthetic-control-secret";
pub const CONTROL_OBFUSCATED_SECRET: &str = "api_\u{1b}[0mkey=synthetic-control-secret";
pub const DUPLICATE_MEMBER_DETAIL: &str = "duplicate object members";
pub const TOOL_NAME: &str = "synthetic_lookup";
pub const OTHER_TOOL_NAME: &str = "synthetic_other";
pub const TOOL_ID: &str = "toolu_synthetic_1";
pub const OTHER_TOOL_ID: &str = "toolu_synthetic_2";
pub const CREDENTIAL_TOOL_ID_ONE: &str = "api_key=synthetic-tool-one";
pub const CREDENTIAL_TOOL_ID_TWO: &str = "api_key=synthetic-tool-two";
pub const TOOL_ARGUMENTS: &str = r#"{"subject":"synthetic"}"#;
pub const NONCANONICAL_TOOL_ARGUMENTS: &str = r#"{"z":1, "a":2}"#;
pub const SUPPRESSED_TOOL_ARGUMENTS: &str = r#"{"sk-opaque-token-key":"safe"}"#;
pub const FINISH_TOKEN_SECRET: &str = "api_key=synthetic-finish-secret";
pub const ERROR_TOKEN_SECRET: &str = "api_key=synthetic-error-secret";
pub const REASONING_SECRET_PREFIX: &str = "sk-";
pub const REASONING_SECRET_CONTINUATION: &str = "synthetic-reasoning-secret";
pub const REASONING_SECRET: &str = "sk-synthetic-reasoning-secret";
// Cross-field reconstruction probes for the emitted-context chain.
//
// The continuation value is deliberately OPAQUE: it contains no substring from
// `CREDENTIAL_INDICATORS`, so the stateless fast path releases it untouched on
// its own. A redaction that fires on these scenarios therefore proves the
// emitted identifier seeded the lookbehind — the property under test — rather
// than proving only that the value happened to look secret-ish. A marker
// containing a credential word would make these tests pass without the seam
// they exist to cover.
pub const OPAQUE_CREDENTIAL_CONTINUATION: &str = "key=zqx7vn4m2p";
pub const CREDENTIAL_PREFIX_MESSAGE_ID: &str = "message-synthetic-api_";
pub const MESSAGE_ID_RECONSTRUCTED_CREDENTIAL: &str = "message-synthetic-api_key=zqx7vn4m2p";
pub const CREDENTIAL_PREFIX_TOOL_ID: &str = "toolu_synthetic_api_";
pub const TOOL_ID_RECONSTRUCTED_CREDENTIAL: &str = "toolu_synthetic_api_key=zqx7vn4m2p";
/// The version the adapter's build script derives from the pin manifest, read
/// here from the same build-script variable the adapter's own constant reads.
/// The fake CLI reports it in its `system/init` handshake so a Renovate pin
/// bump moves the offline handshake with the manifest; a literal here would
/// make the decoder reject every scripted success on the next bump, exactly
/// the hand-maintained second copy the derivation exists to remove.
pub const SUPPORTED_VERSION: &str = env!("SIGNALBOX_CLAUDE_CLI_VERSION");

/// A version no manifest pin can carry — the exact-pin gate rejects a
/// prerelease suffix — so the drift scenario stays a mismatch whatever the
/// manifest pins.
pub const DRIFTED_VERSION: &str = "0.0.0-synthetic-drift";

pub const INPUT_TOKENS: u64 = 11;
pub const OUTPUT_TOKENS: u64 = 7;
pub const CACHE_CREATION_TOKENS: u64 = 2;
pub const CACHE_READ_TOKENS: u64 = 3;

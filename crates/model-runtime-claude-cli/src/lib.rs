//! Claude Code subscription adapter for the Layer-1 model runtime specified in
//! `docs/spec/runtime-substrate.md`.
//!
//! One prepared operation becomes one fresh `claude --print --verbose
//! --output-format=stream-json` process. Process spawn is this adapter's
//! irrevocable-dispatch boundary: preparation performs no spawn, execution
//! never respawns, and a process that ends without definitive typed Claude
//! terminal evidence is never completion.
//!
//! Claude Code owns subscription authentication. This crate invokes the binary
//! and neither locates nor reads its credential store. Provider-controlled
//! output is sanitized for credential-shaped material before it crosses the
//! adapter boundary.

#[allow(dead_code)]
mod bridge;
mod config;
mod event;
mod runtime;
mod status;
mod translate;
mod wire;

pub use config::ClaudeCliConfig;
pub use runtime::{
    ClaudeCliConstructionError, ClaudeCliPreparedRequest, ClaudeCliRuntime,
    DISABLED_CLAUDE_CLI_BUILTIN_TOOLS, SUPPORTED_CLAUDE_CLI_VERSION, validate_model_settings,
};

/// Why a provider-neutral tool catalog could not become the exact MCP support
/// document supplied to Claude Code.
#[derive(Debug)]
pub enum ClaudeCliMcpCatalogError {
    /// Two declarations use the same MCP tool name.
    DuplicateToolName,
    /// The declarations cannot be represented by the Claude MCP adapter.
    Unsupported(signalbox_model_runtime::PreparationFailure),
    /// Adapter-owned translation reached a defective state.
    Defect(signalbox_model_runtime::PreparationDefect),
    /// The translated catalog could not be serialized.
    Serialization(serde_json::Error),
}

impl std::fmt::Display for ClaudeCliMcpCatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateToolName => {
                formatter.write_str("Claude MCP catalog contains a duplicate tool name")
            }
            Self::Unsupported(_) => formatter.write_str("Claude MCP catalog is unsupported"),
            Self::Defect(_) => formatter.write_str("Claude MCP catalog translation is defective"),
            Self::Serialization(_) => {
                formatter.write_str("Claude MCP catalog serialization failed")
            }
        }
    }
}

impl std::error::Error for ClaudeCliMcpCatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialization(error) => Some(error),
            Self::DuplicateToolName | Self::Unsupported(_) | Self::Defect(_) => None,
        }
    }
}

/// Translates provider-neutral tool declarations into the exact MCP catalog
/// bytes written into a prepared Claude CLI request's support directory.
///
/// This offline projection lets callers assert adapter-to-bridge conformance
/// without spawning Claude Code or touching subscription credentials.
pub fn serialize_mcp_catalog(
    tools: &[signalbox_model_runtime::ToolDefinition],
) -> Result<Vec<u8>, ClaudeCliMcpCatalogError> {
    let unique_names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if unique_names.len() != tools.len() {
        return Err(ClaudeCliMcpCatalogError::DuplicateToolName);
    }
    let catalog = translate::tool_catalog(tools).map_err(|error| match error {
        translate::TranslationError::Failure(failure) => {
            ClaudeCliMcpCatalogError::Unsupported(failure)
        }
        translate::TranslationError::Defect(defect) => ClaudeCliMcpCatalogError::Defect(defect),
    })?;
    serialize_catalog(&catalog).map_err(ClaudeCliMcpCatalogError::Serialization)
}

fn serialize_catalog(catalog: &bridge::Catalog) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTHETIC_TOOL_NAME: &str = "synthetic_tool";
    const SYNTHETIC_TOOL_DESCRIPTION: &str = "Synthetic tool";

    fn synthetic_tool() -> signalbox_model_runtime::ToolDefinition {
        signalbox_model_runtime::ToolDefinition::with_schema(
            SYNTHETIC_TOOL_NAME,
            SYNTHETIC_TOOL_DESCRIPTION,
            serde_json::json!({"type": "object"}),
        )
    }

    #[test]
    fn mcp_catalog_serialization_rejects_duplicate_tool_names() {
        let tools = vec![synthetic_tool(), synthetic_tool()];

        assert!(serialize_mcp_catalog(&tools).is_err());
    }
}

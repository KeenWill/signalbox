//! `repository_read_file` registry declaration and typed arguments.

use std::fmt;

use schemars::JsonSchema;
use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::ToolContract;

use super::{
    CodeHostOperation,
    arguments::{
        CodeHostFilePath, CodeHostLineNumber, CodeHostRepository, CodeHostRevision,
        InvalidCodeHostArguments, decode as decode_arguments,
    },
};

/// Registry declaration effect posture: read-only, `Auto`, and
/// `ExternalEffect` because GitHub observes the authenticated request.
pub(super) const NAME: &str = "repository_read_file";
pub(super) const DESCRIPTION: &str = "Reads up to 64 KiB of UTF-8 text from one repository file at a required exact revision. Ranged reads inspect at most 1 MiB and report when that bound makes the range unavailable.";

/// One inclusive, positive line range.
#[derive(Clone, Copy, Debug, Eq, PartialEq, JsonSchema)]
#[schemars(rename = "RepositoryLineRange")]
#[schemars(deny_unknown_fields)]
pub struct RepositoryLineRange {
    /// First one-based line to return.
    start: CodeHostLineNumber,
    /// Last one-based line to return, inclusive.
    end: CodeHostLineNumber,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryLineRangeWire {
    start: CodeHostLineNumber,
    end: CodeHostLineNumber,
}

impl<'de> serde::Deserialize<'de> for RepositoryLineRange {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let wire = RepositoryLineRangeWire::deserialize(deserializer)?;
        Self::try_new(wire.start, wire.end)
            .ok_or_else(|| serde::de::Error::custom(InvalidRepositoryLineRange))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidRepositoryLineRange;

impl fmt::Display for InvalidRepositoryLineRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("repository line range is invalid")
    }
}

impl RepositoryLineRange {
    /// Checks one inclusive, positive line range.
    const fn try_new(start: CodeHostLineNumber, end: CodeHostLineNumber) -> Option<Self> {
        if start.get() <= end.get() {
            Some(Self { start, end })
        } else {
            None
        }
    }

    /// Returns the first one-based requested line.
    pub const fn start(self) -> u32 {
        self.start.get()
    }

    /// Returns the last one-based requested line, inclusive.
    pub const fn end(self) -> u32 {
        self.end.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryReadFileArguments {
    /// Exact owner/repository spelling.
    repository: CodeHostRepository,
    /// Exact repository-relative path; `.` addresses the repository root.
    path: CodeHostFilePath,
    /// Required exact lowercase 40-hex commit revision.
    revision: CodeHostRevision,
    /// Optional inclusive one-based line range.
    line_range: Option<RepositoryLineRange>,
}

pub(super) struct Contract;

impl ToolContract for Contract {
    type Arguments = RepositoryReadFileArguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = DESCRIPTION;
}

impl RepositoryReadFileArguments {
    /// Borrows the exact repository selector.
    pub fn repository(&self) -> &CodeHostRepository {
        &self.repository
    }

    /// Borrows the exact repository-relative path.
    pub fn path(&self) -> &CodeHostFilePath {
        &self.path
    }

    /// Borrows the required exact revision.
    pub fn revision(&self) -> &CodeHostRevision {
        &self.revision
    }

    /// Returns the optional inclusive requested line range.
    pub const fn line_range(&self) -> Option<RepositoryLineRange> {
        self.line_range
    }
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
) -> Result<CodeHostOperation, InvalidCodeHostArguments> {
    decode_arguments::<RepositoryReadFileArguments>(arguments).map(CodeHostOperation::ReadFile)
}

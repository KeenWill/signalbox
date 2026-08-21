//! Checked code-host argument primitives shared by tool declarations.

use std::{borrow::Cow, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::DeserializeOwned;
use signalbox_domain::NormalizedToolArguments;

// numeric-bound: tunable - the repository spelling this tool advertises accepting
pub(super) const MAX_REPOSITORY_BYTES: usize = 256;
// numeric-bound: tunable - the file path length this tool advertises accepting
pub(super) const MAX_FILE_PATH_BYTES: usize = 4 * 1024;
// numeric-bound: tunable - the comment body this tool advertises accepting
pub(super) const MAX_COMMENT_BODY_BYTES: usize = 64 * 1024;
// numeric-bound: tunable - the opaque identifier this tool advertises accepting
pub(super) const MAX_OPAQUE_ID_BYTES: usize = 512;
// numeric-bound: tunable - the pagination cursor this tool advertises accepting
pub(super) const MAX_CURSOR_BYTES: usize = 512;

/// A code-host argument value did not satisfy its checked representation.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCodeHostArguments;

impl fmt::Display for InvalidCodeHostArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid code-host arguments")
    }
}

pub(super) fn decode<Arguments: DeserializeOwned>(
    arguments: &NormalizedToolArguments,
) -> Result<Arguments, InvalidCodeHostArguments> {
    serde_json::from_str(arguments.as_str()).map_err(|_| InvalidCodeHostArguments)
}

/// One checked GitHub repository spelling.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CodeHostRepository {
    value: String,
    owner_end: usize,
}

impl CodeHostRepository {
    pub(super) fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        if value.is_empty()
            || value.len() > MAX_REPOSITORY_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(InvalidCodeHostArguments);
        }
        let mut separators = value.match_indices('/');
        let Some((owner_end, _)) = separators.next() else {
            return Err(InvalidCodeHostArguments);
        };
        if separators.next().is_some() {
            return Err(InvalidCodeHostArguments);
        }
        let owner = &value[..owner_end];
        let repository = &value[owner_end + 1..];
        if !valid_repository_segment(owner) || !valid_repository_segment(repository) {
            return Err(InvalidCodeHostArguments);
        }
        Ok(Self { value, owner_end })
    }

    /// Borrows the canonical `owner/repository` spelling.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub(super) fn owner(&self) -> &str {
        &self.value[..self.owner_end]
    }

    pub(super) fn name(&self) -> &str {
        &self.value[self.owner_end + 1..]
    }
}

fn valid_repository_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl TryFrom<String> for CodeHostRepository {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl JsonSchema for CodeHostRepository {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostRepository")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "maxLength": MAX_REPOSITORY_BYTES,
            "pattern": r"^(?:[A-Za-z0-9_-]|\.[A-Za-z0-9_-]|\.\.[A-Za-z0-9._-])[A-Za-z0-9._-]*/(?:[A-Za-z0-9_-]|\.[A-Za-z0-9_-]|\.\.[A-Za-z0-9._-])[A-Za-z0-9._-]*$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// One positive change-request number representable by GitHub GraphQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "u64")]
pub struct CodeHostChangeRequestNumber(u32);

impl CodeHostChangeRequestNumber {
    pub(super) fn try_new(value: u64) -> Result<Self, InvalidCodeHostArguments> {
        let value = u32::try_from(value).map_err(|_| InvalidCodeHostArguments)?;
        (value > 0 && value <= i32::MAX as u32)
            .then_some(Self(value))
            .ok_or(InvalidCodeHostArguments)
    }

    /// Returns the positive change-request number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u64> for CodeHostChangeRequestNumber {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl JsonSchema for CodeHostChangeRequestNumber {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostChangeRequestNumber")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer",
            "minimum": 1,
            "maximum": i32::MAX,
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// One positive GitHub REST identity representable by JSON and `u64`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "u64")]
pub(super) struct CodeHostPositiveId(u64);

impl CodeHostPositiveId {
    pub(super) const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for CodeHostPositiveId {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        (value > 0)
            .then_some(Self(value))
            .ok_or(InvalidCodeHostArguments)
    }
}

impl JsonSchema for CodeHostPositiveId {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostPositiveId")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer",
            "minimum": 1,
            "maximum": u64::MAX,
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// One positive one-based repository line number.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "u64")]
pub(super) struct CodeHostLineNumber(u32);

impl CodeHostLineNumber {
    pub(super) const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u64> for CodeHostLineNumber {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        let value = u32::try_from(value).map_err(|_| InvalidCodeHostArguments)?;
        (value > 0)
            .then_some(Self(value))
            .ok_or(InvalidCodeHostArguments)
    }
}

impl JsonSchema for CodeHostLineNumber {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostLineNumber")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer",
            "minimum": 1,
            "maximum": u32::MAX,
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// Whether a value is one exact lowercase 40-hex revision. Returned revisions
/// are admitted by this same predicate so a result can always be passed back
/// as a revision argument.
pub(super) fn valid_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One exact lowercase 40-hex revision.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CodeHostRevision(String);

impl CodeHostRevision {
    pub(super) fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        valid_revision(&value)
            .then_some(Self(value))
            .ok_or(InvalidCodeHostArguments)
    }

    /// Borrows the exact revision.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CodeHostRevision {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl JsonSchema for CodeHostRevision {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostRevision")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 40,
            "maxLength": 40,
            "pattern": "^[0-9a-f]{40}$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// One checked repository-relative file path.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CodeHostFilePath(String);

impl CodeHostFilePath {
    pub(super) fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        (!value.is_empty()
            && value.len() <= MAX_FILE_PATH_BYTES
            && !value.contains('\0')
            && (value == "."
                || value.split('/').all(|component| {
                    !component.is_empty() && component != "." && component != ".."
                })))
        .then_some(Self(value))
        .ok_or(InvalidCodeHostArguments)
    }

    /// Borrows the exact repository-relative path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CodeHostFilePath {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl JsonSchema for CodeHostFilePath {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostFilePath")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_FILE_PATH_BYTES,
            "pattern": r"^(?:\.|(?:[^./\u0000][^/\u0000]*|\.[^./\u0000][^/\u0000]*|\.\.[^/\u0000]+)(?:/(?:[^./\u0000][^/\u0000]*|\.[^./\u0000][^/\u0000]*|\.\.[^/\u0000]+))*)$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// One checked repository-relative path that identifies a file, not the root.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
pub(super) struct CodeHostChangedFilePath(CodeHostFilePath);

impl CodeHostChangedFilePath {
    fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        let path = CodeHostFilePath::try_new(value)?;
        (path.as_str() != ".")
            .then_some(Self(path))
            .ok_or(InvalidCodeHostArguments)
    }

    pub(super) const fn as_file_path(&self) -> &CodeHostFilePath {
        &self.0
    }
}

impl TryFrom<String> for CodeHostChangedFilePath {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl JsonSchema for CodeHostChangedFilePath {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostChangedFilePath")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_FILE_PATH_BYTES,
            "pattern": r"^(?:[^./\u0000][^/\u0000]*|\.[^./\u0000][^/\u0000]*|\.\.[^/\u0000]+)(?:/(?:[^./\u0000][^/\u0000]*|\.[^./\u0000][^/\u0000]*|\.\.[^/\u0000]+))*$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// One checked nonempty comment body.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CodeHostCommentBody(String);

impl CodeHostCommentBody {
    pub(super) fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        (!value.is_empty() && value.len() <= MAX_COMMENT_BODY_BYTES && !value.contains('\0'))
            .then_some(Self(value))
            .ok_or(InvalidCodeHostArguments)
    }

    /// Borrows the exact comment text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CodeHostCommentBody {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl JsonSchema for CodeHostCommentBody {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostCommentBody")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_COMMENT_BODY_BYTES,
            "pattern": r"^[^\u0000]+$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// Whether a value is one bounded opaque GraphQL node identity. Returned node
/// identities are admitted by this same predicate so a result can always be
/// passed back as an opaque identity argument.
pub(super) fn valid_opaque_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_OPAQUE_ID_BYTES && !value.chars().any(char::is_control)
}

/// One bounded opaque GraphQL node identity.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CodeHostOpaqueId(String);

impl CodeHostOpaqueId {
    pub(super) fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        valid_opaque_id(&value)
            .then_some(Self(value))
            .ok_or(InvalidCodeHostArguments)
    }

    /// Borrows the opaque identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CodeHostOpaqueId {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl JsonSchema for CodeHostOpaqueId {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostOpaqueId")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_OPAQUE_ID_BYTES,
            "pattern": r"^[^\u0000-\u001F\u007F-\u009F]+$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// One bounded opaque GraphQL pagination cursor.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct CodeHostCursor(String);

impl CodeHostCursor {
    pub(super) fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        valid_cursor(&value)
            .then_some(Self(value))
            .ok_or(InvalidCodeHostArguments)
    }

    /// Borrows the opaque cursor exactly as the code host returned it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CodeHostCursor {
    type Error = InvalidCodeHostArguments;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl JsonSchema for CodeHostCursor {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostCursor")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_CURSOR_BYTES,
            "pattern": r"^[^\u0000-\u001F\u007F-\u009F]+$",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

pub(super) fn valid_cursor(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_CURSOR_BYTES && !value.chars().any(char::is_control)
}

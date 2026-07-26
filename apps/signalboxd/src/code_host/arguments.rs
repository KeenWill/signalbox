//! Checked code-host argument primitives shared by tool declarations.

use signalbox_domain::NormalizedToolArguments;

pub(super) const MAX_REPOSITORY_BYTES: usize = 256;
pub(super) const MAX_FILE_PATH_BYTES: usize = 4 * 1024;
pub(super) const MAX_COMMENT_BODY_BYTES: usize = 64 * 1024;
pub(super) const MAX_OPAQUE_ID_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InvalidCodeHostArguments;

pub(super) fn object(
    arguments: &NormalizedToolArguments,
    expected_members: usize,
) -> Result<serde_json::Map<String, serde_json::Value>, InvalidCodeHostArguments> {
    let serde_json::Value::Object(object) =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidCodeHostArguments)?
    else {
        return Err(InvalidCodeHostArguments);
    };
    (object.len() == expected_members)
        .then_some(object)
        .ok_or(InvalidCodeHostArguments)
}

pub(super) fn take_string(
    object: &mut serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<String, InvalidCodeHostArguments> {
    object
        .remove(member)
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or(InvalidCodeHostArguments)
}

pub(super) fn take_positive_id(
    object: &mut serde_json::Map<String, serde_json::Value>,
    member: &str,
) -> Result<u64, InvalidCodeHostArguments> {
    object
        .remove(member)
        .and_then(|value| value.as_u64())
        .filter(|value| *value > 0)
        .ok_or(InvalidCodeHostArguments)
}

/// One checked GitHub repository spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
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

/// One positive change-request number representable by GitHub GraphQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// One exact lowercase 40-hex revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeHostRevision(String);

impl CodeHostRevision {
    pub(super) fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        (value.len() == 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then_some(Self(value))
        .ok_or(InvalidCodeHostArguments)
    }

    /// Borrows the exact revision.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One checked repository-relative file path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeHostFilePath(String);

impl CodeHostFilePath {
    pub(super) fn try_new(value: String) -> Result<Self, InvalidCodeHostArguments> {
        (!value.is_empty()
            && value.len() <= MAX_FILE_PATH_BYTES
            && !value.contains('\0')
            && !value.starts_with('/'))
        .then_some(Self(value))
        .ok_or(InvalidCodeHostArguments)
    }

    /// Borrows the exact repository-relative path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One checked nonempty comment body.
#[derive(Clone, Debug, Eq, PartialEq)]
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

/// Whether a value is one bounded opaque GraphQL node identity. Returned node
/// identities are admitted by this same predicate so a result can always be
/// passed back as an opaque identity argument.
pub(super) fn valid_opaque_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_OPAQUE_ID_BYTES && !value.chars().any(char::is_control)
}

/// One bounded opaque GraphQL node identity.
#[derive(Clone, Debug, Eq, PartialEq)]
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

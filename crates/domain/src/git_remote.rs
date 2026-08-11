//! Operator-minted Git push destinations and their durable withdrawal.

use std::error::Error;
use std::fmt;

/// Longest admitted remote name in bytes.
const MAX_REMOTE_NAME_BYTES: usize = 255;

/// Longest admitted destination URL in bytes.
const MAX_REMOTE_URL_BYTES: usize = 4096;

/// Longest admitted workspace root in bytes.
const MAX_WORKSPACE_ROOT_BYTES: usize = 4096;

/// The only admitted destination scheme.
const REQUIRED_URL_SCHEME: &str = "https://";

/// Why one Git remote text value was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitRemoteTextError {
    /// The value carried no bytes.
    Empty,
    /// The value carried an interior NUL.
    ContainsNull,
    /// The value exceeded its byte bound.
    TooLong {
        /// Byte length of the refused value.
        bytes: usize,
        /// Largest admitted byte length.
        maximum: usize,
    },
    /// The value carried a byte outside its admitted shape.
    Malformed,
    /// The destination did not name the required https scheme.
    UnsupportedScheme,
    /// The workspace root was not an absolute path.
    NotAbsolute,
}

impl fmt::Display for GitRemoteTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("Git remote text was empty"),
            Self::ContainsNull => formatter.write_str("Git remote text carried an interior NUL"),
            Self::TooLong { bytes, maximum } => write!(
                formatter,
                "Git remote text used {bytes} bytes against a {maximum} byte bound"
            ),
            Self::Malformed => formatter.write_str("Git remote text was malformed"),
            Self::UnsupportedScheme => {
                formatter.write_str("Git remote destination was not an https URL")
            }
            Self::NotAbsolute => formatter.write_str("Git remote workspace root was not absolute"),
        }
    }
}

impl Error for GitRemoteTextError {}

fn validate_text(value: &str, maximum: usize) -> Result<(), GitRemoteTextError> {
    if value.is_empty() {
        return Err(GitRemoteTextError::Empty);
    }
    if value.contains('\0') {
        return Err(GitRemoteTextError::ContainsNull);
    }
    if value.len() > maximum {
        return Err(GitRemoteTextError::TooLong {
            bytes: value.len(),
            maximum,
        });
    }
    Ok(())
}

/// One stable operator-chosen remote name.
///
/// The admitted shape is deliberately narrower than Git's own reference
/// grammar so that one durable row, one Git reference component, and one
/// command argument all admit exactly the same values.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitRemoteName(String);

impl GitRemoteName {
    /// Admits one bounded alphanumeric, dot, dash, or underscore name.
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError> {
        validate_text(&value, MAX_REMOTE_NAME_BYTES)?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(GitRemoteTextError::Malformed);
        }
        Ok(Self(value))
    }

    /// Borrows the remote name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owned remote name.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// One exact https destination for a minted remote.
///
/// The value is never rendered by [`fmt::Debug`] because a destination may
/// carry a deployment-identifying host or path.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitRemoteUrl(String);

impl GitRemoteUrl {
    /// Admits one bounded https URL without control or space bytes.
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError> {
        validate_text(&value, MAX_REMOTE_URL_BYTES)?;
        if !value.starts_with(REQUIRED_URL_SCHEME) {
            return Err(GitRemoteTextError::UnsupportedScheme);
        }
        if value.len() == REQUIRED_URL_SCHEME.len() {
            return Err(GitRemoteTextError::Malformed);
        }
        if value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(GitRemoteTextError::Malformed);
        }
        Ok(Self(value))
    }

    /// Borrows the destination URL.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owned destination URL.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for GitRemoteUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("GitRemoteUrl")
            .field(&"[MINTED]")
            .finish()
    }
}

/// One absolute workspace root that scopes a minted remote.
///
/// A workspace has no durable record in this domain, so the daemon's pinned
/// canonical root is the identity a minted destination is scoped by.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitRemoteWorkspaceRoot(String);

impl GitRemoteWorkspaceRoot {
    /// Admits one bounded absolute path without control bytes.
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError> {
        validate_text(&value, MAX_WORKSPACE_ROOT_BYTES)?;
        if !value.starts_with('/') {
            return Err(GitRemoteTextError::NotAbsolute);
        }
        if value.chars().any(char::is_control) {
            return Err(GitRemoteTextError::Malformed);
        }
        Ok(Self(value))
    }

    /// Borrows the workspace root.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owned workspace root.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// One live operator-minted destination resolved for a workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredGitRemoteRecord {
    mint: crate::GitRemoteMintId,
    workspace_root: GitRemoteWorkspaceRoot,
    name: GitRemoteName,
    url: GitRemoteUrl,
}

impl ConfiguredGitRemoteRecord {
    /// Binds one durable mint identity to its scoped destination.
    pub const fn new(
        mint: crate::GitRemoteMintId,
        workspace_root: GitRemoteWorkspaceRoot,
        name: GitRemoteName,
        url: GitRemoteUrl,
    ) -> Self {
        Self {
            mint,
            workspace_root,
            name,
            url,
        }
    }

    /// Returns the durable mint identity.
    pub const fn mint(&self) -> crate::GitRemoteMintId {
        self.mint
    }

    /// Borrows the workspace root this destination is scoped to.
    pub const fn workspace_root(&self) -> &GitRemoteWorkspaceRoot {
        &self.workspace_root
    }

    /// Borrows the operator-chosen remote name.
    pub const fn name(&self) -> &GitRemoteName {
        &self.name
    }

    /// Borrows the exact https destination.
    pub const fn url(&self) -> &GitRemoteUrl {
        &self.url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str = "/srv/signalbox/workspace";
    const NAME: &str = "origin";
    const URL: &str = "https://example.test/namespace/project.git";

    #[test]
    fn a_bounded_alphanumeric_name_is_admitted() {
        let name = GitRemoteName::try_new(NAME.to_owned()).expect("name is admitted");

        assert_eq!(name.as_str(), NAME);
    }

    #[test]
    fn a_name_carrying_a_path_separator_is_refused() {
        assert_eq!(
            GitRemoteName::try_new("namespace/origin".to_owned()),
            Err(GitRemoteTextError::Malformed)
        );
    }

    #[test]
    fn an_empty_name_is_refused() {
        assert_eq!(
            GitRemoteName::try_new(String::new()),
            Err(GitRemoteTextError::Empty)
        );
    }

    #[test]
    fn an_https_destination_is_admitted() {
        let url = GitRemoteUrl::try_new(URL.to_owned()).expect("destination is admitted");

        assert_eq!(url.as_str(), URL);
    }

    #[test]
    fn a_non_https_destination_is_refused() {
        assert_eq!(
            GitRemoteUrl::try_new("git@example.test:namespace/project.git".to_owned()),
            Err(GitRemoteTextError::UnsupportedScheme)
        );
        assert_eq!(
            GitRemoteUrl::try_new("http://example.test/project.git".to_owned()),
            Err(GitRemoteTextError::UnsupportedScheme)
        );
        assert_eq!(
            GitRemoteUrl::try_new("ssh://example.test/project.git".to_owned()),
            Err(GitRemoteTextError::UnsupportedScheme)
        );
    }

    #[test]
    fn a_scheme_without_a_destination_is_refused() {
        assert_eq!(
            GitRemoteUrl::try_new("https://".to_owned()),
            Err(GitRemoteTextError::Malformed)
        );
    }

    #[test]
    fn a_destination_carrying_whitespace_is_refused() {
        assert_eq!(
            GitRemoteUrl::try_new("https://example.test/a project.git".to_owned()),
            Err(GitRemoteTextError::Malformed)
        );
    }

    #[test]
    fn a_destination_never_renders_through_debug() {
        let url = GitRemoteUrl::try_new(URL.to_owned()).expect("destination is admitted");

        let rendered = format!("{url:?}");

        assert!(!rendered.contains("example.test"));
        assert!(rendered.contains("[MINTED]"));
    }

    #[test]
    fn an_absolute_workspace_root_is_admitted() {
        let root = GitRemoteWorkspaceRoot::try_new(WORKSPACE.to_owned()).expect("root is admitted");

        assert_eq!(root.as_str(), WORKSPACE);
    }

    #[test]
    fn a_relative_workspace_root_is_refused() {
        assert_eq!(
            GitRemoteWorkspaceRoot::try_new("workspace".to_owned()),
            Err(GitRemoteTextError::NotAbsolute)
        );
    }
}

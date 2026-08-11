//! Operator-minted Git push destinations and their durable withdrawal.

use std::error::Error;
use std::fmt;

/// Longest admitted remote name in bytes.
///
/// The push executor's `ConfiguredGitRemote` bounds the same name, so both
/// sides read this single definition. A durable mint the executor then refused
/// would be resolvable and unusable, which is what independent copies of this
/// literal would eventually produce.
const MAX_GIT_REMOTE_NAME_BYTES: usize = 255;

/// Longest admitted destination URL in bytes.
///
/// Shared with the push executor for the same reason as
/// [`MAX_GIT_REMOTE_NAME_BYTES`].
const MAX_GIT_REMOTE_URL_BYTES: usize = 4096;

/// Longest admitted remote name in bytes, for the push executor that must
/// accept every name a mint durably holds.
pub const fn max_git_remote_name_bytes() -> usize {
    MAX_GIT_REMOTE_NAME_BYTES
}

/// Longest admitted destination URL in bytes, for the push executor that must
/// accept every destination a mint durably holds.
pub const fn max_git_remote_url_bytes() -> usize {
    MAX_GIT_REMOTE_URL_BYTES
}

/// Longest admitted workspace root in bytes.
///
/// The bound is smaller than a platform `PATH_MAX` because the durable record
/// indexes the root together with the remote name, and a PostgreSQL B-tree
/// index tuple may not exceed roughly a third of an 8 KiB page. A longer bound
/// would let an otherwise valid mint fail on index insertion rather than here.
const MAX_WORKSPACE_ROOT_BYTES: usize = 1024;

/// Reference component suffix Git refuses.
const REFUSED_NAME_SUFFIX: &str = ".lock";

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

/// Returns whether one URL authority names a host an HTTPS transport could
/// dispatch to.
///
/// The admitted shape mirrors the SQL predicate byte for byte: an optional
/// `userinfo@`, then a host of unreserved name bytes, then an optional numeric
/// port. A scheme-only destination such as `https://?` carries an empty
/// authority and is refused here.
///
/// Accepted cost: a bracketed IP-literal host is refused outright rather than
/// parsed. Admitting one would need a real IPv6 parser on this side and an
/// equivalent one in the SQL predicate, and a POSIX regular expression cannot
/// express that grammar — so the two sides would agree only by accident. A
/// destination is expected to name a host, and this bound is stated rather
/// than approximated.
fn authority_names_a_host(authority: &str) -> bool {
    let host_and_port = authority
        .split_once('@')
        .map_or(authority, |(_, remainder)| remainder);
    let (host, port) = host_and_port
        .split_once(':')
        .map_or((host_and_port, None), |(host, port)| (host, Some(port)));
    !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'~'))
        && port.is_none_or(port_is_numeric)
}

/// Longest admitted port, in digits.
///
/// The bound exists so this rule and the SQL predicate admit the same strings.
/// A zero-padded port such as `000001` parses to a legal `u16` while the
/// predicate's five-digit grammar refuses it, which would let an operator mint
/// a destination the durable `CHECK` then rejects.
const MAX_PORT_DIGITS: usize = 5;

/// Returns whether one port is a run of digits inside the TCP range.
///
/// A port above 65535 names no TCP endpoint, so a destination carrying one
/// could never be dispatched however well-formed its host is. The parse bounds
/// the value; the digit test keeps a leading sign out, which `u16::from_str`
/// would otherwise accept.
fn port_is_numeric(port: &str) -> bool {
    !port.is_empty()
        && port.len() <= MAX_PORT_DIGITS
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

/// One stable operator-chosen remote name.
///
/// The admitted shape is deliberately narrower than Git's own reference
/// grammar so that one durable row, one Git reference component, and one
/// command argument all admit exactly the same values.
///
/// Narrower is only sound while it stays a subset. The push executor builds
/// `refs/remotes/<name>/probe` and validates it as a Git reference, so a name
/// this type admitted but that reference grammar refused would mint a durable
/// destination the executor could never resolve. The dot rules below are that
/// grammar's component rules: no leading dot, no `..`, no trailing dot, and no
/// `.lock` suffix.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitRemoteName(String);

impl GitRemoteName {
    /// Admits one bounded alphanumeric, dot, dash, or underscore name that is
    /// also a legal Git reference component.
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError> {
        validate_text(&value, MAX_GIT_REMOTE_NAME_BYTES)?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(GitRemoteTextError::Malformed);
        }
        if value.starts_with('.')
            || value.ends_with('.')
            || value.contains("..")
            || value.ends_with(REFUSED_NAME_SUFFIX)
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
    /// Admits one bounded https URL naming a host, written in printable ASCII.
    ///
    /// The byte test is `is_ascii_graphic` rather than a Unicode whitespace or
    /// control test, because the SQL predicate that restates this rule
    /// classifies under the `C` collation and would otherwise admit values —
    /// `U+00A0` in a path, for one — that this constructor refuses. The
    /// durable store must never hold a destination this type cannot represent,
    /// so both sides judge the same bytes.
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError> {
        validate_text(&value, MAX_GIT_REMOTE_URL_BYTES)?;
        let Some(remainder) = value.strip_prefix(REQUIRED_URL_SCHEME) else {
            return Err(GitRemoteTextError::UnsupportedScheme);
        };
        if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(GitRemoteTextError::Malformed);
        }
        let authority = remainder
            .find(['/', '?', '#'])
            .map_or(remainder, |offset| &remainder[..offset]);
        if !authority_names_a_host(authority) {
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
    /// Admits one bounded absolute path without ASCII control bytes.
    ///
    /// The control test is ASCII-only, matching what the SQL predicate's
    /// `[:cntrl:]` class means under the `C` collation. A path may legitimately
    /// carry non-ASCII bytes, so narrowing to ASCII outright is wrong here;
    /// agreeing on which bytes are control is what keeps the durable store from
    /// holding a root this type would refuse.
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError> {
        validate_text(&value, MAX_WORKSPACE_ROOT_BYTES)?;
        if !value.starts_with('/') {
            return Err(GitRemoteTextError::NotAbsolute);
        }
        if value.bytes().any(|byte| byte.is_ascii_control()) {
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

    #[track_caller]
    fn assert_name_is_refused(candidate: &str) {
        assert_eq!(
            GitRemoteName::try_new(candidate.to_owned()),
            Err(GitRemoteTextError::Malformed)
        );
    }

    #[track_caller]
    fn assert_name_is_admitted(candidate: &str) {
        assert_eq!(
            GitRemoteName::try_new(candidate.to_owned())
                .as_ref()
                .map(GitRemoteName::as_str),
            Ok(candidate)
        );
    }

    #[test]
    fn a_name_git_would_refuse_as_a_reference_component_is_refused() {
        assert_name_is_refused(".origin");
        assert_name_is_refused("origin..backup");
        assert_name_is_refused("origin.");
        assert_name_is_refused("origin.lock");
        assert_name_is_refused("..");
        assert_name_is_refused(".");
    }

    #[test]
    fn a_name_carrying_interior_punctuation_is_admitted() {
        assert_name_is_admitted("up-stream_2");
        assert_name_is_admitted("v1.0");
        assert_name_is_admitted("origin.lockfile");
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

    #[track_caller]
    fn assert_destination_is_refused(candidate: &str) {
        assert_eq!(
            GitRemoteUrl::try_new(candidate.to_owned()),
            Err(GitRemoteTextError::Malformed)
        );
    }

    #[track_caller]
    fn assert_destination_is_admitted(candidate: &str) {
        assert_eq!(
            GitRemoteUrl::try_new(candidate.to_owned())
                .as_ref()
                .map(GitRemoteUrl::as_str),
            Ok(candidate)
        );
    }

    #[test]
    fn a_destination_naming_no_host_is_refused() {
        assert_destination_is_refused("https://?");
        assert_destination_is_refused("https://#fragment");
        assert_destination_is_refused("https:///namespace/project.git");
        assert_destination_is_refused("https://user@/project.git");
        assert_destination_is_refused("https://example.test:/project.git");
        assert_destination_is_refused("https://example.test:https/project.git");
    }

    /// A bracketed IP-literal host is refused rather than parsed, so that the
    /// SQL predicate can restate this rule exactly. `[....]` is the case that
    /// showed a character-only bracket check admits a host no transport could
    /// dispatch.
    #[test]
    fn a_destination_naming_a_bracketed_literal_host_is_refused() {
        assert_destination_is_refused("https://[2001:db8::1]/namespace/project.git");
        assert_destination_is_refused("https://[2001:db8::1]:8443/namespace/project.git");
        assert_destination_is_refused("https://[....]/namespace/project.git");
    }

    /// Unicode whitespace is refused by the same byte test the SQL predicate
    /// applies, so the store cannot hold a destination this type would refuse.
    #[test]
    fn a_destination_carrying_a_non_ascii_byte_is_refused() {
        assert_destination_is_refused("https://example.test/a\u{00a0}project.git");
        assert_destination_is_refused("https://éxample.test/project.git");
    }

    /// A port above 65535 names no TCP endpoint, so a destination carrying one
    /// could never be dispatched however well-formed its host is.
    #[test]
    fn a_destination_naming_a_port_outside_the_transport_range_is_refused() {
        assert_destination_is_refused("https://example.test:65536/project.git");
        assert_destination_is_refused("https://example.test:99999/project.git");
        assert_destination_is_refused("https://example.test:123456/project.git");
    }

    #[test]
    fn a_destination_naming_the_highest_port_is_admitted() {
        assert_destination_is_admitted("https://example.test:65535/project.git");
        assert_destination_is_admitted("https://example.test:00001/project.git");
    }

    /// A zero-padded port beyond five digits parses to a legal `u16`, so only
    /// the digit bound keeps this rule and the SQL predicate in agreement.
    #[test]
    fn a_destination_naming_a_port_beyond_the_digit_bound_is_refused() {
        assert_destination_is_refused("https://example.test:000001/project.git");
        assert_destination_is_refused("https://example.test:0000000001/project.git");
    }

    #[test]
    fn a_destination_naming_a_host_is_admitted() {
        assert_destination_is_admitted("https://example.test:8443/namespace/project.git");
        assert_destination_is_admitted("https://user@example.test/namespace/project.git");
        assert_destination_is_admitted("https://example.test");
    }

    #[test]
    fn a_workspace_root_beyond_the_indexed_bound_is_refused() {
        let root = format!("/{}", "a".repeat(MAX_WORKSPACE_ROOT_BYTES));

        assert_eq!(
            GitRemoteWorkspaceRoot::try_new(root),
            Err(GitRemoteTextError::TooLong {
                bytes: MAX_WORKSPACE_ROOT_BYTES + 1,
                maximum: MAX_WORKSPACE_ROOT_BYTES,
            })
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

//! Operator-minted Git push destinations and their durable withdrawal.

use std::error::Error;
use std::fmt;

use bstr::ByteSlice;

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
    /// Admits one bounded registry token that is also a legal Git reference component.
    pub fn try_new(value: String) -> Result<Self, GitRemoteTextError> {
        validate_text(&value, MAX_GIT_REMOTE_NAME_BYTES)?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || value.contains('/')
            || gix_validate::reference::name_partial(value.as_bytes().as_bstr()).is_err()
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
        if !value.starts_with(REQUIRED_URL_SCHEME) {
            return Err(GitRemoteTextError::UnsupportedScheme);
        }
        if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(GitRemoteTextError::Malformed);
        }
        let parsed = url::Url::parse(&value).map_err(|_| GitRemoteTextError::Malformed)?;
        let authority = value[REQUIRED_URL_SCHEME.len()..]
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if parsed.scheme() != "https"
            || parsed.host().is_none()
            || authority.is_empty()
            || authority.contains('@')
            || authority.ends_with(':')
            || !authority.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'~' | b':')
            })
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.port() == Some(0)
            || explicit_port_is_too_long(&value)
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

fn explicit_port_is_too_long(value: &str) -> bool {
    value[REQUIRED_URL_SCHEME.len()..]
        .split(['/', '?', '#'])
        .next()
        .and_then(|authority| authority.rsplit_once(':'))
        .is_some_and(|(_, port)| port.len() > 5)
}

impl fmt::Debug for GitRemoteUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("GitRemoteUrl")
            .field(&"[MINTED]")
            .finish()
    }
}

/// One live operator-minted destination resolved for a workspace.
///
/// The scope is a [`crate::WorkspaceId`], never a path. A mint keyed by a root
/// path would be keyed by a spelling: `/srv/workspace` and `/srv/workspace/.`
/// name one directory and would carry two independent live destinations under
/// a rule that promised one. The workspace record canonicalizes once, at the
/// moment it is minted, and every scope comparison after that is between
/// identities — so there is no later point at which a normalization could be
/// forgotten.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredGitRemoteRecord {
    mint: crate::GitRemoteMintId,
    workspace: crate::WorkspaceId,
    name: GitRemoteName,
    url: GitRemoteUrl,
}

impl ConfiguredGitRemoteRecord {
    /// Binds one durable mint identity to its scoped destination.
    pub const fn new(
        mint: crate::GitRemoteMintId,
        workspace: crate::WorkspaceId,
        name: GitRemoteName,
        url: GitRemoteUrl,
    ) -> Self {
        Self {
            mint,
            workspace,
            name,
            url,
        }
    }

    /// Returns the durable mint identity.
    pub const fn mint(&self) -> crate::GitRemoteMintId {
        self.mint
    }

    /// Returns the workspace this destination is scoped to.
    pub const fn workspace(&self) -> crate::WorkspaceId {
        self.workspace
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
            Err(GitRemoteTextError::Malformed),
            "candidate: {candidate}"
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
        assert_destination_is_admitted("https://example.test");
        assert_destination_is_admitted("https://1");
    }

    #[test]
    fn a_destination_requiring_url_normalization_is_refused() {
        assert_destination_is_refused("https://example%2etest/repository.git");
        assert_destination_is_refused(r"https://example.test\repository.git");
    }

    /// A query string is the other common credential channel a URL offers, and
    /// the mint column is append-only, so it is refused on the same reasoning
    /// as userinfo. A fragment carries no meaning for a remote either.
    #[test]
    fn a_destination_carrying_a_query_or_fragment_is_refused() {
        assert_destination_is_refused("https://example.test/repository?access_token=secret");
        assert_destination_is_refused("https://example.test/repository?a=1");
        assert_destination_is_refused("https://example.test?a=1");
        assert_destination_is_refused("https://example.test/repository#fragment");
        assert_destination_is_refused("https://example.test/repository?a=1#fragment");
    }

    /// Port zero is reserved and names no listening service, so an explicit
    /// `:0` mints a destination no push could reach.
    #[test]
    fn a_destination_naming_port_zero_is_refused() {
        assert_destination_is_refused("https://example.test:0/repository");
        assert_destination_is_refused("https://example.test:00000/repository");
    }

    /// A minted destination is stored append-only, so userinfo would publish a
    /// credential no later act could remove. The grammar refuses it until the
    /// push credential policy is decided.
    #[test]
    fn a_destination_carrying_userinfo_is_refused() {
        assert_destination_is_refused("https://user@example.test/namespace/project.git");
        assert_destination_is_refused("https://user:token@example.test/namespace/project.git");
        assert_destination_is_refused("https://@example.test/namespace/project.git");
        assert_destination_is_refused("https://user@example.test:8443/namespace/project.git");
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

    /// The record scopes a destination by workspace identity, so two spellings
    /// of one root cannot reach it at all: there is no path on this type to
    /// compare, and the identity is what the durable row is keyed by.
    #[test]
    fn a_record_scopes_its_destination_by_workspace_identity() {
        let mint = crate::GitRemoteMintId::from_uuid(uuid::Uuid::from_u128(1));
        let workspace = crate::WorkspaceId::from_uuid(uuid::Uuid::from_u128(2));
        let name = GitRemoteName::try_new(NAME.to_owned()).expect("name is admitted");
        let url = GitRemoteUrl::try_new(URL.to_owned()).expect("destination is admitted");

        let record = ConfiguredGitRemoteRecord::new(mint, workspace, name, url);

        assert_eq!(record.mint(), mint);
        assert_eq!(record.workspace(), workspace);
        assert_eq!(record.name().as_str(), NAME);
        assert_eq!(record.url().as_str(), URL);
    }
}

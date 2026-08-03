use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use bstr::BStr;
use git2::ObjectFormat;

use crate::layout::parse_full_object_id;

const MAX_REMOTE_NAME_BYTES: usize = 255;
const MAX_REMOTE_URL_BYTES: usize = 4096;

/// Exact deployment-owned remote configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct ConfiguredGitRemote {
    name: String,
    url: String,
}

impl ConfiguredGitRemote {
    /// Constructs one fixed remote name and exact destination URL.
    pub fn try_new(
        name: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<Self, InvalidConfiguredGitRemote> {
        let name = name.into();
        let url = url.into();
        let probe = format!("refs/remotes/{name}/probe");
        if name.is_empty()
            || name.len() > MAX_REMOTE_NAME_BYTES
            || name.contains('/')
            || gix_validate::reference::name(BStr::new(probe.as_bytes())).is_err()
            || url.is_empty()
            || url.len() > MAX_REMOTE_URL_BYTES
            || url.chars().any(char::is_control)
        {
            return Err(InvalidConfiguredGitRemote);
        }
        Ok(Self { name, url })
    }

    /// Borrows the configured Git remote name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrows the exact configured destination URL.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl fmt::Debug for ConfiguredGitRemote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfiguredGitRemote")
            .field("name", &self.name)
            .field("url", &"[CONFIGURED]")
            .finish()
    }
}

/// Deployment remote configuration was not a bounded name and destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidConfiguredGitRemote;

impl fmt::Display for InvalidConfiguredGitRemote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid configured Git remote")
    }
}

impl Error for InvalidConfiguredGitRemote {}

/// One fully resolved push handed to the deployment transport.
#[derive(Clone, Eq, PartialEq)]
pub struct GitPushRequest {
    repository_root: PathBuf,
    remote: ConfiguredGitRemote,
    branch: String,
    commit: String,
}

impl GitPushRequest {
    pub(super) fn new(
        repository_root: PathBuf,
        remote: ConfiguredGitRemote,
        branch: String,
        commit: String,
    ) -> Self {
        Self {
            repository_root,
            remote,
            branch,
            commit,
        }
    }

    /// Borrows the descriptor-pinned direct repository root.
    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    /// Borrows the immutable deployment remote.
    pub const fn remote(&self) -> &ConfiguredGitRemote {
        &self.remote
    }

    /// Borrows the exact local branch shorthand.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Borrows the resolved commit expected at the remote branch.
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// Returns the non-forced refspec with an object-ID source.
    pub fn refspec(&self) -> String {
        format!("{}:refs/heads/{}", self.commit, self.branch)
    }
}

impl fmt::Debug for GitPushRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitPushRequest")
            .field("repository_root", &"[INJECTED]")
            .field("remote", &self.remote)
            .field("branch", &self.branch)
            .field("commit", &self.commit)
            .finish()
    }
}

/// Successful transport acknowledgement for the exact updated commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitPushReceipt {
    commit: String,
}

impl GitPushReceipt {
    /// Constructs a receipt for a full SHA-1 or SHA-256 object identifier.
    pub fn try_new(commit: impl Into<String>) -> Result<Self, InvalidGitPushReceipt> {
        let commit = commit.into();
        let object_id = parse_full_object_id(&commit, ObjectFormat::Sha1)
            .or_else(|| parse_full_object_id(&commit, ObjectFormat::Sha256))
            .ok_or(InvalidGitPushReceipt)?;
        Ok(Self {
            commit: object_id.to_string(),
        })
    }

    /// Borrows the remote commit acknowledgement.
    pub fn commit(&self) -> &str {
        &self.commit
    }
}

/// A push transport returned an invalid acknowledgement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidGitPushReceipt;

impl fmt::Display for InvalidGitPushReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Git push receipt")
    }
}

impl Error for InvalidGitPushReceipt {}

/// Physical push outcome classification supplied by the injected transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitPushTransportFailure {
    /// The configured remote definitively rejected the update.
    Rejected,
    /// Dispatch could not begin.
    PreDispatchInfrastructure,
    /// Dispatch may have updated the remote.
    DispatchUnknown,
}

impl fmt::Display for GitPushTransportFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rejected => "configured Git remote rejected the push",
            Self::PreDispatchInfrastructure => "Git push could not be dispatched",
            Self::DispatchUnknown => "Git push outcome is unknown",
        })
    }
}

impl Error for GitPushTransportFailure {}

/// Deployment-owned push boundary. Implementations receive the fixed remote;
/// the model never supplies or modifies a destination.
pub trait GitPushTransport: Send {
    /// Pushes one non-forced branch refspec and acknowledges the remote commit.
    fn push(&mut self, request: GitPushRequest) -> Result<GitPushReceipt, GitPushTransportFailure>;
}

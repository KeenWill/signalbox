use super::error::HubModelConfigurationError;
use signalbox_model_runtime::{
    CredentialAccess, CredentialAccessError, CredentialAccessFailure, CredentialReference,
    CredentialValue,
};
use std::{
    collections::HashMap,
    fmt, fs, io,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// Credential-file admission ceiling in configuration-and-credentials.
const MAX_CREDENTIAL_FILE_BYTES: u64 = 64 * 1024;

fn credential_io_failure(error: io::Error) -> CredentialAccessFailure {
    if error.kind() == io::ErrorKind::NotFound {
        CredentialAccessFailure::Unavailable
    } else {
        CredentialAccessFailure::Unreadable
    }
}

fn validate_credential_metadata(
    metadata: &fs::Metadata,
    effective_uid: u32,
) -> Result<(), CredentialAccessFailure> {
    if !metadata.is_file() {
        return Err(CredentialAccessFailure::NotRegularFile);
    }
    if metadata.uid() != effective_uid {
        return Err(CredentialAccessFailure::WrongOwner);
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(CredentialAccessFailure::InsecurePermissions);
    }
    if metadata.len() > MAX_CREDENTIAL_FILE_BYTES {
        return Err(CredentialAccessFailure::TooLarge);
    }
    Ok(())
}

fn open_credential_file(path: &Path) -> Result<fs::File, CredentialAccessFailure> {
    // Reject special files before opening; recheck the actual opened target so
    // a path replacement cannot substitute unchecked bytes.
    let effective_uid = rustix::process::geteuid().as_raw();
    validate_credential_metadata(
        &fs::metadata(path).map_err(credential_io_failure)?,
        effective_uid,
    )?;
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| credential_io_failure(error.into()))?;
    let file = fs::File::from(descriptor);
    validate_credential_metadata(
        &file.metadata().map_err(credential_io_failure)?,
        effective_uid,
    )?;
    Ok(file)
}

fn read_credential_file(path: &Path) -> Result<Vec<u8>, CredentialAccessFailure> {
    let mut file = open_credential_file(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_CREDENTIAL_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(credential_io_failure)?;
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(CredentialAccessFailure::TooLarge);
    }
    validate_credential_metadata(
        &file.metadata().map_err(credential_io_failure)?,
        rustix::process::geteuid().as_raw(),
    )?;
    Ok(bytes)
}

fn validate_credential_file(
    path: &Path,
    reference: CredentialReference,
) -> Result<(), CredentialAccessError> {
    open_credential_file(path)
        .map(|_| ())
        .map_err(|failure| CredentialAccessError::new(reference, failure))
}

pub(super) fn credential_file_references_conflict(left: &Path, right: &Path) -> bool {
    left == right || same_file_identity(left, right)
}

#[cfg(unix)]
fn same_file_identity(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &Path, _right: &Path) -> bool {
    false
}

pub(super) fn resolved_credential_file_reference(
    path: &Path,
) -> Result<PathBuf, HubModelConfigurationError> {
    let mut resolved = normalize_absolute_reference(path)?;
    for _ in 0..40 {
        let mut prefix = PathBuf::new();
        let mut components = resolved.components();
        let mut replacement = None;
        while let Some(component) = components.next() {
            prefix.push(component.as_os_str());
            let metadata = match fs::symlink_metadata(&prefix) {
                Ok(metadata) => metadata,
                Err(_) => return Ok(resolved),
            };
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let target = fs::read_link(&prefix)
                .map_err(|_| HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?;
            let mut target = if target.is_absolute() {
                target
            } else {
                prefix
                    .parent()
                    .ok_or(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)?
                    .join(target)
            };
            target.extend(components.map(|remaining| remaining.as_os_str()));
            replacement = Some(normalize_absolute_reference(&target)?);
            break;
        }
        let Some(replacement) = replacement else {
            return Ok(fs::canonicalize(&resolved).unwrap_or(resolved));
        };
        resolved = replacement;
    }
    Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration)
}

/// Resolves the configured Claude MCP bridge reference to one path.
///
/// The bridge is a program this workspace builds and a deployment installs, so
/// unlike the Claude executable it can be named the way an installed program is
/// named. Two spellings are admitted, told apart by whether the configured
/// value is a bare program name — a value equal to its own final path
/// component:
///
/// - a bare name is looked up in `search_path`, the daemon's own `PATH`, and resolves to the first
///   entry holding a regular file of that name this process can execute;
/// - any other value is a path, returned verbatim for the caller's absolute-existing-file rule to
///   judge, so a configured path never resolves through `PATH` to a different program.
///
/// Only absolute search entries participate. A relative entry — including the
/// empty entry POSIX reads as the working directory — is skipped rather than
/// joined, because the resolved path is written into the MCP server
/// configuration Claude Code spawns from a working directory of its own.
pub(super) fn resolved_mcp_bridge_reference(
    value: &str,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, HubModelConfigurationError> {
    let reference = PathBuf::from(value);
    if reference.file_name() != Some(std::ffi::OsStr::new(value)) {
        return Ok(reference);
    }
    absolute_search_entries(search_path)
        .into_iter()
        .map(|entry| entry.join(value))
        .find(|candidate| is_executable_file(candidate))
        .ok_or(HubModelConfigurationError::UnresolvedClaudeMcpBridgeExecutable)
}

/// Absolute directories of one search path, in their configured order.
pub(super) fn absolute_search_entries(search_path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    search_path
        .map(|value| {
            std::env::split_paths(value)
                .filter(|entry| entry.is_absolute())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether this process could execute `path` as a program.
///
/// Both halves are load-bearing. The metadata check rejects anything that is
/// not a regular file, because execute access on a directory means the right
/// to traverse it. The access check asks the kernel about the daemon's own
/// effective credentials rather than reading permission bits, so a file some
/// other user may execute — mode `0o700` owned by another UID, or one an ACL
/// denies — does not satisfy a search entry and shadow a bridge the daemon can
/// actually run in a later one.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        && rustix::fs::accessat(
            rustix::fs::CWD,
            path,
            rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .is_ok()
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn normalize_absolute_reference(path: &Path) -> Result<PathBuf, HubModelConfigurationError> {
    if !path.is_absolute() {
        return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(HubModelConfigurationError::InvalidRepositoryWatchConfiguration);
                }
            }
        }
    }
    Ok(normalized)
}

/// Line-termination bytes a credential file may end with. `gh auth token`,
/// `op read`, `pass`, and a shell redirect all terminate the line they write,
/// so these bytes are how the file ends rather than part of the secret.
const CREDENTIAL_LINE_TERMINATORS: [u8; 2] = *b"\n\r";

/// Narrows the bytes a credential file holds to the credential value itself by
/// dropping only trailing line termination.
///
/// Every other byte is retained exactly, including interior and leading
/// whitespace: only the terminator a writing tool appends is unambiguously not
/// the secret. A file holding nothing but terminators narrows to an empty
/// value, which the adapter boundary then refuses as unusable exactly as an
/// empty file already was.
pub(super) fn credential_bytes(file_bytes: &[u8]) -> &[u8] {
    let end = file_bytes
        .iter()
        .rposition(|byte| !CREDENTIAL_LINE_TERMINATORS.contains(byte))
        .map_or(0, |last_value_byte| last_value_byte.saturating_add(1));
    &file_bytes[..end]
}

/// Resolves deployment-owned token files and GitHub installation profiles.
/// Files are reread at use; App profiles share their installation-token cache.
#[derive(Clone)]
pub struct FileCredentialAccess {
    request_timeout: Option<std::time::Duration>,
    paths: Arc<HashMap<CredentialReference, PathBuf>>,
    app: Option<(
        CredentialReference,
        Arc<signalbox_github_transport::AppAuthentication>,
    )>,
}

impl FileCredentialAccess {
    /// Binds one GitHub profile without reading credentials.
    pub fn from_github(
        profile: &crate::credential_pools::GithubCredentialProfile,
        reference: CredentialReference,
    ) -> Self {
        match profile.delivery() {
            crate::credential_pools::GithubCredentialDelivery::File(path) => {
                Self::new(path.clone(), reference)
            }
            crate::credential_pools::GithubCredentialDelivery::GithubApp { .. } => Self {
                paths: Arc::new(HashMap::new()),
                request_timeout: None,
                app: profile.authentication().map(|app| (reference, app)),
            },
        }
    }
    /// Shared installation authentication for GitHub request transports.
    pub fn github_app(&self) -> Option<Arc<signalbox_github_transport::AppAuthentication>> {
        self.app.as_ref().map(|(_, app)| app.clone())
    }

    /// Applies the caller's HTTP budget to installation-token preparation.
    pub fn with_request_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Checks each configured file's admission without reading its secret bytes.
    pub fn validate(&self) -> Result<(), CredentialAccessError> {
        for (reference, path) in self.paths.iter() {
            validate_credential_file(path, reference.clone())?;
        }
        Ok(())
    }

    /// Binds one non-secret credential reference to one deployment file.
    pub fn new(path: PathBuf, reference: CredentialReference) -> Self {
        Self::from_files([(reference, path)])
    }

    /// Binds a complete set of non-secret credential references to deployment
    /// files. Each resolution selects and rereads only its mapped path.
    pub fn from_files(files: impl IntoIterator<Item = (CredentialReference, PathBuf)>) -> Self {
        Self {
            paths: Arc::new(files.into_iter().collect()),
            request_timeout: None,
            app: None,
        }
    }

    /// Returns the non-secret reference accepted by this source.
    pub fn credential_reference(&self) -> Option<CredentialReference> {
        if let Some((reference, _)) = &self.app {
            return Some(reference.clone());
        }
        (self.paths.len() == 1)
            .then(|| self.paths.keys().next().cloned())
            .flatten()
    }
}

impl fmt::Debug for FileCredentialAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileCredentialAccess")
            .field("paths", &"[credential file map]")
            .field("reference_count", &self.paths.len())
            .finish()
    }
}

impl CredentialAccess for FileCredentialAccess {
    async fn resolve(
        &self,
        reference: &CredentialReference,
    ) -> Result<CredentialValue, CredentialAccessError> {
        if let Some((mapped, app)) = &self.app {
            if mapped != reference {
                return Err(CredentialAccessError::new(
                    reference.clone(),
                    CredentialAccessFailure::Unmapped,
                ));
            }
            let header = app
                .authorization(self.request_timeout)
                .await
                .map_err(|failure| {
                    tracing::warn!(?failure, "GitHub App credential unavailable");
                    CredentialAccessError::new(
                        reference.clone(),
                        CredentialAccessFailure::Unavailable,
                    )
                })?;
            return Ok(CredentialValue::new(&header.as_bytes()[b"Bearer ".len()..]));
        }
        let path = self.paths.get(reference).ok_or_else(|| {
            CredentialAccessError::new(reference.clone(), CredentialAccessFailure::Unmapped)
        })?;
        let path = path.clone();
        let file_bytes = tokio::task::spawn_blocking(move || read_credential_file(&path))
            .await
            .unwrap_or(Err(CredentialAccessFailure::Unreadable))
            .map_err(|failure| CredentialAccessError::new(reference.clone(), failure))?;
        Ok(CredentialValue::new(credential_bytes(&file_bytes)))
    }
}

impl super::HubModelConfiguration {
    /// Checks token-file isolation between the GitHub tools and repository polling.
    pub fn github_tool_credential_conflicts(&self, fallback: &Path) -> bool {
        use crate::credential_pools::GithubCredentialDelivery;
        let path = match self
            .github_credential_profile(signalbox_tools_code_host::CODE_HOST_CREDENTIAL_REFERENCE)
        {
            Some(profile) => match profile.delivery() {
                GithubCredentialDelivery::File(path) => path.as_path(),
                GithubCredentialDelivery::GithubApp { .. } => return false,
            },
            None => fallback,
        };
        self.repository_watch().is_some_and(|watch| {
            watch.repositories().iter().any(|repository| {
                repository.credential_file().is_some_and(|polling| {
                    crate::repo_watch_credentials::credential_files_conflict(path, polling)
                })
            })
        })
    }

    /// Admits model-provider, token, and webhook files; App keys are admitted at use.
    pub fn validate_credential_files(&self) -> Result<(), CredentialAccessError> {
        for profile in self.credential_profiles.values() {
            use crate::credential_pools::CredentialDelivery;
            match profile.delivery() {
                CredentialDelivery::File { path, .. } => {
                    validate_credential_file(path, CredentialReference::new(profile.name()))?
                }
                CredentialDelivery::Ambient
                | CredentialDelivery::Oauth(_)
                | CredentialDelivery::CodexHome { .. } => {}
            }
        }
        if let Some(watch) = self.repository_watch() {
            for repository in watch.repositories() {
                if let Some(path) = repository.credential_file() {
                    validate_credential_file(path, repository.credential_reference())?;
                }
                if let Some(path) = repository.push_credential_file() {
                    validate_credential_file(
                        path,
                        CredentialReference::new(
                            crate::repo_watch_credentials::GIT_PUSH_CREDENTIAL_REFERENCE,
                        ),
                    )?;
                }
                if let Some(webhook) = repository.webhook()
                    && let Some(reference) = repository.webhook_secret_reference()
                {
                    validate_credential_file(webhook.secret_file(), reference)?;
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn read_github_app_key(
    path: &Path,
) -> Result<Vec<u8>, signalbox_github_transport::AppCredentialFailure> {
    read_credential_file(path)
        .map_err(|_| signalbox_github_transport::AppCredentialFailure::KeyUnreadable)
}

#[cfg(test)]
mod tests;

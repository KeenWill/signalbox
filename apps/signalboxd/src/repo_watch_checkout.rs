//! Provisions the repository at the daemon's derived session root.

use crate::{
    daemon_tools::SessionWorkspaceRoots, repo_watch_credentials::RepositoryWatchClientLoader,
};
use rustix::fs::{Mode, OFlags, mkdirat, openat};
use signalbox_domain::{PullRequestEventContext, RepositorySlug, SessionId};
use signalbox_tools_exec::{
    ProcessEnvironment, ProcessOutcome, ProcessRequest, ProcessRunner, ProcessStatusProtocol,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::fd::{AsRawFd, OwnedFd},
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// Closed provisioning steps are safe to persist and log.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CheckoutStep {
    Configuration,
    Credential,
    Workspace,
    Clone,
    Fetch,
    Checkout,
    Verify,
}

impl CheckoutStep {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Credential => "credential",
            Self::Workspace => "workspace",
            Self::Clone => "clone",
            Self::Fetch => "fetch",
            Self::Checkout => "checkout",
            Self::Verify => "verify",
        }
    }
}

/// No provider output, repository URL, or credential enters failure evidence.
#[derive(Debug)]
pub(crate) struct CheckoutProvisioningFailed {
    pub(crate) step: CheckoutStep,
    pub(crate) outcome: Option<ProcessOutcome>,
}

impl CheckoutProvisioningFailed {
    pub(crate) const fn at(step: CheckoutStep) -> Self {
        Self {
            step,
            outcome: None,
        }
    }
    pub(crate) fn status(&self) -> String {
        match &self.outcome {
            Some(ProcessOutcome::Exited { code: Some(code) }) => format!("exit:{code}"),
            Some(ProcessOutcome::Exited { code: None }) => String::from("signaled"),
            Some(ProcessOutcome::TimedOut) => String::from("timed_out"),
            Some(ProcessOutcome::SpawnFailed { .. }) => String::from("spawn_failed"),
            Some(ProcessOutcome::SupervisionFailed { .. }) => String::from("supervision_failed"),
            None => String::from("not_started"),
        }
    }
}

pub(crate) async fn provision<Runner: ProcessRunner>(
    runner: &mut Runner,
    roots: &SessionWorkspaceRoots,
    session: SessionId,
    repository: &RepositorySlug,
    pull_request: &PullRequestEventContext,
    credentials: &RepositoryWatchClientLoader,
) -> Result<(), CheckoutProvisioningFailed> {
    let authorization = credentials
        .git_authorization()
        .await
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Credential))?;
    let path = roots.derived_path(session);
    let directory = provision_directory(&path)
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
    // The descriptor remains open through every invocation; child cwd resolution
    // cannot redirect Git through a replaced session-directory pathname.
    let working_directory = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let mut environment: BTreeMap<OsString, OsString> = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_COUNT", "4"),
        ("GIT_CONFIG_KEY_1", "credential.helper"),
        ("GIT_CONFIG_VALUE_1", ""),
        ("GIT_CONFIG_KEY_2", "core.hooksPath"),
        ("GIT_CONFIG_VALUE_2", "/dev/null"),
        ("GIT_CONFIG_KEY_3", "http.followRedirects"),
        ("GIT_CONFIG_VALUE_3", "false"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect();
    environment.insert("GIT_CONFIG_VALUE_0".into(), authorization.into());
    if let Some(path) = std::env::var_os("PATH") {
        environment.insert("PATH".into(), path);
    }
    let repository_url = format!("https://github.com/{}.git", repository.as_str());
    environment.insert(
        "GIT_CONFIG_KEY_0".into(),
        format!("http.{repository_url}.extraheader").into(),
    );
    match rustix::fs::statat(&directory, ".git", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => {
            git(
                runner,
                &working_directory,
                &environment,
                CheckoutStep::Clone,
                &["clone", "--no-checkout", "--", &repository_url, "."],
            )
            .await?;
        }
        Ok(stat)
            if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                == rustix::fs::FileType::Directory => {}
        _ => return Err(CheckoutProvisioningFailed::at(CheckoutStep::Workspace)),
    }
    // Fetch the retained SHA explicitly: a branch may advance after observation,
    // and a fork's head need not be reachable from the watched repository's heads.
    let head_url = format!(
        "https://github.com/{}.git",
        pull_request.head_repository().as_str()
    );
    if pull_request.head_repository() != repository {
        environment.insert("GIT_CONFIG_VALUE_0".into(), "".into());
    }
    git(
        runner,
        &working_directory,
        &environment,
        CheckoutStep::Fetch,
        &["fetch", "--", &head_url, pull_request.head_sha().as_str()],
    )
    .await?;
    git(
        runner,
        &working_directory,
        &environment,
        CheckoutStep::Checkout,
        &[
            "checkout",
            "-B",
            pull_request.head_branch().as_str(),
            pull_request.head_sha().as_str(),
            "--",
        ],
    )
    .await?;
    // Use the standing path as the tools will, and reject directory replacement.
    let standing =
        open_directory(&path).map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Verify))?;
    let pinned_stat = rustix::fs::fstat(&directory)
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Verify))?;
    let standing_stat = rustix::fs::fstat(&standing)
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Verify))?;
    if (pinned_stat.st_dev, pinned_stat.st_ino) != (standing_stat.st_dev, standing_stat.st_ino) {
        return Err(CheckoutProvisioningFailed::at(CheckoutStep::Verify));
    }
    Ok(())
}

async fn git<Runner: ProcessRunner>(
    runner: &mut Runner,
    directory: &Path,
    environment: &BTreeMap<OsString, OsString>,
    step: CheckoutStep,
    arguments: &[&str],
) -> Result<(), CheckoutProvisioningFailed> {
    let result = runner
        .run(ProcessRequest {
            program: "git".into(),
            arguments: arguments.iter().map(OsString::from).collect(),
            working_directory: directory.to_owned(),
            // Uses the exec family's maximum command duration; no checkout policy or config key.
            timeout: Duration::from_secs(300),
            capture_bytes: 0,
            environment: environment.clone(),
            environment_inheritance: ProcessEnvironment::Clear,
            status_protocol: ProcessStatusProtocol::Direct,
        })
        .await;
    match result.outcome {
        ProcessOutcome::Exited { code: Some(0) } => Ok(()),
        outcome => Err(CheckoutProvisioningFailed {
            step,
            outcome: Some(outcome),
        }),
    }
}

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn open_directory(path: &Path) -> Result<OwnedFd, rustix::io::Errno> {
    let mut directory = openat(rustix::fs::CWD, "/", DIRECTORY_FLAGS, Mode::empty())?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())?
            }
            _ => return Err(rustix::io::Errno::INVAL),
        }
    }
    Ok(directory)
}

fn provision_directory(path: &Path) -> Result<OwnedFd, rustix::io::Errno> {
    let parent = path.parent().ok_or(rustix::io::Errno::INVAL)?;
    let ancestor = parent.parent().ok_or(rustix::io::Errno::INVAL)?;
    let directory = open_directory(ancestor)?;
    let parent = create_directory(
        &directory,
        parent.file_name().ok_or(rustix::io::Errno::INVAL)?,
    )?;
    create_directory(&parent, path.file_name().ok_or(rustix::io::Errno::INVAL)?)
}

fn create_directory(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
) -> Result<OwnedFd, rustix::io::Errno> {
    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error),
    }
    openat(parent, name, DIRECTORY_FLAGS, Mode::empty())
}

pub(crate) fn remove(
    roots: &SessionWorkspaceRoots,
    session: SessionId,
) -> Result<(), rustix::io::Errno> {
    let path = roots.derived_path(session);
    let parent = match open_directory(path.parent().ok_or(rustix::io::Errno::INVAL)?) {
        Ok(parent) => parent,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error),
    };
    let name = path.file_name().ok_or(rustix::io::Errno::INVAL)?;
    let directory = match openat(&parent, name, DIRECTORY_FLAGS, Mode::empty()) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error),
    };
    remove_contents(&directory)?;
    remove_directory_entry(&parent, name, &directory)
}

fn remove_contents(directory: &OwnedFd) -> Result<(), rustix::io::Errno> {
    use rustix::fs::{AtFlags, Dir, FileType, statat, unlinkat};
    let mut entries = Dir::new(rustix::io::dup(directory)?)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let metadata = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
            let child = openat(directory, name, DIRECTORY_FLAGS, Mode::empty())?;
            remove_contents(&child)?;
            use std::os::unix::ffi::OsStrExt;
            remove_directory_entry(
                directory,
                std::ffi::OsStr::from_bytes(name.to_bytes()),
                &child,
            )?;
        } else {
            // A tracked symlink is removed as an entry, never traversed.
            unlinkat(directory, name, AtFlags::empty())?;
        }
    }
    Ok(())
}

fn remove_directory_entry(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
    directory: &OwnedFd,
) -> Result<(), rustix::io::Errno> {
    use rustix::fs::{AtFlags, fstat, statat, unlinkat};
    let pinned = fstat(directory)?;
    let standing = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if (pinned.st_dev, pinned.st_ino) != (standing.st_dev, standing.st_ino) {
        return Err(rustix::io::Errno::STALE);
    }
    unlinkat(parent, name, AtFlags::REMOVEDIR)
}

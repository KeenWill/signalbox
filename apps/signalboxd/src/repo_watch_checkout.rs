//! Provisions the repository at the daemon's derived session root.

use crate::{
    daemon_tools::SessionWorkspaceRoots, repo_watch_credentials::RepositoryWatchClientLoader,
};
use rustix::fs::{Mode, OFlags, mkdirat, openat};
use signalbox_domain::{PullRequestEventContext, RepoWatchDispatchId, RepositorySlug, SessionId};
use signalbox_module_repo_watch_v2::checkout::CheckoutDirectoryIdentity;
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

pub(crate) struct CheckoutDirectory {
    path: PathBuf,
    dispatch: RepoWatchDispatchId,
    parent: OwnedFd,
    staged_name: Option<OsString>,
    directory: OwnedFd,
    pub(crate) identity: CheckoutDirectoryIdentity,
    pub(crate) created: bool,
}

pub(crate) fn prepare(
    roots: &SessionWorkspaceRoots,
    session: SessionId,
    dispatch: RepoWatchDispatchId,
) -> Result<CheckoutDirectory, CheckoutProvisioningFailed> {
    let path = roots.derived_path(session);
    let parent = provision_parent(&path)
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
    let name = path
        .file_name()
        .ok_or(CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
    let (directory, staged_name, created) =
        match openat(&parent, name, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(directory) => (directory, None, false),
            Err(rustix::io::Errno::NOENT) => {
                let name = staging_name(dispatch);
                let created = match mkdirat(&parent, &name, Mode::RWXU) {
                    Ok(()) => true,
                    Err(rustix::io::Errno::EXIST) => false,
                    Err(_) => return Err(CheckoutProvisioningFailed::at(CheckoutStep::Workspace)),
                };
                let directory = openat(&parent, &name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
                (directory, Some(name), created)
            }
            Err(_) => return Err(CheckoutProvisioningFailed::at(CheckoutStep::Workspace)),
        };
    let stat = rustix::fs::fstat(&directory)
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
    #[allow(
        clippy::unnecessary_cast,
        reason = "device numbers have platform-specific widths"
    )]
    let device = stat.st_dev as u64;
    Ok(CheckoutDirectory {
        path,
        dispatch,
        parent,
        created,
        staged_name,
        directory,
        identity: CheckoutDirectoryIdentity {
            device,
            inode: stat.st_ino,
        },
    })
}

pub(crate) async fn provision<Runner: ProcessRunner>(
    runner: &mut Runner,
    checkout: &mut CheckoutDirectory,
    repository: &RepositorySlug,
    pull_request: &PullRequestEventContext,
    credentials: &RepositoryWatchClientLoader,
) -> Result<(), CheckoutProvisioningFailed> {
    if let Some(name) = &checkout.staged_name {
        // Git clone requires an empty directory; inode metadata survives publication without adding entries.
        #[cfg(target_os = "linux")]
        rustix::fs::fsetxattr(
            &checkout.directory,
            PUBLICATION_MARKER,
            checkout.dispatch.into_uuid().to_string().as_bytes(),
            rustix::fs::XattrFlags::empty(),
        )
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
        rustix::fs::renameat_with(
            &checkout.parent,
            name,
            &checkout.parent,
            checkout
                .path
                .file_name()
                .ok_or(CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
        checkout.staged_name = None;
    }
    let authorization = credentials
        .git_authorization()
        .await
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Credential))?;
    let path = &checkout.path;
    let directory = &checkout.directory;
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
    match rustix::fs::statat(directory, ".git", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
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
    retain_dispatch_marker(checkout)?;
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
        open_directory(path).map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Verify))?;
    let pinned_stat = rustix::fs::fstat(directory)
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Verify))?;
    let standing_stat = rustix::fs::fstat(&standing)
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Verify))?;
    if (pinned_stat.st_dev, pinned_stat.st_ino) != (standing_stat.st_dev, standing_stat.st_ino) {
        return Err(CheckoutProvisioningFailed::at(CheckoutStep::Verify));
    }
    Ok(())
}

fn retain_dispatch_marker(checkout: &CheckoutDirectory) -> Result<(), CheckoutProvisioningFailed> {
    let git_directory = openat(&checkout.directory, ".git", DIRECTORY_FLAGS, Mode::empty())
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
    let marker = openat(
        &git_directory,
        DISPATCH_MARKER,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
    use std::io::Write;
    std::fs::File::from(marker)
        .write_all(checkout.dispatch.into_uuid().to_string().as_bytes())
        .map_err(|_| CheckoutProvisioningFailed::at(CheckoutStep::Workspace))?;
    #[cfg(target_os = "linux")]
    match rustix::fs::fremovexattr(&checkout.directory, PUBLICATION_MARKER) {
        Ok(()) | Err(rustix::io::Errno::NODATA) => {}
        Err(_) => return Err(CheckoutProvisioningFailed::at(CheckoutStep::Workspace)),
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

fn provision_parent(path: &Path) -> Result<OwnedFd, rustix::io::Errno> {
    let parent = path.parent().ok_or(rustix::io::Errno::INVAL)?;
    let ancestor = parent.parent().ok_or(rustix::io::Errno::INVAL)?;
    let directory = open_directory(ancestor)?;
    create_directory(
        &directory,
        parent.file_name().ok_or(rustix::io::Errno::INVAL)?,
    )
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

pub(crate) const DISPATCH_MARKER: &str = "signalbox-dispatch";
#[cfg(target_os = "linux")]
const PUBLICATION_MARKER: &str = "user.signalbox.dispatch";

fn staging_name(dispatch: RepoWatchDispatchId) -> OsString {
    format!(".checkout-{}", dispatch.into_uuid()).into()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn remove(
    _roots: &SessionWorkspaceRoots,
    _session: SessionId,
    _dispatch: RepoWatchDispatchId,
    _created: bool,
    _identity: Option<CheckoutDirectoryIdentity>,
) -> Result<(), rustix::io::Errno> {
    Err(rustix::io::Errno::OPNOTSUPP)
}

#[cfg(target_os = "linux")]
pub(crate) fn remove(
    roots: &SessionWorkspaceRoots,
    session: SessionId,
    dispatch: RepoWatchDispatchId,
    created: bool,
    identity: Option<CheckoutDirectoryIdentity>,
) -> Result<(), rustix::io::Errno> {
    let path = roots.derived_path(session);
    let parent = match open_directory(path.parent().ok_or(rustix::io::Errno::INVAL)?) {
        Ok(parent) => parent,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error),
    };
    // An unpublished staging directory is empty, even before ownership is retained.
    let staged = staging_name(dispatch);
    match pin_removal_directory(&parent, &staged) {
        Ok(directory) => remove_directory_entry(&parent, &staged, &directory)?,
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error),
    }
    if !created {
        return Ok(());
    }
    let identity = identity.ok_or(rustix::io::Errno::STALE)?;
    let name = path.file_name().ok_or(rustix::io::Errno::INVAL)?;
    let (name, directory) = match pin_removal_directory(&parent, name) {
        Ok(directory) => (name.to_owned(), directory),
        Err(rustix::io::Errno::NOENT) => {
            let Some(found) = find_renamed_directory(&parent, identity, dispatch)? else {
                return Ok(());
            };
            found
        }
        Err(error) => return Err(error),
    };
    let stat = rustix::fs::fstat(&directory)?;
    if (identity.device, identity.inode) != (stat.st_dev, stat.st_ino) {
        return Err(rustix::io::Errno::STALE);
    }
    if !marker_matches(&directory, dispatch)? {
        return Ok(());
    }
    let directory = read_removal_directory(&directory)?;
    remove_contents(&directory, &[".git", DISPATCH_MARKER])?;
    remove_directory_entry(&parent, &name, &directory)
}

#[cfg(target_os = "linux")]
fn find_renamed_directory(
    parent: &OwnedFd,
    identity: CheckoutDirectoryIdentity,
    dispatch: RepoWatchDispatchId,
) -> Result<Option<(OsString, OwnedFd)>, rustix::io::Errno> {
    use rustix::fs::{AtFlags, Dir, FileType, statat};
    use std::os::unix::ffi::OsStrExt;

    let mut entries = Dir::new(rustix::io::dup(parent)?)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory
            && (stat.st_dev, stat.st_ino) == (identity.device, identity.inode)
        {
            let name = std::ffi::OsStr::from_bytes(name.to_bytes());
            let directory = pin_removal_directory(parent, name)?;
            let pinned = rustix::fs::fstat(&directory)?;
            if (pinned.st_dev, pinned.st_ino) == (identity.device, identity.inode)
                && marker_matches(&directory, dispatch)?
            {
                return Ok(Some((name.to_owned(), directory)));
            }
        }
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
fn marker_matches(
    directory: &OwnedFd,
    dispatch: RepoWatchDispatchId,
) -> Result<bool, rustix::io::Errno> {
    let readable = read_removal_directory(directory)?;
    let mut publication = [0; uuid::fmt::Hyphenated::LENGTH + 1];
    match rustix::fs::fgetxattr(&readable, PUBLICATION_MARKER, &mut publication) {
        Ok(count) => {
            return Ok(&publication[..count] == dispatch.into_uuid().to_string().as_bytes());
        }
        Err(rustix::io::Errno::NODATA) => {}
        Err(rustix::io::Errno::RANGE) => return Ok(false),
        Err(error) => return Err(error),
    }
    let git_directory = match pin_removal_directory(directory, std::ffi::OsStr::new(".git")) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    restore_owner_permissions(&git_directory, Mode::XUSR)?;
    let marker = match rustix::fs::openat2(
        &git_directory,
        DISPATCH_MARKER,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_XDEV,
    ) {
        Ok(marker) => marker,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if rustix::fs::FileType::from_raw_mode(rustix::fs::fstat(&marker)?.st_mode)
        != rustix::fs::FileType::RegularFile
    {
        return Ok(false);
    }
    restore_owner_permissions(&marker, Mode::RUSR)?;
    let marker = rustix::fs::open(
        format!("/proc/self/fd/{}", marker.as_raw_fd()),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    // One extra byte distinguishes the exact UUID spelling from a longer file.
    let mut contents = [0; uuid::fmt::Hyphenated::LENGTH + 1];
    let count = rustix::io::read(&marker, &mut contents)?;
    Ok(&contents[..count] == dispatch.into_uuid().to_string().as_bytes())
}

#[cfg(target_os = "linux")]
fn pin_removal_directory(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
) -> Result<OwnedFd, rustix::io::Errno> {
    rustix::fs::openat2(
        parent,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        rustix::fs::ResolveFlags::NO_XDEV,
    )
}

#[cfg(target_os = "linux")]
fn read_removal_directory(directory: &OwnedFd) -> Result<OwnedFd, rustix::io::Errno> {
    restore_owner_permissions(directory, Mode::RWXU)?;
    openat(directory, ".", DIRECTORY_FLAGS, Mode::empty())
}

#[cfg(target_os = "linux")]
fn restore_owner_permissions(directory: &OwnedFd, required: Mode) -> Result<(), rustix::io::Errno> {
    let mode = Mode::from_raw_mode(rustix::fs::fstat(directory)?.st_mode);
    if !mode.contains(required) {
        // O_PATH pins unreadable entries; procfs addresses that inode for chmod.
        rustix::fs::chmodat(
            rustix::fs::CWD,
            format!("/proc/self/fd/{}", directory.as_raw_fd()),
            mode | required,
            rustix::fs::AtFlags::empty(),
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_contents(directory: &OwnedFd, marker_path: &[&str]) -> Result<(), rustix::io::Errno> {
    use rustix::fs::Dir;
    use std::os::unix::ffi::OsStrExt;
    let mut entries = Dir::new(rustix::io::dup(directory)?)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..")
            || marker_path
                .first()
                .is_some_and(|last| name.to_bytes() == last.as_bytes())
        {
            continue;
        }
        remove_entry(directory, std::ffi::OsStr::from_bytes(name.to_bytes()), &[])?;
    }
    if let Some((last, remaining)) = marker_path.split_first() {
        match remove_entry(directory, std::ffi::OsStr::new(last), remaining) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_entry(
    directory: &OwnedFd,
    name: &std::ffi::OsStr,
    marker_path: &[&str],
) -> Result<(), rustix::io::Errno> {
    use rustix::fs::{AtFlags, FileType, statat, unlinkat};
    let metadata = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
        let child = read_removal_directory(&pin_removal_directory(directory, name)?)?;
        remove_contents(&child, marker_path)?;
        remove_directory_entry(directory, name, &child)
    } else {
        // A tracked symlink is removed as an entry, never traversed.
        unlinkat(directory, name, AtFlags::empty())
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt},
        process::Command,
    };

    #[test]
    fn staging_is_recoverable_before_identity_retention() -> Result<(), Box<dyn std::error::Error>>
    {
        let temporary = tempfile::tempdir()?;
        let roots = SessionWorkspaceRoots::try_new(&temporary.path().join("workspace"))?;
        let session = SessionId::from_uuid(uuid::Uuid::now_v7());
        let dispatch = RepoWatchDispatchId::from_uuid(uuid::Uuid::now_v7());
        let checkout = prepare(&roots, session, dispatch).expect("prepare checkout");
        let staged = roots
            .derived_path(session)
            .with_file_name(format!(".checkout-{}", dispatch.into_uuid()));
        assert!(staged.is_dir());
        let identity = checkout.identity;
        drop(checkout);
        let reopened = prepare(&roots, session, dispatch).expect("reopen after cancellation");
        assert_eq!(
            reopened.identity, identity,
            "cancellation preserves the inode for identity retention and replay"
        );
        assert!(!reopened.created);
        drop(reopened);
        remove(&roots, session, dispatch, false, None)?;
        assert!(!staged.exists());
        assert!(!roots.derived_path(session).exists());
        Ok(())
    }

    #[test]
    #[ignore = "requires private user and mount namespaces"]
    fn removal_refuses_mount_crossings() -> Result<(), Box<dyn std::error::Error>> {
        const CHILD: &str = "SIGNALBOX_CHECKOUT_MOUNT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = Command::new("unshare")
                .args([
                    "--user",
                    "--map-root-user",
                    "--mount",
                    "--propagation",
                    "private",
                ])
                .arg(std::env::current_exe()?)
                .args([
                    "--exact",
                    "repo_watch_checkout::tests::removal_refuses_mount_crossings",
                    "--include-ignored",
                ])
                .env(CHILD, "1")
                .status()?;
            assert!(
                result.success(),
                "private mount-namespace regression failed"
            );
            return Ok(());
        }
        let temporary = tempfile::tempdir()?;
        let roots = SessionWorkspaceRoots::try_new(&temporary.path().join("workspace"))?;
        let session = SessionId::from_uuid(uuid::Uuid::now_v7());
        let dispatch = RepoWatchDispatchId::from_uuid(uuid::Uuid::now_v7());
        let root = roots
            .derived_path(session)
            .with_file_name("renamed-checkout");
        let nested = root.join("nested");
        let source = temporary.path().join("source");
        std::fs::create_dir_all(&nested)?;
        std::fs::create_dir(root.join(".git"))?;
        std::fs::write(
            root.join(".git/signalbox-dispatch"),
            dispatch.into_uuid().to_string(),
        )?;
        std::fs::create_dir(&source)?;
        let metadata = std::fs::metadata(&root)?;
        let identity = Some(CheckoutDirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        });
        for bind in [false, true] {
            let mut mount = Command::new("mount");
            if bind {
                mount.arg("--bind").arg(&source);
            } else {
                mount.args(["-t", "tmpfs", "tmpfs"]);
            }
            assert!(
                mount.arg(&nested).status()?.success(),
                "mount test filesystem"
            );
            std::fs::write(nested.join("keep"), b"mounted contents")?;
            std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o500))?;
            if bind {
                assert_eq!(
                    std::fs::metadata(&root)?.dev(),
                    std::fs::metadata(&nested)?.dev()
                );
            }
            assert_eq!(
                remove(&roots, session, dispatch, true, identity),
                Err(rustix::io::Errno::XDEV)
            );
            assert_eq!(
                std::fs::read_to_string(root.join(".git/signalbox-dispatch"))?,
                dispatch.into_uuid().to_string(),
                "failed removal preserves ownership evidence for sibling recovery"
            );
            assert_eq!(std::fs::metadata(&nested)?.mode() & 0o777, 0o500);
            assert_eq!(std::fs::read(nested.join("keep"))?, b"mounted contents");
            assert!(Command::new("umount").arg(&nested).status()?.success());
        }
        remove(&roots, session, dispatch, true, identity)?;
        assert!(!root.exists());
        assert_eq!(std::fs::read(source.join("keep"))?, b"mounted contents");
        Ok(())
    }
}

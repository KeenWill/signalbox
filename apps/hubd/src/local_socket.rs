//! Guarded local Unix-socket binding for the process protocol.

use std::{
    error::Error,
    fmt, fs,
    fs::File,
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use rustix::{
    fs::{FlockOperation, Mode, OFlags, fchmod, fcntl_getfl, fcntl_setfl, flock, open},
    io::{FdFlags, fcntl_setfd},
    net::{AddressFamily, SocketAddrUnix, SocketType, bind, connect, getsockname, listen, socket},
    process::geteuid,
};
use tokio::net::{UnixListener, UnixStream, unix::SocketAddr as UnixSocketAddr};

const LISTEN_BACKLOG: i32 = 128;
const OWNER_ONLY_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const GROUP_OR_OTHER_ACCESS: u32 = 0o077;
const GROUP_OR_OTHER_WRITE: u32 = 0o022;
const STICKY_BIT: u32 = 0o1000;

/// A process listener whose filesystem entry was verified before listening.
#[derive(Debug)]
pub struct LocalProcessListener {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
    path_lock: File,
}

impl LocalProcessListener {
    /// Binds one guarded owner-only listener at an absolute configured path.
    pub fn bind(configured_path: &Path) -> Result<Self, LocalSocketError> {
        let path = resolve_socket_path(configured_path)?;
        let path_lock = acquire_path_lock(&path)?;
        prepare_final_entry(&path)?;

        let socket = socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .map_err(|error| LocalSocketError::CreateSocket(rustix_error(error)))?;
        fcntl_setfd(&socket, FdFlags::CLOEXEC)
            .map_err(|error| LocalSocketError::ConfigureSocket(rustix_error(error)))?;
        let address = SocketAddrUnix::new(&path)
            .map_err(|error| LocalSocketError::CreateAddress(rustix_error(error)))?;
        bind(&socket, &address).map_err(|error| LocalSocketError::Bind(rustix_error(error)))?;

        let effective_user = geteuid().as_raw();
        let first_metadata =
            fs::symlink_metadata(&path).map_err(LocalSocketError::ReadBoundIdentity)?;
        let identity = SocketIdentity::capture(&first_metadata, effective_user)
            .ok_or(LocalSocketError::BoundIdentityMismatch)?;
        let cleanup = FailedBindCleanup::new(&path, identity);

        let local_address = getsockname(&socket)
            .map_err(|error| LocalSocketError::ReadLocalAddress(rustix_error(error)))?;
        let local_address = SocketAddrUnix::try_from(local_address)
            .map_err(|error| LocalSocketError::ReadLocalAddress(rustix_error(error)))?;
        if local_address.path_bytes() != Some(path.as_os_str().as_bytes()) {
            return Err(LocalSocketError::BoundAddressMismatch);
        }

        fs::set_permissions(&path, fs::Permissions::from_mode(OWNER_ONLY_MODE))
            .map_err(LocalSocketError::SetPermissions)?;
        let second_metadata =
            fs::symlink_metadata(&path).map_err(LocalSocketError::VerifyBoundIdentity)?;
        if !identity.matches(&second_metadata, effective_user)
            || second_metadata.mode() & PERMISSION_MASK != OWNER_ONLY_MODE
        {
            return Err(LocalSocketError::BoundIdentityMismatch);
        }

        listen(&socket, LISTEN_BACKLOG)
            .map_err(|error| LocalSocketError::Listen(rustix_error(error)))?;
        let std_listener = std::os::unix::net::UnixListener::from(socket);
        std_listener
            .set_nonblocking(true)
            .map_err(LocalSocketError::ConfigureSocket)?;
        let listener =
            UnixListener::from_std(std_listener).map_err(LocalSocketError::RegisterListener)?;
        cleanup.disarm();

        Ok(Self {
            listener,
            path,
            identity,
            path_lock,
        })
    }

    /// Accepts one client after the guarded bind sequence has completed.
    pub async fn accept(&self) -> io::Result<(UnixStream, UnixSocketAddr)> {
        self.listener.accept().await
    }

    /// Returns the once-resolved socket path used for this listener lifetime.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stops listening and removes only this listener's revalidated path entry.
    pub fn cleanup(self) -> Result<(), LocalSocketError> {
        drop(self.listener);
        let metadata =
            fs::symlink_metadata(&self.path).map_err(LocalSocketError::ReadCleanupIdentity)?;
        if !self.identity.matches(&metadata, geteuid().as_raw()) {
            return Err(LocalSocketError::CleanupIdentityMismatch);
        }
        fs::remove_file(&self.path).map_err(LocalSocketError::RemoveSocket)?;
        drop(self.path_lock);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn capture(metadata: &fs::Metadata, effective_user: u32) -> Option<Self> {
        (metadata.file_type().is_socket() && metadata.uid() == effective_user).then_some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn matches(self, metadata: &fs::Metadata, effective_user: u32) -> bool {
        Self::capture(metadata, effective_user) == Some(self)
    }
}

struct FailedBindCleanup {
    path: PathBuf,
    identity: SocketIdentity,
    armed: bool,
}

impl FailedBindCleanup {
    fn new(path: &Path, identity: SocketIdentity) -> Self {
        Self {
            path: path.to_owned(),
            identity,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for FailedBindCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let effective_user = geteuid().as_raw();
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| self.identity.matches(&metadata, effective_user))
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn resolve_socket_path(configured_path: &Path) -> Result<PathBuf, LocalSocketError> {
    if !configured_path.is_absolute() {
        return Err(LocalSocketError::InvalidPath);
    }
    let file_name = configured_path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or(LocalSocketError::InvalidPath)?;
    let parent = configured_path
        .parent()
        .ok_or(LocalSocketError::InvalidPath)?;
    let resolved_parent = fs::canonicalize(parent).map_err(LocalSocketError::ResolveParent)?;
    let metadata = fs::metadata(&resolved_parent).map_err(LocalSocketError::ReadParentMetadata)?;
    if !metadata.is_dir() {
        return Err(LocalSocketError::ParentNotDirectory);
    }
    let effective_user = geteuid().as_raw();
    if metadata.uid() != effective_user {
        return Err(LocalSocketError::ParentOwnerMismatch);
    }
    if metadata.mode() & GROUP_OR_OTHER_ACCESS != 0 {
        return Err(LocalSocketError::ParentPermissionsTooBroad);
    }
    validate_ancestor_chain(&resolved_parent, metadata.uid(), effective_user)?;
    Ok(resolved_parent.join(file_name))
}

fn validate_ancestor_chain(
    resolved_parent: &Path,
    mut child_owner: u32,
    effective_user: u32,
) -> Result<(), LocalSocketError> {
    let mut child = resolved_parent;
    while let Some(ancestor) = child.parent() {
        if ancestor == child {
            break;
        }
        let metadata = fs::metadata(ancestor).map_err(LocalSocketError::ReadAncestorMetadata)?;
        if !ancestor_owner_is_trusted(metadata.uid(), effective_user) {
            return Err(LocalSocketError::AncestorOwnerMismatch);
        }
        let ancestor_is_writable = metadata.mode() & GROUP_OR_OTHER_WRITE != 0;
        let sticky_child_is_protected =
            metadata.mode() & STICKY_BIT != 0 && child_owner == effective_user;
        if ancestor_is_writable && !sticky_child_is_protected {
            return Err(LocalSocketError::AncestorPermissionsTooBroad);
        }
        child = ancestor;
        child_owner = metadata.uid();
    }
    Ok(())
}

fn ancestor_owner_is_trusted(owner: u32, effective_user: u32) -> bool {
    owner == 0 || owner == effective_user
}

fn acquire_path_lock(socket_path: &Path) -> Result<File, LocalSocketError> {
    let mut lock_path = socket_path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let (descriptor, created) = match open(
        &lock_path,
        flags | OFlags::CREATE | OFlags::EXCL,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(descriptor) => (descriptor, true),
        Err(rustix::io::Errno::EXIST) => (
            open(&lock_path, flags, Mode::empty())
                .map_err(|error| LocalSocketError::OpenPathLock(rustix_error(error)))?,
            false,
        ),
        Err(error) => return Err(LocalSocketError::OpenPathLock(rustix_error(error))),
    };
    if created {
        fchmod(&descriptor, Mode::RUSR | Mode::WUSR)
            .map_err(|error| LocalSocketError::ConfigurePathLock(rustix_error(error)))?;
    }
    let path_lock = File::from(descriptor);
    let descriptor_metadata = path_lock
        .metadata()
        .map_err(LocalSocketError::InspectPathLock)?;
    let path_metadata =
        fs::symlink_metadata(&lock_path).map_err(LocalSocketError::InspectPathLock)?;
    let effective_user = geteuid().as_raw();
    let valid_lock = descriptor_metadata.is_file()
        && descriptor_metadata.uid() == effective_user
        && descriptor_metadata.mode() & PERMISSION_MASK == OWNER_ONLY_MODE
        && path_metadata.is_file()
        && path_metadata.uid() == effective_user
        && path_metadata.mode() & PERMISSION_MASK == OWNER_ONLY_MODE
        && descriptor_metadata.dev() == path_metadata.dev()
        && descriptor_metadata.ino() == path_metadata.ino();
    if !valid_lock {
        return Err(LocalSocketError::InvalidPathLock);
    }
    flock(&path_lock, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        if error == rustix::io::Errno::WOULDBLOCK {
            LocalSocketError::PathLockBusy
        } else {
            LocalSocketError::LockPath(rustix_error(error))
        }
    })?;
    let locked_path_metadata =
        fs::symlink_metadata(&lock_path).map_err(LocalSocketError::InspectPathLock)?;
    if !locked_path_metadata.is_file()
        || locked_path_metadata.uid() != effective_user
        || locked_path_metadata.mode() & PERMISSION_MASK != OWNER_ONLY_MODE
        || descriptor_metadata.dev() != locked_path_metadata.dev()
        || descriptor_metadata.ino() != locked_path_metadata.ino()
    {
        return Err(LocalSocketError::InvalidPathLock);
    }
    Ok(path_lock)
}

fn prepare_final_entry(path: &Path) -> Result<(), LocalSocketError> {
    let first_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(LocalSocketError::ReadExistingEntry(error)),
    };
    if !first_metadata.file_type().is_socket() {
        return Err(LocalSocketError::ExistingEntryNotSocket);
    }

    let probe = socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .map_err(|error| LocalSocketError::ProbeExistingSocket(rustix_error(error)))?;
    fcntl_setfd(&probe, FdFlags::CLOEXEC)
        .map_err(|error| LocalSocketError::ProbeExistingSocket(rustix_error(error)))?;
    let flags = fcntl_getfl(&probe)
        .map_err(|error| LocalSocketError::ProbeExistingSocket(rustix_error(error)))?;
    fcntl_setfl(&probe, flags | OFlags::NONBLOCK)
        .map_err(|error| LocalSocketError::ProbeExistingSocket(rustix_error(error)))?;
    let address = SocketAddrUnix::new(path)
        .map_err(|error| LocalSocketError::ProbeExistingSocket(rustix_error(error)))?;
    match connect(&probe, &address) {
        Ok(_) => return Err(LocalSocketError::ExistingSocketLive),
        Err(rustix::io::Errno::CONNREFUSED) => {}
        Err(
            rustix::io::Errno::AGAIN | rustix::io::Errno::INPROGRESS | rustix::io::Errno::ALREADY,
        ) => return Err(LocalSocketError::ExistingSocketLive),
        Err(error) => {
            return Err(LocalSocketError::ProbeExistingSocket(rustix_error(error)));
        }
    }

    let effective_user = geteuid().as_raw();
    let identity = SocketIdentity::capture(&first_metadata, effective_user)
        .ok_or(LocalSocketError::ExistingSocketOwnerMismatch)?;
    let second_metadata =
        fs::symlink_metadata(path).map_err(LocalSocketError::RevalidateExistingSocket)?;
    if !identity.matches(&second_metadata, effective_user) {
        return Err(LocalSocketError::ExistingSocketChanged);
    }
    fs::remove_file(path).map_err(LocalSocketError::RemoveStaleSocket)
}

fn rustix_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

/// Sanitized local process-socket binding or cleanup failure.
#[derive(Debug)]
pub enum LocalSocketError {
    /// The configured path was not an absolute path with a final component.
    InvalidPath,
    /// The configured parent could not be resolved once.
    ResolveParent(io::Error),
    /// The resolved parent's metadata could not be read.
    ReadParentMetadata(io::Error),
    /// The resolved parent was not a directory.
    ParentNotDirectory,
    /// The resolved parent was not owned by the effective user.
    ParentOwnerMismatch,
    /// The resolved parent allowed group or other access.
    ParentPermissionsTooBroad,
    /// An ancestor of the resolved parent could not be inspected.
    ReadAncestorMetadata(io::Error),
    /// An ancestor was owned by neither root nor the effective user.
    AncestorOwnerMismatch,
    /// An ancestor could replace its next component toward the socket.
    AncestorPermissionsTooBroad,
    /// The adjacent sidecar could not be opened without following links.
    OpenPathLock(io::Error),
    /// A newly created sidecar could not be made owner-only.
    ConfigurePathLock(io::Error),
    /// The adjacent sidecar's descriptor or path identity could not be read.
    InspectPathLock(io::Error),
    /// The adjacent sidecar was not the required owner-only regular file.
    InvalidPathLock,
    /// Another process currently owns the adjacent sidecar lock.
    PathLockBusy,
    /// The adjacent sidecar's exclusive advisory lock could not be attempted.
    LockPath(io::Error),
    /// An existing final entry could not be inspected.
    ReadExistingEntry(io::Error),
    /// An existing final entry was not a socket.
    ExistingEntryNotSocket,
    /// An existing socket accepted a connection.
    ExistingSocketLive,
    /// An existing socket did not produce the one accepted stale result.
    ProbeExistingSocket(io::Error),
    /// A refused socket was not owned by the effective user.
    ExistingSocketOwnerMismatch,
    /// A refused socket could not be revalidated before removal.
    RevalidateExistingSocket(io::Error),
    /// A refused socket changed before removal.
    ExistingSocketChanged,
    /// A revalidated stale socket could not be removed.
    RemoveStaleSocket(io::Error),
    /// The unlistening socket could not be created.
    CreateSocket(io::Error),
    /// Descriptor flags required by the async runtime could not be applied.
    ConfigureSocket(io::Error),
    /// The resolved filesystem address could not be represented.
    CreateAddress(io::Error),
    /// The unlistening socket could not be bound.
    Bind(io::Error),
    /// The bound descriptor's local address could not be read.
    ReadLocalAddress(io::Error),
    /// The bound descriptor did not name the resolved path.
    BoundAddressMismatch,
    /// The new path entry could not be inspected.
    ReadBoundIdentity(io::Error),
    /// The new path entry did not retain the required identity or access.
    BoundIdentityMismatch,
    /// Owner-only permissions could not be applied.
    SetPermissions(io::Error),
    /// The permissioned path entry could not be revalidated.
    VerifyBoundIdentity(io::Error),
    /// The verified socket could not begin listening.
    Listen(io::Error),
    /// The listener could not be registered with the async runtime.
    RegisterListener(io::Error),
    /// The final graceful-cleanup identity could not be read.
    ReadCleanupIdentity(io::Error),
    /// The final path no longer named this hub's socket.
    CleanupIdentityMismatch,
    /// The revalidated socket path could not be removed.
    RemoveSocket(io::Error),
}

impl fmt::Display for LocalSocketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "the local process socket path is invalid",
            Self::ResolveParent(_) => "the local process socket parent could not be resolved",
            Self::ReadParentMetadata(_) => {
                "the local process socket parent metadata could not be read"
            }
            Self::ParentNotDirectory => "the local process socket parent is not a directory",
            Self::ParentOwnerMismatch => "the local process socket parent has the wrong owner",
            Self::ParentPermissionsTooBroad => {
                "the local process socket parent permissions are too broad"
            }
            Self::ReadAncestorMetadata(_) => {
                "the local process socket parent ancestry could not be inspected"
            }
            Self::AncestorOwnerMismatch => {
                "the local process socket parent ancestry has an untrusted owner"
            }
            Self::AncestorPermissionsTooBroad => {
                "the local process socket parent ancestry is replaceable"
            }
            Self::OpenPathLock(_) => "the local process socket path lock could not be opened",
            Self::ConfigurePathLock(_) => {
                "the local process socket path lock could not be configured"
            }
            Self::InspectPathLock(_) | Self::InvalidPathLock => {
                "the local process socket path lock is invalid"
            }
            Self::PathLockBusy => "another hub owns the local process socket path lock",
            Self::LockPath(_) => "the local process socket path could not be locked",
            Self::ReadExistingEntry(_) => {
                "the existing local process socket entry could not be inspected"
            }
            Self::ExistingEntryNotSocket => {
                "the existing local process socket entry is not a socket"
            }
            Self::ExistingSocketLive => "the local process socket is already live",
            Self::ProbeExistingSocket(_) => "the existing local process socket did not prove stale",
            Self::ExistingSocketOwnerMismatch => {
                "the stale local process socket has the wrong owner"
            }
            Self::RevalidateExistingSocket(_) | Self::ExistingSocketChanged => {
                "the stale local process socket changed before removal"
            }
            Self::RemoveStaleSocket(_) => "the stale local process socket could not be removed",
            Self::CreateSocket(_) => "the local process socket could not be created",
            Self::ConfigureSocket(_) => "the local process socket could not be configured",
            Self::CreateAddress(_) => "the local process socket address is invalid",
            Self::Bind(_) => "the local process socket could not be bound",
            Self::ReadLocalAddress(_) => {
                "the bound local process socket address could not be verified"
            }
            Self::BoundAddressMismatch => "the bound local process socket address did not match",
            Self::ReadBoundIdentity(_) | Self::VerifyBoundIdentity(_) => {
                "the bound local process socket identity could not be verified"
            }
            Self::BoundIdentityMismatch => "the bound local process socket identity did not match",
            Self::SetPermissions(_) => "the local process socket permissions could not be set",
            Self::Listen(_) => "the local process socket could not begin listening",
            Self::RegisterListener(_) => "the local process socket could not join the runtime",
            Self::ReadCleanupIdentity(_) | Self::CleanupIdentityMismatch => {
                "the local process socket changed before cleanup"
            }
            Self::RemoveSocket(_) => "the local process socket could not be removed",
        })
    }
}

impl Error for LocalSocketError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ResolveParent(error)
            | Self::ReadParentMetadata(error)
            | Self::ReadAncestorMetadata(error)
            | Self::OpenPathLock(error)
            | Self::ConfigurePathLock(error)
            | Self::InspectPathLock(error)
            | Self::LockPath(error)
            | Self::ReadExistingEntry(error)
            | Self::ProbeExistingSocket(error)
            | Self::RevalidateExistingSocket(error)
            | Self::RemoveStaleSocket(error)
            | Self::CreateSocket(error)
            | Self::ConfigureSocket(error)
            | Self::CreateAddress(error)
            | Self::Bind(error)
            | Self::ReadLocalAddress(error)
            | Self::ReadBoundIdentity(error)
            | Self::SetPermissions(error)
            | Self::VerifyBoundIdentity(error)
            | Self::Listen(error)
            | Self::RegisterListener(error)
            | Self::ReadCleanupIdentity(error)
            | Self::RemoveSocket(error) => Some(error),
            Self::InvalidPath
            | Self::ParentNotDirectory
            | Self::ParentOwnerMismatch
            | Self::ParentPermissionsTooBroad
            | Self::AncestorOwnerMismatch
            | Self::AncestorPermissionsTooBroad
            | Self::InvalidPathLock
            | Self::PathLockBusy
            | Self::ExistingEntryNotSocket
            | Self::ExistingSocketLive
            | Self::ExistingSocketOwnerMismatch
            | Self::ExistingSocketChanged
            | Self::BoundAddressMismatch
            | Self::BoundIdentityMismatch
            | Self::CleanupIdentityMismatch => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs::{self, File},
        os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use tokio::net::UnixStream;

    use super::{LocalProcessListener, LocalSocketError, ancestor_owner_is_trusted};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Result<Self, Box<dyn Error>> {
            let path = std::env::current_dir()?.join(format!(
                "sbx-sock-{}-{}",
                std::process::id(),
                NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            Ok(Self(path))
        }

        fn socket_path(&self) -> PathBuf {
            self.0.join("hub.sock")
        }

        fn lock_path(&self) -> PathBuf {
            self.0.join("hub.sock.lock")
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn guarded_bind_listens_only_with_owner_access() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let path = directory.socket_path();
        let listener = LocalProcessListener::bind(&path)?;
        let metadata = fs::symlink_metadata(&path)?;

        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(listener.path(), path);
        let lock_metadata = fs::symlink_metadata(directory.lock_path())?;
        assert!(lock_metadata.is_file());
        assert_eq!(lock_metadata.mode() & 0o7777, 0o600);

        let client = UnixStream::connect(&path).await?;
        let (server, _) = listener.accept().await?;
        drop(client);
        drop(server);
        listener.cleanup()?;
        assert!(!path.exists());
        assert!(directory.lock_path().exists());
        Ok(())
    }

    #[tokio::test]
    async fn lifetime_path_lock_precedes_final_socket_inspection() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let path = directory.socket_path();
        let listener = LocalProcessListener::bind(&path)?;

        let result = LocalProcessListener::bind(&path);

        assert!(matches!(result, Err(LocalSocketError::PathLockBusy)));
        listener.cleanup()?;
        Ok(())
    }

    #[tokio::test]
    async fn invalid_path_lock_never_touches_the_socket_path() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let path = directory.socket_path();
        let lock = File::create(directory.lock_path())?;
        fs::set_permissions(directory.lock_path(), fs::Permissions::from_mode(0o644))?;

        let result = LocalProcessListener::bind(&path);

        assert!(matches!(result, Err(LocalSocketError::InvalidPathLock)));
        assert!(!path.exists());
        drop(lock);
        Ok(())
    }

    #[tokio::test]
    async fn path_lock_symlink_is_rejected_without_following() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let path = directory.socket_path();
        let target = directory.path().join("lock-target");
        File::create(&target)?;
        symlink(&target, directory.lock_path())?;

        let result = LocalProcessListener::bind(&path);

        assert!(matches!(result, Err(LocalSocketError::OpenPathLock(_))));
        assert!(!path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn stale_owned_socket_is_replaced_after_revalidation() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let path = directory.socket_path();
        let stale = std::os::unix::net::UnixListener::bind(&path)?;
        drop(stale);

        let listener = LocalProcessListener::bind(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        let client = UnixStream::connect(&path).await?;
        let (server, _) = listener.accept().await?;
        drop(client);
        drop(server);
        listener.cleanup()?;
        Ok(())
    }

    #[tokio::test]
    async fn live_socket_is_never_replaced() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let path = directory.socket_path();
        let live = std::os::unix::net::UnixListener::bind(&path)?;
        let inode = fs::symlink_metadata(&path)?.ino();

        let result = LocalProcessListener::bind(&path);

        assert!(matches!(result, Err(LocalSocketError::ExistingSocketLive)));
        assert_eq!(fs::symlink_metadata(&path)?.ino(), inode);
        drop(live);
        Ok(())
    }

    #[tokio::test]
    async fn nonsocket_entry_is_never_replaced() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let path = directory.socket_path();
        let file = File::create(&path)?;

        let result = LocalProcessListener::bind(&path);

        assert!(matches!(
            result,
            Err(LocalSocketError::ExistingEntryNotSocket)
        ));
        assert!(fs::symlink_metadata(&path)?.is_file());
        drop(file);
        Ok(())
    }

    #[tokio::test]
    async fn graceful_cleanup_preserves_a_raced_replacement() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let path = directory.socket_path();
        let listener = LocalProcessListener::bind(&path)?;
        fs::remove_file(&path)?;
        let replacement = File::create(&path)?;

        let result = listener.cleanup();

        assert!(matches!(
            result,
            Err(LocalSocketError::CleanupIdentityMismatch)
        ));
        assert!(fs::symlink_metadata(&path)?.is_file());
        drop(replacement);
        Ok(())
    }

    #[tokio::test]
    async fn broad_parent_permissions_fail_before_path_creation() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))?;
        let path = directory.socket_path();

        let result = LocalProcessListener::bind(&path);

        assert!(matches!(
            result,
            Err(LocalSocketError::ParentPermissionsTooBroad)
        ));
        assert!(!path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn writable_nonsticky_ancestor_fails_before_path_creation() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::create()?;
        let parent = directory.path().join("owned-parent");
        fs::create_dir(&parent)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777))?;
        let path = parent.join("hub.sock");

        let result = LocalProcessListener::bind(&path);

        assert!(matches!(
            result,
            Err(LocalSocketError::AncestorPermissionsTooBroad)
        ));
        assert!(!path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn sticky_ancestor_accepts_an_owned_child_component() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::create()?;
        let parent = directory.path().join("owned-parent");
        fs::create_dir(&parent)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o1777))?;
        let path = parent.join("hub.sock");

        let listener = LocalProcessListener::bind(&path)?;

        listener.cleanup()?;
        Ok(())
    }

    #[test]
    fn ancestor_owner_must_be_root_or_the_effective_user() {
        let effective_user = 41_000;

        assert!(ancestor_owner_is_trusted(0, effective_user));
        assert!(ancestor_owner_is_trusted(effective_user, effective_user));
        assert!(!ancestor_owner_is_trusted(41_001, effective_user));
    }

    #[tokio::test]
    async fn relative_path_is_rejected() {
        assert!(matches!(
            LocalProcessListener::bind(Path::new("hub.sock")),
            Err(LocalSocketError::InvalidPath)
        ));
    }
}

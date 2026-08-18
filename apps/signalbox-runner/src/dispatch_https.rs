//! Per-dispatch serving boundary for the runner-owned HTTPS broker.

use std::{
    error::Error,
    fmt, fs,
    fs::File,
    io,
    os::fd::AsRawFd as _,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use rustix::fs::{AtFlags, Mode, OFlags, chmodat, fchmod, mkdirat, openat, unlinkat};
use signalbox_runner_wire::CanonicalUuid;
use signalbox_tools_exec::MAX_HTTPS_PROXY_TUNNELS;
use tokio::{net::UnixListener, sync::oneshot, task::JoinSet, time::Instant};

use crate::HttpsBroker;

const DIRECTORY_MODE: u32 = 0o700;
const PERMISSION_MASK: u32 = 0o7777;
const SOCKET_FILE: &str = "h";

/// Sanitized failure while preparing or serving one dispatch-scoped endpoint.
#[derive(Debug)]
pub enum DispatchHttpsError {
    /// The effective-user-private endpoint directory could not be prepared.
    Directory(io::Error),
    /// The supplied runner root was not the effective-user-private directory contract.
    InvalidRoot,
    /// The exact Unix listener could not be bound.
    Bind(io::Error),
    /// The bound path was not the expected Unix socket.
    InvalidSocket,
    /// The listener failed while the dispatch remained active.
    Accept(io::Error),
    /// One isolated tunnel task did not join normally.
    TunnelTask,
}

impl fmt::Display for DispatchHttpsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Directory(_) => "runner HTTPS endpoint directory could not be prepared",
            Self::InvalidRoot => "runner HTTPS endpoint root is invalid",
            Self::Bind(_) => "runner HTTPS endpoint could not be bound",
            Self::InvalidSocket => "runner HTTPS endpoint identity is invalid",
            Self::Accept(_) => "runner HTTPS endpoint could not accept a tunnel",
            Self::TunnelTask => "runner HTTPS tunnel task failed",
        })
    }
}

impl Error for DispatchHttpsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Directory(source) | Self::Bind(source) | Self::Accept(source) => Some(source),
            Self::InvalidRoot | Self::InvalidSocket | Self::TunnelTask => None,
        }
    }
}

/// One exact private Unix endpoint retained only for a physical dispatch.
#[derive(Debug)]
pub struct DispatchHttpsEndpoint {
    listener: UnixListener,
    root: File,
    directory: File,
    directory_name: String,
    socket: PathBuf,
}

impl DispatchHttpsEndpoint {
    /// Removes recognized endpoint directories left by an earlier terminated runner.
    ///
    /// The caller supplies the validated root descriptor after acquiring its
    /// process-lifetime lock, so every recognized endpoint is stale.
    pub fn reclaim_stale(root: File) -> Result<(), DispatchHttpsError> {
        let root_metadata = root.metadata().map_err(DispatchHttpsError::Directory)?;
        if !root_metadata.is_dir()
            || root_metadata.uid() != rustix::process::geteuid().as_raw()
            || root_metadata.permissions().mode() & PERMISSION_MASK != DIRECTORY_MODE
        {
            return Err(DispatchHttpsError::InvalidRoot);
        }
        let descriptor_path = format!("/proc/self/fd/{}", root.as_raw_fd());
        for entry in fs::read_dir(descriptor_path).map_err(DispatchHttpsError::Directory)? {
            let entry = entry.map_err(DispatchHttpsError::Directory)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(lease) = name.strip_prefix("d-") else {
                continue;
            };
            let Ok(lease) = uuid::Uuid::parse_str(lease) else {
                continue;
            };
            if name != format!("d-{lease}") {
                continue;
            }
            let directory = File::from(
                openat(
                    &root,
                    name.as_str(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| DispatchHttpsError::Directory(rustix_error(error)))?,
            );
            match unlinkat(&directory, SOCKET_FILE, AtFlags::empty()) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(error) => {
                    return Err(DispatchHttpsError::Directory(rustix_error(error)));
                }
            }
            unlinkat(&root, name.as_str(), AtFlags::REMOVEDIR)
                .map_err(|error| DispatchHttpsError::Directory(rustix_error(error)))?;
        }
        Ok(())
    }

    /// Binds the endpoint beneath the already-validated effective-user-private runner root.
    pub fn bind(root: File, lease_id: CanonicalUuid) -> Result<Self, DispatchHttpsError> {
        let root_metadata = root.metadata().map_err(DispatchHttpsError::Directory)?;
        if !root_metadata.is_dir()
            || root_metadata.uid() != rustix::process::geteuid().as_raw()
            || root_metadata.permissions().mode() & PERMISSION_MASK != DIRECTORY_MODE
        {
            return Err(DispatchHttpsError::InvalidRoot);
        }
        let directory_name = format!("d-{lease_id}");
        mkdirat(&root, directory_name.as_str(), Mode::RWXU)
            .map_err(|error| DispatchHttpsError::Directory(rustix_error(error)))?;
        if let Err(error) = chmodat(&root, directory_name.as_str(), Mode::RWXU, AtFlags::empty()) {
            let _ = unlinkat(&root, directory_name.as_str(), AtFlags::REMOVEDIR);
            return Err(DispatchHttpsError::Directory(rustix_error(error)));
        }
        let directory = match openat(
            &root,
            directory_name.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => File::from(directory),
            Err(error) => {
                let _ = unlinkat(&root, directory_name.as_str(), AtFlags::REMOVEDIR);
                return Err(DispatchHttpsError::Directory(rustix_error(error)));
            }
        };
        if let Err(error) = fchmod(&directory, Mode::RWXU) {
            let _ = unlinkat(&root, directory_name.as_str(), AtFlags::REMOVEDIR);
            return Err(DispatchHttpsError::Directory(rustix_error(error)));
        }
        let socket = PathBuf::from(format!(
            "/proc/{}/fd/{}/{directory_name}/{SOCKET_FILE}",
            std::process::id(),
            root.as_raw_fd(),
        ));
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(source) => {
                let _ = unlinkat(&root, directory_name.as_str(), AtFlags::REMOVEDIR);
                return Err(DispatchHttpsError::Bind(source));
            }
        };
        let endpoint = Self {
            listener,
            root,
            directory,
            directory_name,
            socket,
        };
        let directory_metadata = endpoint
            .directory
            .metadata()
            .map_err(DispatchHttpsError::Directory)?;
        let socket_metadata =
            fs::symlink_metadata(&endpoint.socket).map_err(DispatchHttpsError::Bind)?;
        if !directory_metadata.is_dir()
            || directory_metadata.permissions().mode() & PERMISSION_MASK != DIRECTORY_MODE
            || !socket_metadata.file_type().is_socket()
        {
            return Err(DispatchHttpsError::InvalidSocket);
        }
        Ok(endpoint)
    }

    /// Borrows the exact socket path captured by the restricted sandbox.
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Serves bounded tunnels until execution finishes or its deadline expires.
    pub async fn serve(
        self,
        broker: HttpsBroker,
        deadline: Instant,
        mut stop: oneshot::Receiver<()>,
    ) -> Result<(), DispatchHttpsError> {
        let mut tunnels = JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut stop => break,
                _ = tokio::time::sleep_until(deadline) => break,
                completed = tunnels.join_next(), if !tunnels.is_empty() => {
                    if completed.is_some_and(|result| result.is_err()) {
                        return Err(DispatchHttpsError::TunnelTask);
                    }
                }
                accepted = self.listener.accept(), if tunnels.len() < MAX_HTTPS_PROXY_TUNNELS => {
                    let (client, _) = accepted.map_err(DispatchHttpsError::Accept)?;
                    let broker = broker.clone();
                    tunnels.spawn(async move {
                        let _ = broker.tunnel(client, deadline).await;
                    });
                }
            }
        }
        tunnels.abort_all();
        while tunnels.join_next().await.is_some() {}
        Ok(())
    }
}

impl Drop for DispatchHttpsEndpoint {
    fn drop(&mut self) {
        let _ = unlinkat(&self.directory, SOCKET_FILE, AtFlags::empty());
        let _ = unlinkat(&self.root, self.directory_name.as_str(), AtFlags::REMOVEDIR);
    }
}

fn rustix_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use std::{fs, fs::File, os::unix::fs::PermissionsExt as _, time::Duration};

    use signalbox_runner_wire::CanonicalUuid;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::UnixStream,
        sync::oneshot,
        time::Instant,
    };
    use uuid::Uuid;

    use super::{DIRECTORY_MODE, DispatchHttpsEndpoint, DispatchHttpsError, HttpsBroker};

    const LEASE: u128 = 0x018f_6f10_0000_7000_8000_0000_0000_00d1;

    fn private_root() -> TempDir {
        let root = tempfile::Builder::new()
            .prefix("signalbox-runner-https-")
            .tempdir()
            .expect("the endpoint fixture root is created");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the endpoint fixture root is effective-user-private");
        root
    }

    fn root_descriptor(root: &TempDir) -> File {
        File::open(root.path()).expect("the endpoint fixture root descriptor opens")
    }

    #[test]
    fn endpoint_rejects_a_group_readable_runner_root() {
        let root = tempfile::Builder::new()
            .prefix("signalbox-runner-https-open-")
            .tempdir()
            .expect("the open endpoint fixture root is created");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750))
            .expect("the endpoint fixture root is deliberately group-readable");
        let error = DispatchHttpsEndpoint::bind(
            root_descriptor(&root),
            CanonicalUuid::from_uuid(Uuid::from_u128(LEASE)),
        )
        .expect_err("a group-readable root cannot hold the dispatch endpoint");

        assert!(matches!(error, DispatchHttpsError::InvalidRoot));
    }

    #[tokio::test]
    async fn endpoint_is_private_and_removed_on_drop() {
        let root = private_root();
        let endpoint = DispatchHttpsEndpoint::bind(
            root_descriptor(&root),
            CanonicalUuid::from_uuid(Uuid::from_u128(LEASE)),
        )
        .expect("the dispatch endpoint binds");
        let socket = endpoint.socket_path().to_owned();
        let mode = endpoint
            .directory
            .metadata()
            .expect("the dispatch directory is inspectable")
            .permissions()
            .mode()
            & 0o7777;

        assert_eq!(mode, DIRECTORY_MODE);
        assert!(socket.exists());
        drop(endpoint);
        assert!(!socket.exists());
        assert!(
            fs::read_dir(root.path())
                .expect("the endpoint fixture root remains inspectable")
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn endpoint_binds_after_the_runner_root_path_is_renamed() {
        let parent = TempDir::new().expect("the endpoint fixture parent is created");
        let original = parent.path().join("runner-root");
        let renamed = parent.path().join("renamed-runner-root");
        fs::create_dir(&original).expect("the endpoint fixture root is created");
        fs::set_permissions(&original, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the endpoint fixture root is effective-user-private");
        let root = File::open(&original).expect("the endpoint fixture root descriptor opens");
        fs::rename(&original, &renamed).expect("the endpoint fixture root path is renamed");

        let endpoint =
            DispatchHttpsEndpoint::bind(root, CanonicalUuid::from_uuid(Uuid::from_u128(LEASE)))
                .expect("the endpoint binds through the retained root descriptor");

        assert!(endpoint.socket_path().exists());
    }

    #[tokio::test]
    async fn endpoint_socket_path_is_bounded_for_a_long_runner_root() {
        let parent = TempDir::new().expect("the endpoint fixture parent is created");
        let root_path = parent.path().join("long-component-".repeat(12));
        fs::create_dir(&root_path).expect("the long endpoint fixture root is created");
        fs::set_permissions(&root_path, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("the long endpoint fixture root is effective-user-private");
        let root = File::open(&root_path).expect("the long fixture root descriptor opens");

        let endpoint =
            DispatchHttpsEndpoint::bind(root, CanonicalUuid::from_uuid(Uuid::from_u128(LEASE)))
                .expect("the bounded dispatch endpoint binds");

        assert!(endpoint.socket_path().as_os_str().len() < 108);
    }

    #[tokio::test]
    async fn startup_reclaims_an_endpoint_stranded_by_termination() {
        let root = private_root();
        let endpoint = DispatchHttpsEndpoint::bind(
            root_descriptor(&root),
            CanonicalUuid::from_uuid(Uuid::from_u128(LEASE)),
        )
        .expect("the dispatch endpoint binds");
        let socket = endpoint.socket_path().to_owned();
        std::mem::forget(endpoint);

        DispatchHttpsEndpoint::reclaim_stale(root_descriptor(&root))
            .expect("the locked-root startup sweep reclaims the stale endpoint");

        assert!(!socket.exists());
        assert!(
            fs::read_dir(root.path())
                .expect("the endpoint fixture root remains inspectable")
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn endpoint_routes_a_connect_request_into_the_checked_broker() {
        let root = private_root();
        let endpoint = DispatchHttpsEndpoint::bind(
            root_descriptor(&root),
            CanonicalUuid::from_uuid(Uuid::from_u128(LEASE)),
        )
        .expect("the dispatch endpoint binds");
        let socket = endpoint.socket_path().to_owned();
        let (stop_sender, stop_receiver) = oneshot::channel();
        let serving = tokio::spawn(endpoint.serve(
            HttpsBroker::production(&[]),
            Instant::now() + Duration::from_secs(5),
            stop_receiver,
        ));
        let mut client = UnixStream::connect(&socket)
            .await
            .expect("the dispatch-local client reaches the endpoint");

        client
            .write_all(b"CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n")
            .await
            .expect("the canonical CONNECT request reaches the endpoint");
        client
            .shutdown()
            .await
            .expect("the rejected client stream shuts down");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("the checked broker closes the rejected CONNECT stream");
        stop_sender
            .send(())
            .expect("the endpoint remains active until explicitly stopped");
        serving
            .await
            .expect("the endpoint task joins")
            .expect("the endpoint accepts the brokered tunnel");

        assert!(response.is_empty());
        assert!(!socket.exists());
    }
}

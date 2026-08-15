//! Per-dispatch serving boundary for the runner-owned HTTPS broker.

use std::{
    error::Error,
    fmt, fs, io,
    os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use signalbox_runner_wire::CanonicalUuid;
use tokio::{net::UnixListener, sync::oneshot, task::JoinSet, time::Instant};

use crate::HttpsBroker;

const DIRECTORY_MODE: u32 = 0o700;
const PERMISSION_MASK: u32 = 0o7777;
const SOCKET_FILE: &str = "https-broker.sock";
const MAXIMUM_TUNNELS: usize = 8;

/// Sanitized failure while preparing or serving one dispatch-scoped endpoint.
#[derive(Debug)]
pub enum DispatchHttpsError {
    /// The owner-private endpoint directory could not be prepared.
    Directory(io::Error),
    /// The supplied runner root was not the owner-private directory contract.
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
    directory: PathBuf,
    socket: PathBuf,
}

impl DispatchHttpsEndpoint {
    /// Binds the endpoint beneath the already-validated owner-private runner root.
    pub fn bind(runner_root: &Path, lease_id: CanonicalUuid) -> Result<Self, DispatchHttpsError> {
        let root_metadata =
            fs::symlink_metadata(runner_root).map_err(DispatchHttpsError::Directory)?;
        if !root_metadata.is_dir()
            || root_metadata.uid() != rustix::process::geteuid().as_raw()
            || root_metadata.permissions().mode() & PERMISSION_MASK != DIRECTORY_MODE
        {
            return Err(DispatchHttpsError::InvalidRoot);
        }
        let directory = runner_root.join(format!("dispatch-{lease_id}"));
        let mut builder = fs::DirBuilder::new();
        builder.mode(DIRECTORY_MODE);
        builder
            .create(&directory)
            .map_err(DispatchHttpsError::Directory)?;
        let socket = directory.join(SOCKET_FILE);
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(source) => {
                let _ = fs::remove_dir(&directory);
                return Err(DispatchHttpsError::Bind(source));
            }
        };
        let endpoint = Self {
            listener,
            directory,
            socket,
        };
        let directory_metadata =
            fs::symlink_metadata(&endpoint.directory).map_err(DispatchHttpsError::Directory)?;
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
                accepted = self.listener.accept(), if tunnels.len() < MAXIMUM_TUNNELS => {
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
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_dir(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, time::Duration};

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
            .expect("the endpoint fixture root is owner-private");
        root
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
            root.path(),
            CanonicalUuid::from_uuid(Uuid::from_u128(LEASE)),
        )
        .expect_err("a group-readable root cannot hold the dispatch endpoint");

        assert!(matches!(error, DispatchHttpsError::InvalidRoot));
    }

    #[tokio::test]
    async fn endpoint_is_private_and_removed_on_drop() {
        let root = private_root();
        let endpoint = DispatchHttpsEndpoint::bind(
            root.path(),
            CanonicalUuid::from_uuid(Uuid::from_u128(LEASE)),
        )
        .expect("the dispatch endpoint binds");
        let socket = endpoint.socket_path().to_owned();
        let directory = socket
            .parent()
            .expect("the dispatch endpoint has a private directory")
            .to_owned();
        let mode = fs::metadata(&directory)
            .expect("the dispatch directory is inspectable")
            .permissions()
            .mode()
            & 0o7777;

        assert_eq!(mode, DIRECTORY_MODE);
        assert!(socket.exists());
        drop(endpoint);
        assert!(!socket.exists());
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn endpoint_routes_a_connect_request_into_the_checked_broker() {
        let root = private_root();
        let endpoint = DispatchHttpsEndpoint::bind(
            root.path(),
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

//! Registration requires an observed ambient bubblewrap profile.

use std::{error::Error, fs, os::unix::fs::PermissionsExt as _, process::Stdio, time::Duration};

use tokio::{
    io::{AsyncBufReadExt as _, BufReader},
    net::UnixListener,
    process::Command,
    time::timeout,
};

#[tokio::test]
async fn ambient_probe_allows_registration_with_host_filesystem_and_network()
-> Result<(), Box<dyn Error>> {
    check_registration(None, true).await
}

#[tokio::test]
async fn successful_exit_without_a_probe_cannot_enroll() -> Result<(), Box<dyn Error>> {
    check_registration(Some("#!/bin/sh\nexit 0\n"), false).await
}

#[tokio::test]
async fn missing_mount_namespace_cannot_enroll() -> Result<(), Box<dyn Error>> {
    check_registration(
        Some("#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n"),
        false,
    )
    .await
}

#[tokio::test]
async fn read_only_host_binding_cannot_enroll() -> Result<(), Box<dyn Error>> {
    check_registration(Some("#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec /usr/bin/bwrap --ro-bind / / --share-net -- \"$@\"\n"), false).await
}

#[tokio::test]
async fn private_network_namespace_cannot_enroll() -> Result<(), Box<dyn Error>> {
    check_registration(Some("#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec /usr/bin/bwrap --dev-bind / / --unshare-net -- \"$@\"\n"), false).await
}

async fn check_registration(wrapper: Option<&str>, admitted: bool) -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("runner.sock");
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let root = directory.path().join("state");
    let bubblewrap = if let Some(wrapper) = wrapper {
        let path = directory.path().join("bwrap");
        fs::write(&path, wrapper)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        path
    } else {
        "/usr/bin/bwrap".into()
    };
    let configuration = directory.path().join("runner.toml");
    let document = toml::toml! {
        version = 1
        capability_classes = ["echo"]
        tools = ["echo"]
        sandbox_profiles = ["ambient"]
        daemon_socket_path = (socket.to_str().ok_or("fixture socket path")?)
        runner_root = (root.to_str().ok_or("fixture root path")?)
        bubblewrap_path = (bubblewrap.to_str().ok_or("fixture bubblewrap path")?)
        read_only_paths = ["/usr"]
        allowed_network_hosts = []
        git_author_name = "Runner fixture"
        git_author_email = "runner@example.invalid"
        credentials = {}
        repositories = {}
    };
    fs::write(&configuration, toml::to_string(&document)?)?;
    let mut runner = Command::new(signalbox_test_bin::test_bin_path!("signalbox-runner"))
        .arg("--config")
        .arg(configuration)
        .env_clear()
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if admitted {
        let (socket, _) = timeout(Duration::from_secs(10), listener.accept()).await??;
        let mut frames = BufReader::new(socket).lines();
        let mut frame = timeout(Duration::from_secs(10), frames.next_line())
            .await??
            .ok_or("enrollment frame missing")?;
        frame.push('\n');
        let frame = signalbox_runner_wire::decode_line(frame.as_bytes())?;
        assert!(matches!(
            frame.message,
            signalbox_runner_wire::Message::Enroll(_)
        ));
        assert!(root.join("operation-journal.json").is_file());
        runner.kill().await?;
        runner.wait().await?;
    } else {
        let output = timeout(Duration::from_secs(10), runner.wait_with_output()).await??;
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("ambient bubblewrap supervisor is unavailable"),
            "{output:?}"
        );
        assert!(
            !root.exists(),
            "failed supervision must not initialize enrollment state"
        );
    }
    Ok(())
}

//! Ambient bubblewrap supervision and its registration probe.

use std::{
    fs, io,
    io::{Read as _, Write as _},
    os::unix::{fs::MetadataExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::Stdio,
};

use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt as _, process::Command};

/// Internal child mode proving the ambient namespace and filesystem bindings.
pub const AMBIENT_PROBE_ARGUMENT: &str = "--probe-ambient";
const SUPERVISOR_LABEL: &str = "signalbox-runner-ambient";

pub(crate) fn command(bubblewrap: &Path, program: &Path, directory: &Path) -> Command {
    let mut command = Command::new(bubblewrap);
    command.as_std_mut().arg0(SUPERVISOR_LABEL);
    command
        .args([
            "--die-with-parent",
            "--dev-bind",
            "/",
            "/",
            "--share-net",
            "--chdir",
        ])
        .arg(directory)
        .arg("--")
        .arg(program)
        .env_clear()
        .current_dir(directory)
        .kill_on_drop(true);
    command
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    mount_namespace: PathBuf,
    network_namespace: PathBuf,
    pid_namespace: PathBuf,
    uid: u32,
    gid: u32,
    root_device: u64,
    root_inode: u64,
    file: PathBuf,
    nonce: String,
}

pub(crate) async fn verify(bubblewrap: &Path) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let root = fs::metadata("/")?;
    let probe = Probe {
        mount_namespace: fs::read_link("/proc/self/ns/mnt")?,
        network_namespace: fs::read_link("/proc/self/ns/net")?,
        pid_namespace: fs::read_link("/proc/self/ns/pid")?,
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        root_device: root.dev(),
        root_inode: root.ino(),
        file: file.path().to_owned(),
        nonce: uuid::Uuid::now_v7().to_string(),
    };
    file.write_all(probe.nonce.as_bytes())?;
    let mut child = command(
        bubblewrap,
        &std::env::current_exe()?,
        &std::env::current_dir()?,
    )
    .arg(AMBIENT_PROBE_ARGUMENT)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()?;
    let mut input = child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?;
    input.write_all(&serde_json::to_vec(&probe)?).await?;
    input.shutdown().await?;
    drop(input);
    if !child.wait().await?.success() || fs::read(file.path())? != b"ambient" {
        return Err(io::Error::other(
            "ambient bubblewrap behavior could not be verified",
        ));
    }
    Ok(())
}

/// Proves mount separation, shared host authority, and a writable host file.
pub fn run_ambient_probe_child() -> io::Result<()> {
    let mut input = Vec::new();
    io::stdin()
        .take(signalbox_runner_wire::MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut input)?;
    if input.len() > signalbox_runner_wire::MAX_FRAME_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let probe: Probe = serde_json::from_slice(&input)?;
    let root = fs::metadata("/")?;
    if fs::read_link("/proc/self/ns/mnt")? == probe.mount_namespace
        || fs::read_link("/proc/self/ns/net")? != probe.network_namespace
        || fs::read_link("/proc/self/ns/pid")? != probe.pid_namespace
        || rustix::process::getuid().as_raw() != probe.uid
        || rustix::process::getgid().as_raw() != probe.gid
        || root.dev() != probe.root_device
        || root.ino() != probe.root_inode
        || fs::read(&probe.file)? != probe.nonce.as_bytes()
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    fs::write(probe.file, b"ambient")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt as _;

    #[tokio::test]
    async fn ambient_child_is_labeled_and_can_write_outside_its_working_directory() -> io::Result<()>
    {
        let working = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let written = outside.path().join("written");
        let output = command(
            Path::new("/usr/bin/bwrap"),
            Path::new("/bin/sh"),
            working.path(),
        )
        .args([
            "-c",
            "printf ambient > \"$1\"; /bin/cat /proc/$PPID/cmdline",
            "probe",
        ])
        .arg(&written)
        .output()
        .await?;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(fs::read(written)?, b"ambient");
        assert_eq!(
            output.stdout.split(|byte| *byte == 0).next(),
            Some(SUPERVISOR_LABEL.as_bytes())
        );
        Ok(())
    }

    #[tokio::test]
    async fn dropping_the_ambient_supervisor_terminates_its_waiting_child() -> io::Result<()> {
        let mut child = command(
            Path::new("/usr/bin/bwrap"),
            Path::new("/bin/sh"),
            Path::new("/"),
        )
        .args(["-c", "printf ready; read value"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
        let _input = child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let mut output = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let mut ready = [0; 5];
        output.read_exact(&mut ready).await?;
        assert_eq!(&ready, b"ready");
        drop(child);
        let mut remaining = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            output.read_to_end(&mut remaining),
        )
        .await
        .map_err(io::Error::other)??;
        assert!(remaining.is_empty());
        Ok(())
    }
}

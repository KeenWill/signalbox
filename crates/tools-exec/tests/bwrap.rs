//! Integration coverage for the real-bubblewrap containment check.
//!
//! `real_bwrap_profile_confines_or_proves_typed_host_refusal` runs the actual
//! `bwrap` binary against the compiled `signalbox-exec-supervisor` and asserts
//! both the daemon-local and runner-restricted request profiles either confine
//! genuinely or return a typed host-refusal outcome, including a real pinned
//! Unix socket relayed through the namespace-local loopback proxy;
//! `real_bwrap_gate` decides when that check is mandatory (CI) versus skipped
//! (unsupported local host, unless opted in).

#![cfg(target_os = "linux")]

use signalbox_test_bin::test_bin_path;
use signalbox_tools_exec::{
    BwrapAvailability, CaptureCompleteness, ExecArguments, ExecutionConfinement, OutputEncoding,
    ProcessOutcome, ProcessSpawnFailure, SandboxedCommandRunner, TokioProcessRunner,
};

const BRIDGE_REQUEST: &str = "bridge request";
const BRIDGE_RESPONSE: &str = "bridge response";

#[tokio::test]
async fn real_bwrap_profile_confines_or_proves_typed_host_refusal()
-> Result<(), Box<dyn std::error::Error>> {
    run_real_bwrap_profile_when_required().await
}

async fn run_real_bwrap_profile_when_required() -> Result<(), Box<dyn std::error::Error>> {
    let ci = std::env::var_os("CI").is_some();
    let opted_in = std::env::var_os("SIGNALBOX_RUN_BWRAP_INTEGRATION").is_some();
    if !real_bwrap_gate(
        procfs_children_available(),
        std::path::Path::new("/usr/bin/bwrap").is_file(),
        ci,
        opted_in,
    )
    .map_err(std::io::Error::other)?
    {
        return Ok(());
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("tools-exec manifest is not nested under the workspace root")?
        .canonicalize()?;
    let process_runner = TokioProcessRunner::try_new(test_bin_path!("signalbox-exec-supervisor"))?;
    let mut runner = SandboxedCommandRunner::try_new(process_runner, &root)?;
    let arguments = ExecArguments {
        program: String::from("test"),
        arguments: vec![String::from("!"), String::from("-e"), String::from("/home")],
        working_directory: String::from("."),
        timeout_seconds: 5,
    };

    let result = runner.try_run(arguments).await?;

    assert_real_bwrap_result(result, ci)?;

    let nested_arguments = ExecArguments {
        program: String::from("sh"),
        arguments: vec![
            String::from("-c"),
            String::from("test \"$(pwd)\" = /workspace/crates/tools-exec"),
        ],
        working_directory: String::from("crates/tools-exec"),
        timeout_seconds: 5,
    };

    let nested_result = runner.try_run(nested_arguments).await?;

    assert_real_bwrap_result(nested_result, ci)?;

    // The other probes hold whether or not the network namespace is unshared,
    // so none of them would notice `--unshare-net` disappearing from the
    // profile. A network namespace of its own is the one containment property
    // that separates this sandbox from a process that can post the workspace to
    // an arbitrary host, and its direct observable is the interface table: a
    // freshly unshared namespace carries the loopback device and nothing else,
    // whereas sharing the host's namespace exposes every host interface.
    let network_arguments = ExecArguments {
        program: String::from("sh"),
        arguments: vec![
            String::from("-c"),
            String::from(
                "grep -q '^ *lo:' /proc/net/dev && test \"$(grep -c : /proc/net/dev)\" -eq 1",
            ),
        ],
        working_directory: String::from("."),
        timeout_seconds: 5,
    };

    let network_result = runner.try_run(network_arguments).await?;

    assert_real_bwrap_result(network_result, ci)?;

    // The interface table proves `lo` exists, not that it works. A fresh
    // namespace can carry a loopback device that is still down, and a workspace
    // test binding a local server would then fail with `ENETUNREACH` while the
    // probe above still passed. Bind a listener and connect to it, so loopback
    // is asserted usable rather than merely present. A missing `python3` shows
    // up as a typed spawn failure rather than a silent pass.
    let loopback_arguments = ExecArguments {
        program: String::from("python3"),
        arguments: vec![
            String::from("-c"),
            String::from(
                "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(1); c=socket.create_connection(s.getsockname()); c.close(); s.close()",
            ),
        ],
        working_directory: String::from("."),
        timeout_seconds: 5,
    };

    let loopback_result = runner.try_run(loopback_arguments).await?;

    assert_real_bwrap_result(loopback_result, ci)?;

    let missing_arguments = ExecArguments {
        program: String::from("signalbox-exec-definitely-missing-target"),
        arguments: Vec::new(),
        working_directory: String::from("."),
        timeout_seconds: 5,
    };

    let missing_result = runner.try_run(missing_arguments).await?;

    assert_real_bwrap_spawn_failure(missing_result)?;

    let process_runner = TokioProcessRunner::try_new(test_bin_path!("signalbox-exec-supervisor"))?;
    let mut runner = SandboxedCommandRunner::try_new_runner_restricted(
        process_runner,
        &root,
        &[std::path::PathBuf::from("/usr")],
    )?;
    let restricted_arguments = ExecArguments {
        program: String::from("test"),
        arguments: vec![String::from("!"), String::from("-e"), String::from("/home")],
        working_directory: String::from("."),
        timeout_seconds: 5,
    };

    let restricted_result = runner.try_run(restricted_arguments).await?;

    assert_real_bwrap_result(restricted_result, ci)?;

    let mut broker = BrokerSocketFixture::new()?;
    let broker_task = broker.spawn_one_tunnel()?;
    let process_runner = TokioProcessRunner::try_new(test_bin_path!("signalbox-exec-supervisor"))?;
    let mut runner = SandboxedCommandRunner::try_new_runner_restricted_with_https_broker(
        process_runner,
        &root,
        &[std::path::PathBuf::from("/usr")],
        broker.socket(),
    )?;
    let bridged_arguments = ExecArguments {
        program: String::from("python3"),
        arguments: vec![
            String::from("-c"),
            format!(
                "import os, socket\nassert not os.path.exists('/run/signalbox/https-broker.sock')\ninaccessible=False\ntry:\n os.listdir('/proc/1/fd')\nexcept PermissionError:\n inaccessible=True\nassert inaccessible\ns=socket.create_connection(('127.0.0.1',18080))\ns.sendall({BRIDGE_REQUEST:?}.encode())\ns.shutdown(socket.SHUT_WR)\nassert s.recv(32)=={BRIDGE_RESPONSE:?}.encode()"
            ),
        ],
        working_directory: String::from("."),
        timeout_seconds: 5,
    };

    let bridged_result = runner.try_run(bridged_arguments).await?;
    let broker_request = broker_task
        .join()
        .map_err(|_| std::io::Error::other("broker fixture thread failed"))??;

    assert_real_bwrap_bridge_observation(
        bridged_result.confinement,
        broker_request.as_deref(),
        ci,
    )?;
    assert_real_bwrap_result(bridged_result, ci)
}

type BrokerTask = std::thread::JoinHandle<std::io::Result<Option<Vec<u8>>>>;

struct BrokerSocketFixture {
    root: std::path::PathBuf,
    socket: std::path::PathBuf,
    listener: Option<std::os::unix::net::UnixListener>,
}

impl BrokerSocketFixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let identity = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "signalbox-bwrap-https-{}-{identity}",
            std::process::id()
        ));
        std::fs::create_dir(&root)?;
        let socket = root.join("broker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket)?;
        Ok(Self {
            root,
            socket,
            listener: Some(listener),
        })
    }

    fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    fn spawn_one_tunnel(&mut self) -> Result<BrokerTask, Box<dyn std::error::Error>> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| std::io::Error::other("broker fixture listener already consumed"))?;
        listener.set_nonblocking(true)?;
        Ok(std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _address)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return Ok(None);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => return Err(error),
                }
            };
            let mut request = Vec::new();
            std::io::Read::read_to_end(&mut stream, &mut request)?;
            std::io::Write::write_all(&mut stream, BRIDGE_RESPONSE.as_bytes())?;
            stream.shutdown(std::net::Shutdown::Write)?;
            Ok(Some(request))
        }))
    }
}

impl Drop for BrokerSocketFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn assert_real_bwrap_bridge_observation(
    confinement: ExecutionConfinement,
    request: Option<&[u8]>,
    ci: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match confinement {
        ExecutionConfinement::FilesystemConfined => {
            assert_eq!(request, Some(BRIDGE_REQUEST.as_bytes()));
            Ok(())
        }
        ExecutionConfinement::SandboxRefused {
            availability: BwrapAvailability::Unusable,
        } => {
            real_bwrap_refusal_policy(ci).map_err(std::io::Error::other)?;
            if request.is_some() {
                return Err(std::io::Error::other("refused bridge opened a broker tunnel").into());
            }
            Ok(())
        }
        confinement => Err(format!("unexpected bridge confinement: {confinement:?}").into()),
    }
}

#[test]
fn real_bwrap_bridge_observation_accepts_the_exact_confined_tunnel() {
    assert!(
        assert_real_bwrap_bridge_observation(
            ExecutionConfinement::FilesystemConfined,
            Some(BRIDGE_REQUEST.as_bytes()),
            false,
        )
        .is_ok()
    );
}

#[test]
fn real_bwrap_bridge_observation_accepts_typed_local_refusal_without_a_tunnel() {
    assert!(
        assert_real_bwrap_bridge_observation(
            ExecutionConfinement::SandboxRefused {
                availability: BwrapAvailability::Unusable,
            },
            None,
            false,
        )
        .is_ok()
    );
}

#[test]
fn real_bwrap_bridge_observation_rejects_a_refused_tunnel() {
    assert!(
        assert_real_bwrap_bridge_observation(
            ExecutionConfinement::SandboxRefused {
                availability: BwrapAvailability::Unusable,
            },
            Some(BRIDGE_REQUEST.as_bytes()),
            false,
        )
        .is_err()
    );
}

fn real_bwrap_gate(
    procfs_children_available: bool,
    bwrap_exists: bool,
    ci: bool,
    opted_in: bool,
) -> Result<bool, &'static str> {
    if !procfs_children_available {
        if ci {
            return Err("CI requires /proc task children support for the real bwrap profile");
        }
        return Ok(false);
    }
    if !bwrap_exists {
        if ci {
            return Err("CI requires /usr/bin/bwrap for the real profile");
        }
        return Ok(false);
    }
    Ok(ci || opted_in)
}

fn assert_real_bwrap_result(
    result: signalbox_tools_exec::ExecResult,
    ci: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match result.confinement {
        ExecutionConfinement::FilesystemConfined => {
            assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
            Ok(())
        }
        ExecutionConfinement::SandboxRefused {
            availability: BwrapAvailability::Unusable,
        } => {
            real_bwrap_refusal_policy(ci).map_err(std::io::Error::other)?;
            assert_eq!(
                result.outcome,
                ProcessOutcome::SpawnFailed {
                    reason: ProcessSpawnFailure::SandboxUnavailable,
                }
            );
            assert!(result.stdout.text.is_empty());
            assert_eq!(result.stdout.completeness, CaptureCompleteness::Complete);
            assert_eq!(result.stdout.encoding, OutputEncoding::Utf8);
            assert!(result.stderr.text.is_empty());
            assert_eq!(result.stderr.completeness, CaptureCompleteness::Complete);
            assert_eq!(result.stderr.encoding, OutputEncoding::Utf8);
            Ok(())
        }
        confinement => Err(format!(
            "unexpected real bubblewrap result: confinement={confinement:?}, outcome={:?}, stdout={:?}, stderr={:?}",
            result.outcome, result.stdout, result.stderr
        )
        .into()),
    }
}

fn real_bwrap_refusal_policy(ci: bool) -> Result<(), &'static str> {
    if ci {
        Err("CI requires the real bwrap profile to confine successfully")
    } else {
        Ok(())
    }
}

fn assert_real_bwrap_spawn_failure(
    result: signalbox_tools_exec::ExecResult,
) -> Result<(), Box<dyn std::error::Error>> {
    match result.confinement {
        ExecutionConfinement::FilesystemConfined => {
            assert_eq!(
                result.outcome,
                ProcessOutcome::SpawnFailed {
                    reason: ProcessSpawnFailure::NotFound,
                }
            );
            Ok(())
        }
        ExecutionConfinement::SandboxRefused {
            availability: BwrapAvailability::Unusable,
        } => {
            assert_eq!(
                result.outcome,
                ProcessOutcome::SpawnFailed {
                    reason: ProcessSpawnFailure::SandboxUnavailable,
                }
            );
            Ok(())
        }
        confinement => Err(format!("unexpected real bubblewrap result: {confinement:?}").into()),
    }
}

fn procfs_children_available() -> bool {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", std::process::id())) else {
        return false;
    };
    let mut observed_task = false;
    for task in tasks {
        let Ok(task) = task else {
            return false;
        };
        let read = std::fs::read_to_string(task.path().join("children"));
        match classify_task_children_read(&read) {
            TaskChildrenReadOutcome::Observed => observed_task = true,
            // /proc/<pid>/task enumerates live threads at read_dir time, but
            // a thread can exit before its children file is read: the tid
            // directory (and the file inside it) then vanishes mid-scan and
            // the read fails with ENOENT. That race means "this thread has
            // no children anymore", not "procfs task-children support is
            // missing" -- skip it and keep scanning rather than flipping the
            // whole verdict to unavailable.
            TaskChildrenReadOutcome::ThreadExited => continue,
            TaskChildrenReadOutcome::Unavailable => return false,
        }
    }
    observed_task
}

#[derive(Debug, PartialEq, Eq)]
enum TaskChildrenReadOutcome {
    Observed,
    ThreadExited,
    Unavailable,
}

fn classify_task_children_read(read: &std::io::Result<String>) -> TaskChildrenReadOutcome {
    match read {
        Ok(_) => TaskChildrenReadOutcome::Observed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            TaskChildrenReadOutcome::ThreadExited
        }
        Err(_) => TaskChildrenReadOutcome::Unavailable,
    }
}

#[test]
fn real_bwrap_gate_rejects_missing_procfs_support_in_ci() {
    assert_eq!(
        real_bwrap_gate(false, true, true, false),
        Err("CI requires /proc task children support for the real bwrap profile")
    );
}

#[test]
fn real_bwrap_gate_skips_missing_procfs_support_outside_ci() {
    assert_eq!(real_bwrap_gate(false, true, false, true), Ok(false));
}

#[test]
fn real_bwrap_gate_runs_with_ci_and_procfs_support() {
    assert_eq!(real_bwrap_gate(true, true, true, false), Ok(true));
}

#[test]
fn real_bwrap_gate_requires_the_installed_binary_in_ci() {
    assert_eq!(
        real_bwrap_gate(true, false, true, false),
        Err("CI requires /usr/bin/bwrap for the real profile")
    );
}

#[test]
fn real_bwrap_refusal_is_rejected_in_ci() {
    assert_eq!(
        real_bwrap_refusal_policy(true),
        Err("CI requires the real bwrap profile to confine successfully")
    );
}

#[test]
fn real_bwrap_refusal_remains_typed_evidence_outside_ci() {
    assert_eq!(real_bwrap_refusal_policy(false), Ok(()));
}

#[test]
fn task_children_read_success_is_observed() {
    assert_eq!(
        classify_task_children_read(&Ok(String::new())),
        TaskChildrenReadOutcome::Observed
    );
}

#[test]
fn task_children_read_missing_file_is_treated_as_thread_exit_race() {
    let vanished = Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "thread exited mid-scan",
    ));
    assert_eq!(
        classify_task_children_read(&vanished),
        TaskChildrenReadOutcome::ThreadExited
    );
}

#[test]
fn task_children_read_other_errors_remain_genuine_unavailability() {
    let denied = Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "no access to task children",
    ));
    assert_eq!(
        classify_task_children_read(&denied),
        TaskChildrenReadOutcome::Unavailable
    );
}

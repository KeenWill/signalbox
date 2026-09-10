//! Integration coverage for the real-bubblewrap containment check.
//!
//! `real_bwrap_profile_confines_or_proves_typed_host_refusal` runs the actual
//! `bwrap` binary against the compiled `signalbox-exec-supervisor` and asserts
//! either genuine filesystem confinement or a typed host-refusal outcome;
//! `real_bwrap_gate` decides when that check is mandatory (CI) versus skipped
//! (unsupported local host, unless opted in).

#![cfg(target_os = "linux")]

use signalbox_test_bin::test_bin_path;
use signalbox_tools_exec::{
    BwrapAvailability, CaptureCompleteness, ExecArguments, ExecutionConfinement, OutputEncoding,
    ProcessOutcome, ProcessSpawnFailure, SandboxConfiguration, SandboxNetwork,
    SandboxProcessNamespace, SandboxedCommandRunner, TokioProcessRunner,
};

const BWRAP_PROCESS_NAMESPACE_ENVIRONMENT: &str = "SIGNALBOX_BWRAP_PROCESS_NAMESPACE";

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
    let root = std::env::current_dir()?
        .join(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("tools-exec manifest is not nested under the workspace root")?
        .canonicalize()?;
    let process_runner = TokioProcessRunner::try_new(test_bin_path!("signalbox-exec-supervisor"))?;
    let process_namespace = bwrap_process_namespace_from_environment()?;
    let mut runner = SandboxedCommandRunner::try_new_with_process_namespace(
        process_runner,
        root,
        process_namespace,
    )?;
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

    assert_real_bwrap_spawn_failure(missing_result)
}

fn bwrap_process_namespace_from_environment()
-> Result<SandboxProcessNamespace, Box<dyn std::error::Error>> {
    match std::env::var(BWRAP_PROCESS_NAMESPACE_ENVIRONMENT) {
        Err(std::env::VarError::NotPresent) => Ok(SandboxProcessNamespace::Private),
        Ok(value) if value == "container" => Ok(SandboxProcessNamespace::Container),
        Ok(value) => Err(format!(
            "{BWRAP_PROCESS_NAMESPACE_ENVIRONMENT} must be exactly `container`, got `{value}`"
        )
        .into()),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("{BWRAP_PROCESS_NAMESPACE_ENVIRONMENT} must be valid Unicode").into())
        }
    }
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

#[tokio::test]
async fn configured_host_runtime_validates_inside_real_bwrap()
-> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("SIGNALBOX_RUN_BWRAP_INTEGRATION").is_none() {
        return Ok(());
    }
    let workspace = tempfile::tempdir()?;
    let runtime = tempfile::tempdir()?;
    let bin = runtime.path().join("bin");
    std::fs::create_dir(&bin)?;
    for program in ["cargo", "node"] {
        let source = host_output("sh", &["-c", &format!("command -v {program}")])?;
        std::fs::copy(source, bin.join(program))?;
    }
    let rustup_home = std::path::PathBuf::from(host_output("rustup", &["show", "home"])?);
    let rustc = std::path::PathBuf::from(host_output(
        "rustup",
        &["which", "--toolchain", "stable", "rustc"],
    )?);
    let toolchain = rustc
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("toolchain directory missing")?;
    let toolchain_name = toolchain
        .file_name()
        .ok_or("toolchain name missing")?
        .to_str()
        .ok_or("toolchain name is not UTF-8")?;
    let npm = std::path::PathBuf::from(host_output("sh", &["-c", "command -v npm"])?);
    let node_bin = npm.parent().ok_or("npm bin directory missing")?;
    let node_profile = node_bin.parent().ok_or("node profile missing")?;
    let configuration = SandboxConfiguration {
        network: SandboxNetwork::Host,
        read_only_binds: vec![
            runtime.path().to_owned(),
            rustup_home.clone(),
            node_profile.to_owned(),
        ],
        path_prepend: vec![bin.clone(), toolchain.join("bin"), node_bin.to_owned()],
        rustup_home: Some(rustup_home),
        rustup_toolchain: Some(toolchain_name.to_owned()),
    };
    let process_runner = TokioProcessRunner::try_new(test_bin_path!("signalbox-exec-supervisor"))?;
    let runner = SandboxedCommandRunner::try_new_with_process_namespace(
        process_runner,
        workspace.path(),
        bwrap_process_namespace_from_environment()?,
    )?;
    let mut isolated = runner.clone();
    for program in ["cargo", "node"] {
        let result = isolated
            .try_run(ExecArguments {
                program: bin.join(program).to_string_lossy().into_owned(),
                arguments: vec![String::from("--version")],
                working_directory: String::from("."),
                timeout_seconds: 30,
            })
            .await?;
        assert_eq!(
            result.confinement,
            ExecutionConfinement::FilesystemConfined,
            "{result:?}"
        );
        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::NotFound
            },
            "{result:?}"
        );
    }
    let mut configured = runner.with_sandbox_configuration(configuration.clone());
    run_successfully(&mut configured, "cargo", &["--version"], ".").await?;
    run_successfully(&mut configured, "node", &["--version"], ".").await?;
    let runtime_assertions = format!(
        concat!(
            "test ! -e /root && ! touch '{}/write-probe' && ",
            "test \"$(command -v cargo)\" = '{}/cargo' && ",
            "test \"$(command -v node)\" = '{}/node' && ",
            "test \"$CARGO_HOME\" = /workspace/.cargo && ",
            "test \"$npm_config_cache\" = /workspace/.npm && ",
            "test \"$RUSTUP_AUTO_INSTALL\" = 0"
        ),
        runtime.path().display(),
        bin.display(),
        bin.display(),
    );
    run_successfully(&mut configured, "sh", &["-c", &runtime_assertions], ".").await?;
    std::fs::create_dir(workspace.path().join("src"))?;
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"sandbox-validation\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(
        workspace.path().join("src/lib.rs"),
        "#[test]\nfn arithmetic() {\n    assert_eq!(2 + 2, 4);\n}\n",
    )?;
    run_successfully(
        &mut configured,
        "cargo",
        &["fmt", "--all", "--", "--check"],
        ".",
    )
    .await?;
    run_successfully(&mut configured, "cargo", &["check"], ".").await?;
    run_successfully(&mut configured, "cargo", &["test", "--no-fail-fast"], ".").await?;
    run_successfully(
        &mut configured,
        "curl",
        &[
            "--fail",
            "--max-time",
            "30",
            "-sI",
            "https://index.crates.io/config.json",
        ],
        ".",
    )
    .await?;
    std::fs::create_dir_all(workspace.path().join("clients/web"))?;
    std::fs::write(
        workspace.path().join("clients/web/package.json"),
        r#"{"name":"sandbox-validation","version":"0.1.0","dependencies":{"left-pad":"1.3.0"}}"#,
    )?;
    run_successfully(
        &mut configured,
        "npm",
        &["install", "--package-lock-only"],
        "clients/web",
    )
    .await?;
    assert!(
        workspace
            .path()
            .join("clients/web/package-lock.json")
            .is_file()
    );
    let mut no_network = configured.with_sandbox_configuration(SandboxConfiguration {
        network: SandboxNetwork::None,
        ..configuration
    });
    let result = no_network
        .try_run(ExecArguments {
            program: String::from("curl"),
            arguments: [
                "--fail",
                "--max-time",
                "5",
                "-sI",
                "https://index.crates.io/config.json",
            ]
            .map(String::from)
            .to_vec(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        })
        .await?;
    assert_eq!(
        result.confinement,
        ExecutionConfinement::FilesystemConfined,
        "{result:?}"
    );
    assert!(
        matches!(result.outcome, ProcessOutcome::Exited { code: Some(code) } if code != 0),
        "{result:?}"
    );
    Ok(())
}

fn host_output(program: &str, arguments: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()?;
    if !output.status.success() {
        return Err(format!("{program} {arguments:?}: {output:?}").into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn run_successfully(
    runner: &mut SandboxedCommandRunner<TokioProcessRunner>,
    program: &str,
    arguments: &[&str],
    working_directory: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = runner
        .try_run(ExecArguments {
            program: program.to_owned(),
            arguments: arguments.iter().map(|value| (*value).to_owned()).collect(),
            working_directory: working_directory.to_owned(),
            timeout_seconds: 300,
        })
        .await?;
    assert_eq!(
        result.confinement,
        ExecutionConfinement::FilesystemConfined,
        "{program} {arguments:?}: {result:?}"
    );
    assert_eq!(
        result.outcome,
        ProcessOutcome::Exited { code: Some(0) },
        "{program} {arguments:?}: {result:?}"
    );
    Ok(())
}

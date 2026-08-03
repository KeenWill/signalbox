#![cfg(target_os = "linux")]

use signalbox_tools_exec::{
    BwrapAvailability, CaptureCompleteness, ExecArguments, ExecutionConfinement, OutputEncoding,
    ProcessOutcome, ProcessSpawnFailure, SandboxedCommandRunner, TokioProcessRunner,
};

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
    let process_runner =
        TokioProcessRunner::try_new(env!("CARGO_BIN_EXE_signalbox-exec-supervisor"))?;
    let mut runner = SandboxedCommandRunner::try_new(process_runner, root)?;
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

    let missing_arguments = ExecArguments {
        program: String::from("signalbox-exec-definitely-missing-target"),
        arguments: Vec::new(),
        working_directory: String::from("."),
        timeout_seconds: 5,
    };

    let missing_result = runner.try_run(missing_arguments).await?;

    assert_real_bwrap_spawn_failure(missing_result)
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
        if std::fs::read_to_string(task.path().join("children")).is_err() {
            return false;
        }
        observed_task = true;
    }
    observed_task
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

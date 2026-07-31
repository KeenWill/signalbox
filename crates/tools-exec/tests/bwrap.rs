#![cfg(target_os = "linux")]

use signalbox_tools_exec::{
    BwrapAvailability, ExecArguments, ExecutionConfinement, ProcessOutcome, SandboxedCommandRunner,
    TokioProcessRunner,
};

#[tokio::test]
async fn real_bwrap_profile_hides_ambient_home_directory() -> Result<(), Box<dyn std::error::Error>>
{
    run_real_bwrap_profile_when_required().await
}

async fn run_real_bwrap_profile_when_required() -> Result<(), Box<dyn std::error::Error>> {
    if !procfs_children_available()
        || std::env::var_os("CI").is_none()
            && std::env::var_os("SIGNALBOX_RUN_BWRAP_INTEGRATION").is_none()
    {
        return Ok(());
    }
    let root = std::env::current_dir()?;
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

    assert_eq!(result.confinement, ExecutionConfinement::FilesystemConfined);
    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    Ok(())
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
fn bwrap_gate_distinguishes_missing_from_unusable_evidence() {
    assert_ne!(BwrapAvailability::Missing, BwrapAvailability::Unusable);
    assert_ne!(BwrapAvailability::TimedOut, BwrapAvailability::Unusable);
}

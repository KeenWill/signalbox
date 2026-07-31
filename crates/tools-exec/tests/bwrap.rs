#![cfg(target_os = "linux")]

use std::process::Command;

use signalbox_tools_exec::{
    BwrapAvailability, ExecArguments, ExecutionConfinement, ProcessOutcome, SandboxedCommandRunner,
    TokioProcessRunner,
};

#[tokio::test]
async fn real_bwrap_profile_hides_ambient_home_directory() -> Result<(), Box<dyn std::error::Error>>
{
    assert_profile_hides_ambient_home_if_bwrap_exists().await
}

async fn assert_profile_hides_ambient_home_if_bwrap_exists()
-> Result<(), Box<dyn std::error::Error>> {
    if !host_supports_bwrap() {
        return Ok(());
    }
    let root = std::env::current_dir()?;
    let mut runner = SandboxedCommandRunner::try_new(TokioProcessRunner, root)?;
    let arguments = ExecArguments {
        program: String::from("/usr/bin/test"),
        arguments: vec![String::from("!"), String::from("-e"), String::from("/home")],
        working_directory: String::from("."),
        timeout_seconds: 5,
    };

    let result = runner.try_run(arguments).await?;

    assert_eq!(result.confinement, ExecutionConfinement::FilesystemConfined);
    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    Ok(())
}

fn host_supports_bwrap() -> bool {
    Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--", "/bin/true"])
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn bwrap_gate_distinguishes_missing_from_unusable_evidence() {
    assert_ne!(BwrapAvailability::Missing, BwrapAvailability::Unusable);
}

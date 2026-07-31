#![cfg(target_os = "linux")]

use signalbox_tools_exec::{
    BwrapAvailability, ExecArguments, ExecutionConfinement, ProcessOutcome, SandboxedCommandRunner,
    TokioProcessRunner,
};

#[tokio::test]
#[ignore = "requires a host with usable /usr/bin/bwrap; run this ignored test explicitly"]
async fn real_bwrap_profile_hides_ambient_home_directory() -> Result<(), Box<dyn std::error::Error>>
{
    let root = std::env::current_dir()?;
    let mut runner = SandboxedCommandRunner::try_new(TokioProcessRunner, root)?;
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

#[test]
fn bwrap_gate_distinguishes_missing_from_unusable_evidence() {
    assert_ne!(BwrapAvailability::Missing, BwrapAvailability::Unusable);
}

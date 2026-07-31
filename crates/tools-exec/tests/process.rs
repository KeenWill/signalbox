#![cfg(unix)]

use std::{collections::BTreeMap, ffi::OsString, time::Duration};

use signalbox_tools_exec::{
    CaptureCompleteness, ProcessEnvironment, ProcessOutcome, ProcessRequest, ProcessRunner,
    TokioProcessRunner,
};

const OBSERVED_OUTPUT: &str = "12345";
const CAPTURE_BYTES: usize = 4;
const ESCAPED_COMPLETION_SCRIPT: &str = "/usr/bin/setsid /bin/sh -c 'sleep 30' >/dev/null 2>&1 & child=$!; sleep 0.05; printf %s $child";
const ESCAPED_TIMEOUT_SCRIPT: &str =
    "/usr/bin/setsid /bin/sh -c 'sleep 30' >/dev/null 2>&1 & printf %s $!; wait";
const EXPLICIT_ENVIRONMENT_NAME: &str = "SIGNALBOX_EXEC_FIXTURE";
const EXPLICIT_ENVIRONMENT_VALUE: &str = "visible";

#[tokio::test]
async fn production_runner_reports_observed_bytes_beyond_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let request = ProcessRequest {
        program: OsString::from("/usr/bin/printf"),
        arguments: vec![OsString::from(OBSERVED_OUTPUT)],
        working_directory: std::env::current_dir()?,
        timeout: Duration::from_secs(5),
        capture_bytes: CAPTURE_BYTES,
        environment: BTreeMap::new(),
        environment_inheritance: ProcessEnvironment::Clear,
    };

    let result = TokioProcessRunner.run(request).await;

    assert_eq!(
        result.stdout.bytes,
        OBSERVED_OUTPUT.as_bytes()[..CAPTURE_BYTES]
    );
    assert_eq!(result.stdout.completeness, CaptureCompleteness::Truncated);
    Ok(())
}

#[tokio::test]
async fn production_runner_clears_ambient_environment_when_requested()
-> Result<(), Box<dyn std::error::Error>> {
    let request = ProcessRequest {
        program: OsString::from("/usr/bin/env"),
        arguments: Vec::new(),
        working_directory: std::env::current_dir()?,
        timeout: Duration::from_secs(5),
        capture_bytes: 1024,
        environment: BTreeMap::from([(
            OsString::from(EXPLICIT_ENVIRONMENT_NAME),
            OsString::from(EXPLICIT_ENVIRONMENT_VALUE),
        )]),
        environment_inheritance: ProcessEnvironment::Clear,
    };

    let result = TokioProcessRunner.run(request).await;

    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    assert_eq!(
        result.stdout.bytes,
        explicit_environment_output().as_bytes()
    );
    Ok(())
}

#[tokio::test]
async fn production_runner_kills_descendants_after_leader_completion()
-> Result<(), Box<dyn std::error::Error>> {
    let request = ProcessRequest {
        program: OsString::from("/bin/sh"),
        arguments: vec![
            OsString::from("-c"),
            OsString::from("sleep 30 & printf %s $!"),
        ],
        working_directory: std::env::current_dir()?,
        timeout: Duration::from_secs(5),
        capture_bytes: 64,
        environment: BTreeMap::new(),
        environment_inheritance: ProcessEnvironment::Clear,
    };

    let result = TokioProcessRunner.run(request).await;
    let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
    Ok(())
}

#[tokio::test]
async fn production_runner_kills_descendants_on_timeout() -> Result<(), Box<dyn std::error::Error>>
{
    let request = ProcessRequest {
        program: OsString::from("/bin/sh"),
        arguments: vec![
            OsString::from("-c"),
            OsString::from("sleep 30 & printf %s $!; wait"),
        ],
        working_directory: std::env::current_dir()?,
        timeout: Duration::from_millis(100),
        capture_bytes: 64,
        environment: BTreeMap::new(),
        environment_inheritance: ProcessEnvironment::Clear,
    };

    let result = TokioProcessRunner.run(request).await;
    let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(result.outcome, ProcessOutcome::TimedOut);
    assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
    Ok(())
}

#[tokio::test]
async fn production_runner_reaps_new_session_descendant_after_leader_completion()
-> Result<(), Box<dyn std::error::Error>> {
    let request = shell_request(
        ESCAPED_COMPLETION_SCRIPT,
        Duration::from_secs(5),
        std::env::current_dir()?,
    );

    let result = TokioProcessRunner.run(request).await;
    let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;

    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
    Ok(())
}

#[tokio::test]
async fn production_runner_reaps_new_session_descendant_on_timeout()
-> Result<(), Box<dyn std::error::Error>> {
    let timeout = Duration::from_millis(100);
    let request = shell_request(ESCAPED_TIMEOUT_SCRIPT, timeout, std::env::current_dir()?);
    let started = std::time::Instant::now();

    let result = TokioProcessRunner.run(request).await;
    let elapsed = started.elapsed();
    let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;

    assert_eq!(result.outcome, ProcessOutcome::TimedOut);
    assert!(elapsed < Duration::from_secs(2));
    assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
    Ok(())
}

#[tokio::test]
async fn production_runner_reaps_new_session_descendant_on_cancellation()
-> Result<(), Box<dyn std::error::Error>> {
    let pid_file = std::path::PathBuf::from(format!(
        "/tmp/signalbox-tools-exec-cancel-{}",
        std::process::id()
    ));
    let script = format!(
        "/usr/bin/setsid /bin/sh -c 'sleep 30' >/dev/null 2>&1 & printf %s $! > {}; wait",
        pid_file.display()
    );
    let request = shell_request(&script, Duration::from_secs(30), std::env::current_dir()?);
    let task = tokio::spawn(async move { TokioProcessRunner.run(request).await });
    let descendant = await_pid_file(&pid_file).await?;

    task.abort();
    let cancellation = task.await;
    let _ = std::fs::remove_file(&pid_file);

    assert!(cancellation.is_err());
    assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
    Ok(())
}

fn shell_request(
    script: &str,
    timeout: Duration,
    working_directory: std::path::PathBuf,
) -> ProcessRequest {
    ProcessRequest {
        program: OsString::from("/bin/sh"),
        arguments: vec![OsString::from("-c"), OsString::from(script)],
        working_directory,
        timeout,
        capture_bytes: 64,
        environment: BTreeMap::new(),
        environment_inheritance: ProcessEnvironment::Clear,
    }
}

async fn await_pid_file(path: &std::path::Path) -> Result<u32, Box<dyn std::error::Error>> {
    for _ in 0..100 {
        if let Ok(value) = std::fs::read_to_string(path)
            && let Ok(pid) = value.parse::<u32>()
        {
            return Ok(pid);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(std::io::Error::other("descendant pid fixture was not written").into())
}

fn explicit_environment_output() -> String {
    format!("{EXPLICIT_ENVIRONMENT_NAME}={EXPLICIT_ENVIRONMENT_VALUE}\n")
}

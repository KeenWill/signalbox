#![cfg(unix)]

use std::{collections::BTreeMap, ffi::OsString, time::Duration};

use signalbox_tools_exec::{
    CaptureCompleteness, ProcessOutcome, ProcessRequest, ProcessRunner, TokioProcessRunner,
};

const OBSERVED_OUTPUT: &str = "12345";
const CAPTURE_BYTES: usize = 4;

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
    };

    let result = TokioProcessRunner.run(request).await;
    let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(result.outcome, ProcessOutcome::TimedOut);
    assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
    Ok(())
}

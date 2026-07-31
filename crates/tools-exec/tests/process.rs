#![cfg(target_os = "linux")]

use std::{collections::BTreeMap, ffi::OsString, time::Duration};

use signalbox_tools_exec::{
    CaptureCompleteness, ProcessEnvironment, ProcessOutcome, ProcessRequest, ProcessRunner,
    TokioProcessRunner,
};

const OBSERVED_OUTPUT: &str = "12345";
const CAPTURE_BYTES: usize = 4;
const EXPLICIT_ENVIRONMENT_NAME: &str = "SIGNALBOX_EXEC_FIXTURE";
const EXPLICIT_ENVIRONMENT_VALUE: &str = "visible";

#[tokio::test]
async fn production_runner_reports_observed_bytes_beyond_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let request = ProcessRequest {
        program: fixture_program("printf")?.into_os_string(),
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
        program: fixture_program("env")?.into_os_string(),
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
    let script = format!("{} 30 & printf %s $!", fixture_program("sleep")?.display());
    let request = ProcessRequest {
        program: fixture_program("sh")?.into_os_string(),
        arguments: vec![OsString::from("-c"), OsString::from(script)],
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
    let script = format!(
        "{} 30 & printf %s $!; wait",
        fixture_program("sleep")?.display()
    );
    let request = ProcessRequest {
        program: fixture_program("sh")?.into_os_string(),
        arguments: vec![OsString::from("-c"), OsString::from(script)],
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
        &escaped_completion_script()?,
        Duration::from_secs(5),
        std::env::current_dir()?,
    )?;

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
    let request = shell_request(
        &escaped_timeout_script()?,
        timeout,
        std::env::current_dir()?,
    )?;
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
        "{} {} -c '{} 30' >/dev/null 2>&1 & printf %s $! > {}; wait",
        fixture_program("setsid")?.display(),
        fixture_program("sh")?.display(),
        fixture_program("sleep")?.display(),
        pid_file.display()
    );
    let request = shell_request(&script, Duration::from_secs(30), std::env::current_dir()?)?;
    let task = tokio::spawn(async move { TokioProcessRunner.run(request).await });
    let descendant = await_pid_file(&pid_file).await?;

    task.abort();
    let cancellation = task.await;
    await_process_absent(descendant).await?;
    let _ = std::fs::remove_file(&pid_file);

    assert!(cancellation.is_err());
    assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
    Ok(())
}

#[tokio::test]
async fn production_runner_does_not_kill_unrelated_concurrent_child()
-> Result<(), Box<dyn std::error::Error>> {
    let script = format!("{} 0.25", fixture_program("sleep")?.display());
    let request = shell_request(&script, Duration::from_secs(5), std::env::current_dir()?)?;
    let execution = tokio::spawn(async move { TokioProcessRunner.run(request).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut unrelated = std::process::Command::new(fixture_program("sleep")?)
        .arg("30")
        .spawn()?;

    let result = execution.await?;
    let survived = unrelated.try_wait()?.is_none();
    unrelated.kill()?;
    unrelated.wait()?;

    assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
    assert!(survived);
    Ok(())
}

#[tokio::test]
async fn concurrent_execution_timeout_starts_without_global_queueing()
-> Result<(), Box<dyn std::error::Error>> {
    let long_script = format!("{} 2", fixture_program("sleep")?.display());
    let short_script = format!("{} 30", fixture_program("sleep")?.display());
    let long_request = shell_request(
        &long_script,
        Duration::from_secs(5),
        std::env::current_dir()?,
    )?;
    let short_request = shell_request(
        &short_script,
        Duration::from_millis(100),
        std::env::current_dir()?,
    )?;
    let long_execution = tokio::spawn(async move { TokioProcessRunner.run(long_request).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = std::time::Instant::now();

    let short_result = TokioProcessRunner.run(short_request).await;
    let short_elapsed = started.elapsed();
    let long_result = long_execution.await?;

    assert_eq!(short_result.outcome, ProcessOutcome::TimedOut);
    assert!(short_elapsed < Duration::from_millis(1500));
    assert_eq!(
        long_result.outcome,
        ProcessOutcome::Exited { code: Some(0) }
    );
    Ok(())
}

async fn await_process_absent(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(std::io::Error::other("descendant survived bounded cleanup").into())
}

fn shell_request(
    script: &str,
    timeout: Duration,
    working_directory: std::path::PathBuf,
) -> Result<ProcessRequest, Box<dyn std::error::Error>> {
    Ok(ProcessRequest {
        program: fixture_program("sh")?.into_os_string(),
        arguments: vec![OsString::from("-c"), OsString::from(script)],
        working_directory,
        timeout,
        capture_bytes: 64,
        environment: BTreeMap::new(),
        environment_inheritance: ProcessEnvironment::Clear,
    })
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

fn escaped_completion_script() -> Result<String, Box<dyn std::error::Error>> {
    Ok(format!(
        "{} {} -c '{} 30' >/dev/null 2>&1 & child=$!; {} 0.05; printf %s $child",
        fixture_program("setsid")?.display(),
        fixture_program("sh")?.display(),
        fixture_program("sleep")?.display(),
        fixture_program("sleep")?.display()
    ))
}

fn escaped_timeout_script() -> Result<String, Box<dyn std::error::Error>> {
    Ok(format!(
        "{} {} -c '{} 30' >/dev/null 2>&1 & printf %s $!; wait",
        fixture_program("setsid")?.display(),
        fixture_program("sh")?.display(),
        fixture_program("sleep")?.display()
    ))
}

fn fixture_program(name: &str) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            std::io::Error::other(format!("fixture program `{name}` is unavailable")).into()
        })
}

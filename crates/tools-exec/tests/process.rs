#![cfg(target_os = "linux")]

use std::{collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, time::Duration};

use signalbox_tools_exec::{
    CaptureCompleteness, ProcessEnvironment, ProcessOutcome, ProcessRequest, ProcessRunner,
    TokioProcessRunner,
};

const OBSERVED_OUTPUT: &str = "12345";
const CAPTURE_BYTES: usize = 4;
const DISPATCH_MODE: &str = "--dispatch";
const EXPECTED_DISPATCH_MARKER: &[u8] = b"signalbox-exec:dispatched\n";
const EXPLICIT_ENVIRONMENT_NAME: &str = "SIGNALBOX_EXEC_FIXTURE";
const EXPLICIT_ENVIRONMENT_VALUE: &str = "visible";
const SUCCESSFUL_EXIT_CODE: i32 = 0;
const LEGITIMATE_TARGET_EXIT_CODE: i32 = 127;
const REPLACEMENT_SUPERVISOR_EXIT_CODE: i32 = 99;
const SUPERVISOR_PIN_DIRECTORY: &str = "signalbox-exec-supervisor-pin";

#[tokio::test]
async fn production_runner_pins_the_supervisor_executable_identity()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let directory =
            std::env::temp_dir().join(format!("{SUPERVISOR_PIN_DIRECTORY}-{}", std::process::id()));
        std::fs::create_dir(&directory)?;
        let supplied = directory.join("supervisor");
        let moved = directory.join("original-supervisor");
        std::fs::copy(env!("CARGO_BIN_EXE_signalbox-exec-supervisor"), &supplied)?;
        std::fs::set_permissions(&supplied, std::fs::Permissions::from_mode(0o700))?;
        let mut runner = TokioProcessRunner::try_new(&supplied)?;
        std::fs::rename(&supplied, &moved)?;
        std::fs::write(
            &supplied,
            format!("#!/bin/sh\nexit {REPLACEMENT_SUPERVISOR_EXIT_CODE}\n"),
        )?;
        std::fs::set_permissions(&supplied, std::fs::Permissions::from_mode(0o700))?;
        let request = ProcessRequest {
            program: fixture_program("true")?.into_os_string(),
            arguments: Vec::new(),
            working_directory: std::env::current_dir()?,
            timeout: Duration::from_secs(5),
            capture_bytes: 1024,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
        };

        let result = runner.run(request).await;
        drop(runner);
        std::fs::remove_dir_all(directory)?;

        assert_eq!(
            result.outcome,
            ProcessOutcome::Exited {
                code: Some(SUCCESSFUL_EXIT_CODE),
            }
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_preserves_outer_spawn_path_failure()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let request = ProcessRequest {
            program: fixture_program("true")?.into_os_string(),
            arguments: Vec::new(),
            working_directory: std::env::current_dir()?.join("missing-exec-working-directory"),
            timeout: Duration::from_secs(5),
            capture_bytes: 1024,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
        };

        let result = production_runner()?.run(request).await;

        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: signalbox_tools_exec::ProcessSpawnFailure::NotFound,
            }
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_reports_observed_bytes_beyond_limit()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let request = ProcessRequest {
            program: fixture_program("printf")?.into_os_string(),
            arguments: vec![OsString::from(OBSERVED_OUTPUT)],
            working_directory: std::env::current_dir()?,
            timeout: Duration::from_secs(5),
            capture_bytes: CAPTURE_BYTES,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
        };

        let result = production_runner()?.run(request).await;

        assert_eq!(
            result.stdout.bytes,
            OBSERVED_OUTPUT.as_bytes()[..CAPTURE_BYTES]
        );
        assert_eq!(result.stdout.completeness, CaptureCompleteness::Truncated);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_clears_ambient_environment_when_requested()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
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

        let result = production_runner()?.run(request).await;

        assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
        assert_eq!(
            result.stdout.bytes,
            explicit_environment_output().as_bytes()
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_dispatcher_marks_started_target_that_exits_127()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let supervisor = std::path::PathBuf::from(env!("CARGO_BIN_EXE_signalbox-exec-supervisor"));
        let request = ProcessRequest {
            program: supervisor.into_os_string(),
            arguments: vec![
                OsString::from(DISPATCH_MODE),
                fixture_program("sh")?.into_os_string(),
                OsString::from("-c"),
                OsString::from(format!("exit {LEGITIMATE_TARGET_EXIT_CODE}")),
            ],
            working_directory: std::env::current_dir()?,
            timeout: Duration::from_secs(5),
            capture_bytes: 1024,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
        };

        let result = production_runner()?.run(request).await;

        assert_eq!(
            result.outcome,
            ProcessOutcome::Exited {
                code: Some(LEGITIMATE_TARGET_EXIT_CODE),
            }
        );
        assert_eq!(result.stderr.bytes, EXPECTED_DISPATCH_MARKER);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_kills_descendants_after_leader_completion()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
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

        let result = production_runner()?.run(request).await;
        let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
        assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_kills_descendants_on_timeout() -> Result<(), Box<dyn std::error::Error>>
{
    with_procfs_supervision(async {
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

        let result = production_runner()?.run(request).await;
        let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(result.outcome, ProcessOutcome::TimedOut);
        assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_reaps_new_session_descendant_after_leader_completion()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let request = shell_request(
            &escaped_completion_script()?,
            Duration::from_secs(5),
            std::env::current_dir()?,
        )?;

        let result = production_runner()?.run(request).await;
        let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;

        assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
        assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_reaps_new_session_descendant_on_timeout()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let timeout = Duration::from_millis(100);
        let request = shell_request(
            &escaped_timeout_script()?,
            timeout,
            std::env::current_dir()?,
        )?;
        let started = std::time::Instant::now();

        let result = production_runner()?.run(request).await;
        let elapsed = started.elapsed();
        let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;

        assert_eq!(result.outcome, ProcessOutcome::TimedOut);
        assert!(elapsed < Duration::from_secs(2));
        assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_reaps_new_session_descendant_on_cancellation()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
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
        let mut runner = production_runner()?;
        let task = tokio::spawn(async move { runner.run(request).await });
        let descendant = await_pid_file(&pid_file).await?;

        task.abort();
        let cancellation = task.await;
        await_process_absent(descendant).await?;
        let _ = std::fs::remove_file(&pid_file);

        assert!(cancellation.is_err());
        assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_does_not_kill_unrelated_concurrent_child()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let script = format!("{} 0.25", fixture_program("sleep")?.display());
        let request = shell_request(&script, Duration::from_secs(5), std::env::current_dir()?)?;
        let mut runner = production_runner()?;
        let execution = tokio::spawn(async move { runner.run(request).await });
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
    })
    .await
}

#[tokio::test]
async fn concurrent_execution_timeout_starts_without_global_queueing()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
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
        let mut long_runner = production_runner()?;
        let long_execution = tokio::spawn(async move { long_runner.run(long_request).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();

        let short_result = production_runner()?.run(short_request).await;
        let short_elapsed = started.elapsed();
        let long_result = long_execution.await?;

        assert_eq!(short_result.outcome, ProcessOutcome::TimedOut);
        assert!(short_elapsed < Duration::from_millis(1500));
        assert_eq!(
            long_result.outcome,
            ProcessOutcome::Exited { code: Some(0) }
        );
        Ok(())
    })
    .await
}

async fn with_procfs_supervision<Test>(test: Test) -> Result<(), Box<dyn std::error::Error>>
where
    Test: std::future::Future<Output = Result<(), Box<dyn std::error::Error>>>,
{
    if !procfs_children_available() {
        return Ok(());
    }
    test.await
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

fn production_runner() -> Result<TokioProcessRunner, Box<dyn std::error::Error>> {
    Ok(TokioProcessRunner::try_new(env!(
        "CARGO_BIN_EXE_signalbox-exec-supervisor"
    ))?)
}

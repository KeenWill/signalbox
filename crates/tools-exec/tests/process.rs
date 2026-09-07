//! Integration coverage for `SandboxedCommandRunner` / `TokioProcessRunner`
//! driving the real compiled `signalbox-exec-supervisor` binary.
//!
//! Exercises supervisor-executable identity pinning, spawn-failure
//! propagation at each stage, capture-byte limits, ambient-environment
//! clearing, dispatch-marker and exit/signal reporting, target stderr
//! forwarding, and descendant process-tree cleanup on both leader
//! completion and timeout.

#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use signalbox_test_bin::test_bin_path;
use signalbox_tools_exec::{
    CaptureCompleteness, ProcessEnvironment, ProcessOutcome, ProcessRequest, ProcessRunner,
    ProcessStatusProtocol, ProcessSupervisionFailure, TokioProcessRunner,
};

const OBSERVED_OUTPUT: &str = "12345";
const CAPTURE_BYTES: usize = 4;
const DISPATCH_MODE: &str = "--dispatch";
const EXPECTED_DISPATCH_MARKER: &[u8] = b"signalbox-exec:dispatched\n";
const EXPLICIT_ENVIRONMENT_NAME: &str = "SIGNALBOX_EXEC_FIXTURE";
const EXPLICIT_ENVIRONMENT_VALUE: &str = "visible";
const SUCCESSFUL_EXIT_CODE: i32 = 0;
const LEGITIMATE_TARGET_EXIT_CODE: i32 = 127;
const TARGET_STDERR_OUTPUT: &str = "target-stderr";
/// Complete stderr expected when a dispatched target writes
/// `TARGET_STDERR_OUTPUT`: the dispatch marker, then the forwarded bytes.
const EXPECTED_MARKER_THEN_TARGET_STDERR: &[u8] = b"signalbox-exec:dispatched\ntarget-stderr";
/// Bounds the stderr-forwarding regression so a deadlocked supervisor fails
/// that test as `TimedOut` instead of hanging the suite; otherwise arbitrary.
const DEADLOCK_BOUNDING_TIMEOUT: Duration = Duration::from_secs(5);
/// Arbitrary capture budget comfortably above the marker-plus-payload length,
/// so a `Complete` capture reflects forwarding rather than budget truncation.
const FORWARDED_STDERR_CAPTURE_BUDGET: usize = 1024;
const TARGET_TERMINATION_SIGNAL: &str = "PIPE";
const REPLACEMENT_SUPERVISOR_EXIT_CODE: i32 = 99;
const SUPERVISOR_PIN_DIRECTORY: &str = "signalbox-exec-supervisor-pin";
const PARENT_KILL_PID_FILE: &str = "signalbox-exec-parent-kill";
const GRANDPARENT_KILL_PID_FILE: &str = "signalbox-exec-grandparent-kill";
const ESCAPED_SUPERVISOR_KILL_PID_FILE: &str = "signalbox-exec-escaped-supervisor-kill";
const CANCELLATION_PID_FILE: &str = "signalbox-tools-exec-cancel";
const PROCESS_GROUP_KILL_TIME_LIMIT: Duration = Duration::from_secs(5);
struct TemporaryPath {
    path: PathBuf,
}

impl TemporaryPath {
    fn new(stem: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let identity = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("{stem}-{}-{identity}", std::process::id()));
        Ok(Self { path })
    }

    fn as_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test]
async fn production_runner_pins_the_supervisor_executable_identity()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let directory = TemporaryPath::new(SUPERVISOR_PIN_DIRECTORY)?;
        std::fs::create_dir(directory.as_path())?;
        let supplied = directory.as_path().join("supervisor");
        let moved = directory.as_path().join("original-supervisor");
        std::fs::copy(test_bin_path!("signalbox-exec-supervisor"), &supplied)?;
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
            status_protocol: ProcessStatusProtocol::Direct,
        };

        let result = runner.run(request).await;
        drop(runner);

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
            status_protocol: ProcessStatusProtocol::Direct,
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
async fn production_runner_preserves_requested_program_spawn_failure()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let missing = std::env::temp_dir().join(format!(
            "signalbox-exec-missing-target-{}",
            std::process::id()
        ));
        let request = ProcessRequest {
            program: missing.into_os_string(),
            arguments: Vec::new(),
            working_directory: std::env::current_dir()?,
            timeout: Duration::from_secs(5),
            capture_bytes: 1024,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
            status_protocol: ProcessStatusProtocol::Direct,
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
            status_protocol: ProcessStatusProtocol::Direct,
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
            status_protocol: ProcessStatusProtocol::Direct,
        };

        let result = production_runner()?.run(request).await;

        assert_eq!(
            result.outcome,
            ProcessOutcome::Exited {
                code: Some(SUCCESSFUL_EXIT_CODE),
            }
        );
        assert_eq!(
            result.stdout.bytes,
            explicit_environment_output().as_bytes()
        );
        assert_eq!(result.stdout.completeness, CaptureCompleteness::Complete);
        assert_eq!(result.stderr.completeness, CaptureCompleteness::Complete);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_dispatcher_marks_started_target_that_exits_127()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let supervisor = test_bin_path!("signalbox-exec-supervisor");
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
            status_protocol: ProcessStatusProtocol::SandboxDispatch,
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

/// A dispatched target that writes to stderr must still be reaped and reported.
///
/// The supervisor writes its dispatch marker under `std::io::stderr().lock()`.
/// Holding that guard across the target run deadlocks the helper thread that
/// copies the target's stderr, because that helper writes through
/// `&mut std::io::stderr()` and `ReentrantLock` re-enters only for the thread
/// that already owns it. No other dispatch test runs a target that writes to
/// stderr, so none of them can reach the deadlock; this one does.
#[tokio::test]
async fn production_dispatcher_forwards_target_stderr_without_deadlocking()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let supervisor = test_bin_path!("signalbox-exec-supervisor");
        let request = ProcessRequest {
            program: supervisor.into_os_string(),
            arguments: vec![
                OsString::from(DISPATCH_MODE),
                fixture_program("sh")?.into_os_string(),
                OsString::from("-c"),
                OsString::from(format!("printf %s {TARGET_STDERR_OUTPUT} 1>&2")),
            ],
            working_directory: std::env::current_dir()?,
            timeout: DEADLOCK_BOUNDING_TIMEOUT,
            capture_bytes: FORWARDED_STDERR_CAPTURE_BUDGET,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
            status_protocol: ProcessStatusProtocol::SandboxDispatch,
        };

        let result = production_runner()?.run(request).await;

        assert_eq!(
            result.outcome,
            ProcessOutcome::Exited {
                code: Some(SUCCESSFUL_EXIT_CODE),
            }
        );
        assert_eq!(result.stderr.bytes, EXPECTED_MARKER_THEN_TARGET_STDERR);
        assert_eq!(result.stderr.completeness, CaptureCompleteness::Complete);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_dispatcher_preserves_target_spawn_failure()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let supervisor = test_bin_path!("signalbox-exec-supervisor");
        let missing = std::env::temp_dir().join(format!(
            "signalbox-exec-missing-dispatched-target-{}",
            std::process::id()
        ));
        let request = ProcessRequest {
            program: supervisor.into_os_string(),
            arguments: vec![OsString::from(DISPATCH_MODE), missing.into_os_string()],
            working_directory: std::env::current_dir()?,
            timeout: Duration::from_secs(5),
            capture_bytes: 1024,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
            status_protocol: ProcessStatusProtocol::SandboxDispatch,
        };

        let result = production_runner()?.run(request).await;

        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: signalbox_tools_exec::ProcessSpawnFailure::NotFound,
            }
        );
        assert_eq!(result.stderr.bytes, EXPECTED_DISPATCH_MARKER);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_dispatcher_preserves_target_signal_termination()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let supervisor = test_bin_path!("signalbox-exec-supervisor");
        let request = ProcessRequest {
            program: supervisor.into_os_string(),
            arguments: vec![
                OsString::from(DISPATCH_MODE),
                fixture_program("sh")?.into_os_string(),
                OsString::from("-c"),
                OsString::from(format!("kill -{TARGET_TERMINATION_SIGNAL} $$")),
            ],
            working_directory: std::env::current_dir()?,
            timeout: Duration::from_secs(5),
            capture_bytes: 1024,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
            status_protocol: ProcessStatusProtocol::SandboxDispatch,
        };

        let result = production_runner()?.run(request).await;

        assert_eq!(result.outcome, ProcessOutcome::Exited { code: None });
        assert_eq!(result.stderr.bytes, EXPECTED_DISPATCH_MARKER);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_dispatcher_does_not_wait_for_descendant_held_pipes()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let supervisor = test_bin_path!("signalbox-exec-supervisor");
        let script = format!("{} 30 & printf %s $!", fixture_program("sleep")?.display());
        let request = ProcessRequest {
            program: supervisor.into_os_string(),
            arguments: vec![
                OsString::from(DISPATCH_MODE),
                fixture_program("sh")?.into_os_string(),
                OsString::from("-c"),
                OsString::from(script),
            ],
            working_directory: std::env::current_dir()?,
            timeout: Duration::from_secs(5),
            capture_bytes: 64,
            environment: BTreeMap::new(),
            environment_inheritance: ProcessEnvironment::Clear,
            status_protocol: ProcessStatusProtocol::SandboxDispatch,
        };
        let started = std::time::Instant::now();

        let result = production_runner()?.run(request).await;
        let elapsed = started.elapsed();
        let descendant = std::str::from_utf8(&result.stdout.bytes)?.parse::<u32>()?;

        assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
        assert_eq!(result.stdout.completeness, CaptureCompleteness::Truncated);
        assert_eq!(result.stderr.completeness, CaptureCompleteness::Truncated);
        assert!(elapsed < Duration::from_secs(2));
        assert!(!std::path::Path::new(&format!("/proc/{descendant}")).exists());
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
            status_protocol: ProcessStatusProtocol::Direct,
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
            status_protocol: ProcessStatusProtocol::Direct,
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
async fn production_runner_deadline_expires_before_supervisor_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let dispatch_marker = TemporaryPath::new("signalbox-exec-deadline-dispatch")?;
        let script = format!("printf started > {}", dispatch_marker.as_path().display());
        let request = shell_request(&script, Duration::ZERO, std::env::current_dir()?)?;

        let result = production_runner()?.run(request).await;

        assert_eq!(result.outcome, ProcessOutcome::TimedOut);
        assert!(!dispatch_marker.as_path().exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_does_not_retain_process_wide_subreaper_state()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let result = production_runner()?
            .run(shell_request(
                "exit 0",
                Duration::from_secs(5),
                std::env::current_dir()?,
            )?)
            .await;
        let orphan_parent = orphan_parent_after_runner_completion()?;

        assert_eq!(result.outcome, ProcessOutcome::Exited { code: Some(0) });
        assert_ne!(orphan_parent, std::process::id());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_survives_target_killing_its_direct_parent()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let pid_file = TemporaryPath::new(PARENT_KILL_PID_FILE)?;
        let script = format!(
            "printf %s $$ > {}; kill -KILL $PPID; exec {} 30",
            pid_file.as_path().display(),
            fixture_program("sleep")?.display()
        );
        let request = shell_request(&script, Duration::from_secs(5), std::env::current_dir()?)?;
        let started = std::time::Instant::now();

        let result = production_runner()?.run(request).await;
        let elapsed = started.elapsed();
        let target = std::fs::read_to_string(pid_file.as_path())?.parse::<u32>()?;
        await_process_absent(target).await?;

        assert_eq!(
            result.outcome,
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            }
        );
        assert!(elapsed < PROCESS_GROUP_KILL_TIME_LIMIT);
        assert!(!std::path::Path::new(&format!("/proc/{target}")).exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_survives_target_killing_the_authority_supervisor()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let pid_file = TemporaryPath::new(GRANDPARENT_KILL_PID_FILE)?;
        let script = format!(
            "supervisor=$({} '/^PPid:/ {{print $2}}' /proc/$PPID/status); printf %s $$ > {}; kill -KILL $supervisor; exec {} 30",
            fixture_program("awk")?.display(),
            pid_file.as_path().display(),
            fixture_program("sleep")?.display()
        );
        let request = shell_request(&script, Duration::from_secs(5), std::env::current_dir()?)?;
        let started = std::time::Instant::now();

        let result = production_runner()?.run(request).await;
        let elapsed = started.elapsed();
        let target = std::fs::read_to_string(pid_file.as_path())?.parse::<u32>()?;
        await_process_absent(target).await?;

        assert_eq!(
            result.outcome,
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            }
        );
        assert!(elapsed < Duration::from_secs(2));
        assert!(!std::path::Path::new(&format!("/proc/{target}")).exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_reaps_new_session_target_after_supervisor_kill()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let pid_file = TemporaryPath::new(ESCAPED_SUPERVISOR_KILL_PID_FILE)?;
        let script = format!(
            "supervisor=$({} \"/^PPid:/ {{print \\$2}}\" /proc/$PPID/status); {} {} -c \"printf %s \\$\\$ > {}; kill -KILL $supervisor; exec {} 30\"",
            fixture_program("awk")?.display(),
            fixture_program("setsid")?.display(),
            fixture_program("sh")?.display(),
            pid_file.as_path().display(),
            fixture_program("sleep")?.display()
        );
        let request = shell_request(&script, Duration::from_secs(5), std::env::current_dir()?)?;
        let started = std::time::Instant::now();

        let result = production_runner()?.run(request).await;
        let elapsed = started.elapsed();
        let target = std::fs::read_to_string(pid_file.as_path())?.parse::<u32>()?;
        await_process_absent(target).await?;

        assert_eq!(
            result.outcome,
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            }
        );
        assert!(elapsed < Duration::from_secs(2));
        assert!(!std::path::Path::new(&format!("/proc/{target}")).exists());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn production_runner_reaps_new_session_target_after_process_group_kill()
-> Result<(), Box<dyn std::error::Error>> {
    with_procfs_supervision(async {
        let pid_file = TemporaryPath::new("signalbox-exec-process-group-kill-pid")?;
        let script = format!(
            "{} {} -c \"printf %s \\$\\$ > {}; exec {} 30\" & {} 0.05; kill -KILL 0",
            fixture_program("setsid")?.display(),
            fixture_program("sh")?.display(),
            pid_file.as_path().display(),
            fixture_program("sleep")?.display(),
            fixture_program("sleep")?.display()
        );
        let request = shell_request(&script, Duration::from_secs(5), std::env::current_dir()?)?;
        let started = std::time::Instant::now();

        let result = production_runner()?.run(request).await;
        let elapsed = started.elapsed();
        let target = std::fs::read_to_string(pid_file.as_path())?.parse::<u32>()?;
        await_process_absent(target).await?;

        assert_eq!(
            result.outcome,
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            }
        );
        assert!(elapsed < PROCESS_GROUP_KILL_TIME_LIMIT);
        assert!(!std::path::Path::new(&format!("/proc/{target}")).exists());
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
        let pid_file = TemporaryPath::new(CANCELLATION_PID_FILE)?;
        let script = format!(
            "{} {} -c '{} 30' >/dev/null 2>&1 & printf %s $! > {}; wait",
            fixture_program("setsid")?.display(),
            fixture_program("sh")?.display(),
            fixture_program("sleep")?.display(),
            pid_file.as_path().display()
        );
        let request = shell_request(&script, Duration::from_secs(30), std::env::current_dir()?)?;
        let mut runner = production_runner()?;
        let task = tokio::spawn(async move { runner.run(request).await });
        let descendant = await_pid_file(pid_file.as_path()).await?;

        task.abort();
        let cancellation = task.await;
        await_process_absent(descendant).await?;
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
        match std::fs::read_to_string(task.path().join("children")) {
            Ok(_) => observed_task = true,
            // /proc/<pid>/task enumerates live threads at read_dir time, but
            // a thread can exit before its children file is read: the tid
            // directory (and the file inside it) then vanishes mid-scan and
            // the read fails with ENOENT. That race means "this thread has
            // no children anymore", not "procfs task-children support is
            // missing" -- skip it and keep scanning rather than flipping the
            // whole verdict to unavailable. See tests/bwrap.rs's
            // `classify_task_children_read` for the same policy with unit
            // coverage of this exact decision.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        }
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
        status_protocol: ProcessStatusProtocol::Direct,
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

fn orphan_parent_after_runner_completion() -> Result<u32, Box<dyn std::error::Error>> {
    let shell = fixture_program("sh")?;
    let setsid = fixture_program("setsid")?;
    let script = format!(
        "{} '/^PPid:/ {{print $2}}' /proc/$$/status",
        fixture_program("awk")?.display()
    );
    let output = std::process::Command::new(setsid)
        .arg("--fork")
        .arg(shell)
        .arg("-c")
        .arg(script)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other("orphan fixture failed to start").into());
    }
    Ok(std::str::from_utf8(&output.stdout)?.trim().parse::<u32>()?)
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
    Ok(TokioProcessRunner::try_new(test_bin_path!(
        "signalbox-exec-supervisor"
    ))?)
}

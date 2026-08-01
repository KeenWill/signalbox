#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::OsString,
        io::{Read, Write},
        os::unix::process::CommandExt,
        process::{Command, ExitCode, ExitStatus, Stdio},
        sync::{
            Arc, Mutex, PoisonError,
            atomic::{AtomicBool, Ordering},
        },
        thread::JoinHandle,
        time::{Duration, Instant},
    };

    mod supervisor_protocol {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/supervisor_protocol.rs"
        ));
    }

    use supervisor_protocol::{
        LAUNCH_STATUS_TAIL_BYTES, LAUNCH_STATUS_TRAILER, LauncherStatus,
        SupervisorCaptureCompleteness, SupervisorFailureStage, SupervisorSpawnFailure,
        SupervisorStatus,
    };

    const MINIMUM_POLL_INTERVAL: Duration = Duration::from_millis(2);
    const MAXIMUM_POLL_INTERVAL: Duration = Duration::from_millis(100);
    const REAP_DEADLINE: Duration = Duration::from_secs(1);
    const DISPATCH_MODE: &str = "--dispatch";
    const CARGO_TEST_RUNNER_MODE: &str = "--cargo-test-runner";
    const LAUNCH_MODE: &str = "--launch";
    const OUTER_MODE: &str = "--outer";
    const SELF_EXE: &str = "/proc/self/exe";
    const DISPATCH_MARKER: &[u8] = b"signalbox-exec:dispatched\n";
    const STATUS_TRAILER: &[u8] = b"\n\0signalbox-exec-supervisor-status:";
    const MAX_TEST_OUTPUT_LINE_BYTES: usize = 16 * 1024;

    enum TargetFailure {
        Spawn(std::io::Error),
        Supervision,
    }

    struct TargetExit {
        status: ExitStatus,
        stdout: SupervisorCaptureCompleteness,
        stderr: SupervisorCaptureCompleteness,
    }

    struct LauncherRead {
        tail: Vec<u8>,
        parsed: Option<(usize, LauncherStatus)>,
    }

    #[derive(Clone, Copy)]
    enum SupervisionCompletion {
        LauncherExited { success: bool },
        Status(SupervisorStatus),
    }

    pub(super) fn entrypoint() -> ExitCode {
        let mut arguments = std::env::args_os().skip(1);
        let Some(mode_or_timeout) = arguments.next() else {
            return ExitCode::FAILURE;
        };
        if mode_or_timeout == DISPATCH_MODE {
            return dispatch(arguments.collect());
        }
        if mode_or_timeout == CARGO_TEST_RUNNER_MODE {
            return cargo_test_runner(arguments.collect());
        }
        if mode_or_timeout == LAUNCH_MODE {
            return launch(arguments.collect());
        }
        if mode_or_timeout == OUTER_MODE {
            let Some(timeout) = arguments.next() else {
                return ExitCode::FAILURE;
            };
            return match run_outer_supervisor(timeout, arguments.collect()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(()) => ExitCode::FAILURE,
            };
        }
        match run_supervisor(mode_or_timeout, arguments.collect()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(()) => ExitCode::FAILURE,
        }
    }

    fn run_supervisor(timeout: OsString, mut arguments: Vec<OsString>) -> Result<(), ()> {
        let timeout_milliseconds = parse_u64(timeout)?;
        if arguments.is_empty() {
            return Err(());
        }
        let program = arguments.remove(0);
        let status = supervise(
            program,
            arguments,
            Duration::from_millis(timeout_milliseconds),
        );
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(STATUS_TRAILER).map_err(|_| ())?;
        serde_json::to_writer(&mut stdout, &status).map_err(|_| ())?;
        stdout.write_all(b"\n").map_err(|_| ())?;
        stdout.flush().map_err(|_| ())?;
        Ok(())
    }

    fn parse_u64(value: OsString) -> Result<u64, ()> {
        value
            .into_string()
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or(())
    }

    fn run_outer_supervisor(timeout: OsString, mut arguments: Vec<OsString>) -> Result<(), ()> {
        let timeout_milliseconds = parse_u64(timeout.clone())?;
        if arguments.is_empty() || read_control_byte().is_err() {
            return Err(());
        }
        let started = Instant::now();
        let pidfd_reservation = preflight_process_tree()?;
        let program = arguments.remove(0);
        let mut command = Command::new(SELF_EXE);
        command
            .arg(timeout)
            .arg(program)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(0);
        let mut child = command.spawn().map_err(|_| ())?;
        drop(pidfd_reservation);
        let Some(mut control) = child.stdin.take() else {
            terminate_child(&mut child);
            cleanup_untracked_children(std::process::id());
            return Err(());
        };
        let mut tree = match ProcessTreeGuard::new(child.id(), std::process::id()) {
            Ok(tree) => tree,
            Err(()) => {
                terminate_child(&mut child);
                cleanup_untracked_children(std::process::id());
                return Err(());
            }
        };
        control.write_all(&[1]).map_err(|_| ())?;
        let cancelled = cancellation_signal();
        loop {
            match tree.live_descendant_beyond_root() {
                Ok(true) => break,
                Ok(false) => {}
                Err(()) => return Err(()),
            }
            if cancelled.load(Ordering::Acquire)
                || started.elapsed() >= Duration::from_millis(timeout_milliseconds)
            {
                drop(control);
                let _ = tree.finish(&mut child);
                return Err(());
            }
            std::thread::sleep(MINIMUM_POLL_INTERVAL);
        }
        if cancelled.load(Ordering::Acquire)
            || started.elapsed() >= Duration::from_millis(timeout_milliseconds)
        {
            drop(control);
            let _ = tree.finish(&mut child);
            return Err(());
        }
        control.write_all(&[1]).map_err(|_| ())?;
        let mut child_success = None;
        let mut child_wait_failed = false;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    child_success = Some(status.success());
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    child_wait_failed = true;
                    break;
                }
            }
            if cancelled.load(Ordering::Acquire)
                || started.elapsed() >= Duration::from_millis(timeout_milliseconds)
                || !tree.process_tree_supported()
            {
                break;
            }
            std::thread::sleep(MINIMUM_POLL_INTERVAL);
        }
        drop(control);
        match tree.finish(&mut child) {
            CleanupStatus::Complete { root_success }
                if !child_wait_failed && child_success.unwrap_or(root_success) =>
            {
                Ok(())
            }
            CleanupStatus::Complete { .. }
            | CleanupStatus::ProcessTreeUnsupported { .. }
            | CleanupStatus::Failed { .. } => Err(()),
        }
    }

    fn dispatch(arguments: Vec<OsString>) -> ExitCode {
        if arguments.is_empty() {
            return ExitCode::FAILURE;
        }
        let mut stderr = std::io::stderr().lock();
        if stderr
            .write_all(DISPATCH_MARKER)
            .and_then(|()| stderr.flush())
            .is_err()
        {
            return ExitCode::FAILURE;
        }
        emit_target_status(arguments)
    }

    #[derive(Clone, Copy, serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum CargoTestEventOutcome {
        Passed,
        Failed,
        Ignored,
    }

    #[derive(serde::Serialize)]
    struct CargoTestEvent<'a> {
        reason: &'static str,
        executable: &'a str,
        name: &'a str,
        outcome: CargoTestEventOutcome,
    }

    #[derive(serde::Serialize)]
    struct CargoTestSourceTruncated {
        reason: &'static str,
    }

    #[derive(Default)]
    struct CargoTestOutputParser {
        started: bool,
        remaining: usize,
    }

    impl CargoTestOutputParser {
        fn observe_line<Target: Write>(
            &mut self,
            line: &[u8],
            executable: &str,
            target: &mut Target,
        ) -> std::io::Result<()> {
            let Ok(line) = std::str::from_utf8(line) else {
                return Ok(());
            };
            let line = line.strip_suffix('\r').unwrap_or(line);
            if !self.started
                && let Some(count) = parse_libtest_count(line)
            {
                self.started = true;
                self.remaining = count;
                return Ok(());
            }
            if self.remaining == 0 {
                return Ok(());
            }
            let Some((name, outcome)) = parse_libtest_status(line) else {
                return Ok(());
            };
            self.remaining -= 1;
            serde_json::to_writer(
                &mut *target,
                &CargoTestEvent {
                    reason: "signalbox-test-result",
                    executable,
                    name,
                    outcome,
                },
            )
            .map_err(std::io::Error::other)?;
            target.write_all(b"\n")?;
            target.flush()
        }

        fn observe_truncated<Target: Write>(&self, target: &mut Target) -> std::io::Result<()> {
            if !self.started || self.remaining == 0 {
                return Ok(());
            }
            serde_json::to_writer(
                &mut *target,
                &CargoTestSourceTruncated {
                    reason: "signalbox-test-source-truncated",
                },
            )
            .map_err(std::io::Error::other)?;
            target.write_all(b"\n")?;
            target.flush()
        }
    }

    fn parse_libtest_count(line: &str) -> Option<usize> {
        let count = line.strip_prefix("running ")?;
        count
            .strip_suffix(" tests")
            .or_else(|| count.strip_suffix(" test"))?
            .parse()
            .ok()
    }

    fn parse_libtest_status(line: &str) -> Option<(&str, CargoTestEventOutcome)> {
        let stripped = line.strip_prefix("test ")?;
        let (name, outcome) = stripped.rsplit_once(" ... ")?;
        let outcome = match outcome {
            "ok" => CargoTestEventOutcome::Passed,
            "FAILED" => CargoTestEventOutcome::Failed,
            "ignored" => CargoTestEventOutcome::Ignored,
            value if value.starts_with("ignored, ") => CargoTestEventOutcome::Ignored,
            _ => return None,
        };
        Some((name, outcome))
    }

    fn cargo_test_runner(mut arguments: Vec<OsString>) -> ExitCode {
        if arguments.is_empty() {
            return ExitCode::FAILURE;
        }
        let program = arguments.remove(0);
        let executable = program.to_string_lossy().into_owned();
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let Ok(mut child) = command.spawn() else {
            return ExitCode::FAILURE;
        };
        let Some(mut child_stdout) = child.stdout.take() else {
            terminate_child(&mut child);
            return ExitCode::FAILURE;
        };
        let emitted = emit_cargo_test_events(
            &mut child_stdout,
            &executable,
            &mut std::io::stdout().lock(),
        );
        if emitted.is_err() {
            terminate_child(&mut child);
            return ExitCode::FAILURE;
        }
        match child.wait() {
            Ok(status) if status.success() => ExitCode::SUCCESS,
            Ok(status) => status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .map(ExitCode::from)
                .unwrap_or(ExitCode::FAILURE),
            Err(_) => {
                terminate_child(&mut child);
                ExitCode::FAILURE
            }
        }
    }

    fn emit_cargo_test_events<Source: Read, Target: Write>(
        source: &mut Source,
        executable: &str,
        target: &mut Target,
    ) -> std::io::Result<()> {
        let mut parser = CargoTestOutputParser::default();
        let mut read_buffer = [0_u8; 8 * 1024];
        let mut line = Vec::new();
        let mut discarding = false;
        loop {
            let read = source.read(&mut read_buffer)?;
            if read == 0 {
                if discarding {
                    parser.observe_truncated(target)?;
                } else if !line.is_empty() {
                    parser.observe_line(&line, executable, target)?;
                }
                return Ok(());
            }
            for byte in &read_buffer[..read] {
                if *byte == b'\n' {
                    if discarding {
                        parser.observe_truncated(target)?;
                    } else {
                        parser.observe_line(&line, executable, target)?;
                    }
                    line.clear();
                    discarding = false;
                } else if line.len() < MAX_TEST_OUTPUT_LINE_BYTES {
                    line.push(*byte);
                } else {
                    line.clear();
                    discarding = true;
                }
            }
        }
    }

    fn launch(arguments: Vec<OsString>) -> ExitCode {
        if read_control_byte().is_err() {
            return ExitCode::FAILURE;
        }
        emit_target_status(arguments)
    }

    fn emit_target_status(arguments: Vec<OsString>) -> ExitCode {
        let status = match run_target(arguments) {
            Ok(target) => LauncherStatus::Exited {
                code: target.status.code(),
                stdout: target.stdout,
                stderr: target.stderr,
            },
            Err(TargetFailure::Spawn(error)) => LauncherStatus::SpawnFailed {
                reason: spawn_failure(&error),
            },
            Err(TargetFailure::Supervision) => LauncherStatus::SupervisionFailed,
        };
        let mut stdout = std::io::stdout().lock();
        let written = stdout
            .write_all(LAUNCH_STATUS_TRAILER)
            .and_then(|()| {
                serde_json::to_writer(&mut stdout, &status).map_err(std::io::Error::other)
            })
            .and_then(|()| stdout.write_all(b"\n"))
            .and_then(|()| stdout.flush());
        if written.is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    fn run_target(mut arguments: Vec<OsString>) -> Result<TargetExit, TargetFailure> {
        if arguments.is_empty() {
            return Err(TargetFailure::Supervision);
        }
        let program = arguments.remove(0);
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return Err(TargetFailure::Spawn(error)),
        };
        let Some(mut target_stdout) = child.stdout.take() else {
            terminate_child(&mut child);
            return Err(TargetFailure::Supervision);
        };
        let Some(mut target_stderr) = child.stderr.take() else {
            terminate_child(&mut child);
            return Err(TargetFailure::Supervision);
        };
        let leader_exited = Arc::new(AtomicBool::new(false));
        let stdout_leader_exited = Arc::clone(&leader_exited);
        let stdout_copy = std::thread::spawn(move || {
            copy_until_leader_exit(
                &mut target_stdout,
                &mut std::io::stdout(),
                &stdout_leader_exited,
            )
        });
        let stderr_leader_exited = Arc::clone(&leader_exited);
        let stderr_copy = std::thread::spawn(move || {
            copy_until_leader_exit(
                &mut target_stderr,
                &mut std::io::stderr(),
                &stderr_leader_exited,
            )
        });
        let status = match child.wait() {
            Ok(status) => status,
            Err(_) => {
                terminate_child(&mut child);
                leader_exited.store(true, Ordering::Release);
                let _ = stdout_copy.join();
                let _ = stderr_copy.join();
                return Err(TargetFailure::Supervision);
            }
        };
        leader_exited.store(true, Ordering::Release);
        let stdout = stdout_copy
            .join()
            .map_err(|_| TargetFailure::Supervision)?
            .map_err(|_| TargetFailure::Supervision)?;
        let stderr = stderr_copy
            .join()
            .map_err(|_| TargetFailure::Supervision)?
            .map_err(|_| TargetFailure::Supervision)?;
        Ok(TargetExit {
            status,
            stdout,
            stderr,
        })
    }

    fn copy_until_leader_exit<Source, Target>(
        source: &mut Source,
        target: &mut Target,
        leader_exited: &AtomicBool,
    ) -> std::io::Result<SupervisorCaptureCompleteness>
    where
        Source: Read + rustix::fd::AsFd,
        Target: Write,
    {
        let flags = rustix::fs::fcntl_getfl(&*source)?;
        rustix::fs::fcntl_setfl(&*source, flags | rustix::fs::OFlags::NONBLOCK)?;
        let mut buffer = [0_u8; 8 * 1024];
        let mut post_exit_remaining = None;
        loop {
            if post_exit_remaining.is_none() && leader_exited.load(Ordering::Acquire) {
                let remaining = rustix::io::ioctl_fionread(&*source)?;
                post_exit_remaining = Some(remaining);
            }
            if post_exit_remaining == Some(0) {
                return match source.read(&mut buffer[..1]) {
                    Ok(0) => Ok(SupervisorCaptureCompleteness::Complete),
                    Ok(read) => {
                        target.write_all(&buffer[..read])?;
                        target.flush()?;
                        Ok(SupervisorCaptureCompleteness::Incomplete)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        Ok(SupervisorCaptureCompleteness::Incomplete)
                    }
                    Err(error) => Err(error),
                };
            }
            let read_limit = post_exit_remaining
                .map(|remaining| usize::try_from(remaining).unwrap_or(usize::MAX))
                .unwrap_or(buffer.len())
                .min(buffer.len());
            match source.read(&mut buffer[..read_limit]) {
                Ok(0) => return Ok(SupervisorCaptureCompleteness::Complete),
                Ok(read) => {
                    target.write_all(&buffer[..read])?;
                    target.flush()?;
                    if let Some(remaining) = post_exit_remaining.as_mut() {
                        *remaining = remaining.saturating_sub(read as u64);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(MINIMUM_POLL_INTERVAL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn terminate_child(child: &mut std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn supervise(
        program: OsString,
        arguments: Vec<OsString>,
        timeout: Duration,
    ) -> SupervisorStatus {
        if read_control_byte().is_err() {
            return SupervisorStatus::SupervisionFailed {
                stage: SupervisorFailureStage::Cleanup,
            };
        }
        let pidfd_reservation = match preflight_process_tree() {
            Ok(reservation) => reservation,
            Err(()) => {
                return SupervisorStatus::SpawnFailed {
                    reason: SupervisorSpawnFailure::ProcessTreeUnsupported,
                };
            }
        };
        let mut command = Command::new(SELF_EXE);
        command
            .arg(LAUNCH_MODE)
            .arg(program)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return SupervisorStatus::SpawnFailed {
                    reason: spawn_failure(&error),
                };
            }
        };
        drop(pidfd_reservation);
        let Some(mut launcher_control) = child.stdin.take() else {
            terminate_child(&mut child);
            cleanup_untracked_children(std::process::id());
            return SupervisorStatus::SupervisionFailed {
                stage: SupervisorFailureStage::Cleanup,
            };
        };
        let Some(launcher_stdout) = child.stdout.take() else {
            terminate_child(&mut child);
            cleanup_untracked_children(std::process::id());
            return SupervisorStatus::SupervisionFailed {
                stage: SupervisorFailureStage::Cleanup,
            };
        };
        let launcher_reader = std::thread::spawn(move || read_launcher_stdout(launcher_stdout));
        let mut tree = match ProcessTreeGuard::new(child.id(), std::process::id()) {
            Ok(tree) => tree,
            Err(()) => {
                terminate_child(&mut child);
                cleanup_untracked_children(std::process::id());
                let _ = emit_launcher_stdout(
                    launcher_reader,
                    LauncherOutputTrust::UntrustedTargetBytes,
                );
                return SupervisorStatus::SupervisionFailed {
                    stage: SupervisorFailureStage::Cleanup,
                };
            }
        };
        if read_control_byte().is_err() || launcher_control.write_all(&[1]).is_err() {
            let cleanup = tree.finish(&mut child);
            let _ =
                emit_launcher_stdout(launcher_reader, LauncherOutputTrust::UntrustedTargetBytes);
            return match cleanup {
                CleanupStatus::Complete { .. } => SupervisorStatus::Cancelled,
                CleanupStatus::ProcessTreeUnsupported { .. } | CleanupStatus::Failed { .. } => {
                    SupervisorStatus::SupervisionFailed {
                        stage: SupervisorFailureStage::Cleanup,
                    }
                }
            };
        }
        drop(launcher_control);
        let started = Instant::now();
        let cancelled = cancellation_signal();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    break match tree.live_descendant_beyond_root() {
                        Ok(_) => SupervisionCompletion::LauncherExited {
                            success: status.success(),
                        },
                        Err(()) => {
                            SupervisionCompletion::Status(SupervisorStatus::SupervisionFailed {
                                stage: SupervisorFailureStage::Cleanup,
                            })
                        }
                    };
                }
                Ok(None) => {}
                Err(_) => {
                    break SupervisionCompletion::Status(SupervisorStatus::SupervisionFailed {
                        stage: SupervisorFailureStage::Wait,
                    });
                }
            }
            if cancelled.load(Ordering::Acquire) {
                break SupervisionCompletion::Status(SupervisorStatus::Cancelled);
            }
            if !tree.process_tree_supported() {
                break SupervisionCompletion::Status(SupervisorStatus::SupervisionFailed {
                    stage: SupervisorFailureStage::Cleanup,
                });
            }
            if started.elapsed() >= timeout {
                break SupervisionCompletion::Status(SupervisorStatus::TimedOut);
            }
            std::thread::sleep(MINIMUM_POLL_INTERVAL);
        };
        let cleanup = tree.finish(&mut child);
        match cleanup {
            CleanupStatus::Complete { root_success } => {
                let output_trust = if root_success {
                    LauncherOutputTrust::TrustedSupervisorTrailer
                } else {
                    LauncherOutputTrust::UntrustedTargetBytes
                };
                let launcher_status = emit_launcher_stdout(launcher_reader, output_trust);
                match status {
                    SupervisionCompletion::LauncherExited { success: true } => launcher_status
                        .ok()
                        .flatten()
                        .map(launcher_supervisor_status)
                        .unwrap_or(SupervisorStatus::SupervisionFailed {
                            stage: SupervisorFailureStage::Wait,
                        }),
                    SupervisionCompletion::LauncherExited { success: false } => {
                        SupervisorStatus::SupervisionFailed {
                            stage: SupervisorFailureStage::Wait,
                        }
                    }
                    SupervisionCompletion::Status(status) if launcher_status.is_ok() => status,
                    SupervisionCompletion::Status(_) => SupervisorStatus::SupervisionFailed {
                        stage: SupervisorFailureStage::Wait,
                    },
                }
            }
            CleanupStatus::ProcessTreeUnsupported { root_success }
            | CleanupStatus::Failed { root_success } => {
                let output_trust = if root_success {
                    LauncherOutputTrust::TrustedSupervisorTrailer
                } else {
                    LauncherOutputTrust::UntrustedTargetBytes
                };
                let _ = emit_launcher_stdout(launcher_reader, output_trust);
                SupervisorStatus::SupervisionFailed {
                    stage: SupervisorFailureStage::Cleanup,
                }
            }
        }
    }

    fn read_launcher_stdout(
        mut source: std::process::ChildStdout,
    ) -> std::io::Result<LauncherRead> {
        let mut target = std::io::stdout().lock();
        let mut tail = Vec::with_capacity(LAUNCH_STATUS_TAIL_BYTES);
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            tail.extend_from_slice(&buffer[..read]);
            if tail.len() > LAUNCH_STATUS_TAIL_BYTES {
                let flush_bytes = tail.len() - LAUNCH_STATUS_TAIL_BYTES;
                target.write_all(&tail[..flush_bytes])?;
                target.flush()?;
                tail.drain(..flush_bytes);
            }
        }
        let parsed = parse_launcher_status(&tail);
        Ok(LauncherRead { tail, parsed })
    }

    fn parse_launcher_status(tail: &[u8]) -> Option<(usize, LauncherStatus)> {
        if tail.last() != Some(&b'\n') {
            return None;
        }
        let marker = tail
            .windows(LAUNCH_STATUS_TRAILER.len())
            .rposition(|window| window == LAUNCH_STATUS_TRAILER)?;
        let encoded = &tail[marker + LAUNCH_STATUS_TRAILER.len()..tail.len() - 1];
        serde_json::from_slice(encoded)
            .ok()
            .map(|status| (marker, status))
    }

    #[derive(Clone, Copy)]
    enum LauncherOutputTrust {
        UntrustedTargetBytes,
        TrustedSupervisorTrailer,
    }

    fn emit_launcher_stdout(
        reader: JoinHandle<std::io::Result<LauncherRead>>,
        trust: LauncherOutputTrust,
    ) -> Result<Option<LauncherStatus>, ()> {
        let read = reader.join().map_err(|_| ())?.map_err(|_| ())?;
        let (output_bytes, status) = match trust {
            LauncherOutputTrust::UntrustedTargetBytes => (read.tail.as_slice(), None),
            LauncherOutputTrust::TrustedSupervisorTrailer => {
                let (marker, status) = read.parsed.ok_or(())?;
                (&read.tail[..marker], Some(status))
            }
        };
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(output_bytes).map_err(|_| ())?;
        stdout.flush().map_err(|_| ())?;
        Ok(status)
    }

    fn launcher_supervisor_status(status: LauncherStatus) -> SupervisorStatus {
        match status {
            LauncherStatus::Exited {
                code,
                stdout,
                stderr,
            } => SupervisorStatus::Exited {
                code,
                stdout,
                stderr,
            },
            LauncherStatus::SpawnFailed { reason } => SupervisorStatus::SpawnFailed { reason },
            LauncherStatus::SupervisionFailed => SupervisorStatus::SupervisionFailed {
                stage: SupervisorFailureStage::Wait,
            },
        }
    }

    fn preflight_process_tree() -> Result<TrackedProcess, ()> {
        rustix::process::set_child_subreaper(Some(rustix::process::Pid::INIT)).map_err(|_| ())?;
        process_children(std::process::id()).map_err(|_| ())?;
        pin_process(std::process::id())?.ok_or(())
    }

    fn cleanup_untracked_children(supervisor: u32) {
        let deadline = Instant::now() + REAP_DEADLINE;
        loop {
            let children = process_children(supervisor).unwrap_or_default();
            for raw_pid in children {
                kill_process_group(raw_pid);
                if let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32) {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                }
            }
            reap_available_children();
            if process_children(supervisor).is_ok_and(|children| children.is_empty()) {
                return;
            }
            if Instant::now() >= deadline {
                reap_all_children();
                return;
            }
            std::thread::sleep(MINIMUM_POLL_INTERVAL);
        }
    }

    fn spawn_failure(error: &std::io::Error) -> SupervisorSpawnFailure {
        match error.kind() {
            std::io::ErrorKind::NotFound => SupervisorSpawnFailure::NotFound,
            std::io::ErrorKind::PermissionDenied => SupervisorSpawnFailure::PermissionDenied,
            _ => SupervisorSpawnFailure::Other,
        }
    }

    fn cancellation_signal() -> Arc<AtomicBool> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let reader_cancelled = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            let mut byte = [0_u8; 1];
            let _ = std::io::stdin().read(&mut byte);
            reader_cancelled.store(true, Ordering::Release);
        });
        cancelled
    }

    fn read_control_byte() -> Result<(), ()> {
        let mut byte = [0_u8; 1];
        std::io::stdin().read_exact(&mut byte).map_err(|_| ())
    }

    struct ProcessTreeGuard {
        root: u32,
        supervisor: u32,
        descendants: Arc<Mutex<BTreeMap<u32, TrackedProcess>>>,
        stop: Arc<AtomicBool>,
        watcher: Option<JoinHandle<()>>,
        process_tree_supported: Arc<AtomicBool>,
        armed: bool,
    }

    enum CleanupStatus {
        Complete { root_success: bool },
        ProcessTreeUnsupported { root_success: bool },
        Failed { root_success: bool },
    }

    struct TrackedProcess {
        pidfd: rustix::fd::OwnedFd,
        start_time: u64,
    }

    impl ProcessTreeGuard {
        fn new(root: u32, supervisor: u32) -> Result<Self, ()> {
            let root_process = pin_process(root)?.ok_or(())?;
            let descendants = Arc::new(Mutex::new(BTreeMap::from([(root, root_process)])));
            let stop = Arc::new(AtomicBool::new(false));
            let watcher_descendants = Arc::clone(&descendants);
            let watcher_stop = Arc::clone(&stop);
            let process_tree_supported = Arc::new(AtomicBool::new(true));
            let watcher_process_tree_supported = Arc::clone(&process_tree_supported);
            let watcher = std::thread::spawn(move || {
                let mut interval = MINIMUM_POLL_INTERVAL;
                while !watcher_stop.load(Ordering::Acquire) {
                    let changed = match observe_descendants(
                        root,
                        supervisor,
                        &watcher_descendants,
                        Some(&watcher_stop),
                        None,
                    ) {
                        Ok(changed) => changed,
                        Err(()) => {
                            watcher_process_tree_supported.store(false, Ordering::Release);
                            return;
                        }
                    };
                    interval = if changed {
                        MINIMUM_POLL_INTERVAL
                    } else {
                        (interval * 2).min(MAXIMUM_POLL_INTERVAL)
                    };
                    std::thread::sleep(interval);
                }
            });
            Ok(Self {
                root,
                supervisor,
                descendants,
                stop,
                watcher: Some(watcher),
                process_tree_supported,
                armed: true,
            })
        }

        fn finish(&mut self, child: &mut std::process::Child) -> CleanupStatus {
            self.stop_watcher();
            let mut process_tree_supported = self.process_tree_supported();
            let mut root_success = None;
            let deadline = Instant::now() + REAP_DEADLINE;
            loop {
                if observe_descendants(
                    self.root,
                    self.supervisor,
                    &self.descendants,
                    None,
                    Some(deadline),
                )
                .is_err()
                {
                    process_tree_supported = false;
                }
                self.kill_tracked_descendants();
                if root_success.is_none() {
                    root_success = child
                        .try_wait()
                        .ok()
                        .flatten()
                        .map(|status| status.success());
                }
                reap_available_children_except(self.supervisor, self.root);
                let children_empty = match process_children(self.supervisor) {
                    Ok(children) => children.is_empty(),
                    Err(_) => {
                        process_tree_supported = false;
                        false
                    }
                };
                if children_empty {
                    break;
                }
                if Instant::now() >= deadline {
                    self.kill_root();
                    let _ = child.kill();
                    let root_success = root_success
                        .or_else(|| child.wait().ok().map(|status| status.success()))
                        .unwrap_or(false);
                    reap_all_children();
                    self.armed = false;
                    return if process_tree_supported {
                        CleanupStatus::Failed { root_success }
                    } else {
                        CleanupStatus::ProcessTreeUnsupported { root_success }
                    };
                }
                std::thread::sleep(MINIMUM_POLL_INTERVAL);
            }
            let root_success = root_success
                .or_else(|| child.wait().ok().map(|status| status.success()))
                .unwrap_or(false);
            reap_all_children();
            self.armed = false;
            if process_tree_supported {
                CleanupStatus::Complete { root_success }
            } else {
                CleanupStatus::ProcessTreeUnsupported { root_success }
            }
        }

        fn kill_all(&mut self) {
            self.stop_watcher();
            let deadline = Instant::now() + REAP_DEADLINE;
            let _ = observe_descendants(
                self.root,
                self.supervisor,
                &self.descendants,
                None,
                Some(deadline),
            );
            self.kill_tracked_descendants();
            self.kill_root();
        }

        fn kill_tracked_descendants(&self) {
            let descendants = self
                .descendants
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (raw_pid, process) in descendants.iter() {
                if *raw_pid == self.root {
                    continue;
                }
                let _ = rustix::process::pidfd_send_signal(
                    &process.pidfd,
                    rustix::process::Signal::KILL,
                );
            }
        }

        fn kill_root(&self) {
            let descendants = self
                .descendants
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(root) = descendants.get(&self.root) {
                let _ =
                    rustix::process::pidfd_send_signal(&root.pidfd, rustix::process::Signal::KILL);
            }
        }

        fn process_tree_supported(&self) -> bool {
            self.process_tree_supported.load(Ordering::Acquire)
        }

        fn live_descendant_beyond_root(&self) -> Result<bool, ()> {
            observe_descendants(self.root, self.supervisor, &self.descendants, None, None)?;
            let descendants = self
                .descendants
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            Ok(descendants.keys().any(|raw_pid| *raw_pid != self.root))
        }

        fn stop_watcher(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(watcher) = self.watcher.take()
                && watcher.join().is_err()
            {
                self.process_tree_supported.store(false, Ordering::Release);
            }
        }
    }

    impl Drop for ProcessTreeGuard {
        fn drop(&mut self) {
            if self.armed {
                self.kill_all();
                reap_all_children();
            }
        }
    }

    fn kill_process_group(raw_pid: u32) {
        if let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }

    fn observe_descendants(
        root: u32,
        supervisor: u32,
        descendants: &Arc<Mutex<BTreeMap<u32, TrackedProcess>>>,
        stop: Option<&AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<bool, ()> {
        if descendant_observation_should_stop(stop, deadline) {
            return Ok(false);
        }
        let mut tracked = descendants.lock().unwrap_or_else(PoisonError::into_inner);
        let mut changed = retire_exited_or_reused(&mut tracked, stop, deadline)?;
        let mut known = tracked.keys().copied().collect::<BTreeSet<_>>();
        loop {
            if descendant_observation_should_stop(stop, deadline) {
                return Ok(changed);
            }
            let before = known.len();
            let parents = known
                .iter()
                .copied()
                .chain(std::iter::once(supervisor))
                .collect::<Vec<_>>();
            for parent in parents {
                if descendant_observation_should_stop(stop, deadline) {
                    return Ok(changed);
                }
                let expected_start_time = tracked.get(&parent).map(|process| process.start_time);
                if expected_start_time.is_some()
                    && process_start_time(parent)? != expected_start_time
                {
                    tracked.remove(&parent);
                    known.remove(&parent);
                    changed = true;
                    continue;
                }
                let children = match process_children(parent) {
                    Ok(children) => children,
                    Err(ProcessChildrenError::Gone) if parent != supervisor => {
                        if tracked.remove(&parent).is_some() {
                            known.remove(&parent);
                            changed = true;
                        }
                        continue;
                    }
                    Err(ProcessChildrenError::Gone | ProcessChildrenError::Unsupported) => {
                        return Err(());
                    }
                };
                if expected_start_time.is_some()
                    && process_start_time(parent)? != expected_start_time
                {
                    tracked.remove(&parent);
                    known.remove(&parent);
                    changed = true;
                    continue;
                }
                for raw_pid in children {
                    if descendant_observation_should_stop(stop, deadline) {
                        return Ok(changed);
                    }
                    if raw_pid == root || known.contains(&raw_pid) {
                        continue;
                    }
                    if let Some(process) = pin_process(raw_pid)? {
                        tracked.insert(raw_pid, process);
                        known.insert(raw_pid);
                        changed = true;
                    }
                }
            }
            if known.len() == before {
                break;
            }
        }
        Ok(changed)
    }

    fn descendant_observation_should_stop(
        stop: Option<&AtomicBool>,
        deadline: Option<Instant>,
    ) -> bool {
        stop.is_some_and(|stop| stop.load(Ordering::Acquire))
            || deadline.is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn retire_exited_or_reused(
        tracked: &mut BTreeMap<u32, TrackedProcess>,
        stop: Option<&AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<bool, ()> {
        let mut retired = Vec::new();
        for (raw_pid, process) in tracked.iter() {
            if descendant_observation_should_stop(stop, deadline) {
                break;
            }
            let identity_changed = process_start_time(*raw_pid)? != Some(process.start_time);
            if identity_changed || pidfd_has_exited(&process.pidfd)? {
                retired.push(*raw_pid);
            }
        }
        let changed = !retired.is_empty();
        for raw_pid in retired {
            tracked.remove(&raw_pid);
        }
        Ok(changed)
    }

    fn pin_process(raw_pid: u32) -> Result<Option<TrackedProcess>, ()> {
        let Some(start_time) = process_start_time(raw_pid)? else {
            return Ok(None);
        };
        let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32) else {
            return Ok(None);
        };
        let pidfd = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
            Ok(pidfd) => pidfd,
            Err(rustix::io::Errno::SRCH) => return Ok(None),
            Err(_) => return Err(()),
        };
        if process_start_time(raw_pid)? != Some(start_time) {
            return Ok(None);
        }
        Ok(Some(TrackedProcess { pidfd, start_time }))
    }

    fn pidfd_has_exited(pidfd: &rustix::fd::OwnedFd) -> Result<bool, ()> {
        let mut descriptors = [rustix::event::PollFd::new(
            pidfd,
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(
            &mut descriptors,
            Some(&rustix::time::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .map_err(|_| ())?;
        Ok(!descriptors[0].revents().is_empty())
    }

    fn process_start_time(pid: u32) -> Result<Option<u64>, ()> {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if process_gone(&error) => return Ok(None),
            Err(_) => return Err(()),
        };
        let command_end = stat.rfind(')').ok_or(())?;
        let start_time = stat[command_end + 1..]
            .split_whitespace()
            .nth(19)
            .ok_or(())?
            .parse::<u64>()
            .map_err(|_| ())?;
        Ok(Some(start_time))
    }

    fn process_gone(error: &std::io::Error) -> bool {
        error.kind() == std::io::ErrorKind::NotFound
            || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
    }

    #[derive(Clone, Copy)]
    enum ProcessChildrenError {
        Gone,
        Unsupported,
    }

    fn process_children(pid: u32) -> Result<Vec<u32>, ProcessChildrenError> {
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).map_err(|error| {
            if process_gone(&error) {
                ProcessChildrenError::Gone
            } else {
                ProcessChildrenError::Unsupported
            }
        })?;
        let mut children = Vec::new();
        let mut observed_task = false;
        for entry in tasks {
            let entry = entry.map_err(|_| ProcessChildrenError::Unsupported)?;
            let task = entry
                .file_name()
                .to_string_lossy()
                .parse::<u32>()
                .map_err(|_| ProcessChildrenError::Unsupported)?;
            match std::fs::read_to_string(format!("/proc/{pid}/task/{task}/children")) {
                Ok(values) => {
                    observed_task = true;
                    let parsed = values
                        .split_whitespace()
                        .map(str::parse::<u32>)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| ProcessChildrenError::Unsupported)?;
                    children.extend(parsed);
                }
                Err(error) if process_gone(&error) => {}
                Err(_) => return Err(ProcessChildrenError::Unsupported),
            }
        }
        observed_task
            .then_some(children)
            .ok_or(ProcessChildrenError::Gone)
    }

    fn reap_available_children() {
        loop {
            match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return,
            }
        }
    }

    fn reap_available_children_except(supervisor: u32, retained: u32) {
        for raw_pid in process_children(supervisor).unwrap_or_default() {
            if raw_pid == retained {
                continue;
            }
            if let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32) {
                let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG);
            }
        }
    }

    fn reap_all_children() {
        let deadline = Instant::now() + REAP_DEADLINE;
        while Instant::now() < deadline {
            match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
                Ok(Some(_)) => continue,
                Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                Err(rustix::io::Errno::CHILD) => return,
                Err(_) => return,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const CHILD_FIXTURE_NAME: &str = "linux::tests::child_fixture";
        const CARGO_TEST_EXECUTABLE: &str = "/workspace/target/debug/deps/example-target";
        const CARGO_PASSING_TEST: &str = "example::passes";
        const CARGO_IGNORED_TEST: &str = "example::ignored";
        const CARGO_FORGED_TEST: &str = "captured::forged";
        const CARGO_IGNORED_REASON: &str = "platform unavailable";

        #[test]
        fn cargo_test_runner_frames_declared_outcomes_and_rejects_captured_forgery()
        -> Result<(), Box<dyn std::error::Error>> {
            let mut parser = CargoTestOutputParser::default();
            let mut output = Vec::new();
            parser.observe_line(b"running 2 tests", CARGO_TEST_EXECUTABLE, &mut output)?;
            parser.observe_line(
                format!("test {CARGO_PASSING_TEST} ... ok").as_bytes(),
                CARGO_TEST_EXECUTABLE,
                &mut output,
            )?;
            parser.observe_line(
                format!("test {CARGO_IGNORED_TEST} ... ignored, {CARGO_IGNORED_REASON}").as_bytes(),
                CARGO_TEST_EXECUTABLE,
                &mut output,
            )?;
            parser.observe_line(b"running 1 tests", CARGO_TEST_EXECUTABLE, &mut output)?;
            parser.observe_line(
                format!("test {CARGO_FORGED_TEST} ... ok").as_bytes(),
                CARGO_TEST_EXECUTABLE,
                &mut output,
            )?;
            let first = serde_json::to_string(&CargoTestEvent {
                reason: "signalbox-test-result",
                executable: CARGO_TEST_EXECUTABLE,
                name: CARGO_PASSING_TEST,
                outcome: CargoTestEventOutcome::Passed,
            })?;
            let second = serde_json::to_string(&CargoTestEvent {
                reason: "signalbox-test-result",
                executable: CARGO_TEST_EXECUTABLE,
                name: CARGO_IGNORED_TEST,
                outcome: CargoTestEventOutcome::Ignored,
            })?;
            let expected = format!("{first}\n{second}\n").into_bytes();

            assert_eq!(output, expected);
            Ok(())
        }

        #[test]
        fn cargo_test_runner_accepts_the_singular_libtest_count()
        -> Result<(), Box<dyn std::error::Error>> {
            let mut parser = CargoTestOutputParser::default();
            let mut output = Vec::new();
            parser.observe_line(b"running 1 test", CARGO_TEST_EXECUTABLE, &mut output)?;
            parser.observe_line(
                format!("test {CARGO_PASSING_TEST} ... ok").as_bytes(),
                CARGO_TEST_EXECUTABLE,
                &mut output,
            )?;
            let expected = serde_json::to_string(&CargoTestEvent {
                reason: "signalbox-test-result",
                executable: CARGO_TEST_EXECUTABLE,
                name: CARGO_PASSING_TEST,
                outcome: CargoTestEventOutcome::Passed,
            })? + "\n";

            assert_eq!(String::from_utf8(output)?, expected);
            Ok(())
        }

        #[test]
        fn cargo_test_runner_reports_an_overlong_in_progress_status_line()
        -> Result<(), Box<dyn std::error::Error>> {
            let mut parser = CargoTestOutputParser::default();
            let mut output = Vec::new();
            parser.observe_line(b"running 1 test", CARGO_TEST_EXECUTABLE, &mut output)?;
            parser.observe_truncated(&mut output)?;
            let expected = serde_json::to_string(&CargoTestSourceTruncated {
                reason: "signalbox-test-source-truncated",
            })? + "\n";

            assert_eq!(String::from_utf8(output)?, expected);
            Ok(())
        }

        #[test]
        fn esrch_is_process_absence_evidence() {
            let error = std::io::Error::from_raw_os_error(rustix::io::Errno::SRCH.raw_os_error());

            assert!(process_gone(&error));
        }

        #[test]
        fn exited_pidfd_is_retired_before_another_ancestry_pass()
        -> Result<(), Box<dyn std::error::Error>> {
            let mut child = Command::new(std::env::current_exe()?)
                .args(["--ignored", "--exact", CHILD_FIXTURE_NAME])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let raw_pid = child.id();
            let process = pin_process(raw_pid)
                .map_err(|()| std::io::Error::other("pin process"))?
                .ok_or_else(|| std::io::Error::other("child process disappeared"))?;
            let mut tracked = BTreeMap::from([(raw_pid, process)]);
            child.wait()?;

            let changed = retire_exited_or_reused(&mut tracked, None, None)
                .map_err(|()| std::io::Error::other("retire process"))?;

            assert!(changed);
            assert!(tracked.is_empty());
            Ok(())
        }

        #[test]
        fn reaped_root_is_not_reintroduced_as_an_ancestry_root()
        -> Result<(), Box<dyn std::error::Error>> {
            with_procfs_children_support(|| {
                let mut child = Command::new(std::env::current_exe()?)
                    .args(["--ignored", "--exact", CHILD_FIXTURE_NAME])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?;
                let raw_pid = child.id();
                let process = pin_process(raw_pid)
                    .map_err(|()| std::io::Error::other("pin process"))?
                    .ok_or_else(|| std::io::Error::other("child process disappeared"))?;
                let tracked = Arc::new(Mutex::new(BTreeMap::from([(raw_pid, process)])));
                child.wait()?;

                let changed =
                    observe_descendants(raw_pid, std::process::id(), &tracked, None, None)
                        .map_err(|()| std::io::Error::other("observe descendants"))?;
                let tracked = tracked.lock().unwrap_or_else(PoisonError::into_inner);

                assert!(changed);
                assert!(!tracked.contains_key(&raw_pid));
                Ok(())
            })
        }

        #[test]
        fn descendant_observation_honors_the_watcher_stop_signal() {
            let stop = AtomicBool::new(true);
            let tracked = Arc::new(Mutex::new(BTreeMap::new()));

            assert_eq!(
                observe_descendants(u32::MAX, u32::MAX, &tracked, Some(&stop), None),
                Ok(false)
            );
        }

        #[test]
        fn descendant_observation_checks_stop_before_retiring_tracked_processes()
        -> Result<(), Box<dyn std::error::Error>> {
            let stop = AtomicBool::new(true);
            let process = pin_process(std::process::id())
                .map_err(|()| std::io::Error::other("pin current process"))?
                .ok_or_else(|| std::io::Error::other("current process disappeared"))?;
            let tracked = Arc::new(Mutex::new(BTreeMap::from([(u32::MAX, process)])));

            let changed = observe_descendants(u32::MAX, u32::MAX, &tracked, Some(&stop), None)
                .map_err(|()| std::io::Error::other("observe descendants"))?;
            let tracked = tracked.lock().unwrap_or_else(PoisonError::into_inner);

            assert!(!changed);
            assert!(tracked.contains_key(&u32::MAX));
            Ok(())
        }

        #[test]
        fn descendant_observation_honors_the_cleanup_deadline() {
            let tracked = Arc::new(Mutex::new(BTreeMap::new()));

            assert_eq!(
                observe_descendants(u32::MAX, u32::MAX, &tracked, None, Some(Instant::now())),
                Ok(false)
            );
        }

        #[test]
        fn process_tree_preflight_pins_the_supervisor_before_dispatch()
        -> Result<(), Box<dyn std::error::Error>> {
            with_procfs_children_support(|| {
                let reservation = preflight_process_tree()
                    .map_err(|()| std::io::Error::other("preflight process tree"))?;

                let exited = pidfd_has_exited(&reservation.pidfd)
                    .map_err(|()| std::io::Error::other("poll preflight pidfd"))?;

                assert!(!exited);
                Ok(())
            })
        }

        fn with_procfs_children_support<Test>(test: Test) -> Result<(), Box<dyn std::error::Error>>
        where
            Test: FnOnce() -> Result<(), Box<dyn std::error::Error>>,
        {
            if process_children(std::process::id()).is_err() {
                return Ok(());
            }
            test()
        }

        #[test]
        #[ignore = "subprocess fixture for pidfd retirement"]
        fn child_fixture() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::entrypoint()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::FAILURE
}

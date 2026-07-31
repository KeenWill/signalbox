#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::OsString,
        io::{Read, Write},
        os::unix::process::{CommandExt, ExitStatusExt},
        process::{Command, ExitCode, Stdio},
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

    use supervisor_protocol::{SupervisorSpawnFailure, SupervisorStatus};

    const MINIMUM_POLL_INTERVAL: Duration = Duration::from_millis(2);
    const MAXIMUM_POLL_INTERVAL: Duration = Duration::from_millis(100);
    const REAP_DEADLINE: Duration = Duration::from_secs(1);
    const DISPATCH_MODE: &str = "--dispatch";
    const DISPATCH_MARKER: &[u8] = b"signalbox-exec:dispatched\n";
    const STATUS_TRAILER: &[u8] = b"\n\0signalbox-exec-supervisor-status:";

    pub(super) fn entrypoint() -> ExitCode {
        let mut arguments = std::env::args_os().skip(1);
        let Some(mode_or_timeout) = arguments.next() else {
            return ExitCode::FAILURE;
        };
        if mode_or_timeout == DISPATCH_MODE {
            return dispatch(arguments.collect());
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

    fn dispatch(mut arguments: Vec<OsString>) -> ExitCode {
        if arguments.is_empty() {
            return ExitCode::FAILURE;
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
            Err(error) => return dispatch_spawn_failure(&error),
        };
        let Some(mut target_stdout) = child.stdout.take() else {
            terminate_child(&mut child);
            return ExitCode::FAILURE;
        };
        let Some(mut target_stderr) = child.stderr.take() else {
            terminate_child(&mut child);
            return ExitCode::FAILURE;
        };
        let mut stderr = std::io::stderr().lock();
        if stderr
            .write_all(DISPATCH_MARKER)
            .and_then(|()| stderr.flush())
            .is_err()
        {
            terminate_child(&mut child);
            return ExitCode::FAILURE;
        }
        drop(stderr);
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
                return ExitCode::FAILURE;
            }
        };
        leader_exited.store(true, Ordering::Release);
        let stdout_copy_failed = !matches!(stdout_copy.join(), Ok(Ok(())));
        let stderr_copy_failed = !matches!(stderr_copy.join(), Ok(Ok(())));
        if stdout_copy_failed || stderr_copy_failed {
            return ExitCode::FAILURE;
        }
        if let Some(code) = status.code() {
            return ExitCode::from(code as u8);
        }
        if let Some(signal) = status.signal() {
            let _ = signal_hook::low_level::emulate_default_handler(signal);
        }
        ExitCode::FAILURE
    }

    fn copy_until_leader_exit<Source, Target>(
        source: &mut Source,
        target: &mut Target,
        leader_exited: &AtomicBool,
    ) -> std::io::Result<()>
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
                if remaining == 0 {
                    return Ok(());
                }
                post_exit_remaining = Some(remaining);
            }
            let read_limit = post_exit_remaining
                .map(|remaining| usize::try_from(remaining).unwrap_or(usize::MAX))
                .unwrap_or(buffer.len())
                .min(buffer.len());
            match source.read(&mut buffer[..read_limit]) {
                Ok(0) => return Ok(()),
                Ok(read) => {
                    target.write_all(&buffer[..read])?;
                    if let Some(remaining) = post_exit_remaining.as_mut() {
                        *remaining = remaining.saturating_sub(read as u64);
                        if *remaining == 0 {
                            return Ok(());
                        }
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

    fn dispatch_spawn_failure(error: &std::io::Error) -> ExitCode {
        match error.kind() {
            std::io::ErrorKind::NotFound => ExitCode::from(127),
            std::io::ErrorKind::PermissionDenied => ExitCode::from(126),
            _ => ExitCode::FAILURE,
        }
    }

    fn supervise(
        program: OsString,
        arguments: Vec<OsString>,
        timeout: Duration,
    ) -> SupervisorStatus {
        if rustix::process::set_child_subreaper(Some(rustix::process::Pid::INIT)).is_err() {
            return SupervisorStatus::SpawnFailed {
                reason: SupervisorSpawnFailure::ProcessTreeUnsupported,
            };
        }
        if process_children(std::process::id()).is_err() {
            return SupervisorStatus::SpawnFailed {
                reason: SupervisorSpawnFailure::ProcessTreeUnsupported,
            };
        }
        let started = Instant::now();
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return SupervisorStatus::SpawnFailed {
                    reason: spawn_failure(error),
                };
            }
        };
        let mut tree = match ProcessTreeGuard::new(child.id(), std::process::id()) {
            Ok(tree) => tree,
            Err(()) => {
                terminate_child(&mut child);
                return SupervisorStatus::SpawnFailed {
                    reason: SupervisorSpawnFailure::ProcessTreeUnsupported,
                };
            }
        };
        let cancelled = cancellation_signal();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    break SupervisorStatus::Exited {
                        code: status.code(),
                    };
                }
                Ok(None) => {}
                Err(_) => break SupervisorStatus::SupervisionFailed,
            }
            if cancelled.load(Ordering::Acquire) {
                break SupervisorStatus::Cancelled;
            }
            if !tree.process_tree_supported() {
                break SupervisorStatus::SpawnFailed {
                    reason: SupervisorSpawnFailure::ProcessTreeUnsupported,
                };
            }
            if started.elapsed() >= timeout {
                break SupervisorStatus::TimedOut;
            }
            std::thread::sleep(MINIMUM_POLL_INTERVAL);
        };
        match tree.finish(&mut child) {
            CleanupStatus::Complete => status,
            CleanupStatus::ProcessTreeUnsupported => SupervisorStatus::SpawnFailed {
                reason: SupervisorSpawnFailure::ProcessTreeUnsupported,
            },
            CleanupStatus::Failed => SupervisorStatus::SupervisionFailed,
        }
    }

    fn spawn_failure(error: std::io::Error) -> SupervisorSpawnFailure {
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
        Complete,
        ProcessTreeUnsupported,
        Failed,
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
                    let changed = match observe_descendants(root, supervisor, &watcher_descendants)
                    {
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
            let deadline = Instant::now() + REAP_DEADLINE;
            loop {
                if observe_descendants(self.root, self.supervisor, &self.descendants).is_err() {
                    process_tree_supported = false;
                }
                self.kill_tracked();
                let _ = child.kill();
                reap_available_children();
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
                    let _ = child.wait();
                    reap_all_children();
                    self.armed = false;
                    return if process_tree_supported {
                        CleanupStatus::Failed
                    } else {
                        CleanupStatus::ProcessTreeUnsupported
                    };
                }
                std::thread::sleep(MINIMUM_POLL_INTERVAL);
            }
            let _ = child.wait();
            reap_all_children();
            self.armed = false;
            if process_tree_supported {
                CleanupStatus::Complete
            } else {
                CleanupStatus::ProcessTreeUnsupported
            }
        }

        fn kill_all(&mut self) {
            self.stop_watcher();
            let _ = observe_descendants(self.root, self.supervisor, &self.descendants);
            self.kill_tracked();
        }

        fn kill_tracked(&self) {
            let descendants = self
                .descendants
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if descendants.contains_key(&self.root) {
                kill_process_group(self.root);
            }
            for process in descendants.values() {
                let _ = rustix::process::pidfd_send_signal(
                    &process.pidfd,
                    rustix::process::Signal::KILL,
                );
            }
        }

        fn process_tree_supported(&self) -> bool {
            self.process_tree_supported.load(Ordering::Acquire)
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
    ) -> Result<bool, ()> {
        let mut tracked = descendants.lock().unwrap_or_else(PoisonError::into_inner);
        let mut changed = retire_exited_or_reused(&mut tracked)?;
        let mut known = tracked.keys().copied().collect::<BTreeSet<_>>();
        loop {
            let before = known.len();
            let parents = known
                .iter()
                .copied()
                .chain(std::iter::once(supervisor))
                .collect::<Vec<_>>();
            for parent in parents {
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

    fn retire_exited_or_reused(tracked: &mut BTreeMap<u32, TrackedProcess>) -> Result<bool, ()> {
        let mut retired = Vec::new();
        for (raw_pid, process) in tracked.iter() {
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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

    #[derive(Clone, Copy)]
    enum ProcessChildrenError {
        Gone,
        Unsupported,
    }

    fn process_children(pid: u32) -> Result<Vec<u32>, ProcessChildrenError> {
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
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
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
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

            let changed = retire_exited_or_reused(&mut tracked)
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

                let changed = observe_descendants(raw_pid, std::process::id(), &tracked)
                    .map_err(|()| std::io::Error::other("observe descendants"))?;
                let tracked = tracked.lock().unwrap_or_else(PoisonError::into_inner);

                assert!(changed);
                assert!(!tracked.contains_key(&raw_pid));
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

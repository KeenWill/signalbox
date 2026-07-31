#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::{Read, Write},
    os::unix::process::CommandExt,
    process::{Command, ExitCode, Stdio},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[path = "../supervisor_protocol.rs"]
mod supervisor_protocol;

use supervisor_protocol::{SupervisorSpawnFailure, SupervisorStatus};

const MINIMUM_POLL_INTERVAL: Duration = Duration::from_millis(2);
const MAXIMUM_POLL_INTERVAL: Duration = Duration::from_millis(100);
const REAP_DEADLINE: Duration = Duration::from_secs(1);
const STATUS_TRAILER: &[u8] = b"\n\0signalbox-exec-supervisor-status:";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

fn run() -> Result<(), ()> {
    let mut arguments = std::env::args_os().skip(1);
    let timeout_milliseconds = parse_u64(arguments.next())?;
    let program = arguments.next().ok_or(())?;
    let target_arguments = arguments.collect::<Vec<_>>();
    let status = supervise(
        program,
        target_arguments,
        Duration::from_millis(timeout_milliseconds),
    );
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(STATUS_TRAILER).map_err(|_| ())?;
    serde_json::to_writer(&mut stdout, &status).map_err(|_| ())?;
    stdout.write_all(b"\n").map_err(|_| ())?;
    stdout.flush().map_err(|_| ())?;
    Ok(())
}

fn parse_u64(value: Option<OsString>) -> Result<u64, ()> {
    value
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse().ok())
        .ok_or(())
}

fn supervise(program: OsString, arguments: Vec<OsString>, timeout: Duration) -> SupervisorStatus {
    if rustix::process::set_child_subreaper(Some(rustix::process::Pid::INIT)).is_err() {
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
    let mut tree = ProcessTreeGuard::new(child.id(), std::process::id());
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
        if started.elapsed() >= timeout {
            break SupervisorStatus::TimedOut;
        }
        std::thread::sleep(MINIMUM_POLL_INTERVAL);
    };
    tree.finish(&mut child);
    status
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
    descendants: Arc<Mutex<BTreeMap<u32, rustix::fd::OwnedFd>>>,
    stop: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
    armed: bool,
}

impl ProcessTreeGuard {
    fn new(root: u32, supervisor: u32) -> Self {
        let descendants = Arc::new(Mutex::new(BTreeMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher_descendants = Arc::clone(&descendants);
        let watcher_stop = Arc::clone(&stop);
        let watcher = std::thread::spawn(move || {
            let mut interval = MINIMUM_POLL_INTERVAL;
            while !watcher_stop.load(Ordering::Acquire) {
                let changed = observe_descendants(root, supervisor, &watcher_descendants);
                interval = if changed {
                    MINIMUM_POLL_INTERVAL
                } else {
                    (interval * 2).min(MAXIMUM_POLL_INTERVAL)
                };
                std::thread::sleep(interval);
            }
        });
        Self {
            root,
            supervisor,
            descendants,
            stop,
            watcher: Some(watcher),
            armed: true,
        }
    }

    fn finish(&mut self, child: &mut std::process::Child) {
        self.kill_all();
        let _ = child.kill();
        let _ = child.wait();
        reap_all_children();
        self.armed = false;
    }

    fn kill_all(&mut self) {
        self.stop_watcher();
        observe_descendants(self.root, self.supervisor, &self.descendants);
        kill_process_group(self.root);
        let descendants = self
            .descendants
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for pidfd in descendants.values() {
            let _ = rustix::process::pidfd_send_signal(pidfd, rustix::process::Signal::KILL);
        }
    }

    fn stop_watcher(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
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
    descendants: &Arc<Mutex<BTreeMap<u32, rustix::fd::OwnedFd>>>,
) -> bool {
    let mut known = descendants
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    known.insert(root);
    loop {
        let before = known.len();
        let parents = known
            .iter()
            .copied()
            .chain(std::iter::once(supervisor))
            .collect::<Vec<_>>();
        for child in parents.into_iter().flat_map(process_children) {
            known.insert(child);
        }
        if known.len() == before {
            break;
        }
    }
    known.remove(&root);
    let mut changed = false;
    let mut tracked = descendants.lock().unwrap_or_else(PoisonError::into_inner);
    for raw_pid in known {
        if tracked.contains_key(&raw_pid) {
            continue;
        }
        let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32) else {
            continue;
        };
        if let Ok(pidfd) = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
            tracked.insert(raw_pid, pidfd);
            changed = true;
        }
    }
    changed
}

fn process_children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let task = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
            std::fs::read_to_string(format!("/proc/{pid}/task/{task}/children")).ok()
        })
        .flat_map(|children| {
            children
                .split_whitespace()
                .filter_map(|child| child.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
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

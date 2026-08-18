use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    io::{Seek as _, SeekFrom, Write as _},
    os::{fd::AsRawFd as _, unix::fs::PermissionsExt as _, unix::process::CommandExt as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use rustix::{
    fd::AsFd as _,
    process::{Resource, Rlimit, geteuid},
};
use signalbox_file_media_runtime::{
    CancellationSignal, FileMediaProcessCeilings, FileMediaProcessor, FileMediaProcessorFuture,
    FileMediaProviderDeclaration, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    MAX_WORKER_TASKS, ProcessorFailure, ProcessorIsolation, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReaderDeclaration, ReaderIdentity,
    VerifiedBlobSource,
};
use tokio::{
    io::AsyncReadExt as _,
    process::{Child, ChildStdin, ChildStdout, Command},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};

use crate::{
    broker::{BrokerError, RangeBroker, read_frame_with_limit, write_frame_with_limit},
    protocol::{
        DaemonFrame, Invocation, WireReadEnvelope, WireSource, WorkerFrame,
        declaration_fingerprint, encode_bytes,
    },
};

const WORKER_SANDBOX_PATH: &str = "/signalbox-file-media-worker";
const BWRAP_PROBE_ARGUMENT: &str = "--signalbox-file-media-isolation-probe";
const CANCELLATION_POLL: Duration = Duration::from_millis(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const WRITABLE_TMPFS_BUDGET_DIVISOR: u64 = 2;

/// One checked mapping from a provider declaration to its worker executable.
#[derive(Clone, Debug)]
pub struct WorkerBinding {
    program: PathBuf,
    declaration: FileMediaProviderDeclaration,
}

impl WorkerBinding {
    /// Binds one complete provider declaration to one absolute executable.
    pub fn try_new(
        program: impl Into<PathBuf>,
        declaration: FileMediaProviderDeclaration,
    ) -> Result<Self, SandboxedFileMediaProcessorConstructionError> {
        let program = program.into();
        validate_executable(&program, ConstructionTarget::Worker)?;
        Ok(Self {
            program,
            declaration,
        })
    }

    /// Borrows the provider declaration registered with the daemon.
    pub const fn declaration(&self) -> &FileMediaProviderDeclaration {
        &self.declaration
    }
}

/// Fresh-worker implementation of the registry's untrusted processor port.
#[derive(Clone, Debug)]
pub struct SandboxedFileMediaProcessor {
    bubblewrap: PathBuf,
    workers: Arc<BTreeMap<signalbox_file_media_runtime::FileReaderProviderName, PathBuf>>,
    worker_declarations: Arc<BTreeMap<PathBuf, Vec<FileMediaProviderDeclaration>>>,
    readers: Arc<BTreeMap<ReaderIdentity, ReaderDeclaration>>,
    ceilings: FileMediaProcessCeilings,
}

impl SandboxedFileMediaProcessor {
    /// Constructs a fail-closed Linux sandbox configuration.
    pub fn try_new(
        bubblewrap: impl Into<PathBuf>,
        bindings: Vec<WorkerBinding>,
        ceilings: FileMediaProcessCeilings,
    ) -> Result<Self, SandboxedFileMediaProcessorConstructionError> {
        if !cfg!(target_os = "linux") || bindings.is_empty() {
            return Err(SandboxedFileMediaProcessorConstructionError::Unsupported);
        }
        if !task_ceiling_is_enforceable(geteuid().as_raw()) {
            return Err(SandboxedFileMediaProcessorConstructionError::TaskCeiling);
        }
        if !FileMediaProcessCeilings::version_one().admits(ceilings) {
            return Err(SandboxedFileMediaProcessorConstructionError::Ceilings);
        }
        let bubblewrap = bubblewrap.into();
        validate_executable(&bubblewrap, ConstructionTarget::Bubblewrap)?;
        let mut workers = BTreeMap::new();
        let mut worker_declarations = BTreeMap::<PathBuf, Vec<FileMediaProviderDeclaration>>::new();
        let mut readers = BTreeMap::new();
        for binding in bindings {
            let provider = binding.declaration.provider().clone();
            if workers.insert(provider, binding.program.clone()).is_some() {
                return Err(SandboxedFileMediaProcessorConstructionError::DuplicateProvider);
            }
            for reader in binding.declaration.readers() {
                if readers
                    .insert(reader.identity().clone(), reader.clone())
                    .is_some()
                {
                    return Err(SandboxedFileMediaProcessorConstructionError::DuplicateReader);
                }
            }
            worker_declarations
                .entry(binding.program)
                .or_default()
                .push(binding.declaration);
        }
        Ok(Self {
            bubblewrap,
            workers: Arc::new(workers),
            worker_declarations: Arc::new(worker_declarations),
            readers: Arc::new(readers),
            ceilings,
        })
    }

    /// Proves that the exact configured profile can start every registered worker.
    pub async fn verify_isolation(&self) -> ProcessorIsolation {
        for (worker, declarations) in self.worker_declarations.iter() {
            if self.run_probe(worker, declarations).await.is_err() {
                return ProcessorIsolation::Unavailable;
            }
        }
        ProcessorIsolation::Available
    }

    /// Returns the effective lowerable-only process ceilings.
    pub const fn ceilings(&self) -> FileMediaProcessCeilings {
        self.ceilings
    }

    async fn run_probe(
        &self,
        worker: &Path,
        declarations: &[FileMediaProviderDeclaration],
    ) -> Result<(), ProcessorFailure> {
        let mut running = self.spawn(worker, true).await?;
        running.release_startup()?;
        let mut stdout = running
            .child
            .stdout
            .take()
            .ok_or(ProcessorFailure::Unavailable)?;
        let expected = declaration_fingerprint(declarations);
        let output_limit = u64::try_from(expected.len())
            .map_err(|_| ProcessorFailure::Unavailable)?
            .checked_add(1)
            .ok_or(ProcessorFailure::Unavailable)?;
        let deadline = Duration::from_secs(self.ceilings.wall_seconds());
        let waited = tokio::time::timeout(deadline, async {
            let mut observed = Vec::new();
            stdout
                .take(output_limit)
                .read_to_end(&mut observed)
                .await
                .map_err(|_| ProcessorFailure::Unavailable)?;
            let status = running
                .child
                .wait()
                .await
                .map_err(|_| ProcessorFailure::Unavailable)?;
            if status.success() && observed.as_slice() == expected.as_slice() {
                Ok(())
            } else {
                Err(ProcessorFailure::Unavailable)
            }
        })
        .await;
        let result = match waited {
            Ok(result) => result,
            Err(_) => Err(ProcessorFailure::TimedOut),
        };
        if result.is_err() {
            running.terminate().await;
        }
        result
    }

    fn reader(&self, identity: &ReaderIdentity) -> Result<&ReaderDeclaration, ProcessorFailure> {
        self.readers.get(identity).ok_or(ProcessorFailure::Protocol)
    }

    fn worker(&self, identity: &ReaderIdentity) -> Result<&Path, ProcessorFailure> {
        self.workers
            .get(identity.provider())
            .map(PathBuf::as_path)
            .ok_or(ProcessorFailure::Unavailable)
    }

    async fn invoke(
        &self,
        invocation: Invocation,
        expected: ExpectedOutput,
        source: &dyn VerifiedBlobSource,
        cancellation: &dyn CancellationSignal,
    ) -> Result<CompletedOutput, ProcessorFailure> {
        if cancellation.is_cancelled()
            || invocation.source().digest() != source.digest()
            || invocation
                .source()
                .byte_length()
                .map_err(|_| ProcessorFailure::Protocol)?
                != source.byte_length()
        {
            return Err(if cancellation.is_cancelled() {
                ProcessorFailure::Cancelled
            } else {
                ProcessorFailure::Protocol
            });
        }
        let worker = match &invocation {
            Invocation::Probe { reader, .. }
            | Invocation::Validate { reader, .. }
            | Invocation::Read { reader, .. } => {
                let identity = ReaderIdentity::try_from(reader.clone())
                    .map_err(|_| ProcessorFailure::Protocol)?;
                self.worker(&identity)?.to_owned()
            }
        };
        let mut running = self.spawn(&worker, false).await?;
        running.release_startup()?;
        let stdin = running
            .child
            .stdin
            .take()
            .ok_or(ProcessorFailure::Unavailable)?;
        let stdout = running
            .child
            .stdout
            .take()
            .ok_or(ProcessorFailure::Unavailable)?;
        let stderr = running
            .child
            .stderr
            .take()
            .ok_or(ProcessorFailure::Unavailable)?;
        let stderr_limit = self.ceilings.stderr_bytes();
        let mut stderr_task = tokio::spawn(read_and_discard_diagnostics(stderr, stderr_limit));
        let outcome = {
            let session = run_session(
                &mut running.child,
                stdin,
                stdout,
                invocation,
                expected,
                source,
                self.ceilings.frame_bytes(),
            );
            tokio::pin!(session);
            let deadline = Instant::now() + Duration::from_secs(self.ceilings.wall_seconds());
            let mut cancellation_poll = tokio::time::interval(CANCELLATION_POLL);
            cancellation_poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(deadline) => break Err(ProcessorFailure::TimedOut),
                    _ = cancellation_poll.tick() => {
                        if cancellation.is_cancelled() {
                            break Err(ProcessorFailure::Cancelled);
                        }
                    }
                    result = &mut session => {
                        if Instant::now() >= deadline {
                            break Err(ProcessorFailure::TimedOut);
                        }
                        break result;
                    }
                }
            }
        };
        if outcome.is_err() {
            running.terminate().await;
        }
        let diagnostics = finish_diagnostics(&mut stderr_task).await;
        let outcome = admit_completed(outcome, cancellation);
        match (outcome, diagnostics) {
            (Ok(output), Ok(())) => Ok(output),
            (Err(error), _) => Err(error),
            (Ok(_), Err(())) => Err(ProcessorFailure::Protocol),
        }
    }

    async fn spawn(&self, worker: &Path, probe: bool) -> Result<RunningWorker, ProcessorFailure> {
        let seccomp = process_creation_filter().map_err(|_| ProcessorFailure::Unavailable)?;
        let (block_read, block_write) =
            rustix::pipe::pipe().map_err(|_| ProcessorFailure::Unavailable)?;
        rustix::io::fcntl_setfd(block_write.as_fd(), rustix::io::FdFlags::CLOEXEC)
            .map_err(|_| ProcessorFailure::Unavailable)?;
        let profile = sandbox_arguments(
            worker,
            seccomp.as_raw_fd(),
            block_read.as_raw_fd(),
            self.ceilings.memory_bytes(),
            probe,
        );
        let mut command = Command::new(&self.bubblewrap);
        command
            .args(profile)
            .current_dir("/")
            .env_clear()
            .stdin(if probe { Stdio::null() } else { Stdio::piped() })
            .stdout(Stdio::piped())
            .stderr(if probe { Stdio::null() } else { Stdio::piped() })
            .kill_on_drop(true);
        let ceilings = self.ceilings;
        command.as_std_mut().process_group(0);
        unsafe {
            command
                .as_std_mut()
                .pre_exec(move || apply_process_limits(ceilings).map_err(std::io::Error::from));
        }
        let child = command.spawn().map_err(|_| ProcessorFailure::Unavailable)?;
        drop(block_read);
        let raw_pid = child.id().ok_or(ProcessorFailure::Unavailable)?;
        let pid =
            rustix::process::Pid::from_raw(raw_pid as i32).ok_or(ProcessorFailure::Unavailable)?;
        Ok(RunningWorker {
            child,
            process_group: pid,
            startup: Some(block_write),
            _seccomp: seccomp,
        })
    }
}

fn admit_completed(
    outcome: Result<CompletedOutput, ProcessorFailure>,
    cancellation: &dyn CancellationSignal,
) -> Result<CompletedOutput, ProcessorFailure> {
    if cancellation.is_cancelled() {
        Err(ProcessorFailure::Cancelled)
    } else {
        outcome
    }
}

impl FileMediaProcessor for SandboxedFileMediaProcessor {
    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            let declaration = self.reader(reader)?;
            let invocation = Invocation::Probe {
                reader: reader.into(),
                source: WireSource::from_source(source),
                envelope: WireReadEnvelope::for_probe(declaration.probe()),
            };
            match self
                .invoke(invocation, ExpectedOutput::Probe, source, cancellation)
                .await?
            {
                CompletedOutput::Probe(output) => Ok(output),
                CompletedOutput::Validation(_) | CompletedOutput::Read(_) => {
                    Err(ProcessorFailure::Protocol)
                }
            }
        })
    }

    fn validate<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            let declaration = self.reader(reader)?;
            require_file_use_source(&request.source, source)?;
            let invocation = Invocation::Validate {
                reader: reader.into(),
                source: WireSource::from_source(source),
                envelope: WireReadEnvelope::for_probe(declaration.probe()),
                request: (&request).into(),
            };
            match self
                .invoke(invocation, ExpectedOutput::Validation, source, cancellation)
                .await?
            {
                CompletedOutput::Validation(output) => Ok(output),
                CompletedOutput::Probe(_) | CompletedOutput::Read(_) => {
                    Err(ProcessorFailure::Protocol)
                }
            }
        })
    }

    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        Box::pin(async move {
            let declaration = self.reader(reader)?;
            require_file_use_source(&request.source, source)?;
            let view = declaration
                .views()
                .iter()
                .find(|view| view.name() == &request.view)
                .ok_or(ProcessorFailure::Protocol)?;
            let invocation = Invocation::Read {
                reader: reader.into(),
                source: WireSource::from_source(source),
                envelope: WireReadEnvelope::for_view(view),
                request: (&request).into(),
            };
            match self
                .invoke(invocation, ExpectedOutput::Read, source, cancellation)
                .await?
            {
                CompletedOutput::Read(output) => Ok(output),
                CompletedOutput::Probe(_) | CompletedOutput::Validation(_) => {
                    Err(ProcessorFailure::Protocol)
                }
            }
        })
    }
}

fn require_file_use_source(
    file_use: &signalbox_file_media_runtime::FileUse,
    source: &dyn VerifiedBlobSource,
) -> Result<(), ProcessorFailure> {
    if file_use.digest() == source.digest() && file_use.byte_length() == source.byte_length() {
        Ok(())
    } else {
        Err(ProcessorFailure::Protocol)
    }
}

async fn run_session(
    child: &mut Child,
    mut stdin: ChildStdin,
    mut stdout: ChildStdout,
    invocation: Invocation,
    expected: ExpectedOutput,
    source: &dyn VerifiedBlobSource,
    frame_bytes: usize,
) -> Result<CompletedOutput, ProcessorFailure> {
    let envelope = invocation.envelope();
    let source_length = invocation
        .source()
        .byte_length()
        .map_err(|_| ProcessorFailure::Protocol)?
        .get();
    write_frame_with_limit(
        &mut stdin,
        &DaemonFrame::Invocation {
            invocation: Box::new(invocation),
        },
        frame_bytes,
    )
    .await
    .map_err(|_| ProcessorFailure::Protocol)?;
    let maximum_range_bytes =
        u64::try_from(frame_bytes / 2).map_err(|_| ProcessorFailure::Protocol)?;
    let mut broker = RangeBroker::new(source_length, envelope, maximum_range_bytes);
    let completed = loop {
        let frame: WorkerFrame = read_frame_with_limit(&mut stdout, frame_bytes)
            .await
            .map_err(|error| match error {
                BrokerError::Eof => ProcessorFailure::Failed,
                BrokerError::Frame | BrokerError::Range => ProcessorFailure::Protocol,
            })?;
        match frame {
            WorkerFrame::ReadRange { offset, length } => {
                let length = broker
                    .admit(offset, length)
                    .map_err(|_| ProcessorFailure::Protocol)?;
                let bytes = source
                    .read_range(offset, length)
                    .await
                    .map_err(|_| ProcessorFailure::Failed)?;
                if bytes.len()
                    != usize::try_from(length.get()).map_err(|_| ProcessorFailure::Protocol)?
                {
                    return Err(ProcessorFailure::Protocol);
                }
                write_frame_with_limit(
                    &mut stdin,
                    &DaemonFrame::RangeBytes {
                        bytes_base64: encode_bytes(&bytes),
                    },
                    frame_bytes,
                )
                .await
                .map_err(|_| ProcessorFailure::Protocol)?;
            }
            WorkerFrame::ProbeResult { output } if expected == ExpectedOutput::Probe => {
                break CompletedOutput::Probe(output);
            }
            WorkerFrame::ValidationResult { output } if expected == ExpectedOutput::Validation => {
                break CompletedOutput::Validation(output);
            }
            WorkerFrame::ReadResult { output } if expected == ExpectedOutput::Read => {
                break CompletedOutput::Read(output);
            }
            WorkerFrame::ProbeResult { .. }
            | WorkerFrame::ValidationResult { .. }
            | WorkerFrame::ReadResult { .. } => return Err(ProcessorFailure::Protocol),
        }
    };
    drop(stdin);
    let mut trailing = [0_u8; 1];
    if stdout
        .read(&mut trailing)
        .await
        .map_err(|_| ProcessorFailure::Protocol)?
        != 0
    {
        return Err(ProcessorFailure::Protocol);
    }
    let status = child.wait().await.map_err(|_| ProcessorFailure::Failed)?;
    if status.success() {
        Ok(completed)
    } else {
        Err(ProcessorFailure::Failed)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedOutput {
    Probe,
    Validation,
    Read,
}

enum CompletedOutput {
    Probe(ProcessorProbeOutput),
    Validation(ProcessorValidationOutput),
    Read(ProcessorReadOutput),
}

struct RunningWorker {
    child: Child,
    process_group: rustix::process::Pid,
    startup: Option<rustix::fd::OwnedFd>,
    _seccomp: fs::File,
}

impl RunningWorker {
    fn release_startup(&mut self) -> Result<(), ProcessorFailure> {
        let startup = self.startup.take().ok_or(ProcessorFailure::Unavailable)?;
        rustix::io::write(&startup, &[1]).map_err(|_| ProcessorFailure::Unavailable)?;
        Ok(())
    }

    async fn terminate(&mut self) {
        let descendants = process_descendants(self.process_group);
        for descendant in descendants.iter().rev() {
            let _ = rustix::process::kill_process(*descendant, rustix::process::Signal::KILL);
        }
        let _ =
            rustix::process::kill_process_group(self.process_group, rustix::process::Signal::KILL);
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, self.child.wait()).await;
    }
}

fn process_descendants(root: rustix::process::Pid) -> Vec<rustix::process::Pid> {
    let mut pending = vec![root];
    let mut descendants = Vec::new();
    while let Some(parent) = pending.pop() {
        let raw_parent = parent.as_raw_nonzero().get();
        let path = format!("/proc/{raw_parent}/task/{raw_parent}/children");
        let Ok(children) = fs::read_to_string(path) else {
            continue;
        };
        for child in children.split_whitespace() {
            let Some(pid) = child
                .parse::<i32>()
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            else {
                continue;
            };
            if !descendants.contains(&pid) {
                descendants.push(pid);
                pending.push(pid);
            }
        }
    }
    descendants
}

async fn read_and_discard_diagnostics(
    mut stderr: tokio::process::ChildStderr,
    retained_limit: usize,
) -> Result<Vec<u8>, std::io::Error> {
    let mut retained = Vec::with_capacity(retained_limit);
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stderr.read(&mut buffer).await?;
        if read == 0 {
            return Ok(retained);
        }
        let available = retained_limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(available)]);
    }
}

async fn finish_diagnostics(
    task: &mut JoinHandle<Result<Vec<u8>, std::io::Error>>,
) -> Result<(), ()> {
    match tokio::time::timeout(CLEANUP_TIMEOUT, &mut *task).await {
        Ok(Ok(Ok(_))) => Ok(()),
        Ok(Ok(Err(_))) | Ok(Err(_)) => Err(()),
        Err(_) => {
            task.abort();
            Err(())
        }
    }
}

fn apply_process_limits(ceilings: FileMediaProcessCeilings) -> Result<(), rustix::io::Errno> {
    let memory = worker_memory_budget(ceilings.memory_bytes());
    set_limit(Resource::As, memory.address_space_bytes)?;
    set_limit(Resource::Cpu, ceilings.cpu_seconds())?;
    set_limit(Resource::Core, 0)?;
    set_limit(Resource::Nproc, MAX_WORKER_TASKS)?;
    set_limit(Resource::Nofile, ceilings.file_descriptors())
}

fn set_limit(resource: Resource, value: u64) -> Result<(), rustix::io::Errno> {
    rustix::process::setrlimit(
        resource,
        Rlimit {
            current: Some(value),
            maximum: Some(value),
        },
    )
}

fn sandbox_arguments(
    worker: &Path,
    seccomp_fd: i32,
    block_fd: i32,
    memory_bytes: u64,
    probe: bool,
) -> Vec<std::ffi::OsString> {
    let memory = worker_memory_budget(memory_bytes);
    let mut arguments = [
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--assert-userns-disabled",
        "--cap-drop",
        "ALL",
        "--clearenv",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--ro-bind-try",
        "/lib",
        "/lib",
        "--ro-bind-try",
        "/lib64",
        "/lib64",
        "--ro-bind-try",
        "/usr/lib",
        "/usr/lib",
        "--ro-bind-try",
        "/nix/store",
        "/nix/store",
    ]
    .into_iter()
    .map(std::ffi::OsString::from)
    .collect::<Vec<_>>();
    arguments.extend([
        std::ffi::OsString::from("--size"),
        std::ffi::OsString::from(memory.first_tmpfs_bytes.to_string()),
        std::ffi::OsString::from("--tmpfs"),
        std::ffi::OsString::from("/tmp"),
        std::ffi::OsString::from("--size"),
        std::ffi::OsString::from(memory.second_tmpfs_bytes.to_string()),
        std::ffi::OsString::from("--tmpfs"),
        std::ffi::OsString::from("/run"),
        std::ffi::OsString::from("--ro-bind"),
        worker.as_os_str().to_owned(),
        std::ffi::OsString::from(WORKER_SANDBOX_PATH),
        std::ffi::OsString::from("--seccomp"),
        std::ffi::OsString::from(seccomp_fd.to_string()),
        std::ffi::OsString::from("--block-fd"),
        std::ffi::OsString::from(block_fd.to_string()),
        std::ffi::OsString::from("--chdir"),
        std::ffi::OsString::from("/tmp"),
        std::ffi::OsString::from("--setenv"),
        std::ffi::OsString::from("LANG"),
        std::ffi::OsString::from("C.UTF-8"),
        std::ffi::OsString::from("--setenv"),
        std::ffi::OsString::from("LC_ALL"),
        std::ffi::OsString::from("C.UTF-8"),
        std::ffi::OsString::from("--"),
        std::ffi::OsString::from(WORKER_SANDBOX_PATH),
    ]);
    if probe {
        arguments.push(std::ffi::OsString::from(BWRAP_PROBE_ARGUMENT));
    }
    arguments
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkerMemoryBudget {
    address_space_bytes: u64,
    first_tmpfs_bytes: u64,
    second_tmpfs_bytes: u64,
}

const fn worker_memory_budget(memory_bytes: u64) -> WorkerMemoryBudget {
    let tmpfs_bytes = memory_bytes / WRITABLE_TMPFS_BUDGET_DIVISOR;
    let first_tmpfs_bytes = tmpfs_bytes / 2;
    let second_tmpfs_bytes = tmpfs_bytes - first_tmpfs_bytes;
    WorkerMemoryBudget {
        address_space_bytes: memory_bytes - tmpfs_bytes,
        first_tmpfs_bytes,
        second_tmpfs_bytes,
    }
}

const fn task_ceiling_is_enforceable(effective_uid: u32) -> bool {
    effective_uid != 0
}

fn process_creation_filter() -> Result<fs::File, std::io::Error> {
    let mut file = fs::File::from(
        rustix::fs::memfd_create(
            "signalbox-file-media-seccomp",
            rustix::fs::MemfdFlags::CLOEXEC,
        )
        .map_err(std::io::Error::from)?,
    );
    rustix::io::fcntl_setfd(file.as_fd(), rustix::io::FdFlags::empty())
        .map_err(std::io::Error::from)?;
    for instruction in seccomp_instructions()? {
        file.write_all(&instruction.code.to_ne_bytes())?;
        file.write_all(&[instruction.jump_true, instruction.jump_false])?;
        file.write_all(&instruction.value.to_ne_bytes())?;
    }
    file.flush()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

#[derive(Clone, Copy)]
struct FilterInstruction {
    code: u16,
    jump_true: u8,
    jump_false: u8,
    value: u32,
}

fn seccomp_instructions() -> Result<Vec<FilterInstruction>, std::io::Error> {
    const LOAD_WORD_ABSOLUTE: u16 = 0x20;
    const JUMP_EQUAL: u16 = 0x15;
    const JUMP_SET: u16 = 0x45;
    const JUMP_ALWAYS: u16 = 0x05;
    const RETURN: u16 = 0x06;
    const SECCOMP_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_ERRNO_EPERM: u32 = 0x0005_0001;
    const SECCOMP_ERRNO_ENOSYS: u32 = 0x0005_0026;
    const CLONE_THREAD: u32 = 0x0001_0000;
    #[cfg(target_arch = "x86_64")]
    const X32_SYSCALL_BIT: Option<u32> = Some(0x4000_0000);
    #[cfg(not(target_arch = "x86_64"))]
    const X32_SYSCALL_BIT: Option<u32> = None;
    #[cfg(target_arch = "x86_64")]
    let (audit_arch, clone, clone3, fork, vfork) =
        (0xc000_003e, 56_u32, 435_u32, Some(57_u32), Some(58_u32));
    #[cfg(target_arch = "aarch64")]
    let (audit_arch, clone, clone3, fork, vfork) = (0xc000_00b7, 220_u32, 435_u32, None, None);
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unsupported seccomp architecture",
    ));
    let mut syscall_denials = Vec::new();
    if let Some(fork) = fork {
        syscall_denials.push(fork);
    }
    if let Some(vfork) = vfork {
        syscall_denials.push(vfork);
    }
    let x32_check_count = usize::from(X32_SYSCALL_BIT.is_some());
    let clone3_check_index = 4 + x32_check_count + syscall_denials.len();
    let clone_check_index = clone3_check_index + 1;
    let jump_allow_index = clone_check_index + 1;
    let load_clone_flags_index = jump_allow_index + 1;
    let clone_flags_check_index = load_clone_flags_index + 1;
    let deny_index = clone_flags_check_index + 1;
    let clone3_deny_index = deny_index + 1;
    let final_allow_index = clone3_deny_index + 1;
    let mut program = vec![
        instruction(LOAD_WORD_ABSOLUTE, 0, 0, 4),
        instruction(JUMP_EQUAL, 1, 0, audit_arch),
        instruction(RETURN, 0, 0, SECCOMP_KILL_PROCESS),
        instruction(LOAD_WORD_ABSOLUTE, 0, 0, 0),
    ];
    if let Some(x32_syscall_bit) = X32_SYSCALL_BIT {
        program.push(instruction(
            JUMP_SET,
            jump_distance(program.len(), deny_index)?,
            0,
            x32_syscall_bit,
        ));
    }
    for syscall in syscall_denials {
        program.push(instruction(
            JUMP_EQUAL,
            jump_distance(program.len(), deny_index)?,
            0,
            syscall,
        ));
    }
    program.push(instruction(
        JUMP_EQUAL,
        jump_distance(clone3_check_index, clone3_deny_index)?,
        0,
        clone3,
    ));
    program.push(instruction(JUMP_EQUAL, 1, 0, clone));
    program.push(instruction(
        JUMP_ALWAYS,
        0,
        0,
        u32::try_from(final_allow_index - jump_allow_index - 1).map_err(|_| filter_error())?,
    ));
    program.push(instruction(LOAD_WORD_ABSOLUTE, 0, 0, 16));
    program.push(instruction(
        JUMP_SET,
        jump_distance(clone_flags_check_index, final_allow_index)?,
        0,
        CLONE_THREAD,
    ));
    program.push(instruction(RETURN, 0, 0, SECCOMP_ERRNO_EPERM));
    program.push(instruction(RETURN, 0, 0, SECCOMP_ERRNO_ENOSYS));
    program.push(instruction(RETURN, 0, 0, SECCOMP_ALLOW));
    Ok(program)
}

const fn instruction(code: u16, jump_true: u8, jump_false: u8, value: u32) -> FilterInstruction {
    FilterInstruction {
        code,
        jump_true,
        jump_false,
        value,
    }
}

fn jump_distance(from: usize, to: usize) -> Result<u8, std::io::Error> {
    to.checked_sub(from + 1)
        .and_then(|distance| u8::try_from(distance).ok())
        .ok_or_else(filter_error)
}

fn filter_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid seccomp program")
}

fn validate_executable(
    path: &Path,
    target: ConstructionTarget,
) -> Result<(), SandboxedFileMediaProcessorConstructionError> {
    let valid = path.is_absolute()
        && fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if valid {
        Ok(())
    } else {
        Err(match target {
            ConstructionTarget::Bubblewrap => {
                SandboxedFileMediaProcessorConstructionError::Bubblewrap
            }
            ConstructionTarget::Worker => SandboxedFileMediaProcessorConstructionError::Worker,
        })
    }
}

#[derive(Clone, Copy)]
enum ConstructionTarget {
    Bubblewrap,
    Worker,
}

/// Checked sandbox configuration could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxedFileMediaProcessorConstructionError {
    /// This platform cannot provide the accepted sandbox.
    Unsupported,
    /// Bubblewrap was not an absolute executable file.
    Bubblewrap,
    /// A worker was not an absolute executable file.
    Worker,
    /// A process ceiling was zero or exceeded its compiled maximum.
    Ceilings,
    /// The current identity is exempt from the configured task ceiling.
    TaskCeiling,
    /// Two worker bindings claimed the same provider.
    DuplicateProvider,
    /// Two worker bindings claimed the same reader identity.
    DuplicateReader,
}

impl fmt::Display for SandboxedFileMediaProcessorConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "file-media sandbox is unsupported",
            Self::Bubblewrap => "file-media bubblewrap executable is invalid",
            Self::Worker => "file-media worker executable is invalid",
            Self::Ceilings => "file-media process ceilings are invalid",
            Self::TaskCeiling => "file-media task ceiling is unenforceable for this identity",
            Self::DuplicateProvider => "file-media worker provider is duplicated",
            Self::DuplicateReader => "file-media worker reader is duplicated",
        })
    }
}

impl Error for SandboxedFileMediaProcessorConstructionError {}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, path::Path};

    use signalbox_file_media_runtime::{CancellationSignal, ProcessorFailure};

    use super::{
        CompletedOutput, admit_completed, sandbox_arguments, seccomp_instructions,
        task_ceiling_is_enforceable, worker_memory_budget,
    };

    struct Cancelled;

    impl CancellationSignal for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn sandbox_profile_clears_authority_before_the_exact_worker() {
        let arguments =
            sandbox_arguments(Path::new("/fixture/worker"), 8, 9, 512 * 1024 * 1024, false);
        let expected_prefix = [
            "--die-with-parent",
            "--new-session",
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--assert-userns-disabled",
            "--cap-drop",
            "ALL",
            "--clearenv",
        ];
        assert_eq!(
            &arguments[..expected_prefix.len()],
            expected_prefix.map(std::ffi::OsString::from)
        );
        assert!(arguments.windows(3).any(|window| {
            window
                == [
                    OsStr::new("--ro-bind"),
                    OsStr::new("/fixture/worker"),
                    OsStr::new("/signalbox-file-media-worker"),
                ]
        }));
        assert_eq!(
            &arguments[arguments.len() - 2..],
            [OsStr::new("--"), OsStr::new("/signalbox-file-media-worker")]
        );
        assert!(arguments.windows(4).any(|window| {
            window
                == [
                    OsStr::new("--size"),
                    OsStr::new("134217728"),
                    OsStr::new("--tmpfs"),
                    OsStr::new("/tmp"),
                ]
        }));
        assert!(arguments.windows(4).any(|window| {
            window
                == [
                    OsStr::new("--size"),
                    OsStr::new("134217728"),
                    OsStr::new("--tmpfs"),
                    OsStr::new("/run"),
                ]
        }));
    }

    #[test]
    fn completed_output_is_not_admitted_after_cancellation() {
        let outcome = admit_completed(
            Ok(CompletedOutput::Probe(
                signalbox_file_media_runtime::ProcessorProbeOutput::NoMatch,
            )),
            &Cancelled,
        );
        assert!(matches!(outcome, Err(ProcessorFailure::Cancelled)));
    }

    #[test]
    fn memory_budget_combines_address_space_and_writable_tmpfs() {
        let budget = worker_memory_budget(512 * 1024 * 1024);
        assert_eq!(budget.address_space_bytes, 256 * 1024 * 1024);
        assert_eq!(budget.first_tmpfs_bytes, 128 * 1024 * 1024);
        assert_eq!(budget.second_tmpfs_bytes, 128 * 1024 * 1024);
        assert_eq!(
            budget.address_space_bytes + budget.first_tmpfs_bytes + budget.second_tmpfs_bytes,
            512 * 1024 * 1024
        );
    }

    #[test]
    fn root_identity_cannot_claim_an_enforced_task_ceiling() {
        assert!(!task_ceiling_is_enforceable(0));
        assert!(task_ceiling_is_enforceable(1_000));
    }

    #[test]
    fn descendant_filter_has_a_finite_arch_checked_program() {
        let program = seccomp_instructions().expect("the test architecture is supported");
        assert!(program.len() >= 10);
        assert_eq!(program[0].value, 4);
        assert_eq!(program[2].value, 0x8000_0000);
        assert_eq!(program.last().map(|entry| entry.value), Some(0x7fff_0000));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn descendant_filter_rejects_the_x32_syscall_abi() {
        let program = seccomp_instructions().expect("the test architecture is supported");
        let x32_check = program
            .iter()
            .find(|entry| entry.value == 0x4000_0000)
            .expect("the x32 ABI bit is checked");
        assert_eq!(x32_check.code, 0x45);
        assert_ne!(x32_check.jump_true, 0);
    }
}

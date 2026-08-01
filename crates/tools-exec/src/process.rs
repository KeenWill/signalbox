#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsString,
    fmt,
    future::Future,
    path::{Component, Path, PathBuf},
    time::Duration,
};
#[cfg(target_os = "linux")]
use std::{
    process::Stdio,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Instant,
};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};
#[cfg(target_os = "linux")]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[cfg(target_os = "linux")]
use crate::supervisor_protocol::{
    SupervisorFailureStage, SupervisorSpawnFailure, SupervisorStatus,
};

pub const SANDBOXED_EXEC_NAME: &str = "sandboxed_exec";
pub const UNSANDBOXED_EXEC_NAME: &str = "unsandboxed_exec";
const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_PROGRAM_CHARACTERS: usize = 4096;
const MAX_PROGRAM_BYTES: usize = MAX_PROGRAM_CHARACTERS * 4;
const MAX_ARGUMENTS: usize = 16;
const MAX_ARGUMENT_CHARACTERS: usize = 1024;
const MAX_ARGUMENT_BYTES: usize = MAX_ARGUMENT_CHARACTERS * 4;
const MAX_TOTAL_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_WORKING_DIRECTORY_CHARACTERS: usize = 4096;
const MAX_WORKING_DIRECTORY_BYTES: usize = MAX_WORKING_DIRECTORY_CHARACTERS * 4;
pub(crate) const EXEC_CAPTURE_BYTES: usize = 64 * 1024;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded direct-command arguments";
pub(crate) const BWRAP_PROGRAM: &str = "/usr/bin/bwrap";
const SANDBOX_WORKSPACE: &str = "/workspace";
const SANDBOX_DISPATCH_PROGRAM: &str = "/signalbox-exec-dispatch";
const SANDBOX_FALLBACK_PATH: &str = "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub(crate) const SANDBOX_DISPATCH_MARKER: &[u8] = b"signalbox-exec:dispatched\n";
#[cfg(target_os = "linux")]
const SUPERVISOR_STATUS_TRAILER: &[u8] = b"\n\0signalbox-exec-supervisor-status:";
#[cfg(target_os = "linux")]
const SUPERVISOR_STATUS_TAIL_BYTES: usize = 1024;
#[cfg(target_os = "linux")]
const SUPERVISOR_OUTER_MODE: &str = "--outer";
#[cfg(target_os = "linux")]
const OUTER_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(2);
#[cfg(target_os = "linux")]
const OUTER_PROCESS_CLEANUP_DEADLINE: Duration = Duration::from_secs(1);

fn default_timeout_seconds() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

fn default_working_directory() -> String {
    String::from(".")
}

/// Typed direct-command arguments shared by both execution tools.
#[derive(Clone, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecArguments {
    /// Executable name or path, passed directly without shell interpretation.
    #[schemars(length(min = 1, max = MAX_PROGRAM_CHARACTERS))]
    pub program: String,
    /// Direct executable arguments, each passed as one exact argv element.
    #[serde(default)]
    #[schemars(
        length(max = MAX_ARGUMENTS),
        inner(length(max = MAX_ARGUMENT_CHARACTERS))
    )]
    pub arguments: Vec<String>,
    /// Workspace-relative directory in which the process starts.
    #[serde(default = "default_working_directory")]
    #[schemars(length(min = 1, max = MAX_WORKING_DIRECTORY_CHARACTERS))]
    pub working_directory: String,
    /// Whole-process timeout in seconds, from 1 through 300.
    #[serde(default = "default_timeout_seconds")]
    #[schemars(range(min = 1, max = MAX_TIMEOUT_SECONDS))]
    pub timeout_seconds: u64,
}

struct SandboxedExecContract;

impl ToolContract for SandboxedExecContract {
    type Arguments = ExecArguments;
    const NAME: &'static str = SANDBOXED_EXEC_NAME;
    const DESCRIPTION: &'static str =
        "Runs one bounded direct command in a bwrap-confined injected workspace.";
}

struct UnsandboxedExecContract;

impl ToolContract for UnsandboxedExecContract {
    type Arguments = ExecArguments;
    const NAME: &'static str = UNSANDBOXED_EXEC_NAME;
    const DESCRIPTION: &'static str =
        "Runs one bounded direct command outside the filesystem sandbox after explicit approval.";
}

/// Why static tool construction or workspace-root admission failed.
#[derive(Debug)]
pub enum ExecToolConstructionError {
    /// A static tool name was rejected.
    Name,
    /// A static argument schema was rejected.
    Schema,
    /// A static sanitized detail was rejected.
    ErrorDetail,
    /// The one-entry catalog unexpectedly reported a duplicate.
    Duplicate,
    /// The injected workspace root was not an absolute canonical directory.
    WorkspaceRoot {
        /// Supplied root associated with the failure.
        path: PathBuf,
        /// Underlying filesystem failure, when one occurred.
        source: Option<std::io::Error>,
    },
    /// The injected supervisor program was not an absolute canonical file.
    SupervisorProgram {
        /// Supplied program associated with the failure.
        path: PathBuf,
        /// Underlying filesystem failure, when one occurred.
        source: Option<std::io::Error>,
    },
}

impl fmt::Display for ExecToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name => formatter.write_str("exec static name is invalid"),
            Self::Schema => formatter.write_str("exec static schema is invalid"),
            Self::ErrorDetail => formatter.write_str("exec static error detail is invalid"),
            Self::Duplicate => formatter.write_str("exec catalog is duplicated"),
            Self::WorkspaceRoot { path, .. } => {
                write!(
                    formatter,
                    "exec workspace root `{}` is invalid",
                    path.display()
                )
            }
            Self::SupervisorProgram { path, .. } => {
                write!(
                    formatter,
                    "exec supervisor program `{}` is invalid",
                    path.display()
                )
            }
        }
    }
}

impl Error for ExecToolConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WorkspaceRoot {
                source: Some(source),
                ..
            }
            | Self::SupervisorProgram {
                source: Some(source),
                ..
            } => Some(source),
            Self::Name
            | Self::Schema
            | Self::ErrorDetail
            | Self::Duplicate
            | Self::WorkspaceRoot { source: None, .. }
            | Self::SupervisorProgram { source: None, .. } => None,
        }
    }
}

/// One sandboxed catalog entry and its matching executor.
#[derive(Clone, Debug)]
pub struct SandboxedExecTool<Runner> {
    catalog: CompiledToolCatalog,
    executor: ExecExecutor<SandboxedCommandRunner<Runner>>,
}

impl<Runner: ProcessRunner> SandboxedExecTool<Runner> {
    /// Compiles the sandboxed tool around an injected runner and workspace.
    pub fn try_new(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        let command_runner = SandboxedCommandRunner::try_new(runner, workspace_root)?;
        let (catalog, executor) =
            build_tool::<SandboxedExecContract, _>(command_runner, ToolPermissionDefault::Auto)?;
        Ok(Self { catalog, executor })
    }

    /// Returns separate catalog and executor composition roles.
    pub fn into_parts(
        self,
    ) -> (
        CompiledToolCatalog,
        ExecExecutor<SandboxedCommandRunner<Runner>>,
    ) {
        (self.catalog, self.executor)
    }
}

impl SandboxedExecTool<TokioProcessRunner> {
    /// Builds the production sandboxed tool.
    pub fn try_new_production(
        workspace_root: impl AsRef<Path>,
        supervisor_program: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        Self::try_new(
            TokioProcessRunner::try_new(supervisor_program)?,
            workspace_root,
        )
    }
}

/// One unsandboxed catalog entry and its matching executor.
#[derive(Clone, Debug)]
pub struct UnsandboxedExecTool<Runner> {
    catalog: CompiledToolCatalog,
    executor: ExecExecutor<UnsandboxedCommandRunner<Runner>>,
}

impl<Runner: ProcessRunner> UnsandboxedExecTool<Runner> {
    /// Compiles the always-confirm tool around an injected runner and workspace.
    pub fn try_new(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        let command_runner = UnsandboxedCommandRunner::try_new(runner, workspace_root)?;
        let (catalog, executor) = build_tool::<UnsandboxedExecContract, _>(
            command_runner,
            ToolPermissionDefault::AlwaysConfirm,
        )?;
        Ok(Self { catalog, executor })
    }

    /// Returns separate catalog and executor composition roles.
    pub fn into_parts(
        self,
    ) -> (
        CompiledToolCatalog,
        ExecExecutor<UnsandboxedCommandRunner<Runner>>,
    ) {
        (self.catalog, self.executor)
    }
}

impl UnsandboxedExecTool<TokioProcessRunner> {
    /// Builds the production unsandboxed tool with fixed confirmation posture.
    pub fn try_new_production(
        workspace_root: impl AsRef<Path>,
        supervisor_program: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        Self::try_new(
            TokioProcessRunner::try_new(supervisor_program)?,
            workspace_root,
        )
    }
}

fn build_tool<Contract, CommandRunner>(
    command_runner: CommandRunner,
    permission: ToolPermissionDefault,
) -> Result<(CompiledToolCatalog, ExecExecutor<CommandRunner>), ExecToolConstructionError>
where
    Contract: ToolContract<Arguments = ExecArguments>,
    CommandRunner: CommandExecution,
{
    let detail = ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
        .map_err(|_| ExecToolConstructionError::ErrorDetail)?;
    let definition =
        compile_contract_definition::<Contract>(permission, ToolEffectClass::ExternalEffect)
            .map_err(|error| match error {
                ToolContractCompileError::Name => ExecToolConstructionError::Name,
                ToolContractCompileError::Schema => ExecToolConstructionError::Schema,
            })?;
    let compiled = CompiledTool::new(
        definition,
        ExecArgumentValidator {
            detail: detail.clone(),
        },
    );
    let catalog = CompiledToolCatalog::try_new([compiled])
        .map_err(|_| ExecToolConstructionError::Duplicate)?;
    Ok((catalog, ExecExecutor { command_runner }))
}

#[derive(Clone, Debug)]
struct ExecArgumentValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for ExecArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(arguments)
            .map(drop)
            .map_err(|_| self.detail.clone())
    }
}

/// Direct-command arguments violated a bound or workspace-relative shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidExecArguments;

impl fmt::Display for InvalidExecArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(INVALID_ARGUMENTS_DETAIL)
    }
}

impl Error for InvalidExecArguments {}

fn decode_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<ExecArguments, InvalidExecArguments> {
    let decoded: ExecArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidExecArguments)?;
    validate_arguments(&decoded)?;
    Ok(decoded)
}

fn validate_arguments(arguments: &ExecArguments) -> Result<(), InvalidExecArguments> {
    if arguments.program.is_empty()
        || arguments.program.contains('\0')
        || arguments.program.chars().count() > MAX_PROGRAM_CHARACTERS
        || arguments.program.len() > MAX_PROGRAM_BYTES
        || arguments.arguments.len() > MAX_ARGUMENTS
        || arguments.timeout_seconds == 0
        || arguments.timeout_seconds > MAX_TIMEOUT_SECONDS
        || invalid_relative_directory(&arguments.working_directory)
    {
        return Err(InvalidExecArguments);
    }
    let mut total_argument_bytes = 0_usize;
    for argument in &arguments.arguments {
        if argument.contains('\0')
            || argument.chars().count() > MAX_ARGUMENT_CHARACTERS
            || argument.len() > MAX_ARGUMENT_BYTES
        {
            return Err(InvalidExecArguments);
        }
        total_argument_bytes = total_argument_bytes.saturating_add(argument.len());
    }
    if total_argument_bytes > MAX_TOTAL_ARGUMENT_BYTES {
        return Err(InvalidExecArguments);
    }
    Ok(())
}

fn invalid_relative_directory(value: &str) -> bool {
    value.is_empty()
        || value.contains('\0')
        || value.chars().count() > MAX_WORKING_DIRECTORY_CHARACTERS
        || value.len() > MAX_WORKING_DIRECTORY_BYTES
        || Path::new(value).is_absolute()
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Executor shared by sandboxed and unsandboxed direct-command tools.
#[derive(Clone, Debug)]
pub struct ExecExecutor<CommandRunner> {
    command_runner: CommandRunner,
}

/// A checked catalog/executor assumption failed inside command execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecExecutorError {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// Compact structured result encoding unexpectedly failed.
    ResultEncoding,
}

impl fmt::Display for ExecExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ArgumentValidationDrift => "exec argument validation drifted",
            Self::ResultEncoding => "exec result encoding failed",
        })
    }
}

impl Error for ExecExecutorError {}

impl ClassifyOperatorFailure for ExecExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl<CommandRunner: CommandExecution> ToolExecutor for ExecExecutor<CommandRunner> {
    type Error = ExecExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let arguments = decode_arguments(invocation.request().arguments())
            .map_err(|_| ExecExecutorError::ArgumentValidationDrift)?;
        let result = self.command_runner.execute(arguments).await;
        let encoded =
            serde_json::to_string(&result).map_err(|_| ExecExecutorError::ResultEncoding)?;
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText(encoded)))
    }
}

trait CommandExecution: Clone + Send {
    fn execute(&mut self, arguments: ExecArguments) -> impl Future<Output = ExecResult> + Send;
}

/// Exact bounded request supplied to an injected process runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRequest {
    /// Executable name or path.
    pub program: OsString,
    /// Exact argument-vector elements after the program name.
    pub arguments: Vec<OsString>,
    /// Ambient working directory used to spawn the executable.
    pub working_directory: PathBuf,
    /// Whole-process deadline.
    pub timeout: Duration,
    /// Per-stream retained byte limit.
    pub capture_bytes: usize,
    /// Exact environment additions or overrides.
    pub environment: BTreeMap<OsString, OsString>,
    /// Whether the ambient parent environment remains visible.
    pub environment_inheritance: ProcessEnvironment,
}

/// Ambient-environment posture for an injected process request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessEnvironment {
    /// Preserve the parent environment before applying explicit entries.
    Inherit,
    /// Clear the parent environment before applying explicit entries.
    Clear,
}

/// Typed evidence from a bubblewrap usability probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BwrapAvailability {
    /// The configured profile successfully started a trivial process.
    Available,
    /// No bubblewrap executable was found.
    Missing,
    /// The platform or host policy prevented the profile from starting.
    Unusable,
    /// The exact profile probe exhausted the request deadline.
    TimedOut,
}

/// Injectable one-shot process spawning and bubblewrap probing.
pub trait ProcessRunner: Clone + Send {
    /// Exact helper executable used to prove sandboxed target startup.
    fn sandbox_launcher_program(&self) -> &Path;

    /// Probes the exact bubblewrap profile used for later execution.
    fn bwrap_availability(
        &mut self,
        probe: ProcessRequest,
    ) -> impl Future<Output = BwrapAvailability> + Send;

    /// Runs at most one process tree under the supplied finite limits.
    fn run(&mut self, request: ProcessRequest) -> impl Future<Output = ProcessRunResult> + Send;
}

/// Production Tokio process runner using an isolated Linux supervisor process.
#[derive(Clone, Debug)]
pub struct TokioProcessRunner {
    supervisor_program: PathBuf,
    #[cfg(target_os = "linux")]
    _supervisor: Arc<rustix::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
fn inherited_descriptor_above_standard_streams(
    descriptor: rustix::fd::OwnedFd,
) -> Result<rustix::fd::OwnedFd, rustix::io::Errno> {
    if rustix::fd::AsRawFd::as_raw_fd(&descriptor) >= 3 {
        return Ok(descriptor);
    }
    let mut lower_descriptors = vec![descriptor];
    loop {
        let duplicate = rustix::io::dup(&lower_descriptors[0])?;
        if rustix::fd::AsRawFd::as_raw_fd(&duplicate) >= 3 {
            return Ok(duplicate);
        }
        lower_descriptors.push(duplicate);
    }
}

impl TokioProcessRunner {
    /// Pins the separately packaged Linux supervisor executable.
    pub fn try_new(
        supervisor_program: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        let supplied = supervisor_program.as_ref();
        let canonical = supplied.canonicalize().map_err(|source| {
            ExecToolConstructionError::SupervisorProgram {
                path: supplied.to_owned(),
                source: Some(source),
            }
        })?;
        if !canonical.is_absolute() || !canonical.is_file() {
            return Err(ExecToolConstructionError::SupervisorProgram {
                path: supplied.to_owned(),
                source: None,
            });
        }
        #[cfg(target_os = "linux")]
        let (supervisor_program, _supervisor) = {
            let supervisor = rustix::fs::open(
                &canonical,
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .map_err(|source| ExecToolConstructionError::SupervisorProgram {
                path: supplied.to_owned(),
                source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
            })?;
            let supervisor =
                inherited_descriptor_above_standard_streams(supervisor).map_err(|source| {
                    ExecToolConstructionError::SupervisorProgram {
                        path: supplied.to_owned(),
                        source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                    }
                })?;
            let metadata = rustix::fs::fstat(&supervisor).map_err(|source| {
                ExecToolConstructionError::SupervisorProgram {
                    path: supplied.to_owned(),
                    source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                }
            })?;
            if rustix::fs::FileType::from_raw_mode(metadata.st_mode)
                != rustix::fs::FileType::RegularFile
            {
                return Err(ExecToolConstructionError::SupervisorProgram {
                    path: supplied.to_owned(),
                    source: None,
                });
            }
            let pinned_program = PathBuf::from(format!(
                "/proc/self/fd/{}",
                rustix::fd::AsRawFd::as_raw_fd(&supervisor)
            ));
            (pinned_program, Arc::new(supervisor))
        };
        #[cfg(not(target_os = "linux"))]
        let supervisor_program = canonical;
        Ok(Self {
            supervisor_program,
            #[cfg(target_os = "linux")]
            _supervisor,
        })
    }
}

impl ProcessRunner for TokioProcessRunner {
    fn sandbox_launcher_program(&self) -> &Path {
        &self.supervisor_program
    }

    async fn bwrap_availability(&mut self, probe: ProcessRequest) -> BwrapAvailability {
        let result = run_process(&self.supervisor_program, probe).await;
        classify_bwrap_availability(result.outcome)
    }

    async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
        run_process(&self.supervisor_program, request).await
    }
}

fn classify_bwrap_availability(outcome: ProcessOutcome) -> BwrapAvailability {
    match outcome {
        ProcessOutcome::Exited { code: Some(0) } => BwrapAvailability::Available,
        ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::NotFound,
        } => BwrapAvailability::Missing,
        ProcessOutcome::TimedOut => BwrapAvailability::TimedOut,
        ProcessOutcome::Exited { .. }
        | ProcessOutcome::SpawnFailed { .. }
        | ProcessOutcome::SupervisionFailed { .. } => BwrapAvailability::Unusable,
    }
}

/// Sandboxed command service reusable by higher-level tools.
#[derive(Clone, Debug)]
pub struct SandboxedCommandRunner<Runner> {
    runner: Runner,
    workspace_root: PathBuf,
    sandbox_launcher: PathBuf,
    #[cfg(target_os = "linux")]
    workspace_identity: WorkspaceIdentity,
}

impl<Runner: ProcessRunner> SandboxedCommandRunner<Runner> {
    /// Admits one canonical injected workspace root.
    pub fn try_new(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        let workspace_root = canonical_workspace_root(workspace_root.as_ref())?;
        let sandbox_launcher = runner.sandbox_launcher_program().to_owned();
        Ok(Self {
            runner,
            sandbox_launcher,
            #[cfg(target_os = "linux")]
            workspace_identity: WorkspaceIdentity::capture(&workspace_root)?,
            workspace_root,
        })
    }

    /// Validates and runs one command under the production sandbox contract.
    pub async fn try_run(
        &mut self,
        arguments: ExecArguments,
    ) -> Result<ExecResult, InvalidExecArguments> {
        validate_arguments(&arguments)?;
        Ok(self.run_with_capture(arguments, EXEC_CAPTURE_BYTES).await)
    }

    pub(crate) async fn run_with_capture(
        &mut self,
        arguments: ExecArguments,
        capture_bytes: usize,
    ) -> ExecResult {
        let requested_timeout = Duration::from_secs(arguments.timeout_seconds);
        let deadline = tokio::time::Instant::now() + requested_timeout;
        #[cfg(target_os = "linux")]
        if !self.workspace_identity.matches(&self.workspace_root) {
            return ExecResult {
                confinement: ExecutionConfinement::SandboxSetupFailed,
                outcome: ProcessOutcome::SpawnFailed {
                    reason: ProcessSpawnFailure::SandboxSetup,
                },
                stdout: OutputCapture::empty(),
                stderr: OutputCapture::empty(),
            };
        }
        let probe_program = sandbox_shell(&self.workspace_root)
            .to_string_lossy()
            .into_owned();
        let probe = bwrap_request(
            SandboxLaunchContext {
                workspace_root: &self.workspace_root,
                #[cfg(target_os = "linux")]
                bind_source: &self.workspace_identity.bind_source,
                #[cfg(not(target_os = "linux"))]
                bind_source: &self.workspace_root,
                launcher: &self.sandbox_launcher,
            },
            &probe_program,
            &[String::from("-c"), String::from("exit 0")],
            ".",
            requested_timeout.min(Duration::from_secs(5)),
            8 * 1024,
        );
        let availability = self.runner.bwrap_availability(probe).await;
        match availability {
            BwrapAvailability::Available => {
                #[cfg(target_os = "linux")]
                if !self.workspace_identity.matches(&self.workspace_root) {
                    return ExecResult {
                        confinement: ExecutionConfinement::SandboxSetupFailed,
                        outcome: ProcessOutcome::SpawnFailed {
                            reason: ProcessSpawnFailure::SandboxSetup,
                        },
                        stdout: OutputCapture::empty(),
                        stderr: OutputCapture::empty(),
                    };
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return ExecResult {
                        confinement: ExecutionConfinement::SandboxSetupFailed,
                        outcome: ProcessOutcome::TimedOut,
                        stdout: OutputCapture::empty(),
                        stderr: OutputCapture::empty(),
                    };
                }
                let request = bwrap_request(
                    SandboxLaunchContext {
                        workspace_root: &self.workspace_root,
                        #[cfg(target_os = "linux")]
                        bind_source: &self.workspace_identity.bind_source,
                        #[cfg(not(target_os = "linux"))]
                        bind_source: &self.workspace_root,
                        launcher: &self.sandbox_launcher,
                    },
                    &arguments.program,
                    &arguments.arguments,
                    &arguments.working_directory,
                    remaining,
                    capture_bytes,
                );
                sandbox_process_result(self.runner.run(request).await, capture_bytes)
            }
            BwrapAvailability::Missing | BwrapAvailability::Unusable => ExecResult {
                confinement: ExecutionConfinement::SandboxRefused { availability },
                outcome: ProcessOutcome::SpawnFailed {
                    reason: ProcessSpawnFailure::SandboxUnavailable,
                },
                stdout: OutputCapture::empty(),
                stderr: OutputCapture::empty(),
            },
            BwrapAvailability::TimedOut => ExecResult {
                confinement: ExecutionConfinement::SandboxSetupFailed,
                outcome: ProcessOutcome::TimedOut,
                stdout: OutputCapture::empty(),
                stderr: OutputCapture::empty(),
            },
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct WorkspaceIdentity {
    device: u64,
    inode: u64,
    bind_source: PathBuf,
    _directory: Arc<rustix::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
impl WorkspaceIdentity {
    fn capture(path: &Path) -> Result<Self, ExecToolConstructionError> {
        let directory = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| ExecToolConstructionError::WorkspaceRoot {
            path: path.to_owned(),
            source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
        })?;
        let directory =
            inherited_descriptor_above_standard_streams(directory).map_err(|source| {
                ExecToolConstructionError::WorkspaceRoot {
                    path: path.to_owned(),
                    source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                }
            })?;
        let pinned_metadata = rustix::fs::fstat(&directory).map_err(|source| {
            ExecToolConstructionError::WorkspaceRoot {
                path: path.to_owned(),
                source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
            }
        })?;
        let bind_source = PathBuf::from(format!(
            "/proc/self/fd/{}",
            rustix::fd::AsRawFd::as_raw_fd(&directory)
        ));
        Ok(Self {
            device: pinned_metadata.st_dev,
            inode: pinned_metadata.st_ino,
            bind_source,
            _directory: Arc::new(directory),
        })
    }

    fn matches(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok_and(|metadata| {
            metadata.file_type().is_dir()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        })
    }

    fn pin_relative_directory(
        &self,
        path: &str,
    ) -> Result<WorkspaceDirectoryIdentity, ProcessSpawnFailure> {
        if invalid_relative_directory(path) {
            return Err(ProcessSpawnFailure::Other);
        }
        let mut directory = None;
        for component in Path::new(path).components() {
            let Component::Normal(name) = component else {
                continue;
            };
            let parent = directory.as_ref().unwrap_or(self._directory.as_ref());
            directory = Some(
                rustix::fs::openat(
                    parent,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(workspace_directory_failure)?,
            );
        }
        let directory = match directory {
            Some(directory) => directory,
            None => rustix::fs::openat(
                self._directory.as_ref(),
                ".",
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(workspace_directory_failure)?,
        };
        let bind_source = PathBuf::from(format!(
            "/proc/self/fd/{}",
            rustix::fd::AsRawFd::as_raw_fd(&directory)
        ));
        Ok(WorkspaceDirectoryIdentity {
            bind_source,
            _directory: directory,
        })
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct WorkspaceDirectoryIdentity {
    bind_source: PathBuf,
    _directory: rustix::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
fn workspace_directory_failure(error: rustix::io::Errno) -> ProcessSpawnFailure {
    match error {
        rustix::io::Errno::NOENT => ProcessSpawnFailure::NotFound,
        rustix::io::Errno::ACCESS | rustix::io::Errno::PERM => {
            ProcessSpawnFailure::PermissionDenied
        }
        _ => ProcessSpawnFailure::Other,
    }
}

impl<Runner: ProcessRunner> CommandExecution for SandboxedCommandRunner<Runner> {
    async fn execute(&mut self, arguments: ExecArguments) -> ExecResult {
        self.run_with_capture(arguments, EXEC_CAPTURE_BYTES).await
    }
}

/// Unsandboxed command service reusable by its tool executor.
#[derive(Clone, Debug)]
pub struct UnsandboxedCommandRunner<Runner> {
    runner: Runner,
    #[cfg(not(target_os = "linux"))]
    workspace_root: PathBuf,
    #[cfg(target_os = "linux")]
    workspace_identity: WorkspaceIdentity,
}

impl<Runner: ProcessRunner> UnsandboxedCommandRunner<Runner> {
    /// Admits one canonical injected workspace root.
    pub fn try_new(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        let workspace_root = canonical_workspace_root(workspace_root.as_ref())?;
        Ok(Self {
            runner,
            #[cfg(target_os = "linux")]
            workspace_identity: WorkspaceIdentity::capture(&workspace_root)?,
            #[cfg(not(target_os = "linux"))]
            workspace_root,
        })
    }
}

impl<Runner: ProcessRunner> CommandExecution for UnsandboxedCommandRunner<Runner> {
    async fn execute(&mut self, arguments: ExecArguments) -> ExecResult {
        #[cfg(target_os = "linux")]
        let execution_directory = match self
            .workspace_identity
            .pin_relative_directory(&arguments.working_directory)
        {
            Ok(directory) => directory,
            Err(reason) => {
                return ExecResult {
                    confinement: ExecutionConfinement::Unsandboxed,
                    outcome: ProcessOutcome::SpawnFailed { reason },
                    stdout: OutputCapture::empty(),
                    stderr: OutputCapture::empty(),
                };
            }
        };
        #[cfg(target_os = "linux")]
        let execution_root = &execution_directory.bind_source;
        #[cfg(not(target_os = "linux"))]
        let execution_root = &self.workspace_root;
        #[cfg(target_os = "linux")]
        let relative_working_directory = ".";
        #[cfg(not(target_os = "linux"))]
        let relative_working_directory = arguments.working_directory.as_str();
        let request = direct_request(
            execution_root,
            relative_working_directory,
            &arguments,
            EXEC_CAPTURE_BYTES,
        );
        process_result(
            ExecutionConfinement::Unsandboxed,
            self.runner.run(request).await,
        )
    }
}

fn canonical_workspace_root(root: &Path) -> Result<PathBuf, ExecToolConstructionError> {
    let canonical =
        root.canonicalize()
            .map_err(|source| ExecToolConstructionError::WorkspaceRoot {
                path: root.to_owned(),
                source: Some(source),
            })?;
    if !canonical.is_absolute() || !canonical.is_dir() {
        return Err(ExecToolConstructionError::WorkspaceRoot {
            path: root.to_owned(),
            source: None,
        });
    }
    Ok(canonical)
}

fn direct_request(
    root: &Path,
    working_directory: &str,
    arguments: &ExecArguments,
    capture_bytes: usize,
) -> ProcessRequest {
    ProcessRequest {
        program: OsString::from(&arguments.program),
        arguments: arguments.arguments.iter().map(OsString::from).collect(),
        working_directory: root.join(working_directory),
        timeout: Duration::from_secs(arguments.timeout_seconds),
        capture_bytes,
        environment: BTreeMap::new(),
        environment_inheritance: ProcessEnvironment::Inherit,
    }
}

#[derive(Clone, Copy)]
struct SandboxLaunchContext<'a> {
    workspace_root: &'a Path,
    bind_source: &'a Path,
    launcher: &'a Path,
}

fn bwrap_request(
    context: SandboxLaunchContext<'_>,
    program: &str,
    arguments: &[String],
    working_directory: &str,
    timeout: Duration,
    capture_bytes: usize,
) -> ProcessRequest {
    let sandbox_path = sandbox_path(context.workspace_root);
    let sandbox_directory = if working_directory == "." {
        String::from(SANDBOX_WORKSPACE)
    } else {
        format!("{SANDBOX_WORKSPACE}/{working_directory}")
    };
    let mut bwrap_arguments = [
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--ro-bind-try",
        "/usr",
        "/usr",
        "--ro-bind-try",
        "/bin",
        "/bin",
        "--ro-bind-try",
        "/lib",
        "/lib",
        "--ro-bind-try",
        "/lib64",
        "/lib64",
        "--ro-bind-try",
        "/nix/store",
        "/nix/store",
        "--dir",
        "/etc",
        "--ro-bind-try",
        "/etc/alternatives",
        "/etc/alternatives",
        "--ro-bind-try",
        "/etc/hosts",
        "/etc/hosts",
        "--ro-bind-try",
        "/etc/nsswitch.conf",
        "/etc/nsswitch.conf",
        "--ro-bind-try",
        "/etc/resolv.conf",
        "/etc/resolv.conf",
        "--ro-bind-try",
        "/etc/ssl",
        "/etc/ssl",
        "--bind",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    bwrap_arguments.push(context.bind_source.as_os_str().to_owned());
    bwrap_arguments.push(OsString::from(SANDBOX_WORKSPACE));
    bwrap_arguments.extend([
        OsString::from("--ro-bind"),
        context.launcher.as_os_str().to_owned(),
        OsString::from(SANDBOX_DISPATCH_PROGRAM),
        OsString::from("--chdir"),
        OsString::from(sandbox_directory),
        OsString::from("--setenv"),
        OsString::from("HOME"),
        OsString::from(SANDBOX_WORKSPACE),
        OsString::from("--"),
        OsString::from(SANDBOX_DISPATCH_PROGRAM),
        OsString::from("--dispatch"),
        OsString::from(program),
    ]);
    bwrap_arguments.extend(arguments.iter().map(OsString::from));
    ProcessRequest {
        program: OsString::from(BWRAP_PROGRAM),
        arguments: bwrap_arguments,
        working_directory: context.bind_source.to_owned(),
        timeout,
        capture_bytes: capture_bytes.saturating_add(SANDBOX_DISPATCH_MARKER.len()),
        environment: BTreeMap::from([
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
            (OsString::from("LC_ALL"), OsString::from("C.UTF-8")),
            (OsString::from("PATH"), sandbox_path),
        ]),
        environment_inheritance: ProcessEnvironment::Clear,
    }
}

fn sandbox_path(workspace_root: &Path) -> OsString {
    let inherited = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut components = Vec::new();
    for path in inherited
        .into_iter()
        .chain(std::env::split_paths(&OsString::from(
            SANDBOX_FALLBACK_PATH,
        )))
    {
        if let Some(canonical) = trusted_sandbox_path(&path, workspace_root)
            && seen.insert(canonical.clone())
        {
            components.push(canonical);
        }
    }
    std::env::join_paths(components).unwrap_or_default()
}

fn sandbox_shell(workspace_root: &Path) -> PathBuf {
    executable_sandbox_shell(&sandbox_path(workspace_root))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn executable_sandbox_shell(path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join("sh"))
        .find(|candidate| sandbox_program_is_executable(candidate))
}

fn sandbox_program_is_executable(candidate: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        candidate.is_file()
    }
}

fn trusted_sandbox_path(path: &Path, workspace_root: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    let trusted = [
        Path::new("/bin"),
        Path::new("/nix/store"),
        Path::new("/nix/var/nix/profiles/default"),
        Path::new("/run/current-system/sw"),
        Path::new("/sbin"),
        Path::new("/usr"),
    ]
    .into_iter()
    .filter_map(|trusted_root| trusted_root.canonicalize().ok())
    .any(|trusted_root| canonical.starts_with(trusted_root));
    (canonical.is_dir() && !canonical.starts_with(workspace_root) && trusted).then_some(canonical)
}

/// Structured result returned by either direct-command tool.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ExecResult {
    /// Filesystem execution posture actually selected.
    pub confinement: ExecutionConfinement,
    /// Terminal process evidence.
    pub outcome: ProcessOutcome,
    /// Bounded standard output capture.
    pub stdout: OutputCapture,
    /// Bounded standard error capture.
    pub stderr: OutputCapture,
}

/// Filesystem authority actually used for one execution attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionConfinement {
    /// Bubblewrap started the command with workspace-only project authority.
    FilesystemConfined,
    /// The separately composed confirmation-required tool ran directly.
    Unsandboxed,
    /// Bubblewrap was missing or unusable; no requested command was started.
    SandboxRefused {
        /// Typed host evidence from the exact profile probe.
        availability: BwrapAvailability,
    },
    /// Bubblewrap setup did not complete or confirm target dispatch.
    SandboxSetupFailed,
}

/// Terminal process-tree outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessOutcome {
    /// The supervised leader exited and all observed descendants were reaped.
    Exited {
        /// Portable exit code, absent when a signal ended the leader.
        code: Option<i32>,
    },
    /// The bounded deadline elapsed and the entire observed process tree was killed.
    TimedOut,
    /// No requested process tree was started.
    SpawnFailed {
        /// Closed sanitized spawn classification.
        reason: ProcessSpawnFailure,
    },
    /// Supervision failed and the observed process tree was killed fail-closed.
    SupervisionFailed {
        /// Closed failing supervision stage.
        reason: ProcessSupervisionFailure,
    },
}

/// Closed reason a requested process did not start.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSpawnFailure {
    /// An executable or request path lookup failed.
    NotFound,
    /// The host denied process creation.
    PermissionDenied,
    /// Complete process-tree supervision is unavailable on this platform.
    ProcessTreeUnsupported,
    /// Bubblewrap evidence refused sandboxed dispatch.
    SandboxUnavailable,
    /// Bubblewrap did not confirm that it dispatched the requested target.
    SandboxSetup,
    /// Another sanitized spawn failure occurred.
    Other,
}

/// Closed stage at which process supervision failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSupervisionFailure {
    /// Waiting for the supervised process tree failed.
    Wait,
    /// Killing or reaping the process tree failed.
    Cleanup,
    /// Reading standard output failed.
    Stdout,
    /// Reading standard error failed.
    Stderr,
}

/// One bounded UTF-8 presentation of process bytes.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct OutputCapture {
    /// Retained output prefix, replacing invalid UTF-8 sequences when needed.
    pub text: String,
    /// Whether the byte stream fit in the retained prefix.
    pub completeness: CaptureCompleteness,
    /// Whether presentation preserved all retained bytes as UTF-8.
    pub encoding: OutputEncoding,
}

impl OutputCapture {
    fn empty() -> Self {
        Self {
            text: String::new(),
            completeness: CaptureCompleteness::Complete,
            encoding: OutputEncoding::Utf8,
        }
    }
}

/// Whether bytes were discarded after the retained prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureCompleteness {
    /// The complete stream was retained.
    Complete,
    /// At least one byte beyond the retained prefix was observed and discarded.
    Truncated,
}

/// UTF-8 status of the retained output bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputEncoding {
    /// Every retained byte was valid UTF-8.
    Utf8,
    /// Invalid sequences were replaced for JSON presentation.
    LossyUtf8,
}

/// Raw bounded result returned by an injected process runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRunResult {
    /// Terminal process-tree outcome.
    pub outcome: ProcessOutcome,
    /// Bounded standard output bytes.
    pub stdout: ProcessOutput,
    /// Bounded standard error bytes.
    pub stderr: ProcessOutput,
}

/// One retained raw process stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    /// Retained byte prefix.
    pub bytes: Vec<u8>,
    /// Whether the complete byte stream fit in the prefix.
    pub completeness: CaptureCompleteness,
}

fn process_result(confinement: ExecutionConfinement, result: ProcessRunResult) -> ExecResult {
    ExecResult {
        confinement,
        outcome: result.outcome,
        stdout: output_capture(result.stdout),
        stderr: output_capture(result.stderr),
    }
}

fn sandbox_process_result(mut result: ProcessRunResult, capture_bytes: usize) -> ExecResult {
    let dispatched = result.stderr.bytes.starts_with(SANDBOX_DISPATCH_MARKER);
    if result.stderr.bytes.starts_with(SANDBOX_DISPATCH_MARKER) {
        result.stderr.bytes.drain(..SANDBOX_DISPATCH_MARKER.len());
    }
    truncate_process_output(&mut result.stdout, capture_bytes);
    truncate_process_output(&mut result.stderr, capture_bytes);
    if dispatched {
        return process_result(ExecutionConfinement::FilesystemConfined, result);
    }
    let outcome = if result.outcome == ProcessOutcome::TimedOut {
        ProcessOutcome::TimedOut
    } else {
        ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::SandboxSetup,
        }
    };
    ExecResult {
        confinement: ExecutionConfinement::SandboxSetupFailed,
        outcome,
        stdout: output_capture(result.stdout),
        stderr: output_capture(result.stderr),
    }
}

fn truncate_process_output(output: &mut ProcessOutput, limit: usize) {
    if output.bytes.len() > limit {
        output.bytes.truncate(limit);
        output.completeness = CaptureCompleteness::Truncated;
    }
}

fn output_capture(output: ProcessOutput) -> OutputCapture {
    let encoding = if std::str::from_utf8(&output.bytes).is_ok() {
        OutputEncoding::Utf8
    } else {
        OutputEncoding::LossyUtf8
    };
    OutputCapture {
        text: String::from_utf8_lossy(&output.bytes).into_owned(),
        completeness: output.completeness,
        encoding,
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct BoundedBytes {
    bytes: Vec<u8>,
    completeness: CaptureCompleteness,
}

#[cfg(target_os = "linux")]
async fn read_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<BoundedBytes> {
    let mut retained = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 8192];
    let mut completeness = CaptureCompleteness::Complete;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        let retained_from_chunk = remaining.min(read);
        retained.extend_from_slice(&buffer[..retained_from_chunk]);
        if retained_from_chunk < read {
            completeness = CaptureCompleteness::Truncated;
        }
    }
    Ok(BoundedBytes {
        bytes: retained,
        completeness,
    })
}

#[cfg(target_os = "linux")]
async fn read_supervised_stdout(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(BoundedBytes, SupervisorStatus)> {
    let mut retained = Vec::with_capacity(limit);
    let mut tail = Vec::with_capacity(SUPERVISOR_STATUS_TAIL_BYTES);
    let mut buffer = [0_u8; 8192];
    let mut total_bytes = 0_usize;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(read);
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..remaining.min(read)]);
        tail.extend_from_slice(&buffer[..read]);
        if tail.len() > SUPERVISOR_STATUS_TAIL_BYTES {
            tail.drain(..tail.len() - SUPERVISOR_STATUS_TAIL_BYTES);
        }
    }
    if tail.last() != Some(&b'\n') {
        return Err(std::io::Error::other(
            "supervisor status trailer is malformed",
        ));
    }
    let marker = tail
        .windows(SUPERVISOR_STATUS_TRAILER.len())
        .rposition(|window| window == SUPERVISOR_STATUS_TRAILER)
        .ok_or_else(|| std::io::Error::other("supervisor status trailer is missing"))?;
    let encoded = &tail[marker + SUPERVISOR_STATUS_TRAILER.len()..tail.len() - 1];
    let status = serde_json::from_slice(encoded)
        .map_err(|_| std::io::Error::other("supervisor status trailer is malformed"))?;
    let trailer_bytes = tail.len() - marker;
    let output_bytes = total_bytes.saturating_sub(trailer_bytes);
    retained.truncate(output_bytes.min(limit));
    Ok((
        BoundedBytes {
            bytes: retained,
            completeness: if output_bytes > limit {
                CaptureCompleteness::Truncated
            } else {
                CaptureCompleteness::Complete
            },
        },
        status,
    ))
}

#[cfg(target_os = "linux")]
struct OuterTrackedProcess {
    pidfd: rustix::fd::OwnedFd,
    start_time: u64,
}

#[cfg(target_os = "linux")]
struct OuterProcessTreeGuard {
    root: u32,
    descendants: Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>,
    stop: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
    process_tree_supported: Arc<AtomicBool>,
    armed: bool,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OuterCleanupStatus {
    Complete,
    ProcessTreeUnsupported,
    Failed,
}

#[cfg(target_os = "linux")]
impl OuterProcessTreeGuard {
    fn new(root: u32) -> Result<Self, ()> {
        let root_process = outer_pin_process(root)?.ok_or(())?;
        let descendants = Arc::new(Mutex::new(BTreeMap::from([(root, root_process)])));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher_descendants = Arc::clone(&descendants);
        let watcher_stop = Arc::clone(&stop);
        let process_tree_supported = Arc::new(AtomicBool::new(true));
        let watcher_process_tree_supported = Arc::clone(&process_tree_supported);
        let watcher = std::thread::spawn(move || {
            while !watcher_stop.load(Ordering::Acquire) {
                if outer_observe_descendants(root, &watcher_descendants).is_err() {
                    watcher_process_tree_supported.store(false, Ordering::Release);
                    return;
                }
                std::thread::sleep(OUTER_PROCESS_POLL_INTERVAL);
            }
        });
        Ok(Self {
            root,
            descendants,
            stop,
            watcher: Some(watcher),
            process_tree_supported,
            armed: true,
        })
    }

    fn root_exited(&self) -> Result<bool, ()> {
        let descendants = self
            .descendants
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let root = descendants.get(&self.root).ok_or(())?;
        outer_pidfd_has_exited(&root.pidfd)
    }

    fn kill_all(&mut self) {
        self.stop_watcher();
        if outer_observe_descendants(self.root, &self.descendants).is_err() {
            self.process_tree_supported.store(false, Ordering::Release);
        }
        self.kill_tracked();
    }

    fn finish(&mut self) -> OuterCleanupStatus {
        self.stop_watcher();
        let deadline = Instant::now() + OUTER_PROCESS_CLEANUP_DEADLINE;
        loop {
            if outer_observe_descendants(self.root, &self.descendants).is_err() {
                self.process_tree_supported.store(false, Ordering::Release);
            }
            self.kill_tracked();
            outer_reap_tracked(&self.descendants);
            if self.all_tracked_absent() {
                self.armed = false;
                return if self.process_tree_supported.load(Ordering::Acquire) {
                    OuterCleanupStatus::Complete
                } else {
                    OuterCleanupStatus::ProcessTreeUnsupported
                };
            }
            if Instant::now() >= deadline {
                return OuterCleanupStatus::Failed;
            }
            std::thread::sleep(OUTER_PROCESS_POLL_INTERVAL);
        }
    }

    fn kill_tracked(&self) {
        let descendants = self
            .descendants
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for process in descendants.values() {
            let _ =
                rustix::process::pidfd_send_signal(&process.pidfd, rustix::process::Signal::KILL);
        }
    }

    fn all_tracked_absent(&self) -> bool {
        let snapshot = {
            let descendants = self
                .descendants
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            descendants
                .iter()
                .map(|(raw_pid, process)| (*raw_pid, process.start_time))
                .collect::<Vec<_>>()
        };
        snapshot.into_iter().all(|(raw_pid, expected_start_time)| {
            outer_process_start_time(raw_pid)
                .is_ok_and(|start_time| start_time != Some(expected_start_time))
        })
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

#[cfg(target_os = "linux")]
impl Drop for OuterProcessTreeGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.finish();
        }
    }
}

#[cfg(target_os = "linux")]
fn preflight_outer_process_tree() -> Result<OuterTrackedProcess, ()> {
    outer_process_children(std::process::id()).map_err(|_| ())?;
    outer_pin_process(std::process::id())?.ok_or(())
}

#[cfg(target_os = "linux")]
fn outer_observe_descendants(
    root: u32,
    descendants: &Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>,
) -> Result<(), ()> {
    let mut known = {
        let tracked = descendants.lock().unwrap_or_else(PoisonError::into_inner);
        tracked
            .iter()
            .map(|(raw_pid, process)| (*raw_pid, process.start_time))
            .collect::<BTreeMap<_, _>>()
    };
    loop {
        let parents = known
            .iter()
            .map(|(raw_pid, start_time)| (*raw_pid, *start_time))
            .collect::<Vec<_>>();
        let mut additions = Vec::new();
        let mut retired = Vec::new();
        for (parent, expected_start_time) in parents {
            if outer_process_start_time(parent)? != Some(expected_start_time) {
                if parent != root {
                    known.remove(&parent);
                    retired.push((parent, expected_start_time));
                }
                continue;
            }
            let children = match outer_process_children(parent) {
                Ok(children) => children,
                Err(OuterProcessChildrenError::Gone) if parent == root => continue,
                Err(OuterProcessChildrenError::Gone) => {
                    known.remove(&parent);
                    retired.push((parent, expected_start_time));
                    continue;
                }
                Err(OuterProcessChildrenError::Unsupported) => return Err(()),
            };
            if outer_process_start_time(parent)? != Some(expected_start_time) {
                if parent != root {
                    known.remove(&parent);
                    retired.push((parent, expected_start_time));
                }
                continue;
            }
            for raw_pid in children {
                if known.contains_key(&raw_pid) {
                    continue;
                }
                if let Some(process) = outer_pin_process(raw_pid)? {
                    known.insert(raw_pid, process.start_time);
                    additions.push((raw_pid, process));
                }
            }
        }
        let discovered = !additions.is_empty();
        if !retired.is_empty() || discovered {
            let mut tracked = descendants.lock().unwrap_or_else(PoisonError::into_inner);
            for (raw_pid, expected_start_time) in retired {
                if tracked
                    .get(&raw_pid)
                    .is_some_and(|process| process.start_time == expected_start_time)
                {
                    tracked.remove(&raw_pid);
                }
            }
            for (raw_pid, process) in additions {
                if tracked
                    .get(&raw_pid)
                    .is_none_or(|tracked| tracked.start_time != process.start_time)
                {
                    tracked.insert(raw_pid, process);
                }
            }
        }
        if !discovered {
            break;
        }
    }
    outer_retire_reused(root, descendants)
}

#[cfg(target_os = "linux")]
fn outer_retire_reused(
    root: u32,
    descendants: &Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>,
) -> Result<(), ()> {
    let snapshot = {
        let tracked = descendants.lock().unwrap_or_else(PoisonError::into_inner);
        tracked
            .iter()
            .filter_map(|(raw_pid, process)| {
                (*raw_pid != root).then_some((*raw_pid, process.start_time))
            })
            .collect::<Vec<_>>()
    };
    let mut retired = Vec::new();
    for (raw_pid, expected_start_time) in snapshot {
        if outer_process_start_time(raw_pid)? != Some(expected_start_time) {
            retired.push((raw_pid, expected_start_time));
        }
    }
    let mut tracked = descendants.lock().unwrap_or_else(PoisonError::into_inner);
    for (raw_pid, expected_start_time) in retired {
        if tracked
            .get(&raw_pid)
            .is_some_and(|process| process.start_time == expected_start_time)
        {
            tracked.remove(&raw_pid);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn outer_pin_process(raw_pid: u32) -> Result<Option<OuterTrackedProcess>, ()> {
    let Some(start_time) = outer_process_start_time(raw_pid)? else {
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
    if outer_process_start_time(raw_pid)? != Some(start_time) {
        return Ok(None);
    }
    Ok(Some(OuterTrackedProcess { pidfd, start_time }))
}

#[cfg(target_os = "linux")]
fn outer_pidfd_has_exited(pidfd: &rustix::fd::OwnedFd) -> Result<bool, ()> {
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

#[cfg(target_os = "linux")]
fn outer_process_start_time(pid: u32) -> Result<Option<u64>, ()> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if outer_process_gone(&error) => return Ok(None),
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

#[cfg(target_os = "linux")]
fn outer_process_gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum OuterProcessChildrenError {
    Gone,
    Unsupported,
}

#[cfg(target_os = "linux")]
fn outer_process_children(pid: u32) -> Result<Vec<u32>, OuterProcessChildrenError> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).map_err(|error| {
        if outer_process_gone(&error) {
            OuterProcessChildrenError::Gone
        } else {
            OuterProcessChildrenError::Unsupported
        }
    })?;
    let mut children = Vec::new();
    let mut observed_task = false;
    for entry in tasks {
        let entry = entry.map_err(|_| OuterProcessChildrenError::Unsupported)?;
        let task = entry
            .file_name()
            .to_string_lossy()
            .parse::<u32>()
            .map_err(|_| OuterProcessChildrenError::Unsupported)?;
        match std::fs::read_to_string(format!("/proc/{pid}/task/{task}/children")) {
            Ok(values) => {
                observed_task = true;
                let parsed = values
                    .split_whitespace()
                    .map(str::parse::<u32>)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| OuterProcessChildrenError::Unsupported)?;
                children.extend(parsed);
            }
            Err(error) if outer_process_gone(&error) => {}
            Err(_) => return Err(OuterProcessChildrenError::Unsupported),
        }
    }
    observed_task
        .then_some(children)
        .ok_or(OuterProcessChildrenError::Gone)
}

#[cfg(target_os = "linux")]
fn outer_reap_tracked(descendants: &Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>) {
    let descendants = descendants.lock().unwrap_or_else(PoisonError::into_inner);
    for raw_pid in descendants.keys() {
        if let Some(pid) = rustix::process::Pid::from_raw(*raw_pid as i32) {
            let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG);
        }
    }
}

async fn run_process(supervisor_program: &Path, request: ProcessRequest) -> ProcessRunResult {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (supervisor_program, request);
        empty_process_result(ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::ProcessTreeUnsupported,
        })
    }
    #[cfg(target_os = "linux")]
    {
        run_process_linux(supervisor_program, request).await
    }
}

#[cfg(target_os = "linux")]
async fn run_process_linux(supervisor_program: &Path, request: ProcessRequest) -> ProcessRunResult {
    let request_deadline = tokio::time::Instant::now() + request.timeout;
    let outer_reservation = match preflight_outer_process_tree() {
        Ok(reservation) => reservation,
        Err(()) => {
            return empty_process_result(ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::ProcessTreeUnsupported,
            });
        }
    };
    let timeout_milliseconds = request.timeout.as_millis().min(u128::from(u64::MAX));
    let mut command = Command::new(supervisor_program);
    command
        .arg(SUPERVISOR_OUTER_MODE)
        .arg(timeout_milliseconds.to_string())
        .arg(&request.program)
        .args(&request.arguments)
        .current_dir(&request.working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if request.environment_inheritance == ProcessEnvironment::Clear {
        command.env_clear();
    }
    for (name, value) in request.environment {
        command.env(name, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return empty_process_result(ProcessOutcome::SpawnFailed {
                reason: match error.kind() {
                    std::io::ErrorKind::NotFound => ProcessSpawnFailure::NotFound,
                    std::io::ErrorKind::PermissionDenied => ProcessSpawnFailure::PermissionDenied,
                    _ => ProcessSpawnFailure::Other,
                },
            });
        }
    };
    let supervisor_pid = match child.id() {
        Some(supervisor_pid) => supervisor_pid,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return empty_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            });
        }
    };
    let mut outer_tree = match OuterProcessTreeGuard::new(supervisor_pid) {
        Ok(tree) => tree,
        Err(()) => {
            kill_supervisor_process_group(supervisor_pid);
            let _ = child.kill().await;
            let _ = child.wait().await;
            return empty_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            });
        }
    };
    drop(outer_reservation);
    let mut control = child.stdin.take();
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            drop(control);
            outer_tree.kill_all();
            let _ = child.wait().await;
            return empty_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stdout,
            });
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(control);
            outer_tree.kill_all();
            let _ = child.wait().await;
            return empty_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stderr,
            });
        }
    };
    let mut stdout_task = tokio::spawn(read_supervised_stdout(stdout, request.capture_bytes));
    let mut stderr_task = tokio::spawn(read_bounded(stderr, request.capture_bytes));
    let startup = tokio::time::timeout_at(request_deadline, async {
        let control = control.as_mut().ok_or(())?;
        control.write_all(&[1]).await.map_err(|_| ())
    })
    .await;
    if !matches!(startup, Ok(Ok(()))) {
        let timed_out = startup.is_err();
        drop(control);
        kill_supervisor_process_group(supervisor_pid);
        outer_tree.kill_all();
        let _ = child.kill().await;
        let _ = child.wait().await;
        let cleanup = outer_tree.finish();
        stdout_task.abort();
        stderr_task.abort();
        return empty_process_result(if timed_out && cleanup == OuterCleanupStatus::Complete {
            ProcessOutcome::TimedOut
        } else {
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            }
        });
    }
    let root_exit = tokio::time::timeout_at(request_deadline, async {
        loop {
            match outer_tree.root_exited() {
                Ok(true) => return Ok(()),
                Ok(false) => tokio::time::sleep(OUTER_PROCESS_POLL_INTERVAL).await,
                Err(()) => return Err(()),
            }
        }
    })
    .await;
    let request_timed_out = root_exit.is_err();
    let observation_failed = matches!(root_exit, Ok(Err(())));
    drop(control);
    let graceful_deadline = tokio::time::Instant::now() + OUTER_PROCESS_CLEANUP_DEADLINE;
    let mut waited = tokio::time::timeout_at(graceful_deadline, child.wait()).await;
    if waited.is_err() {
        kill_supervisor_process_group(supervisor_pid);
        outer_tree.kill_all();
        let _ = child.kill().await;
        waited = tokio::time::timeout(OUTER_PROCESS_CLEANUP_DEADLINE, child.wait()).await;
    }
    let mut wait_failure = if request_timed_out {
        Some(ProcessOutcome::TimedOut)
    } else if observation_failed {
        Some(ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Cleanup,
        })
    } else {
        None
    };
    let waited_cleanly = match waited {
        Ok(Ok(status)) if status.success() => None,
        Ok(Ok(_)) | Ok(Err(_)) => Some(ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Wait,
        }),
        Err(_) => {
            outer_tree.kill_all();
            let _ = child.kill().await;
            Some(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            })
        }
    };
    if wait_failure.is_none() {
        wait_failure = waited_cleanly;
    }
    if outer_tree.finish() != OuterCleanupStatus::Complete {
        wait_failure = Some(ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Cleanup,
        });
    }
    let captures = tokio::time::timeout(OUTER_PROCESS_CLEANUP_DEADLINE, async {
        let stdout = (&mut stdout_task).await;
        let stderr = (&mut stderr_task).await;
        (stdout, stderr)
    })
    .await;
    let (stdout, stderr) = match captures {
        Ok(captures) => captures,
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            return empty_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            });
        }
    };
    match (stdout, stderr) {
        (Ok(Ok((stdout, status))), Ok(Ok(stderr))) => ProcessRunResult {
            outcome: wait_failure.unwrap_or_else(|| supervisor_outcome(status)),
            stdout: ProcessOutput {
                bytes: stdout.bytes,
                completeness: stdout.completeness,
            },
            stderr: ProcessOutput {
                bytes: stderr.bytes,
                completeness: stderr.completeness,
            },
        },
        (Ok(Err(_)) | Err(_), _) => {
            empty_process_result(wait_failure.unwrap_or(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stdout,
            }))
        }
        (_, Ok(Err(_)) | Err(_)) => {
            empty_process_result(wait_failure.unwrap_or(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stderr,
            }))
        }
    }
}

#[cfg(target_os = "linux")]
fn kill_supervisor_process_group(raw_pid: u32) {
    if let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(target_os = "linux")]
fn supervisor_outcome(status: SupervisorStatus) -> ProcessOutcome {
    match status {
        SupervisorStatus::Exited { code } => ProcessOutcome::Exited { code },
        SupervisorStatus::TimedOut => ProcessOutcome::TimedOut,
        SupervisorStatus::SpawnFailed { reason } => ProcessOutcome::SpawnFailed {
            reason: match reason {
                SupervisorSpawnFailure::NotFound => ProcessSpawnFailure::NotFound,
                SupervisorSpawnFailure::PermissionDenied => ProcessSpawnFailure::PermissionDenied,
                SupervisorSpawnFailure::ProcessTreeUnsupported => {
                    ProcessSpawnFailure::ProcessTreeUnsupported
                }
                SupervisorSpawnFailure::Other => ProcessSpawnFailure::Other,
            },
        },
        SupervisorStatus::Cancelled => ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Wait,
        },
        SupervisorStatus::SupervisionFailed { stage } => ProcessOutcome::SupervisionFailed {
            reason: match stage {
                SupervisorFailureStage::Wait => ProcessSupervisionFailure::Wait,
                SupervisorFailureStage::Cleanup => ProcessSupervisionFailure::Cleanup,
            },
        },
    }
}

fn empty_process_result(outcome: ProcessOutcome) -> ProcessRunResult {
    ProcessRunResult {
        outcome,
        stdout: ProcessOutput {
            bytes: Vec::new(),
            completeness: CaptureCompleteness::Complete,
        },
        stderr: ProcessOutput {
            bytes: Vec::new(),
            completeness: CaptureCompleteness::Complete,
        },
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::{Arc, Mutex, PoisonError};

    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};

    use super::*;

    const SANDBOXED_STDOUT: &str = "checked";
    const SANDBOXED_WORKING_DIRECTORY: &str = "crate";
    const SETUP_CAPTURE_BYTES: usize = 4;
    const SETUP_STDOUT: &[u8] = b"12345";
    const SETUP_STDERR: &[u8] = b"67890";
    const LEGITIMATE_TARGET_EXIT_CODE: i32 = 127;
    const UNUSABLE_PROBE_EXIT_CODE: i32 = 1;
    const REQUEST_TIMEOUT_SECONDS: u64 = 1;
    const SLOW_PROBE_DELAY: Duration = Duration::from_millis(1_100);
    const TEST_SANDBOX_LAUNCHER: &str = "/fixture/signalbox-exec-supervisor";

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_supervision_failure_remains_distinct() {
        let outcome = supervisor_outcome(SupervisorStatus::SupervisionFailed {
            stage: SupervisorFailureStage::Cleanup,
        });

        assert_eq!(
            outcome,
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            }
        );
    }

    struct ReplacementWorkspace {
        path: PathBuf,
        retired: PathBuf,
    }

    #[cfg(target_os = "linux")]
    struct ProbeShellFixture {
        root: PathBuf,
        search_path: OsString,
        expected_shell: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl ProbeShellFixture {
        fn new() -> Result<Self, Box<dyn Error>> {
            let identity = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "signalbox-exec-probe-shell-{}-{identity}",
                std::process::id()
            ));
            let blocked_directory = root.join("blocked");
            let executable_directory = root.join("executable");
            std::fs::create_dir_all(&blocked_directory)?;
            std::fs::create_dir_all(&executable_directory)?;
            let blocked_shell = blocked_directory.join("sh");
            let expected_shell = executable_directory.join("sh");
            std::fs::write(&blocked_shell, b"blocked")?;
            std::fs::write(&expected_shell, b"executable")?;
            std::fs::set_permissions(&blocked_shell, std::fs::Permissions::from_mode(0o600))?;
            std::fs::set_permissions(&expected_shell, std::fs::Permissions::from_mode(0o700))?;
            let search_path = std::env::join_paths([blocked_directory, executable_directory])?;
            Ok(Self {
                root,
                search_path,
                expected_shell,
            })
        }

        fn search_path(&self) -> &std::ffi::OsStr {
            &self.search_path
        }

        fn expected_shell(&self) -> &Path {
            &self.expected_shell
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ProbeShellFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl ReplacementWorkspace {
        fn new() -> Result<Self, std::io::Error> {
            let identity = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(std::io::Error::other)?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "signalbox-exec-workspace-{}-{identity}",
                std::process::id()
            ));
            let retired = path.with_extension("retired");
            std::fs::create_dir(&path)?;
            Ok(Self { path, retired })
        }

        fn replace(&self) -> Result<(), std::io::Error> {
            std::fs::rename(&self.path, &self.retired)?;
            std::fs::create_dir(&self.path)
        }
    }

    impl Drop for ReplacementWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
            let _ = std::fs::remove_dir_all(&self.retired);
        }
    }

    #[derive(Clone, Debug)]
    struct FakeRunner {
        availability: BwrapAvailability,
        probe_delay: Duration,
        results: Arc<Mutex<Vec<ProcessRunResult>>>,
        probes: Arc<Mutex<Vec<ProcessRequest>>>,
        requests: Arc<Mutex<Vec<ProcessRequest>>>,
    }

    impl FakeRunner {
        fn returning(availability: BwrapAvailability, result: ProcessRunResult) -> Self {
            Self {
                availability,
                probe_delay: Duration::ZERO,
                results: Arc::new(Mutex::new(vec![result])),
                probes: Arc::new(Mutex::new(Vec::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn recorded_requests(&self) -> Vec<ProcessRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn recorded_probes(&self) -> Vec<ProcessRequest> {
            self.probes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn with_probe_delay(mut self, probe_delay: Duration) -> Self {
            self.probe_delay = probe_delay;
            self
        }
    }

    impl ProcessRunner for FakeRunner {
        fn sandbox_launcher_program(&self) -> &Path {
            Path::new(TEST_SANDBOX_LAUNCHER)
        }

        async fn bwrap_availability(&mut self, probe: ProcessRequest) -> BwrapAvailability {
            self.probes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(probe);
            tokio::time::sleep(self.probe_delay).await;
            self.availability
        }

        async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request);
            self.results
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(0)
        }
    }

    fn successful_process(stdout: &[u8]) -> ProcessRunResult {
        ProcessRunResult {
            outcome: ProcessOutcome::Exited { code: Some(0) },
            stdout: ProcessOutput {
                bytes: stdout.to_vec(),
                completeness: CaptureCompleteness::Complete,
            },
            stderr: ProcessOutput {
                bytes: Vec::new(),
                completeness: CaptureCompleteness::Complete,
            },
        }
    }

    fn successful_sandbox_process(stdout: &[u8]) -> ProcessRunResult {
        let mut result = successful_process(stdout);
        result.stderr.bytes = SANDBOX_DISPATCH_MARKER.to_vec();
        result
    }

    #[test]
    fn catalogs_fix_sandboxed_auto_and_unsandboxed_always_confirm_permissions()
    -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let sandboxed = SandboxedExecTool::try_new(
            FakeRunner::returning(BwrapAvailability::Available, successful_process(b"")),
            &root,
        )?;
        let unsandboxed = UnsandboxedExecTool::try_new(
            FakeRunner::returning(BwrapAvailability::Available, successful_process(b"")),
            root,
        )?;
        let sandboxed_name = signalbox_domain::ToolName::try_new(String::from(SANDBOXED_EXEC_NAME))
            .map_err(|_| std::io::Error::other("static sandboxed name"))?;
        let unsandboxed_name =
            signalbox_domain::ToolName::try_new(String::from(UNSANDBOXED_EXEC_NAME))
                .map_err(|_| std::io::Error::other("static unsandboxed name"))?;
        let sandboxed_catalog = sandboxed.into_parts().0;
        let unsandboxed_catalog = unsandboxed.into_parts().0;
        let sandboxed_definition = sandboxed_catalog
            .definition(&sandboxed_name)
            .ok_or_else(|| std::io::Error::other("sandboxed definition must be present"))?;
        let unsandboxed_definition = unsandboxed_catalog
            .definition(&unsandboxed_name)
            .ok_or_else(|| std::io::Error::other("unsandboxed definition must be present"))?;

        assert_eq!(
            sandboxed_definition.permission_default(),
            ToolPermissionDefault::Auto
        );
        assert_eq!(
            unsandboxed_definition.permission_default(),
            ToolPermissionDefault::AlwaysConfirm
        );
        Ok(())
    }

    #[test]
    fn production_probe_classifies_missing_timeout_and_unusable_evidence() {
        let missing = ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::NotFound,
        };

        assert_eq!(
            classify_bwrap_availability(missing),
            BwrapAvailability::Missing
        );
        assert_eq!(
            classify_bwrap_availability(ProcessOutcome::TimedOut),
            BwrapAvailability::TimedOut
        );
        assert_eq!(
            classify_bwrap_availability(ProcessOutcome::Exited {
                code: Some(UNUSABLE_PROBE_EXIT_CODE),
            }),
            BwrapAvailability::Unusable
        );
    }

    #[test]
    fn target_profile_setup_timeout_preserves_timeout_evidence() {
        let result = sandbox_process_result(
            ProcessRunResult {
                outcome: ProcessOutcome::TimedOut,
                stdout: ProcessOutput {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::Complete,
                },
                stderr: ProcessOutput {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::Complete,
                },
            },
            EXEC_CAPTURE_BYTES,
        );

        assert_eq!(result.confinement, ExecutionConfinement::SandboxSetupFailed);
        assert_eq!(result.outcome, ProcessOutcome::TimedOut);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_esrch_is_process_absence_evidence() {
        let error = std::io::Error::from_raw_os_error(rustix::io::Errno::SRCH.raw_os_error());

        assert!(outer_process_gone(&error));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_shell_skips_a_non_executable_regular_file() -> Result<(), Box<dyn Error>> {
        let fixture = ProbeShellFixture::new()?;

        let selected = executable_sandbox_shell(fixture.search_path())
            .ok_or_else(|| std::io::Error::other("one executable probe shell"))?;

        assert_eq!(selected, fixture.expected_shell());
        Ok(())
    }

    #[test]
    fn argument_schema_publishes_item_and_aggregate_safe_count_limits() -> Result<(), Box<dyn Error>>
    {
        let schema = serde_json::to_value(schemars::schema_for!(ExecArguments))?;

        assert_eq!(
            schema.pointer("/properties/arguments/items/maxLength"),
            Some(&serde_json::json!(MAX_ARGUMENT_CHARACTERS))
        );
        assert_eq!(
            schema.pointer("/properties/arguments/maxItems"),
            Some(&serde_json::json!(MAX_ARGUMENTS))
        );
        Ok(())
    }

    #[test]
    fn validator_rejects_parent_working_directory_before_execution() -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let tool = SandboxedExecTool::try_new(
            FakeRunner::returning(BwrapAvailability::Available, successful_process(b"")),
            root,
        )?;
        let (catalog, _) = tool.into_parts();
        let name = signalbox_domain::ToolName::try_new(String::from(SANDBOXED_EXEC_NAME))
            .map_err(|_| std::io::Error::other("static sandboxed name"))?;
        let arguments = NormalizedToolArguments::try_from_provider_text(String::from(
            r#"{"program":"cargo","working_directory":"../outside"}"#,
        ))
        .map_err(|_| std::io::Error::other("bounded arguments"))?;
        let detail = ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
            .map_err(|_| std::io::Error::other("static detail"))?;

        assert_eq!(
            catalog.validate_arguments(&name, &arguments),
            Err(ToolCatalogValidationFailure::InvalidArguments {
                detail: Some(detail),
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_bwrap_refuses_without_running_requested_command() -> Result<(), Box<dyn Error>>
    {
        let root = std::env::current_dir()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Missing,
            successful_process(b"must remain unused"),
        );
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, root)?;
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(
            result.confinement,
            ExecutionConfinement::SandboxRefused {
                availability: BwrapAvailability::Missing,
            }
        );
        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::SandboxUnavailable,
            }
        );
        assert_eq!(observation.recorded_requests(), Vec::new());
        Ok(())
    }

    #[tokio::test]
    async fn sandbox_probe_timeout_remains_timeout_evidence() -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::TimedOut,
            successful_process(b"must remain unused"),
        );
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, root)?;
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(result.confinement, ExecutionConfinement::SandboxSetupFailed);
        assert_eq!(result.outcome, ProcessOutcome::TimedOut);
        assert_eq!(observation.recorded_requests(), Vec::new());
        Ok(())
    }

    #[tokio::test]
    async fn process_tree_unsupported_remains_typed_without_terminal_text()
    -> Result<(), Box<dyn Error>> {
        let expected_outcome = ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::ProcessTreeUnsupported,
        };
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            ProcessRunResult {
                outcome: expected_outcome,
                stdout: ProcessOutput {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::Complete,
                },
                stderr: ProcessOutput {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::Complete,
                },
            },
        );
        let mut command_runner =
            UnsandboxedCommandRunner::try_new(runner, std::env::current_dir()?)?;
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(result.outcome, expected_outcome);
        assert_eq!(result.stdout.text, String::new());
        assert_eq!(result.stderr.text, String::new());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unsandboxed_runner_uses_pinned_workspace_after_path_replacement()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_process(b"complete"),
        );
        let observation = runner.clone();
        let mut command_runner = UnsandboxedCommandRunner::try_new(runner, &workspace.path)?;
        workspace.replace()?;
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;
        let requests = observation.recorded_requests();
        let request = requests
            .first()
            .ok_or_else(|| std::io::Error::other("one direct process request"))?;

        assert_eq!(result.confinement, ExecutionConfinement::Unsandboxed);
        assert!(
            request
                .working_directory
                .starts_with(Path::new("/proc/self/fd"))
        );
        assert_ne!(request.working_directory, workspace.path);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unsandboxed_runner_rejects_a_symlinked_working_directory_before_dispatch()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let outside = ReplacementWorkspace::new()?;
        symlink(&outside.path, workspace.path.join("escape"))?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_process(b"must remain unused"),
        );
        let observation = runner.clone();
        let mut command_runner = UnsandboxedCommandRunner::try_new(runner, &workspace.path)?;
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from("escape"),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(result.confinement, ExecutionConfinement::Unsandboxed);
        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::Other,
            }
        );
        assert_eq!(observation.recorded_requests(), Vec::new());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_request_uses_bwrap_profile_and_workspace_mount() -> Result<(), Box<dyn Error>>
    {
        let root = std::env::current_dir()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, &root)?;
        let bind_source = command_runner.workspace_identity.bind_source.clone();
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from(SANDBOXED_WORKING_DIRECTORY),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;
        let requests = observation.recorded_requests();
        let probes = observation.recorded_probes();
        let request = requests
            .first()
            .ok_or_else(|| std::io::Error::other("one requested process"))?;
        let probe = probes
            .first()
            .ok_or_else(|| std::io::Error::other("one sandbox probe"))?;
        let bind_arguments = [
            OsString::from("--bind"),
            bind_source.into_os_string(),
            OsString::from(SANDBOX_WORKSPACE),
        ];
        let chdir_arguments = [
            OsString::from("--chdir"),
            OsString::from(format!("{SANDBOX_WORKSPACE}/{SANDBOXED_WORKING_DIRECTORY}")),
        ];
        let launcher_arguments = [
            OsString::from("--ro-bind"),
            OsString::from(TEST_SANDBOX_LAUNCHER),
            OsString::from(SANDBOX_DISPATCH_PROGRAM),
        ];
        let dispatch_arguments = [
            OsString::from("--"),
            OsString::from(SANDBOX_DISPATCH_PROGRAM),
            OsString::from("--dispatch"),
            OsString::from("cargo"),
            OsString::from("check"),
        ];

        assert_eq!(request.program, OsString::from(BWRAP_PROGRAM));
        assert_eq!(probe.program, OsString::from(BWRAP_PROGRAM));
        assert_eq!(
            request.capture_bytes,
            EXEC_CAPTURE_BYTES + SANDBOX_DISPATCH_MARKER.len()
        );
        assert!(
            probe
                .arguments
                .contains(&sandbox_shell(&root).into_os_string())
        );
        assert_eq!(request.environment_inheritance, ProcessEnvironment::Clear);
        assert_eq!(
            request.environment.get(&OsString::from("PATH")),
            Some(&sandbox_path(&root))
        );
        assert!(
            request
                .arguments
                .windows(bind_arguments.len())
                .any(|arguments| arguments == bind_arguments)
        );
        assert!(
            request
                .arguments
                .windows(chdir_arguments.len())
                .any(|arguments| arguments == chdir_arguments)
        );
        assert!(
            request
                .arguments
                .windows(launcher_arguments.len())
                .any(|arguments| arguments == launcher_arguments)
        );
        assert!(request.arguments.ends_with(&dispatch_arguments));
        assert_eq!(result.stdout.text, SANDBOXED_STDOUT);
        Ok(())
    }

    #[tokio::test]
    async fn sandbox_probe_consumes_the_same_request_deadline() -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        )
        .with_probe_delay(SLOW_PROBE_DELAY);
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, root)?;
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from("."),
            timeout_seconds: REQUEST_TIMEOUT_SECONDS,
        };

        let result = command_runner.execute(arguments).await;
        let probes = observation.recorded_probes();
        let probe = probes
            .first()
            .ok_or_else(|| std::io::Error::other("one sandbox probe"))?;

        assert_eq!(probe.timeout, Duration::from_secs(REQUEST_TIMEOUT_SECONDS));
        assert_eq!(result.confinement, ExecutionConfinement::SandboxSetupFailed);
        assert_eq!(result.outcome, ProcessOutcome::TimedOut);
        assert!(observation.recorded_requests().is_empty());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_runner_refuses_replaced_workspace_before_probe() -> Result<(), Box<dyn Error>>
    {
        let workspace = ReplacementWorkspace::new()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, &workspace.path)?;
        workspace.replace()?;
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(result.confinement, ExecutionConfinement::SandboxSetupFailed);
        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::SandboxSetup,
            }
        );
        assert_eq!(observation.recorded_probes(), Vec::new());
        assert_eq!(observation.recorded_requests(), Vec::new());
        Ok(())
    }

    #[tokio::test]
    async fn sandbox_wrapper_failure_is_typed_without_claiming_confinement()
    -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            ProcessRunResult {
                outcome: ProcessOutcome::Exited { code: Some(127) },
                stdout: ProcessOutput {
                    bytes: Vec::new(),
                    completeness: CaptureCompleteness::Complete,
                },
                stderr: ProcessOutput {
                    bytes: b"target missing".to_vec(),
                    completeness: CaptureCompleteness::Complete,
                },
            },
        );
        let mut command_runner = SandboxedCommandRunner::try_new(runner, root)?;
        let arguments = ExecArguments {
            program: String::from("missing-target"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(result.confinement, ExecutionConfinement::SandboxSetupFailed);
        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::SandboxSetup,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn sandbox_setup_output_excludes_the_dispatch_marker_reserve()
    -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            ProcessRunResult {
                outcome: ProcessOutcome::Exited { code: Some(1) },
                stdout: ProcessOutput {
                    bytes: SETUP_STDOUT.to_vec(),
                    completeness: CaptureCompleteness::Complete,
                },
                stderr: ProcessOutput {
                    bytes: SETUP_STDERR.to_vec(),
                    completeness: CaptureCompleteness::Complete,
                },
            },
        );
        let mut command_runner = SandboxedCommandRunner::try_new(runner, root)?;
        let arguments = ExecArguments {
            program: String::from("missing-target"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner
            .run_with_capture(arguments, SETUP_CAPTURE_BYTES)
            .await;

        assert_eq!(
            result.stdout.text.as_bytes(),
            &SETUP_STDOUT[..SETUP_CAPTURE_BYTES]
        );
        assert_eq!(
            result.stderr.text.as_bytes(),
            &SETUP_STDERR[..SETUP_CAPTURE_BYTES]
        );
        assert_eq!(result.stdout.completeness, CaptureCompleteness::Truncated);
        assert_eq!(result.stderr.completeness, CaptureCompleteness::Truncated);
        Ok(())
    }

    #[tokio::test]
    async fn sandboxed_target_may_legitimately_exit_127() -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let mut process = successful_sandbox_process(b"");
        process.outcome = ProcessOutcome::Exited {
            code: Some(LEGITIMATE_TARGET_EXIT_CODE),
        };
        let runner = FakeRunner::returning(BwrapAvailability::Available, process);
        let mut command_runner = SandboxedCommandRunner::try_new(runner, root)?;
        let arguments = ExecArguments {
            program: String::from("target"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(result.confinement, ExecutionConfinement::FilesystemConfined);
        assert_eq!(
            result.outcome,
            ProcessOutcome::Exited {
                code: Some(LEGITIMATE_TARGET_EXIT_CODE),
            }
        );
        Ok(())
    }
}

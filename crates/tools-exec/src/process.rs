//! `SandboxedExecTool` and `UnsandboxedExecTool`, and the
//! `SandboxedCommandRunner` / `UnsandboxedCommandRunner` process runners that
//! back them.
//!
//! On Linux the sandboxed runner shells out to bubblewrap and the compiled
//! `signalbox-exec-supervisor` binary, decoding its `supervisor_protocol`
//! status for exit code, timeout, cancellation, and stdout/stderr capture
//! completeness.

#[cfg(target_os = "linux")]
use std::os::{fd::AsFd, unix::fs::MetadataExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsString,
    fmt,
    future::Future,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
#[cfg(target_os = "linux")]
use std::{
    process::Stdio,
    sync::{
        Mutex, PoisonError,
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
    LAUNCH_STATUS_TAIL_BYTES, LAUNCH_STATUS_TRAILER, LauncherStatus, SupervisorCaptureCompleteness,
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
const PROCESS_CAPTURE_BYTES_LIMIT: usize = 1024 * 1024;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded direct-command arguments";
pub(crate) const BWRAP_PROGRAM: &str = "/usr/bin/bwrap";
const SANDBOX_WORKSPACE: &str = "/workspace";
const SANDBOX_DISPATCH_PROGRAM: &str = "/signalbox-exec-dispatch";
const SANDBOX_HTTPS_BROKER_DIRECTORY: &str = "/run/signalbox";
const SANDBOX_HTTPS_BROKER_SOCKET: &str = "/run/signalbox/https-broker.sock";
const SANDBOX_HTTPS_PROXY: &str = "http://127.0.0.1:18080";
const SANDBOX_FALLBACK_PATH: &str = "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub(crate) const SANDBOX_DISPATCH_MARKER: &[u8] = b"signalbox-exec:dispatched\n";
#[cfg(target_os = "linux")]
const SUPERVISOR_STATUS_TRAILER: &[u8] = b"\n\0signalbox-exec-supervisor-status:";
#[cfg(target_os = "linux")]
const SUPERVISOR_STATUS_TAIL_BYTES: usize = LAUNCH_STATUS_TAIL_BYTES + 1024;
#[cfg(target_os = "linux")]
const SUPERVISOR_OUTER_MODE: &str = "--outer";
#[cfg(target_os = "linux")]
const OUTER_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(2);
#[cfg(target_os = "linux")]
const OUTER_PROCESS_CLEANUP_DEADLINE: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const OUTER_PROCESS_GRACEFUL_WAIT: Duration = Duration::from_millis(500);

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
    const DESCRIPTION: &'static str = "Runs one bounded direct command in a bwrap-confined injected workspace whose network namespace holds only a loopback interface.";
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
    /// A configured runner read-only path was absent, mutable by identity, or invalid.
    ReadOnlyPath {
        /// Supplied path associated with the failure.
        path: PathBuf,
        /// Underlying filesystem failure, when one occurred.
        source: Option<std::io::Error>,
    },
    /// A per-dispatch HTTPS broker endpoint was not one exact Unix socket.
    HttpsBrokerSocket {
        /// Supplied socket path associated with the failure.
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
    /// The injected bubblewrap program was not an absolute canonical file.
    BubblewrapProgram {
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
            Self::ReadOnlyPath { path, .. } => {
                write!(
                    formatter,
                    "exec read-only path `{}` is invalid",
                    path.display()
                )
            }
            Self::HttpsBrokerSocket { path, .. } => {
                write!(
                    formatter,
                    "exec HTTPS broker socket `{}` is invalid",
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
            Self::BubblewrapProgram { path, .. } => {
                write!(
                    formatter,
                    "exec bubblewrap program `{}` is invalid",
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
            | Self::ReadOnlyPath {
                source: Some(source),
                ..
            }
            | Self::HttpsBrokerSocket {
                source: Some(source),
                ..
            }
            | Self::SupervisorProgram {
                source: Some(source),
                ..
            }
            | Self::BubblewrapProgram {
                source: Some(source),
                ..
            } => Some(source),
            Self::Name
            | Self::Schema
            | Self::ErrorDetail
            | Self::Duplicate
            | Self::WorkspaceRoot { source: None, .. }
            | Self::ReadOnlyPath { source: None, .. }
            | Self::HttpsBrokerSocket { source: None, .. }
            | Self::SupervisorProgram { source: None, .. }
            | Self::BubblewrapProgram { source: None, .. } => None,
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
            build_tool::<SandboxedExecContract, _>(command_runner, ToolPermissionDefault::Confirm)?;
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

    /// Builds the production sandboxed tool around one configured bubblewrap executable.
    pub fn try_new_production_with_bubblewrap(
        workspace_root: impl AsRef<Path>,
        supervisor_program: impl AsRef<Path>,
        bubblewrap_program: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        Self::try_new(
            TokioProcessRunner::try_new_with_bubblewrap(supervisor_program, bubblewrap_program)?,
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
    /// Per-stream retained byte limit, rejected above one MiB before spawn.
    pub capture_bytes: usize,
    /// Exact environment additions or overrides.
    pub environment: BTreeMap<OsString, OsString>,
    /// Whether the ambient parent environment remains visible.
    pub environment_inheritance: ProcessEnvironment,
    /// Trusted status protocol expected from the supervised target.
    pub status_protocol: ProcessStatusProtocol,
}

/// Ambient-environment posture for an injected process request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessEnvironment {
    /// Preserve the parent environment before applying explicit entries.
    Inherit,
    /// Clear the parent environment before applying explicit entries.
    Clear,
}

/// Trusted status protocol carried by one process request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStatusProtocol {
    /// Interpret only the outer supervisor's status.
    Direct,
    /// Require and interpret the sandbox dispatcher's nested launcher status.
    SandboxDispatch,
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

    /// Inherited descriptor for the exact sandbox launcher, when supported.
    fn sandbox_launcher_descriptor(&self) -> Option<i32>;

    /// Exact bubblewrap executable used for sandbox profile probes and runs.
    fn bubblewrap_program(&self) -> &Path {
        Path::new(BWRAP_PROGRAM)
    }

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
    bubblewrap_program: PathBuf,
    #[cfg(target_os = "linux")]
    _supervisor: Arc<rustix::fd::OwnedFd>,
    #[cfg(target_os = "linux")]
    _bubblewrap: Option<Arc<rustix::fd::OwnedFd>>,
    #[cfg(target_os = "linux")]
    sandbox_launcher: Arc<rustix::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
fn inherited_descriptor_above_standard_streams(
    descriptor: rustix::fd::OwnedFd,
) -> Result<rustix::fd::OwnedFd, rustix::io::Errno> {
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
        Self::try_new_inner(supervisor_program.as_ref(), None)
    }

    /// Pins the separately packaged supervisor and one configured bubblewrap executable.
    pub fn try_new_with_bubblewrap(
        supervisor_program: impl AsRef<Path>,
        bubblewrap_program: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        Self::try_new_inner(
            supervisor_program.as_ref(),
            Some(bubblewrap_program.as_ref()),
        )
    }

    fn try_new_inner(
        supervisor_program: &Path,
        bubblewrap_program: Option<&Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        let supplied = supervisor_program;
        if !supplied.is_absolute() {
            return Err(ExecToolConstructionError::SupervisorProgram {
                path: supplied.to_owned(),
                source: None,
            });
        }
        #[cfg(target_os = "linux")]
        let (supervisor_program, _supervisor, sandbox_launcher, bubblewrap_program, _bubblewrap) = {
            let supervisor = rustix::fs::open(
                supplied,
                rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .map_err(|source| ExecToolConstructionError::SupervisorProgram {
                path: supplied.to_owned(),
                source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
            })?;
            let (supervisor_program, supervisor) =
                pin_executable_program(supplied, supervisor, supervisor_program_error)?;
            let sandbox_launcher = inherited_descriptor_above_standard_streams(
                rustix::io::dup(supervisor.as_ref()).map_err(|source| {
                    ExecToolConstructionError::SupervisorProgram {
                        path: supplied.to_owned(),
                        source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                    }
                })?,
            )
            .map_err(|source| ExecToolConstructionError::SupervisorProgram {
                path: supplied.to_owned(),
                source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
            })?;
            let (bubblewrap_program, bubblewrap) = match bubblewrap_program {
                Some(bubblewrap) => {
                    if !bubblewrap.is_absolute() {
                        return Err(bubblewrap_program_error(bubblewrap.to_owned(), None));
                    }
                    let descriptor = rustix::fs::open(
                        bubblewrap,
                        rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW,
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(|source| {
                        bubblewrap_program_error(
                            bubblewrap.to_owned(),
                            Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                        )
                    })?;
                    let (program, descriptor) =
                        pin_executable_program(bubblewrap, descriptor, bubblewrap_program_error)?;
                    (program, Some(descriptor))
                }
                None => (PathBuf::from(BWRAP_PROGRAM), None),
            };
            (
                supervisor_program,
                supervisor,
                Arc::new(sandbox_launcher),
                bubblewrap_program,
                bubblewrap,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let (supervisor_program, bubblewrap_program) = {
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
            let bubblewrap = match bubblewrap_program {
                Some(bubblewrap) => canonical_program(bubblewrap, bubblewrap_program_error)?,
                None => PathBuf::from(BWRAP_PROGRAM),
            };
            (canonical, bubblewrap)
        };
        Ok(Self {
            supervisor_program,
            bubblewrap_program,
            #[cfg(target_os = "linux")]
            _supervisor,
            #[cfg(target_os = "linux")]
            _bubblewrap,
            #[cfg(target_os = "linux")]
            sandbox_launcher,
        })
    }
}

#[cfg(target_os = "linux")]
fn pin_executable_program(
    supplied_path: &Path,
    descriptor: rustix::fd::OwnedFd,
    program_error: fn(PathBuf, Option<std::io::Error>) -> ExecToolConstructionError,
) -> Result<(PathBuf, Arc<rustix::fd::OwnedFd>), ExecToolConstructionError> {
    let descriptor = inherited_descriptor_above_standard_streams(descriptor).map_err(|source| {
        program_error(
            supplied_path.to_owned(),
            Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
        )
    })?;
    let metadata = rustix::fs::fstat(&descriptor).map_err(|source| {
        program_error(
            supplied_path.to_owned(),
            Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
        )
    })?;
    if rustix::fs::FileType::from_raw_mode(metadata.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(program_error(supplied_path.to_owned(), None));
    }
    let pinned_program = PathBuf::from(format!(
        "/proc/self/fd/{}",
        rustix::fd::AsRawFd::as_raw_fd(&descriptor)
    ));
    rustix::fs::accessat(
        rustix::fs::CWD,
        &pinned_program,
        rustix::fs::Access::EXEC_OK,
        rustix::fs::AtFlags::EACCESS,
    )
    .map_err(|source| {
        program_error(
            supplied_path.to_owned(),
            Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
        )
    })?;
    let canonical = pinned_program
        .canonicalize()
        .map_err(|source| program_error(supplied_path.to_owned(), Some(source)))?;
    if !canonical.is_absolute() {
        return Err(program_error(supplied_path.to_owned(), None));
    }
    Ok((pinned_program, Arc::new(descriptor)))
}

fn supervisor_program_error(
    path: PathBuf,
    source: Option<std::io::Error>,
) -> ExecToolConstructionError {
    ExecToolConstructionError::SupervisorProgram { path, source }
}

fn bubblewrap_program_error(
    path: PathBuf,
    source: Option<std::io::Error>,
) -> ExecToolConstructionError {
    ExecToolConstructionError::BubblewrapProgram { path, source }
}

#[cfg(not(target_os = "linux"))]
fn canonical_program(
    supplied: &Path,
    program_error: fn(PathBuf, Option<std::io::Error>) -> ExecToolConstructionError,
) -> Result<PathBuf, ExecToolConstructionError> {
    if !supplied.is_absolute() {
        return Err(program_error(supplied.to_owned(), None));
    }
    let canonical = supplied
        .canonicalize()
        .map_err(|source| program_error(supplied.to_owned(), Some(source)))?;
    if !canonical.is_absolute() || !canonical.is_file() {
        return Err(program_error(supplied.to_owned(), None));
    }
    Ok(canonical)
}

impl ProcessRunner for TokioProcessRunner {
    fn sandbox_launcher_program(&self) -> &Path {
        &self.supervisor_program
    }

    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        #[cfg(target_os = "linux")]
        {
            Some(rustix::fd::AsRawFd::as_raw_fd(
                self.sandbox_launcher.as_ref(),
            ))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    fn bubblewrap_program(&self) -> &Path {
        &self.bubblewrap_program
    }

    async fn bwrap_availability(&mut self, probe: ProcessRequest) -> BwrapAvailability {
        let result = run_process(&self.supervisor_program, probe).await;
        classify_bwrap_availability(&result)
    }

    async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
        run_process(&self.supervisor_program, request).await
    }
}

fn classify_bwrap_availability(result: &ProcessRunResult) -> BwrapAvailability {
    match &result.outcome {
        ProcessOutcome::Exited { code: Some(0) } => BwrapAvailability::Available,
        ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::NotFound,
        } if !result.stderr.bytes.starts_with(SANDBOX_DISPATCH_MARKER) => {
            BwrapAvailability::Missing
        }
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
    bubblewrap_program: PathBuf,
    mount_profile: Arc<SandboxMountProfile>,
    #[cfg(not(target_os = "linux"))]
    sandbox_launcher: PathBuf,
    #[cfg(target_os = "linux")]
    sandbox_launcher_descriptor: i32,
    #[cfg(target_os = "linux")]
    workspace_identity: WorkspaceIdentity,
}

#[derive(Clone, Debug)]
enum SandboxMountProfile {
    DaemonLocal,
    RunnerRestricted {
        paths: Vec<ReadOnlyPathIdentity>,
        https_broker: Option<HttpsBrokerSocketIdentity>,
    },
}

impl SandboxMountProfile {
    fn identities_are_current(&self) -> bool {
        match self {
            Self::DaemonLocal => true,
            Self::RunnerRestricted {
                paths,
                https_broker,
            } => {
                paths.iter().all(ReadOnlyPathIdentity::matches)
                    && https_broker
                        .as_ref()
                        .is_none_or(HttpsBrokerSocketIdentity::matches)
            }
        }
    }

    fn executable_path(&self, workspace_root: &Path) -> OsString {
        match self {
            Self::DaemonLocal => sandbox_path(workspace_root),
            Self::RunnerRestricted { paths, .. } => configured_sandbox_path(workspace_root, paths),
        }
    }
}

impl<Runner: ProcessRunner> SandboxedCommandRunner<Runner> {
    /// Admits one canonical injected workspace root.
    pub fn try_new(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        Self::try_new_with_mount_profile(runner, workspace_root, SandboxMountProfile::DaemonLocal)
    }

    /// Admits one runner workspace and its exact configured read-only path set.
    ///
    /// This profile adds the cgroup namespace, drops every capability, creates
    /// fresh runtime directories, and exposes no read-only host path beyond the
    /// supplied identities. Network remains fully unshared; a later broker can
    /// add explicitly authorized egress without widening this filesystem set.
    pub fn try_new_runner_restricted(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
        read_only_paths: &[PathBuf],
    ) -> Result<Self, ExecToolConstructionError> {
        let read_only_paths = capture_read_only_paths(read_only_paths)?;
        Self::try_new_with_mount_profile(
            runner,
            workspace_root,
            SandboxMountProfile::RunnerRestricted {
                paths: read_only_paths,
                https_broker: None,
            },
        )
    }

    /// Admits the restricted profile plus one exact per-dispatch HTTPS broker
    /// Unix socket exposed only to the namespace-local proxy shim.
    pub fn try_new_runner_restricted_with_https_broker(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
        read_only_paths: &[PathBuf],
        https_broker_socket: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        let read_only_paths = capture_read_only_paths(read_only_paths)?;
        let https_broker = HttpsBrokerSocketIdentity::capture(https_broker_socket.as_ref())?;
        Self::try_new_with_mount_profile(
            runner,
            workspace_root,
            SandboxMountProfile::RunnerRestricted {
                paths: read_only_paths,
                https_broker: Some(https_broker),
            },
        )
    }

    fn try_new_with_mount_profile(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
        mount_profile: SandboxMountProfile,
    ) -> Result<Self, ExecToolConstructionError> {
        #[cfg(target_os = "linux")]
        let workspace_identity = WorkspaceIdentity::capture(workspace_root.as_ref())?;
        #[cfg(target_os = "linux")]
        let workspace_root = workspace_identity.canonical_path.clone();
        #[cfg(not(target_os = "linux"))]
        let workspace_root = canonical_workspace_root(workspace_root.as_ref())?;
        let sandbox_launcher = runner.sandbox_launcher_program().to_owned();
        let bubblewrap_program = runner.bubblewrap_program().to_owned();
        #[cfg(target_os = "linux")]
        let sandbox_launcher_descriptor = runner
            .sandbox_launcher_descriptor()
            .filter(|descriptor| *descriptor >= 3)
            .ok_or_else(|| ExecToolConstructionError::SupervisorProgram {
                path: sandbox_launcher.clone(),
                source: None,
            })?;
        Ok(Self {
            runner,
            #[cfg(not(target_os = "linux"))]
            sandbox_launcher,
            #[cfg(target_os = "linux")]
            sandbox_launcher_descriptor,
            #[cfg(target_os = "linux")]
            workspace_identity,
            workspace_root,
            bubblewrap_program,
            mount_profile: Arc::new(mount_profile),
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

    pub(crate) fn pinned_workspace_root(&self) -> &Path {
        #[cfg(target_os = "linux")]
        {
            &self.workspace_identity.bind_source
        }
        #[cfg(not(target_os = "linux"))]
        {
            &self.workspace_root
        }
    }

    pub(crate) async fn run_with_capture(
        &mut self,
        arguments: ExecArguments,
        capture_bytes: usize,
    ) -> ExecResult {
        let requested_timeout = Duration::from_secs(arguments.timeout_seconds);
        self.run_with_capture_timeout(arguments, requested_timeout, capture_bytes)
            .await
    }

    pub(crate) async fn run_with_capture_timeout(
        &mut self,
        arguments: ExecArguments,
        requested_timeout: Duration,
        capture_bytes: usize,
    ) -> ExecResult {
        let deadline = tokio::time::Instant::now() + requested_timeout;
        if !self.mount_profile.identities_are_current() {
            return ExecResult {
                confinement: ExecutionConfinement::SandboxSetupFailed,
                outcome: ProcessOutcome::SpawnFailed {
                    reason: ProcessSpawnFailure::SandboxSetup,
                },
                stdout: OutputCapture::empty(),
                stderr: OutputCapture::empty(),
            };
        }
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
        let sandbox_path = self.mount_profile.executable_path(&self.workspace_root);
        let probe_program = executable_sandbox_shell(&sandbox_path)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
            .to_string_lossy()
            .into_owned();
        let probe_timeout = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(Duration::from_secs(5));
        if probe_timeout.is_zero() {
            return ExecResult {
                confinement: ExecutionConfinement::SandboxSetupFailed,
                outcome: ProcessOutcome::TimedOut,
                stdout: OutputCapture::empty(),
                stderr: OutputCapture::empty(),
            };
        }
        let probe = bwrap_request(
            SandboxLaunchContext {
                #[cfg(target_os = "linux")]
                bind_source: &self.workspace_identity.bind_source,
                #[cfg(target_os = "linux")]
                bind_descriptor: rustix::fd::AsRawFd::as_raw_fd(
                    self.workspace_identity._directory.as_ref(),
                ),
                #[cfg(not(target_os = "linux"))]
                bind_source: &self.workspace_root,
                #[cfg(not(target_os = "linux"))]
                launcher: &self.sandbox_launcher,
                #[cfg(target_os = "linux")]
                launcher_descriptor: self.sandbox_launcher_descriptor,
                #[cfg(not(target_os = "linux"))]
                working_directory_bind_source: None,
                #[cfg(target_os = "linux")]
                working_directory_bind_descriptor: None,
            },
            &self.bubblewrap_program,
            SandboxInvocation {
                program: &probe_program,
                arguments: &[String::from("-c"), String::from("exit 0")],
                working_directory: ".",
                timeout: probe_timeout,
                capture_bytes: 8 * 1024,
            },
            SandboxRequestProfile {
                mounts: &self.mount_profile,
                executable_path: &sandbox_path,
            },
        );
        let availability = self.runner.bwrap_availability(probe).await;
        match availability {
            BwrapAvailability::Available => {
                if !self.mount_profile.identities_are_current() {
                    return ExecResult {
                        confinement: ExecutionConfinement::SandboxSetupFailed,
                        outcome: ProcessOutcome::SpawnFailed {
                            reason: ProcessSpawnFailure::SandboxSetup,
                        },
                        stdout: OutputCapture::empty(),
                        stderr: OutputCapture::empty(),
                    };
                }
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
                #[cfg(target_os = "linux")]
                let working_directory_identity = if arguments.working_directory == "." {
                    None
                } else {
                    match self
                        .workspace_identity
                        .pin_relative_directory(&arguments.working_directory)
                        .and_then(WorkspaceDirectoryIdentity::inherit)
                    {
                        Ok(directory) => Some(directory),
                        Err(reason) => {
                            return ExecResult {
                                confinement: ExecutionConfinement::SandboxSetupFailed,
                                outcome: ProcessOutcome::SpawnFailed { reason },
                                stdout: OutputCapture::empty(),
                                stderr: OutputCapture::empty(),
                            };
                        }
                    }
                };
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
                        #[cfg(target_os = "linux")]
                        bind_source: &self.workspace_identity.bind_source,
                        #[cfg(target_os = "linux")]
                        bind_descriptor: rustix::fd::AsRawFd::as_raw_fd(
                            self.workspace_identity._directory.as_ref(),
                        ),
                        #[cfg(not(target_os = "linux"))]
                        bind_source: &self.workspace_root,
                        #[cfg(not(target_os = "linux"))]
                        launcher: &self.sandbox_launcher,
                        #[cfg(target_os = "linux")]
                        launcher_descriptor: self.sandbox_launcher_descriptor,
                        #[cfg(target_os = "linux")]
                        working_directory_bind_descriptor: working_directory_identity
                            .as_ref()
                            .map(|directory| rustix::fd::AsRawFd::as_raw_fd(&directory._directory)),
                        #[cfg(not(target_os = "linux"))]
                        working_directory_bind_source: None,
                    },
                    &self.bubblewrap_program,
                    SandboxInvocation {
                        program: &arguments.program,
                        arguments: &arguments.arguments,
                        working_directory: &arguments.working_directory,
                        timeout: remaining,
                        capture_bytes,
                    },
                    SandboxRequestProfile {
                        mounts: &self.mount_profile,
                        executable_path: &sandbox_path,
                    },
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

#[derive(Clone, Debug)]
struct ReadOnlyPathIdentity {
    destination: PathBuf,
    bind_source: PathBuf,
    #[cfg(target_os = "linux")]
    device: u64,
    #[cfg(target_os = "linux")]
    inode: u64,
    #[cfg(target_os = "linux")]
    _descriptor: Arc<rustix::fd::OwnedFd>,
}

#[derive(Clone, Debug)]
struct HttpsBrokerSocketIdentity {
    bind_source: PathBuf,
    #[cfg(target_os = "linux")]
    device: u64,
    #[cfg(target_os = "linux")]
    inode: u64,
    #[cfg(target_os = "linux")]
    _descriptor: Arc<rustix::fd::OwnedFd>,
}

impl HttpsBrokerSocketIdentity {
    fn capture(path: &Path) -> Result<Self, ExecToolConstructionError> {
        #[cfg(not(target_os = "linux"))]
        {
            return Err(ExecToolConstructionError::HttpsBrokerSocket {
                path: path.to_owned(),
                source: None,
            });
        }
        #[cfg(target_os = "linux")]
        {
            if !path.is_absolute()
                || path
                    .symlink_metadata()
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(ExecToolConstructionError::HttpsBrokerSocket {
                    path: path.to_owned(),
                    source: None,
                });
            }
            let current_process_descriptors =
                PathBuf::from(format!("/proc/{}/fd", std::process::id()));
            let descriptor_relative_path = path
                .strip_prefix(&current_process_descriptors)
                .ok()
                .is_some_and(|relative| {
                    let mut components = relative.components();
                    let descriptor_is_numeric = components.next().is_some_and(|component| {
                        matches!(component, std::path::Component::Normal(descriptor)
                        if descriptor.to_str().is_some_and(|descriptor| {
                            !descriptor.is_empty()
                                && descriptor.bytes().all(|byte| byte.is_ascii_digit())
                        }))
                    });
                    descriptor_is_numeric
                        && components.next().is_some_and(|component| {
                            matches!(component, std::path::Component::Normal(_))
                        })
                        && components
                            .all(|component| matches!(component, std::path::Component::Normal(_)))
                });
            let bind_target = if descriptor_relative_path {
                path.to_owned()
            } else {
                let destination = path.canonicalize().map_err(|source| {
                    ExecToolConstructionError::HttpsBrokerSocket {
                        path: path.to_owned(),
                        source: Some(source),
                    }
                })?;
                if destination != path {
                    return Err(ExecToolConstructionError::HttpsBrokerSocket {
                        path: path.to_owned(),
                        source: None,
                    });
                }
                destination
            };
            let descriptor = rustix::fs::open(
                &bind_target,
                rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .map_err(|source| ExecToolConstructionError::HttpsBrokerSocket {
                path: path.to_owned(),
                source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
            })?;
            let descriptor =
                inherited_descriptor_above_standard_streams(descriptor).map_err(|source| {
                    ExecToolConstructionError::HttpsBrokerSocket {
                        path: path.to_owned(),
                        source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                    }
                })?;
            let metadata = rustix::fs::fstat(&descriptor).map_err(|source| {
                ExecToolConstructionError::HttpsBrokerSocket {
                    path: path.to_owned(),
                    source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                }
            })?;
            if rustix::fs::FileType::from_raw_mode(metadata.st_mode) != rustix::fs::FileType::Socket
            {
                return Err(ExecToolConstructionError::HttpsBrokerSocket {
                    path: path.to_owned(),
                    source: None,
                });
            }
            Ok(Self {
                bind_source: descriptor_path(rustix::fd::AsRawFd::as_raw_fd(&descriptor)),
                device: metadata.st_dev,
                inode: metadata.st_ino,
                _descriptor: Arc::new(descriptor),
            })
        }
    }

    fn matches(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            rustix::fs::fstat(self._descriptor.as_ref()).is_ok_and(|metadata| {
                rustix::fs::FileType::from_raw_mode(metadata.st_mode)
                    == rustix::fs::FileType::Socket
                    && metadata.st_dev == self.device
                    && metadata.st_ino == self.inode
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

impl ReadOnlyPathIdentity {
    fn capture(path: &Path) -> Result<Self, ExecToolConstructionError> {
        if !path.is_absolute()
            || path
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(ExecToolConstructionError::ReadOnlyPath {
                path: path.to_owned(),
                source: None,
            });
        }
        let destination =
            path.canonicalize()
                .map_err(|source| ExecToolConstructionError::ReadOnlyPath {
                    path: path.to_owned(),
                    source: Some(source),
                })?;
        if destination != path {
            return Err(ExecToolConstructionError::ReadOnlyPath {
                path: path.to_owned(),
                source: None,
            });
        }
        #[cfg(target_os = "linux")]
        {
            let descriptor = rustix::fs::open(
                &destination,
                rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .map_err(|source| ExecToolConstructionError::ReadOnlyPath {
                path: path.to_owned(),
                source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
            })?;
            let descriptor =
                inherited_descriptor_above_standard_streams(descriptor).map_err(|source| {
                    ExecToolConstructionError::ReadOnlyPath {
                        path: path.to_owned(),
                        source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                    }
                })?;
            let metadata = rustix::fs::fstat(&descriptor).map_err(|source| {
                ExecToolConstructionError::ReadOnlyPath {
                    path: path.to_owned(),
                    source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                }
            })?;
            let bind_source = descriptor_path(rustix::fd::AsRawFd::as_raw_fd(&descriptor));
            Ok(Self {
                destination,
                bind_source,
                device: metadata.st_dev,
                inode: metadata.st_ino,
                _descriptor: Arc::new(descriptor),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Self {
                bind_source: destination.clone(),
                destination,
            })
        }
    }

    fn matches(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.destination.symlink_metadata().is_ok_and(|metadata| {
                !metadata.file_type().is_symlink()
                    && metadata.dev() == self.device
                    && metadata.ino() == self.inode
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.destination
                .canonicalize()
                .is_ok_and(|path| path == self.destination)
        }
    }
}

fn capture_read_only_paths(
    paths: &[PathBuf],
) -> Result<Vec<ReadOnlyPathIdentity>, ExecToolConstructionError> {
    let invalid_path = paths.first().cloned().unwrap_or_default();
    if paths.is_empty() || paths.iter().collect::<BTreeSet<_>>().len() != paths.len() {
        return Err(ExecToolConstructionError::ReadOnlyPath {
            path: invalid_path,
            source: None,
        });
    }
    paths
        .iter()
        .map(|path| ReadOnlyPathIdentity::capture(path))
        .collect()
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct WorkspaceIdentity {
    device: u64,
    inode: u64,
    canonical_path: PathBuf,
    bind_source: PathBuf,
    _directory: Arc<rustix::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
impl WorkspaceIdentity {
    fn capture(path: &Path) -> Result<Self, ExecToolConstructionError> {
        if !path.is_absolute() {
            return Err(ExecToolConstructionError::WorkspaceRoot {
                path: path.to_owned(),
                source: None,
            });
        }
        let directory = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| ExecToolConstructionError::WorkspaceRoot {
            path: path.to_owned(),
            source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
        })?;
        Self::from_open_directory(path, directory)
    }

    fn from_open_directory(
        supplied_path: &Path,
        directory: rustix::fd::OwnedFd,
    ) -> Result<Self, ExecToolConstructionError> {
        let directory =
            inherited_descriptor_above_standard_streams(directory).map_err(|source| {
                ExecToolConstructionError::WorkspaceRoot {
                    path: supplied_path.to_owned(),
                    source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
                }
            })?;
        let pinned_metadata = rustix::fs::fstat(&directory).map_err(|source| {
            ExecToolConstructionError::WorkspaceRoot {
                path: supplied_path.to_owned(),
                source: Some(std::io::Error::from_raw_os_error(source.raw_os_error())),
            }
        })?;
        let bind_source = PathBuf::from(format!(
            "/proc/self/fd/{}",
            rustix::fd::AsRawFd::as_raw_fd(&directory)
        ));
        let canonical_path = bind_source.canonicalize().map_err(|source| {
            ExecToolConstructionError::WorkspaceRoot {
                path: supplied_path.to_owned(),
                source: Some(source),
            }
        })?;
        if !canonical_path.is_absolute() {
            return Err(ExecToolConstructionError::WorkspaceRoot {
                path: supplied_path.to_owned(),
                source: None,
            });
        }
        Ok(Self {
            device: pinned_metadata.st_dev,
            inode: pinned_metadata.st_ino,
            canonical_path,
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
impl WorkspaceDirectoryIdentity {
    fn inherit(mut self) -> Result<Self, ProcessSpawnFailure> {
        self._directory = inherited_descriptor_above_standard_streams(self._directory)
            .map_err(|_| ProcessSpawnFailure::Other)?;
        self.bind_source = PathBuf::from(format!(
            "/proc/self/fd/{}",
            rustix::fd::AsRawFd::as_raw_fd(&self._directory)
        ));
        Ok(self)
    }
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
        #[cfg(target_os = "linux")]
        let workspace_identity = WorkspaceIdentity::capture(workspace_root.as_ref())?;
        #[cfg(not(target_os = "linux"))]
        let workspace_root = canonical_workspace_root(workspace_root.as_ref())?;
        Ok(Self {
            runner,
            #[cfg(target_os = "linux")]
            workspace_identity,
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
            .and_then(WorkspaceDirectoryIdentity::inherit)
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

#[cfg(not(target_os = "linux"))]
fn canonical_workspace_root(root: &Path) -> Result<PathBuf, ExecToolConstructionError> {
    if !root.is_absolute() {
        return Err(ExecToolConstructionError::WorkspaceRoot {
            path: root.to_owned(),
            source: None,
        });
    }
    let supplied_metadata =
        root.symlink_metadata()
            .map_err(|source| ExecToolConstructionError::WorkspaceRoot {
                path: root.to_owned(),
                source: Some(source),
            })?;
    if supplied_metadata.file_type().is_symlink() {
        return Err(ExecToolConstructionError::WorkspaceRoot {
            path: root.to_owned(),
            source: None,
        });
    }
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
        status_protocol: ProcessStatusProtocol::Direct,
    }
}

#[derive(Clone, Copy)]
struct SandboxLaunchContext<'a> {
    bind_source: &'a Path,
    #[cfg(target_os = "linux")]
    bind_descriptor: i32,
    #[cfg(not(target_os = "linux"))]
    launcher: &'a Path,
    #[cfg(target_os = "linux")]
    launcher_descriptor: i32,
    #[cfg(not(target_os = "linux"))]
    working_directory_bind_source: Option<&'a Path>,
    #[cfg(target_os = "linux")]
    working_directory_bind_descriptor: Option<i32>,
}

#[derive(Clone, Copy)]
struct SandboxInvocation<'a> {
    program: &'a str,
    arguments: &'a [String],
    working_directory: &'a str,
    timeout: Duration,
    capture_bytes: usize,
}

#[derive(Clone, Copy)]
struct SandboxRequestProfile<'a> {
    mounts: &'a SandboxMountProfile,
    executable_path: &'a std::ffi::OsStr,
}

fn bwrap_request(
    context: SandboxLaunchContext<'_>,
    bubblewrap_program: &Path,
    invocation: SandboxInvocation<'_>,
    profile: SandboxRequestProfile<'_>,
) -> ProcessRequest {
    let sandbox_directory = if invocation.working_directory == "." {
        String::from(SANDBOX_WORKSPACE)
    } else {
        format!("{SANDBOX_WORKSPACE}/{}", invocation.working_directory)
    };
    // The leading flags are this profile's whole namespace isolation. A deletion
    // or reordering here fails
    // `sandboxed_request_opens_with_the_user_pid_ipc_uts_and_network_unshare_prefix`,
    // which restates the expected prefix independently.
    let mut bwrap_arguments = [
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    if matches!(profile.mounts, SandboxMountProfile::RunnerRestricted { .. }) {
        bwrap_arguments.push(OsString::from("--unshare-cgroup"));
    }
    bwrap_arguments.push(OsString::from("--unshare-net"));
    if matches!(profile.mounts, SandboxMountProfile::RunnerRestricted { .. }) {
        bwrap_arguments.extend([OsString::from("--cap-drop"), OsString::from("ALL")]);
    }
    bwrap_arguments.extend([
        OsString::from("--proc"),
        OsString::from("/proc"),
        OsString::from("--dev"),
        OsString::from("/dev"),
        OsString::from("--tmpfs"),
        OsString::from("/tmp"),
    ]);
    match profile.mounts {
        SandboxMountProfile::DaemonLocal => bwrap_arguments.extend(
            [
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
                "/etc/ssl",
                "/etc/ssl",
            ]
            .into_iter()
            .map(OsString::from),
        ),
        SandboxMountProfile::RunnerRestricted {
            paths,
            https_broker,
        } => {
            bwrap_arguments.extend([OsString::from("--tmpfs"), OsString::from("/run")]);
            append_read_only_parent_directories(&mut bwrap_arguments, paths);
            for path in paths {
                bwrap_arguments.extend([
                    OsString::from("--ro-bind"),
                    path.bind_source.as_os_str().to_owned(),
                    path.destination.as_os_str().to_owned(),
                ]);
            }
            append_usr_merge_aliases(&mut bwrap_arguments, paths);
            if let Some(https_broker) = https_broker {
                bwrap_arguments.extend([
                    OsString::from("--dir"),
                    OsString::from(SANDBOX_HTTPS_BROKER_DIRECTORY),
                    OsString::from("--ro-bind"),
                    https_broker.bind_source.as_os_str().to_owned(),
                    OsString::from(SANDBOX_HTTPS_BROKER_SOCKET),
                ]);
            }
        }
    }
    #[cfg(target_os = "linux")]
    bwrap_arguments.extend([
        OsString::from("--bind"),
        descriptor_path(context.bind_descriptor).into_os_string(),
        OsString::from(SANDBOX_WORKSPACE),
    ]);
    #[cfg(not(target_os = "linux"))]
    bwrap_arguments.extend([
        OsString::from("--bind"),
        context.bind_source.as_os_str().to_owned(),
        OsString::from(SANDBOX_WORKSPACE),
    ]);
    #[cfg(target_os = "linux")]
    if let Some(working_directory_bind_descriptor) = context.working_directory_bind_descriptor {
        bwrap_arguments.extend([
            OsString::from("--bind"),
            descriptor_path(working_directory_bind_descriptor).into_os_string(),
            OsString::from(&sandbox_directory),
        ]);
    }
    #[cfg(not(target_os = "linux"))]
    if let Some(working_directory_bind_source) = context.working_directory_bind_source {
        bwrap_arguments.extend([
            OsString::from("--bind"),
            working_directory_bind_source.as_os_str().to_owned(),
            OsString::from(&sandbox_directory),
        ]);
    }
    #[cfg(target_os = "linux")]
    bwrap_arguments.extend([
        OsString::from("--ro-bind"),
        descriptor_path(context.launcher_descriptor).into_os_string(),
        OsString::from(SANDBOX_DISPATCH_PROGRAM),
    ]);
    #[cfg(not(target_os = "linux"))]
    bwrap_arguments.extend([
        OsString::from("--ro-bind"),
        context.launcher.as_os_str().to_owned(),
        OsString::from(SANDBOX_DISPATCH_PROGRAM),
    ]);
    let https_broker = matches!(
        profile.mounts,
        SandboxMountProfile::RunnerRestricted {
            https_broker: Some(_),
            ..
        }
    );
    bwrap_arguments.extend([
        OsString::from("--chdir"),
        OsString::from(sandbox_directory),
        OsString::from("--setenv"),
        OsString::from("HOME"),
        OsString::from(SANDBOX_WORKSPACE),
    ]);
    if https_broker {
        bwrap_arguments.extend([
            OsString::from("--setenv"),
            OsString::from("HTTPS_PROXY"),
            OsString::from(SANDBOX_HTTPS_PROXY),
            OsString::from("--setenv"),
            OsString::from("https_proxy"),
            OsString::from(SANDBOX_HTTPS_PROXY),
        ]);
    }
    bwrap_arguments.extend([
        OsString::from("--"),
        OsString::from(SANDBOX_DISPATCH_PROGRAM),
        OsString::from(if https_broker {
            "--dispatch-with-https-proxy"
        } else {
            "--dispatch"
        }),
        OsString::from(invocation.program),
    ]);
    bwrap_arguments.extend(invocation.arguments.iter().map(OsString::from));
    ProcessRequest {
        program: bubblewrap_program.as_os_str().to_owned(),
        arguments: bwrap_arguments,
        working_directory: context.bind_source.to_owned(),
        timeout: invocation.timeout,
        capture_bytes: invocation
            .capture_bytes
            .saturating_add(SANDBOX_DISPATCH_MARKER.len()),
        environment: BTreeMap::from([
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
            (OsString::from("LC_ALL"), OsString::from("C.UTF-8")),
            (OsString::from("PATH"), profile.executable_path.to_owned()),
        ]),
        environment_inheritance: ProcessEnvironment::Clear,
        status_protocol: ProcessStatusProtocol::SandboxDispatch,
    }
}

fn append_read_only_parent_directories(
    arguments: &mut Vec<OsString>,
    read_only_paths: &[ReadOnlyPathIdentity],
) {
    let directories = read_only_paths
        .iter()
        .filter_map(|path| path.destination.parent())
        .flat_map(Path::ancestors)
        .filter(|path| *path != Path::new("/"))
        .collect::<BTreeSet<_>>();
    for directory in directories {
        arguments.extend([OsString::from("--dir"), directory.as_os_str().to_owned()]);
    }
}

fn append_usr_merge_aliases(
    arguments: &mut Vec<OsString>,
    read_only_paths: &[ReadOnlyPathIdentity],
) {
    for (destination, target, absolute_target) in [
        ("/bin", "usr/bin", "/usr/bin"),
        ("/sbin", "usr/sbin", "/usr/sbin"),
        ("/lib", "usr/lib", "/usr/lib"),
        ("/lib64", "usr/lib64", "/usr/lib64"),
    ] {
        let destination_is_mounted = read_only_paths
            .iter()
            .any(|path| path.destination == Path::new(destination));
        let target_is_visible = Path::new(absolute_target).exists()
            && read_only_paths
                .iter()
                .any(|path| Path::new(absolute_target).starts_with(&path.destination));
        if !destination_is_mounted && target_is_visible {
            arguments.extend([
                OsString::from("--symlink"),
                OsString::from(target),
                OsString::from(destination),
            ]);
        }
    }
}

#[cfg(target_os = "linux")]
fn descriptor_path(descriptor: i32) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{descriptor}"))
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

fn configured_sandbox_path(
    workspace_root: &Path,
    read_only_paths: &[ReadOnlyPathIdentity],
) -> OsString {
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
        let Some(canonical) = path.canonicalize().ok() else {
            continue;
        };
        let configured = read_only_paths
            .iter()
            .any(|read_only| canonical.starts_with(&read_only.destination));
        if configured
            && canonical.is_dir()
            && !canonical.starts_with(workspace_root)
            && seen.insert(canonical.clone())
        {
            components.push(canonical);
        }
    }
    std::env::join_paths(components).unwrap_or_default()
}

#[cfg(test)]
fn sandbox_shell(workspace_root: &Path) -> PathBuf {
    executable_sandbox_shell(&sandbox_path(workspace_root))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn executable_sandbox_shell(path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|directory| {
        let candidate = directory.join("sh");
        sandbox_program_is_executable(&candidate, &directory).then_some(candidate)
    })
}

fn sandbox_program_is_executable(candidate: &Path, visible_directory: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        candidate.canonicalize().is_ok_and(|resolved| {
            resolved.starts_with(visible_directory)
                && resolved.metadata().is_ok_and(|metadata| metadata.is_file())
        }) && rustix::fs::accessat(
            rustix::fs::CWD,
            candidate,
            rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        candidate
            .canonicalize()
            .is_ok_and(|resolved| resolved.starts_with(visible_directory) && resolved.is_file())
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
    let outcome = match result.outcome {
        ProcessOutcome::TimedOut => ProcessOutcome::TimedOut,
        ProcessOutcome::Exited { .. }
        | ProcessOutcome::SpawnFailed { .. }
        | ProcessOutcome::SupervisionFailed { .. } => ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::SandboxSetup,
        },
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
    let mut retained = Vec::new();
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
    status_protocol: ProcessStatusProtocol,
) -> std::io::Result<(BoundedBytes, SupervisorStatus, Option<LauncherStatus>)> {
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
    let launcher_trailer = match (status_protocol, status) {
        (ProcessStatusProtocol::SandboxDispatch, SupervisorStatus::SpawnFailed { .. }) => None,
        (ProcessStatusProtocol::SandboxDispatch, _) => {
            let (launcher_marker, launcher_status) = parse_launcher_status(&tail[..marker])
                .ok_or_else(|| {
                    std::io::Error::other("sandbox launcher status trailer is malformed")
                })?;
            Some((launcher_marker, launcher_status))
        }
        (ProcessStatusProtocol::Direct, _) => None,
    };
    let first_trailer = launcher_trailer
        .map(|(launcher_marker, _)| launcher_marker)
        .unwrap_or(marker);
    let trailer_bytes = tail.len() - first_trailer;
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
        match status {
            SupervisorStatus::Exited { code: Some(0), .. } => {
                launcher_trailer.map(|(_, status)| status)
            }
            SupervisorStatus::Exited { .. }
            | SupervisorStatus::TimedOut
            | SupervisorStatus::Cancelled
            | SupervisorStatus::SpawnFailed { .. }
            | SupervisorStatus::SupervisionFailed { .. } => None,
        },
    ))
}

#[cfg(target_os = "linux")]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OuterObservationStatus {
    Complete,
    Interrupted,
}

#[cfg(target_os = "linux")]
impl OuterProcessTreeGuard {
    fn new(root: u32) -> Result<Self, ()> {
        Self::new_with_watcher(root, |watcher| {
            std::thread::Builder::new()
                .name(String::from("signalbox-exec-outer-watcher"))
                .spawn(watcher)
                .map_err(|_| ())
        })
    }

    fn new_with_watcher<Spawn>(root: u32, spawn: Spawn) -> Result<Self, ()>
    where
        Spawn: FnOnce(Box<dyn FnOnce() + Send + 'static>) -> Result<JoinHandle<()>, ()>,
    {
        let root_process = outer_pin_process(root)?.ok_or(())?;
        let descendants = Arc::new(Mutex::new(BTreeMap::from([(root, root_process)])));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher_descendants = Arc::clone(&descendants);
        let watcher_stop = Arc::clone(&stop);
        let process_tree_supported = Arc::new(AtomicBool::new(true));
        let watcher_process_tree_supported = Arc::clone(&process_tree_supported);
        let watcher = spawn(Box::new(move || {
            while !watcher_stop.load(Ordering::Acquire) {
                if outer_observe_descendants(root, &watcher_descendants, Some(&watcher_stop), None)
                    .is_err()
                {
                    watcher_process_tree_supported.store(false, Ordering::Release);
                    return;
                }
                std::thread::sleep(OUTER_PROCESS_POLL_INTERVAL);
            }
        }))?;
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

    fn watcher_is_supported(&self) -> Result<(), ()> {
        outer_watcher_is_supported(&self.process_tree_supported)
    }

    fn kill_all(&mut self, deadline: Instant) -> Result<(), ()> {
        self.stop_watcher();
        if outer_observe_descendants(self.root, &self.descendants, None, Some(deadline)).is_err() {
            self.process_tree_supported.store(false, Ordering::Release);
        }
        self.kill_tracked(deadline)
    }

    fn finish(&mut self, deadline: Instant) -> OuterCleanupStatus {
        self.armed = false;
        self.stop_watcher();
        loop {
            let observation =
                outer_observe_descendants(self.root, &self.descendants, None, Some(deadline));
            if observation.is_err() {
                self.process_tree_supported.store(false, Ordering::Release);
            }
            if observation == Ok(OuterObservationStatus::Interrupted) {
                return OuterCleanupStatus::Failed;
            }
            if self.kill_tracked(deadline).is_err()
                || outer_reap_tracked(&self.descendants, deadline).is_err()
            {
                return OuterCleanupStatus::Failed;
            }
            match self.all_tracked_absent(deadline) {
                Ok(true) => {
                    return if self.process_tree_supported.load(Ordering::Acquire) {
                        OuterCleanupStatus::Complete
                    } else {
                        OuterCleanupStatus::ProcessTreeUnsupported
                    };
                }
                Ok(false) => {}
                Err(()) => return OuterCleanupStatus::Failed,
            }
            if Instant::now() >= deadline {
                return OuterCleanupStatus::Failed;
            }
            std::thread::sleep(OUTER_PROCESS_POLL_INTERVAL);
        }
    }

    fn kill_tracked(&self, deadline: Instant) -> Result<(), ()> {
        outer_kill_tracked(&self.descendants, deadline)
    }

    fn all_tracked_absent(&self, deadline: Instant) -> Result<bool, ()> {
        outer_all_tracked_absent(&self.descendants, deadline)
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
fn outer_watcher_is_supported(process_tree_supported: &AtomicBool) -> Result<(), ()> {
    process_tree_supported
        .load(Ordering::Acquire)
        .then_some(())
        .ok_or(())
}

#[cfg(target_os = "linux")]
fn outer_kill_tracked(
    descendants: &Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>,
    deadline: Instant,
) -> Result<(), ()> {
    let descendants = descendants.lock().unwrap_or_else(PoisonError::into_inner);
    for process in descendants.values() {
        if Instant::now() >= deadline {
            return Err(());
        }
        let _ = rustix::process::pidfd_send_signal(&process.pidfd, rustix::process::Signal::KILL);
    }
    (Instant::now() < deadline).then_some(()).ok_or(())
}

#[cfg(target_os = "linux")]
fn outer_all_tracked_absent(
    descendants: &Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>,
    deadline: Instant,
) -> Result<bool, ()> {
    let descendants = descendants.lock().unwrap_or_else(PoisonError::into_inner);
    for (raw_pid, process) in descendants.iter() {
        if Instant::now() >= deadline {
            return Err(());
        }
        if outer_process_start_time(*raw_pid)? == Some(process.start_time) {
            return Ok(false);
        }
    }
    if Instant::now() >= deadline {
        Err(())
    } else {
        Ok(true)
    }
}

#[cfg(target_os = "linux")]
impl Drop for OuterProcessTreeGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.finish(Instant::now() + OUTER_PROCESS_CLEANUP_DEADLINE);
        }
    }
}

#[cfg(target_os = "linux")]
fn preflight_outer_process_tree(deadline: Instant) -> Result<OuterTrackedProcess, ()> {
    outer_process_children_until(std::process::id(), None, Some(deadline)).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let process = outer_pin_process(std::process::id())?.ok_or(())?;
    (Instant::now() < deadline).then_some(process).ok_or(())
}

#[cfg(target_os = "linux")]
fn outer_observe_descendants(
    root: u32,
    descendants: &Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>,
    stop: Option<&AtomicBool>,
    deadline: Option<Instant>,
) -> Result<OuterObservationStatus, ()> {
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
        let mut interrupted = false;
        for (parent, expected_start_time) in parents {
            if outer_observation_should_stop(stop, deadline) {
                interrupted = true;
                break;
            }
            if outer_process_start_time(parent)? != Some(expected_start_time) {
                if parent != root {
                    known.remove(&parent);
                    retired.push((parent, expected_start_time));
                }
                continue;
            }
            let children = match outer_process_children_until(parent, stop, deadline) {
                Ok(children) => children,
                Err(OuterProcessChildrenError::Interrupted) => {
                    interrupted = true;
                    break;
                }
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
                if outer_observation_should_stop(stop, deadline) {
                    interrupted = true;
                    break;
                }
                if known.contains_key(&raw_pid) {
                    continue;
                }
                if let Some(process) = outer_pin_child_process(parent, raw_pid)? {
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
        if interrupted {
            return Ok(OuterObservationStatus::Interrupted);
        }
        if !discovered {
            break;
        }
    }
    outer_retire_reused(root, descendants, stop, deadline)
}

#[cfg(target_os = "linux")]
fn outer_observation_should_stop(stop: Option<&AtomicBool>, deadline: Option<Instant>) -> bool {
    stop.is_some_and(|stop| stop.load(Ordering::Acquire))
        || deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

#[cfg(target_os = "linux")]
fn outer_retire_reused(
    root: u32,
    descendants: &Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>,
    stop: Option<&AtomicBool>,
    deadline: Option<Instant>,
) -> Result<OuterObservationStatus, ()> {
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
        if outer_observation_should_stop(stop, deadline) {
            return Ok(OuterObservationStatus::Interrupted);
        }
        if outer_process_start_time(raw_pid)? != Some(expected_start_time) {
            retired.push((raw_pid, expected_start_time));
        }
    }
    if outer_observation_should_stop(stop, deadline) {
        return Ok(OuterObservationStatus::Interrupted);
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
    Ok(OuterObservationStatus::Complete)
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
fn outer_pin_child_process(parent: u32, raw_pid: u32) -> Result<Option<OuterTrackedProcess>, ()> {
    let Some(process) = outer_pin_process(raw_pid)? else {
        return Ok(None);
    };
    let expected = OuterProcessIdentity {
        parent,
        start_time: process.start_time,
    };
    Ok((outer_process_identity(raw_pid)? == Some(expected)).then_some(process))
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
    Ok(outer_process_identity(pid)?.map(|identity| identity.start_time))
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OuterProcessIdentity {
    parent: u32,
    start_time: u64,
}

#[cfg(target_os = "linux")]
fn outer_process_identity(pid: u32) -> Result<Option<OuterProcessIdentity>, ()> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if outer_process_gone(&error) => return Ok(None),
        Err(_) => return Err(()),
    };
    let command_end = stat.rfind(')').ok_or(())?;
    let mut fields = stat[command_end + 1..].split_whitespace();
    let parent = fields
        .clone()
        .nth(1)
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let start_time = fields.nth(19).ok_or(())?.parse::<u64>().map_err(|_| ())?;
    Ok(Some(OuterProcessIdentity { parent, start_time }))
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
    Interrupted,
    Unsupported,
}

#[cfg(target_os = "linux")]
fn outer_process_children_until(
    pid: u32,
    stop: Option<&AtomicBool>,
    deadline: Option<Instant>,
) -> Result<Vec<u32>, OuterProcessChildrenError> {
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
        if outer_observation_should_stop(stop, deadline) {
            return Err(OuterProcessChildrenError::Interrupted);
        }
        let entry = entry.map_err(|_| OuterProcessChildrenError::Unsupported)?;
        let task = entry
            .file_name()
            .to_string_lossy()
            .parse::<u32>()
            .map_err(|_| OuterProcessChildrenError::Unsupported)?;
        if outer_observation_should_stop(stop, deadline) {
            return Err(OuterProcessChildrenError::Interrupted);
        }
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
fn outer_reap_tracked(
    descendants: &Arc<Mutex<BTreeMap<u32, OuterTrackedProcess>>>,
    deadline: Instant,
) -> Result<(), ()> {
    let descendants = descendants.lock().unwrap_or_else(PoisonError::into_inner);
    for process in descendants.values() {
        if Instant::now() >= deadline {
            return Err(());
        }
        let _ = rustix::process::waitid(
            rustix::process::WaitId::PidFd(process.pidfd.as_fd()),
            rustix::process::WaitIdOptions::NOHANG | rustix::process::WaitIdOptions::EXITED,
        );
    }
    (Instant::now() < deadline).then_some(()).ok_or(())
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
    if request.capture_bytes > PROCESS_CAPTURE_BYTES_LIMIT {
        return empty_process_result(ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::Other,
        });
    }
    let Some(request_deadline) = Instant::now().checked_add(request.timeout) else {
        return empty_process_result(ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::Other,
        });
    };
    let asynchronous_request_deadline = tokio::time::Instant::from_std(request_deadline);
    let status_protocol = request.status_protocol;
    let outer_reservation = match preflight_outer_process_tree(request_deadline) {
        Ok(reservation) => reservation,
        Err(()) if Instant::now() >= request_deadline => {
            return empty_process_result(ProcessOutcome::TimedOut);
        }
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
    match request.environment_inheritance {
        ProcessEnvironment::Clear => {
            command.env_clear();
        }
        ProcessEnvironment::Inherit => {}
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
            let cleanup_deadline = Instant::now() + OUTER_PROCESS_CLEANUP_DEADLINE;
            terminate_child_until(&mut child, cleanup_deadline).await;
            return discarded_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            });
        }
    };
    let mut outer_tree = match OuterProcessTreeGuard::new(supervisor_pid) {
        Ok(tree) => tree,
        Err(()) => {
            let cleanup_deadline = Instant::now() + OUTER_PROCESS_CLEANUP_DEADLINE;
            kill_supervisor_process_group(supervisor_pid);
            terminate_child_until(&mut child, cleanup_deadline).await;
            return discarded_process_result(ProcessOutcome::SupervisionFailed {
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
            let cleanup_deadline = Instant::now() + OUTER_PROCESS_CLEANUP_DEADLINE;
            let _ = outer_tree.kill_all(cleanup_deadline);
            terminate_child_until(&mut child, cleanup_deadline).await;
            return discarded_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stdout,
            });
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(control);
            let cleanup_deadline = Instant::now() + OUTER_PROCESS_CLEANUP_DEADLINE;
            let _ = outer_tree.kill_all(cleanup_deadline);
            terminate_child_until(&mut child, cleanup_deadline).await;
            return discarded_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stderr,
            });
        }
    };
    let mut stdout_task = tokio::spawn(read_supervised_stdout(
        stdout,
        request.capture_bytes,
        status_protocol,
    ));
    let mut stderr_task = tokio::spawn(read_bounded(stderr, request.capture_bytes));
    let startup = tokio::time::timeout_at(asynchronous_request_deadline, async {
        let control = control.as_mut().ok_or(())?;
        control.write_all(&[1]).await.map_err(|_| ())
    })
    .await;
    if !matches!(startup, Ok(Ok(()))) {
        let timed_out = startup.is_err();
        drop(control);
        let cleanup_deadline = Instant::now() + OUTER_PROCESS_CLEANUP_DEADLINE;
        kill_supervisor_process_group(supervisor_pid);
        let _ = outer_tree.kill_all(cleanup_deadline);
        terminate_child_until(&mut child, cleanup_deadline).await;
        let cleanup = outer_tree.finish(cleanup_deadline);
        stdout_task.abort();
        stderr_task.abort();
        return discarded_process_result(if timed_out && cleanup == OuterCleanupStatus::Complete {
            ProcessOutcome::TimedOut
        } else {
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            }
        });
    }
    let root_exit = tokio::time::timeout_at(asynchronous_request_deadline, async {
        loop {
            outer_tree.watcher_is_supported()?;
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
    let cleanup_started = Instant::now();
    let cleanup_deadline = cleanup_started + OUTER_PROCESS_CLEANUP_DEADLINE;
    let graceful_deadline = cleanup_started + OUTER_PROCESS_GRACEFUL_WAIT;
    let async_cleanup_deadline = tokio::time::Instant::from_std(cleanup_deadline);
    let mut waited = if observation_failed {
        kill_supervisor_process_group(supervisor_pid);
        let _ = outer_tree.kill_all(cleanup_deadline);
        let _ = child.start_kill();
        tokio::time::timeout_at(async_cleanup_deadline, child.wait()).await
    } else {
        tokio::time::timeout_at(
            tokio::time::Instant::from_std(graceful_deadline),
            child.wait(),
        )
        .await
    };
    if waited.is_err() {
        kill_supervisor_process_group(supervisor_pid);
        let _ = outer_tree.kill_all(cleanup_deadline);
        let _ = child.start_kill();
        waited = tokio::time::timeout_at(async_cleanup_deadline, child.wait()).await;
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
            let _ = outer_tree.kill_all(cleanup_deadline);
            let _ = child.start_kill();
            Some(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            })
        }
    };
    if wait_failure.is_none() {
        wait_failure = waited_cleanly;
    }
    if outer_tree.finish(cleanup_deadline) != OuterCleanupStatus::Complete {
        wait_failure = Some(ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Cleanup,
        });
    }
    let wait_failed = wait_failure.is_some();
    let captures = tokio::time::timeout_at(async_cleanup_deadline, async {
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
            return discarded_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            });
        }
    };
    match (stdout, stderr) {
        (Ok(Ok((stdout, status, launcher_status))), Ok(Ok(stderr))) => {
            let (supervised_stdout, supervised_stderr) =
                outer_capture_completeness(wait_failed, status, launcher_status);
            ProcessRunResult {
                outcome: wait_failure
                    .unwrap_or_else(|| supervisor_outcome(status, launcher_status)),
                stdout: ProcessOutput {
                    bytes: stdout.bytes,
                    completeness: combined_capture_completeness(
                        stdout.completeness,
                        supervised_stdout,
                    ),
                },
                stderr: ProcessOutput {
                    bytes: stderr.bytes,
                    completeness: combined_capture_completeness(
                        stderr.completeness,
                        supervised_stderr,
                    ),
                },
            }
        }
        (Ok(Err(_)) | Err(_), _) => {
            discarded_process_result(wait_failure.unwrap_or(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stdout,
            }))
        }
        (_, Ok(Err(_)) | Err(_)) => {
            discarded_process_result(wait_failure.unwrap_or(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stderr,
            }))
        }
    }
}

#[cfg(target_os = "linux")]
async fn terminate_child_until(child: &mut tokio::process::Child, deadline: Instant) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), child.wait()).await;
}

#[cfg(target_os = "linux")]
fn kill_supervisor_process_group(raw_pid: u32) {
    if let Some(pid) = rustix::process::Pid::from_raw(raw_pid as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(target_os = "linux")]
fn supervisor_outcome(
    status: SupervisorStatus,
    launcher_status: Option<LauncherStatus>,
) -> ProcessOutcome {
    match status {
        SupervisorStatus::Exited { code, .. } => launcher_status
            .map(launcher_outcome)
            .unwrap_or(ProcessOutcome::Exited { code }),
        SupervisorStatus::TimedOut => ProcessOutcome::TimedOut,
        SupervisorStatus::SpawnFailed { reason } => process_spawn_failure(reason),
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

#[cfg(target_os = "linux")]
fn launcher_outcome(status: LauncherStatus) -> ProcessOutcome {
    match status {
        LauncherStatus::Exited { code, .. } => ProcessOutcome::Exited { code },
        LauncherStatus::SpawnFailed { reason } => process_spawn_failure(reason),
        LauncherStatus::SupervisionFailed => ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Wait,
        },
    }
}

#[cfg(target_os = "linux")]
fn process_spawn_failure(reason: SupervisorSpawnFailure) -> ProcessOutcome {
    ProcessOutcome::SpawnFailed {
        reason: match reason {
            SupervisorSpawnFailure::NotFound => ProcessSpawnFailure::NotFound,
            SupervisorSpawnFailure::PermissionDenied => ProcessSpawnFailure::PermissionDenied,
            SupervisorSpawnFailure::ProcessTreeUnsupported => {
                ProcessSpawnFailure::ProcessTreeUnsupported
            }
            SupervisorSpawnFailure::Other => ProcessSpawnFailure::Other,
        },
    }
}

#[cfg(target_os = "linux")]
fn supervisor_capture_completeness(
    status: SupervisorStatus,
    launcher_status: Option<LauncherStatus>,
) -> (SupervisorCaptureCompleteness, SupervisorCaptureCompleteness) {
    let outer = match status {
        SupervisorStatus::Exited { stdout, stderr, .. } => (stdout, stderr),
        SupervisorStatus::TimedOut
        | SupervisorStatus::Cancelled
        | SupervisorStatus::SupervisionFailed { .. } => (
            SupervisorCaptureCompleteness::Incomplete,
            SupervisorCaptureCompleteness::Incomplete,
        ),
        SupervisorStatus::SpawnFailed { .. } => (
            SupervisorCaptureCompleteness::Complete,
            SupervisorCaptureCompleteness::Complete,
        ),
    };
    let nested = match launcher_status {
        Some(LauncherStatus::Exited { stdout, stderr, .. }) => (stdout, stderr),
        Some(LauncherStatus::SupervisionFailed) => (
            SupervisorCaptureCompleteness::Incomplete,
            SupervisorCaptureCompleteness::Incomplete,
        ),
        Some(LauncherStatus::SpawnFailed { .. }) | None => (
            SupervisorCaptureCompleteness::Complete,
            SupervisorCaptureCompleteness::Complete,
        ),
    };
    (
        combined_supervisor_completeness(outer.0, nested.0),
        combined_supervisor_completeness(outer.1, nested.1),
    )
}

#[cfg(target_os = "linux")]
fn outer_capture_completeness(
    wait_failed: bool,
    status: SupervisorStatus,
    launcher_status: Option<LauncherStatus>,
) -> (SupervisorCaptureCompleteness, SupervisorCaptureCompleteness) {
    if wait_failed {
        (
            SupervisorCaptureCompleteness::Incomplete,
            SupervisorCaptureCompleteness::Incomplete,
        )
    } else {
        supervisor_capture_completeness(status, launcher_status)
    }
}

#[cfg(target_os = "linux")]
fn combined_supervisor_completeness(
    outer: SupervisorCaptureCompleteness,
    nested: SupervisorCaptureCompleteness,
) -> SupervisorCaptureCompleteness {
    match (outer, nested) {
        (SupervisorCaptureCompleteness::Complete, SupervisorCaptureCompleteness::Complete) => {
            SupervisorCaptureCompleteness::Complete
        }
        (SupervisorCaptureCompleteness::Complete, SupervisorCaptureCompleteness::Incomplete)
        | (SupervisorCaptureCompleteness::Incomplete, _) => {
            SupervisorCaptureCompleteness::Incomplete
        }
    }
}

#[cfg(target_os = "linux")]
fn combined_capture_completeness(
    bounded: CaptureCompleteness,
    supervised: SupervisorCaptureCompleteness,
) -> CaptureCompleteness {
    match (bounded, supervised) {
        (CaptureCompleteness::Complete, SupervisorCaptureCompleteness::Complete) => {
            CaptureCompleteness::Complete
        }
        (CaptureCompleteness::Complete, SupervisorCaptureCompleteness::Incomplete)
        | (CaptureCompleteness::Truncated, _) => CaptureCompleteness::Truncated,
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

fn discarded_process_result(outcome: ProcessOutcome) -> ProcessRunResult {
    ProcessRunResult {
        outcome,
        stdout: ProcessOutput {
            bytes: Vec::new(),
            completeness: CaptureCompleteness::Truncated,
        },
        stderr: ProcessOutput {
            bytes: Vec::new(),
            completeness: CaptureCompleteness::Truncated,
        },
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    };
    use std::sync::{Arc, Mutex, PoisonError};

    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};

    use super::*;

    const SANDBOXED_STDOUT: &str = "checked";
    const SANDBOXED_WORKING_DIRECTORY: &str = "crate";
    const SETUP_CAPTURE_BYTES: usize = 4;
    const SETUP_STDOUT: &[u8] = b"12345";
    const SETUP_STDERR: &[u8] = b"67890";
    const SUCCESSFUL_EXIT_CODE: i32 = 0;
    const LEGITIMATE_TARGET_EXIT_CODE: i32 = 127;
    const UNUSABLE_PROBE_EXIT_CODE: i32 = 1;
    const REQUEST_TIMEOUT_SECONDS: u64 = 1;
    const ROOT_WORKSPACE_BIND_COUNT: usize = 1;
    const TEST_SANDBOX_LAUNCHER_DESCRIPTOR: i32 = 91;
    const SLOW_PROBE_DELAY: Duration = Duration::from_millis(1_100);
    const OVERSIZED_CAPTURE_BYTES: usize = PROCESS_CAPTURE_BYTES_LIMIT + 1;
    const TEST_SANDBOX_LAUNCHER: &str = "/fixture/signalbox-exec-supervisor";
    const TEST_BUBBLEWRAP_PROGRAM: &str = "/configured/bin/bwrap";
    #[cfg(target_os = "linux")]
    const TEST_SANDBOX_BIND_DESCRIPTOR: i32 = 90;
    const TEST_SANDBOX_WORKSPACE_ROOT: &str = "/fixture/workspace";
    const ISOLATION_FIXTURE_TIMEOUT: Duration = Duration::from_secs(30);

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_supervision_failure_remains_distinct() {
        let outcome = supervisor_outcome(
            SupervisorStatus::SupervisionFailed {
                stage: SupervisorFailureStage::Cleanup,
            },
            None,
        );

        assert_eq!(
            outcome,
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_supervision_failure_overrides_a_nested_success() {
        let outcome = supervisor_outcome(
            SupervisorStatus::SupervisionFailed {
                stage: SupervisorFailureStage::Cleanup,
            },
            Some(LauncherStatus::Exited {
                code: Some(0),
                stdout: SupervisorCaptureCompleteness::Complete,
                stderr: SupervisorCaptureCompleteness::Complete,
            }),
        );

        assert_eq!(
            outcome,
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Cleanup,
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interrupted_supervision_marks_both_captures_incomplete() {
        let completeness = supervisor_capture_completeness(SupervisorStatus::TimedOut, None);

        assert_eq!(
            completeness,
            (
                SupervisorCaptureCompleteness::Incomplete,
                SupervisorCaptureCompleteness::Incomplete,
            )
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_wait_failure_distrusts_complete_status_trailers() {
        let completeness = outer_capture_completeness(
            true,
            SupervisorStatus::Exited {
                code: Some(0),
                stdout: SupervisorCaptureCompleteness::Complete,
                stderr: SupervisorCaptureCompleteness::Complete,
            },
            Some(LauncherStatus::Exited {
                code: Some(0),
                stdout: SupervisorCaptureCompleteness::Complete,
                stderr: SupervisorCaptureCompleteness::Complete,
            }),
        );

        assert_eq!(
            completeness,
            (
                SupervisorCaptureCompleteness::Incomplete,
                SupervisorCaptureCompleteness::Incomplete,
            )
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_retirement_honors_the_watcher_stop_signal() {
        let stop = AtomicBool::new(true);
        let tracked = Arc::new(Mutex::new(BTreeMap::new()));

        assert_eq!(
            outer_retire_reused(u32::MAX, &tracked, Some(&stop), None),
            Ok(OuterObservationStatus::Interrupted)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_retirement_honors_the_cleanup_deadline() {
        let tracked = Arc::new(Mutex::new(BTreeMap::new()));

        assert_eq!(
            outer_retire_reused(u32::MAX, &tracked, None, Some(Instant::now())),
            Ok(OuterObservationStatus::Interrupted)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_thread_enumeration_honors_the_watcher_stop_signal() {
        let stop = AtomicBool::new(true);

        assert!(matches!(
            outer_process_children_until(std::process::id(), Some(&stop), None),
            Err(OuterProcessChildrenError::Interrupted)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_process_tree_preflight_honors_the_request_deadline() {
        assert!(preflight_outer_process_tree(Instant::now()).is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn process_runner_rejects_an_unrepresentable_deadline_before_spawn() {
        let result = run_process_linux(
            Path::new("/definitely-missing-supervisor"),
            ProcessRequest {
                program: OsString::from("true"),
                arguments: Vec::new(),
                working_directory: PathBuf::from("/"),
                timeout: Duration::MAX,
                capture_bytes: EXEC_CAPTURE_BYTES,
                environment: BTreeMap::new(),
                environment_inheritance: ProcessEnvironment::Clear,
                status_protocol: ProcessStatusProtocol::Direct,
            },
        )
        .await;

        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::Other,
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn process_runner_rejects_an_oversized_capture_before_spawn() {
        let result = run_process_linux(
            Path::new("/definitely-missing-supervisor"),
            ProcessRequest {
                program: OsString::from("true"),
                arguments: Vec::new(),
                working_directory: PathBuf::from("/"),
                timeout: Duration::from_secs(REQUEST_TIMEOUT_SECONDS),
                capture_bytes: OVERSIZED_CAPTURE_BYTES,
                environment: BTreeMap::new(),
                environment_inheritance: ProcessEnvironment::Clear,
                status_protocol: ProcessStatusProtocol::Direct,
            },
        )
        .await;

        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::Other,
            }
        );
    }

    struct ReplacementWorkspace {
        path: PathBuf,
        retired: PathBuf,
    }

    #[cfg(target_os = "linux")]
    struct ReplacementSupervisor {
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
            let escaped_directory = root.join("escaped");
            let outside_directory = root.join("outside");
            let executable_directory = root.join("executable");
            std::fs::create_dir_all(&blocked_directory)?;
            std::fs::create_dir_all(&escaped_directory)?;
            std::fs::create_dir_all(&outside_directory)?;
            std::fs::create_dir_all(&executable_directory)?;
            let blocked_shell = blocked_directory.join("sh");
            let outside_shell = outside_directory.join("sh");
            let expected_shell = executable_directory.join("sh");
            std::fs::write(&blocked_shell, b"blocked")?;
            std::fs::write(&outside_shell, b"outside")?;
            std::fs::write(&expected_shell, b"executable")?;
            std::fs::set_permissions(&blocked_shell, std::fs::Permissions::from_mode(0o000))?;
            std::fs::set_permissions(&outside_shell, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&expected_shell, std::fs::Permissions::from_mode(0o700))?;
            symlink(&outside_shell, escaped_directory.join("sh"))?;
            let search_path =
                std::env::join_paths([blocked_directory, escaped_directory, executable_directory])?;
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

        fn path(&self) -> &Path {
            &self.path
        }

        fn retired_path(&self) -> &Path {
            &self.retired
        }
    }

    #[cfg(target_os = "linux")]
    impl ReplacementSupervisor {
        fn new() -> Result<Self, std::io::Error> {
            let identity = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(std::io::Error::other)?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "signalbox-exec-supervisor-{}-{identity}",
                std::process::id()
            ));
            let retired = path.with_extension("retired");
            std::fs::copy(std::env::current_exe()?, &path)?;
            Ok(Self { path, retired })
        }

        fn replace(&self) -> Result<(), std::io::Error> {
            std::fs::rename(&self.path, &self.retired)?;
            std::fs::copy(std::env::current_exe()?, &self.path)?;
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn workspace_identity_is_derived_from_the_open_directory_after_path_replacement()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let directory = rustix::fs::open(
            workspace.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )?;
        workspace.replace()?;

        let identity = WorkspaceIdentity::from_open_directory(workspace.path(), directory)?;

        assert_eq!(
            identity.canonical_path,
            workspace.retired_path().canonicalize()?
        );
        assert!(!identity.matches(workspace.path()));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn supervisor_identity_is_derived_from_the_open_file_after_path_replacement()
    -> Result<(), Box<dyn Error>> {
        let supervisor = ReplacementSupervisor::new()?;
        let descriptor = rustix::fs::open(
            &supervisor.path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?;
        supervisor.replace()?;

        let (pinned, _descriptor) =
            pin_executable_program(&supervisor.path, descriptor, supervisor_program_error)?;

        assert_eq!(pinned.canonicalize()?, supervisor.retired.canonicalize()?);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_runners_reject_a_final_symlink_workspace_root() -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let symlinked = workspace.path.with_extension("symlink");
        symlink(&workspace.path, &symlinked)?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_process(b"must remain unused"),
        );

        let sandboxed = SandboxedCommandRunner::try_new(runner.clone(), &symlinked);
        let unsandboxed = UnsandboxedCommandRunner::try_new(runner, &symlinked);
        std::fs::remove_file(&symlinked)?;

        assert!(matches!(
            sandboxed,
            Err(ExecToolConstructionError::WorkspaceRoot { .. })
        ));
        assert!(matches!(
            unsandboxed,
            Err(ExecToolConstructionError::WorkspaceRoot { .. })
        ));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_runners_reject_a_relative_workspace_root() {
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_process(b"must remain unused"),
        );

        let sandboxed = SandboxedCommandRunner::try_new(runner.clone(), Path::new("."));
        let unsandboxed = UnsandboxedCommandRunner::try_new(runner, Path::new("."));

        assert!(matches!(
            sandboxed,
            Err(ExecToolConstructionError::WorkspaceRoot { .. })
        ));
        assert!(matches!(
            unsandboxed,
            Err(ExecToolConstructionError::WorkspaceRoot { .. })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_runner_rejects_a_non_executable_supervisor() -> Result<(), Box<dyn Error>> {
        let supervisor = ReplacementSupervisor::new()?;
        std::fs::set_permissions(&supervisor.path, std::fs::Permissions::from_mode(0o600))?;

        let result = TokioProcessRunner::try_new(&supervisor.path);

        assert!(matches!(
            result,
            Err(ExecToolConstructionError::SupervisorProgram { .. })
        ));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_runner_rejects_a_relative_supervisor_before_opening_it() {
        let result = TokioProcessRunner::try_new(Path::new("relative-supervisor"));

        assert!(matches!(
            result,
            Err(ExecToolConstructionError::SupervisorProgram { source: None, .. })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_runner_pins_the_configured_bubblewrap_identity() -> Result<(), Box<dyn Error>> {
        let supervisor = ReplacementSupervisor::new()?;
        let bubblewrap = ReplacementSupervisor::new()?;
        let runner =
            TokioProcessRunner::try_new_with_bubblewrap(&supervisor.path, &bubblewrap.path)?;
        let pinned = runner.bubblewrap_program().to_owned();
        bubblewrap.replace()?;

        assert_eq!(pinned.canonicalize()?, bubblewrap.retired.canonicalize()?);
        assert_ne!(pinned.canonicalize()?, bubblewrap.path.canonicalize()?);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_runner_rejects_a_relative_configured_bubblewrap_before_opening_it()
    -> Result<(), Box<dyn Error>> {
        let supervisor = ReplacementSupervisor::new()?;

        let result = TokioProcessRunner::try_new_with_bubblewrap(
            &supervisor.path,
            Path::new("relative-bwrap"),
        );

        assert!(matches!(
            result,
            Err(ExecToolConstructionError::BubblewrapProgram { source: None, .. })
        ));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_runner_rejects_a_supervisor_fifo_without_blocking() -> Result<(), Box<dyn Error>> {
        let directory = ReplacementWorkspace::new()?;
        let fifo = directory.path().join("supervisor");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;

        let result = TokioProcessRunner::try_new(&fifo);

        assert!(matches!(
            result,
            Err(ExecToolConstructionError::SupervisorProgram { source: None, .. })
        ));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandbox_dispatch_rejects_a_missing_launcher_status_after_nonzero_exit()
    -> Result<(), Box<dyn Error>> {
        let mut output = Vec::new();
        output.extend_from_slice(SUPERVISOR_STATUS_TRAILER);
        serde_json::to_writer(
            &mut output,
            &SupervisorStatus::Exited {
                code: Some(1),
                stdout: SupervisorCaptureCompleteness::Complete,
                stderr: SupervisorCaptureCompleteness::Complete,
            },
        )?;
        output.push(b'\n');

        let result = read_supervised_stdout(
            output.as_slice(),
            EXEC_CAPTURE_BYTES,
            ProcessStatusProtocol::SandboxDispatch,
        )
        .await;

        assert!(result.is_err());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn failed_sandbox_dispatch_distrusts_a_forged_success_trailer()
    -> Result<(), Box<dyn Error>> {
        let mut output = Vec::new();
        output.extend_from_slice(LAUNCH_STATUS_TRAILER);
        serde_json::to_writer(
            &mut output,
            &LauncherStatus::Exited {
                code: Some(SUCCESSFUL_EXIT_CODE),
                stdout: SupervisorCaptureCompleteness::Complete,
                stderr: SupervisorCaptureCompleteness::Complete,
            },
        )?;
        output.push(b'\n');
        output.extend_from_slice(SUPERVISOR_STATUS_TRAILER);
        serde_json::to_writer(
            &mut output,
            &SupervisorStatus::Exited {
                code: Some(UNUSABLE_PROBE_EXIT_CODE),
                stdout: SupervisorCaptureCompleteness::Complete,
                stderr: SupervisorCaptureCompleteness::Complete,
            },
        )?;
        output.push(b'\n');

        let (_, status, launcher_status) = read_supervised_stdout(
            output.as_slice(),
            EXEC_CAPTURE_BYTES,
            ProcessStatusProtocol::SandboxDispatch,
        )
        .await?;
        let outcome = supervisor_outcome(status, launcher_status);

        assert_eq!(
            outcome,
            ProcessOutcome::Exited {
                code: Some(UNUSABLE_PROBE_EXIT_CODE),
            }
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandbox_dispatch_preserves_pre_dispatch_spawn_failure_without_launcher_status()
    -> Result<(), Box<dyn Error>> {
        let mut output = Vec::new();
        output.extend_from_slice(SUPERVISOR_STATUS_TRAILER);
        serde_json::to_writer(
            &mut output,
            &SupervisorStatus::SpawnFailed {
                reason: SupervisorSpawnFailure::NotFound,
            },
        )?;
        output.push(b'\n');

        let (_, status, launcher_status) = read_supervised_stdout(
            output.as_slice(),
            EXEC_CAPTURE_BYTES,
            ProcessStatusProtocol::SandboxDispatch,
        )
        .await?;

        assert_eq!(
            status,
            SupervisorStatus::SpawnFailed {
                reason: SupervisorSpawnFailure::NotFound,
            }
        );
        assert_eq!(launcher_status, None);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_outer_child_must_still_name_its_observed_parent() -> Result<(), Box<dyn Error>> {
        let raw_pid = std::process::id();
        let identity = outer_process_identity(raw_pid)
            .map_err(|()| std::io::Error::other("read current process identity"))?
            .ok_or_else(|| std::io::Error::other("current process disappeared"))?;

        let accepted = outer_pin_child_process(identity.parent, raw_pid)
            .map_err(|()| std::io::Error::other("pin current process as child"))?;
        let rejected = outer_pin_child_process(raw_pid, raw_pid)
            .map_err(|()| std::io::Error::other("reject wrong process parent"))?;

        assert!(accepted.is_some());
        assert!(rejected.is_none());
        Ok(())
    }

    #[test]
    fn discarded_captures_are_reported_as_truncated() {
        let result = discarded_process_result(ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Stdout,
        });

        assert_eq!(result.stdout.completeness, CaptureCompleteness::Truncated);
        assert_eq!(result.stderr.completeness, CaptureCompleteness::Truncated);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_watcher_spawn_failure_is_returned_instead_of_panicking() {
        let result = OuterProcessTreeGuard::new_with_watcher(std::process::id(), |_| Err(()));

        assert!(result.is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_finish_disarms_drop_cleanup_instead_of_granting_a_fresh_deadline() {
        let mut guard = OuterProcessTreeGuard {
            root: std::process::id(),
            descendants: Arc::new(Mutex::new(BTreeMap::new())),
            stop: Arc::new(AtomicBool::new(true)),
            watcher: None,
            process_tree_supported: Arc::new(AtomicBool::new(true)),
            armed: true,
        };

        let result = guard.finish(Instant::now());

        assert_eq!(result, OuterCleanupStatus::Failed);
        assert!(!guard.armed);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn final_absence_scan_honors_an_elapsed_cleanup_deadline() -> Result<(), Box<dyn Error>> {
        let process = outer_pin_process(std::process::id())
            .map_err(|()| std::io::Error::other("pin current process"))?
            .ok_or_else(|| std::io::Error::other("current process disappeared"))?;
        let descendants = Arc::new(Mutex::new(BTreeMap::from([(std::process::id(), process)])));

        let result = outer_all_tracked_absent(&descendants, Instant::now());

        assert_eq!(result, Err(()));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn signaling_and_reaping_honor_an_elapsed_cleanup_deadline() -> Result<(), Box<dyn Error>> {
        let process = outer_pin_process(std::process::id())
            .map_err(|()| std::io::Error::other("pin current process"))?
            .ok_or_else(|| std::io::Error::other("current process disappeared"))?;
        let descendants = Arc::new(Mutex::new(BTreeMap::from([(std::process::id(), process)])));
        let deadline = Instant::now();

        let signaling = outer_kill_tracked(&descendants, deadline);
        let reaping = outer_reap_tracked(&descendants, deadline);

        assert_eq!(signaling, Err(()));
        assert_eq!(reaping, Err(()));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_reaping_uses_the_pinned_identity_not_its_numeric_map_key() -> Result<(), Box<dyn Error>>
    {
        let mut tracked_child = exiting_test_process()?;
        let mut replacement_child = exiting_test_process()?;
        let tracked_process = outer_pin_process(tracked_child.id())
            .map_err(|()| std::io::Error::other("pin tracked child"))?
            .ok_or_else(|| std::io::Error::other("tracked child disappeared"))?;
        let replacement_process = outer_pin_process(replacement_child.id())
            .map_err(|()| std::io::Error::other("pin replacement child"))?
            .ok_or_else(|| std::io::Error::other("replacement child disappeared"))?;
        wait_for_pinned_exit(&tracked_process.pidfd)?;
        wait_for_pinned_exit(&replacement_process.pidfd)?;
        drop(replacement_process);
        let descendants = Arc::new(Mutex::new(BTreeMap::from([(
            replacement_child.id(),
            tracked_process,
        )])));

        outer_reap_tracked(&descendants, Instant::now() + Duration::from_secs(1))
            .map_err(|()| std::io::Error::other("reap tracked child"))?;
        let replacement_status = replacement_child.wait()?;
        let tracked_error = tracked_child
            .wait()
            .expect_err("the pinned tracked child was already reaped");

        assert!(replacement_status.success());
        assert_eq!(
            tracked_error.raw_os_error(),
            Some(rustix::io::Errno::CHILD.raw_os_error())
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn exiting_test_process() -> std::io::Result<std::process::Child> {
        std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "no_test_has_this_name"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    }

    #[cfg(target_os = "linux")]
    fn wait_for_pinned_exit(pidfd: &rustix::fd::OwnedFd) -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match outer_pidfd_has_exited(pidfd) {
                Ok(true) => return Ok(()),
                Ok(false) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(false) | Err(()) => {
                    return Err(std::io::Error::other("child did not exit before deadline"));
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watcher_support_loss_is_fail_closed() {
        let process_tree_supported = AtomicBool::new(false);

        assert_eq!(outer_watcher_is_supported(&process_tree_supported), Err(()));
    }

    impl Drop for ReplacementWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
            let _ = std::fs::remove_dir_all(&self.retired);
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ReplacementSupervisor {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(&self.retired);
        }
    }

    #[derive(Clone, Debug)]
    struct FakeRunner {
        availability: BwrapAvailability,
        bubblewrap_program: PathBuf,
        probe_delay: Duration,
        results: Arc<Mutex<Vec<ProcessRunResult>>>,
        probes: Arc<Mutex<Vec<ProcessRequest>>>,
        requests: Arc<Mutex<Vec<ProcessRequest>>>,
    }

    impl FakeRunner {
        fn returning(availability: BwrapAvailability, result: ProcessRunResult) -> Self {
            Self {
                availability,
                bubblewrap_program: PathBuf::from(BWRAP_PROGRAM),
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

        fn with_bubblewrap_program(mut self, bubblewrap_program: PathBuf) -> Self {
            self.bubblewrap_program = bubblewrap_program;
            self
        }
    }

    impl ProcessRunner for FakeRunner {
        fn sandbox_launcher_program(&self) -> &Path {
            Path::new(TEST_SANDBOX_LAUNCHER)
        }

        fn sandbox_launcher_descriptor(&self) -> Option<i32> {
            Some(TEST_SANDBOX_LAUNCHER_DESCRIPTOR)
        }

        fn bubblewrap_program(&self) -> &Path {
            &self.bubblewrap_program
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
    fn catalogs_fix_sandboxed_confirm_and_unsandboxed_always_confirm_permissions()
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
            ToolPermissionDefault::Confirm
        );
        assert_eq!(
            unsandboxed_definition.permission_default(),
            ToolPermissionDefault::AlwaysConfirm
        );
        Ok(())
    }

    #[test]
    fn production_probe_distinguishes_missing_bwrap_from_missing_probe_target() {
        let mut missing_bwrap = successful_process(b"");
        missing_bwrap.outcome = ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::NotFound,
        };
        let mut missing_probe_target = missing_bwrap.clone();
        missing_probe_target.stderr.bytes = SANDBOX_DISPATCH_MARKER.to_vec();

        assert_eq!(
            classify_bwrap_availability(&missing_bwrap),
            BwrapAvailability::Missing
        );
        assert_eq!(
            classify_bwrap_availability(&missing_probe_target),
            BwrapAvailability::Unusable
        );
    }

    #[test]
    fn production_probe_classifies_timeout_and_nonzero_exit_as_unusable() {
        let mut timed_out = successful_process(b"");
        timed_out.outcome = ProcessOutcome::TimedOut;
        let mut nonzero_exit = successful_process(b"");
        nonzero_exit.outcome = ProcessOutcome::Exited {
            code: Some(UNUSABLE_PROBE_EXIT_CODE),
        };

        assert_eq!(
            classify_bwrap_availability(&timed_out),
            BwrapAvailability::TimedOut
        );
        assert_eq!(
            classify_bwrap_availability(&nonzero_exit),
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
        assert_eq!(request.status_protocol, ProcessStatusProtocol::Direct);
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
        let workspace = ReplacementWorkspace::new()?;
        std::fs::create_dir(workspace.path.join(SANDBOXED_WORKING_DIRECTORY))?;
        let root = workspace.path.clone();
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        )
        .with_bubblewrap_program(PathBuf::from(TEST_BUBBLEWRAP_PROGRAM));
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, &root)?;
        let bind_descriptor =
            rustix::fd::AsRawFd::as_raw_fd(command_runner.workspace_identity._directory.as_ref());
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
            descriptor_path(bind_descriptor).into_os_string(),
            OsString::from(SANDBOX_WORKSPACE),
        ];
        let chdir_arguments = [
            OsString::from("--chdir"),
            OsString::from(format!("{SANDBOX_WORKSPACE}/{SANDBOXED_WORKING_DIRECTORY}")),
        ];
        let working_directory_bind_destination =
            OsString::from(format!("{SANDBOX_WORKSPACE}/{SANDBOXED_WORKING_DIRECTORY}"));
        let launcher_arguments = [
            OsString::from("--ro-bind"),
            descriptor_path(TEST_SANDBOX_LAUNCHER_DESCRIPTOR).into_os_string(),
            OsString::from(SANDBOX_DISPATCH_PROGRAM),
        ];
        let expected_bubblewrap_program = OsString::from(TEST_BUBBLEWRAP_PROGRAM);
        let dispatch_arguments = [
            OsString::from("--"),
            OsString::from(SANDBOX_DISPATCH_PROGRAM),
            OsString::from("--dispatch"),
            OsString::from("cargo"),
            OsString::from("check"),
        ];

        assert_eq!(request.program, expected_bubblewrap_program);
        assert_eq!(probe.program, OsString::from(TEST_BUBBLEWRAP_PROGRAM));
        assert_eq!(
            request.status_protocol,
            ProcessStatusProtocol::SandboxDispatch
        );
        assert_eq!(
            probe.status_protocol,
            ProcessStatusProtocol::SandboxDispatch
        );
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
        let working_directory_bind = request
            .arguments
            .windows(3)
            .find(|arguments| {
                arguments[0] == "--bind" && arguments[2] == working_directory_bind_destination
            })
            .ok_or("nested working directory bind")?;
        assert!(
            working_directory_bind[1]
                .to_string_lossy()
                .starts_with("/proc/self/fd/")
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn runner_restricted_request_uses_only_pinned_configured_read_only_paths()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let read_only = ReplacementWorkspace::new()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );
        let observation = runner.clone();
        let configured_path = read_only.path().to_owned();
        let mut command_runner = SandboxedCommandRunner::try_new_runner_restricted(
            runner,
            workspace.path(),
            std::slice::from_ref(&configured_path),
        )?;
        let arguments = ExecArguments {
            program: String::from("fixture-program"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };
        let expected_isolation_prefix = [
            OsString::from("--die-with-parent"),
            OsString::from("--new-session"),
            OsString::from("--unshare-user"),
            OsString::from("--unshare-pid"),
            OsString::from("--unshare-ipc"),
            OsString::from("--unshare-uts"),
            OsString::from("--unshare-cgroup"),
            OsString::from("--unshare-net"),
            OsString::from("--cap-drop"),
            OsString::from("ALL"),
        ];
        let expected_runtime = [OsString::from("--tmpfs"), OsString::from("/run")];

        let result = command_runner.try_run(arguments).await?;
        let requests = observation.recorded_requests();
        let request = requests
            .first()
            .ok_or_else(|| std::io::Error::other("one requested process"))?;
        let read_only_bind = request
            .arguments
            .windows(3)
            .find(|arguments| arguments[0] == "--ro-bind" && arguments[2] == configured_path)
            .ok_or_else(|| std::io::Error::other("configured read-only bind"))?;

        assert!(request.arguments.starts_with(&expected_isolation_prefix));
        assert!(
            request
                .arguments
                .windows(expected_runtime.len())
                .any(|arguments| arguments == expected_runtime)
        );
        assert!(
            read_only_bind[1]
                .to_string_lossy()
                .starts_with("/proc/self/fd/")
        );
        assert!(!request.arguments.contains(&OsString::from("/etc/hosts")));
        assert_eq!(
            request.environment.get(&OsString::from("PATH")),
            Some(&OsString::new())
        );
        assert_eq!(result.stdout.text, SANDBOXED_STDOUT);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn runner_restricted_https_bridge_binds_only_the_pinned_socket_and_proxy_mode()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let read_only = ReplacementWorkspace::new()?;
        let broker_root = ReplacementWorkspace::new()?;
        let broker_socket = broker_root.path().join("broker.sock");
        let _broker = UnixListener::bind(&broker_socket)?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );
        let observation = runner.clone();
        let mut command_runner =
            SandboxedCommandRunner::try_new_runner_restricted_with_https_broker(
                runner,
                workspace.path(),
                &[read_only.path().to_owned()],
                &broker_socket,
            )?;
        let arguments = ExecArguments {
            program: String::from("fixture-program"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };
        let proxy_environment = [
            OsString::from("--setenv"),
            OsString::from("HTTPS_PROXY"),
            OsString::from(SANDBOX_HTTPS_PROXY),
            OsString::from("--setenv"),
            OsString::from("https_proxy"),
            OsString::from(SANDBOX_HTTPS_PROXY),
        ];
        let dispatch_arguments = [
            OsString::from("--"),
            OsString::from(SANDBOX_DISPATCH_PROGRAM),
            OsString::from("--dispatch-with-https-proxy"),
            OsString::from("fixture-program"),
        ];

        let result = command_runner.try_run(arguments).await?;
        let requests = observation.recorded_requests();
        let request = requests
            .first()
            .ok_or_else(|| std::io::Error::other("one requested process"))?;
        let broker_bind = request
            .arguments
            .windows(3)
            .find(|arguments| {
                arguments[0] == "--ro-bind" && arguments[2] == SANDBOX_HTTPS_BROKER_SOCKET
            })
            .ok_or_else(|| std::io::Error::other("one HTTPS broker bind"))?;

        assert!(
            broker_bind[1]
                .to_string_lossy()
                .starts_with("/proc/self/fd/")
        );
        assert_ne!(broker_bind[1], broker_socket.as_os_str());
        assert!(
            request
                .arguments
                .windows(proxy_environment.len())
                .any(|arguments| arguments == proxy_environment)
        );
        assert!(request.arguments.ends_with(&dispatch_arguments));
        assert_eq!(result.stdout.text, SANDBOXED_STDOUT);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn runner_restricted_https_bridge_retains_descriptor_path_after_root_rename()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let read_only = ReplacementWorkspace::new()?;
        let broker_root = ReplacementWorkspace::new()?;
        let broker_root_descriptor = std::fs::File::open(broker_root.path())?;
        let broker_socket = broker_root.path().join("broker.sock");
        let _broker = UnixListener::bind(&broker_socket)?;
        let descriptor_socket = PathBuf::from(format!(
            "/proc/{}/fd/{}/broker.sock",
            std::process::id(),
            std::os::fd::AsRawFd::as_raw_fd(&broker_root_descriptor),
        ));
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );

        let observation = runner.clone();
        let mut command_runner =
            SandboxedCommandRunner::try_new_runner_restricted_with_https_broker(
                runner,
                workspace.path(),
                &[read_only.path().to_owned()],
                &descriptor_socket,
            )?;
        broker_root.replace()?;
        let arguments = ExecArguments {
            program: String::from("fixture-program"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.try_run(arguments).await?;

        assert_eq!(result.stdout.text, SANDBOXED_STDOUT);
        assert_eq!(observation.recorded_requests().len(), 1);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runner_restricted_https_bridge_rejects_a_regular_file_endpoint() -> Result<(), Box<dyn Error>>
    {
        let workspace = ReplacementWorkspace::new()?;
        let read_only = ReplacementWorkspace::new()?;
        let broker_root = ReplacementWorkspace::new()?;
        let broker_socket = broker_root.path().join("broker.sock");
        std::fs::write(&broker_socket, b"not a socket")?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );

        let rejected = SandboxedCommandRunner::try_new_runner_restricted_with_https_broker(
            runner,
            workspace.path(),
            &[read_only.path().to_owned()],
            &broker_socket,
        );

        assert!(matches!(
            rejected,
            Err(ExecToolConstructionError::HttpsBrokerSocket { source: None, .. })
        ));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn runner_restricted_https_bridge_retains_pinned_socket_after_path_replacement()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let read_only = ReplacementWorkspace::new()?;
        let broker_root = ReplacementWorkspace::new()?;
        let broker_socket = broker_root.path().join("broker.sock");
        let broker = UnixListener::bind(&broker_socket)?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );
        let observation = runner.clone();
        let mut command_runner =
            SandboxedCommandRunner::try_new_runner_restricted_with_https_broker(
                runner,
                workspace.path(),
                &[read_only.path().to_owned()],
                &broker_socket,
            )?;
        drop(broker);
        std::fs::remove_file(&broker_socket)?;
        let _replacement = UnixListener::bind(&broker_socket)?;
        let arguments = ExecArguments {
            program: String::from("fixture-program"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.try_run(arguments).await?;

        assert_eq!(result.confinement, ExecutionConfinement::FilesystemConfined);
        assert_eq!(result.stdout.text, SANDBOXED_STDOUT);
        assert_eq!(observation.recorded_probes().len(), 1);
        assert_eq!(observation.recorded_requests().len(), 1);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn runner_restricted_request_rechecks_read_only_path_identity_before_probe()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let read_only = ReplacementWorkspace::new()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );
        let observation = runner.clone();
        let configured_path = read_only.path().to_owned();
        let mut command_runner = SandboxedCommandRunner::try_new_runner_restricted(
            runner,
            workspace.path(),
            std::slice::from_ref(&configured_path),
        )?;
        read_only.replace()?;
        let arguments = ExecArguments {
            program: String::from("fixture-program"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.try_run(arguments).await?;

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

    /// Builds the launch context the two isolation tests below share. The
    /// descriptor and launcher fields differ by host, so the conditional
    /// compilation lives here and both test bodies stay straight-line.
    fn isolation_fixture_context(workspace_root: &Path) -> SandboxLaunchContext<'_> {
        SandboxLaunchContext {
            bind_source: workspace_root,
            #[cfg(target_os = "linux")]
            bind_descriptor: TEST_SANDBOX_BIND_DESCRIPTOR,
            #[cfg(not(target_os = "linux"))]
            launcher: Path::new(TEST_SANDBOX_LAUNCHER),
            #[cfg(target_os = "linux")]
            launcher_descriptor: TEST_SANDBOX_LAUNCHER_DESCRIPTOR,
            #[cfg(not(target_os = "linux"))]
            working_directory_bind_source: None,
            #[cfg(target_os = "linux")]
            working_directory_bind_descriptor: None,
        }
    }

    /// This profile's containment is exactly the namespace flags it opens with,
    /// and `--unshare-net` is the one that stops an approved command reaching a
    /// remote host. Every other assertion on this request matches a sub-slice,
    /// which a flag array that has silently lost an entry still satisfies, so
    /// this pins the whole isolation prefix in order against an expectation
    /// stated independently of the array under test. It calls `bwrap_request`
    /// directly rather than driving the runner, so it needs no bubblewrap
    /// binary and covers every host including the non-Linux development ones.
    /// The prefix is the whole of what this pins: `--unshare-cgroup` is not
    /// passed, so the cgroup namespace is shared and no assertion here says
    /// otherwise.
    #[test]
    fn sandboxed_request_opens_with_the_user_pid_ipc_uts_and_network_unshare_prefix() {
        let workspace_root = Path::new(TEST_SANDBOX_WORKSPACE_ROOT);
        let mount_profile = SandboxMountProfile::DaemonLocal;
        let executable_path = sandbox_path(workspace_root);
        let expected_isolation_prefix = [
            OsString::from("--die-with-parent"),
            OsString::from("--new-session"),
            OsString::from("--unshare-user"),
            OsString::from("--unshare-pid"),
            OsString::from("--unshare-ipc"),
            OsString::from("--unshare-uts"),
            OsString::from("--unshare-net"),
        ];

        let request = bwrap_request(
            isolation_fixture_context(workspace_root),
            Path::new(BWRAP_PROGRAM),
            SandboxInvocation {
                program: "cargo",
                arguments: &[String::from("check")],
                working_directory: ".",
                timeout: ISOLATION_FIXTURE_TIMEOUT,
                capture_bytes: EXEC_CAPTURE_BYTES,
            },
            SandboxRequestProfile {
                mounts: &mount_profile,
                executable_path: &executable_path,
            },
        );

        assert!(request.arguments.starts_with(&expected_isolation_prefix));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runner_restricted_usr_mount_recreates_standard_usr_merge_aliases()
    -> Result<(), Box<dyn Error>> {
        let workspace_root = Path::new(TEST_SANDBOX_WORKSPACE_ROOT);
        let read_only_paths = vec![ReadOnlyPathIdentity::capture(Path::new("/usr"))?];
        let executable_path = configured_sandbox_path(workspace_root, &read_only_paths);
        let mount_profile = SandboxMountProfile::RunnerRestricted {
            paths: read_only_paths,
            https_broker: None,
        };
        let expected_bin_alias = [
            OsString::from("--symlink"),
            OsString::from("usr/bin"),
            OsString::from("/bin"),
        ];
        let expected_lib64_alias = [
            OsString::from("--symlink"),
            OsString::from("usr/lib64"),
            OsString::from("/lib64"),
        ];

        let request = bwrap_request(
            isolation_fixture_context(workspace_root),
            Path::new(BWRAP_PROGRAM),
            SandboxInvocation {
                program: "test",
                arguments: &[],
                working_directory: ".",
                timeout: ISOLATION_FIXTURE_TIMEOUT,
                capture_bytes: EXEC_CAPTURE_BYTES,
            },
            SandboxRequestProfile {
                mounts: &mount_profile,
                executable_path: &executable_path,
            },
        );

        assert!(
            request
                .arguments
                .windows(expected_bin_alias.len())
                .any(|arguments| arguments == expected_bin_alias)
        );
        assert!(
            request
                .arguments
                .windows(expected_lib64_alias.len())
                .any(|arguments| arguments == expected_lib64_alias)
        );
        Ok(())
    }

    /// `/etc/resolv.conf` served outbound DNS, which needs the network that
    /// `--unshare-net` removes, so binding it would tell a later reader that
    /// egress is still expected to function here. This pins that one path and
    /// nothing broader: `/etc/hosts` and `/etc/nsswitch.conf` stay bound for
    /// loopback and NSS lookups, and `/etc/ssl` stays bound because building a
    /// TLS client reads the trust store even when no connection follows.
    #[test]
    fn sandboxed_request_omits_the_etc_resolv_conf_bind() {
        let workspace_root = Path::new(TEST_SANDBOX_WORKSPACE_ROOT);
        let mount_profile = SandboxMountProfile::DaemonLocal;
        let executable_path = sandbox_path(workspace_root);

        let request = bwrap_request(
            isolation_fixture_context(workspace_root),
            Path::new(BWRAP_PROGRAM),
            SandboxInvocation {
                program: "cargo",
                arguments: &[String::from("check")],
                working_directory: ".",
                timeout: ISOLATION_FIXTURE_TIMEOUT,
                capture_bytes: EXEC_CAPTURE_BYTES,
            },
            SandboxRequestProfile {
                mounts: &mount_profile,
                executable_path: &executable_path,
            },
        );

        assert!(
            !request
                .arguments
                .contains(&OsString::from("/etc/resolv.conf"))
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_root_request_does_not_rebind_the_workspace() -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, &workspace.path)?;
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
            .ok_or_else(|| std::io::Error::other("one requested process"))?;
        let workspace_bind_count = request
            .arguments
            .windows(3)
            .filter(|arguments| arguments[0] == "--bind" && arguments[2] == SANDBOX_WORKSPACE)
            .count();

        assert_eq!(workspace_bind_count, ROOT_WORKSPACE_BIND_COUNT);
        assert_eq!(result.stdout.text, SANDBOXED_STDOUT);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_runner_rejects_a_symlinked_working_directory_before_dispatch()
    -> Result<(), Box<dyn Error>> {
        let workspace = ReplacementWorkspace::new()?;
        let outside = ReplacementWorkspace::new()?;
        symlink(&outside.path, workspace.path.join("escape"))?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_process(b"must remain unused"),
        );
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, &workspace.path)?;
        let arguments = ExecArguments {
            program: String::from("cargo"),
            arguments: vec![String::from("check")],
            working_directory: String::from("escape"),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(result.confinement, ExecutionConfinement::SandboxSetupFailed);
        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::Other,
            }
        );
        assert_eq!(observation.recorded_requests(), Vec::new());
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

        assert!(probe.timeout > Duration::ZERO);
        assert!(probe.timeout < Duration::from_secs(REQUEST_TIMEOUT_SECONDS));
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

    #[tokio::test]
    async fn sandboxed_target_spawn_failure_remains_typed() -> Result<(), Box<dyn Error>> {
        let root = std::env::current_dir()?;
        let mut process = successful_sandbox_process(b"");
        process.outcome = ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::NotFound,
        };
        let runner = FakeRunner::returning(BwrapAvailability::Available, process);
        let mut command_runner = SandboxedCommandRunner::try_new(runner, root)?;
        let arguments = ExecArguments {
            program: String::from("missing-target"),
            arguments: Vec::new(),
            working_directory: String::from("."),
            timeout_seconds: 30,
        };

        let result = command_runner.execute(arguments).await;

        assert_eq!(result.confinement, ExecutionConfinement::FilesystemConfined);
        assert_eq!(
            result.outcome,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::NotFound,
            }
        );
        Ok(())
    }
}

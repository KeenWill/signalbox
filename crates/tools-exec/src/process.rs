#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsString,
    fmt,
    future::Future,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
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
use tokio::{io::AsyncReadExt, process::Command};

use crate::supervisor_protocol::{SupervisorSpawnFailure, SupervisorStatus};

pub const SANDBOXED_EXEC_NAME: &str = "sandboxed_exec";
pub const UNSANDBOXED_EXEC_NAME: &str = "unsandboxed_exec";
const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_PROGRAM_CHARACTERS: usize = 4096;
const MAX_PROGRAM_BYTES: usize = MAX_PROGRAM_CHARACTERS * 4;
const MAX_ARGUMENTS: usize = 128;
const MAX_ARGUMENT_CHARACTERS: usize = 4096;
const MAX_ARGUMENT_BYTES: usize = MAX_ARGUMENT_CHARACTERS * 4;
const MAX_TOTAL_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_WORKING_DIRECTORY_CHARACTERS: usize = 4096;
const MAX_WORKING_DIRECTORY_BYTES: usize = MAX_WORKING_DIRECTORY_CHARACTERS * 4;
pub(crate) const EXEC_CAPTURE_BYTES: usize = 64 * 1024;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded direct-command arguments";
const BWRAP_PROGRAM: &str = "/usr/bin/bwrap";
const SANDBOX_WORKSPACE: &str = "/workspace";
const SANDBOX_FALLBACK_PATH: &str = "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const SANDBOX_DISPATCH_MARKER: &[u8] = b"signalbox-exec:dispatched\n";
#[cfg(target_os = "linux")]
const SUPERVISOR_STATUS_TRAILER: &[u8] = b"\n\0signalbox-exec-supervisor-status:";
#[cfg(target_os = "linux")]
const SUPERVISOR_STATUS_TAIL_BYTES: usize = 1024;
const SANDBOX_DISPATCH_SHELL: &str = "program=$1; shift; case $program in */*) target=$program ;; *) target=; old_ifs=$IFS; IFS=:; for directory in $PATH; do candidate=$directory/$program; if [ -f \"$candidate\" ] && [ -x \"$candidate\" ]; then target=$candidate; break; fi; done; IFS=$old_ifs ;; esac; if [ -z \"$target\" ] || [ ! -f \"$target\" ] || [ ! -x \"$target\" ]; then exit 127; fi; printf 'signalbox-exec:dispatched\\n' >&2; exec \"$target\" \"$@\"";

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
        }
    }
}

impl Error for ExecToolConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WorkspaceRoot {
                source: Some(source),
                ..
            } => Some(source),
            Self::Name
            | Self::Schema
            | Self::ErrorDetail
            | Self::Duplicate
            | Self::WorkspaceRoot { source: None, .. } => None,
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
    ) -> Result<Self, ExecToolConstructionError> {
        Self::try_new(TokioProcessRunner, workspace_root)
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
            ToolPermissionDefault::Confirm,
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
    ) -> Result<Self, ExecToolConstructionError> {
        Self::try_new(TokioProcessRunner, workspace_root)
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
}

/// Injectable one-shot process spawning and bubblewrap probing.
pub trait ProcessRunner: Clone + Send {
    /// Probes the exact bubblewrap profile used for later execution.
    fn bwrap_availability(
        &mut self,
        probe: ProcessRequest,
    ) -> impl Future<Output = BwrapAvailability> + Send;

    /// Runs at most one process tree under the supplied finite limits.
    fn run(&mut self, request: ProcessRequest) -> impl Future<Output = ProcessRunResult> + Send;
}

/// Production Tokio process runner using an isolated Linux supervisor process.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioProcessRunner;

impl ProcessRunner for TokioProcessRunner {
    async fn bwrap_availability(&mut self, probe: ProcessRequest) -> BwrapAvailability {
        let result = run_process(probe).await;
        match result.outcome {
            ProcessOutcome::Exited { code: Some(0) } => BwrapAvailability::Available,
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::NotFound,
            } => BwrapAvailability::Missing,
            ProcessOutcome::Exited { .. }
            | ProcessOutcome::TimedOut
            | ProcessOutcome::SpawnFailed { .. }
            | ProcessOutcome::SupervisionFailed { .. } => BwrapAvailability::Unusable,
        }
    }

    async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
        run_process(request).await
    }
}

/// Sandboxed command service reusable by higher-level tools.
#[derive(Clone, Debug)]
pub struct SandboxedCommandRunner<Runner> {
    runner: Runner,
    workspace_root: PathBuf,
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
        Ok(Self {
            runner,
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
            &self.workspace_root,
            &probe_program,
            &[String::from("-c"), String::from("exit 0")],
            ".",
            Duration::from_secs(5),
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
                let request = bwrap_request(
                    &self.workspace_root,
                    &arguments.program,
                    &arguments.arguments,
                    &arguments.working_directory,
                    Duration::from_secs(arguments.timeout_seconds),
                    capture_bytes,
                );
                sandbox_process_result(self.runner.run(request).await)
            }
            BwrapAvailability::Missing | BwrapAvailability::Unusable => ExecResult {
                confinement: ExecutionConfinement::SandboxRefused { availability },
                outcome: ProcessOutcome::SpawnFailed {
                    reason: ProcessSpawnFailure::SandboxUnavailable,
                },
                stdout: OutputCapture::empty(),
                stderr: OutputCapture::empty(),
            },
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct WorkspaceIdentity {
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
impl WorkspaceIdentity {
    fn capture(path: &Path) -> Result<Self, ExecToolConstructionError> {
        let metadata =
            path.metadata()
                .map_err(|source| ExecToolConstructionError::WorkspaceRoot {
                    path: path.to_owned(),
                    source: Some(source),
                })?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn matches(self, path: &Path) -> bool {
        path.symlink_metadata().is_ok_and(|metadata| {
            metadata.file_type().is_dir()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        })
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
    workspace_root: PathBuf,
}

impl<Runner: ProcessRunner> UnsandboxedCommandRunner<Runner> {
    /// Admits one canonical injected workspace root.
    pub fn try_new(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        Ok(Self {
            runner,
            workspace_root: canonical_workspace_root(workspace_root.as_ref())?,
        })
    }
}

impl<Runner: ProcessRunner> CommandExecution for UnsandboxedCommandRunner<Runner> {
    async fn execute(&mut self, arguments: ExecArguments) -> ExecResult {
        let request = direct_request(&self.workspace_root, &arguments, EXEC_CAPTURE_BYTES);
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

fn direct_request(root: &Path, arguments: &ExecArguments, capture_bytes: usize) -> ProcessRequest {
    ProcessRequest {
        program: OsString::from(&arguments.program),
        arguments: arguments.arguments.iter().map(OsString::from).collect(),
        working_directory: root.join(&arguments.working_directory),
        timeout: Duration::from_secs(arguments.timeout_seconds),
        capture_bytes,
        environment: BTreeMap::new(),
        environment_inheritance: ProcessEnvironment::Inherit,
    }
}

fn bwrap_request(
    root: &Path,
    program: &str,
    arguments: &[String],
    working_directory: &str,
    timeout: Duration,
    capture_bytes: usize,
) -> ProcessRequest {
    let sandbox_path = sandbox_path(root);
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
    bwrap_arguments.push(root.as_os_str().to_owned());
    bwrap_arguments.push(OsString::from(SANDBOX_WORKSPACE));
    bwrap_arguments.extend([
        OsString::from("--chdir"),
        OsString::from(sandbox_directory),
        OsString::from("--setenv"),
        OsString::from("HOME"),
        OsString::from(SANDBOX_WORKSPACE),
        OsString::from("--"),
        sandbox_shell(root).into_os_string(),
        OsString::from("-c"),
        OsString::from(SANDBOX_DISPATCH_SHELL),
        OsString::from("signalbox-exec"),
        OsString::from(program),
    ]);
    bwrap_arguments.extend(arguments.iter().map(OsString::from));
    ProcessRequest {
        program: OsString::from(BWRAP_PROGRAM),
        arguments: bwrap_arguments,
        working_directory: root.to_owned(),
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
    std::env::split_paths(&sandbox_path(workspace_root))
        .map(|directory| directory.join("sh"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
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
    /// Bubblewrap was available but did not confirm target dispatch.
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
    /// No process tree was started.
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
    /// Executable lookup failed.
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
    /// Waiting for the process-group leader failed.
    Wait,
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

fn sandbox_process_result(mut result: ProcessRunResult) -> ExecResult {
    let dispatched = result.stderr.bytes.starts_with(SANDBOX_DISPATCH_MARKER)
        && !matches!(
            result.outcome,
            ProcessOutcome::Exited {
                code: Some(126 | 127)
            }
        );
    if result.stderr.bytes.starts_with(SANDBOX_DISPATCH_MARKER) {
        result.stderr.bytes.drain(..SANDBOX_DISPATCH_MARKER.len());
    }
    if dispatched {
        return process_result(ExecutionConfinement::FilesystemConfined, result);
    }
    ExecResult {
        confinement: ExecutionConfinement::SandboxSetupFailed,
        outcome: ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::SandboxSetup,
        },
        stdout: output_capture(result.stdout),
        stderr: output_capture(result.stderr),
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

#[derive(Debug)]
struct BoundedBytes {
    bytes: Vec<u8>,
    completeness: CaptureCompleteness,
}

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

async fn run_process(request: ProcessRequest) -> ProcessRunResult {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = request;
        empty_process_result(ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::ProcessTreeUnsupported,
        })
    }
    #[cfg(target_os = "linux")]
    {
        run_process_linux(request).await
    }
}

#[cfg(target_os = "linux")]
async fn run_process_linux(request: ProcessRequest) -> ProcessRunResult {
    let Some(supervisor) = supervisor_program() else {
        return empty_process_result(ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::ProcessTreeUnsupported,
        });
    };
    let timeout_milliseconds = request.timeout.as_millis().min(u128::from(u64::MAX));
    let mut command = Command::new(supervisor);
    command
        .arg(timeout_milliseconds.to_string())
        .arg(&request.program)
        .args(&request.arguments)
        .current_dir(&request.working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if request.environment_inheritance == ProcessEnvironment::Clear {
        command.env_clear();
    }
    for (name, value) in request.environment {
        command.env(name, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return empty_process_result(ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::ProcessTreeUnsupported,
            });
        }
    };
    let control = child.stdin.take();
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            drop(control);
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
            let _ = child.wait().await;
            return empty_process_result(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Stderr,
            });
        }
    };
    let stdout_task = tokio::spawn(read_supervised_stdout(stdout, request.capture_bytes));
    let stderr_task = tokio::spawn(read_bounded(stderr, request.capture_bytes));
    let outer_deadline = request.timeout.saturating_add(Duration::from_secs(2));
    let waited = tokio::time::timeout(outer_deadline, child.wait()).await;
    drop(control);
    let wait_failure = match waited {
        Ok(Ok(_)) => None,
        Ok(Err(_)) => Some(ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Wait,
        }),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Some(ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            })
        }
    };
    let stdout = stdout_task.await;
    let stderr = stderr_task.await;
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
        (Ok(Err(_)) | Err(_), _) => empty_process_result(ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Stdout,
        }),
        (_, Ok(Err(_)) | Err(_)) => empty_process_result(ProcessOutcome::SupervisionFailed {
            reason: ProcessSupervisionFailure::Stderr,
        }),
    }
}

#[cfg(target_os = "linux")]
fn supervisor_program() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let directory = current.parent()?;
    let beside_current = directory.join("signalbox-exec-supervisor");
    let beside_target = directory
        .file_name()
        .is_some_and(|name| name == "deps")
        .then(|| directory.parent())
        .flatten()
        .map(|parent| parent.join("signalbox-exec-supervisor"));
    [Some(beside_current), beside_target]
        .into_iter()
        .flatten()
        .find(|candidate| candidate.is_file())
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
        SupervisorStatus::Cancelled | SupervisorStatus::SupervisionFailed => {
            ProcessOutcome::SupervisionFailed {
                reason: ProcessSupervisionFailure::Wait,
            }
        }
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
    use std::sync::{Arc, Mutex, PoisonError};

    use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};

    use super::*;

    const SANDBOXED_STDOUT: &str = "checked";
    const SANDBOXED_WORKING_DIRECTORY: &str = "crate";

    struct ReplacementWorkspace {
        path: PathBuf,
        retired: PathBuf,
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
        results: Arc<Mutex<Vec<ProcessRunResult>>>,
        probes: Arc<Mutex<Vec<ProcessRequest>>>,
        requests: Arc<Mutex<Vec<ProcessRequest>>>,
    }

    impl FakeRunner {
        fn returning(availability: BwrapAvailability, result: ProcessRunResult) -> Self {
            Self {
                availability,
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
    }

    impl ProcessRunner for FakeRunner {
        async fn bwrap_availability(&mut self, probe: ProcessRequest) -> BwrapAvailability {
            self.probes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(probe);
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
    fn catalogs_fix_sandboxed_auto_and_unsandboxed_confirm_permissions()
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
            ToolPermissionDefault::Confirm
        );
        Ok(())
    }

    #[test]
    fn argument_schema_publishes_the_per_item_character_limit() -> Result<(), Box<dyn Error>> {
        let schema = serde_json::to_value(schemars::schema_for!(ExecArguments))?;

        assert_eq!(
            schema.pointer("/properties/arguments/items/maxLength"),
            Some(&serde_json::json!(MAX_ARGUMENT_CHARACTERS))
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
    async fn sandboxed_request_uses_bwrap_profile_and_workspace_mount() -> Result<(), Box<dyn Error>>
    {
        let root = std::env::current_dir()?;
        let runner = FakeRunner::returning(
            BwrapAvailability::Available,
            successful_sandbox_process(SANDBOXED_STDOUT.as_bytes()),
        );
        let observation = runner.clone();
        let mut command_runner = SandboxedCommandRunner::try_new(runner, &root)?;
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
            root.as_os_str().to_owned(),
            OsString::from(SANDBOX_WORKSPACE),
        ];
        let chdir_arguments = [
            OsString::from("--chdir"),
            OsString::from(format!("{SANDBOX_WORKSPACE}/{SANDBOXED_WORKING_DIRECTORY}")),
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
        assert_eq!(result.stdout.text, SANDBOXED_STDOUT);
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
                    bytes: [SANDBOX_DISPATCH_MARKER, b"target missing"].concat(),
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
}

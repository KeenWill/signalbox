//! `CargoDiagnosticsTool`: bounded whole-workspace Cargo check, clippy, and
//! test passes that report typed compiler diagnostic locations and test
//! outcomes instead of raw terminal output.
//!
//! Built on the same sandboxed `process` core as `SandboxedExecTool`; each
//! collection is labeled workspace-influenced evidence because Cargo and
//! test-body output share output channels.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledTool, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolArgumentValidator, ToolExecutionInvocation, ToolExecutor,
    ToolExecutorEvidence,
};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
    ToolResultText, ToolResultTextFailure,
};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

#[cfg(test)]
use crate::process::{BWRAP_PROGRAM, SANDBOX_DISPATCH_MARKER};
use crate::{
    CaptureCompleteness, ExecArguments, ExecResult, ExecToolConstructionError,
    ExecutionConfinement, OutputEncoding, ProcessOutcome, ProcessRunner, SandboxedCommandRunner,
    TokioProcessRunner,
};

/// Catalog name for structured Cargo diagnostics.
pub const CARGO_DIAGNOSTICS_NAME: &str = "cargo_diagnostics";

const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const DIAGNOSTICS_CAPTURE_BYTES: usize = 512 * 1024;
const CARGO_HOST_CAPTURE_BYTES: usize = 16 * 1024;
const MAX_CARGO_HOST_BYTES: usize = 256;
const MAX_DIAGNOSTICS: usize = 64;
const MAX_TESTS: usize = 512;
const MAX_FILE_BYTES: usize = 4096;
const MAX_LEVEL_BYTES: usize = 64;
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_TEST_NAME_BYTES: usize = 1024;
const MAX_CARGO_FAILURE_BYTES: usize = 8192;
const MAX_TEST_EXECUTABLE_BYTES: usize = 4096;
const CARGO_TEST_RUNNER: &str = "['/signalbox-exec-dispatch','--cargo-test-runner']";
const MAX_CARGO_CONFIG_BYTES: u64 = 64 * 1024;
const INVALID_ARGUMENTS_DETAIL: &str = "invalid bounded cargo-diagnostics arguments";

fn default_timeout_seconds() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

/// Supported whole-workspace diagnostic passes.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CargoDiagnosticsCommand {
    /// Runs `cargo check` for all workspace targets and features.
    Check,
    /// Runs `cargo clippy` for all workspace targets and features with warnings denied.
    Clippy,
    /// Runs `cargo test` for all workspace targets and features without fail-fast.
    Test,
}

/// Arguments for one structured Cargo diagnostic pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CargoDiagnosticsArguments {
    /// Cargo pass to run in the injected workspace root.
    pub command: CargoDiagnosticsCommand,
    /// Whole-process timeout in seconds, from 1 through 300.
    #[serde(default = "default_timeout_seconds")]
    #[schemars(range(min = 1, max = MAX_TIMEOUT_SECONDS))]
    pub timeout_seconds: u64,
}

struct CargoDiagnosticsContract;

impl ToolContract for CargoDiagnosticsContract {
    type Arguments = CargoDiagnosticsArguments;
    const NAME: &'static str = CARGO_DIAGNOSTICS_NAME;
    const DESCRIPTION: &'static str = "Runs a bounded sandboxed Cargo check, clippy, or test pass and returns structured diagnostics.";
}

/// Why Cargo diagnostics tool construction failed.
#[derive(Debug)]
pub enum CargoDiagnosticsToolConstructionError {
    /// The sandboxed execution core failed construction.
    Exec(ExecToolConstructionError),
    /// The static tool name was rejected.
    Name,
    /// The static argument schema was rejected.
    Schema,
    /// The static validation detail was rejected.
    ErrorDetail,
    /// The one-entry catalog unexpectedly reported a duplicate.
    Duplicate,
}

impl fmt::Display for CargoDiagnosticsToolConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exec(source) => source.fmt(formatter),
            Self::Name => formatter.write_str("cargo diagnostics static name is invalid"),
            Self::Schema => formatter.write_str("cargo diagnostics static schema is invalid"),
            Self::ErrorDetail => {
                formatter.write_str("cargo diagnostics static error detail is invalid")
            }
            Self::Duplicate => formatter.write_str("cargo diagnostics catalog is duplicated"),
        }
    }
}

impl Error for CargoDiagnosticsToolConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Exec(source) => Some(source),
            Self::Name | Self::Schema | Self::ErrorDetail | Self::Duplicate => None,
        }
    }
}

impl From<ExecToolConstructionError> for CargoDiagnosticsToolConstructionError {
    fn from(value: ExecToolConstructionError) -> Self {
        Self::Exec(value)
    }
}

/// One sandboxed Cargo-diagnostics catalog entry and matching executor.
#[derive(Clone, Debug)]
pub struct CargoDiagnosticsTool<Runner> {
    catalog: CompiledToolCatalog,
    executor: CargoDiagnosticsExecutor<Runner>,
}

impl<Runner: ProcessRunner> CargoDiagnosticsTool<Runner> {
    /// Compiles the diagnostics tool around an injected runner and workspace.
    pub fn try_new(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, CargoDiagnosticsToolConstructionError> {
        let command_runner = SandboxedCommandRunner::try_new(runner, workspace_root)?;
        let detail = ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
            .map_err(|_| CargoDiagnosticsToolConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<CargoDiagnosticsContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => CargoDiagnosticsToolConstructionError::Name,
            ToolContractCompileError::Schema => CargoDiagnosticsToolConstructionError::Schema,
        })?;
        let compiled = CompiledTool::new(definition, CargoDiagnosticsValidator { detail });
        let catalog = CompiledToolCatalog::try_new([compiled])
            .map_err(|_| CargoDiagnosticsToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: CargoDiagnosticsExecutor {
                runner: CargoDiagnosticsRunner { command_runner },
            },
        })
    }

    /// Compiles diagnostics with one pinned read-only Cargo registry cache.
    pub fn try_new_with_cargo_registry(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
        cargo_registry: impl AsRef<Path>,
    ) -> Result<Self, CargoDiagnosticsToolConstructionError> {
        let command_runner = SandboxedCommandRunner::try_new_with_cargo_registry(
            runner,
            workspace_root,
            cargo_registry,
        )?;
        let detail = ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
            .map_err(|_| CargoDiagnosticsToolConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<CargoDiagnosticsContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => CargoDiagnosticsToolConstructionError::Name,
            ToolContractCompileError::Schema => CargoDiagnosticsToolConstructionError::Schema,
        })?;
        let compiled = CompiledTool::new(definition, CargoDiagnosticsValidator { detail });
        let catalog = CompiledToolCatalog::try_new([compiled])
            .map_err(|_| CargoDiagnosticsToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: CargoDiagnosticsExecutor {
                runner: CargoDiagnosticsRunner { command_runner },
            },
        })
    }

    /// Returns separate catalog and executor composition roles.
    pub fn into_parts(self) -> (CompiledToolCatalog, CargoDiagnosticsExecutor<Runner>) {
        (self.catalog, self.executor)
    }
}

impl CargoDiagnosticsTool<TokioProcessRunner> {
    /// Builds the production sandboxed diagnostics tool.
    pub fn try_new_production(
        workspace_root: impl AsRef<Path>,
        supervisor_program: impl AsRef<Path>,
    ) -> Result<Self, CargoDiagnosticsToolConstructionError> {
        Self::try_new(
            TokioProcessRunner::try_new(supervisor_program)?,
            workspace_root,
        )
    }
}

#[derive(Clone, Debug)]
struct CargoDiagnosticsValidator {
    detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for CargoDiagnosticsValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(arguments)
            .map(drop)
            .map_err(|_| self.detail.clone())
    }
}

/// Cargo diagnostics arguments violated a finite bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCargoDiagnosticsArguments;

impl fmt::Display for InvalidCargoDiagnosticsArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(INVALID_ARGUMENTS_DETAIL)
    }
}

impl Error for InvalidCargoDiagnosticsArguments {}

fn decode_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<CargoDiagnosticsArguments, InvalidCargoDiagnosticsArguments> {
    let decoded: CargoDiagnosticsArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidCargoDiagnosticsArguments)?;
    validate_arguments(decoded)?;
    Ok(decoded)
}

fn validate_arguments(
    arguments: CargoDiagnosticsArguments,
) -> Result<(), InvalidCargoDiagnosticsArguments> {
    if arguments.timeout_seconds == 0 || arguments.timeout_seconds > MAX_TIMEOUT_SECONDS {
        return Err(InvalidCargoDiagnosticsArguments);
    }
    Ok(())
}

/// Executes the compiled Cargo-diagnostics catalog entry.
#[derive(Clone, Debug)]
pub struct CargoDiagnosticsExecutor<Runner> {
    runner: CargoDiagnosticsRunner<Runner>,
}

/// A checked catalog/executor assumption failed inside diagnostics execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CargoDiagnosticsExecutorError {
    /// Executor argument decoding disagreed with catalog validation.
    ArgumentValidationDrift,
    /// Compact structured result encoding unexpectedly failed.
    ResultEncoding,
}

impl fmt::Display for CargoDiagnosticsExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ArgumentValidationDrift => "cargo diagnostics argument validation drifted",
            Self::ResultEncoding => "cargo diagnostics result encoding failed",
        })
    }
}

impl Error for CargoDiagnosticsExecutorError {}

impl ClassifyOperatorFailure for CargoDiagnosticsExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::CallerOrHubBug
    }
}

impl<Runner: ProcessRunner> ToolExecutor for CargoDiagnosticsExecutor<Runner> {
    type Error = CargoDiagnosticsExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        let arguments = decode_arguments(invocation.request().arguments())
            .map_err(|_| CargoDiagnosticsExecutorError::ArgumentValidationDrift)?;
        let result = self.runner.run_validated(arguments).await;
        let encoded = encode_tool_result(result)?;
        Ok(invocation.bind(ToolExecutorEvidence::CompletedText(encoded)))
    }
}

/// Reusable structured diagnostics service over the sandboxed exec core.
#[derive(Clone, Debug)]
pub struct CargoDiagnosticsRunner<Runner> {
    command_runner: SandboxedCommandRunner<Runner>,
}

impl<Runner: ProcessRunner> CargoDiagnosticsRunner<Runner> {
    /// Admits an injected process runner and workspace root.
    pub fn try_new(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        Ok(Self {
            command_runner: SandboxedCommandRunner::try_new(runner, workspace_root)?,
        })
    }

    /// Admits a workspace plus one pinned read-only Cargo registry cache.
    pub fn try_new_with_cargo_registry(
        runner: Runner,
        workspace_root: impl AsRef<Path>,
        cargo_registry: impl AsRef<Path>,
    ) -> Result<Self, ExecToolConstructionError> {
        Ok(Self {
            command_runner: SandboxedCommandRunner::try_new_with_cargo_registry(
                runner,
                workspace_root,
                cargo_registry,
            )?,
        })
    }

    /// Validates and executes one structured Cargo pass.
    pub async fn try_run(
        &mut self,
        arguments: CargoDiagnosticsArguments,
    ) -> Result<CargoDiagnosticsResult, InvalidCargoDiagnosticsArguments> {
        validate_arguments(arguments)?;
        Ok(self.run_validated(arguments).await)
    }

    async fn run_validated(
        &mut self,
        arguments: CargoDiagnosticsArguments,
    ) -> CargoDiagnosticsResult {
        let command = arguments.command;
        if command == CargoDiagnosticsCommand::Test {
            return self.run_test(arguments.timeout_seconds).await;
        }
        let exec_arguments = cargo_arguments(
            command,
            arguments.timeout_seconds,
            CargoTestRunnerMode::ConfiguredRunnerPreserved,
            "",
        );
        let result = self
            .command_runner
            .run_with_capture(exec_arguments, DIAGNOSTICS_CAPTURE_BYTES)
            .await;
        structured_result_with_test_runner_mode(
            command,
            result,
            CargoTestRunnerMode::ConfiguredRunnerPreserved,
        )
    }

    async fn run_test(&mut self, timeout_seconds: u64) -> CargoDiagnosticsResult {
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        let host_result = self
            .command_runner
            .run_with_capture(
                cargo_host_arguments(timeout_seconds),
                CARGO_HOST_CAPTURE_BYTES,
            )
            .await;
        let Some(cargo_host) = cargo_host(&host_result).map(String::from) else {
            let mut result = structured_result_with_test_runner_mode(
                CargoDiagnosticsCommand::Test,
                host_result,
                CargoTestRunnerMode::ConfiguredRunnerPreserved,
            );
            result.execution.preparation_failure =
                Some(CargoDiagnosticsPreparationFailure::CargoHostUnavailable);
            return result;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let mut timed_out = host_result;
            timed_out.outcome = ProcessOutcome::TimedOut;
            return structured_result_with_test_runner_mode(
                CargoDiagnosticsCommand::Test,
                timed_out,
                CargoTestRunnerMode::ConfiguredRunnerPreserved,
            );
        }
        let workspace_root = self.command_runner.pinned_workspace_root().to_owned();
        let Some(runner_plan) =
            workspace_test_runner_plan_before_deadline(workspace_root, cargo_host, remaining).await
        else {
            let mut timed_out = host_result;
            timed_out.outcome = ProcessOutcome::TimedOut;
            return structured_result_with_test_runner_mode(
                CargoDiagnosticsCommand::Test,
                timed_out,
                CargoTestRunnerMode::ConfiguredRunnerPreserved,
            );
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let mut timed_out = host_result;
            timed_out.outcome = ProcessOutcome::TimedOut;
            return structured_result_with_test_runner_mode(
                CargoDiagnosticsCommand::Test,
                timed_out,
                CargoTestRunnerMode::ConfiguredRunnerPreserved,
            );
        }
        let exec_arguments = cargo_arguments(
            CargoDiagnosticsCommand::Test,
            timeout_seconds,
            runner_plan.mode,
            &runner_plan.selected_target,
        );
        let result = self
            .command_runner
            .run_with_capture_timeout(exec_arguments, remaining, DIAGNOSTICS_CAPTURE_BYTES)
            .await;
        structured_result_with_test_runner_mode(
            CargoDiagnosticsCommand::Test,
            result,
            runner_plan.mode,
        )
    }
}

async fn workspace_test_runner_plan_before_deadline(
    workspace_root: PathBuf,
    cargo_host: String,
    remaining: Duration,
) -> Option<CargoTestRunnerPlan> {
    cargo_config_inspection_before_deadline(
        move || workspace_test_runner_plan(&workspace_root, &cargo_host),
        remaining,
    )
    .await
}

async fn cargo_config_inspection_before_deadline<Inspect>(
    inspect: Inspect,
    remaining: Duration,
) -> Option<CargoTestRunnerPlan>
where
    Inspect: FnOnce() -> CargoTestRunnerPlan + Send + 'static,
{
    tokio::time::timeout(remaining, tokio::task::spawn_blocking(inspect))
        .await
        .ok()?
        .ok()
}

fn cargo_host_arguments(timeout_seconds: u64) -> ExecArguments {
    ExecArguments {
        program: String::from("cargo"),
        arguments: vec![String::from("-vV")],
        working_directory: String::from("."),
        timeout_seconds,
    }
}

fn cargo_host(result: &ExecResult) -> Option<&str> {
    if result.confinement != ExecutionConfinement::FilesystemConfined
        || result.outcome != (ProcessOutcome::Exited { code: Some(0) })
        || result.stdout.completeness != CaptureCompleteness::Complete
        || result.stdout.encoding != OutputEncoding::Utf8
    {
        return None;
    }
    result.stdout.text.lines().find_map(|line| {
        line.strip_prefix("host: ").filter(|host| {
            !host.is_empty()
                && host.len() <= MAX_CARGO_HOST_BYTES
                && host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    })
}

fn cargo_arguments(
    command: CargoDiagnosticsCommand,
    timeout_seconds: u64,
    test_runner_mode: CargoTestRunnerMode,
    native_target: &str,
) -> ExecArguments {
    let arguments = match command {
        CargoDiagnosticsCommand::Check => [
            "check",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--message-format=json",
        ]
        .map(String::from)
        .to_vec(),
        CargoDiagnosticsCommand::Clippy => [
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--message-format=json",
            "--",
            "-D",
            "warnings",
        ]
        .map(String::from)
        .to_vec(),
        CargoDiagnosticsCommand::Test => cargo_test_arguments(test_runner_mode, native_target),
    };
    ExecArguments {
        program: String::from("cargo"),
        arguments,
        working_directory: String::from("."),
        timeout_seconds,
    }
}

fn cargo_test_arguments(test_runner_mode: CargoTestRunnerMode, native_target: &str) -> Vec<String> {
    let mut arguments = vec![
        String::from("test"),
        String::from("--config"),
        String::from("term.quiet=false"),
    ];
    if test_runner_mode == CargoTestRunnerMode::HelperInstalled {
        arguments.push(String::from("--config"));
        arguments.push(cargo_test_runner_config(native_target));
    }
    arguments.extend(
        [
            "--no-fail-fast",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--message-format=json",
        ]
        .map(String::from),
    );
    arguments
}

#[derive(Debug, Eq, PartialEq)]
struct CargoTestRunnerPlan {
    selected_target: String,
    mode: CargoTestRunnerMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CargoTestRunnerMode {
    HelperInstalled,
    ConfiguredRunnerPreserved,
}

enum CargoConfigRead {
    Absent,
    Contents(String),
    Opaque,
}

fn workspace_test_runner_plan(workspace_root: &Path, cargo_host: &str) -> CargoTestRunnerPlan {
    match read_workspace_cargo_config(workspace_root) {
        CargoConfigRead::Absent => CargoTestRunnerPlan {
            selected_target: String::from(cargo_host),
            mode: CargoTestRunnerMode::HelperInstalled,
        },
        CargoConfigRead::Contents(contents) => config_text_test_runner_plan(&contents, cargo_host)
            .unwrap_or_else(|| CargoTestRunnerPlan {
                selected_target: String::from(cargo_host),
                mode: CargoTestRunnerMode::ConfiguredRunnerPreserved,
            }),
        CargoConfigRead::Opaque => CargoTestRunnerPlan {
            selected_target: String::from(cargo_host),
            mode: CargoTestRunnerMode::ConfiguredRunnerPreserved,
        },
    }
}

#[cfg(target_os = "linux")]
fn read_workspace_cargo_config(workspace_root: &Path) -> CargoConfigRead {
    let root = match rustix::fs::open(
        workspace_root,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(root) => root,
        Err(_) => return CargoConfigRead::Opaque,
    };
    let cargo_directory = match rustix::fs::openat(
        &root,
        ".cargo",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT) => return CargoConfigRead::Absent,
        Err(_) => return CargoConfigRead::Opaque,
    };
    match read_cargo_config_file(&cargo_directory, "config") {
        CargoConfigRead::Absent => read_cargo_config_file(&cargo_directory, "config.toml"),
        selected => selected,
    }
}

#[cfg(target_os = "linux")]
fn read_cargo_config_file(cargo_directory: &rustix::fd::OwnedFd, name: &str) -> CargoConfigRead {
    let descriptor = match rustix::fs::openat(
        cargo_directory,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return CargoConfigRead::Absent,
        Err(_) => return CargoConfigRead::Opaque,
    };
    let metadata = match rustix::fs::fstat(&descriptor) {
        Ok(metadata) => metadata,
        Err(_) => return CargoConfigRead::Opaque,
    };
    if rustix::fs::FileType::from_raw_mode(metadata.st_mode) != rustix::fs::FileType::RegularFile
        || metadata.st_size < 0
        || metadata.st_size as u64 > MAX_CARGO_CONFIG_BYTES
    {
        return CargoConfigRead::Opaque;
    }
    read_cargo_config_contents(File::from(descriptor))
}

#[cfg(not(target_os = "linux"))]
fn read_workspace_cargo_config(_workspace_root: &Path) -> CargoConfigRead {
    CargoConfigRead::Opaque
}

fn read_cargo_config_contents(file: File) -> CargoConfigRead {
    let mut contents = String::new();
    if file
        .take(MAX_CARGO_CONFIG_BYTES + 1)
        .read_to_string(&mut contents)
        .is_err()
        || contents.len() as u64 > MAX_CARGO_CONFIG_BYTES
    {
        return CargoConfigRead::Opaque;
    }
    CargoConfigRead::Contents(contents)
}

fn config_text_test_runner_plan(contents: &str, cargo_host: &str) -> Option<CargoTestRunnerPlan> {
    let config = toml::from_str::<toml::Value>(contents).ok()?;
    if config.get("include").is_some() {
        return None;
    }
    let selected_target = match config.get("build").and_then(|build| build.get("target")) {
        Some(target) => target.as_str()?.to_owned(),
        None => String::from(cargo_host),
    };
    if selected_target.is_empty()
        || selected_target.len() > MAX_CARGO_HOST_BYTES
        || !selected_target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    let configured_runner_present = config
        .get("target")
        .and_then(toml::Value::as_table)
        .is_some_and(|targets| {
            targets.iter().any(|(selector, settings)| {
                (selector == &selected_target || selector.starts_with("cfg("))
                    && settings.get("runner").is_some()
            })
        });
    let mode = if configured_runner_present {
        CargoTestRunnerMode::ConfiguredRunnerPreserved
    } else {
        CargoTestRunnerMode::HelperInstalled
    };
    Some(CargoTestRunnerPlan {
        selected_target,
        mode,
    })
}

fn cargo_test_runner_config(native_target: &str) -> String {
    format!("target.'{native_target}'.runner={CARGO_TEST_RUNNER}")
}

/// Structured, bounded result from one Cargo diagnostic pass.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoDiagnosticsResult {
    /// Cargo pass that was requested.
    pub command: CargoDiagnosticsCommand,
    /// Sandboxing and terminal process evidence.
    pub execution: CargoDiagnosticsExecution,
    /// Bounded compiler diagnostics parsed from Cargo JSON messages.
    pub diagnostics: CargoDiagnosticRecords,
    /// Bounded test outcomes parsed from libtest output.
    pub tests: CargoTestRecords,
}

/// Sandboxing, process, and underlying capture evidence.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoDiagnosticsExecution {
    /// Filesystem posture actually selected.
    pub confinement: ExecutionConfinement,
    /// Terminal process-tree outcome, including timeout or refusal.
    pub outcome: ProcessOutcome,
    /// Standard output retention and encoding evidence.
    pub stdout: CargoDiagnosticsStream,
    /// Standard error retention and encoding evidence.
    pub stderr: CargoDiagnosticsStream,
    /// Bounded Cargo-level failure text when no structured compiler diagnostic explained failure.
    pub cargo_failure: Option<CargoFailureDetail>,
    /// Typed failure of diagnostics preparation before the requested Cargo pass.
    pub preparation_failure: Option<CargoDiagnosticsPreparationFailure>,
}

/// Why diagnostics preparation could not reach the requested Cargo pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CargoDiagnosticsPreparationFailure {
    /// Runtime Cargo did not provide one complete, valid host triple.
    CargoHostUnavailable,
}

/// Bounded text explaining a Cargo-level failure before structured diagnostics were available.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoFailureDetail {
    /// Retained Cargo failure text.
    pub message: String,
    /// Whether all Cargo failure text was retained.
    pub message_completeness: CaptureCompleteness,
}

/// Evidence about one consumed underlying process stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoDiagnosticsStream {
    /// Whether all emitted bytes were retained for parsing.
    pub completeness: CaptureCompleteness,
    /// Whether retained bytes were losslessly represented as UTF-8.
    pub encoding: OutputEncoding,
}

/// Bounded compiler-diagnostic collection and completeness evidence.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoDiagnosticRecords {
    /// Parsed diagnostic prefix, capped at 64 records.
    pub values: Vec<CargoDiagnostic>,
    /// Whether additional parsed diagnostics were omitted by the record cap.
    pub limit_reached: bool,
    /// Provenance of every parsed frame in this collection.
    pub provenance: CargoEvidenceProvenance,
    /// Whether truncation is known; false does not establish completeness.
    pub known_truncated: bool,
}

/// Provenance shared by Cargo and helper frames decoded from the mixed output channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CargoEvidenceProvenance {
    /// Workspace build or test code can add, reorder, or suppress apparent frames.
    WorkspaceInfluenced,
}

/// One bounded compiler diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoDiagnostic {
    /// Primary source file, when Cargo supplied a span.
    pub file: Option<String>,
    /// Whether the complete source-file text was retained.
    pub file_completeness: CaptureCompleteness,
    /// Primary source span, when Cargo supplied one.
    pub span: Option<CargoDiagnosticSpan>,
    /// Compiler diagnostic level.
    pub level: String,
    /// Whether the complete level text was retained.
    pub level_completeness: CaptureCompleteness,
    /// Human-readable compiler message without rendered terminal decoration.
    pub message: String,
    /// Whether the complete message text was retained.
    pub message_completeness: CaptureCompleteness,
}

/// One source span reported by the compiler.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoDiagnosticSpan {
    /// One-based first line.
    pub line_start: u64,
    /// One-based first column.
    pub column_start: u64,
    /// One-based final line.
    pub line_end: u64,
    /// One-based final column.
    pub column_end: u64,
}

/// Bounded test-outcome collection and completeness evidence.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoTestRecords {
    /// Parsed test-outcome observation prefix, capped at 512 records.
    pub values: Vec<CargoTestResult>,
    /// Whether additional parsed test outcomes were omitted by the record cap.
    pub limit_reached: bool,
    /// Provenance of every parsed frame in this collection.
    pub provenance: CargoEvidenceProvenance,
    /// Whether truncation is known; false does not establish completeness.
    pub known_truncated: bool,
}

/// One bounded libtest outcome.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CargoTestResult {
    /// Exact Cargo-invoked test executable, which disambiguates equal names across targets.
    pub executable: String,
    /// Whether the complete executable identity was retained.
    pub executable_completeness: CaptureCompleteness,
    /// Fully qualified test name within that executable when it fit the field bound.
    pub name: String,
    /// Whether the complete test name was retained.
    pub name_completeness: CaptureCompleteness,
    /// Outcome reported by the one-pass libtest observation source.
    pub outcome: CargoTestOutcome,
}

/// Closed libtest outcomes represented by pretty output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CargoTestOutcome {
    /// The test passed.
    Passed,
    /// The test failed.
    Failed,
    /// The test was ignored.
    Ignored,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoTestEvent {
    reason: String,
    executable: String,
    name: String,
    outcome: CargoTestOutcome,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoTestSourceTruncated {
    reason: String,
    executable: String,
}

#[derive(serde::Deserialize)]
struct CargoTestLimitReached {
    reason: String,
    executable: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoTestSourceComplete {
    reason: String,
    executable: String,
}

#[derive(serde::Deserialize)]
struct CargoMessage {
    reason: String,
    message: Option<RustcMessage>,
}

#[derive(serde::Deserialize)]
struct CargoArtifact {
    reason: String,
    executable: Option<String>,
    profile: CargoArtifactProfile,
}

#[derive(serde::Deserialize)]
struct CargoArtifactProfile {
    test: bool,
}

#[derive(serde::Deserialize)]
struct RustcMessage {
    level: String,
    message: String,
    spans: Vec<RustcSpan>,
}

#[derive(Clone, serde::Deserialize)]
struct RustcSpan {
    file_name: String,
    line_start: u64,
    line_end: u64,
    column_start: u64,
    column_end: u64,
    is_primary: bool,
}

#[cfg(test)]
fn structured_result(
    command: CargoDiagnosticsCommand,
    result: ExecResult,
) -> CargoDiagnosticsResult {
    structured_result_with_test_runner_mode(command, result, CargoTestRunnerMode::HelperInstalled)
}

fn structured_result_with_test_runner_mode(
    command: CargoDiagnosticsCommand,
    result: ExecResult,
    test_runner_mode: CargoTestRunnerMode,
) -> CargoDiagnosticsResult {
    let mut known_truncated = result.stdout.completeness == CaptureCompleteness::Truncated;
    let mut test_known_truncated = command == CargoDiagnosticsCommand::Test && known_truncated;
    let mut diagnostics = Vec::new();
    let mut tests = Vec::new();
    let mut diagnostic_limit_reached = false;
    let mut test_limit_reached = false;
    let mut expected_test_executables = BTreeSet::new();
    let mut completed_test_executables = BTreeSet::new();
    let cargo_build = parse_stream(
        &result.stdout.text,
        command,
        &mut diagnostics,
        &mut diagnostic_limit_reached,
        &mut tests,
        &mut test_limit_reached,
        TestSourceEvidence {
            truncated: &mut test_known_truncated,
            expected_executables: &mut expected_test_executables,
            completed_executables: &mut completed_test_executables,
            runner_mode: test_runner_mode,
        },
    );
    if !cargo_build.finished {
        known_truncated = true;
    }
    if command == CargoDiagnosticsCommand::Test
        && (test_runner_mode == CargoTestRunnerMode::ConfiguredRunnerPreserved
            || !cargo_build.succeeded
            || !expected_test_executables.is_subset(&completed_test_executables))
    {
        test_known_truncated = true;
    }
    let has_error_diagnostic = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == "error");
    let cargo_failure = cargo_failure_detail(&result, has_error_diagnostic);
    CargoDiagnosticsResult {
        command,
        execution: CargoDiagnosticsExecution {
            confinement: result.confinement,
            outcome: result.outcome,
            stdout: CargoDiagnosticsStream {
                completeness: result.stdout.completeness,
                encoding: result.stdout.encoding,
            },
            stderr: CargoDiagnosticsStream {
                completeness: result.stderr.completeness,
                encoding: result.stderr.encoding,
            },
            cargo_failure,
            preparation_failure: None,
        },
        diagnostics: CargoDiagnosticRecords {
            values: diagnostics,
            limit_reached: diagnostic_limit_reached,
            provenance: CargoEvidenceProvenance::WorkspaceInfluenced,
            known_truncated,
        },
        tests: CargoTestRecords {
            values: tests,
            limit_reached: test_limit_reached,
            provenance: CargoEvidenceProvenance::WorkspaceInfluenced,
            known_truncated: test_known_truncated,
        },
    }
}

fn cargo_failure_detail(
    result: &ExecResult,
    has_error_diagnostic: bool,
) -> Option<CargoFailureDetail> {
    let cargo_exited_unsuccessfully = matches!(
        result.outcome,
        ProcessOutcome::Exited { code } if code != Some(0)
    );
    let message = result.stderr.text.trim();
    if result.confinement != ExecutionConfinement::FilesystemConfined
        || !cargo_exited_unsuccessfully
        || has_error_diagnostic
        || message.is_empty()
    {
        return None;
    }
    let (message, field_completeness) = bounded_text(message, MAX_CARGO_FAILURE_BYTES);
    let message_completeness = if result.stderr.completeness == CaptureCompleteness::Truncated
        || result.stderr.encoding == OutputEncoding::LossyUtf8
        || field_completeness == CaptureCompleteness::Truncated
    {
        CaptureCompleteness::Truncated
    } else {
        CaptureCompleteness::Complete
    };
    Some(CargoFailureDetail {
        message,
        message_completeness,
    })
}

fn encode_tool_result(
    mut result: CargoDiagnosticsResult,
) -> Result<String, CargoDiagnosticsExecutorError> {
    if let Some(encoded) = admitted_encoding(&result)? {
        return Ok(encoded);
    }
    let tests = std::mem::take(&mut result.tests.values);
    result.tests.limit_reached |= !tests.is_empty();
    if let Some(encoded_without_tests) = admitted_encoding(&result)? {
        return largest_admitted_test_prefix(result, tests, encoded_without_tests);
    }
    let diagnostics = std::mem::take(&mut result.diagnostics.values);
    result.diagnostics.limit_reached |= !diagnostics.is_empty();
    let encoded_without_records =
        admitted_encoding(&result)?.ok_or(CargoDiagnosticsExecutorError::ResultEncoding)?;
    largest_admitted_diagnostic_prefix(result, diagnostics, encoded_without_records)
}

fn admitted_encoding(
    result: &CargoDiagnosticsResult,
) -> Result<Option<String>, CargoDiagnosticsExecutorError> {
    let encoded =
        serde_json::to_string(result).map_err(|_| CargoDiagnosticsExecutorError::ResultEncoding)?;
    match ToolResultText::try_new(encoded) {
        Ok(admitted) => Ok(Some(admitted.into_string())),
        Err(error) => match error.failure() {
            ToolResultTextFailure::TooLarge { .. } => Ok(None),
            ToolResultTextFailure::ContainsNull => {
                Err(CargoDiagnosticsExecutorError::ResultEncoding)
            }
        },
    }
}

fn largest_admitted_test_prefix(
    mut result: CargoDiagnosticsResult,
    values: Vec<CargoTestResult>,
    mut best_encoding: String,
) -> Result<String, CargoDiagnosticsExecutorError> {
    let mut admitted = 0;
    let mut rejected = values.len() + 1;
    while admitted + 1 < rejected {
        let candidate = admitted + (rejected - admitted) / 2;
        result.tests.values = values[..candidate].to_vec();
        if let Some(encoded) = admitted_encoding(&result)? {
            admitted = candidate;
            best_encoding = encoded;
        } else {
            rejected = candidate;
        }
    }
    Ok(best_encoding)
}

fn largest_admitted_diagnostic_prefix(
    mut result: CargoDiagnosticsResult,
    values: Vec<CargoDiagnostic>,
    mut best_encoding: String,
) -> Result<String, CargoDiagnosticsExecutorError> {
    let mut admitted = 0;
    let mut rejected = values.len() + 1;
    while admitted + 1 < rejected {
        let candidate = admitted + (rejected - admitted) / 2;
        result.diagnostics.values = values[..candidate].to_vec();
        if let Some(encoded) = admitted_encoding(&result)? {
            admitted = candidate;
            best_encoding = encoded;
        } else {
            rejected = candidate;
        }
    }
    Ok(best_encoding)
}

fn parse_stream(
    text: &str,
    command: CargoDiagnosticsCommand,
    diagnostics: &mut Vec<CargoDiagnostic>,
    diagnostic_limit_reached: &mut bool,
    tests: &mut Vec<CargoTestResult>,
    test_limit_reached: &mut bool,
    test_source_evidence: TestSourceEvidence<'_>,
) -> CargoBuildEvidence {
    let mut build_finished = false;
    let mut build_succeeded = false;
    for line in text.lines() {
        if command == CargoDiagnosticsCommand::Test
            && !build_finished
            && let Some(executable) = cargo_test_artifact(line)
        {
            test_source_evidence.expected_executables.insert(executable);
        }
        if (command != CargoDiagnosticsCommand::Test || !build_finished)
            && let Some(diagnostic) = parse_diagnostic(line)
        {
            push_bounded(
                diagnostics,
                diagnostic,
                MAX_DIAGNOSTICS,
                diagnostic_limit_reached,
            );
        }
        if let Some(success) = cargo_build_finished(line) {
            build_finished = true;
            build_succeeded = success;
        }
        if command == CargoDiagnosticsCommand::Test
            && build_finished
            && test_source_evidence.runner_mode == CargoTestRunnerMode::HelperInstalled
        {
            if parse_test_source_truncated(line).is_some() {
                *test_source_evidence.truncated = true;
            } else if parse_test_limit_reached(line).is_some() {
                *test_limit_reached = true;
            } else if let Some(executable) = parse_test_source_complete(line) {
                test_source_evidence
                    .completed_executables
                    .insert(executable);
            } else if let Some(test) = parse_test_event(line) {
                push_bounded(tests, test, MAX_TESTS, test_limit_reached);
            }
        }
    }
    CargoBuildEvidence {
        finished: build_finished,
        succeeded: build_finished && build_succeeded,
    }
}

struct CargoBuildEvidence {
    finished: bool,
    succeeded: bool,
}

struct TestSourceEvidence<'a> {
    truncated: &'a mut bool,
    expected_executables: &'a mut BTreeSet<String>,
    completed_executables: &'a mut BTreeSet<String>,
    runner_mode: CargoTestRunnerMode,
}

fn cargo_build_finished(line: &str) -> Option<bool> {
    serde_json::from_str::<CargoReason>(line)
        .ok()
        .filter(|message| message.reason == "build-finished")
        .map(|message| message.success)
}

fn cargo_test_artifact(line: &str) -> Option<String> {
    let artifact = serde_json::from_str::<CargoArtifact>(line).ok()?;
    (artifact.reason == "compiler-artifact" && artifact.profile.test)
        .then_some(artifact.executable)
        .flatten()
}

fn parse_test_source_complete(line: &str) -> Option<String> {
    let event = serde_json::from_str::<CargoTestSourceComplete>(line).ok()?;
    (event.reason == "signalbox-test-source-complete").then_some(event.executable)
}

#[derive(serde::Deserialize)]
struct CargoReason {
    reason: String,
    success: bool,
}

fn push_bounded<T>(values: &mut Vec<T>, value: T, limit: usize, limit_reached: &mut bool) {
    if values.len() < limit {
        values.push(value);
    } else {
        *limit_reached = true;
    }
}

fn parse_diagnostic(line: &str) -> Option<CargoDiagnostic> {
    let parsed: CargoMessage = serde_json::from_str(line).ok()?;
    if parsed.reason != "compiler-message" {
        return None;
    }
    let message = parsed.message?;
    let primary = message
        .spans
        .iter()
        .find(|span| span.is_primary)
        .or_else(|| message.spans.first())
        .cloned();
    let (file, file_completeness, span) =
        primary.map_or((None, CaptureCompleteness::Complete, None), |primary| {
            let (file, completeness) = bounded_text(&primary.file_name, MAX_FILE_BYTES);
            (
                Some(file),
                completeness,
                Some(CargoDiagnosticSpan {
                    line_start: primary.line_start,
                    column_start: primary.column_start,
                    line_end: primary.line_end,
                    column_end: primary.column_end,
                }),
            )
        });
    let (level, level_completeness) = bounded_text(&message.level, MAX_LEVEL_BYTES);
    let (message, message_completeness) = bounded_text(&message.message, MAX_MESSAGE_BYTES);
    Some(CargoDiagnostic {
        file,
        file_completeness,
        span,
        level,
        level_completeness,
        message,
        message_completeness,
    })
}

fn parse_test_event(line: &str) -> Option<CargoTestResult> {
    let event: CargoTestEvent = serde_json::from_str(line).ok()?;
    if event.reason != "signalbox-test-result" {
        return None;
    }
    let (executable, executable_completeness) =
        bounded_text(&event.executable, MAX_TEST_EXECUTABLE_BYTES);
    let (name, name_completeness) = bounded_text(&event.name, MAX_TEST_NAME_BYTES);
    Some(CargoTestResult {
        executable,
        executable_completeness,
        name,
        name_completeness,
        outcome: event.outcome,
    })
}

fn parse_test_source_truncated(line: &str) -> Option<String> {
    let event = serde_json::from_str::<CargoTestSourceTruncated>(line).ok()?;
    (event.reason == "signalbox-test-source-truncated").then_some(event.executable)
}

fn parse_test_limit_reached(line: &str) -> Option<String> {
    let event = serde_json::from_str::<CargoTestLimitReached>(line).ok()?;
    (event.reason == "signalbox-test-limit-reached").then_some(event.executable)
}

fn bounded_text(value: &str, max_bytes: usize) -> (String, CaptureCompleteness) {
    let contains_null = value.contains('\0');
    let sanitized = value.replace('\0', "\u{fffd}");
    if sanitized.len() <= max_bytes {
        let completeness = if contains_null {
            CaptureCompleteness::Truncated
        } else {
            CaptureCompleteness::Complete
        };
        return (sanitized, completeness);
    }
    let mut end = max_bytes;
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    (
        String::from(&sanitized[..end]),
        CaptureCompleteness::Truncated,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        ffi::OsString,
        path::PathBuf,
        sync::{Arc, Mutex, PoisonError},
    };

    use signalbox_application::ToolCatalog;
    use signalbox_domain::{ToolName, ToolPermissionDefault};

    use super::*;
    use crate::{
        BwrapAvailability, ProcessOutput, ProcessRequest, ProcessRunResult, ProcessSpawnFailure,
    };

    const DIAGNOSTIC_FILE: &str = "src/lib.rs";
    const DIAGNOSTIC_LEVEL: &str = "error";
    const WARNING_LEVEL: &str = "warning";
    const DIAGNOSTIC_MESSAGE: &str = "mismatched types";
    const DIAGNOSTIC_LINE: u64 = 7;
    const DIAGNOSTIC_COLUMN_START: u64 = 9;
    const DIAGNOSTIC_COLUMN_END: u64 = 12;
    const DIAGNOSTIC_TIMEOUT_SECONDS: u64 = 42;
    const PASSING_TEST: &str = "crate::passes";
    const FAILING_TEST: &str = "crate::fails";
    const IGNORED_TEST: &str = "crate::later";
    const FORGED_TEST: &str = "forged::pass";
    const TEST_EXECUTABLE: &str = "/workspace/target/debug/deps/example-a";
    const SECOND_TEST_EXECUTABLE: &str = "/workspace/target/debug/deps/example-b";
    const CARGO_FAILURE_MESSAGE: &str = "error: failed to parse manifest at /workspace/Cargo.toml";
    const MISSING_SUPERVISOR: &str = "/fixture/missing-supervisor";
    const TEST_COUNT: usize = [PASSING_TEST, FAILING_TEST, IGNORED_TEST].len();
    const BOUNDED_TEXT_FIXTURE: &str = "abcé";
    const BOUNDED_TEXT_LIMIT: usize = 4;
    const TEST_NATIVE_TARGET: &str = "x86_64-unknown-linux-gnu";
    const FOREIGN_TARGET: &str = "aarch64-unknown-linux-gnu";
    const TEST_SANDBOX_LAUNCHER_DESCRIPTOR: i32 = 93;

    struct CargoConfigFixture {
        root: PathBuf,
        external: PathBuf,
    }

    impl CargoConfigFixture {
        fn new() -> Result<Self, Box<dyn Error>> {
            let identity = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "signalbox-diagnostics-config-{}-{identity}",
                std::process::id()
            ));
            let external = root.with_extension("external");
            std::fs::create_dir(&root)?;
            std::fs::create_dir(&external)?;
            Ok(Self { root, external })
        }

        fn create_cargo_directory(&self) -> Result<(), Box<dyn Error>> {
            std::fs::create_dir(self.root.join(".cargo"))?;
            Ok(())
        }

        fn write_config(&self, name: &str, contents: &str) -> Result<(), Box<dyn Error>> {
            std::fs::write(self.root.join(".cargo").join(name), contents)?;
            Ok(())
        }

        #[cfg(target_os = "linux")]
        fn symlink_external_cargo(&self, contents: &str) -> Result<(), Box<dyn Error>> {
            std::fs::write(self.external.join("config"), contents)?;
            std::os::unix::fs::symlink(&self.external, self.root.join(".cargo"))?;
            Ok(())
        }
    }

    impl Drop for CargoConfigFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_dir_all(&self.external);
        }
    }

    #[derive(Clone, Debug)]
    struct FakeRunner {
        requests: Arc<Mutex<Vec<ProcessRequest>>>,
        result: ProcessRunResult,
        host_result: ProcessRunResult,
        host_delay: Duration,
    }

    impl FakeRunner {
        fn returning(result: ProcessRunResult) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                result,
                host_result: process_result(
                    ProcessOutcome::Exited { code: Some(0) },
                    &format!("cargo 1.0.0\nhost: {TEST_NATIVE_TARGET}\n"),
                    CaptureCompleteness::Complete,
                ),
                host_delay: Duration::ZERO,
            }
        }

        fn returning_with_host(result: ProcessRunResult, host_result: ProcessRunResult) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                result,
                host_result,
                host_delay: Duration::ZERO,
            }
        }

        fn with_host_delay(mut self, host_delay: Duration) -> Self {
            self.host_delay = host_delay;
            self
        }

        fn requests(&self) -> Vec<ProcessRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl ProcessRunner for FakeRunner {
        fn sandbox_launcher_program(&self) -> &Path {
            Path::new(BWRAP_PROGRAM)
        }

        fn sandbox_launcher_descriptor(&self) -> Option<i32> {
            Some(TEST_SANDBOX_LAUNCHER_DESCRIPTOR)
        }

        async fn bwrap_availability(&mut self, _probe: ProcessRequest) -> BwrapAvailability {
            BwrapAvailability::Available
        }

        async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
            let is_cargo_host_query = request.arguments.contains(&OsString::from("-vV"));
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request);
            if is_cargo_host_query {
                tokio::time::sleep(self.host_delay).await;
                self.host_result.clone()
            } else {
                self.result.clone()
            }
        }
    }

    fn process_result(
        outcome: ProcessOutcome,
        stdout: &str,
        stdout_completeness: CaptureCompleteness,
    ) -> ProcessRunResult {
        ProcessRunResult {
            outcome,
            stdout: ProcessOutput {
                bytes: stdout.as_bytes().to_vec(),
                completeness: stdout_completeness,
            },
            stderr: ProcessOutput {
                bytes: SANDBOX_DISPATCH_MARKER.to_vec(),
                completeness: CaptureCompleteness::Complete,
            },
        }
    }

    fn compiler_message() -> String {
        compiler_message_at_level(DIAGNOSTIC_LEVEL)
    }

    fn compiler_message_at_level(level: &str) -> String {
        format!(
            r#"{{"reason":"compiler-message","message":{{"level":"{level}","message":"{DIAGNOSTIC_MESSAGE}","spans":[{{"file_name":"{DIAGNOSTIC_FILE}","line_start":{DIAGNOSTIC_LINE},"line_end":{DIAGNOSTIC_LINE},"column_start":{DIAGNOSTIC_COLUMN_START},"column_end":{DIAGNOSTIC_COLUMN_END},"is_primary":true}}]}}}}"#
        )
    }

    fn build_finished_message() -> &'static str {
        r#"{"reason":"build-finished","success":true}"#
    }

    fn repeated_compiler_messages(count: usize) -> String {
        format!("{}\n", compiler_message()).repeat(count)
    }

    fn test_output() -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            test_artifact_event(TEST_EXECUTABLE),
            build_finished_message(),
            test_event(TEST_EXECUTABLE, PASSING_TEST, CargoTestOutcome::Passed),
            test_event(TEST_EXECUTABLE, FAILING_TEST, CargoTestOutcome::Failed),
            test_event(TEST_EXECUTABLE, IGNORED_TEST, CargoTestOutcome::Ignored),
            test_source_truncated_event(TEST_EXECUTABLE),
            test_source_complete_event(TEST_EXECUTABLE),
        )
    }

    fn test_artifact_event(executable: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "reason": "compiler-artifact",
            "executable": executable,
            "profile": { "test": true },
        }))
        .expect("static Cargo artifact event is serializable")
    }

    fn test_event(executable: &str, name: &str, outcome: CargoTestOutcome) -> String {
        serde_json::to_string(&serde_json::json!({
            "reason": "signalbox-test-result",
            "executable": executable,
            "name": name,
            "outcome": outcome,
        }))
        .expect("static test event is serializable")
    }

    fn test_source_truncated_event(executable: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "reason": "signalbox-test-source-truncated",
            "executable": executable,
        }))
        .expect("static truncation event is serializable")
    }

    fn test_source_complete_event(executable: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "reason": "signalbox-test-source-complete",
            "executable": executable,
        }))
        .expect("static completion event is serializable")
    }

    fn test_limit_reached_event(executable: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "reason": "signalbox-test-limit-reached",
            "executable": executable,
        }))
        .expect("static limit event is serializable")
    }

    fn exited_exec_result(stdout: String) -> ExecResult {
        ExecResult {
            confinement: ExecutionConfinement::FilesystemConfined,
            outcome: ProcessOutcome::Exited { code: Some(0) },
            stdout: crate::OutputCapture {
                text: stdout,
                completeness: CaptureCompleteness::Complete,
                encoding: OutputEncoding::Utf8,
            },
            stderr: crate::OutputCapture {
                text: String::new(),
                completeness: CaptureCompleteness::Complete,
                encoding: OutputEncoding::Utf8,
            },
        }
    }

    fn maximal_result() -> CargoDiagnosticsResult {
        let diagnostic = CargoDiagnostic {
            file: Some("f".repeat(MAX_FILE_BYTES)),
            file_completeness: CaptureCompleteness::Complete,
            span: Some(CargoDiagnosticSpan {
                line_start: DIAGNOSTIC_LINE,
                column_start: DIAGNOSTIC_COLUMN_START,
                line_end: DIAGNOSTIC_LINE,
                column_end: DIAGNOSTIC_COLUMN_END,
            }),
            level: "l".repeat(MAX_LEVEL_BYTES),
            level_completeness: CaptureCompleteness::Complete,
            message: "m".repeat(MAX_MESSAGE_BYTES),
            message_completeness: CaptureCompleteness::Complete,
        };
        let test = CargoTestResult {
            executable: "e".repeat(MAX_TEST_EXECUTABLE_BYTES),
            executable_completeness: CaptureCompleteness::Complete,
            name: "t".repeat(MAX_TEST_NAME_BYTES),
            name_completeness: CaptureCompleteness::Complete,
            outcome: CargoTestOutcome::Passed,
        };
        CargoDiagnosticsResult {
            command: CargoDiagnosticsCommand::Test,
            execution: CargoDiagnosticsExecution {
                confinement: ExecutionConfinement::FilesystemConfined,
                outcome: ProcessOutcome::Exited { code: Some(0) },
                stdout: CargoDiagnosticsStream {
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                stderr: CargoDiagnosticsStream {
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                cargo_failure: None,
                preparation_failure: None,
            },
            diagnostics: CargoDiagnosticRecords {
                values: vec![diagnostic; MAX_DIAGNOSTICS],
                limit_reached: false,
                provenance: CargoEvidenceProvenance::WorkspaceInfluenced,
                known_truncated: false,
            },
            tests: CargoTestRecords {
                values: vec![test; MAX_TESTS],
                limit_reached: false,
                provenance: CargoEvidenceProvenance::WorkspaceInfluenced,
                known_truncated: false,
            },
        }
    }

    #[test]
    fn catalog_fixes_diagnostics_permission_to_auto() -> Result<(), Box<dyn Error>> {
        let tool = CargoDiagnosticsTool::try_new(
            FakeRunner::returning(process_result(
                ProcessOutcome::Exited { code: Some(0) },
                "",
                CaptureCompleteness::Complete,
            )),
            std::env::current_dir()?,
        )?;
        let name = ToolName::try_new(String::from(CARGO_DIAGNOSTICS_NAME))
            .map_err(|_| std::io::Error::other("static diagnostics name"))?;
        let definition = tool
            .into_parts()
            .0
            .definition(&name)
            .ok_or_else(|| std::io::Error::other("diagnostics definition must be present"))?;

        assert_eq!(definition.permission_default(), ToolPermissionDefault::Auto);
        Ok(())
    }

    #[test]
    fn construction_error_preserves_the_supervisor_program_distinction() {
        let error = CargoDiagnosticsToolConstructionError::from(
            ExecToolConstructionError::SupervisorProgram {
                path: PathBuf::from(MISSING_SUPERVISOR),
                source: None,
            },
        );

        assert_eq!(
            error.to_string(),
            format!("exec supervisor program `{MISSING_SUPERVISOR}` is invalid")
        );
    }

    #[tokio::test]
    async fn check_returns_primary_diagnostic_and_exact_cargo_shape() -> Result<(), Box<dyn Error>>
    {
        let expected_outcome = ProcessOutcome::Exited { code: Some(1) };
        let fake = FakeRunner::returning(process_result(
            expected_outcome,
            &compiler_message(),
            CaptureCompleteness::Complete,
        ));
        let observation = fake.clone();
        let mut runner = CargoDiagnosticsRunner::try_new(fake, std::env::current_dir()?)?;

        let result = runner
            .try_run(CargoDiagnosticsArguments {
                command: CargoDiagnosticsCommand::Check,
                timeout_seconds: DIAGNOSTIC_TIMEOUT_SECONDS,
            })
            .await?;
        let requests = observation.requests();
        let request = requests
            .first()
            .ok_or_else(|| std::io::Error::other("one cargo request"))?;
        let diagnostic = result
            .diagnostics
            .values
            .first()
            .ok_or_else(|| std::io::Error::other("one compiler diagnostic"))?;

        assert_eq!(request.program, OsString::from(BWRAP_PROGRAM));
        assert!(request.timeout <= std::time::Duration::from_secs(DIAGNOSTIC_TIMEOUT_SECONDS));
        assert!(!request.timeout.is_zero());
        assert_eq!(
            request.capture_bytes,
            DIAGNOSTICS_CAPTURE_BYTES + SANDBOX_DISPATCH_MARKER.len()
        );
        assert!(request.arguments.contains(&OsString::from("check")));
        assert!(
            request
                .arguments
                .contains(&OsString::from("--message-format=json"))
        );
        assert_eq!(diagnostic.file.as_deref(), Some(DIAGNOSTIC_FILE));
        assert_eq!(
            diagnostic.span,
            Some(CargoDiagnosticSpan {
                line_start: DIAGNOSTIC_LINE,
                column_start: DIAGNOSTIC_COLUMN_START,
                line_end: DIAGNOSTIC_LINE,
                column_end: DIAGNOSTIC_COLUMN_END,
            })
        );
        assert_eq!(diagnostic.level, DIAGNOSTIC_LEVEL);
        assert_eq!(diagnostic.message, DIAGNOSTIC_MESSAGE);
        assert_eq!(result.execution.outcome, expected_outcome);
        Ok(())
    }

    #[tokio::test]
    async fn test_returns_outcomes_and_reports_timeout_with_truncated_source()
    -> Result<(), Box<dyn Error>> {
        let stdout = test_output();
        let expected_outcome = ProcessOutcome::TimedOut;
        let fake = FakeRunner::returning(process_result(
            expected_outcome,
            &stdout,
            CaptureCompleteness::Truncated,
        ));
        let mut runner = CargoDiagnosticsRunner::try_new(fake, std::env::current_dir()?)?;

        let result = runner
            .try_run(CargoDiagnosticsArguments {
                command: CargoDiagnosticsCommand::Test,
                timeout_seconds: 10,
            })
            .await?;

        assert_eq!(result.execution.outcome, expected_outcome);
        assert_eq!(
            result.execution.stdout.completeness,
            CaptureCompleteness::Truncated
        );
        assert!(result.tests.known_truncated);
        assert_eq!(result.tests.values.len(), TEST_COUNT);
        assert_eq!(result.tests.values[0].name, PASSING_TEST);
        assert_eq!(result.tests.values[0].executable, TEST_EXECUTABLE);
        assert_eq!(result.tests.values[0].outcome, CargoTestOutcome::Passed);
        assert_eq!(result.tests.values[1].name, FAILING_TEST);
        assert_eq!(result.tests.values[1].outcome, CargoTestOutcome::Failed);
        assert_eq!(result.tests.values[2].name, IGNORED_TEST);
        assert_eq!(result.tests.values[2].outcome, CargoTestOutcome::Ignored);
        Ok(())
    }

    #[test]
    fn bounded_text_preserves_utf8_boundary_and_reports_truncation() {
        let (text, completeness) = bounded_text(BOUNDED_TEXT_FIXTURE, BOUNDED_TEXT_LIMIT);

        assert_eq!(text, &BOUNDED_TEXT_FIXTURE[..3]);
        assert_eq!(completeness, CaptureCompleteness::Truncated);
    }

    #[test]
    fn bounded_text_replaces_null_and_reports_incomplete_evidence() {
        let (text, completeness) = bounded_text("before\0after", MAX_MESSAGE_BYTES);

        assert_eq!(text, "before\u{fffd}after");
        assert_eq!(completeness, CaptureCompleteness::Truncated);
    }

    #[test]
    fn null_bearing_diagnostic_is_admitted_with_honest_completeness() -> Result<(), Box<dyn Error>>
    {
        let stdout = serde_json::to_string(&serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": DIAGNOSTIC_LEVEL,
                "message": "before\0after",
                "spans": [],
            },
        }))?;
        let result = structured_result(CargoDiagnosticsCommand::Check, exited_exec_result(stdout));
        let encoded = encode_tool_result(result)?;
        let decoded: serde_json::Value = serde_json::from_str(&encoded)?;

        assert_eq!(
            decoded["diagnostics"]["values"][0]["message"],
            "before\u{fffd}after"
        );
        assert_eq!(
            decoded["diagnostics"]["values"][0]["message_completeness"],
            "truncated"
        );
        assert!(ToolResultText::try_new(encoded).is_ok());
        Ok(())
    }

    #[test]
    fn truncated_check_output_does_not_claim_truncated_test_evidence() {
        let mut execution = exited_exec_result(compiler_message());
        execution.stdout.completeness = CaptureCompleteness::Truncated;

        let result = structured_result(CargoDiagnosticsCommand::Check, execution);

        assert!(result.diagnostics.known_truncated);
        assert!(!result.tests.known_truncated);
    }

    #[test]
    fn unfinished_check_marks_diagnostic_evidence_incomplete() {
        let result = structured_result(
            CargoDiagnosticsCommand::Check,
            exited_exec_result(compiler_message()),
        );

        assert!(result.diagnostics.known_truncated);
        assert!(!result.tests.known_truncated);
    }

    #[test]
    fn diagnostic_collection_reports_its_record_cap() {
        let stdout = format!(
            "{}{}\n",
            repeated_compiler_messages(MAX_DIAGNOSTICS + 1),
            build_finished_message(),
        );
        let result = structured_result(
            CargoDiagnosticsCommand::Check,
            ExecResult {
                confinement: ExecutionConfinement::FilesystemConfined,
                outcome: ProcessOutcome::Exited { code: Some(1) },
                stdout: crate::OutputCapture {
                    text: stdout,
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                stderr: crate::OutputCapture {
                    text: String::new(),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
            },
        );

        assert_eq!(result.diagnostics.values.len(), MAX_DIAGNOSTICS);
        assert!(result.diagnostics.limit_reached);
        assert!(!result.diagnostics.known_truncated);
    }

    #[test]
    fn aggregate_result_is_admitted_and_reports_omitted_records() -> Result<(), Box<dyn Error>> {
        let encoded = encode_tool_result(maximal_result())?;
        let decoded: serde_json::Value = serde_json::from_str(&encoded)?;

        assert!(ToolResultText::try_new(encoded).is_ok());
        assert_eq!(decoded["tests"]["limit_reached"], true);
        Ok(())
    }

    #[test]
    fn stderr_cannot_forge_cargo_diagnostics() {
        let result = structured_result(
            CargoDiagnosticsCommand::Check,
            ExecResult {
                confinement: ExecutionConfinement::FilesystemConfined,
                outcome: ProcessOutcome::Exited { code: Some(1) },
                stdout: crate::OutputCapture {
                    text: String::new(),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                stderr: crate::OutputCapture {
                    text: compiler_message(),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
            },
        );

        assert_eq!(result.diagnostics.values, Vec::new());
    }

    #[test]
    fn compiler_frames_are_typed_as_workspace_influenced_evidence() {
        let stdout = format!("{}\n{}", compiler_message(), build_finished_message());
        let result = structured_result(CargoDiagnosticsCommand::Check, exited_exec_result(stdout));

        assert_eq!(result.diagnostics.values.len(), 1);
        assert_eq!(
            result.diagnostics.provenance,
            CargoEvidenceProvenance::WorkspaceInfluenced
        );
        assert!(!result.diagnostics.known_truncated);
    }

    #[test]
    fn workspace_influenced_build_finish_can_suppress_later_diagnostics() {
        let stdout = format!("{}\n{}", build_finished_message(), compiler_message());
        let result = structured_result(CargoDiagnosticsCommand::Test, exited_exec_result(stdout));

        assert_eq!(result.diagnostics.values, Vec::new());
        assert_eq!(
            result.diagnostics.provenance,
            CargoEvidenceProvenance::WorkspaceInfluenced
        );
        assert!(!result.diagnostics.known_truncated);
    }

    #[test]
    fn test_program_output_cannot_forge_a_post_build_compiler_diagnostic() {
        let stdout = format!(
            "{}\n{}\n{}\n",
            compiler_message(),
            build_finished_message(),
            compiler_message()
        );
        let result = structured_result(
            CargoDiagnosticsCommand::Test,
            ExecResult {
                confinement: ExecutionConfinement::FilesystemConfined,
                outcome: ProcessOutcome::Exited { code: Some(0) },
                stdout: crate::OutputCapture {
                    text: stdout,
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                stderr: crate::OutputCapture {
                    text: String::new(),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
            },
        );

        assert_eq!(result.diagnostics.values.len(), 1);
    }

    #[test]
    fn cargo_failure_is_bounded_and_marks_unfinished_diagnostic_evidence() {
        let result = structured_result(
            CargoDiagnosticsCommand::Check,
            ExecResult {
                confinement: ExecutionConfinement::FilesystemConfined,
                outcome: ProcessOutcome::Exited { code: Some(101) },
                stdout: crate::OutputCapture {
                    text: String::new(),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                stderr: crate::OutputCapture {
                    text: String::from(CARGO_FAILURE_MESSAGE),
                    completeness: CaptureCompleteness::Truncated,
                    encoding: OutputEncoding::Utf8,
                },
            },
        );
        let failure = result
            .execution
            .cargo_failure
            .as_ref()
            .expect("failed Cargo invocation retains its explanation");

        assert_eq!(failure.message, CARGO_FAILURE_MESSAGE);
        assert_eq!(failure.message_completeness, CaptureCompleteness::Truncated);
        assert!(result.diagnostics.known_truncated);
        assert!(!result.tests.known_truncated);
    }

    #[test]
    fn cargo_failure_is_retained_when_only_unrelated_warnings_were_structured() {
        let result = structured_result(
            CargoDiagnosticsCommand::Check,
            ExecResult {
                confinement: ExecutionConfinement::FilesystemConfined,
                outcome: ProcessOutcome::Exited { code: Some(101) },
                stdout: crate::OutputCapture {
                    text: compiler_message_at_level(WARNING_LEVEL),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                stderr: crate::OutputCapture {
                    text: String::from(CARGO_FAILURE_MESSAGE),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
            },
        );
        let failure = result
            .execution
            .cargo_failure
            .as_ref()
            .expect("an unrelated warning does not explain Cargo failure");

        assert_eq!(result.diagnostics.values[0].level, WARNING_LEVEL);
        assert_eq!(failure.message, CARGO_FAILURE_MESSAGE);
    }

    #[test]
    fn lossy_cargo_failure_text_is_incomplete() {
        let mut execution = exited_exec_result(String::new());
        execution.outcome = ProcessOutcome::Exited { code: Some(101) };
        execution.stderr.text = String::from(CARGO_FAILURE_MESSAGE);
        execution.stderr.encoding = OutputEncoding::LossyUtf8;

        let result = structured_result(CargoDiagnosticsCommand::Check, execution);
        let failure = result
            .execution
            .cargo_failure
            .as_ref()
            .expect("failed Cargo invocation retains its lossy explanation");

        assert_eq!(failure.message, CARGO_FAILURE_MESSAGE);
        assert_eq!(failure.message_completeness, CaptureCompleteness::Truncated);
    }

    #[test]
    fn test_events_reject_raw_output_and_retain_target_identity() {
        let stdout = format!(
            "{}\n{}\n{}\n{}\ntest {FORGED_TEST} ... ok\n{}\n{}\n{}\n{}\n",
            test_artifact_event(TEST_EXECUTABLE),
            test_artifact_event(SECOND_TEST_EXECUTABLE),
            build_finished_message(),
            test_event(TEST_EXECUTABLE, PASSING_TEST, CargoTestOutcome::Passed),
            test_source_truncated_event(TEST_EXECUTABLE),
            test_event(
                SECOND_TEST_EXECUTABLE,
                PASSING_TEST,
                CargoTestOutcome::Failed
            ),
            test_source_complete_event(TEST_EXECUTABLE),
            test_source_complete_event(SECOND_TEST_EXECUTABLE),
        );
        let result = structured_result(
            CargoDiagnosticsCommand::Test,
            ExecResult {
                confinement: ExecutionConfinement::FilesystemConfined,
                outcome: ProcessOutcome::Exited { code: Some(1) },
                stdout: crate::OutputCapture {
                    text: stdout,
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                stderr: crate::OutputCapture {
                    text: String::new(),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
            },
        );

        assert_eq!(result.tests.values.len(), 2);
        assert!(result.tests.known_truncated);
        assert_eq!(result.tests.values[0].name, PASSING_TEST);
        assert_eq!(result.tests.values[0].executable, TEST_EXECUTABLE);
        assert_eq!(result.tests.values[0].outcome, CargoTestOutcome::Passed);
        assert_eq!(result.tests.values[1].name, PASSING_TEST);
        assert_eq!(result.tests.values[1].executable, SECOND_TEST_EXECUTABLE);
        assert_eq!(result.tests.values[1].outcome, CargoTestOutcome::Failed);
    }

    #[test]
    fn test_results_require_completion_from_every_announced_target() {
        let event = test_event(TEST_EXECUTABLE, PASSING_TEST, CargoTestOutcome::Passed);
        let one_completion = structured_result(
            CargoDiagnosticsCommand::Test,
            exited_exec_result(format!(
                "{}\n{}\n{}\n{event}\n{}",
                test_artifact_event(TEST_EXECUTABLE),
                test_artifact_event(SECOND_TEST_EXECUTABLE),
                build_finished_message(),
                test_source_complete_event(TEST_EXECUTABLE),
            )),
        );
        let every_completion = structured_result(
            CargoDiagnosticsCommand::Test,
            exited_exec_result(format!(
                "{}\n{}\n{}\n{event}\n{}\n{}",
                test_artifact_event(TEST_EXECUTABLE),
                test_artifact_event(SECOND_TEST_EXECUTABLE),
                build_finished_message(),
                test_source_complete_event(TEST_EXECUTABLE),
                test_source_complete_event(SECOND_TEST_EXECUTABLE),
            )),
        );

        assert!(one_completion.tests.known_truncated);
        assert!(!every_completion.tests.known_truncated);
    }

    #[test]
    fn unsuccessful_build_marks_test_evidence_incomplete() {
        let result = structured_result(
            CargoDiagnosticsCommand::Test,
            exited_exec_result(String::from(
                r#"{"reason":"build-finished","success":false}"#,
            )),
        );

        assert!(result.tests.known_truncated);
        assert_eq!(result.tests.values, Vec::new());
    }

    #[test]
    fn helper_limit_frame_marks_the_test_collection() {
        let stdout = format!(
            "{}\n{}\n{}\n{}",
            test_artifact_event(TEST_EXECUTABLE),
            build_finished_message(),
            test_limit_reached_event(TEST_EXECUTABLE),
            test_source_complete_event(TEST_EXECUTABLE),
        );

        let result = structured_result(CargoDiagnosticsCommand::Test, exited_exec_result(stdout));

        assert!(result.tests.limit_reached);
    }

    #[test]
    fn helper_shaped_test_frames_before_build_finished_are_rejected() {
        let forged = format!(
            "{}\n{}\n{}\n{}",
            test_event(TEST_EXECUTABLE, FORGED_TEST, CargoTestOutcome::Passed),
            test_source_complete_event(TEST_EXECUTABLE),
            test_artifact_event(TEST_EXECUTABLE),
            build_finished_message(),
        );

        let result = structured_result(CargoDiagnosticsCommand::Test, exited_exec_result(forged));

        assert_eq!(result.tests.values, Vec::new());
        assert!(result.tests.known_truncated);
    }

    #[test]
    fn preserved_native_runner_output_cannot_forge_helper_frames() {
        let stdout = format!(
            "{}\n{}\n{}\n{}",
            test_artifact_event(TEST_EXECUTABLE),
            build_finished_message(),
            test_event(TEST_EXECUTABLE, FORGED_TEST, CargoTestOutcome::Passed),
            test_source_complete_event(TEST_EXECUTABLE),
        );

        let result = structured_result_with_test_runner_mode(
            CargoDiagnosticsCommand::Test,
            exited_exec_result(stdout),
            CargoTestRunnerMode::ConfiguredRunnerPreserved,
        );

        assert_eq!(result.tests.values, Vec::new());
        assert!(result.tests.known_truncated);
    }

    #[test]
    fn spawn_failure_remains_typed_without_terminal_text() {
        let expected_confinement = ExecutionConfinement::SandboxRefused {
            availability: BwrapAvailability::Missing,
        };
        let expected_outcome = ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::SandboxUnavailable,
        };
        let result = structured_result(
            CargoDiagnosticsCommand::Check,
            ExecResult {
                confinement: expected_confinement,
                outcome: expected_outcome,
                stdout: crate::OutputCapture {
                    text: String::new(),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
                stderr: crate::OutputCapture {
                    text: String::new(),
                    completeness: CaptureCompleteness::Complete,
                    encoding: OutputEncoding::Utf8,
                },
            },
        );

        assert_eq!(result.execution.confinement, expected_confinement);
        assert_eq!(result.execution.outcome, expected_outcome);
        assert_eq!(result.diagnostics.values, Vec::new());
        assert_eq!(result.tests.values, Vec::new());
    }

    #[test]
    fn cargo_test_arguments_keep_no_fail_fast_before_workspace_flags() {
        let arguments = cargo_arguments(
            CargoDiagnosticsCommand::Test,
            300,
            CargoTestRunnerMode::HelperInstalled,
            TEST_NATIVE_TARGET,
        );
        let runner_config = cargo_test_runner_config(TEST_NATIVE_TARGET);

        assert_eq!(
            arguments.arguments,
            [
                "test",
                "--config",
                "term.quiet=false",
                "--config",
                runner_config.as_str(),
                "--no-fail-fast",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--message-format=json",
            ]
            .map(String::from)
        );
        assert!(runner_config.contains(TEST_NATIVE_TARGET));
        assert!(!runner_config.contains("cfg(all())"));
        assert_eq!(arguments.working_directory, ".");
    }

    #[test]
    fn cargo_test_arguments_preserve_a_configured_native_runner() {
        let arguments = cargo_arguments(
            CargoDiagnosticsCommand::Test,
            300,
            CargoTestRunnerMode::ConfiguredRunnerPreserved,
            TEST_NATIVE_TARGET,
        );

        assert_eq!(
            arguments.arguments,
            [
                "test",
                "--config",
                "term.quiet=false",
                "--no-fail-fast",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--message-format=json",
            ]
            .map(String::from)
        );
    }

    #[test]
    fn cargo_config_runner_detection_distinguishes_selected_foreign_and_cfg_tables() {
        let native_target = TEST_NATIVE_TARGET;
        let native = format!("[target.'{native_target}']\nrunner = 'native-wrapper'\n");
        let quoted_runner = format!("[target.'{native_target}']\n\"runner\" = 'quoted-wrapper'\n");
        let dotted_runner = format!("target.'{native_target}'.\"runner\" = 'dotted-wrapper'\n");
        let foreign = "[target.'aarch64-unknown-linux-gnu']\nrunner = 'qemu-aarch64'\n";
        let configured = "[target.'cfg(target_os = \"linux\")']\nrunner = 'linux-wrapper'\n";

        let native_plan = config_text_test_runner_plan(&native, native_target);
        let quoted_plan = config_text_test_runner_plan(&quoted_runner, native_target);
        let dotted_plan = config_text_test_runner_plan(&dotted_runner, native_target);
        let foreign_plan = config_text_test_runner_plan(foreign, native_target);
        let configured_plan = config_text_test_runner_plan(configured, native_target);

        assert_eq!(
            native_plan.map(|plan| plan.mode),
            Some(CargoTestRunnerMode::ConfiguredRunnerPreserved)
        );
        assert_eq!(
            quoted_plan.map(|plan| plan.mode),
            Some(CargoTestRunnerMode::ConfiguredRunnerPreserved)
        );
        assert_eq!(
            dotted_plan.map(|plan| plan.mode),
            Some(CargoTestRunnerMode::ConfiguredRunnerPreserved)
        );
        assert_eq!(
            foreign_plan.map(|plan| plan.mode),
            Some(CargoTestRunnerMode::HelperInstalled)
        );
        assert_eq!(
            configured_plan.map(|plan| plan.mode),
            Some(CargoTestRunnerMode::ConfiguredRunnerPreserved)
        );
    }

    #[test]
    fn configured_build_target_selects_that_targets_runner() {
        let config = format!(
            "[build]\ntarget = '{FOREIGN_TARGET}'\n[target.'{FOREIGN_TARGET}']\nrunner = 'qemu-aarch64'\n"
        );

        let plan = config_text_test_runner_plan(&config, TEST_NATIVE_TARGET)
            .expect("bounded static Cargo config");

        assert_eq!(plan.selected_target, FOREIGN_TARGET);
        assert_eq!(plan.mode, CargoTestRunnerMode::ConfiguredRunnerPreserved);
    }

    #[test]
    fn extensionless_cargo_config_wins_over_config_toml() -> Result<(), Box<dyn Error>> {
        let fixture = CargoConfigFixture::new()?;
        fixture.create_cargo_directory()?;
        fixture.write_config("config", "")?;
        fixture.write_config(
            "config.toml",
            &format!("[target.'{TEST_NATIVE_TARGET}']\nrunner = 'ignored-wrapper'\n"),
        )?;

        let plan = workspace_test_runner_plan(&fixture.root, TEST_NATIVE_TARGET);

        assert_eq!(plan.selected_target, TEST_NATIVE_TARGET);
        assert_eq!(plan.mode, CargoTestRunnerMode::HelperInstalled);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symlinked_cargo_directory_is_opaque_without_reading_its_target() -> Result<(), Box<dyn Error>>
    {
        let fixture = CargoConfigFixture::new()?;
        fixture.symlink_external_cargo("")?;

        let plan = workspace_test_runner_plan(&fixture.root, TEST_NATIVE_TARGET);

        assert_eq!(plan.selected_target, TEST_NATIVE_TARGET);
        assert_eq!(plan.mode, CargoTestRunnerMode::ConfiguredRunnerPreserved);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cargo_config_fifo_is_opaque_without_blocking() -> Result<(), Box<dyn Error>> {
        let fixture = CargoConfigFixture::new()?;
        fixture.create_cargo_directory()?;
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            fixture.root.join(".cargo/config"),
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;

        let plan = workspace_test_runner_plan(&fixture.root, TEST_NATIVE_TARGET);

        assert_eq!(plan.selected_target, TEST_NATIVE_TARGET);
        assert_eq!(plan.mode, CargoTestRunnerMode::ConfiguredRunnerPreserved);
        Ok(())
    }

    #[test]
    fn cargo_host_accepts_the_runtime_cargo_host_triple() {
        let result = exited_exec_result(format!(
            "cargo 1.0.0\nrelease: 1.0.0\nhost: {TEST_NATIVE_TARGET}\n"
        ));

        let host = cargo_host(&result);

        assert_eq!(host, Some(TEST_NATIVE_TARGET));
    }

    #[tokio::test]
    async fn one_second_timeout_preserves_positive_test_run_remainder() -> Result<(), Box<dyn Error>>
    {
        let fake = FakeRunner::returning(process_result(
            ProcessOutcome::Exited { code: Some(0) },
            &test_output(),
            CaptureCompleteness::Complete,
        ))
        .with_host_delay(Duration::from_millis(20));
        let observation = fake.clone();
        let mut runner = CargoDiagnosticsRunner::try_new(fake, std::env::current_dir()?)?;

        let result = runner
            .try_run(CargoDiagnosticsArguments {
                command: CargoDiagnosticsCommand::Test,
                timeout_seconds: 1,
            })
            .await?;
        let requests = observation.requests();
        let test_request = requests
            .last()
            .ok_or_else(|| std::io::Error::other("cargo test request"))?;

        assert_eq!(
            result.execution.outcome,
            ProcessOutcome::Exited { code: Some(0) }
        );
        assert!(test_request.arguments.contains(&OsString::from("test")));
        assert!(!test_request.timeout.is_zero());
        assert!(test_request.timeout < Duration::from_secs(1));
        Ok(())
    }

    #[tokio::test]
    async fn cargo_config_inspection_obeys_the_remaining_request_deadline() {
        let plan = cargo_config_inspection_before_deadline(
            || {
                std::thread::sleep(Duration::from_millis(100));
                CargoTestRunnerPlan {
                    selected_target: String::from(TEST_NATIVE_TARGET),
                    mode: CargoTestRunnerMode::HelperInstalled,
                }
            },
            Duration::from_millis(1),
        )
        .await;

        assert_eq!(plan, None);
    }

    #[tokio::test]
    async fn unusable_host_output_preserves_exit_and_names_preparation_failure()
    -> Result<(), Box<dyn Error>> {
        let expected_outcome = ProcessOutcome::Exited { code: Some(0) };
        let fake = FakeRunner::returning_with_host(
            process_result(
                ProcessOutcome::Exited { code: Some(0) },
                &test_output(),
                CaptureCompleteness::Complete,
            ),
            process_result(
                expected_outcome,
                "cargo 1.0.0\n",
                CaptureCompleteness::Complete,
            ),
        );
        let observation = fake.clone();
        let mut runner = CargoDiagnosticsRunner::try_new(fake, std::env::current_dir()?)?;

        let result = runner
            .try_run(CargoDiagnosticsArguments {
                command: CargoDiagnosticsCommand::Test,
                timeout_seconds: DIAGNOSTIC_TIMEOUT_SECONDS,
            })
            .await?;
        let requests = observation.requests();

        assert_eq!(result.execution.outcome, expected_outcome);
        assert_eq!(
            result.execution.preparation_failure,
            Some(CargoDiagnosticsPreparationFailure::CargoHostUnavailable)
        );
        assert_eq!(requests.len(), 1);
        assert!(requests[0].arguments.contains(&OsString::from("-vV")));
        Ok(())
    }
}

use std::{error::Error, fmt, path::Path};

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
const MAX_DIAGNOSTICS: usize = 64;
const MAX_TESTS: usize = 512;
const MAX_FILE_BYTES: usize = 4096;
const MAX_LEVEL_BYTES: usize = 64;
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_TEST_NAME_BYTES: usize = 1024;
const MAX_CARGO_FAILURE_BYTES: usize = 8192;
const MAX_TEST_EXECUTABLE_BYTES: usize = 4096;
const CARGO_TEST_RUNNER: &str = "['/signalbox-exec-dispatch','--cargo-test-runner']";
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
        let exec_arguments = cargo_arguments(command, arguments.timeout_seconds);
        let result = self
            .command_runner
            .run_with_capture(exec_arguments, DIAGNOSTICS_CAPTURE_BYTES)
            .await;
        structured_result(command, result)
    }
}

fn cargo_arguments(command: CargoDiagnosticsCommand, timeout_seconds: u64) -> ExecArguments {
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
        CargoDiagnosticsCommand::Test => vec![
            String::from("test"),
            String::from("--config"),
            String::from("term.quiet=false"),
            String::from("--config"),
            cargo_test_runner_config(),
            String::from("--no-fail-fast"),
            String::from("--workspace"),
            String::from("--all-targets"),
            String::from("--all-features"),
            String::from("--message-format=json"),
        ],
    };
    ExecArguments {
        program: String::from("cargo"),
        arguments,
        working_directory: String::from("."),
        timeout_seconds,
    }
}

fn cargo_test_runner_config() -> String {
    format!(
        "target.'{}'.runner={CARGO_TEST_RUNNER}",
        env!("SIGNALBOX_EXECUTION_TARGET")
    )
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
    /// Whether an underlying stream was truncated before parsing completed.
    pub source_truncated: bool,
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
    /// Parsed test-outcome prefix, capped at 512 records.
    pub values: Vec<CargoTestResult>,
    /// Whether additional parsed test outcomes were omitted by the record cap.
    pub limit_reached: bool,
    /// Whether an underlying stream was truncated before parsing completed.
    pub source_truncated: bool,
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
    /// Observed libtest outcome.
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
}

#[derive(serde::Deserialize)]
struct CargoMessage {
    reason: String,
    message: Option<RustcMessage>,
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

fn structured_result(
    command: CargoDiagnosticsCommand,
    result: ExecResult,
) -> CargoDiagnosticsResult {
    let source_truncated = result.stdout.completeness == CaptureCompleteness::Truncated;
    let mut test_source_truncated = source_truncated;
    let mut diagnostics = Vec::new();
    let mut tests = Vec::new();
    let mut diagnostic_limit_reached = false;
    let mut test_limit_reached = false;
    let mut test_source_complete = false;
    parse_stream(
        &result.stdout.text,
        command,
        &mut diagnostics,
        &mut diagnostic_limit_reached,
        &mut tests,
        &mut test_limit_reached,
        TestSourceEvidence {
            truncated: &mut test_source_truncated,
            complete: &mut test_source_complete,
        },
    );
    if command == CargoDiagnosticsCommand::Test && !test_source_complete {
        test_source_truncated = true;
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
        },
        diagnostics: CargoDiagnosticRecords {
            values: diagnostics,
            limit_reached: diagnostic_limit_reached,
            source_truncated,
        },
        tests: CargoTestRecords {
            values: tests,
            limit_reached: test_limit_reached,
            source_truncated: test_source_truncated,
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
    loop {
        let encoded = serde_json::to_string(&result)
            .map_err(|_| CargoDiagnosticsExecutorError::ResultEncoding)?;
        match ToolResultText::try_new(encoded) {
            Ok(admitted) => return Ok(admitted.into_string()),
            Err(error) => match error.failure() {
                ToolResultTextFailure::TooLarge { .. } if result.tests.values.pop().is_some() => {
                    result.tests.limit_reached = true;
                }
                ToolResultTextFailure::TooLarge { .. }
                    if result.diagnostics.values.pop().is_some() =>
                {
                    result.diagnostics.limit_reached = true;
                }
                ToolResultTextFailure::TooLarge { .. } | ToolResultTextFailure::ContainsNull => {
                    return Err(CargoDiagnosticsExecutorError::ResultEncoding);
                }
            },
        }
    }
}

fn parse_stream(
    text: &str,
    command: CargoDiagnosticsCommand,
    diagnostics: &mut Vec<CargoDiagnostic>,
    diagnostic_limit_reached: &mut bool,
    tests: &mut Vec<CargoTestResult>,
    test_limit_reached: &mut bool,
    test_source_evidence: TestSourceEvidence<'_>,
) {
    let mut build_finished = false;
    for line in text.lines() {
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
        if command == CargoDiagnosticsCommand::Test && cargo_build_finished(line) {
            build_finished = true;
        }
        if command == CargoDiagnosticsCommand::Test {
            if parse_test_source_truncated(line) {
                *test_source_evidence.truncated = true;
            } else if parse_test_source_complete(line) {
                *test_source_evidence.complete = true;
            } else if let Some(test) = parse_test_event(line) {
                push_bounded(tests, test, MAX_TESTS, test_limit_reached);
            }
        }
    }
}

struct TestSourceEvidence<'a> {
    truncated: &'a mut bool,
    complete: &'a mut bool,
}

fn cargo_build_finished(line: &str) -> bool {
    serde_json::from_str::<CargoReason>(line)
        .is_ok_and(|message| message.reason == "build-finished")
}

fn parse_test_source_complete(line: &str) -> bool {
    serde_json::from_str::<CargoReason>(line)
        .is_ok_and(|event| event.reason == "signalbox-test-source-complete")
}

#[derive(serde::Deserialize)]
struct CargoReason {
    reason: String,
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

fn parse_test_source_truncated(line: &str) -> bool {
    serde_json::from_str::<CargoTestSourceTruncated>(line)
        .is_ok_and(|event| event.reason == "signalbox-test-source-truncated")
}

fn bounded_text(value: &str, max_bytes: usize) -> (String, CaptureCompleteness) {
    if value.len() <= max_bytes {
        return (String::from(value), CaptureCompleteness::Complete);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (String::from(&value[..end]), CaptureCompleteness::Truncated)
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

    #[derive(Clone, Debug)]
    struct FakeRunner {
        requests: Arc<Mutex<Vec<ProcessRequest>>>,
        result: ProcessRunResult,
    }

    impl FakeRunner {
        fn returning(result: ProcessRunResult) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                result,
            }
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

        async fn bwrap_availability(&mut self, _probe: ProcessRequest) -> BwrapAvailability {
            BwrapAvailability::Available
        }

        async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request);
            self.result.clone()
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
            "{}\n{}\n{}\n{}\n",
            test_event(TEST_EXECUTABLE, PASSING_TEST, CargoTestOutcome::Passed),
            test_event(TEST_EXECUTABLE, FAILING_TEST, CargoTestOutcome::Failed),
            test_event(TEST_EXECUTABLE, IGNORED_TEST, CargoTestOutcome::Ignored),
            test_source_complete_event(),
        )
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

    fn test_source_truncated_event() -> String {
        serde_json::to_string(&serde_json::json!({
            "reason": "signalbox-test-source-truncated",
        }))
        .expect("static truncation event is serializable")
    }

    fn test_source_complete_event() -> String {
        serde_json::to_string(&serde_json::json!({
            "reason": "signalbox-test-source-complete",
        }))
        .expect("static completion event is serializable")
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
            },
            diagnostics: CargoDiagnosticRecords {
                values: vec![diagnostic; MAX_DIAGNOSTICS],
                limit_reached: false,
                source_truncated: false,
            },
            tests: CargoTestRecords {
                values: vec![test; MAX_TESTS],
                limit_reached: false,
                source_truncated: false,
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
        assert!(result.tests.source_truncated);
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
    fn diagnostic_collection_reports_its_record_cap() {
        let stdout = repeated_compiler_messages(MAX_DIAGNOSTICS + 1);
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
        assert!(!result.diagnostics.source_truncated);
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
    fn cargo_failure_is_bounded_without_tainting_stdout_collections() {
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
        assert!(!result.diagnostics.source_truncated);
        assert!(!result.tests.source_truncated);
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
    fn test_events_reject_raw_output_and_retain_target_identity() {
        let stdout = format!(
            "{}\ntest {FORGED_TEST} ... ok\n{}\n{}\n{}\n",
            test_event(TEST_EXECUTABLE, PASSING_TEST, CargoTestOutcome::Passed),
            test_source_truncated_event(),
            test_event(
                SECOND_TEST_EXECUTABLE,
                PASSING_TEST,
                CargoTestOutcome::Failed
            ),
            test_source_complete_event(),
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
        assert!(result.tests.source_truncated);
        assert_eq!(result.tests.values[0].name, PASSING_TEST);
        assert_eq!(result.tests.values[0].executable, TEST_EXECUTABLE);
        assert_eq!(result.tests.values[0].outcome, CargoTestOutcome::Passed);
        assert_eq!(result.tests.values[1].name, PASSING_TEST);
        assert_eq!(result.tests.values[1].executable, SECOND_TEST_EXECUTABLE);
        assert_eq!(result.tests.values[1].outcome, CargoTestOutcome::Failed);
    }

    #[test]
    fn test_results_require_a_trusted_source_completion_frame() {
        let event = test_event(TEST_EXECUTABLE, PASSING_TEST, CargoTestOutcome::Passed);
        let without_completion = structured_result(
            CargoDiagnosticsCommand::Test,
            exited_exec_result(event.clone()),
        );
        let with_completion = structured_result(
            CargoDiagnosticsCommand::Test,
            exited_exec_result(format!("{event}\n{}", test_source_complete_event())),
        );

        assert!(without_completion.tests.source_truncated);
        assert!(!with_completion.tests.source_truncated);
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
        let arguments = cargo_arguments(CargoDiagnosticsCommand::Test, 300);
        let runner_config = cargo_test_runner_config();

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
        assert!(runner_config.contains(env!("SIGNALBOX_EXECUTION_TARGET")));
        assert!(!runner_config.contains("cfg(all())"));
        assert_eq!(arguments.working_directory, ".");
    }
}

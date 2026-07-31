use std::{error::Error, fmt, path::Path};

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
    /// The sandboxed execution core rejected the workspace root.
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
        formatter.write_str(match self {
            Self::Exec(_) => "cargo diagnostics workspace root is invalid",
            Self::Name => "cargo diagnostics static name is invalid",
            Self::Schema => "cargo diagnostics static schema is invalid",
            Self::ErrorDetail => "cargo diagnostics static error detail is invalid",
            Self::Duplicate => "cargo diagnostics catalog is duplicated",
        })
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
    ) -> Result<Self, CargoDiagnosticsToolConstructionError> {
        Self::try_new(TokioProcessRunner, workspace_root)
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
        let encoded = serde_json::to_string(&result)
            .map_err(|_| CargoDiagnosticsExecutorError::ResultEncoding)?;
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
        CargoDiagnosticsCommand::Check => vec![
            "check",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--message-format=json",
        ],
        CargoDiagnosticsCommand::Clippy => vec![
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--message-format=json",
            "--",
            "-D",
            "warnings",
        ],
        CargoDiagnosticsCommand::Test => vec![
            "test",
            "--no-fail-fast",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--message-format=json",
            "--",
            "--format=pretty",
        ],
    }
    .into_iter()
    .map(String::from)
    .collect();
    ExecArguments {
        program: String::from("cargo"),
        arguments,
        working_directory: String::from("."),
        timeout_seconds,
    }
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
    /// Fully qualified test name when it fit the field bound.
    pub name: String,
    /// Whether the complete test name was retained.
    pub name_completeness: CaptureCompleteness,
    /// Observed libtest outcome.
    pub outcome: CargoTestOutcome,
}

/// Closed libtest outcomes represented by pretty output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
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
    let source_truncated = result.stdout.completeness == CaptureCompleteness::Truncated
        || result.stderr.completeness == CaptureCompleteness::Truncated;
    let mut diagnostics = Vec::new();
    let mut tests = Vec::new();
    let mut diagnostic_limit_reached = false;
    let mut test_limit_reached = false;
    parse_stream(
        &result.stdout.text,
        command,
        &mut diagnostics,
        &mut diagnostic_limit_reached,
        &mut tests,
        &mut test_limit_reached,
    );
    parse_stream(
        &result.stderr.text,
        command,
        &mut diagnostics,
        &mut diagnostic_limit_reached,
        &mut tests,
        &mut test_limit_reached,
    );
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
        },
        diagnostics: CargoDiagnosticRecords {
            values: diagnostics,
            limit_reached: diagnostic_limit_reached,
            source_truncated,
        },
        tests: CargoTestRecords {
            values: tests,
            limit_reached: test_limit_reached,
            source_truncated,
        },
    }
}

fn parse_stream(
    text: &str,
    command: CargoDiagnosticsCommand,
    diagnostics: &mut Vec<CargoDiagnostic>,
    diagnostic_limit_reached: &mut bool,
    tests: &mut Vec<CargoTestResult>,
    test_limit_reached: &mut bool,
) {
    for line in text.lines() {
        if let Some(diagnostic) = parse_diagnostic(line) {
            push_bounded(
                diagnostics,
                diagnostic,
                MAX_DIAGNOSTICS,
                diagnostic_limit_reached,
            );
        }
        if command == CargoDiagnosticsCommand::Test
            && let Some(test) = parse_test(line)
        {
            push_bounded(tests, test, MAX_TESTS, test_limit_reached);
        }
    }
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

fn parse_test(line: &str) -> Option<CargoTestResult> {
    let stripped = line.trim().strip_prefix("test ")?;
    let (name, outcome) = stripped.rsplit_once(" ... ")?;
    let outcome = match outcome {
        "ok" => CargoTestOutcome::Passed,
        "FAILED" => CargoTestOutcome::Failed,
        "ignored" => CargoTestOutcome::Ignored,
        _ => return None,
    };
    let (name, name_completeness) = bounded_text(name, MAX_TEST_NAME_BYTES);
    Some(CargoTestResult {
        name,
        name_completeness,
        outcome,
    })
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
    const DIAGNOSTIC_MESSAGE: &str = "mismatched types";
    const DIAGNOSTIC_LINE: u64 = 7;
    const DIAGNOSTIC_COLUMN_START: u64 = 9;
    const DIAGNOSTIC_COLUMN_END: u64 = 12;
    const DIAGNOSTIC_TIMEOUT_SECONDS: u64 = 42;
    const PASSING_TEST: &str = "crate::passes";
    const FAILING_TEST: &str = "crate::fails";
    const IGNORED_TEST: &str = "crate::later";
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
        format!(
            r#"{{"reason":"compiler-message","message":{{"level":"{DIAGNOSTIC_LEVEL}","message":"{DIAGNOSTIC_MESSAGE}","spans":[{{"file_name":"{DIAGNOSTIC_FILE}","line_start":{DIAGNOSTIC_LINE},"line_end":{DIAGNOSTIC_LINE},"column_start":{DIAGNOSTIC_COLUMN_START},"column_end":{DIAGNOSTIC_COLUMN_END},"is_primary":true}}]}}}}"#
        )
    }

    fn repeated_compiler_messages(count: usize) -> String {
        format!("{}\n", compiler_message()).repeat(count)
    }

    fn test_output() -> String {
        format!(
            "test {PASSING_TEST} ... ok\ntest {FAILING_TEST} ... FAILED\ntest {IGNORED_TEST} ... ignored\n"
        )
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
        assert_eq!(
            request.timeout,
            std::time::Duration::from_secs(DIAGNOSTIC_TIMEOUT_SECONDS)
        );
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

        assert_eq!(
            arguments.arguments,
            [
                "test",
                "--no-fail-fast",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--message-format=json",
                "--",
                "--format=pretty",
            ]
            .map(String::from)
        );
        assert_eq!(arguments.working_directory, ".");
    }
}

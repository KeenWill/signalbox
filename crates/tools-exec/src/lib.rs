//! Bounded direct command execution rooted at an injected workspace.
//!
//! [`SandboxedExecTool`] confines writable and project-visible filesystem
//! authority to the injected workspace with bubblewrap (`bwrap`). The sandbox
//! also exposes a small read-only operating-system runtime needed to start
//! ordinary programs, clears the ambient environment, and admits only a
//! bounded locale plus executable search paths rooted in that runtime.
//! Production uses the trusted absolute `/usr/bin/bwrap`; missing, unsupported,
//! or unusable `bwrap` is typed refusal evidence and never falls back to
//! unsandboxed execution.
//!
//! [`UnsandboxedExecTool`] is a separate catalog-composable tool whose fixed
//! permission default requires owner confirmation. This crate ships the two
//! tools independently and does not decide which session catalogs contain
//! either one.
//!
//! [`CargoDiagnosticsTool`] builds on the same sandboxed core to run bounded
//! whole-workspace Cargo check, clippy, and test passes. It returns typed
//! compiler locations and test outcomes rather than raw terminal output.
//!
//! This day-one profile does not fence the network or impose resource limits.
//! It is therefore not an admissible boundary for executing untrusted code.
//! Shell sessions and PTYs are likewise outside this crate's contract.
//!
//! The real-bubblewrap containment check is mandatory in CI. Unsupported local
//! hosts skip it unless `SIGNALBOX_RUN_BWRAP_INTEGRATION=1` requests the same
//! fail-closed check explicitly:
//! `SIGNALBOX_RUN_BWRAP_INTEGRATION=1 cargo test -p signalbox-tools-exec
//! --test bwrap`.

mod diagnostics;
mod process;
#[cfg(target_os = "linux")]
mod supervisor_protocol;

pub use diagnostics::{
    CARGO_DIAGNOSTICS_NAME, CargoDiagnostic, CargoDiagnosticRecords, CargoDiagnosticSpan,
    CargoDiagnosticsArguments, CargoDiagnosticsCommand, CargoDiagnosticsExecution,
    CargoDiagnosticsExecutor, CargoDiagnosticsExecutorError, CargoDiagnosticsResult,
    CargoDiagnosticsRunner, CargoDiagnosticsStream, CargoDiagnosticsTool,
    CargoDiagnosticsToolConstructionError, CargoFailureDetail, CargoTestOutcome, CargoTestRecords,
    CargoTestResult, InvalidCargoDiagnosticsArguments,
};
pub use process::{
    BwrapAvailability, CaptureCompleteness, ExecArguments, ExecExecutor, ExecExecutorError,
    ExecResult, ExecToolConstructionError, ExecutionConfinement, InvalidExecArguments,
    OutputCapture, OutputEncoding, ProcessEnvironment, ProcessOutcome, ProcessOutput,
    ProcessRequest, ProcessRunResult, ProcessRunner, ProcessSpawnFailure, ProcessStatusProtocol,
    ProcessSupervisionFailure, SANDBOXED_EXEC_NAME, SandboxedCommandRunner, SandboxedExecTool,
    TokioProcessRunner, UNSANDBOXED_EXEC_NAME, UnsandboxedCommandRunner, UnsandboxedExecTool,
};

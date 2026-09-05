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
//! permission default requires user confirmation. This crate ships the two
//! tools independently and does not decide which session catalogs contain
//! either one.
//!
//! [`CargoDiagnosticsTool`] builds on the same sandboxed core to run bounded
//! whole-workspace Cargo check, clippy, and test passes. It returns typed
//! compiler locations and leaves test-program output uninterpreted. Cargo and
//! workspace build or test code share an output channel, so the collection is
//! labeled as workspace-influenced evidence. Its
//! `known_truncated` flag reports positive truncation evidence; false never
//! claims that the workspace-influenced collection is complete or authentic.
//!
//! The profile unshares the network namespace, imposes no resource limits,
//! drops no uid or gid, and applies no seccomp or landlock policy. It is
//! therefore not an admissible boundary for executing untrusted code; the
//! network fence narrows that gap rather than closing it. Exactly what the
//! profile does and does not confine is owned by
//! `docs/spec/configuration-and-credentials.md` and is not restated here.
//! When configured, the sandbox binds one pinned host Cargo registry read-only
//! below a private writable Cargo home. Without that explicit cache, a Cargo
//! pass through [`CargoDiagnosticsTool`] resolves only against an
//! already-populated workspace-local registry. Shell sessions and PTYs are
//! likewise outside this crate's contract.
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
    CargoDiagnosticsToolConstructionError, CargoEvidenceProvenance, CargoFailureDetail,
    InvalidCargoDiagnosticsArguments,
};
pub use process::{
    BwrapAvailability, CaptureCompleteness, ExecArguments, ExecExecutor, ExecExecutorError,
    ExecResult, ExecToolConstructionError, ExecutionConfinement, InvalidExecArguments,
    OutputCapture, OutputEncoding, ProcessEnvironment, ProcessOutcome, ProcessOutput,
    ProcessRequest, ProcessRunResult, ProcessRunner, ProcessSpawnFailure, ProcessStatusProtocol,
    ProcessSupervisionFailure, SANDBOXED_EXEC_NAME, SandboxProcessNamespace,
    SandboxedCommandRunner, SandboxedExecTool, TokioProcessRunner, UNSANDBOXED_EXEC_NAME,
    UnsandboxedCommandRunner, UnsandboxedExecTool,
};

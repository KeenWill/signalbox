//! Bounded direct command execution rooted at an injected workspace.
//!
//! [`SandboxedExecTool`] confines writable and project-visible filesystem
//! authority to the injected workspace with bubblewrap (`bwrap`). The sandbox
//! also exposes a small read-only operating-system runtime needed to start
//! ordinary programs. Missing, unsupported, or unusable `bwrap` is typed
//! refusal evidence and never falls back to unsandboxed execution.
//!
//! [`UnsandboxedExecTool`] is a separate catalog-composable tool whose fixed
//! permission default requires owner confirmation. This crate ships the two
//! tools independently and does not decide which session catalogs contain
//! either one.
//!
//! This day-one profile does not fence the network or impose resource limits.
//! A full profile providing both is a blocking condition before these tools may
//! execute untrusted code. Shell sessions and PTYs are likewise outside this
//! crate's contract.

mod process;

pub use process::{
    BwrapAvailability, CaptureCompleteness, ExecArguments, ExecExecutor, ExecExecutorError,
    ExecResult, ExecToolConstructionError, ExecutionConfinement, InvalidExecArguments,
    OutputCapture, OutputEncoding, ProcessOutcome, ProcessOutput, ProcessRequest, ProcessRunResult,
    ProcessRunner, ProcessSpawnFailure, ProcessSupervisionFailure, SANDBOXED_EXEC_NAME,
    SandboxedCommandRunner, SandboxedExecTool, TokioProcessRunner, UNSANDBOXED_EXEC_NAME,
    UnsandboxedCommandRunner, UnsandboxedExecTool,
};

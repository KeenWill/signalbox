// Wire status types shared, via `include!`, between `process.rs` (the
// supervising caller) and the `signalbox-exec-supervisor` binary (the
// supervised process) that emits them on its stdout trailer.
//
// `LauncherStatus` reports exit, spawn failure, or supervision failure;
// `SupervisorStatus` additionally reports timeout and cancellation. Only
// `Exited` carries `SupervisorCaptureCompleteness` for stdout and stderr.
//
// Kept as a plain comment, not a `//!` inner doc comment: this file is also
// spliced into `signalbox-exec-supervisor.rs` via
// `mod supervisor_protocol { include!(...) }`, and an inner doc comment
// produced through `include!` is rejected there (E0753) even though the same
// text compiles fine in this file's own `mod supervisor_protocol;` in
// `lib.rs`.

pub(crate) const LAUNCH_STATUS_TRAILER: &[u8] = b"\n\0signalbox-exec-launch-status:";
pub(crate) const LAUNCH_STATUS_TAIL_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum LauncherStatus {
    Exited {
        code: Option<i32>,
        stdout: SupervisorCaptureCompleteness,
        stderr: SupervisorCaptureCompleteness,
    },
    SpawnFailed {
        reason: SupervisorSpawnFailure,
    },
    DeliveryFailed,
    SupervisionFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum SupervisorStatus {
    Exited {
        code: Option<i32>,
        stdout: SupervisorCaptureCompleteness,
        stderr: SupervisorCaptureCompleteness,
    },
    TimedOut,
    Cancelled,
    SpawnFailed {
        reason: SupervisorSpawnFailure,
    },
    DeliveryFailed,
    SupervisionFailed {
        stage: SupervisorFailureStage,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum SupervisorCaptureCompleteness {
    Complete,
    Incomplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum SupervisorSpawnFailure {
    NotFound,
    PermissionDenied,
    ProcessTreeUnsupported,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum SupervisorFailureStage {
    Wait,
    Cleanup,
}

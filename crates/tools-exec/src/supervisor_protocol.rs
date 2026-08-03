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

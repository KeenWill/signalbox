#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum SupervisorStatus {
    Exited { code: Option<i32> },
    TimedOut,
    Cancelled,
    SpawnFailed { reason: SupervisorSpawnFailure },
    SupervisionFailed { stage: SupervisorFailureStage },
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

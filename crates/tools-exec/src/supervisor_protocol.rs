#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(crate) enum SupervisorStatus {
    Exited { code: Option<i32> },
    TimedOut,
    Cancelled,
    SpawnFailed { reason: SupervisorSpawnFailure },
    SupervisionFailed,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(crate) enum SupervisorSpawnFailure {
    NotFound,
    PermissionDenied,
    ProcessTreeUnsupported,
    Other,
}

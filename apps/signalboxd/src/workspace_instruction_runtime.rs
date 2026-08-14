//! Turn-start workspace-instruction discovery and durable provenance.

use std::{error::Error, fmt, path::Path};

use signalbox_application::{
    ClassifyOperatorFailure, InstructionDiscoveryRoot, OperatorFailureClass,
    discover_workspace_instructions,
};
use signalbox_domain::{
    InstructionBundleId, InstructionDiscoveryId, InstructionDiscoveryRootKind, InstructionPath,
    SessionId, TurnId, TurnInstructionManifest, TurnInstructionManifestId,
};
use signalbox_persistence::workspace_instructions::{
    RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepository,
    WorkspaceInstructionRepositoryError,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::daemon_tools::{SessionWorkspaceRoot, SessionWorkspaceRoots};

/// Failure before the daemon can prove which instruction snapshot a turn used.
#[derive(Debug)]
pub enum WorkspaceInstructionRuntimeError {
    /// A deployment-owned workspace path cannot be represented durably.
    InvalidWorkspacePath,
    /// The blocking filesystem scan task failed to join.
    DiscoveryTask(tokio::task::JoinError),
    /// Durable snapshot recording or authentication failed.
    Persistence(WorkspaceInstructionRepositoryError),
}

impl fmt::Display for WorkspaceInstructionRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWorkspacePath => {
                formatter.write_str("workspace instruction root is not a canonical UTF-8 path")
            }
            Self::DiscoveryTask(error) => error.fmt(formatter),
            Self::Persistence(error) => error.fmt(formatter),
        }
    }
}

impl Error for WorkspaceInstructionRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidWorkspacePath => None,
            Self::DiscoveryTask(error) => Some(error),
            Self::Persistence(error) => Some(error),
        }
    }
}

impl ClassifyOperatorFailure for WorkspaceInstructionRuntimeError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::InvalidWorkspacePath => OperatorFailureClass::CallerOrHubBug,
            Self::DiscoveryTask(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::Persistence(error) => error.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::InvalidWorkspacePath => "workspace_instruction_path",
            Self::DiscoveryTask(_) => "workspace_instruction_discovery_task",
            Self::Persistence(error) => error.operator_failure_cause_code(),
        }
    }
}

/// Daemon-owned discovery and turn-manifest composition.
#[derive(Clone, Debug)]
pub struct WorkspaceInstructionRuntime {
    repository: WorkspaceInstructionRepository,
    workspace_roots: Option<SessionWorkspaceRoots>,
    configured_roots: Box<[InstructionPath]>,
}

impl WorkspaceInstructionRuntime {
    /// Supplies persistence, session workspace derivation, and explicit roots.
    pub fn new(
        pool: PgPool,
        workspace_roots: Option<SessionWorkspaceRoots>,
        configured_roots: Vec<InstructionPath>,
    ) -> Self {
        Self {
            repository: WorkspaceInstructionRepository::new(pool),
            workspace_roots,
            configured_roots: configured_roots.into_boxed_slice(),
        }
    }

    /// Greedily scans and atomically records an empty turn-start manifest.
    ///
    /// `false` means the turn stopped being active before evidence could bind
    /// it; callers must do no model work for that stale activation.
    pub async fn prepare(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<bool, WorkspaceInstructionRuntimeError> {
        let mut roots = Vec::with_capacity(self.configured_roots.len() + 1);
        if let Some(workspace_roots) = &self.workspace_roots {
            let path = match workspace_roots.resolve(session) {
                SessionWorkspaceRoot::ConfiguredRoot => workspace_roots.configured().to_owned(),
                SessionWorkspaceRoot::Derived { path, .. } => path,
                SessionWorkspaceRoot::Unresolvable => workspace_roots.derived_path(session),
            };
            roots.push(InstructionDiscoveryRoot::new(
                InstructionDiscoveryRootKind::Workspace,
                instruction_path(&path)?,
            ));
        }
        roots.extend(self.configured_roots.iter().cloned().map(|path| {
            InstructionDiscoveryRoot::new(InstructionDiscoveryRootKind::Configured, path)
        }));
        let snapshot = tokio::task::spawn_blocking(move || discover_workspace_instructions(roots))
            .await
            .map_err(WorkspaceInstructionRuntimeError::DiscoveryTask)?;
        let discovery = InstructionDiscoveryId::from_uuid(Uuid::now_v7());
        let manifest = TurnInstructionManifest::empty_turn_start(
            TurnInstructionManifestId::from_uuid(Uuid::now_v7()),
            session,
            turn,
        );
        let outcome = self
            .repository
            .record_turn_start(discovery, manifest, &snapshot, || {
                InstructionBundleId::from_uuid(Uuid::now_v7())
            })
            .await
            .map_err(WorkspaceInstructionRuntimeError::Persistence)?;
        Ok(!matches!(
            outcome,
            RecordTurnInstructionSnapshotOutcome::TurnUnavailable
        ))
    }
}

fn instruction_path(path: &Path) -> Result<InstructionPath, WorkspaceInstructionRuntimeError> {
    let value = path
        .to_str()
        .ok_or(WorkspaceInstructionRuntimeError::InvalidWorkspacePath)?;
    InstructionPath::try_new(value.to_owned())
        .map_err(|_| WorkspaceInstructionRuntimeError::InvalidWorkspacePath)
}

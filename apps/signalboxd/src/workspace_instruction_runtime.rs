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
    CountedActivationInstructionEvidence, RecordTurnInstructionSnapshotOutcome,
    TurnInstructionManifestPreflight, WorkspaceInstructionPlacementObservation,
    WorkspaceInstructionRepository, WorkspaceInstructionRepositoryError,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::daemon_tools::WorkspaceInstructionRootResolver;

/// Failure before the daemon can prove which instruction snapshot a turn used.
#[derive(Debug)]
pub enum WorkspaceInstructionRuntimeError {
    /// A deployment-owned workspace path cannot be represented durably.
    InvalidWorkspacePath,
    /// The session's configured daemon-local workspace is misprovisioned.
    UnresolvableWorkspace,
    /// The blocking filesystem scan task failed to join.
    DiscoveryTask(tokio::task::JoinError),
    /// A fixed scan safety limit prevented a complete inventory.
    DiscoveryIncomplete,
    /// Durable snapshot recording or authentication failed.
    Persistence(WorkspaceInstructionRepositoryError),
}

impl fmt::Display for WorkspaceInstructionRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWorkspacePath => {
                formatter.write_str("workspace instruction root is not a canonical UTF-8 path")
            }
            Self::UnresolvableWorkspace => formatter
                .write_str("session workspace could not be resolved for instruction discovery"),
            Self::DiscoveryTask(error) => error.fmt(formatter),
            Self::DiscoveryIncomplete => {
                formatter.write_str("workspace instruction discovery limit was reached")
            }
            Self::Persistence(error) => error.fmt(formatter),
        }
    }
}

impl Error for WorkspaceInstructionRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidWorkspacePath => None,
            Self::UnresolvableWorkspace => None,
            Self::DiscoveryTask(error) => Some(error),
            Self::DiscoveryIncomplete => None,
            Self::Persistence(error) => Some(error),
        }
    }
}

impl ClassifyOperatorFailure for WorkspaceInstructionRuntimeError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::InvalidWorkspacePath => OperatorFailureClass::CallerOrHubBug,
            Self::UnresolvableWorkspace => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::DiscoveryTask(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::DiscoveryIncomplete => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::Persistence(error) => error.operator_failure_class(),
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::InvalidWorkspacePath => "workspace_instruction_path",
            Self::UnresolvableWorkspace => "workspace_instruction_workspace_unresolvable",
            Self::DiscoveryTask(_) => "workspace_instruction_discovery_task",
            Self::DiscoveryIncomplete => "workspace_instruction_discovery_limit",
            Self::Persistence(error) => error.operator_failure_cause_code(),
        }
    }
}

/// Daemon-owned discovery and turn-manifest composition.
#[derive(Clone, Debug)]
pub struct WorkspaceInstructionRuntime {
    repository: WorkspaceInstructionRepository,
    workspace_root: Option<WorkspaceInstructionRootResolver>,
    configured_roots: Box<[InstructionPath]>,
}

/// Complete filesystem evidence retained only until a counted activation
/// transaction either commits it or rejects the stale preview.
#[derive(Debug)]
pub(crate) struct PreparedCountedActivationInstructions {
    discovery: InstructionDiscoveryId,
    manifest: TurnInstructionManifest,
    snapshot: signalbox_application::InstructionDiscoverySnapshot,
    bundle_ids: Box<[InstructionBundleId]>,
    placement: WorkspaceInstructionPlacementObservation,
}

impl PreparedCountedActivationInstructions {
    pub(crate) fn evidence(&self) -> CountedActivationInstructionEvidence<'_> {
        CountedActivationInstructionEvidence::new(
            self.discovery,
            &self.manifest,
            &self.snapshot,
            &self.bundle_ids,
            &self.placement,
        )
    }
}

impl WorkspaceInstructionRuntime {
    /// Supplies persistence, session workspace derivation, and explicit roots.
    pub fn new(
        pool: PgPool,
        workspace_root: Option<WorkspaceInstructionRootResolver>,
        configured_roots: Vec<InstructionPath>,
    ) -> Self {
        Self {
            repository: WorkspaceInstructionRepository::new(pool),
            workspace_root,
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
        match self
            .repository
            .preflight_turn_start(session, turn)
            .await
            .map_err(WorkspaceInstructionRuntimeError::Persistence)?
        {
            TurnInstructionManifestPreflight::Available(_) => return Ok(true),
            TurnInstructionManifestPreflight::TurnUnavailable => return Ok(false),
            TurnInstructionManifestPreflight::Absent => {}
        }
        let (snapshot, placement) = self.discover(session).await?;
        let discovery = InstructionDiscoveryId::from_uuid(Uuid::now_v7());
        let manifest = TurnInstructionManifest::empty_turn_start(
            TurnInstructionManifestId::from_uuid(Uuid::now_v7()),
            session,
            turn,
        );
        let outcome = self
            .repository
            .record_turn_start_for_observed_placement(
                discovery,
                manifest,
                &snapshot,
                &placement,
                || InstructionBundleId::from_uuid(Uuid::now_v7()),
            )
            .await
            .map_err(WorkspaceInstructionRuntimeError::Persistence)?;
        outcome_is_available(outcome)
    }

    /// Prepares complete evidence for the counted activation transaction.
    ///
    /// An incomplete scan remains durable diagnostic evidence without binding
    /// a manifest. Complete evidence is returned without persistence so a
    /// stale preview cannot leave an authoritative snapshot behind.
    pub(crate) async fn prepare_counted_activation(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<Option<PreparedCountedActivationInstructions>, WorkspaceInstructionRuntimeError>
    {
        match self
            .repository
            .preflight_counted_activation(session, turn)
            .await
            .map_err(WorkspaceInstructionRuntimeError::Persistence)?
        {
            TurnInstructionManifestPreflight::Available(_) => {
                return Err(WorkspaceInstructionRuntimeError::Persistence(
                    WorkspaceInstructionRepositoryError::Corruption(
                        "queued counted activation manifest preexisted",
                    ),
                ));
            }
            TurnInstructionManifestPreflight::TurnUnavailable => return Ok(None),
            TurnInstructionManifestPreflight::Absent => {}
        }
        let (snapshot, placement) = self.discover(session).await?;
        let discovery = InstructionDiscoveryId::from_uuid(Uuid::now_v7());
        let manifest = TurnInstructionManifest::empty_turn_start(
            TurnInstructionManifestId::from_uuid(Uuid::now_v7()),
            session,
            turn,
        );
        if !snapshot.is_complete() {
            let outcome = self
                .repository
                .record_counted_activation_for_observed_placement(
                    discovery,
                    manifest,
                    &snapshot,
                    &placement,
                    || InstructionBundleId::from_uuid(Uuid::now_v7()),
                )
                .await
                .map_err(WorkspaceInstructionRuntimeError::Persistence)?;
            return match outcome {
                RecordTurnInstructionSnapshotOutcome::DiscoveryIncomplete => {
                    Err(WorkspaceInstructionRuntimeError::DiscoveryIncomplete)
                }
                RecordTurnInstructionSnapshotOutcome::TurnUnavailable => Ok(None),
                RecordTurnInstructionSnapshotOutcome::Recorded(_)
                | RecordTurnInstructionSnapshotOutcome::AlreadyRecorded(_) => {
                    Err(WorkspaceInstructionRuntimeError::Persistence(
                        WorkspaceInstructionRepositoryError::Corruption(
                            "incomplete counted activation bound a manifest",
                        ),
                    ))
                }
            };
        }
        let bundle_ids = snapshot
            .bundles()
            .iter()
            .map(|_| InstructionBundleId::from_uuid(Uuid::now_v7()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Some(PreparedCountedActivationInstructions {
            discovery,
            manifest,
            snapshot,
            bundle_ids,
            placement,
        }))
    }

    async fn discover(
        &self,
        session: SessionId,
    ) -> Result<
        (
            signalbox_application::InstructionDiscoverySnapshot,
            WorkspaceInstructionPlacementObservation,
        ),
        WorkspaceInstructionRuntimeError,
    > {
        let placement = self
            .repository
            .observe_session_runner_placement(session)
            .await
            .map_err(WorkspaceInstructionRuntimeError::Persistence)?;
        let runner_placed = placement.runner_owned();
        let mut roots =
            Vec::with_capacity(self.configured_roots.len() + usize::from(!runner_placed));
        let workspace_binding = if !runner_placed && let Some(workspace_root) = &self.workspace_root
        {
            let path = workspace_root
                .resolve(session)
                .await
                .map_err(|_| WorkspaceInstructionRuntimeError::UnresolvableWorkspace)?;
            roots.push(InstructionDiscoveryRoot::new(
                InstructionDiscoveryRootKind::Workspace,
                instruction_path(&path)?,
            ));
            Some((workspace_root.clone(), path))
        } else {
            None
        };
        roots.extend(self.configured_roots.iter().cloned().map(|path| {
            InstructionDiscoveryRoot::new(InstructionDiscoveryRootKind::Configured, path)
        }));
        let snapshot = tokio::task::spawn_blocking(move || discover_workspace_instructions(roots))
            .await
            .map_err(WorkspaceInstructionRuntimeError::DiscoveryTask)?;
        if let Some((workspace_root, expected_path)) = workspace_binding {
            let revalidated_path = workspace_root
                .resolve(session)
                .await
                .map_err(|_| WorkspaceInstructionRuntimeError::UnresolvableWorkspace)?;
            if revalidated_path != expected_path {
                return Err(WorkspaceInstructionRuntimeError::UnresolvableWorkspace);
            }
        }
        Ok((snapshot, placement))
    }
}

fn outcome_is_available(
    outcome: RecordTurnInstructionSnapshotOutcome,
) -> Result<bool, WorkspaceInstructionRuntimeError> {
    match outcome {
        RecordTurnInstructionSnapshotOutcome::Recorded(_)
        | RecordTurnInstructionSnapshotOutcome::AlreadyRecorded(_) => Ok(true),
        RecordTurnInstructionSnapshotOutcome::DiscoveryIncomplete => {
            Err(WorkspaceInstructionRuntimeError::DiscoveryIncomplete)
        }
        RecordTurnInstructionSnapshotOutcome::TurnUnavailable => Ok(false),
    }
}

fn instruction_path(path: &Path) -> Result<InstructionPath, WorkspaceInstructionRuntimeError> {
    let value = path
        .to_str()
        .ok_or(WorkspaceInstructionRuntimeError::InvalidWorkspacePath)?;
    InstructionPath::try_new(value.to_owned())
        .map_err(|_| WorkspaceInstructionRuntimeError::InvalidWorkspacePath)
}

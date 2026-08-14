//! Append-only PostgreSQL storage for workspace instruction snapshots.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    ClassifyOperatorFailure, InstructionDiscoveryFindingKind, InstructionDiscoverySnapshot,
    OperatorFailureClass,
};
use signalbox_domain::{
    InstructionBundleId, InstructionBundleKind, InstructionDigest, InstructionDiscoveryId,
    InstructionDiscoveryRootKind, SessionId, TurnId, TurnInstructionManifest,
    TurnInstructionManifestId,
};
use sqlx::{PgPool, Row, types::Uuid};

/// Result of idempotently recording one turn-start snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordTurnInstructionSnapshotOutcome {
    Recorded(TurnInstructionManifestId),
    AlreadyRecorded(TurnInstructionManifestId),
    TurnUnavailable,
}

/// Storage or authentication failure at the instruction boundary.
#[derive(Debug)]
pub enum WorkspaceInstructionRepositoryError {
    Database(sqlx::Error),
    Corruption(&'static str),
}

impl fmt::Display for WorkspaceInstructionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::Corruption(reason) => {
                write!(formatter, "workspace instruction corruption: {reason}")
            }
        }
    }
}

impl Error for WorkspaceInstructionRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for WorkspaceInstructionRepositoryError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}

impl ClassifyOperatorFailure for WorkspaceInstructionRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        "workspace_instruction_persistence"
    }
}

/// Durable registration and turn-manifest adapter.
#[derive(Clone, Debug)]
pub struct WorkspaceInstructionRepository {
    pool: PgPool,
}

impl WorkspaceInstructionRepository {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records one complete scan and empty turn-start manifest atomically.
    pub async fn record_turn_start<NextBundleId>(
        &self,
        discovery: InstructionDiscoveryId,
        manifest: TurnInstructionManifest,
        snapshot: &InstructionDiscoverySnapshot,
        mut next_bundle_id: NextBundleId,
    ) -> Result<RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepositoryError>
    where
        NextBundleId: FnMut() -> InstructionBundleId,
    {
        let mut transaction = self.pool.begin().await?;
        let state = sqlx::query_scalar::<_, String>(
            "SELECT state_kind
               FROM turn_lifecycle
              WHERE session_id = $1 AND turn_id = $2
              FOR UPDATE",
        )
        .bind(manifest.session().into_uuid())
        .bind(manifest.turn().into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        if state.as_deref() != Some("active") {
            transaction.rollback().await?;
            return Ok(RecordTurnInstructionSnapshotOutcome::TurnUnavailable);
        }
        if let Some(existing) =
            load_manifest(&mut transaction, manifest.session(), manifest.turn()).await?
        {
            transaction.rollback().await?;
            return Ok(RecordTurnInstructionSnapshotOutcome::AlreadyRecorded(
                existing.id(),
            ));
        }
        sqlx::query(
            "INSERT INTO instruction_discovery
                (instruction_discovery_id, session_id, turn_id)
             VALUES ($1, $2, $3)",
        )
        .bind(discovery.into_uuid())
        .bind(manifest.session().into_uuid())
        .bind(manifest.turn().into_uuid())
        .execute(&mut *transaction)
        .await?;
        for (index, root) in snapshot.roots().iter().enumerate() {
            sqlx::query(
                "INSERT INTO instruction_discovery_root
                    (instruction_discovery_id, root_ordinal, root_kind, root_path)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(discovery.into_uuid())
            .bind((index + 1) as i64)
            .bind(root_kind(root.kind()))
            .bind(root.path().as_str())
            .execute(&mut *transaction)
            .await?;
        }
        for (index, bundle) in snapshot.bundles().iter().enumerate() {
            let candidate = next_bundle_id();
            let skill = bundle.skill();
            let registered = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO registered_instruction_bundle
                        (instruction_bundle_id, root_kind, root_path, source_path,
                         bundle_kind, skill_name, skill_description, source_byte_length,
                         source_hash_algorithm, source_hash)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'sha256_v1', $9)
                 ON CONFLICT (root_kind, root_path, source_path, source_hash) DO NOTHING
                 RETURNING instruction_bundle_id",
            )
            .bind(candidate.into_uuid())
            .bind(root_kind(bundle.root_kind()))
            .bind(bundle.root_path().as_str())
            .bind(bundle.source_path().as_str())
            .bind(bundle_kind(bundle.kind()))
            .bind(skill.map(signalbox_domain::InstructionSkillMetadata::name))
            .bind(skill.map(signalbox_domain::InstructionSkillMetadata::description))
            .bind(Decimal::from(bundle.source_bytes()))
            .bind(bundle.source_hash().as_bytes().as_slice())
            .fetch_optional(&mut *transaction)
            .await?;
            let registered = match registered {
                Some(registered) => registered,
                None => {
                    sqlx::query_scalar::<_, Uuid>(
                        "SELECT instruction_bundle_id
                       FROM registered_instruction_bundle
                      WHERE root_kind = $1 AND root_path = $2
                        AND source_path = $3 AND source_hash = $4",
                    )
                    .bind(root_kind(bundle.root_kind()))
                    .bind(bundle.root_path().as_str())
                    .bind(bundle.source_path().as_str())
                    .bind(bundle.source_hash().as_bytes().as_slice())
                    .fetch_one(&mut *transaction)
                    .await?
                }
            };
            sqlx::query(
                "INSERT INTO instruction_discovery_candidate
                    (instruction_discovery_id, candidate_ordinal, instruction_bundle_id)
                 VALUES ($1, $2, $3)",
            )
            .bind(discovery.into_uuid())
            .bind((index + 1) as i64)
            .bind(registered)
            .execute(&mut *transaction)
            .await?;
        }
        for (index, finding) in snapshot.findings().iter().enumerate() {
            sqlx::query(
                "INSERT INTO instruction_discovery_finding
                    (instruction_discovery_id, finding_ordinal, source_path, finding_kind)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(discovery.into_uuid())
            .bind((index + 1) as i64)
            .bind(finding.path().as_str())
            .bind(finding_kind(finding.kind()))
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query(
            "INSERT INTO turn_instruction_manifest
                (turn_instruction_manifest_id, session_id, turn_id,
                 instruction_discovery_id, boundary_kind,
                 eligibility_hash_algorithm, eligibility_hash,
                 manifest_hash_algorithm, manifest_hash)
             VALUES ($1, $2, $3, $4, 'turn_start', 'sha256_v1', $5, 'sha256_v1', $6)",
        )
        .bind(manifest.id().into_uuid())
        .bind(manifest.session().into_uuid())
        .bind(manifest.turn().into_uuid())
        .bind(discovery.into_uuid())
        .bind(manifest.eligibility_hash().as_bytes().as_slice())
        .bind(manifest.manifest_hash().as_bytes().as_slice())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(RecordTurnInstructionSnapshotOutcome::Recorded(
            manifest.id(),
        ))
    }
}

async fn load_manifest(
    connection: &mut sqlx::PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<TurnInstructionManifest>, WorkspaceInstructionRepositoryError> {
    let row = sqlx::query(
        "SELECT turn_instruction_manifest_id, eligibility_hash, manifest_hash
           FROM turn_instruction_manifest
          WHERE session_id = $1 AND turn_id = $2 AND boundary_kind = 'turn_start'",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_optional(connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id = TurnInstructionManifestId::from_uuid(row.try_get("turn_instruction_manifest_id")?);
    let eligibility_hash = digest(row.try_get("eligibility_hash")?)?;
    let manifest_hash = digest(row.try_get("manifest_hash")?)?;
    TurnInstructionManifest::reconstitute_empty_turn_start(
        id,
        session,
        turn,
        eligibility_hash,
        manifest_hash,
    )
    .map(Some)
    .ok_or(WorkspaceInstructionRepositoryError::Corruption(
        "manifest hash",
    ))
}

fn digest(bytes: Vec<u8>) -> Result<InstructionDigest, WorkspaceInstructionRepositoryError> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| WorkspaceInstructionRepositoryError::Corruption("digest length"))?;
    Ok(InstructionDigest::from_sha256(bytes))
}

const fn root_kind(kind: InstructionDiscoveryRootKind) -> &'static str {
    match kind {
        InstructionDiscoveryRootKind::Workspace => "workspace",
        InstructionDiscoveryRootKind::Configured => "configured",
    }
}

const fn bundle_kind(kind: InstructionBundleKind) -> &'static str {
    match kind {
        InstructionBundleKind::AgentDocument => "agent_document",
        InstructionBundleKind::AgentSkill => "agent_skill",
    }
}

const fn finding_kind(kind: InstructionDiscoveryFindingKind) -> &'static str {
    match kind {
        InstructionDiscoveryFindingKind::RootUnavailable => "root_unavailable",
        InstructionDiscoveryFindingKind::EntryUnreadable => "entry_unreadable",
        InstructionDiscoveryFindingKind::NonUtf8Source => "non_utf8_source",
        InstructionDiscoveryFindingKind::InvalidSkill => "invalid_skill",
    }
}

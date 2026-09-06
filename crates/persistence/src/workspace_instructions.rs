//! Append-only PostgreSQL storage for workspace instruction snapshots.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    ClassifyOperatorFailure, InstructionDiscoverySnapshot, OperatorFailureClass,
};
use signalbox_domain::{
    EmptyTurnInstructionManifestEvidence, InstructionBundleId, InstructionDigest,
    InstructionDiscoveryId, SessionId, TurnId, TurnInstructionManifest, TurnInstructionManifestId,
};
use sqlx::{PgPool, Row, types::Uuid};

use crate::mapping::{
    WorkspaceInstructionAuthorityStorageKind, instruction_bundle_kind_to_str,
    instruction_finding_kind_to_str, instruction_root_kind_to_str,
    workspace_instruction_authority_from_placement_state,
};

/// Result of idempotently recording one turn-start snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordTurnInstructionSnapshotOutcome {
    Recorded(TurnInstructionManifestId),
    AlreadyRecorded(TurnInstructionManifestId),
    DiscoveryIncomplete,
    TurnUnavailable,
}

/// Result of authenticating durable turn-start evidence before discovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnInstructionManifestPreflight {
    /// No manifest is yet bound to this available turn.
    Absent,
    /// One complete canonical manifest is already bound to this turn.
    Available(TurnInstructionManifestId),
    /// The turn is absent or no longer in the required lifecycle state.
    TurnUnavailable,
}

/// Placement head sampled before a workspace-instruction filesystem scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceInstructionPlacementObservation {
    head: Option<Decimal>,
    runner_owned: bool,
}

#[derive(sqlx::FromRow)]
struct WorkspaceInstructionPlacementRow {
    event_ordinal: Option<Decimal>,
    state_kind: Option<String>,
}

impl WorkspaceInstructionPlacementObservation {
    /// Reports whether the sampled placement gave workspace authority to a runner.
    pub const fn runner_owned(&self) -> bool {
        self.runner_owned
    }
}

/// One complete queued-turn snapshot prepared for the activation transaction.
#[derive(Debug, Clone, Copy)]
pub struct CountedActivationInstructionEvidence<'a> {
    discovery: InstructionDiscoveryId,
    manifest: &'a TurnInstructionManifest,
    snapshot: &'a InstructionDiscoverySnapshot,
    bundle_ids: &'a [InstructionBundleId],
    placement: &'a WorkspaceInstructionPlacementObservation,
}

impl<'a> CountedActivationInstructionEvidence<'a> {
    /// Binds exact discovery content and registration identities for atomic
    /// counted activation.
    pub const fn new(
        discovery: InstructionDiscoveryId,
        manifest: &'a TurnInstructionManifest,
        snapshot: &'a InstructionDiscoverySnapshot,
        bundle_ids: &'a [InstructionBundleId],
        placement: &'a WorkspaceInstructionPlacementObservation,
    ) -> Self {
        Self {
            discovery,
            manifest,
            snapshot,
            bundle_ids,
            placement,
        }
    }
}

/// Storage or authentication failure at the instruction boundary.
#[derive(Debug)]
pub enum WorkspaceInstructionRepositoryError {
    Database {
        source: sqlx::Error,
        commit_ambiguous: bool,
    },
    PlacementChanged,
    Corruption(&'static str),
}

impl fmt::Display for WorkspaceInstructionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => source.fmt(formatter),
            Self::PlacementChanged => {
                formatter.write_str("runner placement changed during workspace discovery")
            }
            Self::Corruption(reason) => {
                write!(formatter, "workspace instruction corruption: {reason}")
            }
        }
    }
}

impl Error for WorkspaceInstructionRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::PlacementChanged => None,
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for WorkspaceInstructionRepositoryError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database {
            source: value,
            commit_ambiguous: false,
        }
    }
}

impl WorkspaceInstructionRepositoryError {
    fn ambiguous_commit(source: sqlx::Error) -> Self {
        Self::Database {
            source,
            commit_ambiguous: true,
        }
    }
}

impl ClassifyOperatorFailure for WorkspaceInstructionRepositoryError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => OperatorFailureClass::Infrastructure {
                commit_ambiguous: *commit_ambiguous,
            },
            Self::PlacementChanged => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
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

    /// Authenticates existing evidence for one active turn before discovery.
    pub async fn preflight_turn_start(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<TurnInstructionManifestPreflight, WorkspaceInstructionRepositoryError> {
        self.preflight_for_state(session, turn, "active").await
    }

    /// Authenticates existing evidence for one queued counted activation.
    pub async fn preflight_counted_activation(
        &self,
        session: SessionId,
        turn: TurnId,
    ) -> Result<TurnInstructionManifestPreflight, WorkspaceInstructionRepositoryError> {
        self.preflight_for_state(session, turn, "queued").await
    }

    /// Records one complete scan and empty turn-start manifest atomically.
    pub async fn record_turn_start<NextBundleId>(
        &self,
        discovery: InstructionDiscoveryId,
        manifest: TurnInstructionManifest,
        snapshot: &InstructionDiscoverySnapshot,
        next_bundle_id: NextBundleId,
    ) -> Result<RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepositoryError>
    where
        NextBundleId: FnMut() -> InstructionBundleId,
    {
        let placement = self
            .observe_session_runner_placement(manifest.session())
            .await?;
        self.record_turn_start_for_observed_placement(
            discovery,
            manifest,
            snapshot,
            &placement,
            next_bundle_id,
        )
        .await
    }

    /// Records a turn-start snapshot only while its sampled placement is current.
    pub async fn record_turn_start_for_observed_placement<NextBundleId>(
        &self,
        discovery: InstructionDiscoveryId,
        manifest: TurnInstructionManifest,
        snapshot: &InstructionDiscoverySnapshot,
        placement: &WorkspaceInstructionPlacementObservation,
        next_bundle_id: NextBundleId,
    ) -> Result<RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepositoryError>
    where
        NextBundleId: FnMut() -> InstructionBundleId,
    {
        self.record_for_state(
            discovery,
            manifest,
            snapshot,
            placement,
            next_bundle_id,
            "active",
        )
        .await
    }

    /// Records the empty manifest required by one counted activation while its
    /// candidate is still queued.
    pub async fn record_counted_activation<NextBundleId>(
        &self,
        discovery: InstructionDiscoveryId,
        manifest: TurnInstructionManifest,
        snapshot: &InstructionDiscoverySnapshot,
        next_bundle_id: NextBundleId,
    ) -> Result<RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepositoryError>
    where
        NextBundleId: FnMut() -> InstructionBundleId,
    {
        let placement = self
            .observe_session_runner_placement(manifest.session())
            .await?;
        self.record_counted_activation_for_observed_placement(
            discovery,
            manifest,
            snapshot,
            &placement,
            next_bundle_id,
        )
        .await
    }

    /// Records queued-turn evidence only while its sampled placement is current.
    pub async fn record_counted_activation_for_observed_placement<NextBundleId>(
        &self,
        discovery: InstructionDiscoveryId,
        manifest: TurnInstructionManifest,
        snapshot: &InstructionDiscoverySnapshot,
        placement: &WorkspaceInstructionPlacementObservation,
        next_bundle_id: NextBundleId,
    ) -> Result<RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepositoryError>
    where
        NextBundleId: FnMut() -> InstructionBundleId,
    {
        self.record_for_state(
            discovery,
            manifest,
            snapshot,
            placement,
            next_bundle_id,
            "queued",
        )
        .await
    }

    /// Samples the exact placement head that decides workspace authority.
    pub async fn observe_session_runner_placement(
        &self,
        session: SessionId,
    ) -> Result<WorkspaceInstructionPlacementObservation, WorkspaceInstructionRepositoryError> {
        placement_observation(
            sqlx::query_as::<_, WorkspaceInstructionPlacementRow>(
                "SELECT current_placement.event_ordinal, placement.state_kind
                   FROM session_scheduler AS scheduler
                   LEFT JOIN runner_current_session_placement AS current_placement
                     ON current_placement.session_id = scheduler.session_id
                   LEFT JOIN runner_session_placement_record AS placement
                     ON placement.session_id = current_placement.session_id
                    AND placement.event_ordinal = current_placement.event_ordinal
                  WHERE scheduler.session_id = $1",
            )
            .bind(session.into_uuid())
            .fetch_optional(&self.pool)
            .await?,
        )
    }

    /// Reports whether the currently sampled placement gives workspace authority to a runner.
    pub async fn session_has_runner_placement(
        &self,
        session: SessionId,
    ) -> Result<bool, WorkspaceInstructionRepositoryError> {
        Ok(self
            .observe_session_runner_placement(session)
            .await?
            .runner_owned())
    }

    /// Inserts one complete instruction snapshot inside the transaction that
    /// has already revalidated and activated its counted preview.
    pub(crate) async fn record_counted_activation_in_transaction(
        connection: &mut sqlx::PgConnection,
        evidence: CountedActivationInstructionEvidence<'_>,
    ) -> Result<(), WorkspaceInstructionRepositoryError> {
        let CountedActivationInstructionEvidence {
            discovery,
            manifest,
            snapshot,
            bundle_ids,
            placement,
        } = evidence;
        if !snapshot.is_complete() {
            return Err(WorkspaceInstructionRepositoryError::Corruption(
                "counted activation discovery incomplete",
            ));
        }
        if bundle_ids.len() != snapshot.bundles().len() {
            return Err(WorkspaceInstructionRepositoryError::Corruption(
                "counted activation bundle identities",
            ));
        }
        let mut bundle_ids = bundle_ids.iter().copied();
        let outcome = Self::record_for_state_in_connection(
            connection,
            discovery,
            manifest,
            snapshot,
            placement,
            || {
                bundle_ids
                    .next()
                    .ok_or(WorkspaceInstructionRepositoryError::Corruption(
                        "counted activation bundle identity exhausted",
                    ))
            },
            "active",
        )
        .await?;
        match outcome {
            RecordTurnInstructionSnapshotOutcome::Recorded(recorded)
                if recorded == manifest.id() =>
            {
                Ok(())
            }
            RecordTurnInstructionSnapshotOutcome::Recorded(_) => {
                Err(WorkspaceInstructionRepositoryError::Corruption(
                    "counted activation manifest identity",
                ))
            }
            RecordTurnInstructionSnapshotOutcome::AlreadyRecorded(_) => {
                Err(WorkspaceInstructionRepositoryError::Corruption(
                    "counted activation manifest preexisted",
                ))
            }
            RecordTurnInstructionSnapshotOutcome::DiscoveryIncomplete => {
                Err(WorkspaceInstructionRepositoryError::Corruption(
                    "counted activation discovery completeness",
                ))
            }
            RecordTurnInstructionSnapshotOutcome::TurnUnavailable => {
                Err(WorkspaceInstructionRepositoryError::Corruption(
                    "counted activation turn unavailable",
                ))
            }
        }
    }

    async fn record_for_state<NextBundleId>(
        &self,
        discovery: InstructionDiscoveryId,
        manifest: TurnInstructionManifest,
        snapshot: &InstructionDiscoverySnapshot,
        placement: &WorkspaceInstructionPlacementObservation,
        mut next_bundle_id: NextBundleId,
        required_state: &'static str,
    ) -> Result<RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepositoryError>
    where
        NextBundleId: FnMut() -> InstructionBundleId,
    {
        let mut transaction = self.pool.begin().await?;
        let outcome = Self::record_for_state_in_connection(
            &mut transaction,
            discovery,
            &manifest,
            snapshot,
            placement,
            || Ok(next_bundle_id()),
            required_state,
        )
        .await?;
        match outcome {
            RecordTurnInstructionSnapshotOutcome::Recorded(_) => transaction
                .commit()
                .await
                .map_err(WorkspaceInstructionRepositoryError::ambiguous_commit)?,
            RecordTurnInstructionSnapshotOutcome::DiscoveryIncomplete => {
                transaction.commit().await?;
            }
            RecordTurnInstructionSnapshotOutcome::AlreadyRecorded(_)
            | RecordTurnInstructionSnapshotOutcome::TurnUnavailable => {
                transaction.rollback().await?;
            }
        }
        Ok(outcome)
    }

    async fn record_for_state_in_connection<NextBundleId>(
        connection: &mut sqlx::PgConnection,
        discovery: InstructionDiscoveryId,
        manifest: &TurnInstructionManifest,
        snapshot: &InstructionDiscoverySnapshot,
        placement: &WorkspaceInstructionPlacementObservation,
        mut next_bundle_id: NextBundleId,
        required_state: &'static str,
    ) -> Result<RecordTurnInstructionSnapshotOutcome, WorkspaceInstructionRepositoryError>
    where
        NextBundleId: FnMut() -> Result<InstructionBundleId, WorkspaceInstructionRepositoryError>,
    {
        if !turn_is_available(
            connection,
            manifest.session(),
            manifest.turn(),
            required_state,
        )
        .await?
        {
            return Ok(RecordTurnInstructionSnapshotOutcome::TurnUnavailable);
        }
        let current_placement =
            placement_observation_in_connection(connection, manifest.session()).await?;
        if &current_placement != placement {
            return Err(WorkspaceInstructionRepositoryError::PlacementChanged);
        }
        if let Some((existing, complete)) =
            load_manifest(connection, manifest.session(), manifest.turn()).await?
        {
            return Ok(if complete {
                RecordTurnInstructionSnapshotOutcome::AlreadyRecorded(existing.id())
            } else {
                RecordTurnInstructionSnapshotOutcome::DiscoveryIncomplete
            });
        }
        let classified_entries = i64::try_from(snapshot.classified_entries())
            .map_err(|_| WorkspaceInstructionRepositoryError::Corruption("entry count"))?;
        let finding_count = i64::try_from(snapshot.findings().len())
            .map_err(|_| WorkspaceInstructionRepositoryError::Corruption("finding count"))?;
        let candidate_source_bytes = i64::try_from(snapshot.candidate_source_bytes())
            .map_err(|_| WorkspaceInstructionRepositoryError::Corruption("source byte count"))?;
        let elapsed_millis = i64::try_from(snapshot.elapsed_millis())
            .map_err(|_| WorkspaceInstructionRepositoryError::Corruption("elapsed time"))?;
        for (index, root) in snapshot.roots().iter().enumerate() {
            sqlx::query(
                "INSERT INTO instruction_discovery_root
                    (instruction_discovery_id, root_ordinal, root_kind, root_path)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(discovery.into_uuid())
            .bind((index + 1) as i64)
            .bind(instruction_root_kind_to_str(root.kind()))
            .bind(root.path().as_str())
            .execute(&mut *connection)
            .await?;
        }
        for (index, bundle) in snapshot.bundles().iter().enumerate() {
            let candidate = next_bundle_id()?;
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
            .bind(instruction_root_kind_to_str(bundle.root_kind()))
            .bind(bundle.root_path().as_str())
            .bind(bundle.source_path().absolute_path())
            .bind(instruction_bundle_kind_to_str(bundle.kind()))
            .bind(skill.map(signalbox_domain::InstructionSkillMetadata::name))
            .bind(skill.map(signalbox_domain::InstructionSkillMetadata::description))
            .bind(Decimal::from(bundle.source_bytes()))
            .bind(bundle.source_hash().as_bytes().as_slice())
            .fetch_optional(&mut *connection)
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
                    .bind(instruction_root_kind_to_str(bundle.root_kind()))
                    .bind(bundle.root_path().as_str())
                    .bind(bundle.source_path().absolute_path())
                    .bind(bundle.source_hash().as_bytes().as_slice())
                    .fetch_one(&mut *connection)
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
            .execute(&mut *connection)
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
            .bind(instruction_finding_kind_to_str(finding.kind()))
            .execute(&mut *connection)
            .await?;
        }
        sqlx::query(
            "INSERT INTO instruction_discovery
                (instruction_discovery_id, session_id, turn_id,
                 limit_set_version, classified_entry_count, finding_count,
                 candidate_source_byte_count, elapsed_millis, scan_complete)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(discovery.into_uuid())
        .bind(manifest.session().into_uuid())
        .bind(manifest.turn().into_uuid())
        .bind(
            i16::try_from(snapshot.limit_set_version()).map_err(|_| {
                WorkspaceInstructionRepositoryError::Corruption("limit set version")
            })?,
        )
        .bind(classified_entries)
        .bind(finding_count)
        .bind(candidate_source_bytes)
        .bind(elapsed_millis)
        .bind(snapshot.is_complete())
        .execute(&mut *connection)
        .await?;
        if !snapshot.is_complete() {
            return Ok(RecordTurnInstructionSnapshotOutcome::DiscoveryIncomplete);
        }
        sqlx::query(
            "INSERT INTO turn_instruction_manifest
                (turn_instruction_manifest_id, session_id, turn_id,
                 instruction_discovery_id, boundary_kind,
                 eligibility_hash_algorithm, eligibility_hash,
                 admitted_set_hash_algorithm, admitted_set_hash,
                 manifest_hash_algorithm, manifest_hash)
             VALUES ($1, $2, $3, $4, 'turn_start', 'sha256_v1', $5, 'sha256_v1', $6,
                     'sha256_v1', $7)",
        )
        .bind(manifest.id().into_uuid())
        .bind(manifest.session().into_uuid())
        .bind(manifest.turn().into_uuid())
        .bind(discovery.into_uuid())
        .bind(manifest.eligibility_hash().as_bytes().as_slice())
        .bind(manifest.admitted_set_hash().as_bytes().as_slice())
        .bind(manifest.manifest_hash().as_bytes().as_slice())
        .execute(&mut *connection)
        .await?;
        Ok(RecordTurnInstructionSnapshotOutcome::Recorded(
            manifest.id(),
        ))
    }

    async fn preflight_for_state(
        &self,
        session: SessionId,
        turn: TurnId,
        required_state: &'static str,
    ) -> Result<TurnInstructionManifestPreflight, WorkspaceInstructionRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        if !turn_is_available(&mut transaction, session, turn, required_state).await? {
            transaction.rollback().await?;
            return Ok(TurnInstructionManifestPreflight::TurnUnavailable);
        }
        let existing = load_manifest(&mut transaction, session, turn).await?;
        transaction.rollback().await?;
        match existing {
            Some((manifest, true)) => {
                Ok(TurnInstructionManifestPreflight::Available(manifest.id()))
            }
            Some((_manifest, false)) => Err(WorkspaceInstructionRepositoryError::Corruption(
                "manifest discovery incomplete",
            )),
            None => Ok(TurnInstructionManifestPreflight::Absent),
        }
    }
}

fn placement_observation(
    row: Option<WorkspaceInstructionPlacementRow>,
) -> Result<WorkspaceInstructionPlacementObservation, WorkspaceInstructionRepositoryError> {
    let Some(WorkspaceInstructionPlacementRow {
        event_ordinal: head,
        state_kind: state,
    }) = row
    else {
        return Err(WorkspaceInstructionRepositoryError::Corruption(
            "session scheduler missing during placement observation",
        ));
    };
    if head.is_some() != state.is_some() {
        return Err(WorkspaceInstructionRepositoryError::Corruption(
            "runner placement head",
        ));
    }
    let authority = match state.as_deref() {
        Some(state) => Some(
            workspace_instruction_authority_from_placement_state(state).ok_or(
                WorkspaceInstructionRepositoryError::Corruption("runner placement state_kind"),
            )?,
        ),
        None => None,
    };
    Ok(WorkspaceInstructionPlacementObservation {
        head,
        runner_owned: authority == Some(WorkspaceInstructionAuthorityStorageKind::Runner),
    })
}

async fn placement_observation_in_connection(
    connection: &mut sqlx::PgConnection,
    session: SessionId,
) -> Result<WorkspaceInstructionPlacementObservation, WorkspaceInstructionRepositoryError> {
    placement_observation(
        sqlx::query_as::<_, WorkspaceInstructionPlacementRow>(
            "SELECT current_placement.event_ordinal, placement.state_kind
               FROM session_scheduler AS scheduler
               LEFT JOIN runner_current_session_placement AS current_placement
                 ON current_placement.session_id = scheduler.session_id
               LEFT JOIN runner_session_placement_record AS placement
                 ON placement.session_id = current_placement.session_id
                AND placement.event_ordinal = current_placement.event_ordinal
              WHERE scheduler.session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(connection)
        .await?,
    )
}

async fn turn_is_available(
    connection: &mut sqlx::PgConnection,
    session: SessionId,
    turn: TurnId,
    required_state: &'static str,
) -> Result<bool, sqlx::Error> {
    let scheduler = sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::SUBMIT_INPUT_SCHEDULER)
        .bind(session.into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    if scheduler.is_none() {
        return Ok(false);
    }
    let state = sqlx::query_scalar::<_, String>(
        "SELECT state_kind
           FROM turn_lifecycle
          WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    Ok(state.as_deref() == Some(required_state))
}

async fn load_manifest(
    connection: &mut sqlx::PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<Option<(TurnInstructionManifest, bool)>, WorkspaceInstructionRepositoryError> {
    let row = sqlx::query(
        "SELECT m.turn_instruction_manifest_id,
                m.eligibility_hash_algorithm, m.eligibility_hash,
                m.admitted_set_hash_algorithm, m.admitted_set_hash,
                m.manifest_hash_algorithm, m.manifest_hash, d.scan_complete
           FROM turn_instruction_manifest AS m
           JOIN instruction_discovery AS d
             ON d.instruction_discovery_id = m.instruction_discovery_id
            AND d.session_id = m.session_id
            AND d.turn_id = m.turn_id
          WHERE m.session_id = $1 AND m.turn_id = $2 AND m.boundary_kind = 'turn_start'",
    )
    .bind(session.into_uuid())
    .bind(turn.into_uuid())
    .fetch_optional(connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id = TurnInstructionManifestId::from_uuid(row.try_get("turn_instruction_manifest_id")?);
    if row.try_get::<String, _>("eligibility_hash_algorithm")? != "sha256_v1"
        || row.try_get::<String, _>("admitted_set_hash_algorithm")? != "sha256_v1"
        || row.try_get::<String, _>("manifest_hash_algorithm")? != "sha256_v1"
    {
        return Err(WorkspaceInstructionRepositoryError::Corruption(
            "manifest hash algorithm",
        ));
    }
    let eligibility_hash = digest(row.try_get("eligibility_hash")?)?;
    let admitted_set_hash = digest(row.try_get("admitted_set_hash")?)?;
    let manifest_hash = digest(row.try_get("manifest_hash")?)?;
    let manifest = TurnInstructionManifest::reconstitute_empty_turn_start(
        id,
        session,
        turn,
        EmptyTurnInstructionManifestEvidence {
            eligibility_hash,
            admitted_set_hash,
            manifest_hash,
        },
    )
    .ok_or(WorkspaceInstructionRepositoryError::Corruption(
        "manifest hash",
    ))?;
    Ok(Some((manifest, row.try_get("scan_complete")?)))
}

fn digest(bytes: Vec<u8>) -> Result<InstructionDigest, WorkspaceInstructionRepositoryError> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| WorkspaceInstructionRepositoryError::Corruption("digest length"))?;
    Ok(InstructionDigest::from_sha256(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_commit_database_failure_is_not_commit_ambiguous() {
        let error = WorkspaceInstructionRepositoryError::from(sqlx::Error::RowNotFound);

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        );
    }

    #[test]
    fn commit_failure_is_commit_ambiguous() {
        let error = WorkspaceInstructionRepositoryError::ambiguous_commit(sqlx::Error::RowNotFound);

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        );
    }
}

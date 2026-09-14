//! Durable workspace release authority and acknowledged startup diagnostics.

use super::*;

/// Checked SHA-256 evidence identity supplied by the protocol boundary.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RunnerEvidenceDigest(String);
impl RunnerEvidenceDigest {
    /// Accepts one canonical lowercase digest.
    pub fn try_new(value: String) -> Option<Self> {
        (value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then_some(Self(value))
    }
    /// Returns the canonical stored representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact correlation of one runner-owned workspace release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerWorkspaceRelease {
    pub session: SessionId,
    pub placement_revision: RunnerGeneration,
    pub runner: RunnerId,
    pub manifest: WorkspaceManifestId,
}

/// Durable release disposition used during authenticated reconnect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerWorkspaceReleaseState {
    Pending,
    Completed,
    CleanupFailed(serde_json::Value),
    Unowned,
}

fn correlation(row: &PgRow) -> Result<RunnerWorkspaceRelease, RunnerProtocolStoreError> {
    Ok(RunnerWorkspaceRelease {
        session: session_id(row.decode_column("session_id")?),
        placement_revision: decode_generation(row.decode_column("placement_revision")?)?,
        runner: runner_id(row.decode_column("runner_id")?),
        manifest: WorkspaceManifestId::from_uuid(row.decode_column("manifest_id")?),
    })
}

fn mismatch() -> RunnerProtocolStoreError {
    RunnerProtocolStoreError::Domain(RunnerDomainError::CorrelationMismatch)
}

impl RunnerProtocolStore {
    /// Delivers pending releases only while the provisioning identity retains authority.
    pub async fn workspace_releases(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Vec<RunnerWorkspaceRelease>, RunnerProtocolStoreError> {
        let rows = sqlx::query("SELECT release.* FROM runner_workspace_release release
            JOIN runner_enrollment enrollment USING (enrollment_id)
            JOIN runner_connection_authority_head head USING (enrollment_id)
            JOIN runner_connection_event event ON event.enrollment_id = head.enrollment_id AND event.connection_epoch = head.connection_epoch AND event.event_ordinal = head.connection_event_ordinal
            WHERE release.enrollment_id = $1 AND enrollment.state_kind <> 'revoked' AND event.state_kind IN ('connected','suspect')
                AND NOT EXISTS (SELECT 1 FROM runner_workspace_release_outcome outcome WHERE outcome.manifest_id = release.manifest_id)
                AND NOT EXISTS (SELECT 1 FROM runner_connection_loss_epoch loss WHERE loss.enrollment_id = release.enrollment_id AND loss.connection_epoch >= release.connection_epoch)
            ORDER BY release.session_id, release.placement_revision, release.manifest_id")
            .bind(enrollment.into_uuid()).fetch_all(&self.pool).await?;
        rows.iter().map(correlation).collect()
    }

    /// Reads the exact issued release or its retained terminal disposition.
    pub async fn workspace_release_state(
        &self,
        enrollment: RunnerEnrollmentId,
        expected: &RunnerWorkspaceRelease,
    ) -> Result<Option<RunnerWorkspaceReleaseState>, RunnerProtocolStoreError> {
        let row = sqlx::query("SELECT release.*, outcome.outcome, outcome.detail FROM runner_workspace_release release
            LEFT JOIN runner_workspace_release_outcome outcome USING (manifest_id)
            WHERE release.manifest_id = $1 AND release.enrollment_id = $2")
            .bind(expected.manifest.into_uuid()).bind(enrollment.into_uuid()).fetch_optional(&self.pool).await?;
        let Some(row) = row else { return Ok(None) };
        if correlation(&row)? != *expected {
            return Err(mismatch());
        }
        Ok(Some(
            match row.decode_column::<Option<String>>("outcome")?.as_deref() {
                None => RunnerWorkspaceReleaseState::Pending,
                Some("completed") => RunnerWorkspaceReleaseState::Completed,
                Some("cleanup_failed") => {
                    RunnerWorkspaceReleaseState::CleanupFailed(row.decode_column("detail")?)
                }
                Some("unowned") => RunnerWorkspaceReleaseState::Unowned,
                _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
            },
        ))
    }

    /// Commits completion or cleanup failure before the daemon acknowledges it.
    pub async fn record_workspace_release_outcome(
        &self,
        enrollment: RunnerEnrollmentId,
        expected: &RunnerWorkspaceRelease,
        failure_detail: Option<&serde_json::Value>,
    ) -> Result<(), RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(expected.session.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        sqlx::query(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
            .bind(enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let row = sqlx::query("SELECT release.*, outcome.outcome, outcome.detail FROM runner_workspace_release release
            LEFT JOIN runner_workspace_release_outcome outcome USING (manifest_id)
            WHERE release.manifest_id = $1 AND release.enrollment_id = $2")
            .bind(expected.manifest.into_uuid()).bind(enrollment.into_uuid()).fetch_optional(&mut *transaction).await?.ok_or_else(mismatch)?;
        if correlation(&row)? != *expected {
            return Err(mismatch());
        }
        let outcome = if failure_detail.is_some() {
            "cleanup_failed"
        } else {
            "completed"
        };
        if let Some(prior) = row.decode_column::<Option<String>>("outcome")? {
            return if prior == outcome
                && row
                    .decode_column::<Option<serde_json::Value>>("detail")?
                    .as_ref()
                    == failure_detail
            {
                Ok(())
            } else {
                Err(mismatch())
            };
        }
        let lost: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM runner_connection_loss_epoch WHERE enrollment_id = $1 AND connection_epoch >= $2)")
            .bind(enrollment.into_uuid()).bind(row.decode_column::<Decimal>("connection_epoch")?).fetch_one(&mut *transaction).await?;
        let owner = load_enrollment_in(transaction.as_mut(), enrollment)
            .await?
            .ok_or_else(mismatch)?;
        if lost || owner.state() == RunnerEnrollmentState::Revoked {
            return Err(mismatch());
        }
        sqlx::query("INSERT INTO runner_workspace_release_outcome (manifest_id,outcome,detail) VALUES ($1,$2,$3)")
            .bind(expected.manifest.into_uuid()).bind(outcome).bind(failure_detail).execute(&mut *transaction).await?;
        if failure_detail.is_some() {
            retain_release_leak(
                &mut transaction,
                expected.manifest.into_uuid(),
                "cleanup_failed",
            )
            .await?;
        }
        commit_mutation(transaction).await
    }
}

pub(super) async fn retain_release_leak(
    connection: &mut PgConnection,
    manifest: Uuid,
    kind: &str,
) -> Result<(), RunnerProtocolStoreError> {
    let release = sqlx::query(
        "SELECT release.*, ready.manifest_digest
        FROM runner_workspace_release release
        LEFT JOIN runner_replacement_workspace_ready ready USING (manifest_id)
        WHERE release.manifest_id = $1",
    )
    .bind(manifest)
    .fetch_one(&mut *connection)
    .await?;
    let digest = match release.decode_column::<Option<String>>("manifest_digest")? {
        Some(digest) => digest,
        None => {
            let ordinal = release
                .decode_column::<Option<Decimal>>("source_event_ordinal")?
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            let source = sqlx::query("SELECT * FROM runner_session_placement_record WHERE session_id = $1 AND event_ordinal = $2")
                .bind(release.decode_column::<Uuid>("session_id")?).bind(ordinal)
                .fetch_one(&mut *connection).await?;
            placement_manifest_digest(&source)?
        }
    };
    sqlx::query("INSERT INTO runner_workspace_leak (runner_id,locator,entry_digest,kind,session_id,placement_revision)
        SELECT runner_id,relative_path,$3,$2,session_id,placement_revision
        FROM runner_workspace_release WHERE manifest_id = $1
        ON CONFLICT (runner_id,locator,entry_digest) DO UPDATE SET kind = EXCLUDED.kind")
        .bind(manifest).bind(kind).bind(digest).execute(connection).await?;
    Ok(())
}

fn placement_manifest_digest(source: &PgRow) -> Result<String, RunnerProtocolStoreError> {
    let workspace = decode_provisioned_workspace(
        source,
        session_id(source.decode_column("session_id")?),
        runner_id(source.decode_column("pinned_runner_id")?),
    )?
    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let manifest = signalbox_runner_wire::WorkspaceManifest::from_domain(
        signalbox_runner_wire::ManifestLifecycle::Ready,
        &workspace,
    )
    .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
    signalbox_runner_wire::workspace_manifest_digest(&manifest)
        .map(|digest| digest.as_str().to_owned())
        .map_err(|_| RunnerProtocolCorruption::InvalidEncoding.into())
}

pub(super) async fn retire_releases_on_loss(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
    epoch: RunnerConnectionEpoch,
) -> Result<(), RunnerProtocolStoreError> {
    let manifests: Vec<Uuid> = sqlx::query_scalar("INSERT INTO runner_workspace_release_outcome (manifest_id,outcome)
        SELECT manifest_id,'unowned' FROM runner_workspace_release release
        WHERE enrollment_id = $1 AND connection_epoch <= $2
            AND NOT EXISTS (SELECT 1 FROM runner_workspace_release_outcome outcome WHERE outcome.manifest_id = release.manifest_id)
        ON CONFLICT DO NOTHING RETURNING manifest_id")
        .bind(enrollment.into_uuid()).bind(Decimal::from(epoch.get())).fetch_all(&mut *connection).await?;
    for manifest in manifests {
        retain_release_leak(connection, manifest, "retired_present").await?;
    }
    Ok(())
}

/// Closed diagnostic workspace classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerWorkspaceLeakKind {
    UnknownManifest,
    RetiredPresent,
    ManifestConflict,
    CleanupFailed,
    Unreconciled,
}
impl RunnerWorkspaceLeakKind {
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::UnknownManifest => "unknown_manifest",
            Self::RetiredPresent => "retired_present",
            Self::ManifestConflict => "manifest_conflict",
            Self::CleanupFailed => "cleanup_failed",
            Self::Unreconciled => "unreconciled",
        }
    }
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "unknown_manifest" => Self::UnknownManifest,
            "retired_present" => Self::RetiredPresent,
            "manifest_conflict" => Self::ManifestConflict,
            "cleanup_failed" => Self::CleanupFailed,
            "unreconciled" => Self::Unreconciled,
            _ => return None,
        })
    }
}

/// One runner-root-relative diagnostic fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerWorkspaceLeak {
    pub kind: RunnerWorkspaceLeakKind,
    pub locator: WorkspaceRelativePath,
    pub entry_digest: RunnerEvidenceDigest,
    pub session: Option<SessionId>,
    pub placement_revision: Option<RunnerGeneration>,
}

/// One checked report page retained before acknowledgement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerWorkspaceLeakPage {
    pub registration_revision: RunnerGeneration,
    pub report_digest: RunnerEvidenceDigest,
    pub page: RunnerGeneration,
    pub prior_page_digest: Option<RunnerEvidenceDigest>,
    pub final_page: bool,
    pub page_digest: RunnerEvidenceDigest,
    pub facts: Vec<RunnerWorkspaceLeak>,
}

fn encode_leak_facts(facts: &[RunnerWorkspaceLeak]) -> serde_json::Value {
    serde_json::Value::Array(facts.iter().map(|fact| serde_json::json!({
        "kind": fact.kind.token(), "locator": fact.locator.as_str(), "entry_digest": fact.entry_digest.as_str(),
        "session": fact.session.map(|session| session.into_uuid().to_string()), "placement_revision": fact.placement_revision.map(RunnerGeneration::get),
    })).collect())
}

impl RunnerProtocolStore {
    /// Stores an exactly replayable page and projects its unresolved facts.
    /// Final pages require a strictly ordered report matching its complete digest.
    pub async fn record_workspace_leak_page(
        &self,
        enrollment: RunnerEnrollmentId,
        page: &RunnerWorkspaceLeakPage,
    ) -> Result<(), RunnerProtocolStoreError> {
        // Version-one runner leak pages carry at most 64 facts.
        if page.facts.len() > 64
            || (!page.final_page && page.facts.len() != 64)
            || (page.page.get() == 1) != page.prior_page_digest.is_none()
        {
            return Err(mismatch());
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        sqlx::query(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
            .bind(enrollment.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let owner = load_enrollment_in(transaction.as_mut(), enrollment)
            .await?
            .ok_or_else(mismatch)?;
        if owner.state() == RunnerEnrollmentState::Revoked {
            return Err(mismatch());
        }
        let facts = encode_leak_facts(&page.facts);
        let prior = sqlx::query("SELECT prior_page_digest,final_page,page_digest,facts FROM runner_workspace_leak_page
            WHERE enrollment_id = $1 AND registration_revision = $2 AND report_digest = $3 AND page = $4")
            .bind(enrollment.into_uuid()).bind(Decimal::from(page.registration_revision.get())).bind(page.report_digest.as_str())
            .bind(Decimal::from(page.page.get())).fetch_optional(&mut *transaction).await?;
        if let Some(prior) = prior {
            return if prior
                .decode_column::<Option<String>>("prior_page_digest")?
                .as_deref()
                == page
                    .prior_page_digest
                    .as_ref()
                    .map(RunnerEvidenceDigest::as_str)
                && prior.decode_column::<bool>("final_page")? == page.final_page
                && prior.decode_column::<String>("page_digest")? == page.page_digest.as_str()
                && prior.decode_column::<serde_json::Value>("facts")? == facts
            {
                Ok(())
            } else {
                Err(mismatch())
            };
        }
        if page.page.get() > 1 {
            let prior = sqlx::query("SELECT final_page,page_digest FROM runner_workspace_leak_page WHERE enrollment_id = $1 AND registration_revision = $2 AND report_digest = $3 AND page = $4")
                .bind(enrollment.into_uuid()).bind(Decimal::from(page.registration_revision.get())).bind(page.report_digest.as_str())
                .bind(Decimal::from(page.page.get() - 1)).fetch_optional(&mut *transaction).await?.ok_or_else(mismatch)?;
            if prior.decode_column::<bool>("final_page")?
                || Some(prior.decode_column::<String>("page_digest")?.as_str())
                    != page
                        .prior_page_digest
                        .as_ref()
                        .map(RunnerEvidenceDigest::as_str)
            {
                return Err(mismatch());
            }
        }
        sqlx::query("INSERT INTO runner_workspace_leak_page (enrollment_id,registration_revision,report_digest,page,prior_page_digest,final_page,page_digest,facts) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(enrollment.into_uuid()).bind(Decimal::from(page.registration_revision.get())).bind(page.report_digest.as_str()).bind(Decimal::from(page.page.get()))
            .bind(page.prior_page_digest.as_ref().map(RunnerEvidenceDigest::as_str)).bind(page.final_page).bind(page.page_digest.as_str()).bind(facts).execute(&mut *transaction).await?;
        if page.final_page {
            verify_leak_report(transaction.as_mut(), enrollment, page).await?;
        }
        for fact in &page.facts {
            if let Some(kind) = reconcile_leak(&mut transaction, owner.runner(), fact).await? {
                sqlx::query("INSERT INTO runner_workspace_leak (runner_id,locator,entry_digest,kind,session_id,placement_revision) VALUES ($1,$2,$3,$4,$5,$6)
                    ON CONFLICT (runner_id,locator,entry_digest) DO UPDATE SET kind = EXCLUDED.kind, session_id = EXCLUDED.session_id, placement_revision = EXCLUDED.placement_revision")
                    .bind(owner.runner().into_uuid()).bind(fact.locator.as_str()).bind(fact.entry_digest.as_str()).bind(kind.token())
                    .bind(fact.session.map(SessionId::into_uuid)).bind(fact.placement_revision.map(|revision| Decimal::from(revision.get()))).execute(&mut *transaction).await?;
            }
        }
        commit_mutation(transaction).await
    }
}

async fn verify_leak_report(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
    page: &RunnerWorkspaceLeakPage,
) -> Result<(), RunnerProtocolStoreError> {
    use signalbox_runner_wire::{CanonicalUuid, Digest, LeakFact, LeakFactKind, PositiveU64};
    let rows = sqlx::query("SELECT fact.value->>'kind' AS kind, fact.value->>'locator' AS locator,
            fact.value->>'entry_digest' AS entry_digest, (fact.value->>'session')::uuid AS session_id,
            (fact.value->>'placement_revision')::numeric AS placement_revision
        FROM runner_workspace_leak_page AS page
        CROSS JOIN LATERAL jsonb_array_elements(page.facts) WITH ORDINALITY AS fact(value, ordinal)
        WHERE page.enrollment_id = $1 AND page.registration_revision = $2 AND page.report_digest = $3
        ORDER BY page.page, fact.ordinal")
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(page.registration_revision.get()))
        .bind(page.report_digest.as_str())
        .fetch_all(connection)
        .await?;
    let facts = rows
        .iter()
        .map(|row| {
            let kind = RunnerWorkspaceLeakKind::parse(&row.decode_column::<String>("kind")?)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            Ok(LeakFact {
                kind: match kind {
                    RunnerWorkspaceLeakKind::UnknownManifest => LeakFactKind::UnknownManifest,
                    RunnerWorkspaceLeakKind::RetiredPresent => LeakFactKind::RetiredPresent,
                    RunnerWorkspaceLeakKind::ManifestConflict => LeakFactKind::ManifestConflict,
                    RunnerWorkspaceLeakKind::CleanupFailed => LeakFactKind::CleanupFailed,
                    RunnerWorkspaceLeakKind::Unreconciled => LeakFactKind::Unreconciled,
                },
                locator: row.decode_column("locator")?,
                entry_digest: Digest::try_new(row.decode_column("entry_digest")?)
                    .map_err(|_| mismatch())?,
                session: row
                    .decode_column::<Option<Uuid>>("session_id")?
                    .map(CanonicalUuid::from_uuid),
                placement_revision: row
                    .decode_column::<Option<Decimal>>("placement_revision")?
                    .map(|value| {
                        PositiveU64::try_new(decode_generation(value)?.get())
                            .map_err(|_| mismatch())
                    })
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>, RunnerProtocolStoreError>>()?;
    let digest = signalbox_runner_wire::leak_report_digest(&facts).map_err(|_| mismatch())?;
    if digest.as_str() != page.report_digest.as_str() {
        return Err(mismatch());
    }
    Ok(())
}

async fn reconcile_leak(
    connection: &mut PgConnection,
    runner: RunnerId,
    fact: &RunnerWorkspaceLeak,
) -> Result<Option<RunnerWorkspaceLeakKind>, RunnerProtocolStoreError> {
    use RunnerWorkspaceLeakKind as Kind;
    if fact.kind == Kind::RetiredPresent
        && fact.session.is_none()
        && fact.placement_revision.is_none()
        && let Some(name) = fact.locator.as_str().strip_prefix("trash/")
        && let Ok(manifest) = Uuid::parse_str(name)
        && manifest.to_string() == name
    {
        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS (
            SELECT 1 FROM runner_workspace_release release
            LEFT JOIN runner_workspace_release_outcome outcome USING (manifest_id)
            WHERE release.manifest_id = $1 AND release.runner_id = $2
                AND outcome.outcome IS DISTINCT FROM 'completed')",
        )
        .bind(manifest)
        .bind(runner.into_uuid())
        .fetch_one(&mut *connection)
        .await?;
        if retained {
            return Ok(None);
        }
    }
    if fact.kind != Kind::Unreconciled {
        return Ok(Some(fact.kind));
    }
    let (Some(session), Some(revision)) = (fact.session, fact.placement_revision) else {
        return Ok(Some(Kind::Unreconciled));
    };
    let rows = sqlx::query("SELECT ready.manifest_id,ready.manifest_digest,operation.command_id,outcome.outcome,
        EXISTS (SELECT 1 FROM runner_workspace_release cleanup WHERE cleanup.manifest_id = ready.manifest_id) AS released,
        EXISTS (SELECT 1 FROM runner_current_session_placement head JOIN runner_session_placement_record placement USING (session_id,event_ordinal)
            WHERE placement.workspace_manifest_id = ready.manifest_id AND placement.state_kind <> 'runner_abandoned') AS current,
        EXISTS (SELECT 1 FROM runner_replacement_stage stage WHERE stage.command_id = operation.command_id
            AND NOT EXISTS (SELECT 1 FROM replace_lost_runner_result result WHERE result.command_id = stage.command_id)) AS staged
        FROM runner_replacement_workspace_ready ready JOIN runner_replacement_provisioning_authorization operation USING (authorization_id)
        LEFT JOIN runner_workspace_release_outcome outcome USING (manifest_id)
        WHERE operation.runner_id = $1 AND operation.session_id = $2 AND operation.placement_revision = $3 AND ready.relative_path = $4")
        .bind(runner.into_uuid()).bind(session.into_uuid()).bind(Decimal::from(revision.get())).bind(fact.locator.as_str()).fetch_all(&mut *connection).await?;
    if rows.is_empty() {
        let initial = sqlx::query("SELECT placement.*, outcome.outcome,
                release.manifest_id IS NOT NULL AS released
            FROM runner_session_placement_record placement
            LEFT JOIN runner_current_session_placement head USING (session_id,event_ordinal)
            LEFT JOIN runner_workspace_release release ON release.session_id = placement.session_id
                AND release.source_event_ordinal = placement.event_ordinal
            LEFT JOIN runner_workspace_release_outcome outcome ON outcome.manifest_id = release.manifest_id
            WHERE placement.pinned_runner_id = $1 AND placement.session_id = $2
                AND placement.workspace_placement_revision = $3 AND placement.workspace_relative_path = $4
                AND placement.workspace_manifest_id IS NOT NULL AND placement.state_kind <> 'runner_abandoned'
                AND (head.session_id IS NOT NULL OR release.manifest_id IS NOT NULL)
                AND NOT EXISTS (SELECT 1 FROM runner_replacement_workspace_ready ready WHERE ready.manifest_id = placement.workspace_manifest_id)")
            .bind(runner.into_uuid()).bind(session.into_uuid()).bind(Decimal::from(revision.get()))
            .bind(fact.locator.as_str()).fetch_all(connection).await?;
        for placement in &initial {
            if placement_manifest_digest(placement)? == fact.entry_digest.as_str() {
                return if placement.decode_column::<bool>("released")? {
                    Ok(
                        match placement
                            .decode_column::<Option<String>>("outcome")?
                            .as_deref()
                        {
                            None | Some("completed") => None,
                            Some("cleanup_failed") => Some(Kind::CleanupFailed),
                            Some("unowned") => Some(Kind::RetiredPresent),
                            _ => return Err(mismatch()),
                        },
                    )
                } else {
                    Ok(None)
                };
            }
        }
        return Ok(Some(if initial.is_empty() {
            Kind::UnknownManifest
        } else {
            Kind::ManifestConflict
        }));
    }
    for row in rows {
        if row.decode_column::<String>("manifest_digest")? == fact.entry_digest.as_str() {
            if row.decode_column::<Option<String>>("outcome")?.as_deref() == Some("cleanup_failed")
            {
                return Ok(Some(Kind::CleanupFailed));
            }
            if row.decode_column::<bool>("released")? {
                return Ok(
                    match row.decode_column::<Option<String>>("outcome")?.as_deref() {
                        None | Some("completed") => None,
                        Some("unowned") => Some(Kind::RetiredPresent),
                        _ => return Err(mismatch()),
                    },
                );
            }
            if row.decode_column::<bool>("current")? || row.decode_column::<bool>("staged")? {
                return Ok(None);
            }
            return Ok(Some(Kind::RetiredPresent));
        }
    }
    Ok(Some(Kind::ManifestConflict))
}

pub(super) async fn retain_retired_placement(
    connection: &mut PgConnection,
    session: SessionId,
    ordinal: u64,
) -> Result<(), RunnerProtocolStoreError> {
    let source = sqlx::query("SELECT source.* FROM runner_session_placement_record source
        JOIN runner_session_placement_record retired ON retired.session_id = source.session_id AND retired.event_ordinal = $2
        WHERE source.session_id = $1 AND source.event_ordinal = $2 - 1
            AND source.workspace_manifest_id IS NOT NULL AND source.state_kind <> 'runner_abandoned'
            AND (retired.state_kind = 'runner_abandoned' OR retired.workspace_manifest_id IS DISTINCT FROM source.workspace_manifest_id)")
        .bind(session.into_uuid()).bind(Decimal::from(ordinal)).fetch_optional(&mut *connection).await?;
    let Some(source) = source else { return Ok(()) };
    let enrollment: Uuid = source.decode_column("registration_enrollment_id")?;
    sqlx::query(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
        .bind(enrollment)
        .fetch_one(&mut *connection)
        .await?;
    let manifest: Uuid = source.decode_column("workspace_manifest_id")?;
    let live: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM runner_lease_generation lease
        JOIN runner_current_lease_event current USING (lease_id,generation)
        JOIN runner_lease_event event USING (lease_id,generation,event_ordinal)
        JOIN runner_session_placement_record placement ON placement.session_id = lease.session_id AND placement.event_ordinal = lease.placement_event_ordinal
        WHERE placement.workspace_manifest_id = $1 AND event.state_kind IN ('offered','claimed'))")
        .bind(manifest).fetch_one(&mut *connection).await?;
    if live {
        return Ok(());
    }
    let inserted = sqlx::query("INSERT INTO runner_workspace_release (manifest_id,session_id,placement_revision,runner_id,enrollment_id,connection_epoch,connection_event_ordinal,relative_path,source_event_ordinal,retired_event_ordinal)
        SELECT source.workspace_manifest_id,source.session_id,source.workspace_placement_revision,source.pinned_runner_id,source.registration_enrollment_id,
            head.connection_epoch,head.connection_event_ordinal,source.workspace_relative_path,source.event_ordinal,$3
        FROM runner_session_placement_record source JOIN runner_enrollment enrollment ON enrollment.enrollment_id = source.registration_enrollment_id
        JOIN runner_connection_authority_head head ON head.enrollment_id = enrollment.enrollment_id
        JOIN runner_connection_event event ON event.enrollment_id = head.enrollment_id AND event.connection_epoch = head.connection_epoch AND event.event_ordinal = head.connection_event_ordinal
        WHERE source.session_id = $1 AND source.event_ordinal = $2 AND enrollment.state_kind <> 'revoked' AND event.state_kind = 'connected'
        ON CONFLICT DO NOTHING")
        .bind(session.into_uuid()).bind(Decimal::from(ordinal - 1)).bind(Decimal::from(ordinal)).execute(&mut *connection).await?.rows_affected();
    if inserted == 0 {
        retain_placement_leak(connection, &source).await?;
    }
    Ok(())
}

pub(super) async fn retain_placement_leak(
    connection: &mut PgConnection,
    source: &PgRow,
) -> Result<(), RunnerProtocolStoreError> {
    let Some(manifest) = source.decode_column::<Option<Uuid>>("workspace_manifest_id")? else {
        return Ok(());
    };
    let retained: Option<String> = sqlx::query_scalar(
        "SELECT manifest_digest FROM runner_replacement_workspace_ready WHERE manifest_id = $1",
    )
    .bind(manifest)
    .fetch_optional(&mut *connection)
    .await?;
    let digest = match retained {
        Some(digest) => digest,
        None => placement_manifest_digest(source)?,
    };
    sqlx::query("INSERT INTO runner_workspace_leak (runner_id,locator,entry_digest,kind,session_id,placement_revision)
        VALUES ($1,$2,$3,'retired_present',$4,$5) ON CONFLICT DO NOTHING")
        .bind(source.decode_column::<Uuid>("pinned_runner_id")?)
        .bind(source.decode_column::<String>("workspace_relative_path")?)
        .bind(digest)
        .bind(source.decode_column::<Uuid>("session_id")?)
        .bind(source.decode_column::<Decimal>("workspace_placement_revision")?)
        .execute(connection).await?;
    Ok(())
}

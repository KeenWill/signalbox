//! Read-only traversal of placements, failures, and retained workspace diagnostics.

use super::workspaces::{RunnerEvidenceDigest, RunnerWorkspaceLeak, RunnerWorkspaceLeakKind};
use super::*;
use crate::process_read::{
    ProcessReadError, ProcessRunnerProjection, load_process_runner_projection,
};
use signalbox_domain::RunnerReplacementProvisioning;

/// Exclusive position in the enrollment, placement, failure, then leak traversal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerStatusAfter {
    /// Last emitted enrollment, ordered by runner identity.
    Enrollment(Uuid),
    /// Last emitted placement, ordered by session identity.
    Placement(Uuid),
    /// Last emitted immutable provisioning authorization.
    OperationFailure(Uuid),
    /// Last emitted failed workspace release.
    ReleaseFailure(Uuid),
    /// Last emitted refused lease generation.
    LeaseFailure {
        lease: Uuid,
        generation: RunnerGeneration,
    },
    /// Last emitted leak, ordered by runner, locator, and evidence digest.
    WorkspaceLeak {
        runner: Uuid,
        locator: String,
        entry_digest: RunnerEvidenceDigest,
    },
}

/// Current enrollment or session placement from the page's snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerStatusFact {
    /// Enrollment identity and current authority, including pending successors.
    Enrollment {
        runner: RunnerId,
        request: RunnerEnrollmentRequestId,
        authority: RunnerEnrollmentState,
        connection: Option<RunnerConnectionState>,
    },
    /// Current placement with the ordinary transcript projection.
    Placement {
        session: SessionId,
        runner: ProcessRunnerProjection,
    },
}

/// Retained refusal and its exact immutable authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerStatusFailure {
    Provision {
        authorization: RunnerReplacementProvisioning,
        category: signalbox_domain::RunnerProvisioningFailureKind,
        detail: serde_json::Value,
    },
    Release {
        correlation: super::workspaces::RunnerWorkspaceRelease,
        detail: serde_json::Value,
    },
    LeaseOffer {
        correlation: RunnerLeaseCorrelation,
        category: RunnerLeaseFailureKind,
        detail: serde_json::Value,
    },
}

/// One bounded page of current runner facts followed by retained failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerStatusPage {
    pub runners: Vec<RunnerStatusFact>,
    pub failures: Vec<RunnerStatusFailure>,
    pub leaks: Vec<(RunnerId, RunnerWorkspaceLeak)>,
    pub next_after: Option<RunnerStatusAfter>,
}

/// Failure to read a coherent status page.
#[derive(Debug, signalbox_derive::OperatorError)]
pub enum RunnerStatusError {
    #[error("invalid runner-status page size")]
    InvalidPageSize,
    #[error("runner-status database failure: {field_0}")]
    Database(#[source] sqlx::Error),
    #[error("runner-status retained facts are inconsistent")]
    Corruption,
}

impl From<sqlx::Error> for RunnerStatusError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<RunnerProtocolStoreError> for RunnerStatusError {
    fn from(error: RunnerProtocolStoreError) -> Self {
        match error {
            RunnerProtocolStoreError::Database(error) => Self::Database(error),
            _ => Self::Corruption,
        }
    }
}

/// Reads one repeatable-read snapshot and releases it before protocol output.
pub async fn read_runner_status(
    pool: &PgPool,
    page_size: u32,
    after: Option<RunnerStatusAfter>,
) -> Result<RunnerStatusPage, RunnerStatusError> {
    if !(1..=100).contains(&page_size) {
        return Err(RunnerStatusError::InvalidPageSize);
    }
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *transaction)
        .await?;
    let mut runners = Vec::new();
    let limit = page_size as usize + 1;
    if matches!(after, None | Some(RunnerStatusAfter::Enrollment(_))) {
        let last = match after {
            Some(RunnerStatusAfter::Enrollment(id)) => Some(id),
            _ => None,
        };
        let rows = sqlx::query(
            "SELECT enrollment.runner_id, receipt.request_id, enrollment.state_kind,
            connection.state_kind AS connection_state
            FROM runner_enrollment AS enrollment
            JOIN runner_enrollment_request_receipt AS receipt USING (enrollment_id)
            LEFT JOIN LATERAL (SELECT state_kind FROM runner_connection_event
                WHERE enrollment_id = enrollment.enrollment_id
                ORDER BY connection_epoch DESC, event_ordinal DESC LIMIT 1) AS connection ON true
            WHERE ($1::uuid IS NULL OR enrollment.runner_id > $1)
            ORDER BY enrollment.runner_id LIMIT $2",
        )
        .bind(last)
        .bind(limit as i64)
        .fetch_all(&mut *transaction)
        .await?;
        for row in rows {
            let authority = match row.try_get::<String, _>("state_kind")?.as_str() {
                "pending" => RunnerEnrollmentState::Pending,
                "active" => RunnerEnrollmentState::Active,
                "revoked" => RunnerEnrollmentState::Revoked,
                _ => return Err(RunnerStatusError::Corruption),
            };
            let connection = match row
                .try_get::<Option<String>, _>("connection_state")?
                .as_deref()
            {
                None => None,
                Some("connected") => Some(RunnerConnectionState::Connected),
                Some("suspect") => Some(RunnerConnectionState::Suspect),
                Some("shutdown") => Some(RunnerConnectionState::Shutdown),
                Some("lost") => Some(RunnerConnectionState::Lost),
                _ => return Err(RunnerStatusError::Corruption),
            };
            runners.push(RunnerStatusFact::Enrollment {
                runner: RunnerId::from_uuid(row.try_get("runner_id")?),
                request: RunnerEnrollmentRequestId::from_uuid(row.try_get("request_id")?),
                authority,
                connection,
            });
        }
    }
    if runners.len() < limit
        && matches!(
            after,
            None | Some(RunnerStatusAfter::Enrollment(_) | RunnerStatusAfter::Placement(_))
        )
    {
        let last = match after {
            Some(RunnerStatusAfter::Placement(id)) => Some(id),
            _ => None,
        };
        let sessions: Vec<Uuid> = sqlx::query_scalar(
            "SELECT session_id FROM runner_current_session_placement
            WHERE ($1::uuid IS NULL OR session_id > $1) ORDER BY session_id LIMIT $2",
        )
        .bind(last)
        .bind((limit - runners.len()) as i64)
        .fetch_all(&mut *transaction)
        .await?;
        for session in sessions {
            let session = SessionId::from_uuid(session);
            let runner = load_process_runner_projection(&mut transaction, session)
                .await
                .map_err(|error| match error {
                    ProcessReadError::Database(error) => RunnerStatusError::Database(error),
                    _ => RunnerStatusError::Corruption,
                })?
                .ok_or(RunnerStatusError::Corruption)?;
            runners.push(RunnerStatusFact::Placement { session, runner });
        }
    }
    let mut failures = Vec::new();
    if runners.len() < limit
        && matches!(
            after,
            None | Some(
                RunnerStatusAfter::Enrollment(_)
                    | RunnerStatusAfter::Placement(_)
                    | RunnerStatusAfter::OperationFailure(_)
            )
        )
    {
        let last = match after {
            Some(RunnerStatusAfter::OperationFailure(id)) => Some(id),
            _ => None,
        };
        let rows = sqlx::query(
            "SELECT operation.*, failure.failure_kind, failure.detail
            FROM runner_replacement_provisioning_failure AS failure
            JOIN runner_replacement_provisioning_authorization AS operation USING (authorization_id)
            WHERE ($1::uuid IS NULL OR failure.authorization_id > $1)
            ORDER BY failure.authorization_id LIMIT $2",
        )
        .bind(last)
        .bind((limit - runners.len()) as i64)
        .fetch_all(&mut *transaction)
        .await?;
        for row in rows {
            use signalbox_domain::RunnerProvisioningFailureKind as Kind;
            let category = match row.try_get::<String, _>("failure_kind")?.as_str() {
                "credential_unavailable" => Kind::CredentialUnavailable,
                "repository_unavailable" => Kind::RepositoryUnavailable,
                "sandbox_unavailable" => Kind::SandboxUnavailable,
                "workspace_conflict" => Kind::WorkspaceConflict,
                _ => return Err(RunnerStatusError::Corruption),
            };
            failures.push(RunnerStatusFailure::Provision {
                authorization: super::provisioning::decode_authorization(&row)?,
                category,
                detail: row.try_get("detail")?,
            });
        }
    }
    if runners.len() + failures.len() < limit
        && !matches!(
            after,
            Some(RunnerStatusAfter::LeaseFailure { .. } | RunnerStatusAfter::WorkspaceLeak { .. })
        )
    {
        let last = match after {
            Some(RunnerStatusAfter::ReleaseFailure(id)) => Some(id),
            _ => None,
        };
        let rows = sqlx::query("SELECT release.*, outcome.detail FROM runner_workspace_release release
            JOIN runner_workspace_release_outcome outcome USING (manifest_id)
            WHERE outcome.outcome = 'cleanup_failed' AND ($1::uuid IS NULL OR release.manifest_id > $1)
            ORDER BY release.manifest_id LIMIT $2")
            .bind(last).bind((limit - runners.len() - failures.len()) as i64).fetch_all(&mut *transaction).await?;
        for row in rows {
            failures.push(RunnerStatusFailure::Release {
                correlation: super::workspaces::RunnerWorkspaceRelease {
                    session: SessionId::from_uuid(row.try_get("session_id")?),
                    placement_revision: decode_generation(row.try_get("placement_revision")?)?,
                    runner: RunnerId::from_uuid(row.try_get("runner_id")?),
                    manifest: signalbox_domain::WorkspaceManifestId::from_uuid(
                        row.try_get("manifest_id")?,
                    ),
                },
                detail: row.try_get("detail")?,
            });
        }
    }
    if runners.len() + failures.len() < limit
        && !matches!(after, Some(RunnerStatusAfter::WorkspaceLeak { .. }))
    {
        let (last, generation) = match after {
            Some(RunnerStatusAfter::LeaseFailure { lease, generation }) => {
                (Some(lease), Some(Decimal::from(generation.get())))
            }
            _ => (None, None),
        };
        let rows = sqlx::query("SELECT lease.*, attempt.turn_id, attempt.issuing_turn_attempt_id, attempt.request_id, attempt.dispatch_generation,
                placement.placement_revision, placement.pinned_working_directory, placement.requested_sandbox_profile,
                failure.category, failure.detail
            FROM runner_lease_failure failure JOIN runner_lease_generation lease USING (lease_id,generation)
            JOIN tool_attempt attempt ON attempt.attempt_id = lease.attempt_id
            JOIN runner_session_placement_record placement ON placement.session_id = lease.session_id AND placement.event_ordinal = lease.placement_event_ordinal
            WHERE ($1::uuid IS NULL OR (lease.lease_id, lease.generation) > ($1,$2))
            ORDER BY lease.lease_id,lease.generation LIMIT $3")
            .bind(last).bind(generation).bind((limit - runners.len() - failures.len()) as i64).fetch_all(&mut *transaction).await?;
        for row in rows {
            failures.push(RunnerStatusFailure::LeaseOffer {
                correlation: RunnerLeaseCorrelation {
                    lease: runner_lease_id(row.try_get("lease_id")?),
                    generation: decode_generation(row.try_get("generation")?)?,
                    runner: runner_id(row.try_get("runner_id")?),
                    registration_revision: decode_generation(
                        row.try_get("offer_registration_revision")?,
                    )?,
                    placement_revision: decode_generation(row.try_get("placement_revision")?)?,
                    working_directory: working_directory(row.try_get("pinned_working_directory")?)?,
                    sandbox: decode_sandbox(row.try_get("requested_sandbox_profile")?)?,
                    tool: tool_name(row.try_get("tool_name")?)?,
                    dispatch: ToolAttemptDispatchCorrelation::reconstitute(
                        ToolAttemptDispatchCorrelationReconstitutionInput {
                            session: session_id(row.try_get("session_id")?),
                            turn: TurnId::from_uuid(row.try_get("turn_id")?),
                            issuing_attempt: TurnAttemptId::from_uuid(
                                row.try_get("issuing_turn_attempt_id")?,
                            ),
                            request: ToolRequestId::from_uuid(row.try_get("request_id")?),
                            attempt: tool_attempt_id(row.try_get("attempt_id")?),
                            generation: decode_dispatch_generation(
                                row.try_get("dispatch_generation")?,
                            )?,
                        },
                    ),
                },
                category: RunnerLeaseFailureKind::parse(&row.try_get::<String, _>("category")?)
                    .ok_or(RunnerStatusError::Corruption)?,
                detail: row.try_get("detail")?,
            });
        }
    }
    let mut leaks = Vec::new();
    if runners.len() + failures.len() < limit {
        let (runner, locator, digest) = match &after {
            Some(RunnerStatusAfter::WorkspaceLeak {
                runner,
                locator,
                entry_digest,
            }) => (
                Some(*runner),
                Some(locator.as_str()),
                Some(entry_digest.as_str()),
            ),
            _ => (None, None, None),
        };
        let rows = sqlx::query(
            "SELECT * FROM runner_workspace_leak
            WHERE ($1::uuid IS NULL OR (runner_id, locator, entry_digest) > ($1, $2, $3))
            ORDER BY runner_id, locator, entry_digest LIMIT $4",
        )
        .bind(runner)
        .bind(locator)
        .bind(digest)
        .bind((limit - runners.len() - failures.len()) as i64)
        .fetch_all(&mut *transaction)
        .await?;
        for row in rows {
            leaks.push((
                RunnerId::from_uuid(row.try_get("runner_id")?),
                RunnerWorkspaceLeak {
                    kind: RunnerWorkspaceLeakKind::parse(&row.try_get::<String, _>("kind")?)
                        .ok_or(RunnerStatusError::Corruption)?,
                    locator: WorkspaceRelativePath::try_new(row.try_get("locator")?)
                        .map_err(|_| RunnerStatusError::Corruption)?,
                    entry_digest: RunnerEvidenceDigest::try_new(row.try_get("entry_digest")?)
                        .ok_or(RunnerStatusError::Corruption)?,
                    session: row
                        .try_get::<Option<Uuid>, _>("session_id")?
                        .map(SessionId::from_uuid),
                    placement_revision: row
                        .try_get::<Option<Decimal>, _>("placement_revision")?
                        .map(decode_generation)
                        .transpose()?,
                },
            ));
        }
    }
    let next_after = if runners.len() + failures.len() + leaks.len() > page_size as usize {
        if leaks.pop().is_none() && failures.pop().is_none() {
            runners.pop();
        }
        leaks
            .last()
            .map(|(runner, leak)| RunnerStatusAfter::WorkspaceLeak {
                runner: runner.into_uuid(),
                locator: leak.locator.as_str().to_owned(),
                entry_digest: leak.entry_digest.clone(),
            })
            .or_else(|| {
                failures.last().map(|row| match row {
                    RunnerStatusFailure::Provision { authorization, .. } => {
                        RunnerStatusAfter::OperationFailure(authorization.authorization.into_uuid())
                    }
                    RunnerStatusFailure::Release { correlation, .. } => {
                        RunnerStatusAfter::ReleaseFailure(correlation.manifest.into_uuid())
                    }
                    RunnerStatusFailure::LeaseOffer { correlation, .. } => {
                        RunnerStatusAfter::LeaseFailure {
                            lease: correlation.lease.into_uuid(),
                            generation: correlation.generation,
                        }
                    }
                })
            })
            .or_else(|| {
                runners.last().map(|row| match row {
                    RunnerStatusFact::Enrollment { runner, .. } => {
                        RunnerStatusAfter::Enrollment(runner.into_uuid())
                    }
                    RunnerStatusFact::Placement { session, .. } => {
                        RunnerStatusAfter::Placement(session.into_uuid())
                    }
                })
            })
    } else {
        None
    };
    transaction.commit().await?;
    Ok(RunnerStatusPage {
        runners,
        failures,
        leaks,
        next_after,
    })
}

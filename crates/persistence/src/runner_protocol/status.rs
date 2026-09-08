//! Read-only traversal of current placements and retained provisioning refusals.

use super::*;
use crate::process_read::{
    ProcessReadError, ProcessRunnerProjection, load_process_runner_projection,
};
use signalbox_domain::RunnerReplacementProvisioning;

/// Exclusive position in the failure-before-leak traversal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerStatusAfter {
    /// Last emitted immutable provisioning authorization.
    OperationFailure(Uuid),
    /// A leak cursor is beyond every provisioning failure.
    WorkspaceLeak,
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
pub struct RunnerStatusFailure {
    pub authorization: RunnerReplacementProvisioning,
    pub category: signalbox_domain::RunnerProvisioningFailureKind,
    pub detail: serde_json::Value,
}

/// First-page placement facts and a bounded page of failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerStatusPage {
    pub runners: Vec<RunnerStatusFact>,
    pub failures: Vec<RunnerStatusFailure>,
    pub next_after: Option<Uuid>,
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
    if after.is_none() {
        let rows = sqlx::query(
            "SELECT enrollment.runner_id, receipt.request_id, enrollment.state_kind,
            connection.state_kind AS connection_state
            FROM runner_enrollment AS enrollment
            JOIN runner_enrollment_request_receipt AS receipt USING (enrollment_id)
            LEFT JOIN LATERAL (SELECT state_kind FROM runner_connection_event
                WHERE enrollment_id = enrollment.enrollment_id
                ORDER BY connection_epoch DESC, event_ordinal DESC LIMIT 1) AS connection ON true
            ORDER BY enrollment.runner_id",
        )
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
        let sessions: Vec<Uuid> = sqlx::query_scalar(
            "SELECT session_id FROM runner_current_session_placement ORDER BY session_id",
        )
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
    if !matches!(after, Some(RunnerStatusAfter::WorkspaceLeak)) {
        let last = match after {
            Some(RunnerStatusAfter::OperationFailure(id)) => Some(id),
            _ => None,
        };
        let rows = sqlx::query("SELECT operation.*, failure.failure_kind, failure.detail
            FROM runner_replacement_provisioning_failure AS failure
            JOIN runner_replacement_provisioning_authorization AS operation USING (authorization_id)
            WHERE ($1::uuid IS NULL OR failure.authorization_id > $1)
            ORDER BY failure.authorization_id LIMIT $2")
            .bind(last).bind(i64::from(page_size) + 1).fetch_all(&mut *transaction).await?;
        for row in rows {
            use signalbox_domain::RunnerProvisioningFailureKind as Kind;
            let category = match row.try_get::<String, _>("failure_kind")?.as_str() {
                "credential_unavailable" => Kind::CredentialUnavailable,
                "repository_unavailable" => Kind::RepositoryUnavailable,
                "sandbox_unavailable" => Kind::SandboxUnavailable,
                "workspace_conflict" => Kind::WorkspaceConflict,
                _ => return Err(RunnerStatusError::Corruption),
            };
            failures.push(RunnerStatusFailure {
                authorization: super::provisioning::decode_authorization(&row)?,
                category,
                detail: row.try_get("detail")?,
            });
        }
    }
    let next_after = if failures.len() > page_size as usize {
        failures.truncate(page_size as usize);
        failures
            .last()
            .map(|row| row.authorization.authorization.into_uuid())
    } else {
        None
    };
    transaction.commit().await?;
    Ok(RunnerStatusPage {
        runners,
        failures,
        next_after,
    })
}

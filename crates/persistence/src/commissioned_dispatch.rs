//! Durable operator-commissioned dispatch admission and audit.
//!
//! One transaction commits the same composite a repository-watch dispatch
//! action commits — created session, recorded authority fence, initial input
//! with its reserved turn, and commissioned goal — so a commissioned session is
//! never durably visible without the fence and goal stating what it acts
//! under. The approval judge consumes the fence through the same authority
//! loading as the repository-watch source (`crate::approval_judge`).

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{CommissionedDispatchFence, PreparedCommissionedDispatch};
use signalbox_domain::{
    CommissionedDispatchId, DurableCommandId, FrozenAliasDefinition, ModelAlias, SessionId,
};
use sqlx::{PgPool, Row};

use crate::{commit_failure_is_ambiguous, mapping::session_id_to_uuid};

/// Durable effect of one commission request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommissionDispatchOutcome {
    /// This transaction committed the complete composite.
    Dispatched {
        /// The recorded append-only dispatch identity.
        dispatch: CommissionedDispatchId,
        /// The created session.
        session: SessionId,
    },
    /// The command identity already committed an equal commission.
    Replayed {
        /// The recorded append-only dispatch identity.
        dispatch: CommissionedDispatchId,
        /// The previously created session.
        session: SessionId,
    },
    /// The command identity already names a different commission.
    ConflictingReuse,
}

/// Database or durable-shape failure while committing one commission.
#[derive(Debug)]
pub enum CommissionedDispatchRepositoryError {
    /// PostgreSQL failure with explicit commit ambiguity.
    Database {
        /// Original driver error.
        source: sqlx::Error,
        /// Whether a failed commit acknowledgement leaves outcome unknown.
        commit_ambiguous: bool,
    },
    /// Session creation refused inside the composite transaction.
    SessionCreation(crate::create_session::CreateSessionRepositoryError),
    /// Initial-input admission refused inside the composite transaction.
    InitialInput(crate::submit_input::SubmitInputRepositoryError),
    /// Goal commissioning refused inside the composite transaction.
    GoalCommission(crate::goal::GoalRepositoryError),
    /// Durable rows contradicted the closed composite shape.
    Corruption(&'static str),
}

impl fmt::Display for CommissionedDispatchRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database {
                commit_ambiguous: true,
                ..
            } => formatter.write_str("commissioned dispatch commit outcome is ambiguous"),
            Self::Database {
                commit_ambiguous: false,
                ..
            } => formatter.write_str("commissioned dispatch database operation failed"),
            Self::SessionCreation(error) => error.fmt(formatter),
            Self::InitialInput(error) => error.fmt(formatter),
            Self::GoalCommission(error) => error.fmt(formatter),
            Self::Corruption(reason) => write!(
                formatter,
                "commissioned dispatch storage is inconsistent: {reason}"
            ),
        }
    }
}

impl Error for CommissionedDispatchRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::SessionCreation(error) => Some(error),
            Self::InitialInput(error) => Some(error),
            Self::GoalCommission(error) => Some(error),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for CommissionedDispatchRepositoryError {
    fn from(source: sqlx::Error) -> Self {
        Self::Database {
            source,
            commit_ambiguous: false,
        }
    }
}

/// PostgreSQL implementation of atomic commissioned-dispatch admission.
#[derive(Clone, Debug)]
pub struct PostgresCommissionedDispatchStore {
    pool: PgPool,
    credential_pin: crate::SessionCredentialPin,
}

impl PostgresCommissionedDispatchStore {
    /// Uses the exact credential pin session creation records.
    pub fn new(pool: PgPool, credential_pin: crate::SessionCredentialPin) -> Self {
        Self {
            pool,
            credential_pin,
        }
    }

    /// Commits the complete commission, replaying an already-committed equal one.
    ///
    /// Replay equality binds the create-command identity to the recorded
    /// template, fence, and commissioned statement. A command identity claimed
    /// by anything other than a committed commission — another command kind, or
    /// an ordinary session creation with no fence row — is a conflicting reuse
    /// rather than corruption, because the caller's identity names intent this
    /// store never recorded. The alias resolver serves the initial input's
    /// frozen model configuration exactly as it does for a repository-watch
    /// dispatch.
    pub async fn commission<SelectDefinition>(
        &self,
        prepared: PreparedCommissionedDispatch,
        select_definition: SelectDefinition,
    ) -> Result<CommissionDispatchOutcome, CommissionedDispatchRepositoryError>
    where
        SelectDefinition: Fn(ModelAlias) -> Option<FrozenAliasDefinition> + Copy + Send,
    {
        let mut transaction = self.pool.begin().await?;
        let command = prepared.prepared_session().command();
        let command_id = command.command_id();
        let provenance = command.template_provenance().ok_or(
            CommissionedDispatchRepositoryError::Corruption(
                "commissioned session lacks template provenance",
            ),
        )?;
        let template_name = provenance.name().as_str().to_owned();
        let template_digest = provenance.content_digest().as_bytes().to_vec();
        if let Some(recorded) = load_recorded_commission(&mut transaction, command_id).await? {
            transaction.rollback().await?;
            let statement = statement_text(&prepared).ok_or(
                CommissionedDispatchRepositoryError::Corruption(
                    "commissioned goal action is not an attachment",
                ),
            )?;
            let equal = recorded.template_name == template_name
                && recorded.fence_matches(prepared.fence())
                && recorded.statement.as_deref() == Some(statement);
            return Ok(if equal {
                CommissionDispatchOutcome::Replayed {
                    dispatch: recorded.dispatch,
                    session: recorded.session,
                }
            } else {
                CommissionDispatchOutcome::ConflictingReuse
            });
        }
        let claimed_elsewhere: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM durable_command WHERE command_id = $1)",
        )
        .bind(command_id.as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if claimed_elsewhere {
            // Re-read the fence row before answering: an equal commission that
            // committed between the two reads above is a replay, not a reuse.
            let recorded = load_recorded_commission(&mut transaction, command_id).await?;
            transaction.rollback().await?;
            return Ok(match recorded {
                Some(recorded)
                    if recorded.template_name == template_name
                        && recorded.fence_matches(prepared.fence())
                        && recorded.statement.as_deref() == statement_text(&prepared) =>
                {
                    CommissionDispatchOutcome::Replayed {
                        dispatch: recorded.dispatch,
                        session: recorded.session,
                    }
                }
                _ => CommissionDispatchOutcome::ConflictingReuse,
            });
        }
        let (
            dispatch_id,
            fence,
            prepared_session,
            initial_input,
            accepted_input,
            turn,
            cancellation_entry,
            cancellation_frontier,
            goal,
        ) = prepared.into_parts();
        let session = prepared_session.applied_result().session();
        if initial_input.session() != session {
            return Err(CommissionedDispatchRepositoryError::Corruption(
                "commissioned initial input targets another session",
            ));
        }
        if goal.session() != session {
            return Err(CommissionedDispatchRepositoryError::Corruption(
                "commissioned goal targets another session",
            ));
        }
        crate::create_session::insert_fresh_prepared(
            &mut transaction,
            prepared_session,
            &self.credential_pin,
        )
        .await
        .map_err(CommissionedDispatchRepositoryError::SessionCreation)?;
        insert_commissioned_dispatch(
            &mut transaction,
            dispatch_id,
            session,
            command_id,
            &template_name,
            &template_digest,
            &fence,
        )
        .await?;
        crate::submit_input::insert_fresh_initial_input(
            &mut transaction,
            initial_input,
            accepted_input,
            turn,
            cancellation_entry,
            cancellation_frontier,
            select_definition,
        )
        .await
        .map_err(CommissionedDispatchRepositoryError::InitialInput)?;
        // The commission adopts the turn just accepted above rather than
        // scheduling one of its own, so the session runs its template once,
        // against the operator's context, under the generation that turn is
        // recorded in — exactly as a repository-watch dispatch action does.
        crate::goal::insert_fresh_commissioned_goal(&mut transaction, goal, accepted_input, turn)
            .await
            .map_err(CommissionedDispatchRepositoryError::GoalCommission)?;
        transaction.commit().await.map_err(|error| {
            CommissionedDispatchRepositoryError::Database {
                commit_ambiguous: commit_failure_is_ambiguous(&error),
                source: error,
            }
        })?;
        Ok(CommissionDispatchOutcome::Dispatched {
            dispatch: dispatch_id,
            session,
        })
    }
}

/// Borrows the statement the prepared commission attaches.
fn statement_text(prepared: &PreparedCommissionedDispatch) -> Option<&str> {
    match prepared.goal().action() {
        signalbox_domain::GoalUserAction::Attach(statement) => Some(statement.as_str()),
        _ => None,
    }
}

/// The committed facts one replayed commission is compared against.
struct RecordedCommission {
    dispatch: CommissionedDispatchId,
    session: SessionId,
    template_name: String,
    target_kind: String,
    repository: String,
    pull_request_number: Option<Decimal>,
    head_sha: Option<String>,
    head_repository: Option<String>,
    head_branch: Option<String>,
    base_branch: Option<String>,
    branch: Option<String>,
    statement: Option<String>,
}

impl RecordedCommission {
    fn fence_matches(&self, fence: &CommissionedDispatchFence) -> bool {
        match fence {
            CommissionedDispatchFence::PullRequest {
                repository,
                pull_request,
                head_sha,
                head_repository,
                head_branch,
                base_branch,
            } => {
                self.target_kind == "pull_request"
                    && self.repository == repository.as_str()
                    && self.pull_request_number == Some(Decimal::from(pull_request.get()))
                    && self.head_sha.as_deref() == Some(head_sha.as_str())
                    && self.head_repository.as_deref() == Some(head_repository.as_str())
                    && self.head_branch.as_deref() == Some(head_branch.as_str())
                    && self.base_branch.as_deref() == Some(base_branch.as_str())
            }
            CommissionedDispatchFence::Branch { repository, branch } => {
                self.target_kind == "branch"
                    && self.repository == repository.as_str()
                    && self.branch.as_deref() == Some(branch.as_str())
            }
        }
    }
}

async fn load_recorded_commission(
    connection: &mut sqlx::PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<RecordedCommission>, CommissionedDispatchRepositoryError> {
    let Some(row) = sqlx::query(
        "SELECT dispatch.dispatch_id, dispatch.session_id, dispatch.template_name,
                dispatch.target_kind, dispatch.repository, dispatch.pull_request_number,
                dispatch.head_sha, dispatch.head_repository, dispatch.head_branch,
                dispatch.base_branch, dispatch.branch, commissioned.statement
           FROM commissioned_dispatch AS dispatch
           LEFT JOIN goal_event AS commissioned
             ON commissioned.session_id = dispatch.session_id
            AND commissioned.generation = 1
            AND commissioned.event_kind = 'commissioned'
          WHERE dispatch.create_command_id = $1",
    )
    .bind(command_id.as_uuid())
    .fetch_optional(&mut *connection)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(RecordedCommission {
        dispatch: CommissionedDispatchId::from_uuid(row.try_get("dispatch_id")?),
        session: SessionId::from_uuid(row.try_get("session_id")?),
        template_name: row.try_get("template_name")?,
        target_kind: row.try_get("target_kind")?,
        repository: row.try_get("repository")?,
        pull_request_number: row.try_get("pull_request_number")?,
        head_sha: row.try_get("head_sha")?,
        head_repository: row.try_get("head_repository")?,
        head_branch: row.try_get("head_branch")?,
        base_branch: row.try_get("base_branch")?,
        branch: row.try_get("branch")?,
        statement: row.try_get("statement")?,
    }))
}

async fn insert_commissioned_dispatch(
    connection: &mut sqlx::PgConnection,
    dispatch: CommissionedDispatchId,
    session: SessionId,
    create_command: DurableCommandId,
    template_name: &str,
    template_digest: &[u8],
    fence: &CommissionedDispatchFence,
) -> Result<(), CommissionedDispatchRepositoryError> {
    let (
        target_kind,
        repository,
        pull_request_number,
        head_sha,
        head_repository,
        head_branch,
        base_branch,
        branch,
    ) = match fence {
        CommissionedDispatchFence::PullRequest {
            repository,
            pull_request,
            head_sha,
            head_repository,
            head_branch,
            base_branch,
        } => (
            "pull_request",
            repository.as_str(),
            Some(Decimal::from(pull_request.get())),
            Some(head_sha.as_str()),
            Some(head_repository.as_str()),
            Some(head_branch.as_str()),
            Some(base_branch.as_str()),
            None,
        ),
        CommissionedDispatchFence::Branch { repository, branch } => (
            "branch",
            repository.as_str(),
            None,
            None,
            None,
            None,
            None,
            Some(branch.as_str()),
        ),
    };
    sqlx::query(
        "INSERT INTO commissioned_dispatch
            (dispatch_id, session_id, create_command_id, template_name,
             template_content_digest, target_kind, repository,
             pull_request_number, head_sha, head_repository, head_branch,
             base_branch, branch)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(dispatch.as_uuid())
    .bind(session_id_to_uuid(session))
    .bind(create_command.as_uuid())
    .bind(template_name)
    .bind(template_digest)
    .bind(target_kind)
    .bind(repository)
    .bind(pull_request_number)
    .bind(head_sha)
    .bind(head_repository)
    .bind(head_branch)
    .bind(base_branch)
    .bind(branch)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

//! Durable operator-commissioned dispatch admission and audit.
//!
//! One transaction commits the same composite a repository-watch dispatch
//! action commits — created session, recorded authority fence, initial input
//! with its reserved turn, and commissioned goal — so a commissioned session is
//! never durably visible without the fence and goal stating what it acts
//! under. The approval judge consumes the fence through the same authority
//! loading as the repository-watch source (`crate::approval_judge`).

use std::{error::Error, fmt, time::Duration};

use rust_decimal::Decimal;
use signalbox_application::{
    CommissionDispatchRequest, CommissionedDispatchFence, PreparedCommissionedDispatch,
    SubmitInputIdGenerator,
};
use signalbox_domain::{
    CommandPrincipal, CommissionedDispatchId, DispatchingModule, DurableCommandId,
    FrozenAliasDefinition, ModelAlias, SessionId,
};
use sqlx::{PgPool, Row};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{session_id_from_uuid, session_id_to_uuid},
};

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
    /// Another live commissioned session already owns this pull request.
    TargetBusy {
        /// The live session that prevents a racing dispatch.
        session: SessionId,
    },
    /// A recent terminal session still holds the target cool-off.
    TargetCoolingOff {
        /// The recent session that established the cool-off.
        session: SessionId,
    },
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

impl CommissionedDispatchRepositoryError {
    /// Whether this failure may have committed the complete commission.
    pub const fn commit_ambiguous(&self) -> bool {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => *commit_ambiguous,
            Self::SessionCreation(error) => matches!(
                error,
                crate::create_session::CreateSessionRepositoryError::CommitAmbiguous(_)
            ),
            Self::InitialInput(error) => matches!(
                error,
                crate::submit_input::SubmitInputRepositoryError::CommitAmbiguous(_)
            ),
            Self::GoalCommission(error) => {
                matches!(error, crate::goal::GoalRepositoryError::CommitAmbiguous(_))
            }
            Self::Corruption(_) => false,
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

    /// Loads the committed commission a create-command identity names, if any.
    ///
    /// This is the replay lookup the daemon runs before resolving the live
    /// template catalog; it needs nothing but the durable record, so an
    /// ambiguous first commit stays discoverable through the required retry
    /// path even after template configuration drifts.
    pub async fn load(
        &self,
        command: DurableCommandId,
    ) -> Result<Option<RecordedCommissionedDispatch>, CommissionedDispatchRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        Ok(load_recorded_commission(&mut connection, command)
            .await?
            .map(|inner| RecordedCommissionedDispatch { inner }))
    }

    /// Reports whether the durable-command registry holds this identity.
    ///
    /// The daemon consults this only on the unknown-template path, where
    /// `load` has already ruled out a committed commission: a registered
    /// identity then belongs to another command kind or an ordinary session
    /// creation, and the refusal must name that conflicting reuse rather
    /// than the template's `invalid_request`, which claims no command.
    pub async fn identity_claimed(
        &self,
        command: DurableCommandId,
    ) -> Result<bool, CommissionedDispatchRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        let claimed = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM durable_command WHERE command_id = $1)",
        )
        .bind(crate::mapping::durable_command_id_to_uuid(command))
        .fetch_one(&mut *connection)
        .await?;
        Ok(claimed)
    }

    /// Commits the complete commission, replaying an already-committed equal one.
    ///
    /// Replay equality binds the create-command identity to the recorded
    /// template, fence, commissioned statement, and the digest of the initial
    /// content. A command identity claimed by anything other than a committed
    /// commission — another command kind, or an ordinary session creation with
    /// no fence row — is a conflicting reuse rather than corruption, because
    /// the caller's identity names intent this store never recorded; a claim
    /// lost to a concurrent commit re-reads the winner and answers the same
    /// way. The alias resolver serves the initial input's frozen model
    /// configuration exactly as it does for a repository-watch dispatch.
    pub async fn commission<SelectDefinition>(
        &self,
        prepared: PreparedCommissionedDispatch,
        ids: &mut impl SubmitInputIdGenerator,
        select_definition: SelectDefinition,
    ) -> Result<CommissionDispatchOutcome, CommissionedDispatchRepositoryError>
    where
        SelectDefinition: Fn(ModelAlias) -> Option<FrozenAliasDefinition> + Copy + Send,
    {
        self.commission_with_cool_off(prepared, ids, None, select_definition)
            .await
    }

    /// Commits a commission only after the target's locked cool-off has elapsed.
    pub async fn commission_after_cool_off<SelectDefinition>(
        &self,
        prepared: PreparedCommissionedDispatch,
        ids: &mut impl SubmitInputIdGenerator,
        cool_off: Duration,
        select_definition: SelectDefinition,
    ) -> Result<CommissionDispatchOutcome, CommissionedDispatchRepositoryError>
    where
        SelectDefinition: Fn(ModelAlias) -> Option<FrozenAliasDefinition> + Copy + Send,
    {
        self.commission_with_cool_off(prepared, ids, Some(cool_off), select_definition)
            .await
    }

    async fn commission_with_cool_off<SelectDefinition>(
        &self,
        prepared: PreparedCommissionedDispatch,
        ids: &mut impl SubmitInputIdGenerator,
        cool_off: Option<Duration>,
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
        let statement = statement_text(&prepared)
            .ok_or(CommissionedDispatchRepositoryError::Corruption(
                "commissioned goal action is not an attachment",
            ))?
            .to_owned();
        let content_digest = prepared.initial_content_digest();
        if let Some(recorded) = load_recorded_commission(&mut transaction, command_id).await? {
            transaction.rollback().await?;
            return Ok(replay_or_conflict(
                &recorded,
                &template_name,
                prepared.fence(),
                &statement,
                &content_digest,
            ));
        }
        let live_target = lock_live_pull_request_target(&mut transaction, prepared.fence()).await?;
        // The target lock can have waited behind an equal commission. Re-read
        // command identity before treating that winner as unrelated live work.
        if let Some(recorded) = load_recorded_commission(&mut transaction, command_id).await? {
            transaction.rollback().await?;
            return Ok(replay_or_conflict(
                &recorded,
                &template_name,
                prepared.fence(),
                &statement,
                &content_digest,
            ));
        }
        if let Some(session) = live_target {
            transaction.rollback().await?;
            return Ok(CommissionDispatchOutcome::TargetBusy { session });
        }
        if let Some(cool_off) = cool_off
            && let Some(session) =
                recent_pull_request_session(&mut transaction, prepared.fence(), cool_off).await?
        {
            transaction.rollback().await?;
            return Ok(CommissionDispatchOutcome::TargetCoolingOff { session });
        }
        let (dispatch_id, fence, prepared_session, initial_input, goal) = prepared.into_parts();
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
        let principal = CommandPrincipal::Module {
            module: DispatchingModule::CommissionedDispatch,
        };
        if !crate::create_session::claim_create_session_command(
            &mut transaction,
            command_id,
            principal,
        )
        .await
        .map_err(CommissionedDispatchRepositoryError::SessionCreation)?
        {
            // Lost the claim to a concurrent commit. Re-read the winner under
            // a fresh statement snapshot: an equal committed commission is a
            // replay; anything else the identity names — an unequal
            // commission, an ordinary session creation, another command kind —
            // is a conflicting reuse, never corruption.
            let recorded = load_recorded_commission(&mut transaction, command_id).await?;
            transaction.rollback().await?;
            return Ok(match recorded {
                Some(recorded) => replay_or_conflict(
                    &recorded,
                    &template_name,
                    &fence,
                    &statement,
                    &content_digest,
                ),
                None => CommissionDispatchOutcome::ConflictingReuse,
            });
        }
        crate::create_session::insert_prepared(
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
            &content_digest,
            &fence,
        )
        .await?;
        let minted = crate::submit_input::insert_fresh_initial_input(
            &mut transaction,
            initial_input,
            principal,
            ids,
            select_definition,
        )
        .await
        .map_err(CommissionedDispatchRepositoryError::InitialInput)?;
        // The commission adopts the turn just accepted above rather than
        // scheduling one of its own, so the session runs its template once,
        // against the operator's context, under the generation that turn is
        // recorded in — exactly as a repository-watch dispatch action does.
        crate::goal::insert_fresh_commissioned_goal(
            &mut transaction,
            goal,
            principal,
            minted.accepted_input,
            minted.turn,
        )
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

async fn recent_pull_request_session(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &CommissionedDispatchFence,
    cool_off: Duration,
) -> Result<Option<SessionId>, CommissionedDispatchRepositoryError> {
    let CommissionedDispatchFence::PullRequest {
        repository,
        pull_request,
        ..
    } = fence
    else {
        return Ok(None);
    };
    let session: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT dispatch.session_id
           FROM commissioned_dispatch AS dispatch
          WHERE dispatch.target_kind = 'pull_request'
            AND dispatch.repository = $1
            AND dispatch.pull_request_number = $2
            AND dispatch.recorded_at > clock_timestamp() - $3 * interval '1 second'
          ORDER BY dispatch.recorded_at DESC, dispatch.dispatch_id DESC, dispatch.session_id DESC
          LIMIT 1",
    )
    .bind(repository.as_str())
    .bind(Decimal::from(pull_request.get()))
    .bind(i64::try_from(cool_off.as_secs()).unwrap_or(i64::MAX))
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(session.map(session_id_from_uuid))
}

async fn lock_live_pull_request_target(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &CommissionedDispatchFence,
) -> Result<Option<SessionId>, CommissionedDispatchRepositoryError> {
    let CommissionedDispatchFence::PullRequest {
        repository,
        pull_request,
        ..
    } = fence
    else {
        return Ok(None);
    };
    let pull_request = Decimal::from(pull_request.get());
    lock_pull_request_target(transaction, repository.as_str(), &pull_request).await?;
    live_pull_request_session(transaction, repository.as_str(), &pull_request, None)
        .await
        .map_err(Into::into)
}

#[derive(Debug, sqlx::FromRow)]
struct PullRequestTargetRow {
    repository: String,
    pull_request_number: Decimal,
}

pub(crate) async fn lock_competing_pull_request_session(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: SessionId,
) -> Result<Option<SessionId>, sqlx::Error> {
    let target: Option<PullRequestTargetRow> = sqlx::query_as(
        "SELECT repository, pull_request_number
           FROM commissioned_dispatch
          WHERE session_id = $1 AND target_kind = 'pull_request'
          ORDER BY recorded_at DESC
          LIMIT 1",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(PullRequestTargetRow {
        repository,
        pull_request_number,
    }) = target
    else {
        return Ok(None);
    };
    lock_pull_request_target(transaction, &repository, &pull_request_number).await?;
    live_pull_request_session(
        transaction,
        &repository,
        &pull_request_number,
        Some(session),
    )
    .await
}

pub(crate) async fn lock_pull_request_target(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    repository: &str,
    pull_request: &Decimal,
) -> Result<(), sqlx::Error> {
    let key = format!("commissioned-dispatch:{repository}:{pull_request}");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn live_pull_request_session(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    repository: &str,
    pull_request: &Decimal,
    excluded_session: Option<SessionId>,
) -> Result<Option<SessionId>, sqlx::Error> {
    let session: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT dispatch.session_id
           FROM commissioned_dispatch AS dispatch
          WHERE dispatch.target_kind = 'pull_request'
            AND dispatch.repository = $1
            AND dispatch.pull_request_number = $2
            AND ($3::uuid IS NULL OR dispatch.session_id <> $3)
            AND coalesce((
                SELECT event.event_kind IN ('commissioned', 'resumed', 'superseded')
                  FROM goal_event AS event
                 WHERE event.session_id = dispatch.session_id
                 ORDER BY event.event_ordinal DESC LIMIT 1
            ), false)
          ORDER BY dispatch.recorded_at DESC, dispatch.session_id DESC
          LIMIT 1",
    )
    .bind(repository)
    .bind(pull_request)
    .bind(excluded_session.map(session_id_to_uuid))
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(session.map(session_id_from_uuid))
}

/// Borrows the statement the prepared commission attaches.
fn statement_text(prepared: &PreparedCommissionedDispatch) -> Option<&str> {
    match prepared.goal().action() {
        signalbox_domain::GoalUserAction::Attach(statement) => Some(statement.as_str()),
        _ => None,
    }
}

/// Answers replay for an equal committed commission, reuse for anything else.
fn replay_or_conflict(
    recorded: &RecordedCommission,
    template_name: &str,
    fence: &CommissionedDispatchFence,
    statement: &str,
    content_digest: &[u8; 32],
) -> CommissionDispatchOutcome {
    let equal = recorded.template_name == template_name
        && recorded.fence_matches(fence)
        && recorded.statement.as_deref() == Some(statement)
        && recorded.initial_content_digest == content_digest;
    if equal {
        CommissionDispatchOutcome::Replayed {
            dispatch: recorded.dispatch,
            session: recorded.session,
        }
    } else {
        CommissionDispatchOutcome::ConflictingReuse
    }
}

/// One committed commission, loadable by its create-command identity alone.
///
/// The daemon consults this before resolving the live template catalog, so a
/// retry of a committed commission replays even after configuration removed or
/// renamed the template it was commissioned from.
#[derive(Debug)]
pub struct RecordedCommissionedDispatch {
    inner: RecordedCommission,
}

impl RecordedCommissionedDispatch {
    /// Returns the recorded append-only dispatch identity.
    #[must_use]
    pub const fn dispatch(&self) -> CommissionedDispatchId {
        self.inner.dispatch
    }

    /// Returns the previously created session.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.inner.session
    }

    /// Reports whether this record is the request's exact committed equal.
    ///
    /// The comparison is the same replay equality `commission` enforces:
    /// template name, fence, commissioned statement, and the digest of the
    /// initial content.
    #[must_use]
    pub fn matches(&self, request: &CommissionDispatchRequest) -> bool {
        matches!(
            replay_or_conflict(
                &self.inner,
                request.template().as_str(),
                request.fence(),
                request.statement().as_str(),
                &request.initial_content_digest(),
            ),
            CommissionDispatchOutcome::Replayed { .. }
        )
    }
}

/// The committed facts one replayed commission is compared against.
#[derive(Debug)]
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
    initial_content_digest: Vec<u8>,
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
                dispatch.base_branch, dispatch.branch, dispatch.initial_content_digest,
                commissioned.statement
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
        initial_content_digest: row.try_get("initial_content_digest")?,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn insert_commissioned_dispatch(
    connection: &mut sqlx::PgConnection,
    dispatch: CommissionedDispatchId,
    session: SessionId,
    create_command: DurableCommandId,
    template_name: &str,
    template_digest: &[u8],
    initial_content_digest: &[u8; 32],
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
             template_content_digest, initial_content_digest, target_kind,
             repository, pull_request_number, head_sha, head_repository,
             head_branch, base_branch, branch, recorded_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                 clock_timestamp())",
    )
    .bind(dispatch.as_uuid())
    .bind(session_id_to_uuid(session))
    .bind(create_command.as_uuid())
    .bind(template_name)
    .bind(template_digest)
    .bind(initial_content_digest.as_slice())
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

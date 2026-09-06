//! Repository-watch v2's bounded module-local persistence surface.
//!
//! The crate has one Signalbox dependency: `signalbox-ownership-seam`. SQL is
//! unqualified and must run on a pool whose effective role and search path are
//! confined to `mod_repo_watch`.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    num::NonZeroU64,
};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use signalbox_ownership_seam::{
    BranchName, CheckConclusion, ChecksOutcome, CommandOutsideSeam, CommandSettlement, CommitSha,
    CreateSession, CreateSessionOutcome, FinishCondition, LifecycleEvent, LifecycleEventKind,
    MergeableState, ModuleDispatch, OffsetDateTime, PullRequestBody, PullRequestNumber,
    PullRequestTitle, ReactionChange, ReactionSubject, RepoWatchAuthorLogin, RepoWatchDispatchId,
    RepoWatchEvent, RepoWatchEventContentIdentityV1,
    RepoWatchEventIdentityFrontierEntryV1, RepoWatchEventKindNameV1, RepoWatchEventKindV1,
    RepoWatchEventTarget, RepoWatchRule, RepoWatchRuleActionV1, RepoWatchRuleId,
    RepoWatchRuleVersion, RepositorySlug, ReviewState, SessionCommand, SessionCreationCause,
    SessionId, SessionLifecycleCommand, SessionLifecycleOperation, SessionOwnership, StartGate,
    StopStickiness,
};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

mod baseline;
mod github;

use baseline::observation_payload;
pub use github::{GitHubClient, GitHubClientError};

const CONFIGURATION_LOCK: &str = "repo-watch\u{1f}configuration";

/// Current provider state for one watched repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryState<'a> {
    /// Canonical repository identity.
    pub repository: &'a RepositorySlug,
    /// Current default branch.
    pub default_branch: &'a BranchName,
    /// Current default-branch head.
    pub default_head: &'a CommitSha,
    /// When the complete observation was made.
    pub observed_at: OffsetDateTime,
}

/// Provider lifecycle retained for one current pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullRequestLifecycle {
    /// The pull request remains open.
    Open,
    /// It closed without merge.
    Closed,
    /// It merged.
    Merged,
}

impl PullRequestLifecycle {
    const fn storage(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }
}

/// Normalized current provider state for one pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestState<'a> {
    /// Watched repository.
    pub repository: &'a RepositorySlug,
    /// Positive provider number.
    pub number: PullRequestNumber,
    /// Provider lifecycle.
    pub lifecycle: PullRequestLifecycle,
    /// Current head revision.
    pub head: &'a CommitSha,
    /// Repository holding the head branch.
    pub head_repository: &'a RepositorySlug,
    /// Current head branch.
    pub head_branch: &'a BranchName,
    /// Current base branch.
    pub base_branch: &'a BranchName,
    /// Current title.
    pub title: &'a PullRequestTitle,
    /// Current body.
    pub body: &'a PullRequestBody,
    /// Whether GitHub marks it draft.
    pub draft: bool,
    /// Current author when GitHub retains the identity.
    pub author: Option<&'a RepoWatchAuthorLogin>,
    /// When the complete observation was made.
    pub observed_at: OffsetDateTime,
}

/// Complete current provider projection committed with one frontier candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryProjection<'a> {
    /// Repository-level provider state.
    pub repository: RepositoryState<'a>,
    /// Complete current pull-request projection for the repository.
    pub pull_requests: Vec<PullRequestState<'a>>,
    /// Complete normalized state used by the repository-event differ.
    pub comparison_baseline: &'a signalbox_ownership_seam::RepoWatchObservation,
    /// Compact baselines retained for merged pull requests.
    pub merged_baselines: &'a [signalbox_ownership_seam::RepoWatchMergedPullRequestBaselineV1],
}

/// Authenticated webhook intake retained until its caller-selected expiry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookDelivery<'a> {
    /// Watched repository selected by the authenticated hook.
    pub repository: &'a RepositorySlug,
    /// Positive provider hook identity.
    pub hook_id: u64,
    /// Provider delivery identity.
    pub delivery_id: Uuid,
    /// GitHub event header.
    pub event: &'a str,
    /// Optional GitHub action member.
    pub action: Option<&'a str>,
    /// Exact authenticated bytes.
    pub body: &'a [u8],
    /// Admission time.
    pub received_at: OffsetDateTime,
    /// Row-specific TTL boundary; no global retention window is selected.
    pub expires_at: OffsetDateTime,
}

/// Result of idempotently admitting a webhook delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookAdmission {
    /// The delivery was new.
    Inserted,
    /// The exact delivery was already retained.
    Replayed,
    /// The provider identity was already bound to different bytes or metadata.
    ConflictingReuse,
}

/// Terminal processing result for one admitted webhook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookDisposition {
    /// The delivery updated module state.
    Applied,
    /// It was valid but produced no current-state change.
    Ignored,
    /// Its authenticated body could not form an admitted event.
    Rejected,
}

/// Result of idempotently recording one normalized GitHub fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventAdmission {
    /// The fact was new.
    Inserted,
    /// The exact fact was already retained.
    Replayed,
}

/// One normalized fact supplied with its content identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventCandidate<'a> {
    /// Complete normalized fact.
    pub event: &'a RepoWatchEvent,
    /// Source-independent identity assigned to the fact.
    pub content_identity: RepoWatchEventContentIdentityV1,
}

/// Closed producer recorded with every durable repository-watch fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventProducer {
    /// A complete or targeted provider poll produced the fact.
    Poll,
    /// An authenticated primary-mode webhook produced the fact.
    Webhook,
}

impl EventProducer {
    const fn storage(self) -> &'static str {
        match self {
            Self::Poll => "poll",
            Self::Webhook => "webhook",
        }
    }
}

/// Result of atomically committing a complete frontier candidate and event batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrontierEventAdmission {
    /// The frontier and ordered facts committed together.
    Committed {
        /// Generation assigned to this complete candidate.
        generation: u64,
        /// Admission of each ordered fact.
        events: Box<[EventAdmission]>,
    },
    /// The complete candidate already matches the stored frontier and carried no facts.
    Unchanged,
    /// A durable event identity was already bound to a different fact.
    ConflictingReuse,
    /// A newer frontier entry already committed for this repository.
    Stale,
}

/// Result of idempotently releasing one frontier stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrontierReleaseAdmission {
    /// The stream was removed at this generation.
    Released { generation: u64 },
    /// The same release already committed at this generation.
    Replayed { generation: u64 },
    /// The stream is absent and the latest frontier mutation was not its release.
    Absent,
    /// The repository frontier advanced after the caller observed it.
    Stale,
}

/// Result of activating one configured rule revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleAdmission {
    /// The rule was first observed.
    Inserted,
    /// A newer revision became active.
    Updated,
    /// The exact active revision was already retained.
    Replayed,
    /// The revision was already bound to different rule semantics.
    ConflictingReuse,
    /// The supplied revision predates the active revision.
    Stale,
}

/// Result of idempotently recording one emitted command.
#[derive(Clone, Debug)]
pub enum DispatchAdmission {
    /// The complete action batch was new.
    Inserted,
    /// This rule revision and event already have a retained action batch.
    Replayed {
        /// The exact retained command batch decoded by core.
        commands: Box<[PlannedCommand]>,
    },
    /// A durable dispatch or command identity was already bound elsewhere.
    ConflictingReuse,
    /// The named rule revision is not currently active in this repository.
    InactiveRule,
}

/// Module provenance retained beside one checked seam command.
#[derive(Clone, Debug)]
pub struct PlannedCommand {
    dispatch: RepoWatchDispatchId,
    action_ordinal: u64,
    repository: RepositorySlug,
    rule_id: RepoWatchRuleId,
    rule_revision: RepoWatchRuleVersion,
    event_id: signalbox_ownership_seam::RepoWatchEventId,
    trigger_sequence: Option<u64>,
    command: SessionCommand,
}

impl PlannedCommand {
    fn new(
        dispatch: RepoWatchDispatchId,
        action_ordinal: u64,
        rule: &RepoWatchRule,
        event: &RepoWatchEvent,
        trigger_sequence: Option<u64>,
        command: SessionCommand,
    ) -> Self {
        Self {
            dispatch,
            action_ordinal,
            repository: event.repository().clone(),
            rule_id: rule.id().clone(),
            rule_revision: rule.version(),
            event_id: event.id(),
            trigger_sequence,
            command,
        }
    }

    /// Returns the module-local dispatch reference.
    pub const fn dispatch(&self) -> RepoWatchDispatchId {
        self.dispatch
    }

    /// Returns the one-based position in the rule's ordered action batch.
    pub const fn action_ordinal(&self) -> u64 {
        self.action_ordinal
    }

    /// Returns the repository whose fact matched the rule.
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    /// Returns the checked rule identity.
    pub const fn rule_id(&self) -> &RepoWatchRuleId {
        &self.rule_id
    }

    /// Returns the checked rule revision.
    pub const fn rule_revision(&self) -> RepoWatchRuleVersion {
        self.rule_revision
    }

    /// Returns the triggering normalized GitHub fact.
    pub const fn event_id(&self) -> signalbox_ownership_seam::RepoWatchEventId {
        self.event_id
    }

    /// Returns the lifecycle trigger, absent for a direct rule/event match.
    pub const fn trigger_sequence(&self) -> Option<u64> {
        self.trigger_sequence
    }

    /// Borrows the checked command sent through the seam.
    pub const fn command(&self) -> &SessionCommand {
        &self.command
    }

    /// Consumes the plan and returns its checked command.
    pub fn into_command(self) -> SessionCommand {
        self.command
    }

    fn with_retained_command(
        mut self,
        dispatch: RepoWatchDispatchId,
        command: SessionCommand,
    ) -> Self {
        self.dispatch = dispatch;
        self.command = command;
        self
    }
}

/// Retained origin of the create action that produced one session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedDispatchAction {
    dispatch: RepoWatchDispatchId,
    action_ordinal: NonZeroU64,
    repository: RepositorySlug,
    rule_id: RepoWatchRuleId,
    rule_revision: RepoWatchRuleVersion,
    event_id: signalbox_ownership_seam::RepoWatchEventId,
}

impl RetainedDispatchAction {
    /// Returns the committed dispatch reference.
    pub const fn dispatch(&self) -> RepoWatchDispatchId {
        self.dispatch
    }

    /// Returns the originating action ordinal.
    pub const fn action_ordinal(&self) -> NonZeroU64 {
        self.action_ordinal
    }

    /// Returns the retained repository identity.
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    /// Returns the retained rule identity.
    pub const fn rule_id(&self) -> &RepoWatchRuleId {
        &self.rule_id
    }

    /// Returns the retained rule revision.
    pub const fn rule_revision(&self) -> RepoWatchRuleVersion {
        self.rule_revision
    }

    /// Returns the retained triggering event identity.
    pub const fn event_id(&self) -> signalbox_ownership_seam::RepoWatchEventId {
        self.event_id
    }
}

/// Core-owned factory for the resolved create-session payload.
pub trait CreateSessionCommandFactory {
    /// Infrastructure or template-resolution failure.
    type Error;

    /// Builds a command with module-dispatch provenance for the supplied reference.
    fn create_session(
        &mut self,
        dispatch: RepoWatchDispatchId,
        template: &signalbox_ownership_seam::SessionTemplateName,
        event: &RepoWatchEvent,
    ) -> Result<CreateSession, Self::Error>;
}

/// Core-owned durable encoding for commands retained in the module ledger.
///
/// The module treats the bytes as opaque. Core must encode the complete checked
/// command payload and decode that same representation after restart.
pub trait SessionCommandCodec {
    /// Encodes one complete checked command payload.
    fn encode(&mut self, command: &SessionCommand) -> Option<Vec<u8>>;

    /// Decodes one complete checked command payload.
    fn decode(&mut self, payload: &[u8]) -> Option<SessionCommand>;
}

/// Module-local source of opaque dispatch references.
pub trait DispatchReferenceGenerator {
    /// Returns the next dispatch reference.
    fn next_dispatch(&mut self) -> RepoWatchDispatchId;
}

/// Why a lifecycle reaction did not fit repo-watch's closed command policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleReactionError {
    /// Only session terminal and goal change events drive these reactions.
    UnsupportedTrigger,
    /// The reaction command must target the session named by its trigger.
    MismatchedSession,
    /// Only start release and sticky stop are repo-watch lifecycle reactions.
    UnsupportedCommand,
}

impl fmt::Display for LifecycleReactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedTrigger => {
                "repository-watch lifecycle reaction has no admitted trigger"
            }
            Self::MismatchedSession => {
                "repository-watch lifecycle reaction targets another session"
            }
            Self::UnsupportedCommand => {
                "repository-watch lifecycle reaction has no admitted command"
            }
        })
    }
}

impl Error for LifecycleReactionError {}

/// Why a matching event could not become a checked command batch.
#[derive(Debug)]
pub enum PlanRepositoryEventError<E> {
    /// The core-owned command factory failed.
    Factory(E),
    /// The factory returned creation provenance outside the ownership seam.
    CommandOutsideSeam,
}

impl<E: fmt::Display> fmt::Display for PlanRepositoryEventError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Factory(error) => write!(formatter, "create-session factory failed: {error}"),
            Self::CommandOutsideSeam => {
                formatter.write_str("create-session command is outside the ownership seam")
            }
        }
    }
}

impl<E> Error for PlanRepositoryEventError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Factory(error) => Some(error),
            Self::CommandOutsideSeam => None,
        }
    }
}

/// Result of reconciling one repository's complete configured rule set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuleReconciliationAdmission {
    /// The complete set was admitted atomically.
    Applied {
        /// Per-input outcomes in the supplied order.
        rules: Box<[RuleAdmission]>,
        /// Number of formerly active rules deactivated by this commit.
        deactivated: u64,
    },
    /// A retained revision was already bound to different semantics.
    ConflictingReuse,
    /// At least one supplied revision was older than retained lineage.
    Stale,
}

/// One repository in the complete configured rule set.
#[derive(Clone, Copy, Debug)]
pub struct RepositoryRuleSet<'a> {
    repository: &'a RepositorySlug,
    rules: &'a [RepoWatchRule],
}

impl<'a> RepositoryRuleSet<'a> {
    /// Binds one repository to its complete ordered checked rule set.
    pub const fn new(repository: &'a RepositorySlug, rules: &'a [RepoWatchRule]) -> Self {
        Self { repository, rules }
    }
}

impl WebhookDisposition {
    const fn storage(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Ignored => "ignored",
            Self::Rejected => "rejected",
        }
    }
}

/// Module-local storage failure.
#[derive(Debug)]
pub enum StoreError {
    /// PostgreSQL rejected or could not complete the operation.
    Database(sqlx::Error),
    /// A positive provider identity cannot fit the durable numeric shape.
    InvalidProviderIdentity,
    /// The webhook expiry does not follow its receipt time.
    InvalidWebhookExpiry,
    /// The supplied frontier generation has no successor.
    InvalidFrontierGeneration,
    /// A retained event has an invalid durable evaluation position.
    InvalidEventEvaluationPosition,
    /// The repository-wide event ordinal has no successor.
    InvalidEventOrdinal,
    /// A fact in a frontier commit belongs to another repository.
    EventRepositoryMismatch,
    /// A pull-request projection belongs to another repository.
    ProjectionRepositoryMismatch,
    /// The checked rule exposed too many identity fields for the durable inventory.
    InvalidRuleFieldInventory,
    /// Planned commands do not form one complete ordered rule/event batch.
    InvalidDispatchBatch,
    /// Core could not encode or decode an exact retained command payload.
    InvalidRetainedCommand,
    /// A complete configured set repeated one repository-scoped rule identity.
    DuplicateRuleIdentity,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Database(_) => "repository-watch module database operation failed",
            Self::InvalidProviderIdentity => "repository-watch provider identity is not positive",
            Self::InvalidWebhookExpiry => "repository-watch webhook expiry is not after receipt",
            Self::InvalidFrontierGeneration => {
                "repository-watch frontier generation has no successor"
            }
            Self::InvalidEventEvaluationPosition => {
                "repository-watch event has an invalid evaluation position"
            }
            Self::InvalidEventOrdinal => "repository-watch event ordinal has no successor",
            Self::EventRepositoryMismatch => {
                "repository-watch event does not belong to the frontier repository"
            }
            Self::ProjectionRepositoryMismatch => {
                "repository-watch projection does not belong to the frontier repository"
            }
            Self::InvalidRuleFieldInventory => {
                "repository-watch rule identity-field inventory is too large"
            }
            Self::InvalidDispatchBatch => {
                "repository-watch commands do not form one ordered dispatch batch"
            }
            Self::InvalidRetainedCommand => "repository-watch retained command payload is invalid",
            Self::DuplicateRuleIdentity => {
                "repository-watch configured rule set repeats an identity"
            }
        })
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::InvalidProviderIdentity
            | Self::InvalidWebhookExpiry
            | Self::InvalidFrontierGeneration
            | Self::InvalidEventEvaluationPosition
            | Self::InvalidEventOrdinal
            | Self::EventRepositoryMismatch
            | Self::ProjectionRepositoryMismatch
            | Self::InvalidRuleFieldInventory
            | Self::InvalidDispatchBatch
            | Self::InvalidRetainedCommand
            | Self::DuplicateRuleIdentity => None,
        }
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// SQL implementation over repository-watch's role-confined module pool.
#[derive(Clone, Debug)]
pub struct RepoWatchStore {
    pool: PgPool,
}

impl RepoWatchStore {
    /// Uses a pool already confined to the repository-watch role and schema.
    pub const fn new(module_pool: PgPool) -> Self {
        Self { pool: module_pool }
    }

    /// Atomically admits the delivery metadata, body, and pending disposition.
    pub async fn admit_webhook(
        &self,
        delivery: WebhookDelivery<'_>,
    ) -> Result<WebhookAdmission, StoreError> {
        validate_webhook(&delivery)?;
        let hook_id = Decimal::from(delivery.hook_id);
        let body_digest: [u8; 32] = Sha256::digest(delivery.body).into();
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO webhook_delivery
                (hook_id, delivery_id, repository, event_kind, action,
                 body_digest, received_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT DO NOTHING",
        )
        .bind(hook_id)
        .bind(delivery.delivery_id)
        .bind(delivery.repository.as_str())
        .bind(delivery.event)
        .bind(delivery.action)
        .bind(body_digest.as_slice())
        .bind(delivery.received_at)
        .bind(delivery.expires_at)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if inserted {
            sqlx::query(
                "INSERT INTO webhook_body (hook_id, delivery_id, body)
                 VALUES ($1, $2, $3)",
            )
            .bind(hook_id)
            .bind(delivery.delivery_id)
            .bind(delivery.body)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO webhook_disposition (hook_id, delivery_id, disposition)
                 VALUES ($1, $2, 'pending')",
            )
            .bind(hook_id)
            .bind(delivery.delivery_id)
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            return Ok(WebhookAdmission::Inserted);
        }

        let equal: bool = sqlx::query_scalar(
            "SELECT delivery.repository = $3
                    AND delivery.event_kind = $4
                    AND delivery.action IS NOT DISTINCT FROM $5
                    AND delivery.body_digest = $6
                    AND body.body = $7
               FROM webhook_delivery AS delivery
               JOIN webhook_body AS body USING (hook_id, delivery_id)
              WHERE delivery.hook_id = $1 AND delivery.delivery_id = $2",
        )
        .bind(hook_id)
        .bind(delivery.delivery_id)
        .bind(delivery.repository.as_str())
        .bind(delivery.event)
        .bind(delivery.action)
        .bind(body_digest.as_slice())
        .bind(delivery.body)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.rollback().await?;
        Ok(if equal {
            WebhookAdmission::Replayed
        } else {
            WebhookAdmission::ConflictingReuse
        })
    }

    /// Records one terminal disposition exactly once.
    pub async fn settle_webhook(
        &self,
        hook_id: u64,
        delivery_id: Uuid,
        disposition: WebhookDisposition,
        settled_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        if hook_id == 0 {
            return Err(StoreError::InvalidProviderIdentity);
        }
        let updated = sqlx::query(
            "UPDATE webhook_disposition
                SET disposition = $3, settled_at = $4
              WHERE hook_id = $1 AND delivery_id = $2
                AND disposition = 'pending'",
        )
        .bind(Decimal::from(hook_id))
        .bind(delivery_id)
        .bind(disposition.storage())
        .bind(settled_at)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Advances module-local application from the prior visible core event.
    pub async fn advance_core_event(
        &self,
        prior_visible_sequence: u64,
        sequence: u64,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query(
            "UPDATE core_event_cursor
                SET applied_through = $1, updated_at = statement_timestamp()
              WHERE singleton
                AND applied_through = $2
                AND $1 > $2",
        )
        .bind(Decimal::from(sequence))
        .bind(Decimal::from(prior_visible_sequence))
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Atomically commits a complete frontier candidate and its ordered facts.
    pub async fn commit_frontier_candidate(
        &self,
        projection: &RepositoryProjection<'_>,
        expected_generation: u64,
        frontier: &[RepoWatchEventIdentityFrontierEntryV1],
        events: &[EventCandidate<'_>],
        producer: EventProducer,
        recorded_at: OffsetDateTime,
    ) -> Result<FrontierEventAdmission, StoreError> {
        let repository_state = &projection.repository;
        let pull_request_states = projection.pull_requests.as_slice();
        let comparison_baseline =
            observation_payload(projection.comparison_baseline, projection.merged_baselines)
                .to_string();
        let repository = repository_state.repository;
        if pull_request_states
            .iter()
            .any(|state| state.repository != repository)
        {
            return Err(StoreError::ProjectionRepositoryMismatch);
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        let projections_match = stored_projections_match(
            &mut transaction,
            repository_state,
            pull_request_states,
            &comparison_baseline,
        )
        .await?;
        upsert_repository(&mut transaction, repository_state, &comparison_baseline).await?;
        replace_pull_requests(&mut transaction, repository, pull_request_states).await?;
        let candidate_identity = frontier_candidate_identity(
            repository_state,
            pull_request_states,
            comparison_baseline.as_bytes(),
            frontier,
            events,
        );
        let current_generation: Decimal = sqlx::query_scalar(
            "SELECT frontier_generation FROM repository_state
              WHERE repository = $1 FOR UPDATE",
        )
        .bind(repository.as_str())
        .fetch_one(&mut *transaction)
        .await?;
        let expected = Decimal::from(expected_generation);
        if current_generation != expected {
            let exact_replay: bool = sqlx::query_scalar(
                "SELECT frontier_generation = $2 + 1
                        AND last_frontier_commit_digest = sha256($3)
                   FROM repository_state WHERE repository = $1",
            )
            .bind(repository.as_str())
            .bind(expected)
            .bind(candidate_identity.as_slice())
            .fetch_one(&mut *transaction)
            .await?;
            transaction.rollback().await?;
            return if exact_replay {
                Ok(FrontierEventAdmission::Committed {
                    generation: expected_generation
                        .checked_add(1)
                        .ok_or(StoreError::InvalidFrontierGeneration)?,
                    events: vec![EventAdmission::Replayed; events.len()].into_boxed_slice(),
                })
            } else {
                Ok(FrontierEventAdmission::Stale)
            };
        }
        let stream_identities = frontier
            .iter()
            .map(|entry| entry.stream_identity().to_vec())
            .collect::<Vec<_>>();
        let sequences = frontier
            .iter()
            .map(|entry| Decimal::from(entry.sequence().get()))
            .collect::<Vec<_>>();
        let pull_request_numbers = frontier
            .iter()
            .map(|entry| {
                entry
                    .pull_request_number()
                    .map(|number| Decimal::from(number.get()))
            })
            .collect::<Vec<_>>();
        let (unchanged, omitted, stale): (bool, bool, bool) = sqlx::query_as(
            "WITH candidate AS (
                SELECT *
                  FROM UNNEST($2::bytea[], $3::numeric[], $4::numeric[])
                       AS entry(stream_identity, sequence, pull_request_number)
             ),
             stored AS (
                SELECT stream_identity, sequence, pull_request_number
                  FROM frontier
                 WHERE repository = $1
             )
             SELECT
                NOT EXISTS (
                    (SELECT * FROM stored EXCEPT SELECT * FROM candidate)
                    UNION ALL
                    (SELECT * FROM candidate EXCEPT SELECT * FROM stored)
                ),
                EXISTS (
                    SELECT 1
                      FROM stored
                      LEFT JOIN candidate USING (stream_identity)
                     WHERE candidate.stream_identity IS NULL
                ),
                EXISTS (
                    SELECT 1
                      FROM stored
                      JOIN candidate USING (stream_identity)
                     WHERE stored.sequence > candidate.sequence
                )",
        )
        .bind(repository.as_str())
        .bind(&stream_identities)
        .bind(&sequences)
        .bind(&pull_request_numbers)
        .fetch_one(&mut *transaction)
        .await?;
        if unchanged && projections_match {
            if events.is_empty() {
                transaction.rollback().await?;
                return Ok(FrontierEventAdmission::Unchanged);
            }
            if !events.is_empty() {
                transaction.rollback().await?;
                return Ok(FrontierEventAdmission::ConflictingReuse);
            }
        }
        if omitted || stale {
            transaction.rollback().await?;
            return Ok(FrontierEventAdmission::Stale);
        }
        let next_generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::InvalidFrontierGeneration)?;
        let mut repository_event_ordinal = sqlx::query_scalar::<_, Decimal>(
            "SELECT COALESCE(max(repository_event_ordinal), 0) FROM gh_event
              WHERE repository = $1",
        )
        .bind(repository.as_str())
        .fetch_one(&mut *transaction)
        .await?
        .to_u64()
        .ok_or(StoreError::InvalidEventEvaluationPosition)?;
        let mut admissions = Vec::with_capacity(events.len());
        for (event_ordinal, candidate) in (1_u64..).zip(events) {
            if candidate.event.repository() != repository {
                return Err(StoreError::EventRepositoryMismatch);
            }
            let candidate_repository_ordinal = repository_event_ordinal
                .checked_add(1)
                .ok_or(StoreError::InvalidEventOrdinal)?;
            match append_event(
                &mut transaction,
                candidate,
                producer,
                candidate_repository_ordinal,
                next_generation,
                event_ordinal,
                recorded_at,
            )
            .await?
            {
                Some(admission) => {
                    if admission == EventAdmission::Inserted {
                        repository_event_ordinal = candidate_repository_ordinal;
                    }
                    admissions.push(admission);
                }
                None => {
                    transaction.rollback().await?;
                    return Ok(FrontierEventAdmission::ConflictingReuse);
                }
            }
        }
        sqlx::query(
            "WITH candidate AS (
                SELECT *
                  FROM UNNEST($2::bytea[], $3::numeric[], $4::numeric[])
                       AS entry(stream_identity, sequence, pull_request_number)
             )
             INSERT INTO frontier
                (repository, stream_identity, sequence, pull_request_number, updated_at)
             SELECT $1, stream_identity, sequence, pull_request_number, statement_timestamp()
               FROM candidate
             ON CONFLICT (repository, stream_identity) DO UPDATE
             SET sequence = EXCLUDED.sequence,
                 pull_request_number = EXCLUDED.pull_request_number,
                 updated_at = statement_timestamp()",
        )
        .bind(repository.as_str())
        .bind(&stream_identities)
        .bind(&sequences)
        .bind(&pull_request_numbers)
        .execute(&mut *transaction)
        .await?;
        let advanced = sqlx::query(
            "UPDATE repository_state
                SET frontier_generation = $2,
                    last_frontier_commit_digest = sha256($3),
                    updated_at = statement_timestamp()
              WHERE repository = $1 AND frontier_generation = $4",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(next_generation))
        .bind(candidate_identity.as_slice())
        .bind(expected)
        .execute(&mut *transaction)
        .await?;
        if advanced.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(FrontierEventAdmission::Stale);
        }
        transaction.commit().await?;
        Ok(FrontierEventAdmission::Committed {
            generation: next_generation,
            events: admissions.into_boxed_slice(),
        })
    }

    /// Releases one recurring stream after its rebuild subject has retired.
    pub async fn release_frontier(
        &self,
        repository: &RepositorySlug,
        expected_generation: u64,
        stream_identity: &[u8; 32],
    ) -> Result<FrontierReleaseAdmission, StoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        let current: Option<(Decimal, bool)> = sqlx::query_as(
            "SELECT frontier_generation,
                    COALESCE(last_frontier_commit_digest = sha256($2), false)
               FROM repository_state
              WHERE repository = $1
              FOR UPDATE",
        )
        .bind(repository.as_str())
        .bind(frontier_release_identity(stream_identity))
        .fetch_optional(&mut *transaction)
        .await?;
        let Some((current_generation, same_release)) = current else {
            transaction.rollback().await?;
            return Ok(FrontierReleaseAdmission::Absent);
        };
        let current_generation = current_generation
            .to_u64()
            .ok_or(StoreError::InvalidFrontierGeneration)?;
        if current_generation != expected_generation {
            transaction.rollback().await?;
            return if expected_generation.checked_add(1) == Some(current_generation) && same_release
            {
                Ok(FrontierReleaseAdmission::Replayed {
                    generation: current_generation,
                })
            } else {
                Ok(FrontierReleaseAdmission::Stale)
            };
        }
        let deleted = sqlx::query(
            "DELETE FROM frontier
              WHERE repository = $1 AND stream_identity = $2",
        )
        .bind(repository.as_str())
        .bind(stream_identity.as_slice())
        .execute(&mut *transaction)
        .await?;
        if deleted.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(FrontierReleaseAdmission::Absent);
        }
        let next_generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::InvalidFrontierGeneration)?;
        let advanced = sqlx::query(
            "UPDATE repository_state
                SET frontier_generation = $2,
                    last_frontier_commit_digest = sha256($3),
                    updated_at = statement_timestamp()
              WHERE repository = $1 AND frontier_generation = $4",
        )
        .bind(repository.as_str())
        .bind(Decimal::from(next_generation))
        .bind(frontier_release_identity(stream_identity))
        .bind(Decimal::from(expected_generation))
        .execute(&mut *transaction)
        .await?;
        if advanced.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(StoreError::InvalidFrontierGeneration);
        }
        transaction.commit().await?;
        Ok(FrontierReleaseAdmission::Released {
            generation: next_generation,
        })
    }

    /// Reconciles the complete configured repository and rule set atomically.
    pub async fn reconcile_rules(
        &self,
        repositories: &[RepositoryRuleSet<'_>],
        activated_at: OffsetDateTime,
    ) -> Result<RuleReconciliationAdmission, StoreError> {
        let mut configured = BTreeMap::new();
        for repository in repositories {
            let mut rule_ids = BTreeSet::new();
            for rule in repository.rules {
                if !rule_ids.insert(rule.id().as_str()) {
                    return Err(StoreError::DuplicateRuleIdentity);
                }
            }
            if configured
                .insert(repository.repository.as_str(), rule_ids)
                .is_some()
            {
                return Err(StoreError::DuplicateRuleIdentity);
            }
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(CONFIGURATION_LOCK)
            .execute(&mut *transaction)
            .await?;
        let active_repositories: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT repository FROM rule ORDER BY repository")
                .fetch_all(&mut *transaction)
                .await?;
        let mut locked_repositories = configured.keys().copied().collect::<BTreeSet<_>>();
        locked_repositories.extend(active_repositories.iter().map(String::as_str));
        for repository in locked_repositories {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('frontier:' || $1, 0))")
                .bind(repository)
                .execute(&mut *transaction)
                .await?;
        }
        let active_rules: Vec<(String, String, Decimal)> = sqlx::query_as(
            "SELECT repository, rule_id, active_revision FROM rule
              ORDER BY repository, rule_id FOR UPDATE",
        )
        .fetch_all(&mut *transaction)
        .await?;
        let mut plans = Vec::new();
        for repository in repositories {
            let activation_tail: Decimal = sqlx::query_scalar(
                "SELECT COALESCE(max(repository_event_ordinal), 0)
                   FROM gh_event WHERE repository = $1",
            )
            .bind(repository.repository.as_str())
            .fetch_one(&mut *transaction)
            .await?;
            for rule in repository.rules {
                let revision = Decimal::from(rule.version().get());
                let digest = rule.content_digest();
                let active: Option<(Decimal, Vec<u8>)> = sqlx::query_as(
                    "SELECT active_revision, content_digest
                       FROM rule
                      WHERE repository = $1 AND rule_id = $2 FOR UPDATE",
                )
                .bind(repository.repository.as_str())
                .bind(rule.id().as_str())
                .fetch_optional(&mut *transaction)
                .await?;
                let latest_revision: Option<Decimal> = sqlx::query_scalar(
                    "SELECT max(revision) FROM rule_revision
                      WHERE repository = $1 AND rule_id = $2",
                )
                .bind(repository.repository.as_str())
                .bind(rule.id().as_str())
                .fetch_one(&mut *transaction)
                .await?;
                let historical_digest: Option<Vec<u8>> = sqlx::query_scalar(
                    "SELECT content_digest FROM rule_revision
                      WHERE repository = $1 AND rule_id = $2 AND revision = $3",
                )
                .bind(repository.repository.as_str())
                .bind(rule.id().as_str())
                .bind(revision)
                .fetch_optional(&mut *transaction)
                .await?;
                if historical_digest
                    .as_ref()
                    .is_some_and(|historical| historical != digest.as_bytes())
                    || active
                        .as_ref()
                        .is_some_and(|(active_revision, active_digest)| {
                            *active_revision == revision && active_digest != digest.as_bytes()
                        })
                {
                    transaction.rollback().await?;
                    return Ok(RuleReconciliationAdmission::ConflictingReuse);
                }
                if historical_digest.is_some() {
                    if active
                        .as_ref()
                        .is_some_and(|(active_revision, _)| *active_revision == revision)
                    {
                        plans.push((
                            repository.repository,
                            rule,
                            active.map(|(revision, _)| revision),
                            RuleAdmission::Replayed,
                            activation_tail,
                        ));
                        continue;
                    }
                    transaction.rollback().await?;
                    return Ok(RuleReconciliationAdmission::Stale);
                }
                if active
                    .as_ref()
                    .is_some_and(|(active_revision, _)| revision < *active_revision)
                    || latest_revision.is_some_and(|latest| revision < latest)
                {
                    transaction.rollback().await?;
                    return Ok(RuleReconciliationAdmission::Stale);
                }
                let admission = if active.is_some() || latest_revision.is_some() {
                    RuleAdmission::Updated
                } else {
                    RuleAdmission::Inserted
                };
                plans.push((
                    repository.repository,
                    rule,
                    active.map(|(revision, _)| revision),
                    admission,
                    activation_tail,
                ));
            }
        }
        for (repository, rule, active_revision, admission, activation_tail) in &plans {
            if *admission == RuleAdmission::Replayed {
                continue;
            }
            insert_rule_revision(
                &mut transaction,
                repository,
                rule,
                activated_at,
                *activation_tail,
            )
            .await?;
            if let Some(active_revision) = active_revision {
                retire_rule_revision(
                    &mut transaction,
                    repository.as_str(),
                    rule.id().as_str(),
                    *active_revision,
                    activated_at,
                )
                .await?;
                sqlx::query(
                    "UPDATE rule
                        SET active_revision = $3, content_digest = $4,
                            updated_at = statement_timestamp()
                      WHERE repository = $1 AND rule_id = $2",
                )
                .bind(repository.as_str())
                .bind(rule.id().as_str())
                .bind(Decimal::from(rule.version().get()))
                .bind(rule.content_digest().as_bytes().as_slice())
                .execute(&mut *transaction)
                .await?;
            } else {
                sqlx::query(
                    "INSERT INTO rule
                        (repository, rule_id, active_revision, content_digest, updated_at)
                     VALUES ($1, $2, $3, $4, statement_timestamp())",
                )
                .bind(repository.as_str())
                .bind(rule.id().as_str())
                .bind(Decimal::from(rule.version().get()))
                .bind(rule.content_digest().as_bytes().as_slice())
                .execute(&mut *transaction)
                .await?;
            }
        }
        let mut deactivated = 0_u64;
        for (repository, rule_id, active_revision) in active_rules {
            if configured
                .get(repository.as_str())
                .is_some_and(|rule_ids| rule_ids.contains(rule_id.as_str()))
            {
                continue;
            }
            retire_rule_revision(
                &mut transaction,
                &repository,
                &rule_id,
                active_revision,
                activated_at,
            )
            .await?;
            sqlx::query("DELETE FROM rule WHERE repository = $1 AND rule_id = $2")
                .bind(&repository)
                .bind(&rule_id)
                .execute(&mut *transaction)
                .await?;
            deactivated = deactivated
                .checked_add(1)
                .ok_or(StoreError::InvalidRuleFieldInventory)?;
        }
        transaction.commit().await?;
        Ok(RuleReconciliationAdmission::Applied {
            rules: plans
                .into_iter()
                .map(|(_, _, _, admission, _)| admission)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            deactivated,
        })
    }
}

#[derive(sqlx::FromRow)]
struct StoredPullRequestProjection {
    pull_request_number: Decimal,
    lifecycle: String,
    head_sha: String,
    head_repository: String,
    head_branch: String,
    base_branch: String,
    title: String,
    body: String,
    draft: bool,
    author: Option<String>,
    observed_at: OffsetDateTime,
}

async fn stored_projections_match(
    transaction: &mut Transaction<'_, Postgres>,
    repository_state: &RepositoryState<'_>,
    pull_request_states: &[PullRequestState<'_>],
    comparison_baseline: &str,
) -> Result<bool, StoreError> {
    let repository: Option<(String, String, OffsetDateTime, bool)> = sqlx::query_as(
        "SELECT default_branch, default_head_sha, observed_at,
                comparison_baseline = $2::jsonb
           FROM repository_state
          WHERE repository = $1",
    )
    .bind(repository_state.repository.as_str())
    .bind(comparison_baseline)
    .fetch_optional(&mut **transaction)
    .await?;
    if repository.as_ref().is_none_or(
        |(default_branch, default_head, observed_at, baseline_matches)| {
            default_branch != repository_state.default_branch.as_str()
                || default_head != repository_state.default_head.as_str()
                || !baseline_matches
                || postgres_timestamp_micros(*observed_at)
                    != postgres_timestamp_micros(repository_state.observed_at)
        },
    ) {
        return Ok(false);
    }
    let stored: Vec<StoredPullRequestProjection> = sqlx::query_as(
        "SELECT pull_request_number, lifecycle, head_sha, head_repository,
                head_branch, base_branch, title, body, draft, author, observed_at
           FROM pr_state
          WHERE repository = $1
          ORDER BY pull_request_number",
    )
    .bind(repository_state.repository.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    let mut candidate = pull_request_states.iter().collect::<Vec<_>>();
    candidate.sort_by_key(|state| state.number);
    Ok(stored.len() == candidate.len()
        && stored.iter().zip(candidate).all(|(stored, candidate)| {
            stored.pull_request_number == Decimal::from(candidate.number.get())
                && stored.lifecycle == candidate.lifecycle.storage()
                && stored.head_sha == candidate.head.as_str()
                && stored.head_repository == candidate.head_repository.as_str()
                && stored.head_branch == candidate.head_branch.as_str()
                && stored.base_branch == candidate.base_branch.as_str()
                && stored.title == candidate.title.as_str()
                && stored.body == candidate.body.as_str()
                && stored.draft == candidate.draft
                && stored.author.as_deref() == candidate.author.map(RepoWatchAuthorLogin::as_str)
                && postgres_timestamp_micros(stored.observed_at)
                    == postgres_timestamp_micros(candidate.observed_at)
        }))
}

fn postgres_timestamp_micros(timestamp: OffsetDateTime) -> i128 {
    const POSTGRES_EPOCH_UNIX_NANOS: i128 = 946_684_800_000_000_000;
    (timestamp.unix_timestamp_nanos() - POSTGRES_EPOCH_UNIX_NANOS) / 1_000
}

async fn upsert_repository(
    transaction: &mut Transaction<'_, Postgres>,
    state: &RepositoryState<'_>,
    comparison_baseline: &str,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO repository_state
            (repository, default_branch, default_head_sha, observed_at,
             comparison_baseline, updated_at)
         VALUES ($1, $2, $3, $4, $5::jsonb, statement_timestamp())
         ON CONFLICT (repository) DO UPDATE
         SET default_branch = EXCLUDED.default_branch,
             default_head_sha = EXCLUDED.default_head_sha,
             observed_at = EXCLUDED.observed_at,
             comparison_baseline = EXCLUDED.comparison_baseline,
             updated_at = statement_timestamp()",
    )
    .bind(state.repository.as_str())
    .bind(state.default_branch.as_str())
    .bind(state.default_head.as_str())
    .bind(state.observed_at)
    .bind(comparison_baseline)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn replace_pull_requests(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    states: &[PullRequestState<'_>],
) -> Result<(), StoreError> {
    let retained_numbers = states
        .iter()
        .map(|state| Decimal::from(state.number.get()))
        .collect::<Vec<_>>();
    sqlx::query(
        "DELETE FROM pr_state
          WHERE repository = $1
            AND NOT (pull_request_number = ANY($2::numeric[]))",
    )
    .bind(repository.as_str())
    .bind(&retained_numbers)
    .execute(&mut **transaction)
    .await?;
    for state in states {
        sqlx::query(
            "INSERT INTO pr_state
                (repository, pull_request_number, lifecycle, head_sha,
                 head_repository, head_branch, base_branch, title, body, draft,
                 author, observed_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     statement_timestamp())
             ON CONFLICT (repository, pull_request_number) DO UPDATE
             SET lifecycle = EXCLUDED.lifecycle,
                 head_sha = EXCLUDED.head_sha,
                 head_repository = EXCLUDED.head_repository,
                 head_branch = EXCLUDED.head_branch,
                 base_branch = EXCLUDED.base_branch,
                 title = EXCLUDED.title,
                 body = EXCLUDED.body,
                 draft = EXCLUDED.draft,
                 author = EXCLUDED.author,
                 observed_at = EXCLUDED.observed_at,
                 updated_at = statement_timestamp()",
        )
        .bind(state.repository.as_str())
        .bind(Decimal::from(state.number.get()))
        .bind(state.lifecycle.storage())
        .bind(state.head.as_str())
        .bind(state.head_repository.as_str())
        .bind(state.head_branch.as_str())
        .bind(state.base_branch.as_str())
        .bind(state.title.as_str())
        .bind(state.body.as_str())
        .bind(state.draft)
        .bind(state.author.map(RepoWatchAuthorLogin::as_str))
        .bind(state.observed_at)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

fn frontier_candidate_identity(
    repository_state: &RepositoryState<'_>,
    pull_request_states: &[PullRequestState<'_>],
    comparison_baseline: &[u8],
    frontier: &[RepoWatchEventIdentityFrontierEntryV1],
    events: &[EventCandidate<'_>],
) -> Vec<u8> {
    let mut identity = b"signalbox-repo-watch-frontier-commit-v1".to_vec();
    push_identity_field(
        &mut identity,
        repository_state.repository.as_str().as_bytes(),
    );
    push_identity_field(
        &mut identity,
        repository_state.default_branch.as_str().as_bytes(),
    );
    push_identity_field(
        &mut identity,
        repository_state.default_head.as_str().as_bytes(),
    );
    identity
        .extend_from_slice(&postgres_timestamp_micros(repository_state.observed_at).to_be_bytes());
    push_identity_field(&mut identity, comparison_baseline);
    let mut pull_request_states = pull_request_states.iter().collect::<Vec<_>>();
    pull_request_states.sort_by_key(|state| state.number);
    for state in pull_request_states {
        identity.push(b'P');
        identity.extend_from_slice(&state.number.get().to_be_bytes());
        push_identity_field(&mut identity, state.lifecycle.storage().as_bytes());
        push_identity_field(&mut identity, state.head.as_str().as_bytes());
        push_identity_field(&mut identity, state.head_repository.as_str().as_bytes());
        push_identity_field(&mut identity, state.head_branch.as_str().as_bytes());
        push_identity_field(&mut identity, state.base_branch.as_str().as_bytes());
        push_identity_field(&mut identity, state.title.as_str().as_bytes());
        push_identity_field(&mut identity, state.body.as_str().as_bytes());
        identity.push(u8::from(state.draft));
        match state.author {
            Some(author) => {
                identity.push(1);
                push_identity_field(&mut identity, author.as_str().as_bytes());
            }
            None => identity.push(0),
        }
        identity.extend_from_slice(&postgres_timestamp_micros(state.observed_at).to_be_bytes());
    }
    for entry in frontier {
        identity.push(b'F');
        identity.extend_from_slice(entry.stream_identity().as_slice());
        identity.extend_from_slice(&entry.sequence().get().to_be_bytes());
        match entry.pull_request_number() {
            Some(number) => {
                identity.push(1);
                identity.extend_from_slice(&number.get().to_be_bytes());
            }
            None => identity.push(0),
        }
    }
    identity.push(b'E');
    for candidate in events {
        identity.extend_from_slice(candidate.content_identity.as_bytes());
    }
    identity
}

fn push_identity_field(identity: &mut Vec<u8>, field: &[u8]) {
    identity.extend_from_slice(&(field.len() as u64).to_be_bytes());
    identity.extend_from_slice(field);
}

fn frontier_release_identity(stream_identity: &[u8; 32]) -> Vec<u8> {
    let mut identity = b"signalbox-repo-watch-frontier-release-v1".to_vec();
    identity.extend_from_slice(stream_identity);
    identity
}

async fn append_event(
    transaction: &mut Transaction<'_, Postgres>,
    candidate: &EventCandidate<'_>,
    producer: EventProducer,
    repository_event_ordinal: u64,
    frontier_generation: u64,
    event_ordinal: u64,
    recorded_at: OffsetDateTime,
) -> Result<Option<EventAdmission>, StoreError> {
    let event = candidate.event;
    let (target_kind, pull_request_number) = match event.target() {
        RepoWatchEventTarget::PullRequest(context) => {
            ("pull_request", Some(Decimal::from(context.number().get())))
        }
        RepoWatchEventTarget::Branch => ("branch", None),
    };
    let payload = canonical_json(normalized_event_payload(event))
        .to_string()
        .into_bytes();
    let inserted = sqlx::query(
        "INSERT INTO gh_event
            (event_id, content_identity, repository, event_kind, target_kind,
             pull_request_number, normalized_payload, producer,
             repository_event_ordinal, frontier_generation, event_ordinal, recorded_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         ON CONFLICT DO NOTHING",
    )
    .bind(event.id().into_uuid())
    .bind(candidate.content_identity.as_bytes().as_slice())
    .bind(event.repository().as_str())
    .bind(event_kind_storage(event.kind().name()))
    .bind(target_kind)
    .bind(pull_request_number)
    .bind(payload.as_slice())
    .bind(producer.storage())
    .bind(Decimal::from(repository_event_ordinal))
    .bind(Decimal::from(frontier_generation))
    .bind(Decimal::from(event_ordinal))
    .bind(recorded_at)
    .execute(&mut **transaction)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        return Ok(Some(EventAdmission::Inserted));
    }
    let exact: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM gh_event
             WHERE content_identity = $2 AND normalized_payload = $3)
            AND NOT EXISTS (
                SELECT 1 FROM gh_event
                 WHERE event_id = $1
                   AND (content_identity <> $2 OR normalized_payload <> $3))",
    )
    .bind(event.id().into_uuid())
    .bind(candidate.content_identity.as_bytes().as_slice())
    .bind(payload.as_slice())
    .fetch_one(&mut **transaction)
    .await?;
    Ok(exact.then_some(EventAdmission::Replayed))
}

async fn insert_rule_revision(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    rule: &RepoWatchRule,
    activated_at: OffsetDateTime,
    activated_after_event_ordinal: Decimal,
) -> Result<(), StoreError> {
    let revision = Decimal::from(rule.version().get());
    let digest = rule.content_digest();
    sqlx::query(
        "INSERT INTO rule_revision
            (repository, rule_id, revision, content_digest, activated_at,
             activated_after_event_ordinal)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(repository.as_str())
    .bind(rule.id().as_str())
    .bind(revision)
    .bind(digest.as_bytes().as_slice())
    .bind(activated_at)
    .bind(activated_after_event_ordinal)
    .execute(&mut **transaction)
    .await?;
    for (index, (field, field_digest)) in rule.identity_field_digests().into_iter().enumerate() {
        let ordinal = i16::try_from(index).map_err(|_| StoreError::InvalidRuleFieldInventory)?;
        sqlx::query(
            "INSERT INTO rule_field_fingerprint
                (repository, rule_id, revision, field_ordinal, field_name, field_digest)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(repository.as_str())
        .bind(rule.id().as_str())
        .bind(revision)
        .bind(ordinal)
        .bind(field.configuration_path())
        .bind(field_digest.as_bytes().as_slice())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn retire_rule_revision(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &str,
    rule_id: &str,
    revision: Decimal,
    retired_at: OffsetDateTime,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE rule_revision
            SET retired_at = GREATEST(activated_at, $4)
          WHERE repository = $1 AND rule_id = $2 AND revision = $3",
    )
    .bind(repository)
    .bind(rule_id)
    .bind(revision)
    .bind(retired_at)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn canonical_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonical_json(value)))
                    .collect(),
            )
        }
        scalar => scalar,
    }
}

fn normalized_event_payload(event: &RepoWatchEvent) -> Value {
    let target = match event.target() {
        RepoWatchEventTarget::PullRequest(context) => json!({
            "kind": "pull_request",
            "number": context.number().get(),
            "head_sha": context.head_sha().as_str(),
            "head_repository": context.head_repository().as_str(),
            "base_branch": context.base_branch().as_str(),
            "head_branch": context.head_branch().as_str(),
            "title": context.title().as_str(),
            "body": context.body().as_str(),
            "labels": context.labels().iter().map(|label| label.as_str()).collect::<Vec<_>>(),
            "draft": context.draft(),
            "author": context.author().map(RepoWatchAuthorLogin::as_str),
        }),
        RepoWatchEventTarget::Branch => json!({ "kind": "branch" }),
    };
    let kind = match event.kind() {
        RepoWatchEventKindV1::PullRequestOpened => json!({ "name": "pull_request_opened" }),
        RepoWatchEventKindV1::PullRequestClosed => json!({ "name": "pull_request_closed" }),
        RepoWatchEventKindV1::PullRequestMerged => json!({ "name": "pull_request_merged" }),
        RepoWatchEventKindV1::HeadChanged { previous, current } => json!({
            "name": "head_changed",
            "previous": previous.as_str(),
            "current": current.as_str(),
        }),
        RepoWatchEventKindV1::MergeableStateChanged { current } => json!({
            "name": "mergeable_state_changed",
            "current": mergeable_state_storage(*current),
        }),
        RepoWatchEventKindV1::ChecksCompleted { outcome } => json!({
            "name": "checks_completed",
            "outcome": checks_outcome_storage(*outcome),
        }),
        RepoWatchEventKindV1::CheckRunCompleted { name, conclusion } => json!({
            "name": "check_run_completed",
            "check_run": name.as_str(),
            "conclusion": check_conclusion_storage(*conclusion),
        }),
        RepoWatchEventKindV1::BranchWorkflowRunCompleted {
            branch,
            workflow,
            conclusion,
        } => json!({
            "name": "branch_workflow_run_completed",
            "branch": branch.as_str(),
            "workflow": workflow.as_str(),
            "conclusion": check_conclusion_storage(*conclusion),
        }),
        RepoWatchEventKindV1::ReviewSubmitted {
            reviewer,
            state,
            commit,
        } => json!({
            "name": "review_submitted",
            "reviewer": reviewer.as_str(),
            "state": review_state_storage(*state),
            "commit": commit.as_str(),
        }),
        RepoWatchEventKindV1::ThreadOpened { thread } => json!({
            "name": "thread_opened",
            "thread": thread.as_str(),
        }),
        RepoWatchEventKindV1::ThreadResolved { thread } => json!({
            "name": "thread_resolved",
            "thread": thread.as_str(),
        }),
        RepoWatchEventKindV1::Labeled { label } => json!({
            "name": "labeled",
            "label": label.as_str(),
        }),
        RepoWatchEventKindV1::Unlabeled { label } => json!({
            "name": "unlabeled",
            "label": label.as_str(),
        }),
        RepoWatchEventKindV1::BaseAdvanced { branch } => json!({
            "name": "base_advanced",
            "branch": branch.as_str(),
        }),
        RepoWatchEventKindV1::ReactionChanged {
            subject,
            reactor,
            content,
            change,
        } => json!({
            "name": "reaction_changed",
            "subject": reaction_subject_payload(*subject),
            "reactor": reactor.as_str(),
            "content": content.as_str(),
            "change": reaction_change_storage(*change),
        }),
    };
    json!({
        "repository": event.repository().as_str(),
        "target": target,
        "kind": kind,
    })
}

const fn mergeable_state_storage(value: MergeableState) -> &'static str {
    match value {
        MergeableState::Mergeable => "mergeable",
        MergeableState::Conflicting => "conflicting",
        MergeableState::Unknown => "unknown",
    }
}

const fn checks_outcome_storage(value: ChecksOutcome) -> &'static str {
    match value {
        ChecksOutcome::Success => "success",
        ChecksOutcome::Failure => "failure",
    }
}

const fn check_conclusion_storage(value: CheckConclusion) -> &'static str {
    match value {
        CheckConclusion::Success => "success",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::TimedOut => "timed_out",
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Stale => "stale",
        CheckConclusion::StartupFailure => "startup_failure",
    }
}

const fn review_state_storage(value: ReviewState) -> &'static str {
    match value {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
        ReviewState::Commented => "commented",
    }
}

fn reaction_subject_payload(value: ReactionSubject) -> Value {
    match value {
        ReactionSubject::PullRequestBody => json!({ "kind": "pull_request_body" }),
        ReactionSubject::IssueComment { id } => {
            json!({ "kind": "issue_comment", "id": id.get() })
        }
        ReactionSubject::ReviewComment { id } => {
            json!({ "kind": "review_comment", "id": id.get() })
        }
    }
}

const fn reaction_change_storage(value: ReactionChange) -> &'static str {
    match value {
        ReactionChange::Added => "added",
        ReactionChange::Removed => "removed",
    }
}

impl RepoWatchStore {
    /// Atomically records one complete emitted action batch before submission.
    pub async fn record_commands<Codec: SessionCommandCodec>(
        &self,
        planned: &[PlannedCommand],
        issued_at: OffsetDateTime,
        codec: &mut Codec,
    ) -> Result<DispatchAdmission, StoreError> {
        let Some(first) = planned.first() else {
            return Err(StoreError::InvalidDispatchBatch);
        };
        let initial_batch = first.trigger_sequence().is_none();
        if planned.iter().any(|command| command.action_ordinal() == 0)
            || planned
                .windows(2)
                .any(|commands| commands[0].action_ordinal() >= commands[1].action_ordinal())
            || planned.iter().enumerate().any(|(index, command)| {
                command.dispatch() != first.dispatch()
                    || command.repository() != first.repository()
                    || command.rule_id() != first.rule_id()
                    || command.rule_revision() != first.rule_revision()
                    || command.event_id() != first.event_id()
                    || command.trigger_sequence() != first.trigger_sequence()
                    || (initial_batch
                        && command.action_ordinal()
                            != u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1))
            })
        {
            return Err(StoreError::InvalidDispatchBatch);
        }
        let encoded_commands = planned
            .iter()
            .map(|command| {
                codec
                    .encode(command.command())
                    .ok_or(StoreError::InvalidRetainedCommand)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(
                hashtextextended(length($1)::text || ':' || $1 || $2, 0))",
        )
        .bind(first.repository().as_str())
        .bind(first.rule_id().as_str())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(
                hashtextextended('dispatch:' || $1::text, 0))",
        )
        .bind(first.dispatch().into_uuid())
        .execute(&mut *transaction)
        .await?;
        let retained_actions: Vec<(Uuid, Decimal, Uuid, String, Vec<u8>)> = sqlx::query_as(
            "SELECT dispatch_ref, action_ordinal, command_id, command_kind, command_payload
               FROM dispatch_ledger
              WHERE repository = $1 AND rule_id = $2 AND rule_revision = $3
                AND event_id = $4 AND trigger_sequence IS NOT DISTINCT FROM $5
              ORDER BY action_ordinal",
        )
        .bind(first.repository().as_str())
        .bind(first.rule_id().as_str())
        .bind(Decimal::from(first.rule_revision().get()))
        .bind(first.event_id().into_uuid())
        .bind(first.trigger_sequence().map(Decimal::from))
        .fetch_all(&mut *transaction)
        .await?;
        if retained_actions.len() == planned.len() {
            let mut commands = Vec::with_capacity(planned.len());
            for (planned, (dispatch, ordinal, command_id, kind, payload)) in
                planned.iter().cloned().zip(retained_actions)
            {
                if ordinal != Decimal::from(planned.action_ordinal())
                    || kind != planned.command().kind().durable_spelling()
                {
                    transaction.rollback().await?;
                    return Ok(DispatchAdmission::ConflictingReuse);
                }
                let command = codec
                    .decode(&payload)
                    .ok_or(StoreError::InvalidRetainedCommand)?;
                if command.command_id().into_uuid() != command_id
                    || command.kind() != planned.command().kind()
                {
                    transaction.rollback().await?;
                    return Err(StoreError::InvalidRetainedCommand);
                }
                commands.push(
                    planned
                        .with_retained_command(RepoWatchDispatchId::from_uuid(dispatch), command),
                );
            }
            transaction.rollback().await?;
            return Ok(DispatchAdmission::Replayed {
                commands: commands.into_boxed_slice(),
            });
        }
        if !retained_actions.is_empty() {
            transaction.rollback().await?;
            return Ok(DispatchAdmission::ConflictingReuse);
        }
        let conflicting_dispatch: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM dispatch_ledger WHERE dispatch_ref = $1
                  AND (repository IS DISTINCT FROM $2
                       OR rule_id IS DISTINCT FROM $3
                       OR rule_revision IS DISTINCT FROM $4
                       OR event_id IS DISTINCT FROM $5))",
        )
        .bind(first.dispatch().into_uuid())
        .bind(first.repository().as_str())
        .bind(first.rule_id().as_str())
        .bind(Decimal::from(first.rule_revision().get()))
        .bind(first.event_id().into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if conflicting_dispatch {
            transaction.rollback().await?;
            return Ok(DispatchAdmission::ConflictingReuse);
        }
        if first.trigger_sequence().is_some() {
            let committed_origin_ordinals: Vec<Decimal> = sqlx::query_scalar(
                "SELECT action_ordinal FROM dispatch_ledger
                  WHERE dispatch_ref = $1 AND repository = $2 AND rule_id = $3
                    AND rule_revision = $4 AND event_id = $5
                    AND trigger_sequence IS NULL",
            )
            .bind(first.dispatch().into_uuid())
            .bind(first.repository().as_str())
            .bind(first.rule_id().as_str())
            .bind(Decimal::from(first.rule_revision().get()))
            .bind(first.event_id().into_uuid())
            .fetch_all(&mut *transaction)
            .await?;
            if planned.iter().any(|command| {
                !committed_origin_ordinals.contains(&Decimal::from(command.action_ordinal()))
            }) {
                transaction.rollback().await?;
                return Ok(DispatchAdmission::ConflictingReuse);
            }
        } else {
            let active_revision: Option<Decimal> = sqlx::query_scalar(
                "SELECT active.active_revision
                   FROM rule AS active
                   JOIN rule_revision AS revision
                     ON revision.repository = active.repository
                    AND revision.rule_id = active.rule_id
                    AND revision.revision = active.active_revision
                   JOIN gh_event AS event
                     ON event.event_id = $3
                  WHERE active.repository = $1 AND active.rule_id = $2
                    AND event.repository_event_ordinal
                        > revision.activated_after_event_ordinal
                  FOR UPDATE OF active",
            )
            .bind(first.repository().as_str())
            .bind(first.rule_id().as_str())
            .bind(first.event_id().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
            if active_revision != Some(Decimal::from(first.rule_revision().get())) {
                transaction.rollback().await?;
                return Ok(DispatchAdmission::InactiveRule);
            }
        }
        let mut inserted_count = 0_usize;
        for (command, payload) in planned.iter().zip(encoded_commands) {
            inserted_count += usize::from(
                sqlx::query(
                    "INSERT INTO dispatch_ledger
                        (dispatch_ref, action_ordinal, command_id, repository, rule_id,
                         rule_revision, event_id, trigger_sequence, command_kind, command_payload,
                         status, issued_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'pending', $11)
                     ON CONFLICT DO NOTHING",
                )
                .bind(command.dispatch().into_uuid())
                .bind(Decimal::from(command.action_ordinal()))
                .bind(command.command().command_id().into_uuid())
                .bind(command.repository().as_str())
                .bind(command.rule_id().as_str())
                .bind(Decimal::from(command.rule_revision().get()))
                .bind(command.event_id().into_uuid())
                .bind(command.trigger_sequence().map(Decimal::from))
                .bind(command.command().kind().durable_spelling())
                .bind(payload)
                .bind(issued_at)
                .execute(&mut *transaction)
                .await?
                .rows_affected()
                    == 1,
            );
        }
        if inserted_count == planned.len() {
            transaction.commit().await?;
            return Ok(DispatchAdmission::Inserted);
        }
        transaction.rollback().await?;
        Ok(DispatchAdmission::ConflictingReuse)
    }

    /// Rebuilds every pending command directly from the committed ledger.
    ///
    /// This path needs no active or configured rule: durable provenance and
    /// the opaque core encoding are sufficient after a restart.
    pub async fn recover_pending_commands<Codec: SessionCommandCodec>(
        &self,
        codec: &mut Codec,
    ) -> Result<Box<[PlannedCommand]>, StoreError> {
        type PendingCommandRow = (
            Uuid,
            Decimal,
            Uuid,
            String,
            String,
            Decimal,
            Uuid,
            Option<Decimal>,
            String,
            Vec<u8>,
        );

        let rows: Vec<PendingCommandRow> = sqlx::query_as(
            "SELECT dispatch_ref, action_ordinal, command_id, repository, rule_id,
                    rule_revision, event_id, trigger_sequence, command_kind, command_payload
               FROM dispatch_ledger
              WHERE status = 'pending'
              ORDER BY issued_at, dispatch_ref, trigger_sequence NULLS FIRST, action_ordinal",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(
                    dispatch,
                    ordinal,
                    command_id,
                    repository,
                    rule_id,
                    rule_revision,
                    event_id,
                    trigger_sequence,
                    kind,
                    payload,
                )| {
                    let command = codec
                        .decode(&payload)
                        .ok_or(StoreError::InvalidRetainedCommand)?;
                    if command.command_id().into_uuid() != command_id
                        || command.kind().durable_spelling() != kind
                    {
                        return Err(StoreError::InvalidRetainedCommand);
                    }
                    let action_ordinal = ordinal
                        .to_u64()
                        .filter(|ordinal| *ordinal > 0)
                        .ok_or(StoreError::InvalidRetainedCommand)?;
                    let rule_revision = rule_revision
                        .to_u64()
                        .and_then(NonZeroU64::new)
                        .and_then(RepoWatchRuleVersion::new)
                        .ok_or(StoreError::InvalidRetainedCommand)?;
                    let trigger_sequence = match trigger_sequence {
                        None => None,
                        Some(sequence) => Some(
                            sequence
                                .to_u64()
                                .filter(|sequence| *sequence > 0)
                                .ok_or(StoreError::InvalidRetainedCommand)?,
                        ),
                    };
                    Ok(PlannedCommand {
                        dispatch: RepoWatchDispatchId::from_uuid(dispatch),
                        action_ordinal,
                        repository: RepositorySlug::try_new(repository)
                            .map_err(|_| StoreError::InvalidRetainedCommand)?,
                        rule_id: RepoWatchRuleId::try_new(rule_id)
                            .map_err(|_| StoreError::InvalidRetainedCommand)?,
                        rule_revision,
                        event_id: signalbox_ownership_seam::RepoWatchEventId::from_uuid(event_id),
                        trigger_sequence,
                        command,
                    })
                },
            )
            .collect::<Result<Vec<_>, _>>()
            .map(Vec::into_boxed_slice)
    }

    /// Finds the retained create action that produced one session.
    ///
    /// The lookup joins the retained rule revision and event, so it remains
    /// sufficient for lifecycle reaction planning after active configuration
    /// is removed and after a daemon restart.
    pub async fn reaction_origin_for_session(
        &self,
        session: SessionId,
    ) -> Result<Option<RetainedDispatchAction>, StoreError> {
        type OriginRow = (Uuid, Decimal, String, String, Decimal, Uuid);
        let rows: Vec<OriginRow> = sqlx::query_as(
            "SELECT ledger.dispatch_ref, ledger.action_ordinal, ledger.repository,
                    ledger.rule_id, ledger.rule_revision, ledger.event_id
               FROM dispatch_ledger AS ledger
               JOIN rule_revision AS retained_rule
                 ON retained_rule.repository = ledger.repository
                AND retained_rule.rule_id = ledger.rule_id
                AND retained_rule.revision = ledger.rule_revision
               JOIN gh_event AS retained_event
                 ON retained_event.event_id = ledger.event_id
                AND retained_event.repository = ledger.repository
              WHERE ledger.created_session_id = $1
                AND ledger.trigger_sequence IS NULL
              ORDER BY ledger.dispatch_ref, ledger.action_ordinal
              LIMIT 2",
        )
        .bind(session.into_uuid())
        .fetch_all(&self.pool)
        .await?;
        let [row] = rows.as_slice() else {
            return if rows.is_empty() {
                Ok(None)
            } else {
                Err(StoreError::InvalidRetainedCommand)
            };
        };
        let (dispatch, ordinal, repository, rule_id, rule_revision, event_id) = row;
        let action_ordinal = ordinal
            .to_u64()
            .and_then(NonZeroU64::new)
            .ok_or(StoreError::InvalidRetainedCommand)?;
        let rule_revision = rule_revision
            .to_u64()
            .and_then(NonZeroU64::new)
            .and_then(RepoWatchRuleVersion::new)
            .ok_or(StoreError::InvalidRetainedCommand)?;
        Ok(Some(RetainedDispatchAction {
            dispatch: RepoWatchDispatchId::from_uuid(*dispatch),
            action_ordinal,
            repository: RepositorySlug::try_new(repository.clone())
                .map_err(|_| StoreError::InvalidRetainedCommand)?,
            rule_id: RepoWatchRuleId::try_new(rule_id.clone())
                .map_err(|_| StoreError::InvalidRetainedCommand)?,
            rule_revision,
            event_id: signalbox_ownership_seam::RepoWatchEventId::from_uuid(*event_id),
        }))
    }

    /// Applies one lifecycle event to the module command ledger.
    pub async fn apply_lifecycle_event(&self, event: &LifecycleEvent) -> Result<bool, StoreError> {
        if let LifecycleEventKind::SessionCreated(created) = event.kind() {
            let (
                Some(session),
                SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::RepositoryWatch { dispatch },
                },
            ) = (event.session(), created.cause)
            else {
                return Ok(false);
            };
            return self
                .settle_created_session(dispatch, session, event.recorded_at())
                .await;
        }
        let LifecycleEventKind::CommandSettled { command, result } = event.kind() else {
            return Ok(false);
        };
        let (status, rejection_kind) = match result {
            CommandSettlement::Applied => ("applied", None),
            CommandSettlement::Rejected { kind } => ("rejected", Some(kind.as_str())),
        };
        let updated = sqlx::query(
            "UPDATE dispatch_ledger
                SET status = $2, rejection_kind = $3, settled_at = $4,
                    created_session_id = CASE
                        WHEN command_kind = 'create_session' AND $2 = 'applied' THEN $5
                        ELSE NULL
                    END
              WHERE command_id = $1 AND status = 'pending'",
        )
        .bind(command.into_uuid())
        .bind(status)
        .bind(rejection_kind)
        .bind(event.recorded_at())
        .bind(
            event
                .session()
                .map(signalbox_ownership_seam::SessionId::into_uuid),
        )
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Records a synchronous terminal result from create-session submission.
    ///
    /// Applied creation remains pending until its `SessionCreated` event links
    /// the created session. A conflicting command identity produces no core
    /// event, so the submitter must await this update before advancing to the
    /// next ordered action.
    pub async fn apply_create_session_outcome(
        &self,
        outcome: &CreateSessionOutcome,
        settled_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let CreateSessionOutcome::ConflictingReuse { command_id } = outcome else {
            return Ok(false);
        };
        let updated = sqlx::query(
            "UPDATE dispatch_ledger
                SET status = 'rejected', rejection_kind = 'conflicting_reuse', settled_at = $2
              WHERE command_id = $1 AND command_kind = 'create_session' AND status = 'pending'",
        )
        .bind(command_id.into_uuid())
        .bind(settled_at)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    async fn settle_created_session(
        &self,
        dispatch: RepoWatchDispatchId,
        session: SessionId,
        settled_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query(
            "WITH candidate AS (
                SELECT command_id
                  FROM dispatch_ledger
                 WHERE dispatch_ref = $1 AND command_kind = 'create_session'
                   AND (status = 'pending' OR created_session_id = $2)
                 ORDER BY (created_session_id = $2) DESC NULLS LAST, action_ordinal
                 LIMIT 1
             )
             UPDATE dispatch_ledger
                SET status = 'applied', settled_at = $3, created_session_id = $2
              WHERE command_id = (SELECT command_id FROM candidate)
                AND status = 'pending'",
        )
        .bind(dispatch.into_uuid())
        .bind(session.into_uuid())
        .bind(settled_at)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }
}

/// Reduces one normalized GitHub fact into held create-session commands.
pub fn plan_repository_event<Ids, Factory>(
    rules: &[RepoWatchRule],
    event: &RepoWatchEvent,
    ids: &mut Ids,
    factory: &mut Factory,
) -> Result<Vec<Vec<PlannedCommand>>, PlanRepositoryEventError<Factory::Error>>
where
    Ids: DispatchReferenceGenerator,
    Factory: CreateSessionCommandFactory,
{
    let mut batches = Vec::new();
    for rule in matching_rules(rules, event) {
        let dispatch = ids.next_dispatch();
        let mut commands = Vec::with_capacity(rule.actions().len());
        for (index, action) in rule.actions().iter().enumerate() {
            let RepoWatchRuleActionV1::DispatchSession { template } = action;
            let command = factory
                .create_session(dispatch, template, event)
                .map_err(PlanRepositoryEventError::Factory)?
                .with_lifecycle(
                    StartGate::Held,
                    SessionOwnership::Owned,
                    Some(FinishCondition::ExternalGate),
                );
            commands.push(PlannedCommand::new(
                dispatch,
                u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                rule,
                event,
                None,
                SessionCommand::create_session(command).map_err(|_: CommandOutsideSeam| {
                    PlanRepositoryEventError::CommandOutsideSeam
                })?,
            ));
        }
        batches.push(commands);
    }
    Ok(batches)
}

/// Admits a start release or sticky stop driven by a lifecycle event.
pub fn plan_lifecycle_reaction(
    trigger: &LifecycleEvent,
    dispatch: RepoWatchDispatchId,
    rule: &RepoWatchRule,
    event: &RepoWatchEvent,
    action_ordinal: NonZeroU64,
    command: SessionLifecycleCommand,
) -> Result<PlannedCommand, LifecycleReactionError> {
    if !matches!(
        trigger.kind(),
        LifecycleEventKind::SessionTerminal(_) | LifecycleEventKind::GoalChanged(_)
    ) {
        return Err(LifecycleReactionError::UnsupportedTrigger);
    }
    plan_lifecycle_reaction_at_sequence(
        trigger.sequence(),
        trigger.session(),
        dispatch,
        rule,
        event,
        action_ordinal,
        command,
    )
}

fn plan_lifecycle_reaction_at_sequence(
    trigger_sequence: u64,
    trigger_session: Option<SessionId>,
    dispatch: RepoWatchDispatchId,
    rule: &RepoWatchRule,
    event: &RepoWatchEvent,
    action_ordinal: NonZeroU64,
    command: SessionLifecycleCommand,
) -> Result<PlannedCommand, LifecycleReactionError> {
    if trigger_session != Some(command.session()) {
        return Err(LifecycleReactionError::MismatchedSession);
    }
    let admitted = matches!(command.operation(), SessionLifecycleOperation::ReleaseStart)
        || matches!(
            command.operation(),
            SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                ..
            }
        );
    if !admitted {
        return Err(LifecycleReactionError::UnsupportedCommand);
    }
    let command = SessionCommand::lifecycle(command)
        .map_err(|_| LifecycleReactionError::UnsupportedCommand)?;
    Ok(PlannedCommand::new(
        dispatch,
        action_ordinal.get(),
        rule,
        event,
        Some(trigger_sequence),
        command,
    ))
}

/// Admits a lifecycle reaction using the durable origin of its created session.
pub fn plan_retained_lifecycle_reaction(
    trigger: &LifecycleEvent,
    origin: &RetainedDispatchAction,
    command: SessionLifecycleCommand,
) -> Result<PlannedCommand, LifecycleReactionError> {
    if !matches!(
        trigger.kind(),
        LifecycleEventKind::SessionTerminal(_) | LifecycleEventKind::GoalChanged(_)
    ) {
        return Err(LifecycleReactionError::UnsupportedTrigger);
    }
    plan_retained_lifecycle_reaction_at_sequence(
        trigger.sequence(),
        trigger.session(),
        origin,
        command,
    )
}

fn plan_retained_lifecycle_reaction_at_sequence(
    trigger_sequence: u64,
    trigger_session: Option<SessionId>,
    origin: &RetainedDispatchAction,
    command: SessionLifecycleCommand,
) -> Result<PlannedCommand, LifecycleReactionError> {
    if trigger_session != Some(command.session()) {
        return Err(LifecycleReactionError::MismatchedSession);
    }
    let admitted = matches!(command.operation(), SessionLifecycleOperation::ReleaseStart)
        || matches!(
            command.operation(),
            SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                ..
            }
        );
    if !admitted {
        return Err(LifecycleReactionError::UnsupportedCommand);
    }
    let command = SessionCommand::lifecycle(command)
        .map_err(|_| LifecycleReactionError::UnsupportedCommand)?;
    Ok(PlannedCommand {
        dispatch: origin.dispatch,
        action_ordinal: origin.action_ordinal.get(),
        repository: origin.repository.clone(),
        rule_id: origin.rule_id.clone(),
        rule_revision: origin.rule_revision,
        event_id: origin.event_id,
        trigger_sequence: Some(trigger_sequence),
        command,
    })
}

/// Builds a lifecycle reaction from an explicit positive trigger sequence.
///
/// This constructor exists only for persistence-boundary integration tests;
/// production callers must supply a typed [`LifecycleEvent`].
#[cfg(feature = "test-support")]
pub fn plan_lifecycle_reaction_for_test(
    trigger_sequence: NonZeroU64,
    trigger_session: SessionId,
    dispatch: RepoWatchDispatchId,
    rule: &RepoWatchRule,
    event: &RepoWatchEvent,
    action_ordinal: NonZeroU64,
    command: SessionLifecycleCommand,
) -> Result<PlannedCommand, LifecycleReactionError> {
    plan_lifecycle_reaction_at_sequence(
        trigger_sequence.get(),
        Some(trigger_session),
        dispatch,
        rule,
        event,
        action_ordinal,
        command,
    )
}

/// Builds a retained-origin reaction from an explicit positive trigger sequence.
#[cfg(feature = "test-support")]
pub fn plan_retained_lifecycle_reaction_for_test(
    trigger_sequence: NonZeroU64,
    trigger_session: SessionId,
    origin: &RetainedDispatchAction,
    command: SessionLifecycleCommand,
) -> Result<PlannedCommand, LifecycleReactionError> {
    plan_retained_lifecycle_reaction_at_sequence(
        trigger_sequence.get(),
        Some(trigger_session),
        origin,
        command,
    )
}

/// Returns configured rules whose checked matcher accepts one normalized fact.
pub fn matching_rules<'a>(
    rules: &'a [RepoWatchRule],
    event: &RepoWatchEvent,
) -> Vec<&'a RepoWatchRule> {
    rules
        .iter()
        .filter(|rule| rule.matcher().matches(event))
        .collect()
}

const fn event_kind_storage(kind: RepoWatchEventKindNameV1) -> &'static str {
    match kind {
        RepoWatchEventKindNameV1::PullRequestOpened => "pull_request_opened",
        RepoWatchEventKindNameV1::PullRequestClosed => "pull_request_closed",
        RepoWatchEventKindNameV1::PullRequestMerged => "pull_request_merged",
        RepoWatchEventKindNameV1::HeadChanged => "head_changed",
        RepoWatchEventKindNameV1::MergeableStateChanged => "mergeable_state_changed",
        RepoWatchEventKindNameV1::ChecksCompleted => "checks_completed",
        RepoWatchEventKindNameV1::CheckRunCompleted => "check_run_completed",
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => "branch_workflow_run_completed",
        RepoWatchEventKindNameV1::ReviewSubmitted => "review_submitted",
        RepoWatchEventKindNameV1::ThreadOpened => "thread_opened",
        RepoWatchEventKindNameV1::ThreadResolved => "thread_resolved",
        RepoWatchEventKindNameV1::Labeled => "labeled",
        RepoWatchEventKindNameV1::Unlabeled => "unlabeled",
        RepoWatchEventKindNameV1::BaseAdvanced => "base_advanced",
        RepoWatchEventKindNameV1::ReactionChanged => "reaction_changed",
    }
}

fn validate_webhook(delivery: &WebhookDelivery<'_>) -> Result<(), StoreError> {
    if delivery.hook_id == 0 {
        Err(StoreError::InvalidProviderIdentity)
    } else if delivery.expires_at <= delivery.received_at {
        Err(StoreError::InvalidWebhookExpiry)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{StoreError, WebhookDelivery, canonical_json, validate_webhook};
    use serde_json::{Map, Value};
    use signalbox_ownership_seam::{OffsetDateTime, RepositorySlug};
    use uuid::Uuid;

    #[test]
    fn webhook_expiry_validation_names_the_only_module_owned_ttl_boundary() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let repository = RepositorySlug::try_new(String::from("owner/repository"))
            .expect("fixture repository is valid");
        let input = WebhookDelivery {
            repository: &repository,
            hook_id: 1,
            delivery_id: Uuid::from_u128(2),
            event: "pull_request",
            action: Some("opened"),
            body: b"{}",
            received_at: now,
            expires_at: now,
        };
        assert!(matches!(
            validate_webhook(&input),
            Err(StoreError::InvalidWebhookExpiry)
        ));
    }

    #[test]
    fn normalized_payload_object_keys_are_canonical_at_every_depth() {
        let mut nested = Map::new();
        nested.insert(String::from("z"), Value::from(1));
        nested.insert(String::from("a"), Value::from(2));
        let mut payload = Map::new();
        payload.insert(String::from("target"), Value::Object(nested));
        payload.insert(String::from("repository"), Value::from("owner/repository"));

        assert_eq!(
            canonical_json(Value::Object(payload)).to_string(),
            r#"{"repository":"owner/repository","target":{"a":2,"z":1}}"#
        );
    }
}

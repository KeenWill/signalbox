//! Durable repository-watch cursor and event storage.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    num::{NonZeroU16, NonZeroU32, NonZeroU64},
};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use signalbox_application::{
    RepoWatchBranchHead, RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchConvergenceAssessment,
    RepoWatchConvergenceVerdict, RepoWatchEventContentIdentityV1,
    RepoWatchEventIdentityFrontierEntryV1, RepoWatchEventIdentityFrontierV1,
    RepoWatchEventOccurrenceV1, RepoWatchObservation, RepoWatchPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchReactionObservation, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, RepoWatchReviewObservation,
    RepoWatchStaleReviewClearanceCandidate, RepoWatchThreadObservation,
    RepoWatchWorkflowRunObservation, repo_watch_events_have_equal_identified_content,
};
use signalbox_domain::{
    BranchName, CheckRunName, CommitSha, GitHubObjectId, LabelName, PullRequestBody,
    PullRequestEventContext, PullRequestEventContextInput, PullRequestNumber, PullRequestTitle,
    ReactionSubject, RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventId,
    RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventTarget, RepoWatchTextError,
    RepoWatchWorkflowRunAttempt, RepositorySlug, ReviewThreadId, WorkflowName,
};
use sqlx::{
    PgPool, Postgres, Transaction,
    types::{Json, Uuid},
};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{
        RepoWatchEventProducerStorageKind, RepoWatchEventTargetStorageKind,
        RepoWatchReactionSubjectStorageKind, positive_u64_from_numeric,
        repo_watch_check_conclusion_from_str, repo_watch_check_conclusion_to_str,
        repo_watch_checks_outcome_from_str, repo_watch_checks_outcome_to_str,
        repo_watch_event_kind_from_str, repo_watch_event_kind_to_str,
        repo_watch_event_producer_from_str, repo_watch_event_producer_to_str,
        repo_watch_event_target_from_str, repo_watch_event_target_to_str,
        repo_watch_convergence_verdict_to_str, repo_watch_mergeable_state_from_str,
        repo_watch_mergeable_state_to_str, repo_watch_observed_review_state_to_str,
        repo_watch_pull_request_lifecycle_from_str, repo_watch_pull_request_lifecycle_to_str,
        repo_watch_reaction_change_from_str, repo_watch_reaction_change_to_str,
        repo_watch_reaction_subject_kind_from_str, repo_watch_reaction_subject_kind_to_str,
        repo_watch_reaction_subject_to_storage, repo_watch_review_decision_to_str,
        repo_watch_review_state_from_str, repo_watch_review_state_to_str,
        repo_watch_stale_review_clearance_outcome_to_str, repo_watch_thread_state_from_str,
        repo_watch_thread_state_to_str,
    },
    repo_watch_webhook::{
        RepoWatchWebhookDeliveryKey, RepoWatchWebhookDisposition, RepoWatchWebhookProjection,
        RepoWatchWebhookStoreError, RepoWatchWebhookTerminalOutcome,
        RepoWatchWebhookTerminalRequest, record_terminal_in_transaction,
    },
};

const CURSOR_STORAGE_VERSION: u64 = 2;
const CURSOR_STORAGE_VERSION_DB: i16 = 2;
const EVENT_CONTENT_IDENTITY_VERSION_V1: i16 = 1;
const EVENT_VERSION_V1: i16 = 1;
const MAX_EVENT_PAGE_SIZE: u16 = 100;

/// One positive append-only cursor generation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchCursorGeneration(NonZeroU64);

impl RepoWatchCursorGeneration {
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn try_from_stored(value: i64) -> Result<Self, RepoWatchPersistenceCorruption> {
        let value = u64::try_from(value)
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(RepoWatchPersistenceCorruption::InvalidCursorGeneration)?;
        Ok(Self(value))
    }

    /// Returns the next durable cursor generation when the storage range permits it.
    pub fn next(self) -> Option<Self> {
        let next = self.get().checked_add(1)?;
        if next > i64::MAX as u64 {
            return None;
        }
        NonZeroU64::new(next).map(Self)
    }
}

/// Canonical cursor payload prepared by one complete successful poll.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchCursorCandidate {
    observation: RepoWatchObservation,
    event_identity_frontier: RepoWatchEventIdentityFrontierV1,
}

impl RepoWatchCursorCandidate {
    pub fn new(observation: RepoWatchObservation) -> Self {
        Self {
            observation,
            event_identity_frontier: RepoWatchEventIdentityFrontierV1::default(),
        }
    }

    pub const fn with_event_identity_frontier(
        observation: RepoWatchObservation,
        event_identity_frontier: RepoWatchEventIdentityFrontierV1,
    ) -> Self {
        Self {
            observation,
            event_identity_frontier,
        }
    }

    pub const fn observation(&self) -> &RepoWatchObservation {
        &self.observation
    }

    pub const fn event_identity_frontier(&self) -> &RepoWatchEventIdentityFrontierV1 {
        &self.event_identity_frontier
    }
}

/// Latest accepted durable cursor for one repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchCursor {
    repository: RepositorySlug,
    generation: RepoWatchCursorGeneration,
    candidate: RepoWatchCursorCandidate,
}

impl RepoWatchCursor {
    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    pub const fn generation(&self) -> RepoWatchCursorGeneration {
        self.generation
    }

    pub const fn candidate(&self) -> &RepoWatchCursorCandidate {
        &self.candidate
    }
}

/// One optimistic atomic cursor-and-event commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchCommitRequest {
    expected_generation: Option<RepoWatchCursorGeneration>,
    candidate: RepoWatchCursorCandidate,
    events: Box<[RepoWatchEventOccurrenceV1]>,
    producer: RepoWatchEventProducer,
}

impl RepoWatchCommitRequest {
    pub fn new(
        expected_generation: Option<RepoWatchCursorGeneration>,
        candidate: RepoWatchCursorCandidate,
        events: Vec<RepoWatchEventOccurrenceV1>,
    ) -> Self {
        Self {
            expected_generation,
            candidate,
            events: events.into_boxed_slice(),
            producer: RepoWatchEventProducer::Poll,
        }
    }

    /// Constructs a commit whose facts were produced from an authenticated webhook.
    pub fn from_webhook(
        expected_generation: RepoWatchCursorGeneration,
        candidate: RepoWatchCursorCandidate,
        events: Vec<RepoWatchEventOccurrenceV1>,
    ) -> Self {
        Self {
            expected_generation: Some(expected_generation),
            candidate,
            events: events.into_boxed_slice(),
            producer: RepoWatchEventProducer::Webhook,
        }
    }

    pub const fn expected_generation(&self) -> Option<RepoWatchCursorGeneration> {
        self.expected_generation
    }

    pub const fn candidate(&self) -> &RepoWatchCursorCandidate {
        &self.candidate
    }

    pub fn events(&self) -> &[RepoWatchEventOccurrenceV1] {
        &self.events
    }

    pub const fn producer(&self) -> RepoWatchEventProducer {
        self.producer
    }
}

/// Auditable producer that won one repository-watch event commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchEventProducer {
    Poll,
    Webhook,
}

impl RepoWatchEventProducer {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Poll => "poll",
            Self::Webhook => "webhook",
        }
    }
}

/// Outcome of one atomic cursor-and-event commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchCommitOutcome {
    Committed(RepoWatchCursor),
    Replayed(RepoWatchCursor),
    Unchanged(RepoWatchCursor),
    Conflict {
        current: Option<RepoWatchCursorGeneration>,
    },
}

/// Positive position of one event within its cursor generation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchEventPosition {
    generation: RepoWatchCursorGeneration,
    ordinal: NonZeroU32,
}

impl RepoWatchEventPosition {
    const fn new(generation: RepoWatchCursorGeneration, ordinal: NonZeroU32) -> Self {
        Self {
            generation,
            ordinal,
        }
    }

    pub const fn generation(self) -> RepoWatchCursorGeneration {
        self.generation
    }

    pub const fn ordinal(self) -> NonZeroU32 {
        self.ordinal
    }
}

/// One bounded durable event-page size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchEventPageSize(NonZeroU16);

impl RepoWatchEventPageSize {
    pub fn try_new(value: NonZeroU16) -> Result<Self, RepoWatchPageSizeError> {
        if value.get() > MAX_EVENT_PAGE_SIZE {
            Err(RepoWatchPageSizeError)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// A requested durable event page exceeded the fixed bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchPageSizeError;

impl fmt::Display for RepoWatchPageSizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("repository-watch event page size exceeds 100")
    }
}

impl Error for RepoWatchPageSizeError {}

/// One positioned durable repository-watch fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PositionedRepoWatchEvent {
    position: RepoWatchEventPosition,
    event: RepoWatchEvent,
}

impl PositionedRepoWatchEvent {
    pub const fn position(&self) -> RepoWatchEventPosition {
        self.position
    }

    pub const fn event(&self) -> &RepoWatchEvent {
        &self.event
    }
}

/// One bounded keyset-ordered page of durable facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchEventPage {
    events: Box<[PositionedRepoWatchEvent]>,
    next_after: Option<RepoWatchEventPosition>,
}

impl RepoWatchEventPage {
    pub fn events(&self) -> &[PositionedRepoWatchEvent] {
        &self.events
    }

    pub const fn next_after(&self) -> Option<RepoWatchEventPosition> {
        self.next_after
    }
}

/// Closed fail-closed classification for malformed durable repository-watch data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchPersistenceCorruption {
    InvalidCursorGeneration,
    MalformedCursorDocument,
    UnsupportedCursorVersion,
    InvalidCursorField(&'static str),
    UnknownCursorDiscriminator(&'static str),
    NonCanonicalCursor,
    InvalidEventPosition,
    UnsupportedEventVersion,
    UnsupportedEventContentIdentityVersion,
    InvalidEventContentIdentity,
    UnknownEventProducer,
    InvalidEventField(&'static str),
    UnknownEventDiscriminator(&'static str),
    InvalidStoredDomainValue,
    EventShapeMismatch,
}

impl fmt::Display for RepoWatchPersistenceCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCursorGeneration => formatter.write_str("invalid cursor generation"),
            Self::MalformedCursorDocument => formatter.write_str("malformed cursor document"),
            Self::UnsupportedCursorVersion => formatter.write_str("unsupported cursor version"),
            Self::InvalidCursorField(field) => write!(formatter, "invalid cursor field {field}"),
            Self::UnknownCursorDiscriminator(field) => {
                write!(formatter, "unknown cursor discriminator {field}")
            }
            Self::NonCanonicalCursor => formatter.write_str("noncanonical cursor payload"),
            Self::InvalidEventPosition => formatter.write_str("invalid event position"),
            Self::UnsupportedEventVersion => formatter.write_str("unsupported event version"),
            Self::UnsupportedEventContentIdentityVersion => {
                formatter.write_str("unsupported event content identity version")
            }
            Self::InvalidEventContentIdentity => {
                formatter.write_str("invalid event content identity")
            }
            Self::UnknownEventProducer => formatter.write_str("unknown event producer"),
            Self::InvalidEventField(field) => write!(formatter, "invalid event field {field}"),
            Self::UnknownEventDiscriminator(field) => {
                write!(formatter, "unknown event discriminator {field}")
            }
            Self::InvalidStoredDomainValue => formatter.write_str("invalid stored domain value"),
            Self::EventShapeMismatch => formatter.write_str("event row shape mismatch"),
        }
    }
}

impl Error for RepoWatchPersistenceCorruption {}

/// Database, request, or fail-closed repository-watch storage failure.
#[derive(Debug)]
pub enum RepoWatchStoreError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    CursorEncoding(serde_json::Error),
    Corruption(RepoWatchPersistenceCorruption),
    EventRepositoryMismatch,
    DuplicateEventIdentity(RepoWatchEventId),
    DuplicateEventContentIdentity(RepoWatchEventContentIdentityV1),
    EventsWithoutStateChange,
    CursorGenerationExhausted,
    EventBatchTooLarge,
    ConvergenceEvidenceTooLarge,
    ConvergenceEvidenceMismatch,
    StaleReviewClearanceMismatch,
    WebhookTerminal(RepoWatchWebhookStoreError),
    InvalidWebhookCommit,
}

impl fmt::Display for RepoWatchStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "repository-watch database failure: {error}")
            }
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "repository-watch commit outcome is ambiguous: {error}"
                )
            }
            Self::CursorEncoding(error) => {
                write!(
                    formatter,
                    "repository-watch cursor encoding failed: {error}"
                )
            }
            Self::Corruption(error) => {
                write!(formatter, "repository-watch storage is corrupt: {error}")
            }
            Self::EventRepositoryMismatch => {
                formatter.write_str("repository-watch event names another repository")
            }
            Self::DuplicateEventIdentity(id) => write!(
                formatter,
                "repository-watch event batch repeats identity {id:?}"
            ),
            Self::DuplicateEventContentIdentity(identity) => write!(
                formatter,
                "repository-watch event batch repeats content identity {:02x?}",
                identity.as_bytes()
            ),
            Self::EventsWithoutStateChange => {
                formatter.write_str("repository-watch events accompany an unchanged cursor state")
            }
            Self::CursorGenerationExhausted => {
                formatter.write_str("repository-watch cursor generation is exhausted")
            }
            Self::EventBatchTooLarge => formatter
                .write_str("repository-watch event batch exceeds the durable ordinal range"),
            Self::ConvergenceEvidenceTooLarge => {
                formatter.write_str("repository-watch convergence evidence exceeds durable bounds")
            }
            Self::ConvergenceEvidenceMismatch => formatter
                .write_str("repository-watch convergence evidence names another cursor state"),
            Self::StaleReviewClearanceMismatch => formatter.write_str(
                "repository-watch stale review clearance names ineligible or changed evidence",
            ),
            Self::WebhookTerminal(error) => {
                write!(
                    formatter,
                    "repository-watch webhook terminal commit failed: {error}"
                )
            }
            Self::InvalidWebhookCommit => formatter
                .write_str("repository-watch webhook commit has invalid producer or projections"),
        }
    }
}

impl Error for RepoWatchStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::CursorEncoding(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::WebhookTerminal(error) => Some(error),
            Self::EventRepositoryMismatch
            | Self::DuplicateEventIdentity(_)
            | Self::DuplicateEventContentIdentity(_)
            | Self::EventsWithoutStateChange
            | Self::CursorGenerationExhausted
            | Self::EventBatchTooLarge
            | Self::ConvergenceEvidenceTooLarge
            | Self::ConvergenceEvidenceMismatch
            | Self::StaleReviewClearanceMismatch
            | Self::InvalidWebhookCommit => None,
        }
    }
}

impl From<RepoWatchWebhookStoreError> for RepoWatchStoreError {
    fn from(error: RepoWatchWebhookStoreError) -> Self {
        Self::WebhookTerminal(error)
    }
}

impl From<sqlx::Error> for RepoWatchStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Durable intent created before one stale review dismissal request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchPlannedStaleReviewClearance {
    clearance_id: Uuid,
    number: PullRequestNumber,
    current_head_sha: CommitSha,
    review_node_id: Box<str>,
    reviewer: RepoWatchAuthorLogin,
    reviewed_head_sha: CommitSha,
    dismissal_message: Box<str>,
}

impl RepoWatchPlannedStaleReviewClearance {
    pub const fn clearance_id(&self) -> Uuid {
        self.clearance_id
    }

    pub const fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub const fn current_head_sha(&self) -> &CommitSha {
        &self.current_head_sha
    }

    pub const fn review_node_id(&self) -> &str {
        &self.review_node_id
    }

    pub const fn reviewer(&self) -> &RepoWatchAuthorLogin {
        &self.reviewer
    }

    pub const fn reviewed_head_sha(&self) -> &CommitSha {
        &self.reviewed_head_sha
    }

    pub const fn dismissal_message(&self) -> &str {
        &self.dismissal_message
    }
}

/// Terminal observation for one durable stale-review clearance intent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchStaleReviewClearanceOutcome {
    Dismissed,
    AlreadyDismissed,
    ClearedElsewhere,
    Superseded,
}

/// Provider state observed while settling a clearance intent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchObservedReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

impl From<RepoWatchPersistenceCorruption> for RepoWatchStoreError {
    fn from(error: RepoWatchPersistenceCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<RepoWatchTextError> for RepoWatchStoreError {
    fn from(_error: RepoWatchTextError) -> Self {
        Self::Corruption(RepoWatchPersistenceCorruption::InvalidStoredDomainValue)
    }
}

/// PostgreSQL adapter for atomic repository-watch cursors and events.
#[derive(Clone, Debug)]
pub struct PostgresRepoWatchStore {
    pool: PgPool,
}

impl PostgresRepoWatchStore {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn load_cursor(
        &self,
        repository: &RepositorySlug,
    ) -> Result<Option<RepoWatchCursor>, RepoWatchStoreError> {
        let row = sqlx::query_as::<_, CursorRow>(
            "SELECT generation, cursor_payload
               FROM repo_watch_cursor
              WHERE repository = $1
              ORDER BY generation DESC
              LIMIT 1",
        )
        .bind(repository.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| decode_cursor_row(repository, row))
            .transpose()
    }

    pub async fn commit(
        &self,
        repository: &RepositorySlug,
        request: RepoWatchCommitRequest,
    ) -> Result<RepoWatchCommitOutcome, RepoWatchStoreError> {
        self.commit_internal(repository, request, None, None).await
    }

    /// Commits webhook-derived state and its terminal delivery record atomically.
    pub async fn commit_webhook(
        &self,
        repository: &RepositorySlug,
        request: RepoWatchCommitRequest,
        delivery: RepoWatchWebhookDeliveryKey,
        projections: Vec<RepoWatchWebhookProjection>,
    ) -> Result<RepoWatchCommitOutcome, RepoWatchStoreError> {
        if request.producer() != RepoWatchEventProducer::Webhook {
            return Err(RepoWatchStoreError::InvalidWebhookCommit);
        }
        self.commit_internal(repository, request, Some((delivery, projections)), None)
            .await
    }

    /// Commits webhook-derived state, targeted convergence evidence, and the
    /// terminal delivery record atomically.
    pub async fn commit_webhook_with_convergence(
        &self,
        repository: &RepositorySlug,
        request: RepoWatchCommitRequest,
        delivery: RepoWatchWebhookDeliveryKey,
        projections: Vec<RepoWatchWebhookProjection>,
        assessments: &[RepoWatchConvergenceAssessment],
    ) -> Result<RepoWatchCommitOutcome, RepoWatchStoreError> {
        if request.producer() != RepoWatchEventProducer::Webhook {
            return Err(RepoWatchStoreError::InvalidWebhookCommit);
        }
        self.commit_internal(
            repository,
            request,
            Some((delivery, projections)),
            Some(assessments),
        )
        .await
    }

    /// Atomically commits the cursor, derived events, convergence assessments,
    /// and any seals created by those assessments.
    pub async fn commit_with_convergence(
        &self,
        repository: &RepositorySlug,
        request: RepoWatchCommitRequest,
        assessments: &[RepoWatchConvergenceAssessment],
    ) -> Result<RepoWatchCommitOutcome, RepoWatchStoreError> {
        self.commit_internal(repository, request, None, Some(assessments))
            .await
    }

    async fn commit_internal(
        &self,
        repository: &RepositorySlug,
        request: RepoWatchCommitRequest,
        webhook: Option<(RepoWatchWebhookDeliveryKey, Vec<RepoWatchWebhookProjection>)>,
        assessments: Option<&[RepoWatchConvergenceAssessment]>,
    ) -> Result<RepoWatchCommitOutcome, RepoWatchStoreError> {
        let has_webhook = webhook.is_some();
        let has_assessments = assessments.is_some();
        validate_event_batch(repository, request.events())?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        let current = load_cursor_in_transaction(&mut transaction, repository).await?;
        let current_generation = current.as_ref().map(RepoWatchCursor::generation);
        if current_generation != request.expected_generation() {
            let replayed = exact_replay(&mut transaction, repository, &request).await?;
            let Some(cursor) = replayed else {
                transaction.rollback().await?;
                return Ok(RepoWatchCommitOutcome::Conflict {
                    current: current_generation,
                });
            };
            if let Some((delivery, projections)) = webhook {
                record_committed_webhook_terminal(
                    &mut transaction,
                    delivery,
                    projections,
                    cursor.generation(),
                )
                .await?;
            }
            if let Some(assessments) = assessments
                && current_generation == Some(cursor.generation())
            {
                Self::record_convergence_assessments_in_transaction(
                    &mut transaction,
                    repository,
                    &cursor,
                    assessments,
                    has_webhook,
                )
                .await?;
            }
            if has_webhook || has_assessments {
                commit_repo_watch_transaction(transaction).await?;
            } else {
                transaction.rollback().await?;
            }
            return Ok(RepoWatchCommitOutcome::Replayed(cursor));
        }
        if let Some(current) = current.as_ref()
            && current.candidate() == request.candidate()
        {
            if request.events().is_empty() {
                let cursor = current.clone();
                if let Some((delivery, projections)) = webhook {
                    record_committed_webhook_terminal(
                        &mut transaction,
                        delivery,
                        projections,
                        cursor.generation(),
                    )
                    .await?;
                }
                if let Some(assessments) = assessments {
                    Self::record_convergence_assessments_in_transaction(
                        &mut transaction,
                        repository,
                        &cursor,
                        assessments,
                        has_webhook,
                    )
                    .await?;
                }
                if has_webhook || has_assessments {
                    commit_repo_watch_transaction(transaction).await?;
                } else {
                    transaction.rollback().await?;
                }
                return Ok(RepoWatchCommitOutcome::Unchanged(cursor));
            }
            transaction.rollback().await?;
            return Err(RepoWatchStoreError::EventsWithoutStateChange);
        }
        let generation = match current_generation {
            Some(generation) => generation
                .next()
                .ok_or(RepoWatchStoreError::CursorGenerationExhausted)?,
            None => RepoWatchCursorGeneration::INITIAL,
        };
        let payload = encode_cursor_candidate(request.candidate())?;
        sqlx::query(
            "INSERT INTO repo_watch_cursor
                (repository, generation, storage_version, cursor_payload)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(repository.as_str())
        .bind(generation_to_i64(generation))
        .bind(CURSOR_STORAGE_VERSION_DB)
        .bind(Json(payload))
        .execute(&mut *transaction)
        .await?;
        let already_durable =
            durable_occurrences(&mut transaction, repository, request.events(), None).await?;
        let fresh = request
            .events()
            .iter()
            .filter(|occurrence| is_new_occurrence(&already_durable, occurrence))
            .collect::<Vec<_>>();
        insert_events(&mut transaction, repository, generation, &fresh).await?;
        let cursor = RepoWatchCursor {
            repository: repository.clone(),
            generation,
            candidate: request.candidate.clone(),
        };
        if let Some((delivery, projections)) = webhook {
            record_committed_webhook_terminal(&mut transaction, delivery, projections, generation)
                .await?;
        }
        if let Some(assessments) = assessments {
            Self::record_convergence_assessments_in_transaction(
                &mut transaction,
                repository,
                &cursor,
                assessments,
                has_webhook,
            )
            .await?;
        }
        commit_repo_watch_transaction(transaction).await?;
        Ok(RepoWatchCommitOutcome::Committed(cursor))
    }

    /// Appends changed convergence evidence and seals every converged head/base
    /// identity. Equal evidence for that identity is an idempotent replay.
    pub async fn record_convergence_assessments(
        &self,
        repository: &RepositorySlug,
        cursor_generation: RepoWatchCursorGeneration,
        assessments: &[RepoWatchConvergenceAssessment],
    ) -> Result<(), RepoWatchStoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        let cursor = load_cursor_in_transaction(&mut transaction, repository)
            .await?
            .ok_or(RepoWatchStoreError::ConvergenceEvidenceMismatch)?;
        if cursor.generation() != cursor_generation {
            transaction.rollback().await?;
            return Err(RepoWatchStoreError::ConvergenceEvidenceMismatch);
        }
        Self::record_convergence_assessments_in_transaction(
            &mut transaction,
            repository,
            &cursor,
            assessments,
            false,
        )
        .await?;
        commit_repo_watch_transaction(transaction).await
    }

    async fn record_convergence_assessments_in_transaction(
        transaction: &mut Transaction<'_, Postgres>,
        repository: &RepositorySlug,
        cursor: &RepoWatchCursor,
        assessments: &[RepoWatchConvergenceAssessment],
        partial: bool,
    ) -> Result<(), RepoWatchStoreError> {
        let state = cursor.candidate().observation().state();
        let pull_requests = state.pull_requests();
        if !partial && pull_requests.len() != assessments.len() {
            return Err(RepoWatchStoreError::ConvergenceEvidenceMismatch);
        }
        let mut assessed_pull_requests = HashSet::with_capacity(assessments.len());
        for assessment in assessments {
            if !assessed_pull_requests.insert(assessment.number()) {
                return Err(RepoWatchStoreError::ConvergenceEvidenceMismatch);
            }
            let pull_request_matches = pull_requests.iter().any(|pull_request| {
                pull_request.context().number() == assessment.number()
                    && pull_request.context().head_sha() == assessment.head_sha()
                    && pull_request.context().base_branch() == assessment.base_branch()
                    && pull_request.mergeable_state() == assessment.mergeable_state()
            });
            // GitHub's pull-request `baseRefOid` is the exact base revision
            // associated with that pull request. It can remain behind the
            // current branch ref until the pull request is merged forward.
            // Complete polls must contain the named base branch. A targeted
            // webhook read may reach a pull request before the cursor learns
            // that branch, so it still records the assessment to supersede
            // older evidence. Admission and cutoff remain ineligible until a
            // later cursor contains the assessed branch revision.
            let base_branch_matches = partial
                || state
                    .branch_heads()
                    .iter()
                    .any(|branch_head| branch_head.branch() == assessment.base_branch());
            if !pull_request_matches || !base_branch_matches {
                return Err(RepoWatchStoreError::ConvergenceEvidenceMismatch);
            }
        }
        for assessment in assessments {
            let unresolved_threads = assessment
                .unresolved_threads()
                .iter()
                .map(|thread| thread.as_str().to_owned())
                .collect::<Vec<_>>();
            let non_green_gating_checks = assessment
                .non_green_gating_checks()
                .iter()
                .map(|check| check.as_str().to_owned())
                .collect::<Vec<_>>();
            let gating_check_count = i64::try_from(assessment.gating_check_count())
                .map_err(|_| RepoWatchStoreError::ConvergenceEvidenceTooLarge)?;
            let evidence_is_unchanged: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1
                      FROM (
                            SELECT head_sha, base_branch, base_revision, mergeable_state,
                                   review_decision, unresolved_threads,
                                   gating_check_count, non_green_gating_checks,
                                   verdict_kind
                             FROM repo_watch_pull_request_convergence_assessment
                             WHERE repository = $1
                               AND pull_request_number = $2
                             ORDER BY recorded_at DESC, assessment_id DESC
                             LIMIT 1
                           ) AS current
                     WHERE current.head_sha = $3
                       AND current.base_revision = $4
                       AND current.base_branch = $5
                       AND current.mergeable_state = $6
                       AND current.review_decision = $7
                       AND current.unresolved_threads = $8
                       AND current.gating_check_count = $9
                       AND current.non_green_gating_checks = $10
                       AND current.verdict_kind = $11
                )",
            )
            .bind(repository.as_str())
            .bind(Decimal::from(assessment.number().get()))
            .bind(assessment.head_sha().as_str())
            .bind(assessment.base_revision().as_str())
            .bind(assessment.base_branch().as_str())
            .bind(repo_watch_mergeable_state_to_str(
                assessment.mergeable_state(),
            ))
            .bind(repo_watch_review_decision_to_str(
                assessment.review_decision(),
            ))
            .bind(&unresolved_threads)
            .bind(gating_check_count)
            .bind(&non_green_gating_checks)
            .bind(repo_watch_convergence_verdict_to_str(assessment.verdict()))
            .fetch_one(&mut **transaction)
            .await?;
            if evidence_is_unchanged {
                continue;
            }
            let assessment_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO repo_watch_pull_request_convergence_assessment
                    (assessment_id, repository, cursor_generation,
                     pull_request_number, head_sha, base_branch, base_revision,
                     mergeable_state, review_decision, unresolved_threads,
                     gating_check_count, non_green_gating_checks, verdict_kind)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
            )
            .bind(assessment_id)
            .bind(repository.as_str())
            .bind(generation_to_i64(cursor.generation()))
            .bind(Decimal::from(assessment.number().get()))
            .bind(assessment.head_sha().as_str())
            .bind(assessment.base_branch().as_str())
            .bind(assessment.base_revision().as_str())
            .bind(repo_watch_mergeable_state_to_str(
                assessment.mergeable_state(),
            ))
            .bind(repo_watch_review_decision_to_str(
                assessment.review_decision(),
            ))
            .bind(&unresolved_threads)
            .bind(gating_check_count)
            .bind(&non_green_gating_checks)
            .bind(repo_watch_convergence_verdict_to_str(assessment.verdict()))
            .execute(&mut **transaction)
            .await?;
            if assessment.verdict() != RepoWatchConvergenceVerdict::NotConverged {
                sqlx::query(
                    "INSERT INTO repo_watch_pull_request_convergence
                        (repository, pull_request_number, head_sha, base_revision,
                         assessment_id, convergence_kind)
                     VALUES ($1,$2,$3,$4,$5,$6)
                     ON CONFLICT DO NOTHING",
                )
                .bind(repository.as_str())
                .bind(Decimal::from(assessment.number().get()))
                .bind(assessment.head_sha().as_str())
                .bind(assessment.base_revision().as_str())
                .bind(assessment_id)
                .bind(repo_watch_convergence_verdict_to_str(assessment.verdict()))
                .execute(&mut **transaction)
                .await?;
            }
        }
        Ok(())
    }

    /// Durably records eligible exact-head dismissal intents before any forge
    /// mutation. Existing equal intents are replayed with their original ID.
    pub async fn plan_stale_review_clearances(
        &self,
        repository: &RepositorySlug,
        cursor_generation: RepoWatchCursorGeneration,
        candidates: &[RepoWatchStaleReviewClearanceCandidate],
    ) -> Result<Vec<RepoWatchPlannedStaleReviewClearance>, RepoWatchStoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        let cursor = load_cursor_in_transaction(&mut transaction, repository)
            .await?
            .ok_or(RepoWatchStoreError::StaleReviewClearanceMismatch)?;
        if cursor.generation() != cursor_generation {
            transaction.rollback().await?;
            return Err(RepoWatchStoreError::StaleReviewClearanceMismatch);
        }
        let mut review_ids = HashSet::with_capacity(candidates.len());
        let mut planned = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if !review_ids.insert(candidate.review_node_id()) {
                transaction.rollback().await?;
                return Err(RepoWatchStoreError::StaleReviewClearanceMismatch);
            }
            let (assessment_id, base_revision) = sqlx::query_as::<_, (Uuid, String)>(
                "SELECT assessment_id, base_revision
                   FROM (
                         SELECT assessment_id, head_sha, base_branch, base_revision,
                                review_decision, unresolved_threads,
                                non_green_gating_checks, mergeable_state, verdict_kind
                           FROM repo_watch_pull_request_convergence_assessment
                          WHERE repository = $1
                            AND pull_request_number = $2
                          ORDER BY recorded_at DESC, assessment_id DESC
                          LIMIT 1
                        ) AS current
                  WHERE current.head_sha = $3
                    AND current.review_decision = 'changes_requested'
                    AND cardinality(current.unresolved_threads) = 0
                    AND cardinality(current.non_green_gating_checks) = 0
                    AND current.mergeable_state <> 'conflicting'
                    AND current.verdict_kind = 'not_converged'
                    AND EXISTS (
                        SELECT 1
                          FROM repo_watch_cursor AS cursor
                          CROSS JOIN LATERAL jsonb_array_elements(
                              cursor.cursor_payload -> 'state' -> 'branch_heads'
                          ) AS branch_head
                         WHERE cursor.repository = $1
                           AND cursor.generation = $4
                           AND branch_head ->> 'branch' = current.base_branch
                           AND branch_head ->> 'head' = current.base_revision
                    )",
            )
            .bind(repository.as_str())
            .bind(Decimal::from(candidate.number().get()))
            .bind(candidate.current_head_sha().as_str())
            .bind(generation_to_i64(cursor_generation))
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RepoWatchStoreError::StaleReviewClearanceMismatch)?;
            let dismissal_message = stale_review_dismissal_message(candidate);
            let clearance_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO repo_watch_stale_review_clearance
                    (clearance_id, assessment_id, repository,
                     pull_request_number, current_head_sha, base_revision, review_node_id,
                     reviewer, reviewed_head_sha, dismissal_message)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                 ON CONFLICT (assessment_id, review_node_id) DO NOTHING",
            )
            .bind(clearance_id)
            .bind(assessment_id)
            .bind(repository.as_str())
            .bind(Decimal::from(candidate.number().get()))
            .bind(candidate.current_head_sha().as_str())
            .bind(&base_revision)
            .bind(candidate.review_node_id())
            .bind(candidate.reviewer().as_str())
            .bind(candidate.reviewed_head_sha().as_str())
            .bind(&dismissal_message)
            .execute(&mut *transaction)
            .await?;
            let stored = sqlx::query_as::<_, PlannedClearanceRow>(
                "SELECT clearance.clearance_id,
                        clearance.dismissal_message,
                        result.clearance_id IS NOT NULL AS completed
                   FROM repo_watch_stale_review_clearance AS clearance
                   LEFT JOIN repo_watch_stale_review_clearance_result AS result
                     ON result.clearance_id = clearance.clearance_id
                  WHERE clearance.assessment_id = $1
                    AND clearance.review_node_id = $2
                    AND clearance.repository = $3
                    AND clearance.pull_request_number = $4
                    AND clearance.current_head_sha = $5
                    AND clearance.base_revision = $6
                    AND clearance.reviewer = $7
                    AND clearance.reviewed_head_sha = $8",
            )
            .bind(assessment_id)
            .bind(candidate.review_node_id())
            .bind(repository.as_str())
            .bind(Decimal::from(candidate.number().get()))
            .bind(candidate.current_head_sha().as_str())
            .bind(&base_revision)
            .bind(candidate.reviewer().as_str())
            .bind(candidate.reviewed_head_sha().as_str())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RepoWatchStoreError::StaleReviewClearanceMismatch)?;
            if stored.completed {
                continue;
            }
            planned.push(RepoWatchPlannedStaleReviewClearance {
                clearance_id: stored.clearance_id,
                number: candidate.number(),
                current_head_sha: candidate.current_head_sha().clone(),
                review_node_id: candidate.review_node_id().into(),
                reviewer: candidate.reviewer().clone(),
                reviewed_head_sha: candidate.reviewed_head_sha().clone(),
                dismissal_message: stored.dismissal_message.into_boxed_str(),
            });
        }
        transaction.commit().await?;
        Ok(planned)
    }

    /// Loads a bounded oldest-first page of intents lacking a terminal result.
    pub async fn load_pending_stale_review_clearances(
        &self,
        repository: &RepositorySlug,
    ) -> Result<Vec<RepoWatchPlannedStaleReviewClearance>, RepoWatchStoreError> {
        let rows = sqlx::query_as::<_, PendingClearanceRow>(
            "SELECT clearance.clearance_id,
                    clearance.pull_request_number,
                    clearance.current_head_sha,
                    clearance.review_node_id,
                    clearance.reviewer,
                    clearance.reviewed_head_sha,
                    clearance.dismissal_message
               FROM repo_watch_stale_review_clearance AS clearance
               LEFT JOIN repo_watch_stale_review_clearance_result AS result
                 ON result.clearance_id = clearance.clearance_id
              WHERE clearance.repository = $1
                AND result.clearance_id IS NULL
              ORDER BY clearance.planned_at, clearance.clearance_id
              LIMIT 128",
        )
        .bind(repository.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(decode_pending_clearance).collect()
    }

    /// Appends the first terminal provider observation for one clearance.
    pub async fn record_stale_review_clearance_outcome(
        &self,
        clearance_id: Uuid,
        outcome: RepoWatchStaleReviewClearanceOutcome,
        provider_state: RepoWatchObservedReviewState,
    ) -> Result<(), RepoWatchStoreError> {
        sqlx::query(
            "INSERT INTO repo_watch_stale_review_clearance_result
                (clearance_id, outcome_kind, provider_review_state)
             VALUES ($1,$2,$3)
             ON CONFLICT (clearance_id) DO NOTHING",
        )
        .bind(clearance_id)
        .bind(repo_watch_stale_review_clearance_outcome_to_str(outcome))
        .bind(repo_watch_observed_review_state_to_str(provider_state))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_event_page(
        &self,
        repository: &RepositorySlug,
        after: Option<RepoWatchEventPosition>,
        page_size: RepoWatchEventPageSize,
    ) -> Result<RepoWatchEventPage, RepoWatchStoreError> {
        let mut rows = load_event_rows(
            &self.pool,
            repository,
            after,
            usize::from(page_size.get()) + 1,
        )
        .await?;
        let has_more = rows.len() > usize::from(page_size.get());
        rows.truncate(usize::from(page_size.get()));
        let events = rows
            .into_iter()
            .map(|row| decode_positioned_event(repository, row))
            .collect::<Result<Vec<_>, _>>()?;
        let next_after = has_more
            .then(|| events.last().map(PositionedRepoWatchEvent::position))
            .flatten();
        Ok(RepoWatchEventPage {
            events: events.into_boxed_slice(),
            next_after,
        })
    }

    /// Loads one exact durable event identity through the closed decoder.
    pub async fn load_event(
        &self,
        repository: &RepositorySlug,
        event: RepoWatchEventId,
    ) -> Result<Option<RepoWatchEvent>, RepoWatchStoreError> {
        let row = sqlx::query_as::<_, EventRow>(EVENT_BY_ID_SQL)
            .bind(repository.as_str())
            .bind(event.as_uuid())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| decode_positioned_event(repository, row).map(|positioned| positioned.event))
            .transpose()
    }
}

#[derive(sqlx::FromRow)]
struct CursorRow {
    generation: i64,
    cursor_payload: Json<Value>,
}

#[derive(sqlx::FromRow)]
struct PlannedClearanceRow {
    clearance_id: Uuid,
    dismissal_message: String,
    completed: bool,
}

#[derive(sqlx::FromRow)]
struct PendingClearanceRow {
    clearance_id: Uuid,
    pull_request_number: Decimal,
    current_head_sha: String,
    review_node_id: String,
    reviewer: String,
    reviewed_head_sha: String,
    dismissal_message: String,
}

fn decode_pending_clearance(
    row: PendingClearanceRow,
) -> Result<RepoWatchPlannedStaleReviewClearance, RepoWatchStoreError> {
    let number = positive_u64_from_numeric(row.pull_request_number)
        .map_err(|_| RepoWatchPersistenceCorruption::InvalidStoredDomainValue)?;
    let number = pull_request_number(number, "stale review pull request")?;
    let current_head_sha = CommitSha::try_new(row.current_head_sha)
        .map_err(|_| RepoWatchPersistenceCorruption::InvalidStoredDomainValue)?;
    if row.review_node_id.is_empty()
        || row.review_node_id.len() > 256
        || row.review_node_id.contains('\0')
        || row.dismissal_message.is_empty()
        || row.dismissal_message.len() > 1024
        || row.dismissal_message.contains('\0')
    {
        return Err(RepoWatchPersistenceCorruption::InvalidStoredDomainValue.into());
    }
    let reviewer = RepoWatchAuthorLogin::try_new(row.reviewer)
        .map_err(|_| RepoWatchPersistenceCorruption::InvalidStoredDomainValue)?;
    let reviewed_head_sha = CommitSha::try_new(row.reviewed_head_sha)
        .map_err(|_| RepoWatchPersistenceCorruption::InvalidStoredDomainValue)?;
    if current_head_sha == reviewed_head_sha {
        return Err(RepoWatchPersistenceCorruption::InvalidStoredDomainValue.into());
    }
    Ok(RepoWatchPlannedStaleReviewClearance {
        clearance_id: row.clearance_id,
        number,
        current_head_sha,
        review_node_id: row.review_node_id.into_boxed_str(),
        reviewer,
        reviewed_head_sha,
        dismissal_message: row.dismissal_message.into_boxed_str(),
    })
}

fn stale_review_dismissal_message(candidate: &RepoWatchStaleReviewClearanceCandidate) -> String {
    format!(
        "Repository watch dismissed stale review {} by {}: every review thread is resolved and every other convergence gate is green on current head {}; the review targeted superseded head {}.",
        candidate.review_node_id(),
        candidate.reviewer().as_str(),
        candidate.current_head_sha().as_str(),
        candidate.reviewed_head_sha().as_str(),
    )
}

async fn load_cursor_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
) -> Result<Option<RepoWatchCursor>, RepoWatchStoreError> {
    let row = sqlx::query_as::<_, CursorRow>(
        "SELECT generation, cursor_payload
           FROM repo_watch_cursor
          WHERE repository = $1
          ORDER BY generation DESC
          LIMIT 1",
    )
    .bind(repository.as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(|row| decode_cursor_row(repository, row))
        .transpose()
}

async fn load_cursor_generation_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    generation: RepoWatchCursorGeneration,
) -> Result<Option<RepoWatchCursor>, RepoWatchStoreError> {
    let row = sqlx::query_as::<_, CursorRow>(
        "SELECT generation, cursor_payload
           FROM repo_watch_cursor
          WHERE repository = $1
            AND generation = $2",
    )
    .bind(repository.as_str())
    .bind(generation_to_i64(generation))
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(|row| decode_cursor_row(repository, row))
        .transpose()
}

fn decode_cursor_row(
    repository: &RepositorySlug,
    row: CursorRow,
) -> Result<RepoWatchCursor, RepoWatchStoreError> {
    Ok(RepoWatchCursor {
        repository: repository.clone(),
        generation: RepoWatchCursorGeneration::try_from_stored(row.generation)?,
        candidate: decode_cursor_candidate(row.cursor_payload.0)?,
    })
}

fn generation_to_i64(generation: RepoWatchCursorGeneration) -> i64 {
    generation.get() as i64
}

async fn commit_repo_watch_transaction(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), RepoWatchStoreError> {
    transaction.commit().await.map_err(|error| {
        if commit_failure_is_ambiguous(&error) {
            RepoWatchStoreError::CommitAmbiguous(error)
        } else {
            RepoWatchStoreError::Database(error)
        }
    })
}

async fn exact_replay(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    request: &RepoWatchCommitRequest,
) -> Result<Option<RepoWatchCursor>, RepoWatchStoreError> {
    let expected_replay_generation = match request.expected_generation() {
        Some(generation) => generation.next(),
        None => Some(RepoWatchCursorGeneration::INITIAL),
    };
    let Some(expected_replay_generation) = expected_replay_generation else {
        return Ok(None);
    };
    let Some(replayed) =
        load_cursor_generation_in_transaction(transaction, repository, expected_replay_generation)
            .await?
    else {
        return Ok(None);
    };
    if replayed.candidate() != request.candidate() {
        return Ok(None);
    }
    let stored =
        load_generation_events_in_transaction(transaction, repository, expected_replay_generation)
            .await?;
    // A commit coalesces occurrences already durable when it runs, so the replayed
    // generation holds the requested batch minus those. Comparing against the raw
    // request would read a coalesced replay as a conflict.
    let coalesced = durable_occurrences(
        transaction,
        repository,
        request.events(),
        Some(expected_replay_generation),
    )
    .await?;
    let expected = request
        .events()
        .iter()
        .filter(|occurrence| is_new_occurrence(&coalesced, occurrence))
        .collect::<Vec<_>>();
    // Every requested occurrence is accounted for: one this generation stored is
    // compared on its whole event value, candidate identity included, while one
    // it coalesced was just proven durable in an earlier generation under the
    // same identity and identified content. A coalesced occurrence's own
    // candidate identity is not compared, because it was never written — the
    // fact is durable under the identity of the occurrence that first recorded
    // it, so a fresh candidate has nothing to be checked against.
    let exact_events = stored.len() == expected.len()
        && stored.iter().zip(expected).all(|(stored, requested)| {
            stored.event == *requested.event()
                && stored.content_identity == requested.content_identity()
        });
    if exact_events {
        Ok(Some(replayed))
    } else {
        Ok(None)
    }
}

/// Whether this occurrence still has to be written.
///
/// An occurrence already durable under the same identity *and* the same content
/// is the one that was recorded before, so writing it again would mint a second
/// row for a single occurrence. A provider entity that leaves the observation
/// and returns re-derives exactly that, and before this check the duplicate
/// aborted the whole cursor-and-event transaction and stalled the repository.
///
/// An occurrence whose identity is durable under different content is not that
/// occurrence. It stays in the batch and the durable unique constraint rejects
/// it, because a content identity that does not identify its content is the
/// failure this design exists to prevent.
fn is_new_occurrence(
    durable: &HashMap<RepoWatchEventContentIdentityV1, RepoWatchEvent>,
    occurrence: &RepoWatchEventOccurrenceV1,
) -> bool {
    match durable.get(&occurrence.content_identity()) {
        Some(stored) => !is_same_occurrence(stored, occurrence.event()),
        None => true,
    }
}

/// Whether two events already known to share a content identity agree on the
/// content that identity is derived from.
///
/// Only ever asked after a lookup by content identity, which is the precondition
/// that makes the answer meaningful: identified content alone does not separate
/// two occurrences of one recurring fact, because their sequences do.
///
/// Delegated to the application crate, which frames this content with the same
/// function the identity is computed over. Comparing whole events here instead
/// would let storage disagree with the identity it is coalescing on — a workflow
/// renamed while its run was out of the observation restates its identity but
/// not its display name, and the disagreement would abort the commit on the
/// durable unique constraint.
fn is_same_occurrence(stored: &RepoWatchEvent, derived: &RepoWatchEvent) -> bool {
    repo_watch_events_have_equal_identified_content(stored, derived)
}

/// The already-durable occurrences among these, by content identity.
///
/// `before` bounds the search to generations earlier than the given one, which
/// is how replay detection reconstructs the batch a past commit would have
/// stored rather than reading a coalesced replay as a conflict.
async fn durable_occurrences(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    events: &[RepoWatchEventOccurrenceV1],
    before: Option<RepoWatchCursorGeneration>,
) -> Result<HashMap<RepoWatchEventContentIdentityV1, RepoWatchEvent>, RepoWatchStoreError> {
    if events.is_empty() {
        return Ok(HashMap::new());
    }
    let requested = events
        .iter()
        .map(|occurrence| occurrence.content_identity().as_bytes().to_vec())
        .collect::<Vec<_>>();
    let rows = sqlx::query_as::<_, EventRow>(EVENT_BY_CONTENT_IDENTITY_SQL)
        .bind(repository.as_str())
        .bind(EVENT_CONTENT_IDENTITY_VERSION_V1)
        .bind(&requested)
        .bind(before.map(generation_to_i64))
        .fetch_all(&mut **transaction)
        .await?;
    let mut durable = HashMap::with_capacity(rows.len());
    for row in rows {
        let bytes: [u8; 32] = row.content_identity.as_slice().try_into().map_err(|_| {
            RepoWatchStoreError::from(RepoWatchPersistenceCorruption::InvalidEventContentIdentity)
        })?;
        let identity = RepoWatchEventContentIdentityV1::from_bytes(bytes);
        let positioned = decode_positioned_event(repository, row)?;
        durable.insert(identity, positioned.event);
    }
    Ok(durable)
}

fn validate_event_batch(
    repository: &RepositorySlug,
    events: &[RepoWatchEventOccurrenceV1],
) -> Result<(), RepoWatchStoreError> {
    if events.len() > i32::MAX as usize {
        return Err(RepoWatchStoreError::EventBatchTooLarge);
    }
    let mut identities = HashSet::with_capacity(events.len());
    let mut content_identities = HashSet::with_capacity(events.len());
    for occurrence in events {
        let event = occurrence.event();
        if event.repository() != repository {
            return Err(RepoWatchStoreError::EventRepositoryMismatch);
        }
        if !identities.insert(event.id()) {
            return Err(RepoWatchStoreError::DuplicateEventIdentity(event.id()));
        }
        if !content_identities.insert(occurrence.content_identity()) {
            return Err(RepoWatchStoreError::DuplicateEventContentIdentity(
                occurrence.content_identity(),
            ));
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorRecord {
    storage_version: u64,
    signal_reviewers: Vec<String>,
    event_identity_frontier: Vec<EventIdentityFrontierRecord>,
    state: RepositoryStateRecord,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventIdentityFrontierRecord {
    stream_identity: [u8; 32],
    sequence: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryStateRecord {
    pull_requests: Vec<PullRequestStateRecord>,
    workflow_runs: Vec<WorkflowRunRecord>,
    branch_heads: Vec<BranchHeadRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullRequestStateRecord {
    number: u64,
    head_sha: String,
    head_repository: String,
    base_branch: String,
    head_branch: String,
    title: String,
    body: String,
    labels: Vec<String>,
    draft: bool,
    author: Option<String>,
    lifecycle: String,
    mergeable_state: String,
    completed_check_suites: Vec<CheckSuiteRecord>,
    completed_check_runs: Vec<CheckRunRecord>,
    reviews: Vec<ReviewRecord>,
    threads: Vec<ThreadRecord>,
    reactions: Vec<ReactionRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckSuiteRecord {
    id: u64,
    completion_generation: String,
    outcome: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRunRecord {
    id: u64,
    completion_generation: String,
    name: String,
    conclusion: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRecord {
    id: u64,
    reviewer: String,
    state: Option<String>,
    commit: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadRecord {
    thread: String,
    state: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactionRecord {
    subject_kind: String,
    subject_id: Option<u64>,
    reactor: String,
    content: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowRunRecord {
    id: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_workflow_id",
        skip_serializing_if = "Option::is_none"
    )]
    workflow_id: Option<u64>,
    attempt: u64,
    branch: String,
    workflow: String,
    conclusion: String,
}

fn deserialize_optional_workflow_id<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    u64::deserialize(deserializer).map(Some)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchHeadRecord {
    branch: String,
    head: String,
}

fn encode_cursor_candidate(
    candidate: &RepoWatchCursorCandidate,
) -> Result<Value, RepoWatchStoreError> {
    serde_json::to_value(cursor_record(candidate)).map_err(RepoWatchStoreError::CursorEncoding)
}

fn encode_legacy_cursor_candidate(
    candidate: &RepoWatchCursorCandidate,
) -> Result<Value, RepoWatchStoreError> {
    let mut record = cursor_record(candidate);
    for run in &mut record.state.workflow_runs {
        run.workflow_id = None;
    }
    record.state.workflow_runs.sort_by(|left, right| {
        (&left.branch, &left.workflow).cmp(&(&right.branch, &right.workflow))
    });
    if record
        .state
        .workflow_runs
        .windows(2)
        .any(|runs| runs[0].branch == runs[1].branch && runs[0].workflow == runs[1].workflow)
    {
        return Err(RepoWatchPersistenceCorruption::NonCanonicalCursor.into());
    }
    serde_json::to_value(record).map_err(RepoWatchStoreError::CursorEncoding)
}

fn cursor_record(candidate: &RepoWatchCursorCandidate) -> CursorRecord {
    CursorRecord {
        storage_version: CURSOR_STORAGE_VERSION,
        signal_reviewers: candidate
            .observation()
            .signal_reviewers()
            .iter()
            .map(|reviewer| reviewer.as_str().to_owned())
            .collect(),
        event_identity_frontier: candidate
            .event_identity_frontier()
            .entries()
            .map(|entry| EventIdentityFrontierRecord {
                stream_identity: *entry.stream_identity(),
                sequence: entry.sequence().get(),
            })
            .collect(),
        state: repository_state_record(candidate.observation().state()),
    }
}

fn repository_state_record(state: &RepoWatchRepositoryState) -> RepositoryStateRecord {
    RepositoryStateRecord {
        pull_requests: state
            .pull_requests()
            .iter()
            .map(pull_request_state_record)
            .collect(),
        workflow_runs: state
            .workflow_runs()
            .iter()
            .map(|run| WorkflowRunRecord {
                id: run.id().get(),
                workflow_id: Some(run.workflow_id().get()),
                attempt: run.attempt().get(),
                branch: run.branch().as_str().to_owned(),
                workflow: run.workflow().as_str().to_owned(),
                conclusion: repo_watch_check_conclusion_to_str(run.conclusion()).to_owned(),
            })
            .collect(),
        branch_heads: state
            .branch_heads()
            .iter()
            .map(|head| BranchHeadRecord {
                branch: head.branch().as_str().to_owned(),
                head: head.head().as_str().to_owned(),
            })
            .collect(),
    }
}

fn pull_request_state_record(state: &RepoWatchPullRequestState) -> PullRequestStateRecord {
    let context = state.context();
    PullRequestStateRecord {
        number: context.number().get(),
        head_sha: context.head_sha().as_str().to_owned(),
        head_repository: context.head_repository().as_str().to_owned(),
        base_branch: context.base_branch().as_str().to_owned(),
        head_branch: context.head_branch().as_str().to_owned(),
        title: context.title().as_str().to_owned(),
        body: context.body().as_str().to_owned(),
        labels: context
            .labels()
            .iter()
            .map(|label| label.as_str().to_owned())
            .collect(),
        draft: context.draft(),
        author: context.author().map(|author| author.as_str().to_owned()),
        lifecycle: repo_watch_pull_request_lifecycle_to_str(state.lifecycle()).to_owned(),
        mergeable_state: repo_watch_mergeable_state_to_str(state.mergeable_state()).to_owned(),
        completed_check_suites: state
            .completed_check_suites()
            .iter()
            .map(|suite| CheckSuiteRecord {
                id: suite.id().get(),
                completion_generation: suite.completion_generation().as_str().to_owned(),
                outcome: repo_watch_checks_outcome_to_str(suite.outcome()).to_owned(),
            })
            .collect(),
        completed_check_runs: state
            .completed_check_runs()
            .iter()
            .map(|run| CheckRunRecord {
                id: run.id().get(),
                completion_generation: run.completion_generation().as_str().to_owned(),
                name: run.name().as_str().to_owned(),
                conclusion: repo_watch_check_conclusion_to_str(run.conclusion()).to_owned(),
            })
            .collect(),
        reviews: state
            .reviews()
            .iter()
            .map(|review| ReviewRecord {
                id: review.id().get(),
                reviewer: review.reviewer().as_str().to_owned(),
                state: review
                    .state()
                    .map(repo_watch_review_state_to_str)
                    .map(str::to_owned),
                commit: review.commit().as_str().to_owned(),
            })
            .collect(),
        threads: state
            .threads()
            .iter()
            .map(|thread| ThreadRecord {
                thread: thread.thread().as_str().to_owned(),
                state: repo_watch_thread_state_to_str(thread.state()).to_owned(),
            })
            .collect(),
        reactions: state
            .reactions()
            .iter()
            .map(|reaction| {
                let (kind, id) = repo_watch_reaction_subject_to_storage(reaction.subject());
                ReactionRecord {
                    subject_kind: repo_watch_reaction_subject_kind_to_str(kind).to_owned(),
                    subject_id: id,
                    reactor: reaction.reactor().as_str().to_owned(),
                    content: reaction.content().as_str().to_owned(),
                }
            })
            .collect(),
    }
}

fn decode_cursor_candidate(value: Value) -> Result<RepoWatchCursorCandidate, RepoWatchStoreError> {
    let mut record: CursorRecord = serde_json::from_value(value.clone()).map_err(|_| {
        RepoWatchStoreError::Corruption(RepoWatchPersistenceCorruption::MalformedCursorDocument)
    })?;
    if record.storage_version != CURSOR_STORAGE_VERSION {
        return Err(RepoWatchPersistenceCorruption::UnsupportedCursorVersion.into());
    }
    let legacy_workflow_shape = record
        .state
        .workflow_runs
        .iter()
        .all(|run| run.workflow_id.is_none());
    let current_workflow_shape = record
        .state
        .workflow_runs
        .iter()
        .all(|run| run.workflow_id.is_some());
    if !legacy_workflow_shape && !current_workflow_shape {
        return Err(RepoWatchPersistenceCorruption::NonCanonicalCursor.into());
    }
    for run in &mut record.state.workflow_runs {
        if run.workflow_id.is_none() {
            run.workflow_id = Some(run.id);
        }
    }
    let signal_reviewers = record
        .signal_reviewers
        .into_iter()
        .map(RepoWatchAuthorLogin::try_new)
        .collect::<Result<Vec<_>, _>>()?;
    let event_identity_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(
        record
            .event_identity_frontier
            .into_iter()
            .map(|entry| {
                NonZeroU64::new(entry.sequence)
                    .map(|sequence| {
                        RepoWatchEventIdentityFrontierEntryV1::new(entry.stream_identity, sequence)
                    })
                    .ok_or(RepoWatchPersistenceCorruption::InvalidCursorField(
                        "event_identity_frontier.sequence",
                    ))
            })
            .collect::<Result<Vec<_>, _>>()?,
    )
    .map_err(|_| RepoWatchPersistenceCorruption::InvalidCursorField("event_identity_frontier"))?;
    let state = decode_repository_state(record.state)?;
    let candidate = RepoWatchCursorCandidate::with_event_identity_frontier(
        RepoWatchObservation::new(signal_reviewers, state),
        event_identity_frontier,
    );
    let canonical = if legacy_workflow_shape {
        encode_legacy_cursor_candidate(&candidate)?
    } else {
        encode_cursor_candidate(&candidate)?
    };
    if canonical != value {
        return Err(RepoWatchPersistenceCorruption::NonCanonicalCursor.into());
    }
    Ok(candidate)
}

fn decode_repository_state(
    record: RepositoryStateRecord,
) -> Result<RepoWatchRepositoryState, RepoWatchStoreError> {
    let pull_requests = record
        .pull_requests
        .into_iter()
        .map(decode_pull_request_state)
        .collect::<Result<Vec<_>, _>>()?;
    let workflow_runs = record
        .workflow_runs
        .into_iter()
        .map(|run| {
            Ok(RepoWatchWorkflowRunObservation::new(
                github_object_id(run.id, "workflow_run.id")?,
                github_object_id(
                    run.workflow_id
                        .ok_or(RepoWatchPersistenceCorruption::InvalidCursorField(
                            "workflow_run.workflow_id",
                        ))?,
                    "workflow_run.workflow_id",
                )?,
                NonZeroU64::new(run.attempt)
                    .map(RepoWatchWorkflowRunAttempt::new)
                    .ok_or(RepoWatchPersistenceCorruption::InvalidCursorField(
                        "workflow_run.attempt",
                    ))?,
                BranchName::try_new(run.branch)?,
                WorkflowName::try_new(run.workflow)?,
                repo_watch_check_conclusion_from_str(&run.conclusion).ok_or(
                    RepoWatchPersistenceCorruption::UnknownCursorDiscriminator(
                        "workflow_run.conclusion",
                    ),
                )?,
            ))
        })
        .collect::<Result<Vec<_>, RepoWatchStoreError>>()?;
    let branch_heads = record
        .branch_heads
        .into_iter()
        .map(|head| {
            Ok(RepoWatchBranchHead::new(
                BranchName::try_new(head.branch)?,
                CommitSha::try_new(head.head)?,
            ))
        })
        .collect::<Result<Vec<_>, RepoWatchStoreError>>()?;
    RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
        pull_requests,
        workflow_runs,
        branch_heads,
    })
    .map_err(|_| RepoWatchPersistenceCorruption::InvalidCursorField("repository_state").into())
}

fn decode_pull_request_state(
    record: PullRequestStateRecord,
) -> Result<RepoWatchPullRequestState, RepoWatchStoreError> {
    let context = PullRequestEventContext::new(PullRequestEventContextInput {
        number: pull_request_number(record.number, "pull_request.number")?,
        head_sha: CommitSha::try_new(record.head_sha)?,
        head_repository: RepositorySlug::try_new(record.head_repository)?,
        base_branch: BranchName::try_new(record.base_branch)?,
        head_branch: BranchName::try_new(record.head_branch)?,
        title: PullRequestTitle::try_new(record.title)?,
        body: PullRequestBody::try_new(record.body)?,
        labels: record
            .labels
            .into_iter()
            .map(LabelName::try_new)
            .collect::<Result<Vec<_>, _>>()?,
        draft: record.draft,
        author: record
            .author
            .map(RepoWatchAuthorLogin::try_new)
            .transpose()?,
    });
    let completed_check_suites = record
        .completed_check_suites
        .into_iter()
        .map(|suite| {
            Ok(RepoWatchCheckSuiteObservation::new(
                github_object_id(suite.id, "check_suite.id")?,
                RepoWatchCheckCompletionGeneration::try_new(suite.completion_generation).map_err(
                    |_| {
                        RepoWatchPersistenceCorruption::InvalidCursorField(
                            "check_suite.completion_generation",
                        )
                    },
                )?,
                repo_watch_checks_outcome_from_str(&suite.outcome).ok_or(
                    RepoWatchPersistenceCorruption::UnknownCursorDiscriminator(
                        "check_suite.outcome",
                    ),
                )?,
            ))
        })
        .collect::<Result<Vec<_>, RepoWatchStoreError>>()?;
    let completed_check_runs = record
        .completed_check_runs
        .into_iter()
        .map(|run| {
            Ok(RepoWatchCheckRunObservation::new(
                github_object_id(run.id, "check_run.id")?,
                RepoWatchCheckCompletionGeneration::try_new(run.completion_generation).map_err(
                    |_| {
                        RepoWatchPersistenceCorruption::InvalidCursorField(
                            "check_run.completion_generation",
                        )
                    },
                )?,
                CheckRunName::try_new(run.name)?,
                repo_watch_check_conclusion_from_str(&run.conclusion).ok_or(
                    RepoWatchPersistenceCorruption::UnknownCursorDiscriminator(
                        "check_run.conclusion",
                    ),
                )?,
            ))
        })
        .collect::<Result<Vec<_>, RepoWatchStoreError>>()?;
    let reviews = record
        .reviews
        .into_iter()
        .map(|review| {
            Ok(RepoWatchReviewObservation::new(
                github_object_id(review.id, "review.id")?,
                RepoWatchAuthorLogin::try_new(review.reviewer)?,
                review
                    .state
                    .map(|state| {
                        repo_watch_review_state_from_str(&state).ok_or(
                            RepoWatchPersistenceCorruption::UnknownCursorDiscriminator(
                                "review.state",
                            ),
                        )
                    })
                    .transpose()?,
                CommitSha::try_new(review.commit)?,
            ))
        })
        .collect::<Result<Vec<_>, RepoWatchStoreError>>()?;
    let threads = record
        .threads
        .into_iter()
        .map(|thread| {
            Ok(RepoWatchThreadObservation::new(
                ReviewThreadId::try_new(thread.thread)?,
                repo_watch_thread_state_from_str(&thread.state).ok_or(
                    RepoWatchPersistenceCorruption::UnknownCursorDiscriminator("thread.state"),
                )?,
            ))
        })
        .collect::<Result<Vec<_>, RepoWatchStoreError>>()?;
    let reactions = record
        .reactions
        .into_iter()
        .map(decode_reaction_record)
        .collect::<Result<Vec<_>, _>>()?;
    RepoWatchPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context,
        lifecycle: repo_watch_pull_request_lifecycle_from_str(&record.lifecycle).ok_or(
            RepoWatchPersistenceCorruption::UnknownCursorDiscriminator("pull_request.lifecycle"),
        )?,
        mergeable_state: repo_watch_mergeable_state_from_str(&record.mergeable_state).ok_or(
            RepoWatchPersistenceCorruption::UnknownCursorDiscriminator(
                "pull_request.mergeable_state",
            ),
        )?,
        completed_check_suites,
        completed_check_runs,
        reviews,
        threads,
        reactions,
    })
    .map_err(|_| RepoWatchPersistenceCorruption::InvalidCursorField("pull_request").into())
}

fn decode_reaction_record(
    record: ReactionRecord,
) -> Result<RepoWatchReactionObservation, RepoWatchStoreError> {
    let kind = repo_watch_reaction_subject_kind_from_str(&record.subject_kind).ok_or(
        RepoWatchPersistenceCorruption::UnknownCursorDiscriminator("reaction.subject_kind"),
    )?;
    let subject = decode_reaction_subject(kind, record.subject_id, "reaction.subject_id")?;
    Ok(RepoWatchReactionObservation::new(
        subject,
        RepoWatchAuthorLogin::try_new(record.reactor)?,
        signalbox_domain::ReactionContent::try_new(record.content)?,
    ))
}

fn decode_reaction_subject(
    kind: RepoWatchReactionSubjectStorageKind,
    id: Option<u64>,
    field: &'static str,
) -> Result<ReactionSubject, RepoWatchStoreError> {
    match (kind, id) {
        (RepoWatchReactionSubjectStorageKind::PullRequestBody, None) => {
            Ok(ReactionSubject::PullRequestBody)
        }
        (RepoWatchReactionSubjectStorageKind::IssueComment, Some(id)) => {
            Ok(ReactionSubject::IssueComment {
                id: github_object_id(id, field)?,
            })
        }
        (RepoWatchReactionSubjectStorageKind::ReviewComment, Some(id)) => {
            Ok(ReactionSubject::ReviewComment {
                id: github_object_id(id, field)?,
            })
        }
        (RepoWatchReactionSubjectStorageKind::PullRequestBody, Some(_))
        | (RepoWatchReactionSubjectStorageKind::IssueComment, None)
        | (RepoWatchReactionSubjectStorageKind::ReviewComment, None) => {
            Err(RepoWatchPersistenceCorruption::InvalidCursorField(field).into())
        }
    }
}

fn github_object_id(
    value: u64,
    field: &'static str,
) -> Result<GitHubObjectId, RepoWatchStoreError> {
    NonZeroU64::new(value)
        .map(GitHubObjectId::new)
        .ok_or_else(|| RepoWatchPersistenceCorruption::InvalidCursorField(field).into())
}

fn pull_request_number(
    value: u64,
    field: &'static str,
) -> Result<PullRequestNumber, RepoWatchStoreError> {
    NonZeroU64::new(value)
        .map(PullRequestNumber::new)
        .ok_or_else(|| RepoWatchPersistenceCorruption::InvalidCursorField(field).into())
}

// Event row encoding and decoding follows below; keeping it in this module
// leaves the normalized cursor record distinct from the append-only fact row.

#[derive(Debug, PartialEq)]
struct EncodedEvent {
    event_id: Uuid,
    repository: String,
    event_version: i16,
    target_kind: &'static str,
    event_kind: &'static str,
    pull_request_number: Option<Decimal>,
    head_sha: Option<String>,
    head_repository: Option<String>,
    base_branch: Option<String>,
    head_branch: Option<String>,
    title: Option<String>,
    body: Option<String>,
    labels: Option<Vec<String>>,
    draft: Option<bool>,
    author: Option<String>,
    previous_sha: Option<String>,
    current_sha: Option<String>,
    mergeable_state: Option<&'static str>,
    checks_outcome: Option<&'static str>,
    check_run_name: Option<String>,
    conclusion: Option<&'static str>,
    workflow_branch: Option<String>,
    workflow_name: Option<String>,
    review_reviewer: Option<String>,
    review_state: Option<&'static str>,
    review_commit: Option<String>,
    thread_id: Option<String>,
    label_name: Option<String>,
    advanced_branch: Option<String>,
    reaction_subject_kind: Option<&'static str>,
    reaction_subject_id: Option<Decimal>,
    reaction_reactor: Option<String>,
    reaction_content: Option<String>,
    reaction_change: Option<&'static str>,
}

impl EncodedEvent {
    fn from_event(event: &RepoWatchEvent) -> Self {
        let mut encoded = Self {
            event_id: *event.id().as_uuid(),
            repository: event.repository().as_str().to_owned(),
            event_version: EVENT_VERSION_V1,
            target_kind: repo_watch_event_target_to_str(match event.target() {
                RepoWatchEventTarget::PullRequest(_) => {
                    RepoWatchEventTargetStorageKind::PullRequest
                }
                RepoWatchEventTarget::Branch => RepoWatchEventTargetStorageKind::Branch,
            }),
            event_kind: repo_watch_event_kind_to_str(event.kind().name()),
            pull_request_number: None,
            head_sha: None,
            head_repository: None,
            base_branch: None,
            head_branch: None,
            title: None,
            body: None,
            labels: None,
            draft: None,
            author: None,
            previous_sha: None,
            current_sha: None,
            mergeable_state: None,
            checks_outcome: None,
            check_run_name: None,
            conclusion: None,
            workflow_branch: None,
            workflow_name: None,
            review_reviewer: None,
            review_state: None,
            review_commit: None,
            thread_id: None,
            label_name: None,
            advanced_branch: None,
            reaction_subject_kind: None,
            reaction_subject_id: None,
            reaction_reactor: None,
            reaction_content: None,
            reaction_change: None,
        };
        match event.target() {
            RepoWatchEventTarget::PullRequest(context) => {
                encoded.pull_request_number = Some(Decimal::from(context.number().get()));
                encoded.head_sha = Some(context.head_sha().as_str().to_owned());
                encoded.head_repository = Some(context.head_repository().as_str().to_owned());
                encoded.base_branch = Some(context.base_branch().as_str().to_owned());
                encoded.head_branch = Some(context.head_branch().as_str().to_owned());
                encoded.title = Some(context.title().as_str().to_owned());
                encoded.body = Some(context.body().as_str().to_owned());
                encoded.labels = Some(
                    context
                        .labels()
                        .iter()
                        .map(|label| label.as_str().to_owned())
                        .collect(),
                );
                encoded.draft = Some(context.draft());
                encoded.author = context.author().map(|author| author.as_str().to_owned());
            }
            RepoWatchEventTarget::Branch => {}
        }
        match event.kind() {
            RepoWatchEventKindV1::PullRequestOpened
            | RepoWatchEventKindV1::PullRequestClosed
            | RepoWatchEventKindV1::PullRequestMerged => {}
            RepoWatchEventKindV1::HeadChanged { previous, current } => {
                encoded.previous_sha = Some(previous.as_str().to_owned());
                encoded.current_sha = Some(current.as_str().to_owned());
            }
            RepoWatchEventKindV1::MergeableStateChanged { current } => {
                encoded.mergeable_state = Some(repo_watch_mergeable_state_to_str(*current));
            }
            RepoWatchEventKindV1::ChecksCompleted { outcome } => {
                encoded.checks_outcome = Some(repo_watch_checks_outcome_to_str(*outcome));
            }
            RepoWatchEventKindV1::CheckRunCompleted { name, conclusion } => {
                encoded.check_run_name = Some(name.as_str().to_owned());
                encoded.conclusion = Some(repo_watch_check_conclusion_to_str(*conclusion));
            }
            RepoWatchEventKindV1::BranchWorkflowRunCompleted {
                branch,
                workflow,
                conclusion,
            } => {
                encoded.workflow_branch = Some(branch.as_str().to_owned());
                encoded.workflow_name = Some(workflow.as_str().to_owned());
                encoded.conclusion = Some(repo_watch_check_conclusion_to_str(*conclusion));
            }
            RepoWatchEventKindV1::ReviewSubmitted {
                reviewer,
                state,
                commit,
            } => {
                encoded.review_reviewer = Some(reviewer.as_str().to_owned());
                encoded.review_state = Some(repo_watch_review_state_to_str(*state));
                encoded.review_commit = Some(commit.as_str().to_owned());
            }
            RepoWatchEventKindV1::ThreadOpened { thread }
            | RepoWatchEventKindV1::ThreadResolved { thread } => {
                encoded.thread_id = Some(thread.as_str().to_owned());
            }
            RepoWatchEventKindV1::Labeled { label } | RepoWatchEventKindV1::Unlabeled { label } => {
                encoded.label_name = Some(label.as_str().to_owned());
            }
            RepoWatchEventKindV1::BaseAdvanced { branch } => {
                encoded.advanced_branch = Some(branch.as_str().to_owned());
            }
            RepoWatchEventKindV1::ReactionChanged {
                subject,
                reactor,
                content,
                change,
            } => {
                let (kind, id) = repo_watch_reaction_subject_to_storage(*subject);
                encoded.reaction_subject_kind = Some(repo_watch_reaction_subject_kind_to_str(kind));
                encoded.reaction_subject_id = id.map(Decimal::from);
                encoded.reaction_reactor = Some(reactor.as_str().to_owned());
                encoded.reaction_content = Some(content.as_str().to_owned());
                encoded.reaction_change = Some(repo_watch_reaction_change_to_str(*change));
            }
        }
        encoded
    }
}

async fn insert_events(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    generation: RepoWatchCursorGeneration,
    events: &[&RepoWatchEventOccurrenceV1],
) -> Result<(), RepoWatchStoreError> {
    for (index, occurrence) in events.iter().enumerate() {
        let ordinal =
            i32::try_from(index + 1).map_err(|_| RepoWatchStoreError::EventBatchTooLarge)?;
        let event = occurrence.event();
        let encoded = EncodedEvent::from_event(event);
        sqlx::query(
            "INSERT INTO repo_watch_event (
                event_id, repository, cursor_generation, event_ordinal, event_version,
                content_identity_version, content_identity, producer,
                target_kind, event_kind,
                pull_request_number, head_sha, head_repository, base_branch,
                head_branch, title, body, labels, draft, author,
                previous_sha, current_sha, mergeable_state, checks_outcome,
                check_run_name, conclusion, workflow_branch, workflow_name,
                review_reviewer, review_state, review_commit, thread_id,
                label_name, advanced_branch, reaction_subject_kind,
                reaction_subject_id, reaction_reactor, reaction_content,
                reaction_change
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7,
                $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                $18, $19, $20, $21, $22, $23, $24, $25, $26, $27,
                $28, $29, $30, $31, $32, $33, $34, $35, $36, $37,
                $38, $39
             )",
        )
        .bind(encoded.event_id)
        .bind(repository.as_str())
        .bind(generation_to_i64(generation))
        .bind(ordinal)
        .bind(encoded.event_version)
        .bind(EVENT_CONTENT_IDENTITY_VERSION_V1)
        .bind(occurrence.content_identity().as_bytes().as_slice())
        .bind(repo_watch_event_producer_to_str(
            RepoWatchEventProducerStorageKind::Poll,
        ))
        .bind(encoded.target_kind)
        .bind(encoded.event_kind)
        .bind(encoded.pull_request_number)
        .bind(encoded.head_sha)
        .bind(encoded.head_repository)
        .bind(encoded.base_branch)
        .bind(encoded.head_branch)
        .bind(encoded.title)
        .bind(encoded.body)
        .bind(encoded.labels)
        .bind(encoded.draft)
        .bind(encoded.author)
        .bind(encoded.previous_sha)
        .bind(encoded.current_sha)
        .bind(encoded.mergeable_state)
        .bind(encoded.checks_outcome)
        .bind(encoded.check_run_name)
        .bind(encoded.conclusion)
        .bind(encoded.workflow_branch)
        .bind(encoded.workflow_name)
        .bind(encoded.review_reviewer)
        .bind(encoded.review_state)
        .bind(encoded.review_commit)
        .bind(encoded.thread_id)
        .bind(encoded.label_name)
        .bind(encoded.advanced_branch)
        .bind(encoded.reaction_subject_kind)
        .bind(encoded.reaction_subject_id)
        .bind(encoded.reaction_reactor)
        .bind(encoded.reaction_content)
        .bind(encoded.reaction_change)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn record_committed_webhook_terminal(
    transaction: &mut Transaction<'_, Postgres>,
    delivery: RepoWatchWebhookDeliveryKey,
    projections: Vec<RepoWatchWebhookProjection>,
    generation: RepoWatchCursorGeneration,
) -> Result<(), RepoWatchStoreError> {
    let request = RepoWatchWebhookTerminalRequest::try_new(
        projections,
        RepoWatchWebhookDisposition::Committed(generation),
        None,
    )
    .map_err(|_| RepoWatchStoreError::InvalidWebhookCommit)?;
    match record_terminal_in_transaction(transaction, delivery, &request).await? {
        RepoWatchWebhookTerminalOutcome::Recorded
        | RepoWatchWebhookTerminalOutcome::AlreadyTerminal => Ok(()),
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EventRow {
    event_id: Uuid,
    repository: String,
    cursor_generation: i64,
    event_ordinal: i32,
    event_version: i16,
    content_identity_version: i16,
    content_identity: Vec<u8>,
    producer: String,
    target_kind: String,
    event_kind: String,
    pull_request_number: Option<Decimal>,
    head_sha: Option<String>,
    head_repository: Option<String>,
    base_branch: Option<String>,
    head_branch: Option<String>,
    title: Option<String>,
    body: Option<String>,
    labels: Option<Vec<String>>,
    draft: Option<bool>,
    author: Option<String>,
    previous_sha: Option<String>,
    current_sha: Option<String>,
    mergeable_state: Option<String>,
    checks_outcome: Option<String>,
    check_run_name: Option<String>,
    conclusion: Option<String>,
    workflow_branch: Option<String>,
    workflow_name: Option<String>,
    review_reviewer: Option<String>,
    review_state: Option<String>,
    review_commit: Option<String>,
    thread_id: Option<String>,
    label_name: Option<String>,
    advanced_branch: Option<String>,
    reaction_subject_kind: Option<String>,
    reaction_subject_id: Option<Decimal>,
    reaction_reactor: Option<String>,
    reaction_content: Option<String>,
    reaction_change: Option<String>,
}

const EVENT_PAGE_SQL: &str = "SELECT event_id, repository, cursor_generation, event_ordinal,
        event_version, content_identity_version, content_identity, producer,
        target_kind, event_kind,
        pull_request_number, head_sha, head_repository, base_branch,
        head_branch, title, body, labels, draft, author,
        previous_sha, current_sha, mergeable_state, checks_outcome,
        check_run_name, conclusion, workflow_branch, workflow_name,
        review_reviewer, review_state, review_commit, thread_id,
        label_name, advanced_branch, reaction_subject_kind,
        reaction_subject_id, reaction_reactor, reaction_content,
        reaction_change
   FROM repo_watch_event
  WHERE repository = $1
    AND (
        $2::bigint IS NULL
        OR cursor_generation > $2
        OR (cursor_generation = $2 AND event_ordinal > $3::integer)
    )
  ORDER BY cursor_generation, event_ordinal
  LIMIT $4";

const EVENT_GENERATION_SQL: &str = "SELECT event_id, repository, cursor_generation, event_ordinal,
        event_version, content_identity_version, content_identity, producer,
        target_kind, event_kind,
        pull_request_number, head_sha, head_repository, base_branch,
        head_branch, title, body, labels, draft, author,
        previous_sha, current_sha, mergeable_state, checks_outcome,
        check_run_name, conclusion, workflow_branch, workflow_name,
        review_reviewer, review_state, review_commit, thread_id,
        label_name, advanced_branch, reaction_subject_kind,
        reaction_subject_id, reaction_reactor, reaction_content,
        reaction_change
   FROM repo_watch_event
  WHERE repository = $1 AND cursor_generation = $2
  ORDER BY event_ordinal";

const EVENT_BY_CONTENT_IDENTITY_SQL: &str = "SELECT event_id, repository, cursor_generation,
        event_ordinal, event_version, content_identity_version, content_identity, producer,
        target_kind, event_kind,
        pull_request_number, head_sha, head_repository, base_branch,
        head_branch, title, body, labels, draft, author,
        previous_sha, current_sha, mergeable_state, checks_outcome,
        check_run_name, conclusion, workflow_branch, workflow_name,
        review_reviewer, review_state, review_commit, thread_id,
        label_name, advanced_branch, reaction_subject_kind,
        reaction_subject_id, reaction_reactor, reaction_content,
        reaction_change
   FROM repo_watch_event
  WHERE repository = $1
    AND content_identity_version = $2
    AND content_identity = ANY($3)
    AND ($4::bigint IS NULL OR cursor_generation < $4)";

const EVENT_BY_ID_SQL: &str = "SELECT event_id, repository, cursor_generation, event_ordinal,
        event_version, content_identity_version, content_identity, producer,
        target_kind, event_kind,
        pull_request_number, head_sha, head_repository, base_branch,
        head_branch, title, body, labels, draft, author,
        previous_sha, current_sha, mergeable_state, checks_outcome,
        check_run_name, conclusion, workflow_branch, workflow_name,
        review_reviewer, review_state, review_commit, thread_id,
        label_name, advanced_branch, reaction_subject_kind,
        reaction_subject_id, reaction_reactor, reaction_content,
        reaction_change
   FROM repo_watch_event
  WHERE repository = $1 AND event_id = $2";

async fn load_event_rows(
    pool: &PgPool,
    repository: &RepositorySlug,
    after: Option<RepoWatchEventPosition>,
    limit: usize,
) -> Result<Vec<EventRow>, RepoWatchStoreError> {
    sqlx::query_as::<_, EventRow>(EVENT_PAGE_SQL)
        .bind(repository.as_str())
        .bind(after.map(|position| generation_to_i64(position.generation())))
        .bind(after.map(|position| position.ordinal().get() as i32))
        .bind(i64::try_from(limit).map_err(|_| RepoWatchStoreError::EventBatchTooLarge)?)
        .fetch_all(pool)
        .await
        .map_err(RepoWatchStoreError::Database)
}

async fn load_generation_events_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    generation: RepoWatchCursorGeneration,
) -> Result<Vec<StoredEventOccurrence>, RepoWatchStoreError> {
    let rows = sqlx::query_as::<_, EventRow>(EVENT_GENERATION_SQL)
        .bind(repository.as_str())
        .bind(generation_to_i64(generation))
        .fetch_all(&mut **transaction)
        .await?;
    rows.into_iter()
        .map(|row| {
            if row.content_identity_version != EVENT_CONTENT_IDENTITY_VERSION_V1 {
                return Err(
                    RepoWatchPersistenceCorruption::UnsupportedEventContentIdentityVersion.into(),
                );
            }
            let bytes: [u8; 32] = row.content_identity.as_slice().try_into().map_err(|_| {
                RepoWatchStoreError::from(
                    RepoWatchPersistenceCorruption::InvalidEventContentIdentity,
                )
            })?;
            let content_identity = RepoWatchEventContentIdentityV1::from_bytes(bytes);
            let positioned = decode_positioned_event(repository, row)?;
            Ok(StoredEventOccurrence {
                event: positioned.event,
                content_identity,
            })
        })
        .collect()
}

struct StoredEventOccurrence {
    event: RepoWatchEvent,
    content_identity: RepoWatchEventContentIdentityV1,
}

fn decode_positioned_event(
    repository: &RepositorySlug,
    row: EventRow,
) -> Result<PositionedRepoWatchEvent, RepoWatchStoreError> {
    if row.repository != repository.as_str() {
        return Err(RepoWatchPersistenceCorruption::InvalidEventField("repository").into());
    }
    let generation = RepoWatchCursorGeneration::try_from_stored(row.cursor_generation)?;
    let ordinal = u32::try_from(row.event_ordinal)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or(RepoWatchPersistenceCorruption::InvalidEventPosition)?;
    if row.event_version != EVENT_VERSION_V1 {
        return Err(RepoWatchPersistenceCorruption::UnsupportedEventVersion.into());
    }
    if row.content_identity_version != EVENT_CONTENT_IDENTITY_VERSION_V1 {
        return Err(RepoWatchPersistenceCorruption::UnsupportedEventContentIdentityVersion.into());
    }
    if row.content_identity.len() != 32 {
        return Err(RepoWatchPersistenceCorruption::InvalidEventContentIdentity.into());
    }
    match repo_watch_event_producer_from_str(&row.producer) {
        Some(RepoWatchEventProducerStorageKind::Poll) => {}
        None => return Err(RepoWatchPersistenceCorruption::UnknownEventProducer.into()),
    }
    let target = repo_watch_event_target_from_str(&row.target_kind).ok_or(
        RepoWatchPersistenceCorruption::UnknownEventDiscriminator("target_kind"),
    )?;
    let kind = repo_watch_event_kind_from_str(&row.event_kind).ok_or(
        RepoWatchPersistenceCorruption::UnknownEventDiscriminator("event_kind"),
    )?;
    let event_id = RepoWatchEventId::from_uuid(row.event_id);
    let event = match target {
        RepoWatchEventTargetStorageKind::PullRequest => {
            let context = decode_event_pull_request_context(&row)?;
            let payload = decode_event_kind(kind, &row)?;
            RepoWatchEvent::try_pull_request(event_id, repository.clone(), context, payload)
                .map_err(|_| RepoWatchPersistenceCorruption::EventShapeMismatch)?
        }
        RepoWatchEventTargetStorageKind::Branch => {
            if kind != RepoWatchEventKindNameV1::BranchWorkflowRunCompleted {
                return Err(RepoWatchPersistenceCorruption::EventShapeMismatch.into());
            }
            let branch =
                BranchName::try_new(required(row.workflow_branch.clone(), "workflow_branch")?)?;
            let workflow =
                WorkflowName::try_new(required(row.workflow_name.clone(), "workflow_name")?)?;
            let conclusion = repo_watch_check_conclusion_from_str(required_ref(
                row.conclusion.as_deref(),
                "conclusion",
            )?)
            .ok_or(RepoWatchPersistenceCorruption::UnknownEventDiscriminator(
                "conclusion",
            ))?;
            RepoWatchEvent::branch_workflow(
                event_id,
                repository.clone(),
                branch,
                workflow,
                conclusion,
            )
        }
    };
    if !event_row_matches_encoded(&row, &EncodedEvent::from_event(&event)) {
        return Err(RepoWatchPersistenceCorruption::EventShapeMismatch.into());
    }
    Ok(PositionedRepoWatchEvent {
        position: RepoWatchEventPosition::new(generation, ordinal),
        event,
    })
}

fn decode_event_pull_request_context(
    row: &EventRow,
) -> Result<PullRequestEventContext, RepoWatchStoreError> {
    let number =
        positive_u64_from_numeric(required(row.pull_request_number, "pull_request_number")?)
            .ok()
            .and_then(NonZeroU64::new)
            .map(PullRequestNumber::new)
            .ok_or(RepoWatchPersistenceCorruption::InvalidEventField(
                "pull_request_number",
            ))?;
    Ok(PullRequestEventContext::new(PullRequestEventContextInput {
        number,
        head_sha: CommitSha::try_new(required(row.head_sha.clone(), "head_sha")?)?,
        head_repository: RepositorySlug::try_new(required(
            row.head_repository.clone(),
            "head_repository",
        )?)?,
        base_branch: BranchName::try_new(required(row.base_branch.clone(), "base_branch")?)?,
        head_branch: BranchName::try_new(required(row.head_branch.clone(), "head_branch")?)?,
        title: PullRequestTitle::try_new(required(row.title.clone(), "title")?)?,
        body: PullRequestBody::try_new(required(row.body.clone(), "body")?)?,
        labels: required(row.labels.clone(), "labels")?
            .into_iter()
            .map(LabelName::try_new)
            .collect::<Result<Vec<_>, _>>()?,
        draft: required(row.draft, "draft")?,
        author: row
            .author
            .clone()
            .map(RepoWatchAuthorLogin::try_new)
            .transpose()?,
    }))
}

fn decode_event_kind(
    kind: RepoWatchEventKindNameV1,
    row: &EventRow,
) -> Result<RepoWatchEventKindV1, RepoWatchStoreError> {
    match kind {
        RepoWatchEventKindNameV1::PullRequestOpened => Ok(RepoWatchEventKindV1::PullRequestOpened),
        RepoWatchEventKindNameV1::PullRequestClosed => Ok(RepoWatchEventKindV1::PullRequestClosed),
        RepoWatchEventKindNameV1::PullRequestMerged => Ok(RepoWatchEventKindV1::PullRequestMerged),
        RepoWatchEventKindNameV1::HeadChanged => Ok(RepoWatchEventKindV1::HeadChanged {
            previous: CommitSha::try_new(required(row.previous_sha.clone(), "previous_sha")?)?,
            current: CommitSha::try_new(required(row.current_sha.clone(), "current_sha")?)?,
        }),
        RepoWatchEventKindNameV1::MergeableStateChanged => {
            Ok(RepoWatchEventKindV1::MergeableStateChanged {
                current: repo_watch_mergeable_state_from_str(required_ref(
                    row.mergeable_state.as_deref(),
                    "mergeable_state",
                )?)
                .ok_or(
                    RepoWatchPersistenceCorruption::UnknownEventDiscriminator("mergeable_state"),
                )?,
            })
        }
        RepoWatchEventKindNameV1::ChecksCompleted => Ok(RepoWatchEventKindV1::ChecksCompleted {
            outcome: repo_watch_checks_outcome_from_str(required_ref(
                row.checks_outcome.as_deref(),
                "checks_outcome",
            )?)
            .ok_or(RepoWatchPersistenceCorruption::UnknownEventDiscriminator(
                "checks_outcome",
            ))?,
        }),
        RepoWatchEventKindNameV1::CheckRunCompleted => {
            Ok(RepoWatchEventKindV1::CheckRunCompleted {
                name: CheckRunName::try_new(required(
                    row.check_run_name.clone(),
                    "check_run_name",
                )?)?,
                conclusion: repo_watch_check_conclusion_from_str(required_ref(
                    row.conclusion.as_deref(),
                    "conclusion",
                )?)
                .ok_or(
                    RepoWatchPersistenceCorruption::UnknownEventDiscriminator("conclusion"),
                )?,
            })
        }
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted => {
            Err(RepoWatchPersistenceCorruption::EventShapeMismatch.into())
        }
        RepoWatchEventKindNameV1::ReviewSubmitted => Ok(RepoWatchEventKindV1::ReviewSubmitted {
            reviewer: RepoWatchAuthorLogin::try_new(required(
                row.review_reviewer.clone(),
                "review_reviewer",
            )?)?,
            state: repo_watch_review_state_from_str(required_ref(
                row.review_state.as_deref(),
                "review_state",
            )?)
            .ok_or(RepoWatchPersistenceCorruption::UnknownEventDiscriminator(
                "review_state",
            ))?,
            commit: CommitSha::try_new(required(row.review_commit.clone(), "review_commit")?)?,
        }),
        RepoWatchEventKindNameV1::ThreadOpened => Ok(RepoWatchEventKindV1::ThreadOpened {
            thread: ReviewThreadId::try_new(required(row.thread_id.clone(), "thread_id")?)?,
        }),
        RepoWatchEventKindNameV1::ThreadResolved => Ok(RepoWatchEventKindV1::ThreadResolved {
            thread: ReviewThreadId::try_new(required(row.thread_id.clone(), "thread_id")?)?,
        }),
        RepoWatchEventKindNameV1::Labeled => Ok(RepoWatchEventKindV1::Labeled {
            label: LabelName::try_new(required(row.label_name.clone(), "label_name")?)?,
        }),
        RepoWatchEventKindNameV1::Unlabeled => Ok(RepoWatchEventKindV1::Unlabeled {
            label: LabelName::try_new(required(row.label_name.clone(), "label_name")?)?,
        }),
        RepoWatchEventKindNameV1::BaseAdvanced => Ok(RepoWatchEventKindV1::BaseAdvanced {
            branch: BranchName::try_new(required(row.advanced_branch.clone(), "advanced_branch")?)?,
        }),
        RepoWatchEventKindNameV1::ReactionChanged => {
            let subject_kind = repo_watch_reaction_subject_kind_from_str(required_ref(
                row.reaction_subject_kind.as_deref(),
                "reaction_subject_kind",
            )?)
            .ok_or(RepoWatchPersistenceCorruption::UnknownEventDiscriminator(
                "reaction_subject_kind",
            ))?;
            let subject_id = row
                .reaction_subject_id
                .map(positive_u64_from_numeric)
                .transpose()
                .map_err(|_| {
                    RepoWatchPersistenceCorruption::InvalidEventField("reaction_subject_id")
                })?;
            Ok(RepoWatchEventKindV1::ReactionChanged {
                subject: decode_reaction_subject(subject_kind, subject_id, "reaction_subject_id")?,
                reactor: RepoWatchAuthorLogin::try_new(required(
                    row.reaction_reactor.clone(),
                    "reaction_reactor",
                )?)?,
                content: signalbox_domain::ReactionContent::try_new(required(
                    row.reaction_content.clone(),
                    "reaction_content",
                )?)?,
                change: repo_watch_reaction_change_from_str(required_ref(
                    row.reaction_change.as_deref(),
                    "reaction_change",
                )?)
                .ok_or(
                    RepoWatchPersistenceCorruption::UnknownEventDiscriminator("reaction_change"),
                )?,
            })
        }
    }
}

fn required<T>(value: Option<T>, field: &'static str) -> Result<T, RepoWatchPersistenceCorruption> {
    value.ok_or(RepoWatchPersistenceCorruption::InvalidEventField(field))
}

fn required_ref<'a>(
    value: Option<&'a str>,
    field: &'static str,
) -> Result<&'a str, RepoWatchPersistenceCorruption> {
    value.ok_or(RepoWatchPersistenceCorruption::InvalidEventField(field))
}

fn event_row_matches_encoded(row: &EventRow, encoded: &EncodedEvent) -> bool {
    row.event_id == encoded.event_id
        && row.repository == encoded.repository
        && row.event_version == encoded.event_version
        && row.target_kind == encoded.target_kind
        && row.event_kind == encoded.event_kind
        && row.pull_request_number == encoded.pull_request_number
        && row.head_sha == encoded.head_sha
        && row.head_repository == encoded.head_repository
        && row.base_branch == encoded.base_branch
        && row.head_branch == encoded.head_branch
        && row.title == encoded.title
        && row.body == encoded.body
        && row.labels == encoded.labels
        && row.draft == encoded.draft
        && row.author == encoded.author
        && row.previous_sha == encoded.previous_sha
        && row.current_sha == encoded.current_sha
        && row.mergeable_state.as_deref() == encoded.mergeable_state
        && row.checks_outcome.as_deref() == encoded.checks_outcome
        && row.check_run_name == encoded.check_run_name
        && row.conclusion.as_deref() == encoded.conclusion
        && row.workflow_branch == encoded.workflow_branch
        && row.workflow_name == encoded.workflow_name
        && row.review_reviewer == encoded.review_reviewer
        && row.review_state.as_deref() == encoded.review_state
        && row.review_commit == encoded.review_commit
        && row.thread_id == encoded.thread_id
        && row.label_name == encoded.label_name
        && row.advanced_branch == encoded.advanced_branch
        && row.reaction_subject_kind.as_deref() == encoded.reaction_subject_kind
        && row.reaction_subject_id == encoded.reaction_subject_id
        && row.reaction_reactor == encoded.reaction_reactor
        && row.reaction_content == encoded.reaction_content
        && row.reaction_change.as_deref() == encoded.reaction_change
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        num::{NonZeroU16, NonZeroU64},
    };

    use signalbox_domain::CheckConclusion;

    use super::*;

    const WORKFLOW_RUN_ID: u64 = 41;
    const WORKFLOW_ID: u64 = 42;
    const BRANCH: &str = "main";
    const WORKFLOW: &str = "continuous-integration";
    const LEGACY_WORKFLOW_RUN_IDS: [u64; 2] = [82, 40];
    const LEGACY_WORKFLOW_IDS: [u64; 2] = [43, 44];
    const LEGACY_WORKFLOW_NAMES: [&str; 2] = ["alpha", "zulu"];

    fn github_object_id(value: u64) -> GitHubObjectId {
        GitHubObjectId::new(NonZeroU64::new(value).expect("fixture object identity is positive"))
    }

    fn workflow_attempt(value: u64) -> RepoWatchWorkflowRunAttempt {
        RepoWatchWorkflowRunAttempt::new(
            NonZeroU64::new(value).expect("fixture workflow attempt is positive"),
        )
    }

    fn workflow_candidate() -> Result<RepoWatchCursorCandidate, Box<dyn Error>> {
        let workflow_run = RepoWatchWorkflowRunObservation::new(
            github_object_id(WORKFLOW_RUN_ID),
            github_object_id(WORKFLOW_ID),
            workflow_attempt(1),
            BranchName::try_new(String::from(BRANCH))?,
            WorkflowName::try_new(String::from(WORKFLOW))?,
            CheckConclusion::Success,
        );
        let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: vec![workflow_run],
            branch_heads: Vec::new(),
        })?;
        Ok(RepoWatchCursorCandidate::new(RepoWatchObservation::new(
            Vec::new(),
            state,
        )))
    }

    fn multi_workflow_candidate() -> Result<RepoWatchCursorCandidate, Box<dyn Error>> {
        let alpha = RepoWatchWorkflowRunObservation::new(
            github_object_id(LEGACY_WORKFLOW_RUN_IDS[0]),
            github_object_id(LEGACY_WORKFLOW_IDS[0]),
            workflow_attempt(1),
            BranchName::try_new(String::from(BRANCH))?,
            WorkflowName::try_new(String::from(LEGACY_WORKFLOW_NAMES[0]))?,
            CheckConclusion::Success,
        );
        let zulu = RepoWatchWorkflowRunObservation::new(
            github_object_id(LEGACY_WORKFLOW_RUN_IDS[1]),
            github_object_id(LEGACY_WORKFLOW_IDS[1]),
            workflow_attempt(1),
            BranchName::try_new(String::from(BRANCH))?,
            WorkflowName::try_new(String::from(LEGACY_WORKFLOW_NAMES[1]))?,
            CheckConclusion::Failure,
        );
        let state = RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: vec![zulu, alpha],
            branch_heads: Vec::new(),
        })?;
        Ok(RepoWatchCursorCandidate::new(RepoWatchObservation::new(
            Vec::new(),
            state,
        )))
    }

    fn legacy_cursor_value(candidate: &RepoWatchCursorCandidate) -> Result<Value, Box<dyn Error>> {
        let mut encoded = encode_cursor_candidate(candidate)?;
        let workflow_runs = encoded
            .get_mut("state")
            .and_then(|state| state.get_mut("workflow_runs"))
            .and_then(Value::as_array_mut)
            .ok_or("fixture workflow cursor shape is present")?;
        for run in workflow_runs.iter_mut() {
            run.as_object_mut()
                .ok_or("fixture workflow run is an object")?
                .remove("workflow_id");
        }
        workflow_runs.sort_by(|left, right| {
            left.get("workflow")
                .and_then(Value::as_str)
                .cmp(&right.get("workflow").and_then(Value::as_str))
        });
        Ok(encoded)
    }

    #[test]
    fn event_page_size_has_a_fixed_upper_bound() -> Result<(), Box<dyn Error>> {
        let admitted = RepoWatchEventPageSize::try_new(
            NonZeroU16::new(100).ok_or("fixture event-page size must be positive")?,
        );
        let rejected = RepoWatchEventPageSize::try_new(
            NonZeroU16::new(101).ok_or("fixture event-page size must be positive")?,
        );

        assert_eq!(admitted?.get(), 100);
        assert_eq!(rejected, Err(RepoWatchPageSizeError));
        Ok(())
    }

    #[test]
    fn cursor_round_trip_retains_provider_workflow_identity_and_attempt()
    -> Result<(), Box<dyn Error>> {
        let candidate = workflow_candidate()?;

        let encoded = encode_cursor_candidate(&candidate)?;
        let decoded = decode_cursor_candidate(encoded)?;

        assert_eq!(decoded, candidate);
        assert_eq!(
            decoded.observation().state().workflow_runs()[0].workflow_id(),
            github_object_id(WORKFLOW_ID)
        );
        assert_eq!(
            decoded.observation().state().workflow_runs()[0].attempt(),
            candidate.observation().state().workflow_runs()[0].attempt()
        );
        Ok(())
    }

    #[test]
    fn version_one_cursor_without_workflow_identity_remains_readable() -> Result<(), Box<dyn Error>>
    {
        let candidate = workflow_candidate()?;
        let mut encoded = encode_cursor_candidate(&candidate)?;
        let workflow_run = encoded
            .get_mut("state")
            .and_then(|state| state.get_mut("workflow_runs"))
            .and_then(Value::as_array_mut)
            .and_then(|runs| runs.first_mut())
            .and_then(Value::as_object_mut)
            .ok_or("fixture workflow cursor shape is present")?;
        workflow_run.remove("workflow_id");

        let decoded = decode_cursor_candidate(encoded)?;

        assert_eq!(
            decoded.observation().state().workflow_runs()[0].workflow_id(),
            github_object_id(WORKFLOW_RUN_ID)
        );
        Ok(())
    }

    #[test]
    fn legacy_cursor_preserves_name_order_while_translating_workflow_identity()
    -> Result<(), Box<dyn Error>> {
        let candidate = multi_workflow_candidate()?;
        let encoded = legacy_cursor_value(&candidate)?;

        let decoded = decode_cursor_candidate(encoded)?;
        let workflows = decoded.observation().state().workflow_runs();

        assert_eq!(workflows.len(), LEGACY_WORKFLOW_NAMES.len());
        assert_eq!(
            workflows[0].id(),
            github_object_id(LEGACY_WORKFLOW_RUN_IDS[1])
        );
        assert_eq!(
            workflows[1].id(),
            github_object_id(LEGACY_WORKFLOW_RUN_IDS[0])
        );
        Ok(())
    }
}

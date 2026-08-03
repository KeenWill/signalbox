//! Durable repository-watch cursor and event storage.

use std::{
    collections::HashSet,
    error::Error,
    fmt,
    num::{NonZeroU16, NonZeroU32, NonZeroU64},
};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use signalbox_application::{
    RepoWatchBranchHead, RepoWatchCheckRunObservation, RepoWatchCheckSuiteObservation,
    RepoWatchObservation, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchReactionObservation, RepoWatchRepositoryState, RepoWatchRepositoryStateInput,
    RepoWatchReviewObservation, RepoWatchThreadObservation, RepoWatchWorkflowRunObservation,
};
use signalbox_domain::{
    BranchName, CheckRunName, CommitSha, GitHubObjectId, LabelName, PullRequestBody,
    PullRequestEventContext, PullRequestEventContextInput, PullRequestNumber, PullRequestTitle,
    ReactionSubject, RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventId,
    RepoWatchEventKindNameV1, RepoWatchEventKindV1, RepoWatchEventTarget, RepoWatchTextError,
    RepositorySlug, ReviewThreadId, WorkflowName,
};
use sqlx::{
    PgPool, Postgres, Transaction,
    types::{Json, Uuid},
};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{
        RepoWatchEventTargetStorageKind, RepoWatchReactionSubjectStorageKind,
        positive_u64_from_numeric, repo_watch_check_conclusion_from_str,
        repo_watch_check_conclusion_to_str, repo_watch_checks_outcome_from_str,
        repo_watch_checks_outcome_to_str, repo_watch_event_kind_from_str,
        repo_watch_event_kind_to_str, repo_watch_event_target_from_str,
        repo_watch_event_target_to_str, repo_watch_mergeable_state_from_str,
        repo_watch_mergeable_state_to_str, repo_watch_pull_request_lifecycle_from_str,
        repo_watch_pull_request_lifecycle_to_str, repo_watch_reaction_change_from_str,
        repo_watch_reaction_change_to_str, repo_watch_reaction_subject_kind_from_str,
        repo_watch_reaction_subject_kind_to_str, repo_watch_reaction_subject_to_storage,
        repo_watch_review_state_from_str, repo_watch_review_state_to_str,
        repo_watch_thread_state_from_str, repo_watch_thread_state_to_str,
    },
};

const CURSOR_STORAGE_VERSION: u64 = 1;
const CURSOR_STORAGE_VERSION_DB: i16 = 1;
const EVENT_VERSION_V1: i16 = 1;
const MAX_RESOURCE_KEY_BYTES: usize = 512;
const MAX_ENTITY_TAG_BYTES: usize = 1_024;
const MAX_EVENT_PAGE_SIZE: u16 = 100;

/// One safe local identifier for a fetched resource or result page.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RepoWatchResourceKey(String);

impl RepoWatchResourceKey {
    pub fn try_new(value: String) -> Result<Self, RepoWatchCursorValueError> {
        if value.is_empty() {
            return Err(RepoWatchCursorValueError::EmptyResourceKey);
        }
        if value.len() > MAX_RESOURCE_KEY_BYTES {
            return Err(RepoWatchCursorValueError::ResourceKeyTooLong);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
        }) {
            return Err(RepoWatchCursorValueError::MalformedResourceKey);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One bounded HTTP entity-tag value retained for a conditional request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchEntityTag(String);

impl RepoWatchEntityTag {
    pub fn try_new(value: String) -> Result<Self, RepoWatchCursorValueError> {
        if value.is_empty() {
            return Err(RepoWatchCursorValueError::EmptyEntityTag);
        }
        if value.len() > MAX_ENTITY_TAG_BYTES {
            return Err(RepoWatchCursorValueError::EntityTagTooLong);
        }
        if !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
        {
            return Err(RepoWatchCursorValueError::MalformedEntityTag);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One resource-specific validator in a durable poll cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchResourceValidator {
    resource: RepoWatchResourceKey,
    entity_tag: RepoWatchEntityTag,
}

impl RepoWatchResourceValidator {
    pub const fn new(resource: RepoWatchResourceKey, entity_tag: RepoWatchEntityTag) -> Self {
        Self {
            resource,
            entity_tag,
        }
    }

    pub const fn resource(&self) -> &RepoWatchResourceKey {
        &self.resource
    }

    pub const fn entity_tag(&self) -> &RepoWatchEntityTag {
        &self.entity_tag
    }
}

/// Why one cursor transport value was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchCursorValueError {
    EmptyResourceKey,
    ResourceKeyTooLong,
    MalformedResourceKey,
    EmptyEntityTag,
    EntityTagTooLong,
    MalformedEntityTag,
    DuplicateResourceKey,
}

impl fmt::Display for RepoWatchCursorValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyResourceKey => "repository-watch resource key is empty",
            Self::ResourceKeyTooLong => "repository-watch resource key exceeds its byte bound",
            Self::MalformedResourceKey => "repository-watch resource key has an invalid shape",
            Self::EmptyEntityTag => "repository-watch entity tag is empty",
            Self::EntityTagTooLong => "repository-watch entity tag exceeds its byte bound",
            Self::MalformedEntityTag => "repository-watch entity tag has an invalid shape",
            Self::DuplicateResourceKey => "repository-watch cursor repeats a resource key",
        })
    }
}

impl Error for RepoWatchCursorValueError {}

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

    fn next(self) -> Option<Self> {
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
    validators: Box<[RepoWatchResourceValidator]>,
    observation: RepoWatchObservation,
}

impl RepoWatchCursorCandidate {
    pub fn try_new(
        mut validators: Vec<RepoWatchResourceValidator>,
        observation: RepoWatchObservation,
    ) -> Result<Self, RepoWatchCursorValueError> {
        validators.sort_by(|left, right| left.resource().cmp(right.resource()));
        if validators
            .windows(2)
            .any(|adjacent| adjacent[0].resource() == adjacent[1].resource())
        {
            return Err(RepoWatchCursorValueError::DuplicateResourceKey);
        }
        Ok(Self {
            validators: validators.into_boxed_slice(),
            observation,
        })
    }

    pub fn validators(&self) -> &[RepoWatchResourceValidator] {
        &self.validators
    }

    pub const fn observation(&self) -> &RepoWatchObservation {
        &self.observation
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
    events: Box<[RepoWatchEvent]>,
}

impl RepoWatchCommitRequest {
    pub fn new(
        expected_generation: Option<RepoWatchCursorGeneration>,
        candidate: RepoWatchCursorCandidate,
        events: Vec<RepoWatchEvent>,
    ) -> Self {
        Self {
            expected_generation,
            candidate,
            events: events.into_boxed_slice(),
        }
    }

    pub const fn expected_generation(&self) -> Option<RepoWatchCursorGeneration> {
        self.expected_generation
    }

    pub const fn candidate(&self) -> &RepoWatchCursorCandidate {
        &self.candidate
    }

    pub fn events(&self) -> &[RepoWatchEvent] {
        &self.events
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
    EventsWithoutStateChange,
    CursorGenerationExhausted,
    EventBatchTooLarge,
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
            Self::EventsWithoutStateChange => {
                formatter.write_str("repository-watch events accompany an unchanged cursor state")
            }
            Self::CursorGenerationExhausted => {
                formatter.write_str("repository-watch cursor generation is exhausted")
            }
            Self::EventBatchTooLarge => formatter
                .write_str("repository-watch event batch exceeds the durable ordinal range"),
        }
    }
}

impl Error for RepoWatchStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::CursorEncoding(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::EventRepositoryMismatch
            | Self::DuplicateEventIdentity(_)
            | Self::EventsWithoutStateChange
            | Self::CursorGenerationExhausted
            | Self::EventBatchTooLarge => None,
        }
    }
}

impl From<sqlx::Error> for RepoWatchStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<RepoWatchPersistenceCorruption> for RepoWatchStoreError {
    fn from(error: RepoWatchPersistenceCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<RepoWatchCursorValueError> for RepoWatchStoreError {
    fn from(_error: RepoWatchCursorValueError) -> Self {
        Self::Corruption(RepoWatchPersistenceCorruption::InvalidCursorField(
            "cursor_payload",
        ))
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
        validate_event_batch(repository, request.events())?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(repository.as_str())
            .execute(&mut *transaction)
            .await?;
        let current = load_cursor_in_transaction(&mut transaction, repository).await?;
        let current_generation = current.as_ref().map(RepoWatchCursor::generation);
        if current_generation != request.expected_generation() {
            let replayed =
                exact_replay(&mut transaction, repository, current.as_ref(), &request).await?;
            transaction.rollback().await?;
            return Ok(match replayed {
                Some(cursor) => RepoWatchCommitOutcome::Replayed(cursor),
                None => RepoWatchCommitOutcome::Conflict {
                    current: current_generation,
                },
            });
        }
        if let Some(current) = current.as_ref()
            && current.candidate() == request.candidate()
        {
            if request.events().is_empty() {
                let cursor = current.clone();
                transaction.rollback().await?;
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
        insert_events(&mut transaction, repository, generation, request.events()).await?;
        transaction.commit().await.map_err(|error| {
            if commit_failure_is_ambiguous(&error) {
                RepoWatchStoreError::CommitAmbiguous(error)
            } else {
                RepoWatchStoreError::Database(error)
            }
        })?;
        Ok(RepoWatchCommitOutcome::Committed(RepoWatchCursor {
            repository: repository.clone(),
            generation,
            candidate: request.candidate,
        }))
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
}

#[derive(sqlx::FromRow)]
struct CursorRow {
    generation: i64,
    cursor_payload: Json<Value>,
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

async fn exact_replay(
    transaction: &mut Transaction<'_, Postgres>,
    repository: &RepositorySlug,
    current: Option<&RepoWatchCursor>,
    request: &RepoWatchCommitRequest,
) -> Result<Option<RepoWatchCursor>, RepoWatchStoreError> {
    let expected_replay_generation = match request.expected_generation() {
        Some(generation) => generation.next(),
        None => Some(RepoWatchCursorGeneration::INITIAL),
    };
    let Some(current) = current else {
        return Ok(None);
    };
    if Some(current.generation()) != expected_replay_generation
        || current.candidate() != request.candidate()
    {
        return Ok(None);
    }
    let stored =
        load_generation_events_in_transaction(transaction, repository, current.generation())
            .await?;
    if stored == request.events() {
        Ok(Some(current.clone()))
    } else {
        Ok(None)
    }
}

fn validate_event_batch(
    repository: &RepositorySlug,
    events: &[RepoWatchEvent],
) -> Result<(), RepoWatchStoreError> {
    if events.len() > i32::MAX as usize {
        return Err(RepoWatchStoreError::EventBatchTooLarge);
    }
    let mut identities = HashSet::with_capacity(events.len());
    for event in events {
        if event.repository() != repository {
            return Err(RepoWatchStoreError::EventRepositoryMismatch);
        }
        if !identities.insert(event.id()) {
            return Err(RepoWatchStoreError::DuplicateEventIdentity(event.id()));
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorRecord {
    storage_version: u64,
    validators: Vec<ValidatorRecord>,
    signal_reviewers: Vec<String>,
    state: RepositoryStateRecord,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidatorRecord {
    resource: String,
    entity_tag: String,
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
    outcome: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRunRecord {
    id: u64,
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
    branch: String,
    workflow: String,
    conclusion: String,
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

fn cursor_record(candidate: &RepoWatchCursorCandidate) -> CursorRecord {
    CursorRecord {
        storage_version: CURSOR_STORAGE_VERSION,
        validators: candidate
            .validators()
            .iter()
            .map(|validator| ValidatorRecord {
                resource: validator.resource().as_str().to_owned(),
                entity_tag: validator.entity_tag().as_str().to_owned(),
            })
            .collect(),
        signal_reviewers: candidate
            .observation()
            .signal_reviewers()
            .iter()
            .map(|reviewer| reviewer.as_str().to_owned())
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
                outcome: repo_watch_checks_outcome_to_str(suite.outcome()).to_owned(),
            })
            .collect(),
        completed_check_runs: state
            .completed_check_runs()
            .iter()
            .map(|run| CheckRunRecord {
                id: run.id().get(),
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
    let record: CursorRecord = serde_json::from_value(value.clone()).map_err(|_| {
        RepoWatchStoreError::Corruption(RepoWatchPersistenceCorruption::MalformedCursorDocument)
    })?;
    if record.storage_version != CURSOR_STORAGE_VERSION {
        return Err(RepoWatchPersistenceCorruption::UnsupportedCursorVersion.into());
    }
    let validators = record
        .validators
        .into_iter()
        .map(|validator| {
            Ok(RepoWatchResourceValidator::new(
                RepoWatchResourceKey::try_new(validator.resource)?,
                RepoWatchEntityTag::try_new(validator.entity_tag)?,
            ))
        })
        .collect::<Result<Vec<_>, RepoWatchStoreError>>()?;
    let signal_reviewers = record
        .signal_reviewers
        .into_iter()
        .map(RepoWatchAuthorLogin::try_new)
        .collect::<Result<Vec<_>, _>>()?;
    let state = decode_repository_state(record.state)?;
    let candidate = RepoWatchCursorCandidate::try_new(
        validators,
        RepoWatchObservation::new(signal_reviewers, state),
    )?;
    if encode_cursor_candidate(&candidate)? != value {
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
        if let RepoWatchEventTarget::PullRequest(context) = event.target() {
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
    events: &[RepoWatchEvent],
) -> Result<(), RepoWatchStoreError> {
    for (index, event) in events.iter().enumerate() {
        let ordinal =
            i32::try_from(index + 1).map_err(|_| RepoWatchStoreError::EventBatchTooLarge)?;
        let encoded = EncodedEvent::from_event(event);
        sqlx::query(
            "INSERT INTO repo_watch_event (
                event_id, repository, cursor_generation, event_ordinal, event_version,
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
                $28, $29, $30, $31, $32, $33, $34, $35, $36
             )",
        )
        .bind(encoded.event_id)
        .bind(repository.as_str())
        .bind(generation_to_i64(generation))
        .bind(ordinal)
        .bind(encoded.event_version)
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

#[derive(Debug, sqlx::FromRow)]
struct EventRow {
    event_id: Uuid,
    repository: String,
    cursor_generation: i64,
    event_ordinal: i32,
    event_version: i16,
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
        event_version,
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
        event_version,
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
) -> Result<Vec<RepoWatchEvent>, RepoWatchStoreError> {
    let rows = sqlx::query_as::<_, EventRow>(EVENT_GENERATION_SQL)
        .bind(repository.as_str())
        .bind(generation_to_i64(generation))
        .fetch_all(&mut **transaction)
        .await?;
    rows.into_iter()
        .map(|row| decode_positioned_event(repository, row).map(|positioned| positioned.event))
        .collect()
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
    use std::{error::Error, num::NonZeroU16};

    use super::*;

    const FIRST_RESOURCE: &str = "checks/page/1";
    const SECOND_RESOURCE: &str = "pulls/page/1";
    const ENTITY_TAG: &str = "\"fixture-etag\"";

    fn empty_observation() -> RepoWatchObservation {
        RepoWatchObservation::new(Vec::new(), RepoWatchRepositoryState::default())
    }

    fn validator(resource: &str) -> Result<RepoWatchResourceValidator, RepoWatchCursorValueError> {
        Ok(RepoWatchResourceValidator::new(
            RepoWatchResourceKey::try_new(resource.to_owned())?,
            RepoWatchEntityTag::try_new(ENTITY_TAG.to_owned())?,
        ))
    }

    #[test]
    fn resource_keys_exclude_query_data() {
        let result = RepoWatchResourceKey::try_new(String::from("pulls?page=1"));

        assert_eq!(result, Err(RepoWatchCursorValueError::MalformedResourceKey));
    }

    #[test]
    fn cursor_candidate_canonicalizes_validator_order() -> Result<(), Box<dyn Error>> {
        let candidate = RepoWatchCursorCandidate::try_new(
            vec![validator(SECOND_RESOURCE)?, validator(FIRST_RESOURCE)?],
            empty_observation(),
        )?;

        assert_eq!(
            candidate.validators()[0].resource().as_str(),
            FIRST_RESOURCE
        );
        assert_eq!(
            candidate.validators()[1].resource().as_str(),
            SECOND_RESOURCE
        );
        Ok(())
    }

    #[test]
    fn cursor_candidate_rejects_duplicate_resource_keys() -> Result<(), Box<dyn Error>> {
        let result = RepoWatchCursorCandidate::try_new(
            vec![validator(FIRST_RESOURCE)?, validator(FIRST_RESOURCE)?],
            empty_observation(),
        );

        assert_eq!(result, Err(RepoWatchCursorValueError::DuplicateResourceKey));
        Ok(())
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
}

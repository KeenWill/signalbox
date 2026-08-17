//! Durable GitHub webhook intake and repository-watch shadow projections.
//!
//! Signature verification and payload interpretation remain outside this
//! storage adapter. This module admits only already-authenticated exact bytes.

use std::{
    error::Error,
    fmt,
    num::{NonZeroU16, NonZeroU64},
    time::SystemTime,
};

use rust_decimal::Decimal;
use signalbox_application::RepoWatchEventContentIdentityV1;
use signalbox_domain::{CommitSha, PullRequestNumber, RepoWatchEventKindNameV1, RepositorySlug};
use sqlx::{
    PgPool, Postgres, Transaction,
    types::{Uuid, time::OffsetDateTime},
};

use crate::{
    commit_failure_is_ambiguous,
    mapping::{positive_u64_from_numeric, repo_watch_event_kind_to_str},
};

const CONTENT_IDENTITY_VERSION_V1: i16 = 1;
const MAX_PENDING_PAGE_SIZE: u16 = 100;
/// What one pending page may hold in exact payload bytes at once.
///
/// The page count alone admits one hundred near-limit bodies, and the startup
/// drain reads this same durable page, so an accumulated backlog would
/// otherwise be able to restart the daemon out of memory repeatedly.
pub const MAX_PENDING_PAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_WEBHOOK_NAME_BYTES: usize = 64;
const MAX_OUTCOME_CODE_BYTES: usize = 64;

/// Permanent replay identity supplied by GitHub for one repository webhook.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RepoWatchWebhookDeliveryKey {
    hook_id: NonZeroU64,
    delivery_id: Uuid,
}

impl RepoWatchWebhookDeliveryKey {
    pub const fn new(hook_id: NonZeroU64, delivery_id: Uuid) -> Self {
        Self {
            hook_id,
            delivery_id,
        }
    }

    pub const fn hook_id(self) -> NonZeroU64 {
        self.hook_id
    }

    pub const fn delivery_id(self) -> Uuid {
        self.delivery_id
    }
}

/// One already-authenticated exact delivery prepared for durable admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookAdmission {
    key: RepoWatchWebhookDeliveryKey,
    repository: RepositorySlug,
    event_name: String,
    action_name: Option<String>,
    body_digest: [u8; 32],
    body: Box<[u8]>,
}

impl RepoWatchWebhookAdmission {
    pub fn try_new(
        key: RepoWatchWebhookDeliveryKey,
        repository: RepositorySlug,
        event_name: String,
        action_name: Option<String>,
        body_digest: [u8; 32],
        body: Vec<u8>,
    ) -> Result<Self, RepoWatchWebhookRequestError> {
        if !valid_name(&event_name, MAX_WEBHOOK_NAME_BYTES) {
            return Err(RepoWatchWebhookRequestError::InvalidEventName);
        }
        if action_name
            .as_deref()
            .is_some_and(|action| !valid_name(action, MAX_WEBHOOK_NAME_BYTES))
        {
            return Err(RepoWatchWebhookRequestError::InvalidActionName);
        }
        if body.is_empty() {
            return Err(RepoWatchWebhookRequestError::EmptyBody);
        }
        Ok(Self {
            key,
            repository,
            event_name,
            action_name,
            body_digest,
            body: body.into_boxed_slice(),
        })
    }

    pub const fn key(&self) -> RepoWatchWebhookDeliveryKey {
        self.key
    }

    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    pub fn event_name(&self) -> &str {
        &self.event_name
    }

    pub fn action_name(&self) -> Option<&str> {
        self.action_name.as_deref()
    }

    pub const fn body_digest(&self) -> &[u8; 32] {
        &self.body_digest
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Validation failure before a webhook persistence request reaches PostgreSQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookRequestError {
    InvalidEventName,
    InvalidActionName,
    InvalidOutcomeCode,
    EmptyBody,
    EmptyOccurrenceKey,
    TooManyProjections,
}

impl fmt::Display for RepoWatchWebhookRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidEventName => "invalid GitHub webhook event name",
            Self::InvalidActionName => "invalid GitHub webhook action name",
            Self::InvalidOutcomeCode => "invalid webhook terminal outcome code",
            Self::EmptyBody => "GitHub webhook body is empty",
            Self::EmptyOccurrenceKey => "webhook event occurrence key is empty",
            Self::TooManyProjections => {
                "webhook projection batch exceeds the durable ordinal range"
            }
        })
    }
}

impl Error for RepoWatchWebhookRequestError {}

/// Positive durable intake position returned only after the local commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookReceipt {
    sequence: NonZeroU64,
    received_at: SystemTime,
}

impl RepoWatchWebhookReceipt {
    pub const fn sequence(self) -> NonZeroU64 {
        self.sequence
    }

    pub const fn received_at(self) -> SystemTime {
        self.received_at
    }
}

/// Replay-sensitive outcome of durable delivery admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookAdmissionOutcome {
    Admitted(RepoWatchWebhookReceipt),
    EqualDuplicate(RepoWatchWebhookReceipt),
    Conflict,
}

/// One bounded pending-delivery page size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookPendingPageSize(NonZeroU16);

impl RepoWatchWebhookPendingPageSize {
    pub fn try_new(value: NonZeroU16) -> Result<Self, RepoWatchWebhookPageSizeError> {
        if value.get() > MAX_PENDING_PAGE_SIZE {
            Err(RepoWatchWebhookPageSizeError)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// A requested pending webhook page exceeded the fixed storage bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookPageSizeError;

impl fmt::Display for RepoWatchWebhookPageSizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("repository-watch pending webhook page size exceeds 100")
    }
}

impl Error for RepoWatchWebhookPageSizeError {}

/// One accepted delivery that still lacks a terminal disposition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingRepoWatchWebhookDelivery {
    key: RepoWatchWebhookDeliveryKey,
    repository: RepositorySlug,
    event_name: String,
    action_name: Option<String>,
    body_digest: [u8; 32],
    receipt: RepoWatchWebhookReceipt,
    body: Box<[u8]>,
}

impl PendingRepoWatchWebhookDelivery {
    pub const fn key(&self) -> RepoWatchWebhookDeliveryKey {
        self.key
    }

    pub const fn repository(&self) -> &RepositorySlug {
        &self.repository
    }

    pub fn event_name(&self) -> &str {
        &self.event_name
    }

    pub fn action_name(&self) -> Option<&str> {
        self.action_name.as_deref()
    }

    pub const fn body_digest(&self) -> &[u8; 32] {
        &self.body_digest
    }

    pub const fn receipt(&self) -> RepoWatchWebhookReceipt {
        self.receipt
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Closed targeted refreshes that are intentionally not direct event mappings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookTargetedQuery {
    PullRequestHydration(PullRequestNumber),
    Mergeability(PullRequestNumber),
    CheckRollup(CommitSha),
}

/// Closed reasons a shadow projection may not match a poll-produced event.
///
/// The rollout gate is no unexplained divergence, so every webhook-only or
/// poll-only parity row must name one of these.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookParityCauseV1 {
    /// Polling observed one state where the delivery stream saw several.
    CompressedTransition,
    /// A hashed context field differed between the two sources.
    ContextDrift,
    /// An event family polling produces and webhooks are not designed to.
    PollOnlyFamily,
    /// The shadow baseline was re-seeded between the two occurrences.
    CrossDrainShadowGap,
}

impl RepoWatchWebhookParityCauseV1 {
    pub const fn code(self) -> &'static str {
        match self {
            Self::CompressedTransition => "compressed_transition",
            Self::ContextDrift => "context_drift",
            Self::PollOnlyFamily => "poll_only_family",
            Self::CrossDrainShadowGap => "cross_drain_shadow_gap",
        }
    }
}

/// One shadow projection derived from an admitted delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookProjection {
    Event {
        content_identity: RepoWatchEventContentIdentityV1,
        event_kind: RepoWatchEventKindNameV1,
        occurrence_key: Box<[u8]>,
        cause: Option<RepoWatchWebhookParityCauseV1>,
    },
    TargetedQuery(RepoWatchWebhookTargetedQuery),
}

impl RepoWatchWebhookProjection {
    /// One projected occurrence, with the reason it may not match if the
    /// producing delivery already knows one.
    pub fn event(
        content_identity: RepoWatchEventContentIdentityV1,
        event_kind: RepoWatchEventKindNameV1,
        occurrence_key: Vec<u8>,
        cause: Option<RepoWatchWebhookParityCauseV1>,
    ) -> Result<Self, RepoWatchWebhookRequestError> {
        if occurrence_key.is_empty() {
            return Err(RepoWatchWebhookRequestError::EmptyOccurrenceKey);
        }
        Ok(Self::Event {
            content_identity,
            event_kind,
            occurrence_key: occurrence_key.into_boxed_slice(),
            cause,
        })
    }
}

/// Closed terminal processing result for one accepted delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookDisposition {
    Projected,
    DuplicateState,
    Superseded,
    Ignored,
    Quarantined,
}

/// One atomic shadow-projection and terminal-disposition request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoWatchWebhookTerminalRequest {
    projections: Box<[RepoWatchWebhookProjection]>,
    disposition: RepoWatchWebhookDisposition,
    outcome_code: Option<String>,
}

impl RepoWatchWebhookTerminalRequest {
    pub fn try_new(
        projections: Vec<RepoWatchWebhookProjection>,
        disposition: RepoWatchWebhookDisposition,
        outcome_code: Option<String>,
    ) -> Result<Self, RepoWatchWebhookRequestError> {
        if projections.len() > i32::MAX as usize {
            return Err(RepoWatchWebhookRequestError::TooManyProjections);
        }
        if outcome_code
            .as_deref()
            .is_some_and(|code| !valid_name(code, MAX_OUTCOME_CODE_BYTES))
        {
            return Err(RepoWatchWebhookRequestError::InvalidOutcomeCode);
        }
        Ok(Self {
            projections: projections.into_boxed_slice(),
            disposition,
            outcome_code,
        })
    }

    pub fn projections(&self) -> &[RepoWatchWebhookProjection] {
        &self.projections
    }

    pub const fn disposition(&self) -> RepoWatchWebhookDisposition {
        self.disposition
    }

    pub fn outcome_code(&self) -> Option<&str> {
        self.outcome_code.as_deref()
    }
}

/// Outcome of an append-once terminal record attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookTerminalOutcome {
    Recorded,
    AlreadyTerminal,
}

/// Closed fail-closed classification for malformed webhook storage rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoWatchWebhookStorageCorruption {
    InvalidHookId,
    InvalidReceiptSequence,
    InvalidRepository,
    InvalidBodyDigest,
}

impl fmt::Display for RepoWatchWebhookStorageCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidHookId => "invalid stored webhook hook ID",
            Self::InvalidReceiptSequence => "invalid stored webhook receipt sequence",
            Self::InvalidRepository => "invalid stored webhook repository",
            Self::InvalidBodyDigest => "invalid stored webhook body digest",
        })
    }
}

impl Error for RepoWatchWebhookStorageCorruption {}

/// Database, transaction, or fail-closed durable webhook intake failure.
#[derive(Debug)]
pub enum RepoWatchWebhookStoreError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    Corruption(RepoWatchWebhookStorageCorruption),
    MissingDelivery,
}

impl fmt::Display for RepoWatchWebhookStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "webhook intake database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "webhook intake commit outcome is ambiguous: {error}"
                )
            }
            Self::Corruption(error) => {
                write!(formatter, "webhook intake storage is corrupt: {error}")
            }
            Self::MissingDelivery => formatter.write_str("webhook delivery does not exist"),
        }
    }
}

impl Error for RepoWatchWebhookStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::MissingDelivery => None,
        }
    }
}

impl From<sqlx::Error> for RepoWatchWebhookStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<RepoWatchWebhookStorageCorruption> for RepoWatchWebhookStoreError {
    fn from(error: RepoWatchWebhookStorageCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL adapter for replay-safe webhook intake and shadow projection.
#[derive(Clone, Debug)]
pub struct PostgresRepoWatchWebhookStore {
    pool: PgPool,
}

impl PostgresRepoWatchWebhookStore {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn admit(
        &self,
        request: &RepoWatchWebhookAdmission,
    ) -> Result<RepoWatchWebhookAdmissionOutcome, RepoWatchWebhookStoreError> {
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query_as::<_, ReceiptRow>(
            "INSERT INTO repo_watch_webhook_delivery (
                hook_id, delivery_id, repository, event_name, action_name, body_digest
             ) VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (hook_id, delivery_id) DO NOTHING
             RETURNING receipt_sequence, received_at",
        )
        .bind(Decimal::from(request.key.hook_id.get()))
        .bind(request.key.delivery_id)
        .bind(request.repository.as_str())
        .bind(&request.event_name)
        .bind(request.action_name.as_deref())
        .bind(request.body_digest.as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(row) = inserted {
            sqlx::query(
                "INSERT INTO repo_watch_webhook_payload (hook_id, delivery_id, body)
                 VALUES ($1, $2, $3)",
            )
            .bind(Decimal::from(request.key.hook_id.get()))
            .bind(request.key.delivery_id)
            .bind(request.body.as_ref())
            .execute(&mut *transaction)
            .await?;
            commit(transaction).await?;
            return Ok(RepoWatchWebhookAdmissionOutcome::Admitted(decode_receipt(
                row,
            )?));
        }
        let stored = load_conflict_row(&mut transaction, request.key).await?;
        transaction.rollback().await?;
        let receipt = decode_receipt(ReceiptRow {
            receipt_sequence: stored.receipt_sequence,
            received_at: stored.received_at,
        })?;
        let equal = stored.repository == request.repository.as_str()
            && stored.event_name == request.event_name
            && stored.action_name.as_deref() == request.action_name.as_deref()
            && stored.body_digest.as_slice() == request.body_digest;
        if equal {
            Ok(RepoWatchWebhookAdmissionOutcome::EqualDuplicate(receipt))
        } else {
            Ok(RepoWatchWebhookAdmissionOutcome::Conflict)
        }
    }

    /// Loads pending deliveries in receipt order, bounded by both the page size
    /// and the bytes the page may retain.
    ///
    /// Bodies are read one at a time against the page's own snapshot rather than
    /// materialized together, because a page of near-limit payloads would
    /// otherwise retain far more than admission itself is allowed to hold.
    pub async fn load_pending(
        &self,
        repository: &RepositorySlug,
        page_size: RepoWatchWebhookPendingPageSize,
        after_receipt: Option<NonZeroU64>,
    ) -> Result<Vec<PendingRepoWatchWebhookDelivery>, RepoWatchWebhookStoreError> {
        let mut transaction = self.pool.begin().await?;
        let headers = sqlx::query_as::<_, PendingHeaderRow>(
            "SELECT delivery.hook_id,
                    delivery.delivery_id,
                    delivery.repository,
                    delivery.event_name,
                    delivery.action_name,
                    delivery.body_digest,
                    delivery.receipt_sequence,
                    delivery.received_at
               FROM repo_watch_webhook_delivery AS delivery
               JOIN repo_watch_webhook_payload AS payload
                 ON payload.hook_id = delivery.hook_id
                AND payload.delivery_id = delivery.delivery_id
               LEFT JOIN repo_watch_webhook_disposition AS disposition
                 ON disposition.hook_id = delivery.hook_id
                AND disposition.delivery_id = delivery.delivery_id
              WHERE delivery.repository = $1
                AND disposition.delivery_id IS NULL
                AND ($3::bigint IS NULL OR delivery.receipt_sequence > $3)
              ORDER BY delivery.receipt_sequence
              LIMIT $2",
        )
        .bind(repository.as_str())
        .bind(i64::from(page_size.get()))
        .bind(after_receipt.map(|receipt| receipt.get() as i64))
        .fetch_all(&mut *transaction)
        .await?;
        let mut deliveries = Vec::with_capacity(headers.len());
        let mut retained_bytes = 0_usize;
        for header in headers {
            let hook_id = header.hook_id;
            let delivery_id = header.delivery_id;
            let body = sqlx::query_scalar::<_, Vec<u8>>(
                "SELECT body
                   FROM repo_watch_webhook_payload
                  WHERE hook_id = $1 AND delivery_id = $2",
            )
            .bind(hook_id)
            .bind(delivery_id)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(body) = body else {
                continue;
            };
            // The oldest delivery is always kept, so one body at the admission
            // ceiling still drains instead of wedging the queue head. Every
            // later body is discarded rather than allowed to overshoot, so the
            // page retains no more than the ceiling states.
            if !deliveries.is_empty()
                && retained_bytes.saturating_add(body.len()) > MAX_PENDING_PAGE_BYTES
            {
                break;
            }
            retained_bytes = retained_bytes.saturating_add(body.len());
            deliveries.push(decode_pending(header, body)?);
        }
        transaction.commit().await?;
        Ok(deliveries)
    }

    pub async fn record_terminal(
        &self,
        key: RepoWatchWebhookDeliveryKey,
        request: &RepoWatchWebhookTerminalRequest,
    ) -> Result<RepoWatchWebhookTerminalOutcome, RepoWatchWebhookStoreError> {
        let mut transaction = self.pool.begin().await?;
        let present =
            sqlx::query_scalar::<_, i64>(crate::lock_inventory::REPO_WATCH_WEBHOOK_DELIVERY)
                .bind(Decimal::from(key.hook_id.get()))
                .bind(key.delivery_id)
                .fetch_optional(&mut *transaction)
                .await?;
        if present.is_none() {
            transaction.rollback().await?;
            return Err(RepoWatchWebhookStoreError::MissingDelivery);
        }
        let already_terminal = sqlx::query_scalar::<_, String>(
            "SELECT disposition
               FROM repo_watch_webhook_disposition
              WHERE hook_id = $1 AND delivery_id = $2",
        )
        .bind(Decimal::from(key.hook_id.get()))
        .bind(key.delivery_id)
        .fetch_optional(&mut *transaction)
        .await?
        .is_some();
        if already_terminal {
            transaction.rollback().await?;
            return Ok(RepoWatchWebhookTerminalOutcome::AlreadyTerminal);
        }
        insert_projections(&mut transaction, key, request.projections()).await?;
        sqlx::query(
            "INSERT INTO repo_watch_webhook_disposition (
                hook_id, delivery_id, disposition, outcome_code
             ) VALUES ($1, $2, $3, $4)",
        )
        .bind(Decimal::from(key.hook_id.get()))
        .bind(key.delivery_id)
        .bind(encode_disposition(request.disposition()))
        .bind(request.outcome_code())
        .execute(&mut *transaction)
        .await?;
        commit(transaction).await?;
        Ok(RepoWatchWebhookTerminalOutcome::Recorded)
    }

    /// Whether one delivery already carries a terminal disposition.
    ///
    /// A commit whose result was lost leaves the caller unable to tell whether
    /// it landed. This read settles that without writing anything, so it cannot
    /// itself be ambiguous.
    pub async fn terminal_disposition_exists(
        &self,
        key: RepoWatchWebhookDeliveryKey,
    ) -> Result<bool, RepoWatchWebhookStoreError> {
        let recorded = sqlx::query_scalar::<_, i64>(
            "SELECT 1
               FROM repo_watch_webhook_disposition
              WHERE hook_id = $1 AND delivery_id = $2",
        )
        .bind(Decimal::from(key.hook_id.get()))
        .bind(key.delivery_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(recorded.is_some())
    }

    /// Deletes exact bodies whose deliveries have been terminal for at least seven days.
    ///
    /// Delivery identities and digests remain in their permanent tombstone rows.
    pub async fn purge_expired_payloads(&self) -> Result<u64, RepoWatchWebhookStoreError> {
        let result = sqlx::query(
            "DELETE FROM repo_watch_webhook_payload AS payload
              USING repo_watch_webhook_delivery AS delivery,
                    repo_watch_webhook_disposition AS disposition
              WHERE delivery.hook_id = payload.hook_id
                AND delivery.delivery_id = payload.delivery_id
                AND disposition.hook_id = delivery.hook_id
                AND disposition.delivery_id = delivery.delivery_id
                AND disposition.recorded_at <= statement_timestamp() - interval '7 days'",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[derive(sqlx::FromRow)]
struct ReceiptRow {
    receipt_sequence: i64,
    received_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct ConflictRow {
    repository: String,
    event_name: String,
    action_name: Option<String>,
    body_digest: Vec<u8>,
    receipt_sequence: i64,
    received_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct PendingHeaderRow {
    hook_id: Decimal,
    delivery_id: Uuid,
    repository: String,
    event_name: String,
    action_name: Option<String>,
    body_digest: Vec<u8>,
    receipt_sequence: i64,
    received_at: OffsetDateTime,
}

async fn load_conflict_row(
    transaction: &mut Transaction<'_, Postgres>,
    key: RepoWatchWebhookDeliveryKey,
) -> Result<ConflictRow, RepoWatchWebhookStoreError> {
    sqlx::query_as::<_, ConflictRow>(
        "SELECT repository, event_name, action_name, body_digest,
                receipt_sequence, received_at
           FROM repo_watch_webhook_delivery
          WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(key.hook_id.get()))
    .bind(key.delivery_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(RepoWatchWebhookStoreError::Database)
}

fn decode_receipt(row: ReceiptRow) -> Result<RepoWatchWebhookReceipt, RepoWatchWebhookStoreError> {
    let sequence = u64::try_from(row.receipt_sequence)
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(RepoWatchWebhookStorageCorruption::InvalidReceiptSequence)?;
    let received_at = SystemTime::from(row.received_at);
    Ok(RepoWatchWebhookReceipt {
        sequence,
        received_at,
    })
}

fn decode_pending(
    row: PendingHeaderRow,
    body: Vec<u8>,
) -> Result<PendingRepoWatchWebhookDelivery, RepoWatchWebhookStoreError> {
    let hook_id = positive_u64_from_numeric(row.hook_id)
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(RepoWatchWebhookStorageCorruption::InvalidHookId)?;
    let repository = RepositorySlug::try_new(row.repository)
        .map_err(|_| RepoWatchWebhookStorageCorruption::InvalidRepository)?;
    let body_digest = row
        .body_digest
        .as_slice()
        .try_into()
        .map_err(|_| RepoWatchWebhookStorageCorruption::InvalidBodyDigest)?;
    let receipt = decode_receipt(ReceiptRow {
        receipt_sequence: row.receipt_sequence,
        received_at: row.received_at,
    })?;
    Ok(PendingRepoWatchWebhookDelivery {
        key: RepoWatchWebhookDeliveryKey::new(hook_id, row.delivery_id),
        repository,
        event_name: row.event_name,
        action_name: row.action_name,
        body_digest,
        receipt,
        body: body.into_boxed_slice(),
    })
}

async fn insert_projections(
    transaction: &mut Transaction<'_, Postgres>,
    key: RepoWatchWebhookDeliveryKey,
    projections: &[RepoWatchWebhookProjection],
) -> Result<(), RepoWatchWebhookStoreError> {
    for (index, projection) in projections.iter().enumerate() {
        let ordinal = i32::try_from(index + 1).map_err(|_| {
            RepoWatchWebhookStoreError::Database(sqlx::Error::Protocol(
                "webhook projection ordinal exceeds i32".to_owned(),
            ))
        })?;
        let encoded = EncodedProjection::from_projection(projection);
        sqlx::query(
            "INSERT INTO repo_watch_webhook_projection (
                hook_id, delivery_id, projection_ordinal, projection_kind,
                content_identity_version, content_identity, event_kind,
                targeted_query_kind, targeted_query_key, occurrence_key,
                cause_code
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(Decimal::from(key.hook_id.get()))
        .bind(key.delivery_id)
        .bind(ordinal)
        .bind(encoded.projection_kind)
        .bind(encoded.content_identity_version)
        .bind(encoded.content_identity)
        .bind(encoded.event_kind)
        .bind(encoded.targeted_query_kind)
        .bind(encoded.targeted_query_key)
        .bind(encoded.occurrence_key)
        .bind(encoded.cause_code)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

struct EncodedProjection {
    projection_kind: &'static str,
    content_identity_version: Option<i16>,
    content_identity: Option<Vec<u8>>,
    event_kind: Option<&'static str>,
    targeted_query_kind: Option<&'static str>,
    targeted_query_key: Option<String>,
    occurrence_key: Option<Vec<u8>>,
    cause_code: Option<&'static str>,
}

impl EncodedProjection {
    fn from_projection(projection: &RepoWatchWebhookProjection) -> Self {
        match projection {
            RepoWatchWebhookProjection::Event {
                content_identity,
                event_kind,
                occurrence_key,
                cause,
            } => Self {
                projection_kind: "event",
                content_identity_version: Some(CONTENT_IDENTITY_VERSION_V1),
                content_identity: Some(content_identity.as_bytes().to_vec()),
                event_kind: Some(repo_watch_event_kind_to_str(*event_kind)),
                targeted_query_kind: None,
                targeted_query_key: None,
                occurrence_key: Some(occurrence_key.to_vec()),
                cause_code: cause.map(RepoWatchWebhookParityCauseV1::code),
            },
            RepoWatchWebhookProjection::TargetedQuery(
                RepoWatchWebhookTargetedQuery::PullRequestHydration(number),
            ) => Self {
                projection_kind: "targeted_query",
                content_identity_version: None,
                content_identity: None,
                event_kind: None,
                targeted_query_kind: Some("pull_request_hydration"),
                targeted_query_key: Some(number.get().to_string()),
                occurrence_key: None,
                cause_code: None,
            },
            RepoWatchWebhookProjection::TargetedQuery(
                RepoWatchWebhookTargetedQuery::Mergeability(number),
            ) => Self {
                projection_kind: "targeted_query",
                content_identity_version: None,
                content_identity: None,
                event_kind: None,
                targeted_query_kind: Some("mergeability"),
                targeted_query_key: Some(number.get().to_string()),
                occurrence_key: None,
                cause_code: None,
            },
            RepoWatchWebhookProjection::TargetedQuery(
                RepoWatchWebhookTargetedQuery::CheckRollup(head),
            ) => Self {
                projection_kind: "targeted_query",
                content_identity_version: None,
                content_identity: None,
                event_kind: None,
                targeted_query_kind: Some("check_rollup"),
                targeted_query_key: Some(head.as_str().to_owned()),
                occurrence_key: None,
                cause_code: None,
            },
        }
    }
}

const fn encode_disposition(disposition: RepoWatchWebhookDisposition) -> &'static str {
    match disposition {
        RepoWatchWebhookDisposition::Projected => "projected",
        RepoWatchWebhookDisposition::DuplicateState => "duplicate_state",
        RepoWatchWebhookDisposition::Superseded => "superseded",
        RepoWatchWebhookDisposition::Ignored => "ignored",
        RepoWatchWebhookDisposition::Quarantined => "quarantined",
    }
}

async fn commit(transaction: Transaction<'_, Postgres>) -> Result<(), RepoWatchWebhookStoreError> {
    transaction.commit().await.map_err(|error| {
        if commit_failure_is_ambiguous(&error) {
            RepoWatchWebhookStoreError::CommitAmbiguous(error)
        } else {
            RepoWatchWebhookStoreError::Database(error)
        }
    })
}

fn valid_name(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

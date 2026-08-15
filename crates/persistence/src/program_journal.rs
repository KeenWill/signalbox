//! PostgreSQL adapter for durable program execution journals.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    DeliveryFrame, DeliveryKind, DeliveryOrdinal, EffectRequest, FaultCause, FaultEvidenceRef,
    InlineFramePayload, JournalEntry, JournalFrame, JournalPosition, NondeterminismError,
    ProgramFault, ProgramJournal, ProgramJournalError, ProgramRunId, RequestFrame, RequestKind,
    RequestOrdinal, ScopeOrdinal, ScopeRequest,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};

use crate::{
    commit_failure_is_ambiguous,
    lock_inventory::PROGRAM_JOURNAL_SEQUENCE,
    mapping::{
        ProgramDeliveryStorageKind, ProgramRequestStorageKind, positive_u64_from_numeric,
        program_capability_from_str, program_capability_to_str, program_delivery_kind_from_str,
        program_delivery_kind_to_str, program_fault_cause_from_str, program_fault_cause_to_str,
        program_reject_reason_from_str, program_reject_reason_to_str,
        program_request_kind_from_str, program_request_kind_to_str,
        program_scope_operation_from_str, program_scope_operation_to_str,
    },
};

const FRAME_CONTRACT_VERSION: i64 = 1;

const LOAD_JOURNAL: &str = r#"SELECT entry.journal_position, entry.frame_direction,
       entry.frame_kind, entry.request_ordinal, entry.delivery_ordinal,
       entry.resolves_request_ordinal, entry.request_scope_ordinal,
       entry.scope_operation, entry.declared_scope_ordinal,
       entry.parent_scope_ordinal, entry.effect_capability, entry.effect_method,
       entry.reject_reason, entry.fault_cause, entry.payload_inline,
       divergence.expected_request_ordinal,
       divergence.expected_request_scope_ordinal,
       divergence.expected_kind, divergence.expected_scope_operation,
       divergence.expected_declared_scope_ordinal,
       divergence.expected_parent_scope_ordinal,
       divergence.expected_effect_capability, divergence.expected_effect_method,
       divergence.expected_payload_inline,
       divergence.observed_request_ordinal,
       divergence.observed_request_scope_ordinal,
       divergence.observed_kind, divergence.observed_scope_operation,
       divergence.observed_declared_scope_ordinal,
       divergence.observed_parent_scope_ordinal,
       divergence.observed_effect_capability, divergence.observed_effect_method,
       divergence.observed_payload_inline
  FROM program_run_journal_entry AS entry
  LEFT JOIN program_run_journal_nondeterminism AS divergence
    ON divergence.run_id = entry.run_id
   AND divergence.journal_position = entry.journal_position
 WHERE entry.run_id = $1
 ORDER BY entry.journal_position"#;

/// Durable rows could not reconstruct one checked typed journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgramJournalCorruption {
    MissingStream,
    MissingSequenceState,
    InvalidOrdinal(&'static str),
    Unsupported { field: &'static str, value: String },
    Inconsistent(&'static str),
    Domain(ProgramJournalError),
}

impl fmt::Display for ProgramJournalCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingStream => formatter.write_str("program journal stream is missing"),
            Self::MissingSequenceState => {
                formatter.write_str("program journal sequence state is missing")
            }
            Self::InvalidOrdinal(field) => write!(formatter, "invalid program journal {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported program journal {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent program journal {relationship}")
            }
            Self::Domain(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProgramJournalCorruption {}

/// Storage failure, preserving whether a final commit response was ambiguous.
#[derive(Debug)]
pub enum ProgramJournalRepositoryError {
    Database {
        source: sqlx::Error,
        commit_ambiguous: bool,
    },
    Corruption(ProgramJournalCorruption),
}

impl ProgramJournalRepositoryError {
    pub const fn corruption(&self) -> Option<&ProgramJournalCorruption> {
        match self {
            Self::Corruption(error) => Some(error),
            Self::Database { .. } => None,
        }
    }

    pub const fn commit_ambiguous(&self) -> bool {
        match self {
            Self::Database {
                commit_ambiguous, ..
            } => *commit_ambiguous,
            Self::Corruption(_) => false,
        }
    }
}

impl fmt::Display for ProgramJournalRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { source, .. } => {
                write!(formatter, "program journal database: {source}")
            }
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProgramJournalRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ProgramJournalRepositoryError {
    fn from(source: sqlx::Error) -> Self {
        Self::Database {
            source,
            commit_ambiguous: false,
        }
    }
}

impl From<ProgramJournalCorruption> for ProgramJournalRepositoryError {
    fn from(error: ProgramJournalCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// Typed append/read boundary for program execution journals.
#[derive(Clone, Debug)]
pub struct ProgramJournalRepository {
    pool: PgPool,
}

impl ProgramJournalRepository {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Creates the journal anchor for one new run under frame contract v1.
    pub async fn create_stream(
        &self,
        run: ProgramRunId,
    ) -> Result<(), ProgramJournalRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO program_run_journal_stream (run_id, frame_contract_version)
             VALUES ($1, $2)",
        )
        .bind(run.into_uuid())
        .bind(FRAME_CONTRACT_VERSION)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("INSERT INTO program_run_journal_sequence_state (run_id) VALUES ($1)")
            .bind(run.into_uuid())
            .execute(&mut *transaction)
            .await?;
        commit(transaction).await
    }

    /// Appends one request and allocates its request ordinal in program order.
    pub async fn append_request(
        &self,
        run: ProgramRunId,
        scope: Option<ScopeOrdinal>,
        kind: RequestKind,
    ) -> Result<RequestFrame, ProgramJournalRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let sequence = lock_sequence(&mut transaction, run).await?;
        let position = next_position(sequence.last_position)?;
        let ordinal = next_request_ordinal(sequence.last_request)?;
        let frame = RequestFrame::new(ordinal, scope, kind);
        insert_request(&mut transaction, run, position, &frame).await?;
        advance_sequence(
            &mut transaction,
            run,
            position.as_u64(),
            ordinal.as_u64(),
            sequence.last_delivery,
        )
        .await?;
        commit(transaction).await?;
        Ok(frame)
    }

    /// Appends one host delivery and allocates its durable delivery order.
    pub async fn append_delivery(
        &self,
        run: ProgramRunId,
        kind: DeliveryKind,
    ) -> Result<DeliveryFrame, ProgramJournalRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        let frame = Self::append_delivery_in_transaction(&mut transaction, run, kind).await?;
        commit(transaction).await?;
        Ok(frame)
    }

    /// Appends a delivery inside the transaction that owns its consequence.
    ///
    /// This method neither begins nor commits, preserving the transactional
    /// effect idiom: the caller's durable state and answer frame succeed or
    /// fail as one commit.
    pub async fn append_delivery_in_transaction(
        transaction: &mut Transaction<'_, Postgres>,
        run: ProgramRunId,
        kind: DeliveryKind,
    ) -> Result<DeliveryFrame, ProgramJournalRepositoryError> {
        if matches!(
            kind,
            DeliveryKind::Fault(ProgramFault::Nondeterminism { .. })
        ) {
            return Err(ProgramJournalCorruption::Inconsistent(
                "nondeterminism fault without replay failure",
            )
            .into());
        }
        append_delivery_frame_in_transaction(transaction, run, kind).await
    }

    /// Persists the typed divergence produced by the replay seam as a fault.
    pub async fn append_nondeterminism_fault(
        &self,
        failure: NondeterminismError,
    ) -> Result<DeliveryFrame, ProgramJournalRepositoryError> {
        let run = failure.run();
        let mut transaction = self.pool.begin().await?;
        let frame = append_delivery_frame_in_transaction(
            &mut transaction,
            run,
            DeliveryKind::Fault(failure.into_fault()),
        )
        .await?;
        commit(transaction).await?;
        Ok(frame)
    }

    /// Loads and fail-closed reconstitutes a run's complete journal.
    pub async fn load(
        &self,
        run: ProgramRunId,
    ) -> Result<Option<ProgramJournal>, ProgramJournalRepositoryError> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM program_run_journal_stream WHERE run_id = $1
             )",
        )
        .bind(run.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        if !exists {
            return Ok(None);
        }
        let rows = sqlx::query(LOAD_JOURNAL)
            .bind(run.into_uuid())
            .fetch_all(&self.pool)
            .await?;
        let entries = rows
            .iter()
            .map(decode_entry)
            .collect::<Result<Vec<_>, ProgramJournalRepositoryError>>()?;
        ProgramJournal::try_new(run, entries)
            .map(Some)
            .map_err(|error| ProgramJournalCorruption::Domain(error).into())
    }
}

async fn append_delivery_frame_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    run: ProgramRunId,
    kind: DeliveryKind,
) -> Result<DeliveryFrame, ProgramJournalRepositoryError> {
    let sequence = lock_sequence(transaction, run).await?;
    let position = next_position(sequence.last_position)?;
    let ordinal = next_delivery_ordinal(sequence.last_delivery)?;
    let frame = DeliveryFrame::new(ordinal, kind);
    insert_delivery(transaction, run, position, &frame).await?;
    advance_sequence(
        transaction,
        run,
        position.as_u64(),
        sequence.last_request,
        ordinal.as_u64(),
    )
    .await?;
    Ok(frame)
}

async fn commit(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), ProgramJournalRepositoryError> {
    match transaction.commit().await {
        Ok(()) => Ok(()),
        Err(source) => {
            let commit_ambiguous = commit_failure_is_ambiguous(&source);
            Err(ProgramJournalRepositoryError::Database {
                source,
                commit_ambiguous,
            })
        }
    }
}

struct SequenceState {
    last_position: u64,
    last_request: u64,
    last_delivery: u64,
}

async fn lock_sequence(
    transaction: &mut Transaction<'_, Postgres>,
    run: ProgramRunId,
) -> Result<SequenceState, ProgramJournalRepositoryError> {
    let row = sqlx::query(PROGRAM_JOURNAL_SEQUENCE)
        .bind(run.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
    let Some(row) = row else {
        let stream_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM program_run_journal_stream WHERE run_id = $1
             )",
        )
        .bind(run.into_uuid())
        .fetch_one(&mut **transaction)
        .await?;
        let corruption = if stream_exists {
            ProgramJournalCorruption::MissingSequenceState
        } else {
            ProgramJournalCorruption::MissingStream
        };
        return Err(corruption.into());
    };
    Ok(SequenceState {
        last_position: nonnegative_u64(row.try_get("last_position")?, "sequence last position")?,
        last_request: nonnegative_u64(
            row.try_get("last_request_ordinal")?,
            "sequence last request ordinal",
        )?,
        last_delivery: nonnegative_u64(
            row.try_get("last_delivery_ordinal")?,
            "sequence last delivery ordinal",
        )?,
    })
}

fn nonnegative_u64(value: Decimal, field: &'static str) -> Result<u64, ProgramJournalCorruption> {
    if value == Decimal::ZERO {
        Ok(0)
    } else {
        positive_u64_from_numeric(value)
            .map_err(|_| ProgramJournalCorruption::InvalidOrdinal(field))
    }
}

fn next_position(last: u64) -> Result<JournalPosition, ProgramJournalCorruption> {
    JournalPosition::try_from_u64(
        last.checked_add(1)
            .ok_or(ProgramJournalCorruption::InvalidOrdinal("next position"))?,
    )
    .ok_or(ProgramJournalCorruption::InvalidOrdinal("next position"))
}

fn next_request_ordinal(last: u64) -> Result<RequestOrdinal, ProgramJournalCorruption> {
    RequestOrdinal::try_from_u64(last.checked_add(1).ok_or(
        ProgramJournalCorruption::InvalidOrdinal("next request ordinal"),
    )?)
    .ok_or(ProgramJournalCorruption::InvalidOrdinal(
        "next request ordinal",
    ))
}

fn next_delivery_ordinal(last: u64) -> Result<DeliveryOrdinal, ProgramJournalCorruption> {
    DeliveryOrdinal::try_from_u64(last.checked_add(1).ok_or(
        ProgramJournalCorruption::InvalidOrdinal("next delivery ordinal"),
    )?)
    .ok_or(ProgramJournalCorruption::InvalidOrdinal(
        "next delivery ordinal",
    ))
}

async fn advance_sequence(
    transaction: &mut Transaction<'_, Postgres>,
    run: ProgramRunId,
    position: u64,
    request: u64,
    delivery: u64,
) -> Result<(), ProgramJournalRepositoryError> {
    let updated = sqlx::query(
        "UPDATE program_run_journal_sequence_state
            SET last_position = $2,
                last_request_ordinal = $3,
                last_delivery_ordinal = $4
          WHERE run_id = $1",
    )
    .bind(run.into_uuid())
    .bind(Decimal::from(position))
    .bind(Decimal::from(request))
    .bind(Decimal::from(delivery))
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(ProgramJournalCorruption::MissingSequenceState.into());
    }
    Ok(())
}

struct EncodedRequest<'a> {
    kind: &'static str,
    payload: &'a [u8],
    scope_operation: Option<&'static str>,
    declared_scope: Option<u64>,
    parent_scope: Option<u64>,
    capability: Option<&'static str>,
    method: Option<&'a str>,
}

fn encode_request(request: &RequestKind) -> EncodedRequest<'_> {
    let mut encoded = EncodedRequest {
        kind: program_request_kind_to_str(request),
        payload: &[],
        scope_operation: None,
        declared_scope: None,
        parent_scope: None,
        capability: None,
        method: None,
    };
    match request {
        RequestKind::Now(payload)
        | RequestKind::Random(payload)
        | RequestKind::Sleep(payload)
        | RequestKind::AwaitEvent(payload)
        | RequestKind::Terminal(payload) => encoded.payload = payload.as_bytes(),
        RequestKind::Effect(effect) => {
            encoded.payload = effect.payload().as_bytes();
            encoded.capability = Some(program_capability_to_str(effect.capability()));
            encoded.method = Some(effect.method());
        }
        RequestKind::Scope(scope) => {
            encoded.scope_operation = Some(program_scope_operation_to_str(scope.operation()));
            encoded.declared_scope = Some(scope.scope().as_u64());
            encoded.parent_scope = scope.parent().map(ScopeOrdinal::as_u64);
        }
    }
    encoded
}

async fn insert_request(
    transaction: &mut Transaction<'_, Postgres>,
    run: ProgramRunId,
    position: JournalPosition,
    frame: &RequestFrame,
) -> Result<(), ProgramJournalRepositoryError> {
    let encoded = encode_request(frame.kind());
    sqlx::query(
        "INSERT INTO program_run_journal_entry (
             run_id, journal_position, frame_direction, frame_kind,
             request_ordinal, request_scope_ordinal, scope_operation,
             declared_scope_ordinal, parent_scope_ordinal, effect_capability,
             effect_method, payload_inline
         ) VALUES ($1, $2, 'request', $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(run.into_uuid())
    .bind(Decimal::from(position.as_u64()))
    .bind(encoded.kind)
    .bind(Decimal::from(frame.ordinal().as_u64()))
    .bind(frame.scope().map(|scope| Decimal::from(scope.as_u64())))
    .bind(encoded.scope_operation)
    .bind(encoded.declared_scope.map(Decimal::from))
    .bind(encoded.parent_scope.map(Decimal::from))
    .bind(encoded.capability)
    .bind(encoded.method)
    .bind(encoded.payload)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

struct EncodedDelivery<'a> {
    kind: &'static str,
    resolves: Option<u64>,
    reject_reason: Option<&'static str>,
    fault_cause: Option<&'static str>,
    payload: &'a [u8],
    divergence: Option<(&'a RequestFrame, &'a RequestFrame)>,
}

fn encode_delivery(delivery: &DeliveryKind) -> EncodedDelivery<'_> {
    let mut encoded = EncodedDelivery {
        kind: program_delivery_kind_to_str(delivery),
        resolves: delivery.resolves().map(RequestOrdinal::as_u64),
        reject_reason: None,
        fault_cause: None,
        payload: &[],
        divergence: None,
    };
    match delivery {
        DeliveryKind::Answer { payload, .. }
        | DeliveryKind::Wake { payload, .. }
        | DeliveryKind::Cancel { payload, .. }
        | DeliveryKind::RunCancel(payload) => encoded.payload = payload.as_bytes(),
        DeliveryKind::Reject { reason, .. } => {
            encoded.reject_reason = Some(program_reject_reason_to_str(*reason));
        }
        DeliveryKind::Fault(fault) => {
            encoded.fault_cause = Some(program_fault_cause_to_str(fault.cause()));
            match fault.evidence() {
                FaultEvidenceRef::Ordinary(payload) => encoded.payload = payload.as_bytes(),
                FaultEvidenceRef::Nondeterminism { expected, observed } => {
                    encoded.divergence = Some((expected, observed));
                }
            }
        }
    }
    encoded
}

async fn insert_delivery(
    transaction: &mut Transaction<'_, Postgres>,
    run: ProgramRunId,
    position: JournalPosition,
    frame: &DeliveryFrame,
) -> Result<(), ProgramJournalRepositoryError> {
    let encoded = encode_delivery(frame.kind());
    sqlx::query(
        "INSERT INTO program_run_journal_entry (
             run_id, journal_position, frame_direction, frame_kind,
             delivery_ordinal, resolves_request_ordinal, reject_reason,
             fault_cause, payload_inline
         ) VALUES ($1, $2, 'delivery', $3, $4, $5, $6, $7, $8)",
    )
    .bind(run.into_uuid())
    .bind(Decimal::from(position.as_u64()))
    .bind(encoded.kind)
    .bind(Decimal::from(frame.ordinal().as_u64()))
    .bind(encoded.resolves.map(Decimal::from))
    .bind(encoded.reject_reason)
    .bind(encoded.fault_cause)
    .bind(encoded.payload)
    .execute(&mut **transaction)
    .await?;

    if let Some((expected, observed)) = encoded.divergence {
        insert_divergence(transaction, run, position, expected, observed).await?;
    }
    Ok(())
}

async fn insert_divergence(
    transaction: &mut Transaction<'_, Postgres>,
    run: ProgramRunId,
    position: JournalPosition,
    expected: &RequestFrame,
    observed: &RequestFrame,
) -> Result<(), ProgramJournalRepositoryError> {
    let expected_encoded = encode_request(expected.kind());
    let observed_encoded = encode_request(observed.kind());
    sqlx::query(
        "INSERT INTO program_run_journal_nondeterminism (
             run_id, journal_position,
             expected_request_ordinal, expected_request_scope_ordinal,
             expected_kind, expected_scope_operation,
             expected_declared_scope_ordinal, expected_parent_scope_ordinal,
             expected_effect_capability, expected_effect_method,
             expected_payload_inline,
             observed_request_ordinal, observed_request_scope_ordinal,
             observed_kind, observed_scope_operation,
             observed_declared_scope_ordinal, observed_parent_scope_ordinal,
             observed_effect_capability, observed_effect_method,
             observed_payload_inline
         ) VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
             $12, $13, $14, $15, $16, $17, $18, $19, $20
         )",
    )
    .bind(run.into_uuid())
    .bind(Decimal::from(position.as_u64()))
    .bind(Decimal::from(expected.ordinal().as_u64()))
    .bind(expected.scope().map(|scope| Decimal::from(scope.as_u64())))
    .bind(expected_encoded.kind)
    .bind(expected_encoded.scope_operation)
    .bind(expected_encoded.declared_scope.map(Decimal::from))
    .bind(expected_encoded.parent_scope.map(Decimal::from))
    .bind(expected_encoded.capability)
    .bind(expected_encoded.method)
    .bind(expected_encoded.payload)
    .bind(Decimal::from(observed.ordinal().as_u64()))
    .bind(observed.scope().map(|scope| Decimal::from(scope.as_u64())))
    .bind(observed_encoded.kind)
    .bind(observed_encoded.scope_operation)
    .bind(observed_encoded.declared_scope.map(Decimal::from))
    .bind(observed_encoded.parent_scope.map(Decimal::from))
    .bind(observed_encoded.capability)
    .bind(observed_encoded.method)
    .bind(observed_encoded.payload)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn decode_entry(row: &PgRow) -> Result<JournalEntry, ProgramJournalRepositoryError> {
    let position = journal_position(row.try_get("journal_position")?, "journal position")?;
    let direction: String = row.try_get("frame_direction")?;
    let kind: String = row.try_get("frame_kind")?;
    let frame = match direction.as_str() {
        "request" => JournalFrame::Request(decode_request(
            request_fields(row, RequestColumns::Entry)?,
            &kind,
        )?),
        "delivery" => JournalFrame::Delivery(decode_delivery(row, &kind)?),
        _ => {
            return Err(ProgramJournalCorruption::Unsupported {
                field: "frame direction",
                value: direction,
            }
            .into());
        }
    };
    Ok(JournalEntry::new(position, frame))
}

enum RequestColumns {
    Entry,
    Expected,
    Observed,
}

struct StoredRequest {
    ordinal: Decimal,
    scope: Option<Decimal>,
    operation: Option<String>,
    declared_scope: Option<Decimal>,
    parent_scope: Option<Decimal>,
    capability: Option<String>,
    method: Option<String>,
    payload: Vec<u8>,
}

fn request_fields(row: &PgRow, columns: RequestColumns) -> Result<StoredRequest, sqlx::Error> {
    match columns {
        RequestColumns::Entry => Ok(StoredRequest {
            ordinal: row.try_get("request_ordinal")?,
            scope: row.try_get("request_scope_ordinal")?,
            operation: row.try_get("scope_operation")?,
            declared_scope: row.try_get("declared_scope_ordinal")?,
            parent_scope: row.try_get("parent_scope_ordinal")?,
            capability: row.try_get("effect_capability")?,
            method: row.try_get("effect_method")?,
            payload: row.try_get("payload_inline")?,
        }),
        RequestColumns::Expected => Ok(StoredRequest {
            ordinal: row.try_get("expected_request_ordinal")?,
            scope: row.try_get("expected_request_scope_ordinal")?,
            operation: row.try_get("expected_scope_operation")?,
            declared_scope: row.try_get("expected_declared_scope_ordinal")?,
            parent_scope: row.try_get("expected_parent_scope_ordinal")?,
            capability: row.try_get("expected_effect_capability")?,
            method: row.try_get("expected_effect_method")?,
            payload: row.try_get("expected_payload_inline")?,
        }),
        RequestColumns::Observed => Ok(StoredRequest {
            ordinal: row.try_get("observed_request_ordinal")?,
            scope: row.try_get("observed_request_scope_ordinal")?,
            operation: row.try_get("observed_scope_operation")?,
            declared_scope: row.try_get("observed_declared_scope_ordinal")?,
            parent_scope: row.try_get("observed_parent_scope_ordinal")?,
            capability: row.try_get("observed_effect_capability")?,
            method: row.try_get("observed_effect_method")?,
            payload: row.try_get("observed_payload_inline")?,
        }),
    }
}

fn decode_request(
    stored: StoredRequest,
    kind: &str,
) -> Result<RequestFrame, ProgramJournalRepositoryError> {
    let ordinal = request_ordinal(stored.ordinal, "request ordinal")?;
    let scope = stored
        .scope
        .map(|value| scope_ordinal(value, "request scope ordinal"))
        .transpose()?;
    let payload = InlineFramePayload::new(stored.payload);
    let kind = match program_request_kind_from_str(kind).ok_or_else(|| {
        ProgramJournalCorruption::Unsupported {
            field: "request kind",
            value: kind.to_owned(),
        }
    })? {
        ProgramRequestStorageKind::Now => RequestKind::Now(payload),
        ProgramRequestStorageKind::Random => RequestKind::Random(payload),
        ProgramRequestStorageKind::Sleep => RequestKind::Sleep(payload),
        ProgramRequestStorageKind::AwaitEvent => RequestKind::AwaitEvent(payload),
        ProgramRequestStorageKind::Terminal => RequestKind::Terminal(payload),
        ProgramRequestStorageKind::Effect => {
            let capability_value = stored
                .capability
                .ok_or(ProgramJournalCorruption::Inconsistent("effect capability"))?;
            let capability = program_capability_from_str(&capability_value).ok_or({
                ProgramJournalCorruption::Unsupported {
                    field: "effect capability",
                    value: capability_value,
                }
            })?;
            let method = stored
                .method
                .ok_or(ProgramJournalCorruption::Inconsistent("effect method"))?;
            RequestKind::Effect(EffectRequest::new(capability, method, payload))
        }
        ProgramRequestStorageKind::Scope => {
            let operation_value = stored
                .operation
                .ok_or(ProgramJournalCorruption::Inconsistent("scope operation"))?;
            let operation = program_scope_operation_from_str(&operation_value).ok_or({
                ProgramJournalCorruption::Unsupported {
                    field: "scope operation",
                    value: operation_value,
                }
            })?;
            let declared = stored
                .declared_scope
                .ok_or(ProgramJournalCorruption::Inconsistent(
                    "declared scope ordinal",
                ))?;
            let parent = stored
                .parent_scope
                .map(|value| scope_ordinal(value, "parent scope ordinal"))
                .transpose()?;
            RequestKind::Scope(ScopeRequest::new(
                operation,
                scope_ordinal(declared, "declared scope ordinal")?,
                parent,
            ))
        }
    };
    Ok(RequestFrame::new(ordinal, scope, kind))
}

fn decode_delivery(
    row: &PgRow,
    kind: &str,
) -> Result<DeliveryFrame, ProgramJournalRepositoryError> {
    let ordinal = delivery_ordinal(row.try_get("delivery_ordinal")?, "delivery ordinal")?;
    let resolves: Option<Decimal> = row.try_get("resolves_request_ordinal")?;
    let resolves = resolves
        .map(|value| request_ordinal(value, "resolved request ordinal"))
        .transpose()?;
    let payload = InlineFramePayload::new(row.try_get::<Vec<u8>, _>("payload_inline")?);
    let kind = match program_delivery_kind_from_str(kind).ok_or_else(|| {
        ProgramJournalCorruption::Unsupported {
            field: "delivery kind",
            value: kind.to_owned(),
        }
    })? {
        ProgramDeliveryStorageKind::Answer => DeliveryKind::Answer {
            resolves: required_resolution(resolves)?,
            payload,
        },
        ProgramDeliveryStorageKind::Wake => DeliveryKind::Wake {
            resolves: required_resolution(resolves)?,
            payload,
        },
        ProgramDeliveryStorageKind::Reject => {
            let reason_value: String = row.try_get("reject_reason")?;
            let reason = program_reject_reason_from_str(&reason_value).ok_or({
                ProgramJournalCorruption::Unsupported {
                    field: "reject reason",
                    value: reason_value,
                }
            })?;
            DeliveryKind::Reject {
                resolves: required_resolution(resolves)?,
                reason,
            }
        }
        ProgramDeliveryStorageKind::Cancel => DeliveryKind::Cancel {
            resolves: required_resolution(resolves)?,
            payload,
        },
        ProgramDeliveryStorageKind::RunCancel => DeliveryKind::RunCancel(payload),
        ProgramDeliveryStorageKind::Fault => DeliveryKind::Fault(decode_fault(row, payload)?),
    };
    Ok(DeliveryFrame::new(ordinal, kind))
}

fn required_resolution(
    value: Option<RequestOrdinal>,
) -> Result<RequestOrdinal, ProgramJournalCorruption> {
    value.ok_or(ProgramJournalCorruption::Inconsistent(
        "delivery resolution",
    ))
}

fn decode_fault(
    row: &PgRow,
    payload: InlineFramePayload,
) -> Result<ProgramFault, ProgramJournalRepositoryError> {
    let cause_value: String = row.try_get("fault_cause")?;
    let cause = program_fault_cause_from_str(&cause_value).ok_or({
        ProgramJournalCorruption::Unsupported {
            field: "fault cause",
            value: cause_value,
        }
    })?;
    let divergence_is_present: bool = row
        .try_get::<Option<Decimal>, _>("expected_request_ordinal")?
        .is_some();
    let fault = match cause {
        FaultCause::Timeout => ProgramFault::Timeout(payload),
        FaultCause::Memory => ProgramFault::Memory(payload),
        FaultCause::ProgramError => ProgramFault::ProgramError(payload),
        FaultCause::ContractRetired => ProgramFault::ContractRetired(payload),
        FaultCause::JournalBound => ProgramFault::JournalBound(payload),
        FaultCause::PayloadTooLarge => ProgramFault::PayloadTooLarge(payload),
        FaultCause::Nondeterminism => {
            if !divergence_is_present {
                return Err(
                    ProgramJournalCorruption::Inconsistent("nondeterminism evidence").into(),
                );
            }
            let expected_kind: String = row.try_get("expected_kind")?;
            let observed_kind: String = row.try_get("observed_kind")?;
            ProgramFault::Nondeterminism {
                expected: decode_request(
                    request_fields(row, RequestColumns::Expected)?,
                    &expected_kind,
                )?,
                observed: decode_request(
                    request_fields(row, RequestColumns::Observed)?,
                    &observed_kind,
                )?,
            }
        }
    };
    if cause != FaultCause::Nondeterminism && divergence_is_present {
        return Err(
            ProgramJournalCorruption::Inconsistent("unexpected divergence evidence").into(),
        );
    }
    Ok(fault)
}

fn journal_position(
    value: Decimal,
    field: &'static str,
) -> Result<JournalPosition, ProgramJournalCorruption> {
    JournalPosition::try_from_u64(
        positive_u64_from_numeric(value)
            .map_err(|_| ProgramJournalCorruption::InvalidOrdinal(field))?,
    )
    .ok_or(ProgramJournalCorruption::InvalidOrdinal(field))
}

fn request_ordinal(
    value: Decimal,
    field: &'static str,
) -> Result<RequestOrdinal, ProgramJournalCorruption> {
    RequestOrdinal::try_from_u64(
        positive_u64_from_numeric(value)
            .map_err(|_| ProgramJournalCorruption::InvalidOrdinal(field))?,
    )
    .ok_or(ProgramJournalCorruption::InvalidOrdinal(field))
}

fn delivery_ordinal(
    value: Decimal,
    field: &'static str,
) -> Result<DeliveryOrdinal, ProgramJournalCorruption> {
    DeliveryOrdinal::try_from_u64(
        positive_u64_from_numeric(value)
            .map_err(|_| ProgramJournalCorruption::InvalidOrdinal(field))?,
    )
    .ok_or(ProgramJournalCorruption::InvalidOrdinal(field))
}

fn scope_ordinal(
    value: Decimal,
    field: &'static str,
) -> Result<ScopeOrdinal, ProgramJournalCorruption> {
    ScopeOrdinal::try_from_u64(
        positive_u64_from_numeric(value)
            .map_err(|_| ProgramJournalCorruption::InvalidOrdinal(field))?,
    )
    .ok_or(ProgramJournalCorruption::InvalidOrdinal(field))
}

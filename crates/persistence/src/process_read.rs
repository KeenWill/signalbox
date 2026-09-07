//! Read-only PostgreSQL projections for the local process protocol.
//!
//! These values are persistence-owned snapshots, not process-protocol frames or
//! domain aggregates. Reads use one read-only repeatable-read transaction so
//! the hub can map a complete, stable projection explicitly.

mod entry;
mod load;
mod reader;
mod repository;
mod session;
mod transcript_types;
mod turn_decode;
mod turn_facts;

pub(crate) use load::load_process_runner_projection;

pub use reader::ProcessTranscriptReader;
pub use session::{
    ProcessModelSelection, ProcessRunnerConnectionHealth, ProcessRunnerProjection,
    ProcessRunnerProjectionState, ProcessScopedTranscriptRead, ProcessSessionDefaults,
    ProcessSessionDefaultsRead, ProcessSessionSummary, ProcessSessionSummaryReader,
};
pub use transcript_types::{
    ProcessAttachmentPreparationFailureCause, ProcessCurrentModelCall,
    ProcessCurrentModelCallState, ProcessFailedModelCallDisposition,
    ProcessFailedTerminalModelCall, ProcessImportedContentKind, ProcessImportedSourceSpeaker,
    ProcessModelCallInputTokenSemantics, ProcessModelCallRecoveryPrecondition,
    ProcessModelCallTokenUsage, ProcessModelCallUsageProvenance,
    ProcessProviderModelCallFailureCause, ProcessReconciliationOperation, ProcessSessionAncestry,
    ProcessToolApproval, ProcessToolExecutionResultDisposition, ProcessTranscriptEntry,
    ProcessTranscriptItem, ProcessTranscriptModelCallUsage, ProcessTranscriptSnapshot,
    ProcessTranscriptSummary, ProcessTranscriptTurn, ProcessTurnState,
};

use rust_decimal::Decimal;
use signalbox_domain::{
    DirectModelSelection, ImportedConversationId, ImportedSourceAttestation,
    ImportedTranscriptContent, ImportedTranscriptEntryId, ModelCallId, RunnerGeneration,
    SemanticTranscriptEntryId, SessionId, ToolApprovalDecider, ToolApprovalDecision,
    ToolApprovalResolutionReconstitutionInput, ToolDecisionRationale, ToolDenialReason,
    ToolRequestId,
};
use sqlx::{PgPool, Row, postgres::PgRow, types::Uuid};

use crate::mapping::{
    ToolApprovalDecisionSourceStorageKind, ToolAttemptDispositionStorageKind,
    durable_command_id_from_uuid, tool_approval_decision_source_from_str,
    tool_attempt_disposition_from_str,
};

const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";
/// Hard safety ceiling on session identities read ahead by one summary cursor;
/// it bounds page memory and the number of histories authenticated per query.
const SESSION_SUMMARY_PAGE_SIZE: i64 = 64;

#[derive(signalbox_derive::OperatorError)]
/// A committed read shape that cannot form the closed process projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessReadCorruption {
    #[error("process read is missing {field_0}")]
    /// One required row or field was absent.
    Missing(&'static str),
    #[error("process read has unsupported {field}: {value}")]
    /// A closed storage discriminator had no admitted mapping.
    Unsupported {
        /// Storage field containing the discriminator.
        field: &'static str,
        /// Unsupported durable spelling.
        value: String,
    },
    #[error("process read has inconsistent {field_0}")]
    /// Related durable fields disagreed.
    Inconsistent(&'static str),
    #[error("process read has invalid {field_0}")]
    /// A stored ordinal was not an admitted unsigned integer.
    InvalidOrdinal(&'static str),
}

#[derive(signalbox_derive::OperatorError)]
/// PostgreSQL failure or fail-closed projection corruption.
#[derive(Debug)]
pub enum ProcessReadError {
    #[error("process read database operation failed")]
    /// PostgreSQL could not complete the repeatable-read transaction.
    Database(#[source] sqlx::Error),
    #[error(transparent)]
    /// Committed rows could not form the closed projection.
    Corruption(#[source] ProcessReadCorruption),
}

impl From<sqlx::Error> for ProcessReadError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ProcessReadCorruption> for ProcessReadError {
    fn from(error: ProcessReadCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL-backed process read boundary.
#[derive(Clone, Debug)]
pub struct ProcessReadRepository {
    pool: PgPool,
    automatic_reconciliation_attempt_budget: Option<Option<u32>>,
}

fn decode_tool_result_disposition(
    value: &str,
) -> Result<ToolAttemptDispositionStorageKind, ProcessReadCorruption> {
    tool_attempt_disposition_from_str(value).ok_or_else(|| ProcessReadCorruption::Unsupported {
        field: "terminal_disposition_kind",
        value: value.to_owned(),
    })
}

fn decode_process_tool_approval(
    row: &PgRow,
) -> Result<Option<ProcessToolApproval>, ProcessReadError> {
    let decision_kind: Option<String> = row.try_get("transcript_decision_kind")?;
    let source: Option<String> = row.try_get("transcript_decision_source")?;
    let denial_reason: Option<String> = row.try_get("transcript_denial_reason")?;
    let user_command: Option<Uuid> = row.try_get("transcript_user_command_id")?;
    let delegate_model: Option<Uuid> = row.try_get("transcript_delegate_model_selection_id")?;
    let delegate_call: Option<Uuid> = row.try_get("transcript_delegate_model_call_id")?;
    let rationale: Option<String> = row.try_get("transcript_decision_rationale")?;
    let override_denied: Option<Uuid> = row.try_get("transcript_override_denied_request_id")?;
    let override_command: Option<Uuid> = row.try_get("transcript_override_command_id")?;
    let Some(source) = source else {
        if decision_kind.is_some()
            || denial_reason.is_some()
            || user_command.is_some()
            || delegate_model.is_some()
            || delegate_call.is_some()
            || rationale.is_some()
            || override_denied.is_some()
            || override_command.is_some()
        {
            return Err(ProcessReadCorruption::Inconsistent(
                "tool approval projection without source",
            )
            .into());
        }
        return Ok(None);
    };
    let source_kind = tool_approval_decision_source_from_str(&source).ok_or_else(|| {
        ProcessReadError::from(ProcessReadCorruption::Unsupported {
            field: "tool approval decision source",
            value: source,
        })
    })?;
    let decision = match decision_kind.as_deref() {
        Some("approve") if denial_reason.is_none() => ToolApprovalDecision::Approve,
        Some("deny") => ToolApprovalDecision::Deny {
            reason: denial_reason
                .map(ToolDenialReason::try_new)
                .transpose()
                .map_err(|_| ProcessReadCorruption::Inconsistent("tool denial reason"))?,
        },
        _ => {
            return Err(ProcessReadCorruption::Inconsistent("tool approval decision kind").into());
        }
    };
    if source_kind != ToolApprovalDecisionSourceStorageKind::UserOverride
        && (override_denied.is_some() || override_command.is_some())
    {
        return Err(ProcessReadCorruption::Inconsistent("tool approval provenance shape").into());
    }
    let runtime_safety_decision = ToolApprovalResolutionReconstitutionInput::runtime_safety(
        ToolRequestId::from_uuid(Uuid::nil()),
    )
    .reconstitute()
    .map_err(|_| ProcessReadCorruption::Inconsistent("runtime safety approval evidence"))?;
    match (
        source_kind,
        user_command,
        delegate_model,
        delegate_call,
        rationale,
    ) {
        (
            ToolApprovalDecisionSourceStorageKind::PolicyAuto
            | ToolApprovalDecisionSourceStorageKind::SessionBlanket,
            None,
            None,
            None,
            None,
        ) if decision == ToolApprovalDecision::Approve => Ok(None),
        (ToolApprovalDecisionSourceStorageKind::RuntimeSafety, None, None, None, None)
            if runtime_safety_decision.decision() == &decision =>
        {
            Ok(None)
        }
        (ToolApprovalDecisionSourceStorageKind::LifecycleClosure, Some(_), None, None, None)
            if decision == (ToolApprovalDecision::Deny { reason: None }) =>
        {
            Ok(None)
        }
        (ToolApprovalDecisionSourceStorageKind::UserCommand, Some(command), None, None, None) => {
            Ok(Some(ProcessToolApproval {
                decision,
                decider: ToolApprovalDecider::User {
                    command: durable_command_id_from_uuid(command).map_err(|_| {
                        ProcessReadCorruption::Inconsistent("tool approval user command")
                    })?,
                },
                rationale: None,
            }))
        }
        (
            ToolApprovalDecisionSourceStorageKind::Delegate,
            None,
            Some(model),
            Some(call),
            Some(rationale),
        ) => {
            let rationale = ToolDecisionRationale::try_new(rationale)
                .map_err(|_| ProcessReadCorruption::Inconsistent("tool decision rationale"))?;
            // A delegate denial's stored reason equals the derivation from
            // its rationale — null exactly when the rationale derives
            // nothing — so missing current evidence reads as corruption.
            if let ToolApprovalDecision::Deny { ref reason } = decision
                && *reason != ToolDenialReason::from_rationale(&rationale)
            {
                return Err(ProcessReadCorruption::Inconsistent("delegate denial payload").into());
            }
            Ok(Some(ProcessToolApproval {
                decision,
                decider: ToolApprovalDecider::Delegate {
                    model: DirectModelSelection::from_uuid(model),
                    call: ModelCallId::from_uuid(call),
                },
                rationale: Some(rationale),
            }))
        }
        (ToolApprovalDecisionSourceStorageKind::UserOverride, None, None, None, None)
            if decision == ToolApprovalDecision::Approve =>
        {
            match (override_denied, override_command) {
                (Some(denied_request), Some(command)) => Ok(Some(ProcessToolApproval {
                    decision,
                    decider: ToolApprovalDecider::UserOverride {
                        command: durable_command_id_from_uuid(command).map_err(|_| {
                            ProcessReadCorruption::Inconsistent("tool approval override command")
                        })?,
                        denied_request: ToolRequestId::from_uuid(denied_request),
                    },
                    rationale: None,
                })),
                (None, _) | (_, None) => Err(ProcessReadCorruption::Inconsistent(
                    "tool approval provenance shape",
                )
                .into()),
            }
        }
        (
            ToolApprovalDecisionSourceStorageKind::PolicyAuto
            | ToolApprovalDecisionSourceStorageKind::SessionBlanket
            | ToolApprovalDecisionSourceStorageKind::UserCommand
            | ToolApprovalDecisionSourceStorageKind::Delegate
            | ToolApprovalDecisionSourceStorageKind::RuntimeSafety
            | ToolApprovalDecisionSourceStorageKind::LifecycleClosure
            | ToolApprovalDecisionSourceStorageKind::UserOverride,
            ..,
        ) => Err(ProcessReadCorruption::Inconsistent("tool approval provenance shape").into()),
    }
}

fn decode_imported_source_speaker(
    value: String,
) -> Result<ProcessImportedSourceSpeaker, ProcessReadError> {
    match value.as_str() {
        "not_attested" => Ok(ProcessImportedSourceSpeaker::NotAttested),
        "attested_absent" => Ok(ProcessImportedSourceSpeaker::AttestedAbsent),
        "attested_user" => Ok(ProcessImportedSourceSpeaker::User),
        "attested_assistant" => Ok(ProcessImportedSourceSpeaker::Assistant),
        _ => Err(ProcessReadCorruption::Unsupported {
            field: "imported source speaker",
            value,
        }
        .into()),
    }
}

fn project_imported_entry(
    entry_index: u64,
    source_session: SessionId,
    entry: SemanticTranscriptEntryId,
    imported_conversation: ImportedConversationId,
    imported_entry: ImportedTranscriptEntryId,
    source_speaker: ProcessImportedSourceSpeaker,
    content: ImportedTranscriptContent,
) -> ProcessTranscriptEntry {
    match content {
        ImportedTranscriptContent::Text(ImportedSourceAttestation::Attested(content)) => {
            ProcessTranscriptEntry::ImportedText {
                entry_index,
                source_session,
                entry,
                imported_conversation,
                imported_entry,
                source_speaker,
                content: content.into_string(),
            }
        }
        content => ProcessTranscriptEntry::Imported {
            entry_index,
            source_session,
            entry,
            imported_conversation,
            imported_entry,
            source_speaker,
            content_kind: match content {
                ImportedTranscriptContent::SourceEvent { .. } => {
                    ProcessImportedContentKind::SourceEvent
                }
                ImportedTranscriptContent::SourceMessageBlock { .. } => {
                    ProcessImportedContentKind::SourceMessageBlock
                }
                ImportedTranscriptContent::Text(_) => ProcessImportedContentKind::Text,
                ImportedTranscriptContent::ToolCall { .. } => ProcessImportedContentKind::ToolCall,
                ImportedTranscriptContent::ToolResult { .. } => {
                    ProcessImportedContentKind::ToolResult
                }
                ImportedTranscriptContent::Thinking { .. } => ProcessImportedContentKind::Thinking,
                ImportedTranscriptContent::RedactedThinking { .. } => {
                    ProcessImportedContentKind::RedactedThinking
                }
                ImportedTranscriptContent::Document { .. } => ProcessImportedContentKind::Document,
                ImportedTranscriptContent::MessageContentAbsent(_) => {
                    ProcessImportedContentKind::MessageContentAbsent
                }
            },
        },
    }
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, ProcessReadError>
where
    for<'row> T: sqlx::Decode<'row, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ProcessReadCorruption::Missing(field).into())
}

fn decode_nonnegative(value: Decimal, field: &'static str) -> Result<u64, ProcessReadCorruption> {
    if !value.fract().is_zero() || value.is_sign_negative() {
        return Err(ProcessReadCorruption::InvalidOrdinal(field));
    }
    u64::try_from(value).map_err(|_| ProcessReadCorruption::InvalidOrdinal(field))
}

fn decode_positive(value: Decimal, field: &'static str) -> Result<u64, ProcessReadCorruption> {
    let value = decode_nonnegative(value, field)?;
    if value == 0 {
        Err(ProcessReadCorruption::InvalidOrdinal(field))
    } else {
        Ok(value)
    }
}

fn decode_runner_generation(
    value: Decimal,
    field: &'static str,
) -> Result<RunnerGeneration, ProcessReadCorruption> {
    RunnerGeneration::try_from_u64(decode_nonnegative(value, field)?)
        .ok_or(ProcessReadCorruption::InvalidOrdinal(field))
}

#[cfg(test)]
mod tests;

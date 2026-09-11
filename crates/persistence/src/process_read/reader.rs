use super::load::{fetch_next_transcript_entry, open_transcript_entry_cursor};
use super::session::ProcessRunnerProjection;
use super::transcript_types::{ProcessTranscriptItem, ProcessTranscriptSummary};
use super::turn_decode::decode_transcript_turn;
use super::turn_facts::{
    decode_model_call_usage, load_next_model_call_usage, load_next_transcript_turn,
};
use super::{ProcessReadCorruption, ProcessReadError, decode_nonnegative};
use crate::credential_pool_exhaustion::CredentialPoolEvidenceError;
use crate::mapping::session_id_to_uuid;
use rust_decimal::Decimal;
use signalbox_domain::{ContextFrontierId, ModelCallId, SessionId, TurnId};
use sqlx::types::Uuid;
use sqlx::{Postgres, Row, Transaction};

/// One repeatable-read transcript cursor that owns at most one decoded row.
///
/// Call [`Self::next_item`] until it returns `None`. That terminal call commits
/// the read-only transaction and makes [`Self::summary`] available. Dropping a
/// reader early rolls its transaction back.
#[derive(Debug)]
pub struct ProcessTranscriptReader {
    pub(super) workspace_root_kind: Option<signalbox_domain::SessionWorkspaceRootKind>,
    pub(super) transaction: Option<Transaction<'static, Postgres>>,
    pub(super) session: SessionId,
    pub(super) cursor: u64,
    pub(super) runner: Option<ProcessRunnerProjection>,
    pub(super) lineage_tip: Option<TurnId>,
    pub(super) latest_frontier: Option<ContextFrontierId>,
    pub(super) expected_turn_count: u64,
    pub(super) turn_count: u64,
    pub(super) next_turn_after: Option<u64>,
    pub(super) turns_complete: bool,
    pub(super) expected_model_call_count: u64,
    pub(super) model_call_count: u64,
    pub(super) next_model_call_after: Option<(u64, ModelCallId)>,
    pub(super) model_calls_complete: bool,
    pub(super) entry_count: Option<u64>,
    pub(super) next_entry_index: u64,
    pub(super) after_frontier: Option<ContextFrontierId>,
    pub(super) summary: Option<ProcessTranscriptSummary>,
    pub(super) automatic_reconciliation_attempt_budget: Option<Option<u32>>,
}

impl ProcessTranscriptReader {
    /// Returns the recorded daemon-local binding from this snapshot.
    pub const fn workspace_root_kind(&self) -> Option<signalbox_domain::SessionWorkspaceRootKind> {
        self.workspace_root_kind
    }
    /// Returns the selected session while the reader is active.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the current runner placement from this same repeatable-read snapshot.
    pub const fn runner(&self) -> Option<&ProcessRunnerProjection> {
        self.runner.as_ref()
    }

    /// Returns the snapshot's global outbox cursor.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Returns the committed summary only after [`Self::next_item`] returned
    /// `None`.
    pub const fn summary(&self) -> Option<ProcessTranscriptSummary> {
        self.summary
    }

    /// Yields one turn, model-call usage record, or entry without retaining
    /// prior decoded rows.
    pub async fn next_item(&mut self) -> Result<Option<ProcessTranscriptItem>, ProcessReadError> {
        if self.summary.is_some() {
            return Ok(None);
        }

        if !self.turns_complete {
            let session = self.session;
            let next_turn_after = self.next_turn_after;
            let row = load_next_transcript_turn(self.transaction_mut()?, session, next_turn_after)
                .await?;
            if let Some(row) = row {
                let mut decoded =
                    decode_transcript_turn(&row, self.automatic_reconciliation_attempt_budget)?;
                if row.try_get::<bool, _>("credential_pool_exhausted")?
                    && let Some(evidence) = crate::credential_pool_exhaustion::load(
                        &mut **self.transaction_mut()?,
                        session.into_uuid(),
                        decoded.turn.turn().into_uuid(),
                    )
                    .await
                    .map_err(|error| match error {
                        CredentialPoolEvidenceError::Database(error) => {
                            ProcessReadError::from(error)
                        }
                        CredentialPoolEvidenceError::Corruption => ProcessReadError::from(
                            ProcessReadCorruption::Inconsistent("pool exhaustion evidence"),
                        ),
                    })?
                {
                    decoded.turn.state =
                        super::transcript_types::ProcessTurnState::FailedCredentialPoolExhausted(
                            Box::new(evidence),
                        );
                }
                match (decoded.start_lineage, decoded.latest_frontier) {
                    (None, None) => {}
                    (Some(_), Some(frontier)) => {
                        if Some(decoded.turn.turn()) == self.lineage_tip
                            && self.latest_frontier.replace(frontier).is_some()
                        {
                            return Err(ProcessReadCorruption::Inconsistent(
                                "turn execution lineage",
                            )
                            .into());
                        }
                    }
                    _ => {
                        return Err(ProcessReadCorruption::Inconsistent(
                            "started turn frontier shape",
                        )
                        .into());
                    }
                }
                self.next_turn_after = Some(decoded.turn.acceptance_position());
                self.turn_count =
                    self.turn_count
                        .checked_add(1)
                        .ok_or(ProcessReadCorruption::InvalidOrdinal(
                            "transcript turn count",
                        ))?;
                return Ok(Some(ProcessTranscriptItem::Turn(decoded.turn)));
            }
            if self.turn_count != self.expected_turn_count {
                return Err(ProcessReadCorruption::Inconsistent("turn acceptance ordering").into());
            }
            if self.lineage_tip.is_some() && self.latest_frontier.is_none() {
                return Err(ProcessReadCorruption::Inconsistent("turn execution lineage").into());
            }
            self.turns_complete = true;
        }

        if !self.model_calls_complete {
            let session = self.session;
            let next_model_call_after = self.next_model_call_after;
            let row =
                load_next_model_call_usage(self.transaction_mut()?, session, next_model_call_after)
                    .await?;
            if let Some(row) = row {
                let (acceptance_position, usage) = decode_model_call_usage(&row)?;
                self.next_model_call_after = Some((acceptance_position, usage.call()));
                self.model_call_count = self.model_call_count.checked_add(1).ok_or(
                    ProcessReadCorruption::InvalidOrdinal("transcript model-call count"),
                )?;
                return Ok(Some(ProcessTranscriptItem::ModelCallUsage(usage)));
            }
            if self.model_call_count != self.expected_model_call_count {
                return Err(
                    ProcessReadCorruption::Inconsistent("terminal model-call ordering").into(),
                );
            }
            self.model_calls_complete = true;
        }

        if self.entry_count.is_none() {
            let session = self.session;
            let current_frontier = self.latest_frontier;
            let latest_frontier = advance_through_latest_compaction(
                self.transaction_mut()?,
                session,
                current_frontier,
            )
            .await?;
            self.latest_frontier = latest_frontier;
            let after_entry_count = if let Some(after_frontier) = self.after_frontier {
                let Some(current_frontier) = latest_frontier else {
                    return Err(ProcessReadError::ResyncRequired);
                };
                let acknowledged: Option<(Decimal, bool)> = sqlx::query_as(
                    "SELECT member_count,
                            context_frontier_preserves_prefix($1, $2, $3)
                       FROM context_frontier
                      WHERE owning_session_id = $1
                        AND context_frontier_id = $2",
                )
                .bind(session_id_to_uuid(session))
                .bind(after_frontier.into_uuid())
                .bind(current_frontier.into_uuid())
                .fetch_optional(&mut **self.transaction_mut()?)
                .await?;
                let Some((member_count, preserves_prefix)) = acknowledged else {
                    return Err(ProcessReadError::ResyncRequired);
                };
                let after_entry_count =
                    decode_nonnegative(member_count, "acknowledged frontier member count")?;
                if !preserves_prefix {
                    return Err(ProcessReadError::ResyncRequired);
                }
                after_entry_count
            } else {
                0
            };
            let entry_count = match latest_frontier {
                Some(frontier) => {
                    let entry_count = open_transcript_entry_cursor(
                        self.transaction_mut()?,
                        session,
                        frontier,
                        after_entry_count,
                    )
                    .await?;
                    if after_entry_count > entry_count {
                        return Err(ProcessReadError::ResyncRequired);
                    }
                    entry_count
                }
                None => 0,
            };
            self.next_entry_index = after_entry_count;
            self.entry_count = Some(entry_count);
        }

        let entry_count = self
            .entry_count
            .ok_or(ProcessReadCorruption::Missing("transcript entry count"))?;
        if self.latest_frontier.is_some() {
            let entry_index = self.next_entry_index;
            if let Some(entry) =
                fetch_next_transcript_entry(self.transaction_mut()?, entry_index, entry_count)
                    .await?
            {
                if entry_index >= entry_count {
                    return Err(ProcessReadCorruption::Inconsistent(
                        "context frontier declared membership",
                    )
                    .into());
                }
                self.next_entry_index = self.next_entry_index.checked_add(1).ok_or(
                    ProcessReadCorruption::InvalidOrdinal("transcript entry index"),
                )?;
                return Ok(Some(ProcessTranscriptItem::Entry(entry)));
            }
        }
        if self.next_entry_index != entry_count {
            return Err(ProcessReadCorruption::Inconsistent(
                "context frontier declared membership",
            )
            .into());
        }

        let transaction = self
            .transaction
            .take()
            .ok_or(ProcessReadCorruption::Missing("process read transaction"))?;
        transaction.commit().await?;
        self.summary = Some(ProcessTranscriptSummary {
            session: self.session,
            cursor: self.cursor,
            turn_count: self.turn_count,
            model_call_count: self.model_call_count,
            entry_count,
            frontier: self.latest_frontier,
        });
        Ok(None)
    }

    fn transaction_mut(&mut self) -> Result<&mut Transaction<'static, Postgres>, ProcessReadError> {
        self.transaction
            .as_mut()
            .ok_or_else(|| ProcessReadCorruption::Missing("process read transaction").into())
    }
}

async fn advance_through_latest_compaction(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    current: Option<ContextFrontierId>,
) -> Result<Option<ContextFrontierId>, ProcessReadError> {
    let latest: Option<Uuid> = sqlx::query_scalar(
        "SELECT candidate.result_frontier_id
           FROM context_compaction AS candidate
          WHERE candidate.session_id = $1
            AND NOT EXISTS (
                SELECT 1
                  FROM context_compaction AS successor
                 WHERE successor.session_id = candidate.session_id
                   AND successor.predecessor_compaction_id =
                           candidate.context_compaction_id
            )",
    )
    .bind(session_id_to_uuid(session))
    .fetch_optional(&mut **transaction)
    .await?;
    let current = later_transcript_frontier(
        transaction,
        session,
        current,
        latest.map(ContextFrontierId::from_uuid),
    )
    .await?;
    let placement: Option<Uuid> = sqlx::query_scalar("SELECT boundary.context_frontier_id FROM runner_session_placement_frontier AS head JOIN runner_placement_boundary AS boundary USING (session_id, placement_revision) WHERE head.session_id = $1")
        .bind(session_id_to_uuid(session)).fetch_optional(&mut **transaction).await?;
    later_transcript_frontier(
        transaction,
        session,
        current,
        placement.map(ContextFrontierId::from_uuid),
    )
    .await
}

async fn later_transcript_frontier(
    transaction: &mut Transaction<'static, Postgres>,
    session: SessionId,
    current: Option<ContextFrontierId>,
    latest: Option<ContextFrontierId>,
) -> Result<Option<ContextFrontierId>, ProcessReadError> {
    let Some(latest) = latest else {
        return Ok(current);
    };
    let Some(current) = current else {
        return Ok(Some(latest));
    };
    if current == latest {
        return Ok(Some(current));
    }
    let row: (bool, bool) = sqlx::query_as(
        "SELECT
            NOT EXISTS (
                SELECT 1
                  FROM resolve_context_frontier_members($1, $2) AS earlier
                  LEFT JOIN resolve_context_frontier_members($1, $3) AS later
                    ON later.member_position = earlier.member_position
                   AND later.source_session_id = earlier.source_session_id
                   AND later.semantic_entry_id = earlier.semantic_entry_id
                 WHERE later.member_position IS NULL
            ),
            NOT EXISTS (
                SELECT 1
                  FROM resolve_context_frontier_members($1, $3) AS earlier
                  LEFT JOIN resolve_context_frontier_members($1, $2) AS later
                    ON later.member_position = earlier.member_position
                   AND later.source_session_id = earlier.source_session_id
                   AND later.semantic_entry_id = earlier.semantic_entry_id
                 WHERE later.member_position IS NULL
            )",
    )
    .bind(session_id_to_uuid(session))
    .bind(current.into_uuid())
    .bind(latest.into_uuid())
    .fetch_one(&mut **transaction)
    .await?;
    match row {
        (true, false) => Ok(Some(latest)),
        (false, true) => Ok(Some(current)),
        _ => {
            Err(ProcessReadCorruption::Inconsistent("transcript boundary frontier lineage").into())
        }
    }
}

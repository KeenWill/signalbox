//! PostgreSQL store for review-workflow aggregates.
//!
//! SQL rows remain adapter-private. Complete values are reconstructed through
//! the domain API defined by `docs/spec/review-workflows.md`.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, ReviewChangeRequestNumber, ReviewConfidence,
    ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalLinkAttachment, ReviewExternalLinkId, ReviewExternalLinkObservation,
    ReviewExternalObjectKind, ReviewExternalObjectState, ReviewFinding, ReviewFindingContent,
    ReviewFindingDiffSide, ReviewFindingEvent, ReviewFindingEventKind,
    ReviewFindingExternalLinkRef, ReviewFindingId, ReviewFindingLocation, ReviewFindingProposal,
    ReviewFindingRef, ReviewFindingSeverity, ReviewFindingStatus, ReviewKey, ReviewLineRange,
    ReviewPass, ReviewPassEvidence, ReviewPassId, ReviewPassKind, ReviewPassReconstitutionInput,
    ReviewPassRef, ReviewPassState, ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy,
    ReviewPolicyVersion, ReviewRun, ReviewRunId, ReviewRunReconstitutionInput, ReviewRunRef,
    ReviewRunState, ReviewTarget, ReviewTargetId, ReviewTargetParentRef, ReviewTargetSubject,
    ReviewText, ReviewWorkflowKind, SessionId, TurnId,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

/// PostgreSQL adapter for the review-workflow bounded context.
#[derive(Clone, Debug)]
pub struct ReviewWorkflowStore {
    pool: PgPool,
}

impl ReviewWorkflowStore {
    /// Binds the store to the guarded application pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts one immutable target snapshot.
    pub async fn insert_target(
        &self,
        target: &ReviewTarget,
    ) -> Result<(), ReviewWorkflowStoreError> {
        let (subject_kind, change_request_number) = match target.subject() {
            ReviewTargetSubject::ChangeRequest(number) => {
                ("change_request", Some(Decimal::from(number.get())))
            }
            ReviewTargetSubject::Commit => ("commit", None),
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_target
                (target_id, provider_key, repository_key, subject_kind,
                 change_request_number, head_revision, base_revision,
                 stack_parent_target_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(target.id().into_uuid())
        .bind(target.provider().as_str())
        .bind(target.repository().as_str())
        .bind(subject_kind)
        .bind(change_request_number)
        .bind(target.head_revision().as_str())
        .bind(target.base_revision().map(ReviewKey::as_str))
        .bind(
            target
                .stack_parent()
                .map(|parent| parent.target().into_uuid()),
        )
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }

    /// Loads and validates one target snapshot.
    pub async fn load_target(
        &self,
        target: ReviewTargetId,
    ) -> Result<Option<ReviewTarget>, ReviewWorkflowStoreError> {
        let row = sqlx::query(
            "SELECT target.target_id, target.provider_key,
                    target.repository_key, target.subject_kind,
                    target.change_request_number, target.head_revision,
                    target.base_revision, target.stack_parent_target_id,
                    parent.provider_key AS stack_parent_provider_key,
                    parent.repository_key AS stack_parent_repository_key
               FROM review_target AS target
               LEFT JOIN review_target AS parent
                 ON parent.target_id = target.stack_parent_target_id
              WHERE target.target_id = $1",
        )
        .bind(target.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| decode_target(&row)).transpose()
    }

    /// Inserts one queued run with its complete frozen policy.
    pub async fn insert_run(&self, run: &ReviewRun) -> Result<(), ReviewWorkflowStoreError> {
        if run.state() != ReviewRunState::Queued {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::RunNotQueued { state: run.state() },
            ));
        }
        let (state_kind, state_pass_id) = encode_run_state(run.state());
        let policy = run.policy();
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_run
                (run_id, target_id, workflow_kind, policy_version,
                 minimum_judge_confidence, minimum_publication_confidence,
                 state_kind, state_pass_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(run.reference().run().into_uuid())
        .bind(run.reference().target().into_uuid())
        .bind(encode_workflow_kind(run.workflow()))
        .bind(i64::from(policy.version().get()))
        .bind(i32::from(policy.minimum_judge_confidence().basis_points()))
        .bind(i32::from(
            policy.minimum_publication_confidence().basis_points(),
        ))
        .bind(state_kind)
        .bind(state_pass_id.map(ReviewPassId::into_uuid))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }

    /// Loads and validates one run projection.
    pub async fn load_run(
        &self,
        run: ReviewRunId,
    ) -> Result<Option<ReviewRun>, ReviewWorkflowStoreError> {
        let row = sqlx::query(
            "SELECT workflow_run.run_id, workflow_run.target_id,
                    workflow_run.workflow_kind, workflow_run.policy_version,
                    workflow_run.minimum_judge_confidence,
                    workflow_run.minimum_publication_confidence,
                    workflow_run.state_kind, workflow_run.state_pass_id,
                    canonical_target.target_id AS canonical_target_id,
                    canonical_pass.pass_id AS evidence_pass_id,
                    canonical_pass.run_id AS evidence_pass_run_id,
                    canonical_pass.target_id AS evidence_pass_target_id,
                    canonical_pass.pass_kind AS evidence_pass_kind,
                    canonical_pass.state_kind AS evidence_pass_state_kind,
                    canonical_pass.turn_id AS evidence_pass_turn_id,
                    canonical_pass.output_frontier_id
                        AS evidence_pass_output_frontier_id
               FROM review_run AS workflow_run
               LEFT JOIN review_target AS canonical_target
                 ON canonical_target.target_id = workflow_run.target_id
               LEFT JOIN review_pass AS canonical_pass
                 ON canonical_pass.pass_id = workflow_run.state_pass_id
              WHERE workflow_run.run_id = $1",
        )
        .bind(run.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            require_joined_reference(
                &row,
                "canonical_target_id",
                "review_run",
                "referenced target row is missing",
            )?;
            decode_run(row)
        })
        .transpose()
    }

    /// Applies one domain-validated run transition under row lock.
    pub async fn transition_run(
        &self,
        run: ReviewRunId,
        next: ReviewRunState,
    ) -> Result<Option<ReviewRun>, ReviewWorkflowStoreError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(crate::lock_inventory::REVIEW_RUN_TRANSITION)
            .bind(run.into_uuid())
            .bind(encode_run_state(next).1.map(ReviewPassId::into_uuid))
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let (current, pass_evidence) = decode_run_for_transition(row)?;
        let transitioned = current.transition(next, pass_evidence).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Run(error))
        })?;
        let (state_kind, state_pass) = encode_run_state(transitioned.state());
        sqlx::query(
            "UPDATE review_run
                SET state_kind = $2,
                    state_pass_id = $3
              WHERE run_id = $1",
        )
        .bind(run.into_uuid())
        .bind(state_kind)
        .bind(state_pass.map(ReviewPassId::into_uuid))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(transitioned))
    }

    /// Inserts one pass after its exact session input has been accepted.
    pub async fn insert_pass(&self, pass: &ReviewPass) -> Result<(), ReviewWorkflowStoreError> {
        if pass.state() != ReviewPassState::Queued {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::PassNotQueued {
                    state: pass.state(),
                },
            ));
        }
        let state = encode_pass_state(pass.state());
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_pass
                (pass_id, run_id, target_id, pass_kind, session_id,
                 accepted_input_id, state_kind, turn_id, output_frontier_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(pass.reference().pass().into_uuid())
        .bind(pass.reference().run().run().into_uuid())
        .bind(pass.reference().target().into_uuid())
        .bind(encode_pass_kind(pass.kind()))
        .bind(pass.session().into_uuid())
        .bind(pass.accepted_input().into_uuid())
        .bind(state.kind)
        .bind(state.turn.map(TurnId::into_uuid))
        .bind(state.frontier.map(ContextFrontierId::into_uuid))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }

    /// Loads and validates one pass projection.
    pub async fn load_pass(
        &self,
        pass: ReviewPassId,
    ) -> Result<Option<ReviewPass>, ReviewWorkflowStoreError> {
        let row = sqlx::query(
            "SELECT workflow_pass.pass_id, workflow_pass.run_id,
                    workflow_pass.target_id, workflow_pass.pass_kind,
                    canonical_run.workflow_kind AS run_workflow_kind,
                    workflow_pass.session_id AS pass_session_id,
                    workflow_pass.accepted_input_id, workflow_pass.state_kind,
                    workflow_pass.turn_id, workflow_pass.output_frontier_id,
                    canonical_input.session_id AS accepted_input_session_id,
                    canonical_turn.turn_id AS evidence_turn_id,
                    canonical_turn.session_id AS turn_session_id,
                    canonical_turn.origin_accepted_input_id
                        AS turn_accepted_input_id,
                    canonical_turn.state_kind AS turn_state_kind,
                    canonical_turn.terminal_disposition_kind
                        AS turn_terminal_disposition_kind,
                    canonical_turn.terminal_frontier_id
                        AS turn_terminal_frontier_id,
                    canonical_run.run_id AS canonical_run_id,
                    canonical_run.target_id AS canonical_run_target_id
               FROM review_pass AS workflow_pass
               LEFT JOIN review_run AS canonical_run
                 ON canonical_run.run_id = workflow_pass.run_id
                AND canonical_run.target_id = workflow_pass.target_id
               LEFT JOIN accepted_input AS canonical_input
                 ON canonical_input.accepted_input_id =
                    workflow_pass.accepted_input_id
               LEFT JOIN turn_lifecycle AS canonical_turn
                 ON canonical_turn.turn_id = workflow_pass.turn_id
              WHERE workflow_pass.pass_id = $1",
        )
        .bind(pass.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            require_joined_reference(
                &row,
                "canonical_run_id",
                "review_pass",
                "referenced run row is missing",
            )?;
            decode_pass(row)
        })
        .transpose()
    }

    /// Applies one domain-validated pass transition under row lock.
    pub async fn transition_pass(
        &self,
        pass: ReviewPassId,
        next: ReviewPassState,
    ) -> Result<Option<ReviewPass>, ReviewWorkflowStoreError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(crate::lock_inventory::REVIEW_PASS_TRANSITION)
            .bind(pass.into_uuid())
            .bind(encode_pass_state(next).turn.map(TurnId::into_uuid))
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let (current, turn_evidence) = decode_pass_for_transition(row)?;
        let transitioned = current.transition(next, turn_evidence).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Pass(error))
        })?;
        let state = encode_pass_state(transitioned.state());
        sqlx::query(
            "UPDATE review_pass
                SET state_kind = $2,
                    turn_id = $3,
                    output_frontier_id = $4
              WHERE pass_id = $1",
        )
        .bind(pass.into_uuid())
        .bind(state.kind)
        .bind(state.turn.map(TurnId::into_uuid))
        .bind(state.frontier.map(ContextFrontierId::into_uuid))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(transitioned))
    }

    /// Applies one pass transition and its matching run projection atomically.
    pub async fn transition_run_and_pass(
        &self,
        run: ReviewRunId,
        pass: ReviewPassId,
        next_run: ReviewRunState,
        next_pass: ReviewPassState,
    ) -> Result<Option<(ReviewRun, ReviewPass)>, ReviewWorkflowStoreError> {
        let mut transaction = self.pool.begin().await?;
        let run_row = sqlx::query(crate::lock_inventory::REVIEW_RUN_TRANSITION)
            .bind(run.into_uuid())
            .bind(encode_run_state(next_run).1.map(ReviewPassId::into_uuid))
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(run_row) = run_row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let pass_row = sqlx::query(crate::lock_inventory::REVIEW_PASS_TRANSITION)
            .bind(pass.into_uuid())
            .bind(encode_pass_state(next_pass).turn.map(TurnId::into_uuid))
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(pass_row) = pass_row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let (current_run, _) = decode_run_for_transition(run_row)?;
        let (current_pass, turn_evidence) = decode_pass_for_transition(pass_row)?;
        let transitioned_pass =
            current_pass
                .transition(next_pass, turn_evidence)
                .map_err(|error| {
                    ReviewWorkflowStoreError::InvalidTransition(
                        ReviewWorkflowTransitionError::Pass(error),
                    )
                })?;
        let pass_evidence = ReviewPassEvidence::new(
            transitioned_pass.reference(),
            transitioned_pass.kind(),
            current_run.policy(),
            transitioned_pass.state(),
        );
        let run_pass_evidence = match next_run {
            ReviewRunState::Queued | ReviewRunState::Cancelled { last_pass: None } => None,
            _ => Some(pass_evidence),
        };
        let transitioned_run = current_run
            .transition(next_run, run_pass_evidence)
            .map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Run(
                    error,
                ))
            })?;

        let encoded_pass = encode_pass_state(transitioned_pass.state());
        sqlx::query(
            "UPDATE review_pass
                SET state_kind = $2,
                    turn_id = $3,
                    output_frontier_id = $4
              WHERE pass_id = $1",
        )
        .bind(pass.into_uuid())
        .bind(encoded_pass.kind)
        .bind(encoded_pass.turn.map(TurnId::into_uuid))
        .bind(encoded_pass.frontier.map(ContextFrontierId::into_uuid))
        .execute(&mut *transaction)
        .await?;
        let (run_state_kind, run_state_pass) = encode_run_state(transitioned_run.state());
        sqlx::query(
            "UPDATE review_run
                SET state_kind = $2,
                    state_pass_id = $3
              WHERE run_id = $1",
        )
        .bind(run.into_uuid())
        .bind(run_state_kind)
        .bind(run_state_pass.map(ReviewPassId::into_uuid))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some((transitioned_run, transitioned_pass)))
    }

    /// Inserts immutable finding content in its initial open state.
    pub async fn insert_finding(
        &self,
        finding: &ReviewFinding,
    ) -> Result<(), ReviewWorkflowStoreError> {
        if finding.status() != ReviewFindingStatus::Open {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::FindingNotOpen {
                    status: finding.status(),
                },
            ));
        }
        let proposal = finding.proposal();
        let reference = proposal.reference();
        let pass = proposal.producing_pass();
        let content = proposal.content();
        let location = content.location();
        let (line_start, line_end) = match location.line_range() {
            Some(range) => (Some(i64::from(range.start())), Some(i64::from(range.end()))),
            None => (None, None),
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_finding
                (finding_id, run_id, target_id, producing_pass_id, file_path,
                 line_start, line_end, diff_side, title, body, severity,
                 confidence, category, recommended_fix)
             VALUES (
                 $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
             )",
        )
        .bind(reference.finding().into_uuid())
        .bind(reference.run().run().into_uuid())
        .bind(reference.target().into_uuid())
        .bind(pass.reference().pass().into_uuid())
        .bind(location.file_path().as_str())
        .bind(line_start)
        .bind(line_end)
        .bind(location.diff_side().map(encode_diff_side))
        .bind(content.title().as_str())
        .bind(content.body().as_str())
        .bind(encode_severity(content.severity()))
        .bind(i32::from(content.confidence().basis_points()))
        .bind(content.category().as_str())
        .bind(content.recommended_fix().map(ReviewText::as_str))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }

    /// Appends one event after domain validation of the complete current history.
    pub async fn append_finding_event(
        &self,
        finding: ReviewFindingId,
        event: ReviewFindingEvent,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let Some(current) = self.load_finding(finding).await? else {
            return Ok(None);
        };
        let next = current.apply(event.clone()).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(
                error,
            ))
        })?;
        let mut transaction = self.pool.begin().await?;
        insert_finding_event(&mut transaction, &event).await?;
        commit_mutation(transaction).await?;
        Ok(Some(next))
    }

    /// Loads and validates immutable finding content plus complete event history.
    pub async fn load_finding(
        &self,
        finding: ReviewFindingId,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT finding.finding_id, finding.run_id, finding.target_id,
                    finding.producing_pass_id, finding.file_path,
                    finding.line_start, finding.line_end, finding.diff_side,
                    finding.title, finding.body, finding.severity,
                    finding.confidence, finding.category,
                    finding.recommended_fix,
                    target.provider_key, target.repository_key,
                    target.subject_kind, target.change_request_number,
                    target.head_revision, target.base_revision,
                    target.stack_parent_target_id,
                    target.target_id AS canonical_target_id,
                    parent.provider_key AS stack_parent_provider_key,
                    parent.repository_key AS stack_parent_repository_key,
                    producing_pass.pass_id
                        AS canonical_producing_pass_id,
                    producing_pass.pass_kind AS producing_pass_kind,
                    producing_pass.state_kind AS producing_pass_state_kind,
                    producing_pass.turn_id AS producing_pass_turn_id,
                    producing_pass.output_frontier_id
                        AS producing_pass_output_frontier_id,
                    producing_run.policy_version
                        AS producing_policy_version,
                    producing_run.minimum_judge_confidence
                        AS producing_minimum_judge_confidence,
                    producing_run.minimum_publication_confidence
                        AS producing_minimum_publication_confidence
               FROM review_finding AS finding
               LEFT JOIN review_target AS target
                 ON target.target_id = finding.target_id
               LEFT JOIN review_target AS parent
                 ON parent.target_id = target.stack_parent_target_id
               LEFT JOIN review_pass AS producing_pass
                 ON producing_pass.pass_id = finding.producing_pass_id
                AND producing_pass.run_id = finding.run_id
                AND producing_pass.target_id = finding.target_id
               LEFT JOIN review_run AS producing_run
                 ON producing_run.run_id = producing_pass.run_id
                AND producing_run.target_id = producing_pass.target_id
              WHERE finding.finding_id = $1",
        )
        .bind(finding.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        require_joined_reference(
            &row,
            "canonical_target_id",
            "review_finding",
            "referenced target row is missing",
        )?;
        require_joined_reference(
            &row,
            "canonical_producing_pass_id",
            "review_finding",
            "producing pass row is missing",
        )?;
        let proposal = decode_finding_proposal(&row)?;
        let event_rows = sqlx::query(
            "SELECT event.finding_id, event.event_ordinal,
                    event.finding_run_id, event.target_id,
                    event.event_pass_id, event.event_pass_run_id,
                    event_pass.pass_id AS canonical_event_pass_id,
                    event_pass.pass_kind AS event_pass_kind,
                    event_pass.state_kind AS event_pass_state_kind,
                    event_pass.turn_id AS event_pass_turn_id,
                    event_pass.output_frontier_id
                        AS event_pass_output_frontier_id,
                    event_run.policy_version AS event_policy_version,
                    event_run.minimum_judge_confidence
                        AS event_minimum_judge_confidence,
                    event_run.minimum_publication_confidence
                        AS event_minimum_publication_confidence,
                    event.event_kind, event.reason,
                    event.referenced_finding_id, event.external_link_id,
                    referenced_finding.producing_pass_id
                        AS referenced_finding_pass_id,
                    canonical_link.external_link_id AS canonical_link_id,
                    canonical_link.target_id AS link_target_id,
                    canonical_link.association_kind AS link_association_kind,
                    canonical_link.run_id AS link_run_id,
                    canonical_link.finding_id AS link_finding_id,
                    link_finding.producing_pass_id
                        AS link_finding_producing_pass_id,
                    canonical_link.provider_key AS link_provider_key,
                    canonical_link.object_kind AS link_object_kind,
                    attachment.external_link_id
                        AS attachment_external_link_id,
                    attachment.target_id AS attachment_target_id,
                    attachment.pass_run_id AS attachment_pass_run_id,
                    attachment.pass_id AS attachment_pass_id,
                    attachment_pass.pass_kind AS attachment_pass_kind,
                    attachment_pass.state_kind AS attachment_pass_state_kind,
                    attachment_pass.turn_id AS attachment_pass_turn_id,
                    attachment_pass.output_frontier_id
                        AS attachment_pass_output_frontier_id,
                    attachment_run.policy_version
                        AS attachment_policy_version,
                    attachment_run.minimum_judge_confidence
                        AS attachment_minimum_judge_confidence,
                    attachment_run.minimum_publication_confidence
                        AS attachment_minimum_publication_confidence,
                    attachment.external_object_key
                        AS attachment_external_object_key
               FROM review_finding_event AS event
               LEFT JOIN review_pass AS event_pass
                 ON event_pass.pass_id = event.event_pass_id
                AND event_pass.run_id = event.event_pass_run_id
                AND event_pass.target_id = event.target_id
               LEFT JOIN review_run AS event_run
                 ON event_run.run_id = event_pass.run_id
                AND event_run.target_id = event_pass.target_id
               LEFT JOIN review_finding AS referenced_finding
                 ON referenced_finding.finding_id =
                    event.referenced_finding_id
                AND referenced_finding.run_id = event.finding_run_id
                AND referenced_finding.target_id = event.target_id
               LEFT JOIN review_external_link AS canonical_link
                 ON canonical_link.external_link_id = event.external_link_id
               LEFT JOIN review_finding AS link_finding
                 ON link_finding.finding_id = canonical_link.finding_id
                AND link_finding.run_id = canonical_link.run_id
                AND link_finding.target_id = canonical_link.target_id
               LEFT JOIN review_external_link_attachment AS attachment
                 ON attachment.external_link_id =
                    canonical_link.external_link_id
               LEFT JOIN review_pass AS attachment_pass
                 ON attachment_pass.pass_id = attachment.pass_id
                AND attachment_pass.run_id = attachment.pass_run_id
                AND attachment_pass.target_id = attachment.target_id
               LEFT JOIN review_run AS attachment_run
                 ON attachment_run.run_id = attachment_pass.run_id
                AND attachment_run.target_id = attachment_pass.target_id
              WHERE event.finding_id = $1
              ORDER BY event.event_ordinal",
        )
        .bind(finding.into_uuid())
        .fetch_all(&mut *transaction)
        .await?;
        let events = event_rows
            .into_iter()
            .map(|row| {
                require_joined_reference(
                    &row,
                    "canonical_event_pass_id",
                    "review_finding_event",
                    "event pass row is missing",
                )?;
                decode_finding_event(&row, proposal.reference())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let finding = ReviewFinding::try_reconstitute(proposal, events).map_err(|error| {
            corruption(
                "review_finding",
                format!("domain reconstitution failed: {:?}", error.failure()),
            )
        })?;
        transaction.commit().await?;
        Ok(Some(finding))
    }

    /// Idempotently inserts one canonical pre-effect external-link reservation.
    pub async fn reserve_external_link(
        &self,
        requested: ReviewExternalLink,
    ) -> Result<ReserveExternalLinkOutcome, ReviewWorkflowStoreError> {
        if requested.attachment().is_some() || !requested.observations().is_empty() {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::ExternalLinkNotPending,
            ));
        }
        let association = encode_link_association(requested.association());
        let result = sqlx::query(
            "INSERT INTO review_external_link
                (external_link_id, target_id, association_kind, run_id,
                 finding_id, provider_key, object_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (external_link_id) DO NOTHING",
        )
        .bind(requested.id().into_uuid())
        .bind(requested.association().target().into_uuid())
        .bind(association.kind)
        .bind(association.run.map(ReviewRunId::into_uuid))
        .bind(association.finding.map(ReviewFindingId::into_uuid))
        .bind(requested.provider().as_str())
        .bind(encode_external_object_kind(requested.object_kind()))
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            return Ok(ReserveExternalLinkOutcome::Inserted(requested));
        }
        let existing = self
            .load_external_link(requested.id())
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("conflicting reservation disappeared"),
                )
            })?;
        if same_reservation(&existing, &requested) {
            Ok(ReserveExternalLinkOutcome::Existing(existing))
        } else {
            Err(ReviewWorkflowStoreError::ReservationConflict(
                ReviewExternalLinkReservationConflict {
                    existing: Box::new(existing),
                    requested: Box::new(requested),
                },
            ))
        }
    }

    /// Appends the one immutable external-object attachment.
    pub async fn attach_external_link(
        &self,
        link: ReviewExternalLinkId,
        attachment: ReviewExternalLinkAttachment,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let Some(current) = self.load_external_link(link).await? else {
            return Ok(None);
        };
        let next = current.attach(attachment.clone()).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(
                ReviewWorkflowTransitionError::ExternalLink(error),
            )
        })?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_external_link_attachment
                (external_link_id, target_id, pass_run_id, pass_id,
                 provider_key, object_kind, external_object_key)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(attachment.link().into_uuid())
        .bind(next.association().target().into_uuid())
        .bind(attachment.pass().run().run().into_uuid())
        .bind(attachment.pass().pass().into_uuid())
        .bind(next.provider().as_str())
        .bind(encode_external_object_kind(next.object_kind()))
        .bind(attachment.external_object().as_str())
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(next))
    }

    /// Appends one same-target external-state observation.
    pub async fn append_external_observation(
        &self,
        link: ReviewExternalLinkId,
        observation: ReviewExternalLinkObservation,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let Some(current) = self.load_external_link(link).await? else {
            return Ok(None);
        };
        let next = current.observe(observation).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(
                ReviewWorkflowTransitionError::ExternalLink(error),
            )
        })?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_external_link_observation
                (external_link_id, observation_ordinal, target_id,
                 pass_run_id, pass_id, object_state)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(observation.link().into_uuid())
        .bind(i64::from(observation.ordinal().get()))
        .bind(next.association().target().into_uuid())
        .bind(observation.pass().run().run().into_uuid())
        .bind(observation.pass().pass().into_uuid())
        .bind(encode_external_object_state(observation.state()))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(next))
    }

    /// Loads and validates a reservation, optional attachment, and observations.
    pub async fn load_external_link(
        &self,
        link: ReviewExternalLinkId,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT link.external_link_id, link.target_id,
                    link.association_kind, link.run_id, link.finding_id,
                    canonical_target.target_id AS canonical_target_id,
                    canonical_run.run_id AS canonical_run_id,
                    association_finding.producing_pass_id
                        AS finding_producing_pass_id,
                    link.provider_key, link.object_kind
               FROM review_external_link AS link
               LEFT JOIN review_target AS canonical_target
                 ON canonical_target.target_id = link.target_id
               LEFT JOIN review_run AS canonical_run
                 ON canonical_run.run_id = link.run_id
                AND canonical_run.target_id = link.target_id
               LEFT JOIN review_finding AS association_finding
                 ON association_finding.finding_id = link.finding_id
                AND association_finding.run_id = link.run_id
                AND association_finding.target_id = link.target_id
              WHERE link.external_link_id = $1",
        )
        .bind(link.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        require_joined_reference(
            &row,
            "canonical_target_id",
            "review_external_link",
            "referenced target row is missing",
        )?;
        if row.try_get::<Option<Uuid>, _>("run_id")?.is_some() {
            require_joined_reference(
                &row,
                "canonical_run_id",
                "review_external_link",
                "referenced run row is missing",
            )?;
        }
        let (id, association, provider, kind) = decode_external_link_root(&row)?;
        let attachment = sqlx::query(
            "SELECT attachment.external_link_id, attachment.pass_run_id,
                    attachment.pass_id, attachment.target_id,
                    pass.pass_id AS canonical_attachment_pass_id,
                    pass.pass_kind, pass.state_kind AS pass_state_kind,
                    pass.turn_id AS pass_turn_id,
                    pass.output_frontier_id AS pass_output_frontier_id,
                    pass_run.policy_version AS pass_policy_version,
                    pass_run.minimum_judge_confidence
                        AS pass_minimum_judge_confidence,
                    pass_run.minimum_publication_confidence
                        AS pass_minimum_publication_confidence,
                    attachment.external_object_key
               FROM review_external_link_attachment AS attachment
               LEFT JOIN review_pass AS pass
                 ON pass.pass_id = attachment.pass_id
                AND pass.run_id = attachment.pass_run_id
                AND pass.target_id = attachment.target_id
               LEFT JOIN review_run AS pass_run
                 ON pass_run.run_id = pass.run_id
                AND pass_run.target_id = pass.target_id
              WHERE attachment.external_link_id = $1",
        )
        .bind(link.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?
        .map(|row| {
            require_joined_reference(
                &row,
                "canonical_attachment_pass_id",
                "review_external_link_attachment",
                "attaching pass row is missing",
            )?;
            decode_external_link_attachment(&row)
        })
        .transpose()?;
        let observations = sqlx::query(
            "SELECT observation.external_link_id,
                    observation.observation_ordinal,
                    observation.pass_run_id, observation.pass_id,
                    observation.target_id, observation.object_state,
                    pass.pass_id AS canonical_observation_pass_id,
                    pass.pass_kind, pass.state_kind AS pass_state_kind,
                    pass.turn_id AS pass_turn_id,
                    pass.output_frontier_id AS pass_output_frontier_id,
                    pass_run.policy_version AS pass_policy_version,
                    pass_run.minimum_judge_confidence
                        AS pass_minimum_judge_confidence,
                    pass_run.minimum_publication_confidence
                        AS pass_minimum_publication_confidence
               FROM review_external_link_observation AS observation
               LEFT JOIN review_pass AS pass
                 ON pass.pass_id = observation.pass_id
                AND pass.run_id = observation.pass_run_id
                AND pass.target_id = observation.target_id
               LEFT JOIN review_run AS pass_run
                 ON pass_run.run_id = pass.run_id
                AND pass_run.target_id = pass.target_id
              WHERE observation.external_link_id = $1
              ORDER BY observation.observation_ordinal",
        )
        .bind(link.into_uuid())
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .map(|row| {
            require_joined_reference(
                &row,
                "canonical_observation_pass_id",
                "review_external_link_observation",
                "observing pass row is missing",
            )?;
            decode_external_link_observation(&row)
        })
        .collect::<Result<Vec<_>, _>>()?;
        let link = ReviewExternalLink::try_reconstitute(
            id,
            association,
            provider,
            kind,
            attachment,
            observations,
        )
        .map_err(|error| corruption("review_external_link", format!("{error:?}")))?;
        transaction.commit().await?;
        Ok(Some(link))
    }
}

async fn commit_mutation(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), ReviewWorkflowStoreError> {
    transaction
        .commit()
        .await
        .map_err(classify_mutating_commit_error)
}

fn classify_mutating_commit_error(error: sqlx::Error) -> ReviewWorkflowStoreError {
    if crate::commit_failure_is_ambiguous(&error) {
        ReviewWorkflowStoreError::CommitAmbiguous(error)
    } else {
        ReviewWorkflowStoreError::Database(error)
    }
}

fn require_joined_reference(
    row: &PgRow,
    column: &str,
    aggregate: &'static str,
    detail: &'static str,
) -> Result<(), ReviewWorkflowStoreError> {
    if row.try_get::<Option<Uuid>, _>(column)?.is_none() {
        return Err(corruption(aggregate, String::from(detail)));
    }
    Ok(())
}

async fn begin_repeatable_read(
    pool: &PgPool,
) -> Result<Transaction<'_, Postgres>, ReviewWorkflowStoreError> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

async fn insert_finding_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &ReviewFindingEvent,
) -> Result<(), ReviewWorkflowStoreError> {
    let finding = event.finding();
    let encoded = encode_finding_event(event.kind());
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, external_link_id,
             external_link_association_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(finding.finding().into_uuid())
    .bind(i64::from(event.ordinal().get()))
    .bind(finding.run().run().into_uuid())
    .bind(finding.target().into_uuid())
    .bind(event.pass().pass().into_uuid())
    .bind(event.pass().run().run().into_uuid())
    .bind(encoded.kind)
    .bind(encoded.reason)
    .bind(encoded.referenced_finding.map(ReviewFindingId::into_uuid))
    .bind(encoded.external_link.map(ReviewExternalLinkId::into_uuid))
    .bind(encoded.external_link.map(|_| "finding"))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn decode_target(row: &PgRow) -> Result<ReviewTarget, ReviewWorkflowStoreError> {
    let id = target_id(row.try_get("target_id")?);
    let provider = review_key(row.try_get("provider_key")?, "review_target")?;
    let repository = review_key(row.try_get("repository_key")?, "review_target")?;
    let subject_kind: String = row.try_get("subject_kind")?;
    let change_request_number: Option<Decimal> = row.try_get("change_request_number")?;
    let subject = match (subject_kind.as_str(), change_request_number) {
        ("change_request", Some(number)) => ReviewTargetSubject::ChangeRequest(
            ReviewChangeRequestNumber::try_new(decimal_u64(number, "review_target")?)
                .map_err(|_| corruption("review_target", String::from("zero change request")))?,
        ),
        ("commit", None) => ReviewTargetSubject::Commit,
        _ => {
            return Err(corruption(
                "review_target",
                format!("invalid subject shape {subject_kind}"),
            ));
        }
    };
    let stack_parent = match (
        row.try_get::<Option<Uuid>, _>("stack_parent_target_id")?,
        row.try_get::<Option<String>, _>("stack_parent_provider_key")?,
        row.try_get::<Option<String>, _>("stack_parent_repository_key")?,
    ) {
        (None, None, None) => None,
        (Some(target), Some(parent_provider), Some(parent_repository)) => {
            Some(ReviewTargetParentRef::new(
                target_id(target),
                review_key(parent_provider, "review_target")?,
                review_key(parent_repository, "review_target")?,
            ))
        }
        _ => {
            return Err(corruption(
                "review_target",
                String::from("torn canonical stack-parent evidence"),
            ));
        }
    };
    ReviewTarget::try_new(
        id,
        provider,
        repository,
        subject,
        review_key(row.try_get("head_revision")?, "review_target")?,
        row.try_get::<Option<String>, _>("base_revision")?
            .map(|value| review_key(value, "review_target"))
            .transpose()?,
        stack_parent,
    )
    .map_err(|error| corruption("review_target", format!("{error:?}")))
}

fn decode_run(row: PgRow) -> Result<ReviewRun, ReviewWorkflowStoreError> {
    let pass_evidence = decode_run_pass_evidence(&row)?;
    reconstitute_run(&row, pass_evidence)
}

fn decode_run_for_transition(
    row: PgRow,
) -> Result<(ReviewRun, Option<ReviewPassEvidence>), ReviewWorkflowStoreError> {
    let canonical_evidence = decode_run_pass_evidence(&row)?;
    let (_, _, _, state) = decode_run_facts(&row)?;
    let current_evidence = projected_current_run_pass_evidence(state, canonical_evidence)?;
    Ok((
        reconstitute_run(&row, current_evidence)?,
        canonical_evidence,
    ))
}

fn reconstitute_run(
    row: &PgRow,
    pass_evidence: Option<ReviewPassEvidence>,
) -> Result<ReviewRun, ReviewWorkflowStoreError> {
    let (reference, workflow, policy, state) = decode_run_facts(row)?;
    ReviewRun::try_reconstitute(ReviewRunReconstitutionInput::new(
        reference,
        workflow,
        policy,
        state,
        pass_evidence,
    ))
    .map_err(|error| {
        corruption(
            "review_run",
            format!("domain reconstitution failed: {:?}", error.failure()),
        )
    })
}

fn decode_run_facts(
    row: &PgRow,
) -> Result<
    (
        ReviewRunRef,
        ReviewWorkflowKind,
        ReviewPolicy,
        ReviewRunState,
    ),
    ReviewWorkflowStoreError,
> {
    let reference = ReviewRunRef::new(
        target_id(row.try_get("target_id")?),
        run_id(row.try_get("run_id")?),
    );
    let policy = decode_review_policy(
        row,
        "policy_version",
        "minimum_judge_confidence",
        "minimum_publication_confidence",
        "review_run",
    )?;
    let workflow: String = row.try_get("workflow_kind")?;
    let state_kind: String = row.try_get("state_kind")?;
    let state_pass: Option<Uuid> = row.try_get("state_pass_id")?;
    let state = decode_run_state(reference, &state_kind, state_pass)?;
    Ok((reference, decode_workflow_kind(&workflow)?, policy, state))
}

fn decode_run_pass_evidence(
    row: &PgRow,
) -> Result<Option<ReviewPassEvidence>, ReviewWorkflowStoreError> {
    let pass: Option<Uuid> = row.try_get("evidence_pass_id")?;
    let run: Option<Uuid> = row.try_get("evidence_pass_run_id")?;
    let target: Option<Uuid> = row.try_get("evidence_pass_target_id")?;
    let kind: Option<String> = row.try_get("evidence_pass_kind")?;
    let state_kind: Option<String> = row.try_get("evidence_pass_state_kind")?;
    let turn: Option<Uuid> = row.try_get("evidence_pass_turn_id")?;
    let frontier: Option<Uuid> = row.try_get("evidence_pass_output_frontier_id")?;
    match (pass, run, target, kind, state_kind) {
        (None, None, None, None, None) if turn.is_none() && frontier.is_none() => Ok(None),
        (Some(pass), Some(run), Some(target), Some(kind), Some(state_kind)) => {
            Ok(Some(ReviewPassEvidence::new(
                ReviewPassRef::new(
                    ReviewRunRef::new(target_id(target), run_id(run)),
                    pass_id(pass),
                ),
                decode_pass_kind(&kind)?,
                decode_review_policy(
                    row,
                    "policy_version",
                    "minimum_judge_confidence",
                    "minimum_publication_confidence",
                    "review_run",
                )?,
                decode_pass_state(&state_kind, turn, frontier)?,
            )))
        }
        _ => Err(corruption(
            "review_run",
            String::from("torn canonical pass evidence"),
        )),
    }
}

fn projected_current_run_pass_evidence(
    state: ReviewRunState,
    canonical: Option<ReviewPassEvidence>,
) -> Result<Option<ReviewPassEvidence>, ReviewWorkflowStoreError> {
    let Some(expected) = encode_run_state(state).1 else {
        return Ok(None);
    };
    let Some(canonical) = canonical else {
        return Err(corruption(
            "review_run",
            String::from("referenced pass row is missing"),
        ));
    };
    if canonical.reference().pass() != expected {
        return Err(corruption(
            "review_run",
            String::from("canonical pass identity mismatch"),
        ));
    }
    let projected = match state {
        ReviewRunState::Running { .. } => ReviewPassState::Running {
            turn: pass_state_turn(canonical.state()).ok_or_else(|| {
                corruption(
                    "review_run",
                    String::from("canonical pass has no turn for running projection"),
                )
            })?,
        },
        _ => canonical.state(),
    };
    Ok(Some(ReviewPassEvidence::new(
        canonical.reference(),
        canonical.kind(),
        canonical.policy(),
        projected,
    )))
}

fn decode_pass(row: PgRow) -> Result<ReviewPass, ReviewWorkflowStoreError> {
    let turn_evidence = decode_pass_turn_evidence(&row)?;
    reconstitute_pass(&row, turn_evidence)
}

fn decode_pass_for_transition(
    row: PgRow,
) -> Result<(ReviewPass, Option<ReviewPassTurnEvidence>), ReviewWorkflowStoreError> {
    let canonical_evidence = decode_pass_turn_evidence(&row)?;
    let state = decode_pass_row_state(&row)?;
    let current_evidence = projected_current_pass_turn_evidence(state, canonical_evidence)?;
    Ok((
        reconstitute_pass(&row, current_evidence)?,
        canonical_evidence,
    ))
}

fn reconstitute_pass(
    row: &PgRow,
    turn_evidence: Option<ReviewPassTurnEvidence>,
) -> Result<ReviewPass, ReviewWorkflowStoreError> {
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(row.try_get("target_id")?),
            run_id(row.try_get("run_id")?),
        ),
        pass_id(row.try_get("pass_id")?),
    );
    let state_kind: String = row.try_get("state_kind")?;
    let turn: Option<Uuid> = row.try_get("turn_id")?;
    let frontier: Option<Uuid> = row.try_get("output_frontier_id")?;
    let accepted_input_session = row
        .try_get::<Option<Uuid>, _>("accepted_input_session_id")?
        .ok_or_else(|| corruption("review_pass", String::from("accepted input row is missing")))?;
    let workflow_run_id = row
        .try_get::<Option<Uuid>, _>("canonical_run_id")?
        .ok_or_else(|| corruption("review_pass", String::from("referenced run row is missing")))?;
    let workflow_run_target = row
        .try_get::<Option<Uuid>, _>("canonical_run_target_id")?
        .ok_or_else(|| corruption("review_pass", String::from("referenced run row is missing")))?;
    ReviewPass::try_reconstitute(ReviewPassReconstitutionInput::new(
        reference,
        decode_pass_kind(&row.try_get::<String, _>("pass_kind")?)?,
        ReviewRunRef::new(target_id(workflow_run_target), run_id(workflow_run_id)),
        decode_workflow_kind(&row.try_get::<String, _>("run_workflow_kind")?)?,
        session_id(row.try_get("pass_session_id")?),
        accepted_input_id(row.try_get("accepted_input_id")?),
        session_id(accepted_input_session),
        decode_pass_state(&state_kind, turn, frontier)?,
        turn_evidence,
    ))
    .map_err(|error| {
        corruption(
            "review_pass",
            format!("domain reconstitution failed: {:?}", error.failure()),
        )
    })
}

fn decode_pass_row_state(row: &PgRow) -> Result<ReviewPassState, ReviewWorkflowStoreError> {
    decode_pass_state(
        &row.try_get::<String, _>("state_kind")?,
        row.try_get("turn_id")?,
        row.try_get("output_frontier_id")?,
    )
}

fn decode_pass_turn_evidence(
    row: &PgRow,
) -> Result<Option<ReviewPassTurnEvidence>, ReviewWorkflowStoreError> {
    let turn: Option<Uuid> = row.try_get("evidence_turn_id")?;
    let session: Option<Uuid> = row.try_get("turn_session_id")?;
    let accepted_input: Option<Uuid> = row.try_get("turn_accepted_input_id")?;
    let state: Option<String> = row.try_get("turn_state_kind")?;
    let disposition: Option<String> = row.try_get("turn_terminal_disposition_kind")?;
    let frontier: Option<Uuid> = row.try_get("turn_terminal_frontier_id")?;
    match (turn, session, accepted_input, state) {
        (None, None, None, None) if disposition.is_none() && frontier.is_none() => Ok(None),
        (Some(turn), Some(session), Some(accepted_input), Some(state)) => {
            Ok(Some(ReviewPassTurnEvidence::new(
                turn_id(turn),
                session_id(session),
                accepted_input_id(accepted_input),
                decode_turn_outcome(&state, disposition.as_deref())?,
                frontier.map(context_frontier_id),
            )))
        }
        _ => Err(corruption(
            "review_pass",
            String::from("torn canonical turn evidence"),
        )),
    }
}

fn projected_current_pass_turn_evidence(
    state: ReviewPassState,
    canonical: Option<ReviewPassTurnEvidence>,
) -> Result<Option<ReviewPassTurnEvidence>, ReviewWorkflowStoreError> {
    let Some(expected_turn) = pass_state_turn(state) else {
        return Ok(None);
    };
    let Some(canonical) = canonical else {
        return Err(corruption(
            "review_pass",
            String::from("referenced turn row is missing"),
        ));
    };
    if canonical.turn() != expected_turn {
        return Err(corruption(
            "review_pass",
            String::from("canonical turn identity mismatch"),
        ));
    }
    let (outcome, frontier) = match state {
        ReviewPassState::Running { .. } => (ReviewPassTurnOutcome::Active, None),
        _ => (canonical.outcome(), canonical.terminal_frontier()),
    };
    Ok(Some(ReviewPassTurnEvidence::new(
        canonical.turn(),
        canonical.session(),
        canonical.accepted_input(),
        outcome,
        frontier,
    )))
}

fn decode_turn_outcome(
    state: &str,
    disposition: Option<&str>,
) -> Result<ReviewPassTurnOutcome, ReviewWorkflowStoreError> {
    match (state, disposition) {
        ("active", None) => Ok(ReviewPassTurnOutcome::Active),
        ("terminal", Some("completed")) => Ok(ReviewPassTurnOutcome::Completed),
        ("terminal", Some("refused")) => Ok(ReviewPassTurnOutcome::Refused),
        ("terminal", Some("failed")) => Ok(ReviewPassTurnOutcome::Failed),
        ("terminal", Some("cancelled")) => Ok(ReviewPassTurnOutcome::Cancelled),
        ("terminal", Some("reconciliation_required")) => {
            Ok(ReviewPassTurnOutcome::ReconciliationRequired)
        }
        _ => Err(corruption(
            "review_pass",
            format!("invalid canonical turn outcome {state}/{disposition:?}"),
        )),
    }
}

fn decode_finding_proposal(row: &PgRow) -> Result<ReviewFindingProposal, ReviewWorkflowStoreError> {
    let run = ReviewRunRef::new(
        target_id(row.try_get("target_id")?),
        run_id(row.try_get("run_id")?),
    );
    let producing_pass_reference =
        ReviewPassRef::new(run, pass_id(row.try_get("producing_pass_id")?));
    let producing_pass = ReviewPassEvidence::new(
        producing_pass_reference,
        decode_pass_kind(&row.try_get::<String, _>("producing_pass_kind")?)?,
        decode_review_policy(
            row,
            "producing_policy_version",
            "producing_minimum_judge_confidence",
            "producing_minimum_publication_confidence",
            "review_finding",
        )?,
        decode_pass_state(
            &row.try_get::<String, _>("producing_pass_state_kind")?,
            row.try_get("producing_pass_turn_id")?,
            row.try_get("producing_pass_output_frontier_id")?,
        )?,
    );
    let reference = ReviewFindingRef::new(
        producing_pass_reference,
        finding_id(row.try_get("finding_id")?),
    );
    let line_start: Option<i64> = row.try_get("line_start")?;
    let line_end: Option<i64> = row.try_get("line_end")?;
    let line_range = match (line_start, line_end) {
        (None, None) => None,
        (Some(start), Some(end)) => Some(
            ReviewLineRange::try_new(
                positive_u32(start, "review_finding")?,
                positive_u32(end, "review_finding")?,
            )
            .map_err(|error| corruption("review_finding", format!("{error:?}")))?,
        ),
        _ => {
            return Err(corruption(
                "review_finding",
                String::from("torn line range"),
            ));
        }
    };
    let content = ReviewFindingContent::new(
        ReviewFindingLocation::new(
            review_key(row.try_get("file_path")?, "review_finding")?,
            line_range,
            row.try_get::<Option<String>, _>("diff_side")?
                .map(|side| decode_diff_side(&side))
                .transpose()?,
        ),
        review_text(row.try_get("title")?, "review_finding")?,
        review_text(row.try_get("body")?, "review_finding")?,
        decode_severity(&row.try_get::<String, _>("severity")?)?,
        confidence(row.try_get("confidence")?, "review_finding")?,
        review_key(row.try_get("category")?, "review_finding")?,
        row.try_get::<Option<String>, _>("recommended_fix")?
            .map(|value| review_text(value, "review_finding"))
            .transpose()?,
    );
    let target = decode_target(row)?;
    ReviewFindingProposal::try_new(reference, producing_pass, &target, content)
        .map_err(|error| corruption("review_finding", format!("{error:?}")))
}

fn decode_finding_event(
    row: &PgRow,
    finding: ReviewFindingRef,
) -> Result<ReviewFindingEvent, ReviewWorkflowStoreError> {
    let row_finding = finding_id(row.try_get("finding_id")?);
    let row_run = run_id(row.try_get("finding_run_id")?);
    let row_target = target_id(row.try_get("target_id")?);
    if row_finding != finding.finding()
        || row_run != finding.run().run()
        || row_target != finding.target()
    {
        return Err(corruption(
            "review_finding_event",
            String::from("finding ancestry mismatch"),
        ));
    }
    let pass_reference = ReviewPassRef::new(
        ReviewRunRef::new(row_target, run_id(row.try_get("event_pass_run_id")?)),
        pass_id(row.try_get("event_pass_id")?),
    );
    let pass = ReviewPassEvidence::new(
        pass_reference,
        decode_pass_kind(&row.try_get::<String, _>("event_pass_kind")?)?,
        decode_review_policy(
            row,
            "event_policy_version",
            "event_minimum_judge_confidence",
            "event_minimum_publication_confidence",
            "review_finding_event",
        )?,
        decode_pass_state(
            &row.try_get::<String, _>("event_pass_state_kind")?,
            row.try_get("event_pass_turn_id")?,
            row.try_get("event_pass_output_frontier_id")?,
        )?,
    );
    let kind: String = row.try_get("event_kind")?;
    let reason: Option<String> = row.try_get("reason")?;
    let referenced: Option<Uuid> = row.try_get("referenced_finding_id")?;
    let referenced_pass: Option<Uuid> = row.try_get("referenced_finding_pass_id")?;
    let external_link: Option<Uuid> = row.try_get("external_link_id")?;
    let kind = match (
        kind.as_str(),
        reason,
        referenced,
        referenced_pass,
        external_link,
    ) {
        ("accepted", None, None, None, None) => ReviewFindingEventKind::Accepted,
        ("rejected", Some(reason), None, None, None) => ReviewFindingEventKind::Rejected {
            reason: review_text(reason, "review_finding_event")?,
        },
        ("duplicate", None, Some(referenced), Some(referenced_pass), None) => {
            ReviewFindingEventKind::Duplicate {
                canonical: ReviewFindingRef::new(
                    ReviewPassRef::new(finding.run(), pass_id(referenced_pass)),
                    finding_id(referenced),
                ),
            }
        }
        ("superseded", None, Some(referenced), Some(referenced_pass), None) => {
            ReviewFindingEventKind::Superseded {
                successor: ReviewFindingRef::new(
                    ReviewPassRef::new(finding.run(), pass_id(referenced_pass)),
                    finding_id(referenced),
                ),
            }
        }
        ("stale", None, None, None, None) => ReviewFindingEventKind::Stale,
        ("posted", None, None, None, Some(link)) => ReviewFindingEventKind::Posted {
            link: decode_finding_external_link(row, finding, external_link_id(link))?,
        },
        ("fixed", None, None, None, None) => ReviewFindingEventKind::Fixed,
        ("blocked_with_reason", Some(reason), None, None, None) => {
            ReviewFindingEventKind::BlockedWithReason {
                reason: review_text(reason, "review_finding_event")?,
            }
        }
        _ => {
            return Err(corruption(
                "review_finding_event",
                format!("invalid event shape {kind}"),
            ));
        }
    };
    Ok(ReviewFindingEvent::new(
        finding,
        ReviewEventOrdinal::try_new(positive_u32(
            row.try_get("event_ordinal")?,
            "review_finding_event",
        )?)
        .map_err(|_| corruption("review_finding_event", String::from("zero ordinal")))?,
        pass,
        kind,
    ))
}

fn decode_finding_external_link(
    row: &PgRow,
    finding: ReviewFindingRef,
    event_link: ReviewExternalLinkId,
) -> Result<ReviewFindingExternalLinkRef, ReviewWorkflowStoreError> {
    let id = row
        .try_get::<Option<Uuid>, _>("canonical_link_id")?
        .map(external_link_id)
        .ok_or_else(|| {
            corruption(
                "review_finding_event",
                String::from("posted event canonical link is missing"),
            )
        })?;
    if id != event_link {
        return Err(corruption(
            "review_finding_event",
            String::from("posted event link identity mismatch"),
        ));
    }
    let target = row
        .try_get::<Option<Uuid>, _>("link_target_id")?
        .map(target_id)
        .ok_or_else(|| {
            corruption(
                "review_finding_event",
                String::from("posted event link target is missing"),
            )
        })?;
    let association_kind = row
        .try_get::<Option<String>, _>("link_association_kind")?
        .ok_or_else(|| {
            corruption(
                "review_finding_event",
                String::from("posted event link association is missing"),
            )
        })?;
    let run = row.try_get::<Option<Uuid>, _>("link_run_id")?;
    let canonical_finding = row.try_get::<Option<Uuid>, _>("link_finding_id")?;
    let canonical_finding_pass =
        row.try_get::<Option<Uuid>, _>("link_finding_producing_pass_id")?;
    let association = match (
        association_kind.as_str(),
        run,
        canonical_finding,
        canonical_finding_pass,
    ) {
        ("target", None, None, None) => ReviewExternalLinkAssociation::Target(target),
        ("run", Some(run), None, None) => {
            ReviewExternalLinkAssociation::Run(ReviewRunRef::new(target, run_id(run)))
        }
        ("finding", Some(run), Some(canonical_finding), Some(canonical_finding_pass)) => {
            ReviewExternalLinkAssociation::Finding(ReviewFindingRef::new(
                ReviewPassRef::new(
                    ReviewRunRef::new(target, run_id(run)),
                    pass_id(canonical_finding_pass),
                ),
                finding_id(canonical_finding),
            ))
        }
        _ => {
            return Err(corruption(
                "review_finding_event",
                format!("invalid posted link association shape {association_kind}"),
            ));
        }
    };
    let provider = row
        .try_get::<Option<String>, _>("link_provider_key")?
        .ok_or_else(|| {
            corruption(
                "review_finding_event",
                String::from("posted event link provider is missing"),
            )
        })?;
    let object_kind = row
        .try_get::<Option<String>, _>("link_object_kind")?
        .ok_or_else(|| {
            corruption(
                "review_finding_event",
                String::from("posted event link object kind is missing"),
            )
        })?;
    let attachment_link = row.try_get::<Option<Uuid>, _>("attachment_external_link_id")?;
    let attachment_target = row.try_get::<Option<Uuid>, _>("attachment_target_id")?;
    let attachment_run = row.try_get::<Option<Uuid>, _>("attachment_pass_run_id")?;
    let attachment_pass = row.try_get::<Option<Uuid>, _>("attachment_pass_id")?;
    let attachment_pass_kind = row.try_get::<Option<String>, _>("attachment_pass_kind")?;
    let attachment_pass_state = row.try_get::<Option<String>, _>("attachment_pass_state_kind")?;
    let attachment_pass_turn = row.try_get::<Option<Uuid>, _>("attachment_pass_turn_id")?;
    let attachment_pass_frontier =
        row.try_get::<Option<Uuid>, _>("attachment_pass_output_frontier_id")?;
    let attachment_object = row.try_get::<Option<String>, _>("attachment_external_object_key")?;
    let attachment = match (
        attachment_link,
        attachment_target,
        attachment_run,
        attachment_pass,
        attachment_pass_kind,
        attachment_pass_state,
        attachment_object,
    ) {
        (None, None, None, None, None, None, None)
            if attachment_pass_turn.is_none() && attachment_pass_frontier.is_none() =>
        {
            None
        }
        (
            Some(link),
            Some(target),
            Some(run),
            Some(pass),
            Some(kind),
            Some(state),
            Some(object),
        ) => Some(ReviewExternalLinkAttachment::new(
            external_link_id(link),
            ReviewPassEvidence::new(
                ReviewPassRef::new(
                    ReviewRunRef::new(target_id(target), run_id(run)),
                    pass_id(pass),
                ),
                decode_pass_kind(&kind)?,
                decode_review_policy(
                    row,
                    "attachment_policy_version",
                    "attachment_minimum_judge_confidence",
                    "attachment_minimum_publication_confidence",
                    "review_finding_event",
                )?,
                decode_pass_state(&state, attachment_pass_turn, attachment_pass_frontier)?,
            ),
            review_key(object, "review_external_link_attachment")?,
        )),
        _ => {
            return Err(corruption(
                "review_finding_event",
                String::from("torn posted link attachment"),
            ));
        }
    };
    let canonical = ReviewExternalLink::try_reconstitute(
        id,
        association,
        review_key(provider, "review_external_link")?,
        decode_external_object_kind(&object_kind)?,
        attachment,
        Vec::new(),
    )
    .map_err(|error| corruption("review_finding_event", format!("{error:?}")))?;
    ReviewFindingExternalLinkRef::try_new(finding, &canonical).map_err(|error| {
        corruption(
            "review_finding_event",
            format!("invalid canonical posted link: {:?}", error.failure()),
        )
    })
}

fn decode_external_link_root(
    row: &PgRow,
) -> Result<
    (
        ReviewExternalLinkId,
        ReviewExternalLinkAssociation,
        ReviewKey,
        ReviewExternalObjectKind,
    ),
    ReviewWorkflowStoreError,
> {
    let id = external_link_id(row.try_get("external_link_id")?);
    let target = target_id(row.try_get("target_id")?);
    let association_kind: String = row.try_get("association_kind")?;
    let run: Option<Uuid> = row.try_get("run_id")?;
    let finding: Option<Uuid> = row.try_get("finding_id")?;
    let finding_pass: Option<Uuid> = row.try_get("finding_producing_pass_id")?;
    let association = match (association_kind.as_str(), run, finding, finding_pass) {
        ("target", None, None, None) => ReviewExternalLinkAssociation::Target(target),
        ("run", Some(run), None, None) => {
            ReviewExternalLinkAssociation::Run(ReviewRunRef::new(target, run_id(run)))
        }
        ("finding", Some(run), Some(finding), Some(finding_pass)) => {
            ReviewExternalLinkAssociation::Finding(ReviewFindingRef::new(
                ReviewPassRef::new(
                    ReviewRunRef::new(target, run_id(run)),
                    pass_id(finding_pass),
                ),
                finding_id(finding),
            ))
        }
        _ => {
            return Err(corruption(
                "review_external_link",
                format!("invalid association shape {association_kind}"),
            ));
        }
    };
    Ok((
        id,
        association,
        review_key(row.try_get("provider_key")?, "review_external_link")?,
        decode_external_object_kind(&row.try_get::<String, _>("object_kind")?)?,
    ))
}

fn decode_external_link_attachment(
    row: &PgRow,
) -> Result<ReviewExternalLinkAttachment, ReviewWorkflowStoreError> {
    Ok(ReviewExternalLinkAttachment::new(
        external_link_id(row.try_get("external_link_id")?),
        ReviewPassEvidence::new(
            ReviewPassRef::new(
                ReviewRunRef::new(
                    target_id(row.try_get("target_id")?),
                    run_id(row.try_get("pass_run_id")?),
                ),
                pass_id(row.try_get("pass_id")?),
            ),
            decode_pass_kind(&row.try_get::<String, _>("pass_kind")?)?,
            decode_review_policy(
                row,
                "pass_policy_version",
                "pass_minimum_judge_confidence",
                "pass_minimum_publication_confidence",
                "review_external_link_attachment",
            )?,
            decode_pass_state(
                &row.try_get::<String, _>("pass_state_kind")?,
                row.try_get("pass_turn_id")?,
                row.try_get("pass_output_frontier_id")?,
            )?,
        ),
        review_key(
            row.try_get("external_object_key")?,
            "review_external_link_attachment",
        )?,
    ))
}

fn decode_external_link_observation(
    row: &PgRow,
) -> Result<ReviewExternalLinkObservation, ReviewWorkflowStoreError> {
    Ok(ReviewExternalLinkObservation::new(
        external_link_id(row.try_get("external_link_id")?),
        ReviewEventOrdinal::try_new(positive_u32(
            row.try_get("observation_ordinal")?,
            "review_external_link_observation",
        )?)
        .map_err(|_| {
            corruption(
                "review_external_link_observation",
                String::from("zero ordinal"),
            )
        })?,
        ReviewPassEvidence::new(
            ReviewPassRef::new(
                ReviewRunRef::new(
                    target_id(row.try_get("target_id")?),
                    run_id(row.try_get("pass_run_id")?),
                ),
                pass_id(row.try_get("pass_id")?),
            ),
            decode_pass_kind(&row.try_get::<String, _>("pass_kind")?)?,
            decode_review_policy(
                row,
                "pass_policy_version",
                "pass_minimum_judge_confidence",
                "pass_minimum_publication_confidence",
                "review_external_link_observation",
            )?,
            decode_pass_state(
                &row.try_get::<String, _>("pass_state_kind")?,
                row.try_get("pass_turn_id")?,
                row.try_get("pass_output_frontier_id")?,
            )?,
        ),
        decode_external_object_state(&row.try_get::<String, _>("object_state")?)?,
    ))
}

fn same_reservation(left: &ReviewExternalLink, right: &ReviewExternalLink) -> bool {
    left.id() == right.id()
        && left.association() == right.association()
        && left.provider() == right.provider()
        && left.object_kind() == right.object_kind()
}

struct EncodedPassState {
    kind: &'static str,
    turn: Option<TurnId>,
    frontier: Option<ContextFrontierId>,
}

fn pass_state_turn(state: ReviewPassState) -> Option<TurnId> {
    match state {
        ReviewPassState::Queued | ReviewPassState::Cancelled { turn: None } => None,
        ReviewPassState::Running { turn }
        | ReviewPassState::Succeeded { turn, .. }
        | ReviewPassState::Failed { turn }
        | ReviewPassState::Blocked { turn }
        | ReviewPassState::Cancelled { turn: Some(turn) } => Some(turn),
    }
}

fn encode_pass_state(state: ReviewPassState) -> EncodedPassState {
    match state {
        ReviewPassState::Queued => EncodedPassState {
            kind: "queued",
            turn: None,
            frontier: None,
        },
        ReviewPassState::Running { turn } => EncodedPassState {
            kind: "running",
            turn: Some(turn),
            frontier: None,
        },
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
        } => EncodedPassState {
            kind: "succeeded",
            turn: Some(turn),
            frontier: Some(output_frontier),
        },
        ReviewPassState::Failed { turn } => EncodedPassState {
            kind: "failed",
            turn: Some(turn),
            frontier: None,
        },
        ReviewPassState::Blocked { turn } => EncodedPassState {
            kind: "blocked",
            turn: Some(turn),
            frontier: None,
        },
        ReviewPassState::Cancelled { turn } => EncodedPassState {
            kind: "cancelled",
            turn,
            frontier: None,
        },
    }
}

fn decode_pass_state(
    kind: &str,
    turn: Option<Uuid>,
    frontier: Option<Uuid>,
) -> Result<ReviewPassState, ReviewWorkflowStoreError> {
    match (kind, turn, frontier) {
        ("queued", None, None) => Ok(ReviewPassState::Queued),
        ("running", Some(turn), None) => Ok(ReviewPassState::Running {
            turn: turn_id(turn),
        }),
        ("succeeded", Some(turn), Some(frontier)) => Ok(ReviewPassState::Succeeded {
            turn: turn_id(turn),
            output_frontier: context_frontier_id(frontier),
        }),
        ("failed", Some(turn), None) => Ok(ReviewPassState::Failed {
            turn: turn_id(turn),
        }),
        ("blocked", Some(turn), None) => Ok(ReviewPassState::Blocked {
            turn: turn_id(turn),
        }),
        ("cancelled", turn, None) => Ok(ReviewPassState::Cancelled {
            turn: turn.map(turn_id),
        }),
        _ => Err(corruption(
            "review_pass",
            format!("invalid state shape {kind}"),
        )),
    }
}

fn encode_run_state(state: ReviewRunState) -> (&'static str, Option<ReviewPassId>) {
    match state {
        ReviewRunState::Queued => ("queued", None),
        ReviewRunState::Running { active_pass } => ("running", Some(active_pass.pass())),
        ReviewRunState::Succeeded { concluding_pass } => {
            ("succeeded", Some(concluding_pass.pass()))
        }
        ReviewRunState::Failed { failed_pass } => ("failed", Some(failed_pass.pass())),
        ReviewRunState::Blocked { blocking_pass } => ("blocked", Some(blocking_pass.pass())),
        ReviewRunState::Cancelled { last_pass } => {
            ("cancelled", last_pass.map(ReviewPassRef::pass))
        }
    }
}

fn decode_run_state(
    run: ReviewRunRef,
    kind: &str,
    pass: Option<Uuid>,
) -> Result<ReviewRunState, ReviewWorkflowStoreError> {
    let pass = pass.map(|pass| ReviewPassRef::new(run, pass_id(pass)));
    match (kind, pass) {
        ("queued", None) => Ok(ReviewRunState::Queued),
        ("running", Some(active_pass)) => Ok(ReviewRunState::Running { active_pass }),
        ("succeeded", Some(concluding_pass)) => Ok(ReviewRunState::Succeeded { concluding_pass }),
        ("failed", Some(failed_pass)) => Ok(ReviewRunState::Failed { failed_pass }),
        ("blocked", Some(blocking_pass)) => Ok(ReviewRunState::Blocked { blocking_pass }),
        ("cancelled", last_pass) => Ok(ReviewRunState::Cancelled { last_pass }),
        _ => Err(corruption(
            "review_run",
            format!("invalid state shape {kind}"),
        )),
    }
}

struct EncodedLinkAssociation {
    kind: &'static str,
    run: Option<ReviewRunId>,
    finding: Option<ReviewFindingId>,
}

fn encode_link_association(association: ReviewExternalLinkAssociation) -> EncodedLinkAssociation {
    match association {
        ReviewExternalLinkAssociation::Target(_) => EncodedLinkAssociation {
            kind: "target",
            run: None,
            finding: None,
        },
        ReviewExternalLinkAssociation::Run(run) => EncodedLinkAssociation {
            kind: "run",
            run: Some(run.run()),
            finding: None,
        },
        ReviewExternalLinkAssociation::Finding(finding) => EncodedLinkAssociation {
            kind: "finding",
            run: Some(finding.run().run()),
            finding: Some(finding.finding()),
        },
    }
}

struct EncodedFindingEvent<'a> {
    kind: &'static str,
    reason: Option<&'a str>,
    referenced_finding: Option<ReviewFindingId>,
    external_link: Option<ReviewExternalLinkId>,
}

fn encode_finding_event(event: &ReviewFindingEventKind) -> EncodedFindingEvent<'_> {
    let empty = |kind| EncodedFindingEvent {
        kind,
        reason: None,
        referenced_finding: None,
        external_link: None,
    };
    match event {
        ReviewFindingEventKind::Accepted => empty("accepted"),
        ReviewFindingEventKind::Rejected { reason } => EncodedFindingEvent {
            kind: "rejected",
            reason: Some(reason.as_str()),
            referenced_finding: None,
            external_link: None,
        },
        ReviewFindingEventKind::Duplicate { canonical } => EncodedFindingEvent {
            kind: "duplicate",
            reason: None,
            referenced_finding: Some(canonical.finding()),
            external_link: None,
        },
        ReviewFindingEventKind::Superseded { successor } => EncodedFindingEvent {
            kind: "superseded",
            reason: None,
            referenced_finding: Some(successor.finding()),
            external_link: None,
        },
        ReviewFindingEventKind::Stale => empty("stale"),
        ReviewFindingEventKind::Posted { link } => EncodedFindingEvent {
            kind: "posted",
            reason: None,
            referenced_finding: None,
            external_link: Some(link.link()),
        },
        ReviewFindingEventKind::Fixed => empty("fixed"),
        ReviewFindingEventKind::BlockedWithReason { reason } => EncodedFindingEvent {
            kind: "blocked_with_reason",
            reason: Some(reason.as_str()),
            referenced_finding: None,
            external_link: None,
        },
    }
}

fn encode_workflow_kind(kind: ReviewWorkflowKind) -> &'static str {
    match kind {
        ReviewWorkflowKind::ImportExternalContext => "import_external_context",
        ReviewWorkflowKind::ReadOnlyReview => "read_only_review",
        ReviewWorkflowKind::JudgeFindings => "judge_findings",
        ReviewWorkflowKind::DedupeFindings => "dedupe_findings",
        ReviewWorkflowKind::PublishReview => "publish_review",
        ReviewWorkflowKind::FixFindings => "fix_findings",
        ReviewWorkflowKind::PropagateStack => "propagate_stack",
    }
}

fn decode_workflow_kind(kind: &str) -> Result<ReviewWorkflowKind, ReviewWorkflowStoreError> {
    match kind {
        "import_external_context" => Ok(ReviewWorkflowKind::ImportExternalContext),
        "read_only_review" => Ok(ReviewWorkflowKind::ReadOnlyReview),
        "judge_findings" => Ok(ReviewWorkflowKind::JudgeFindings),
        "dedupe_findings" => Ok(ReviewWorkflowKind::DedupeFindings),
        "publish_review" => Ok(ReviewWorkflowKind::PublishReview),
        "fix_findings" => Ok(ReviewWorkflowKind::FixFindings),
        "propagate_stack" => Ok(ReviewWorkflowKind::PropagateStack),
        _ => Err(corruption(
            "review_run",
            format!("unknown workflow kind {kind}"),
        )),
    }
}

fn encode_pass_kind(kind: ReviewPassKind) -> &'static str {
    match kind {
        ReviewPassKind::ImportExternalContext => "import_external_context",
        ReviewPassKind::ReadOnlyReview => "read_only_review",
        ReviewPassKind::Judge => "judge",
        ReviewPassKind::Dedupe => "dedupe",
        ReviewPassKind::Publish => "publish",
        ReviewPassKind::Fix => "fix",
        ReviewPassKind::PropagateStack => "propagate_stack",
    }
}

fn decode_pass_kind(kind: &str) -> Result<ReviewPassKind, ReviewWorkflowStoreError> {
    match kind {
        "import_external_context" => Ok(ReviewPassKind::ImportExternalContext),
        "read_only_review" => Ok(ReviewPassKind::ReadOnlyReview),
        "judge" => Ok(ReviewPassKind::Judge),
        "dedupe" => Ok(ReviewPassKind::Dedupe),
        "publish" => Ok(ReviewPassKind::Publish),
        "fix" => Ok(ReviewPassKind::Fix),
        "propagate_stack" => Ok(ReviewPassKind::PropagateStack),
        _ => Err(corruption(
            "review_pass",
            format!("unknown pass kind {kind}"),
        )),
    }
}

fn encode_diff_side(side: ReviewFindingDiffSide) -> &'static str {
    match side {
        ReviewFindingDiffSide::Left => "left",
        ReviewFindingDiffSide::Right => "right",
    }
}

fn decode_diff_side(side: &str) -> Result<ReviewFindingDiffSide, ReviewWorkflowStoreError> {
    match side {
        "left" => Ok(ReviewFindingDiffSide::Left),
        "right" => Ok(ReviewFindingDiffSide::Right),
        _ => Err(corruption(
            "review_finding",
            format!("unknown diff side {side}"),
        )),
    }
}

fn encode_severity(severity: ReviewFindingSeverity) -> &'static str {
    match severity {
        ReviewFindingSeverity::Info => "info",
        ReviewFindingSeverity::Low => "low",
        ReviewFindingSeverity::Medium => "medium",
        ReviewFindingSeverity::High => "high",
        ReviewFindingSeverity::Critical => "critical",
    }
}

fn decode_severity(severity: &str) -> Result<ReviewFindingSeverity, ReviewWorkflowStoreError> {
    match severity {
        "info" => Ok(ReviewFindingSeverity::Info),
        "low" => Ok(ReviewFindingSeverity::Low),
        "medium" => Ok(ReviewFindingSeverity::Medium),
        "high" => Ok(ReviewFindingSeverity::High),
        "critical" => Ok(ReviewFindingSeverity::Critical),
        _ => Err(corruption(
            "review_finding",
            format!("unknown severity {severity}"),
        )),
    }
}

fn encode_external_object_kind(kind: ReviewExternalObjectKind) -> &'static str {
    match kind {
        ReviewExternalObjectKind::ChangeRequest => "change_request",
        ReviewExternalObjectKind::Commit => "commit",
        ReviewExternalObjectKind::Review => "review",
        ReviewExternalObjectKind::ReviewThread => "review_thread",
        ReviewExternalObjectKind::ReviewComment => "review_comment",
        ReviewExternalObjectKind::ChangeRequestComment => "change_request_comment",
    }
}

fn decode_external_object_kind(
    kind: &str,
) -> Result<ReviewExternalObjectKind, ReviewWorkflowStoreError> {
    match kind {
        "change_request" => Ok(ReviewExternalObjectKind::ChangeRequest),
        "commit" => Ok(ReviewExternalObjectKind::Commit),
        "review" => Ok(ReviewExternalObjectKind::Review),
        "review_thread" => Ok(ReviewExternalObjectKind::ReviewThread),
        "review_comment" => Ok(ReviewExternalObjectKind::ReviewComment),
        "change_request_comment" => Ok(ReviewExternalObjectKind::ChangeRequestComment),
        _ => Err(corruption(
            "review_external_link",
            format!("unknown object kind {kind}"),
        )),
    }
}

fn encode_external_object_state(state: ReviewExternalObjectState) -> &'static str {
    match state {
        ReviewExternalObjectState::Current => "current",
        ReviewExternalObjectState::Outdated => "outdated",
        ReviewExternalObjectState::Resolved => "resolved",
    }
}

fn decode_external_object_state(
    state: &str,
) -> Result<ReviewExternalObjectState, ReviewWorkflowStoreError> {
    match state {
        "current" => Ok(ReviewExternalObjectState::Current),
        "outdated" => Ok(ReviewExternalObjectState::Outdated),
        "resolved" => Ok(ReviewExternalObjectState::Resolved),
        _ => Err(corruption(
            "review_external_link_observation",
            format!("unknown state {state}"),
        )),
    }
}

fn review_key(
    value: String,
    aggregate: &'static str,
) -> Result<ReviewKey, ReviewWorkflowStoreError> {
    ReviewKey::try_new(value).map_err(|error| {
        corruption(
            aggregate,
            format!("invalid review key: {:?}", error.failure()),
        )
    })
}

fn review_text(
    value: String,
    aggregate: &'static str,
) -> Result<ReviewText, ReviewWorkflowStoreError> {
    ReviewText::try_new(value).map_err(|error| {
        corruption(
            aggregate,
            format!("invalid review text: {:?}", error.failure()),
        )
    })
}

fn confidence(
    value: i32,
    aggregate: &'static str,
) -> Result<ReviewConfidence, ReviewWorkflowStoreError> {
    let value = u16::try_from(value)
        .map_err(|_| corruption(aggregate, format!("invalid confidence {value}")))?;
    ReviewConfidence::try_from_basis_points(value)
        .map_err(|error| corruption(aggregate, format!("{error:?}")))
}

fn decode_review_policy(
    row: &PgRow,
    version_column: &str,
    judge_column: &str,
    publication_column: &str,
    aggregate: &'static str,
) -> Result<ReviewPolicy, ReviewWorkflowStoreError> {
    let version = positive_u32(row.try_get(version_column)?, aggregate)?;
    let judge = confidence(row.try_get(judge_column)?, aggregate)?;
    let publication = confidence(row.try_get(publication_column)?, aggregate)?;
    ReviewPolicy::try_new(
        ReviewPolicyVersion::try_new(version)
            .map_err(|_| corruption(aggregate, String::from("zero policy version")))?,
        judge,
        publication,
    )
    .map_err(|error| corruption(aggregate, format!("{error:?}")))
}

fn positive_u32(value: i64, aggregate: &'static str) -> Result<u32, ReviewWorkflowStoreError> {
    let value = u32::try_from(value)
        .map_err(|_| corruption(aggregate, format!("invalid positive u32 {value}")))?;
    if value == 0 {
        Err(corruption(
            aggregate,
            String::from("zero where positive u32 required"),
        ))
    } else {
        Ok(value)
    }
}

fn decimal_u64(value: Decimal, aggregate: &'static str) -> Result<u64, ReviewWorkflowStoreError> {
    value
        .to_string()
        .parse()
        .map_err(|_| corruption(aggregate, format!("invalid u64 decimal {value}")))
}

fn target_id(value: Uuid) -> ReviewTargetId {
    ReviewTargetId::from_uuid(value)
}

fn run_id(value: Uuid) -> ReviewRunId {
    ReviewRunId::from_uuid(value)
}

fn pass_id(value: Uuid) -> ReviewPassId {
    ReviewPassId::from_uuid(value)
}

fn finding_id(value: Uuid) -> ReviewFindingId {
    ReviewFindingId::from_uuid(value)
}

fn external_link_id(value: Uuid) -> ReviewExternalLinkId {
    ReviewExternalLinkId::from_uuid(value)
}

fn session_id(value: Uuid) -> SessionId {
    SessionId::from_uuid(value)
}

fn accepted_input_id(value: Uuid) -> AcceptedInputId {
    AcceptedInputId::from_uuid(value)
}

fn turn_id(value: Uuid) -> TurnId {
    TurnId::from_uuid(value)
}

fn context_frontier_id(value: Uuid) -> ContextFrontierId {
    ContextFrontierId::from_uuid(value)
}

fn corruption(aggregate: &'static str, detail: String) -> ReviewWorkflowStoreError {
    ReviewWorkflowStoreError::Corruption(ReviewWorkflowCorruption { aggregate, detail })
}

/// First reservation outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReserveExternalLinkOutcome {
    /// This call inserted the pending reservation.
    Inserted(ReviewExternalLink),
    /// An equal reservation already existed and its complete state was loaded.
    Existing(ReviewExternalLink),
}

/// Conflicting reuse of a review external-link reservation identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewExternalLinkReservationConflict {
    existing: Box<ReviewExternalLink>,
    requested: Box<ReviewExternalLink>,
}

impl ReviewExternalLinkReservationConflict {
    /// Borrows the retained canonical aggregate.
    pub fn existing(&self) -> &ReviewExternalLink {
        &self.existing
    }

    /// Borrows the rejected reservation request.
    pub fn requested(&self) -> &ReviewExternalLink {
        &self.requested
    }

    /// Returns both complete aggregates.
    pub fn into_parts(self) -> (ReviewExternalLink, ReviewExternalLink) {
        (*self.existing, *self.requested)
    }
}

impl fmt::Display for ReviewExternalLinkReservationConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "review external-link identity was reused for a different canonical reservation",
        )
    }
}

impl Error for ReviewExternalLinkReservationConflict {}

/// Caller-supplied aggregate shape that cannot begin a new store record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowInsertionError {
    /// A run insertion carried state that can only result from transition.
    RunNotQueued {
        /// Rejected current state.
        state: ReviewRunState,
    },
    /// A pass insertion carried state that can only result from transition.
    PassNotQueued {
        /// Rejected current state.
        state: ReviewPassState,
    },
    /// A finding insertion already carried lifecycle history.
    FindingNotOpen {
        /// Rejected current status.
        status: ReviewFindingStatus,
    },
    /// A reservation insertion already carried post-effect evidence.
    ExternalLinkNotPending,
}

impl fmt::Display for ReviewWorkflowInsertionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunNotQueued { state } => {
                write!(formatter, "new review run is not queued: {state:?}")
            }
            Self::PassNotQueued { state } => {
                write!(formatter, "new review pass is not queued: {state:?}")
            }
            Self::FindingNotOpen { status } => {
                write!(formatter, "new review finding is not open: {status:?}")
            }
            Self::ExternalLinkNotPending => {
                formatter.write_str("new review external-link reservation is not pending")
            }
        }
    }
}

impl Error for ReviewWorkflowInsertionError {}

/// Domain transition rejected before persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowTransitionError {
    /// Run transition failed.
    Run(signalbox_domain::ReviewRunTransitionError),
    /// Pass transition failed.
    Pass(signalbox_domain::ReviewPassTransitionError),
    /// Finding event application failed.
    Finding(signalbox_domain::ReviewFindingTransitionError),
    /// External-link attachment or observation failed.
    ExternalLink(signalbox_domain::ReviewExternalLinkTransitionError),
}

impl fmt::Display for ReviewWorkflowTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run(error) => write!(formatter, "review-run transition rejected: {error:?}"),
            Self::Pass(error) => write!(formatter, "review-pass transition rejected: {error:?}"),
            Self::Finding(error) => {
                write!(
                    formatter,
                    "review-finding transition rejected: {:?}",
                    error.failure()
                )
            }
            Self::ExternalLink(error) => {
                write!(
                    formatter,
                    "review external-link transition rejected: {error:?}"
                )
            }
        }
    }
}

impl Error for ReviewWorkflowTransitionError {}

/// Stored workflow facts could not form one domain aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewWorkflowCorruption {
    aggregate: &'static str,
    detail: String,
}

impl ReviewWorkflowCorruption {
    /// Returns the aggregate family that failed.
    pub const fn aggregate(&self) -> &'static str {
        self.aggregate
    }

    /// Borrows the content-safe diagnostic detail.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for ReviewWorkflowCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} durable facts are corrupt: {}",
            self.aggregate, self.detail
        )
    }
}

impl Error for ReviewWorkflowCorruption {}

/// Review-workflow persistence failure.
#[derive(Debug)]
pub enum ReviewWorkflowStoreError {
    /// PostgreSQL or transport failure.
    Database(sqlx::Error),
    /// PostgreSQL may have committed a mutation before the response was lost.
    CommitAmbiguous(sqlx::Error),
    /// Stored facts failed closed reconstitution.
    Corruption(ReviewWorkflowCorruption),
    /// A caller attempted to insert a post-transition aggregate as new.
    InvalidInsertion(ReviewWorkflowInsertionError),
    /// A caller requested an invalid domain transition.
    InvalidTransition(ReviewWorkflowTransitionError),
    /// An external-link identity was reused for another canonical payload.
    ReservationConflict(ReviewExternalLinkReservationConflict),
}

impl fmt::Display for ReviewWorkflowStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "review-workflow database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "review-workflow commit outcome is ambiguous: {error}"
                )
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::InvalidInsertion(error) => error.fmt(formatter),
            Self::InvalidTransition(error) => error.fmt(formatter),
            Self::ReservationConflict(error) => error.fmt(formatter),
        }
    }
}

impl Error for ReviewWorkflowStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::InvalidInsertion(error) => Some(error),
            Self::InvalidTransition(error) => Some(error),
            Self::ReservationConflict(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ReviewWorkflowStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{ReviewWorkflowStoreError, classify_mutating_commit_error};

    #[test]
    fn unknown_commit_transport_failure_is_ambiguous() {
        let classified = classify_mutating_commit_error(sqlx::Error::PoolClosed);
        assert!(matches!(
            classified,
            ReviewWorkflowStoreError::CommitAmbiguous(sqlx::Error::PoolClosed)
        ));
    }
}

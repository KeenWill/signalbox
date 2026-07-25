//! PostgreSQL store for review-workflow aggregates.
//!
//! SQL rows remain adapter-private. Complete values are reconstructed through
//! the domain API defined by `docs/spec/review-workflows.md`.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, ReviewChangeRequestNumber, ReviewConfidence,
    ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalLinkAttachment, ReviewExternalLinkAttachmentResult, ReviewExternalLinkId,
    ReviewExternalLinkObservation, ReviewExternalLinkObservationResult,
    ReviewExternalLinkTransitionFailure, ReviewExternalObjectKind, ReviewExternalObjectState,
    ReviewFinding, ReviewFindingContent, ReviewFindingDiffSide, ReviewFindingEvent,
    ReviewFindingEventKind, ReviewFindingEventResult, ReviewFindingEventResultKind,
    ReviewFindingExternalLinkRef, ReviewFindingId, ReviewFindingLocation,
    ReviewFindingPendingExternalLinkRef, ReviewFindingProposal, ReviewFindingRef,
    ReviewFindingSeverity, ReviewFindingStatus, ReviewKey, ReviewLineRange, ReviewPass,
    ReviewPassAcceptedInputEvidence, ReviewPassEvidence, ReviewPassId, ReviewPassKind,
    ReviewPassReconstitutionInput, ReviewPassRef, ReviewPassResult, ReviewPassState,
    ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy, ReviewPolicyVersion,
    ReviewProducedFindings, ReviewReferencedFindingEvidence, ReviewRun, ReviewRunEvidence,
    ReviewRunId, ReviewRunReconstitutionInput, ReviewRunRef, ReviewRunState, ReviewTarget,
    ReviewTargetId, ReviewTargetSubject, ReviewText, ReviewWorkflowKind, SessionId, TurnId,
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
                    parent.repository_key AS stack_parent_repository_key,
                    parent.subject_kind AS stack_parent_subject_kind,
                    parent.change_request_number
                        AS stack_parent_change_request_number,
                    parent.head_revision AS stack_parent_head_revision,
                    parent.base_revision AS stack_parent_base_revision
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
                ReviewWorkflowInsertionError::RunNotQueued {
                    state: Box::new(run.state()),
                },
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
        let mut transaction = begin_repeatable_read(&self.pool).await?;
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
                        AS evidence_pass_output_frontier_id,
                    canonical_pass.result_kind
                        AS evidence_pass_result_kind,
                    canonical_pass.result_finding_id
                        AS evidence_pass_result_finding_id,
                    canonical_pass.result_finding_run_id
                        AS evidence_pass_result_finding_run_id,
                    canonical_pass.result_finding_pass_id
                        AS evidence_pass_result_finding_pass_id,
                    canonical_pass.result_event_ordinal
                        AS evidence_pass_result_event_ordinal,
                    canonical_pass.result_event_kind
                        AS evidence_pass_result_event_kind,
                    canonical_pass.result_reason
                        AS evidence_pass_result_reason,
                    canonical_pass.result_referenced_finding_id
                        AS evidence_pass_result_referenced_finding_id,
                    canonical_pass.result_referenced_finding_run_id
                        AS evidence_pass_result_referenced_finding_run_id,
                    canonical_pass.result_referenced_finding_pass_id
                        AS evidence_pass_result_referenced_finding_pass_id,
                    canonical_pass.result_referenced_finding_status
                        AS evidence_pass_result_referenced_finding_status,
                    canonical_pass.result_external_link_id
                        AS evidence_pass_result_external_link_id,
                    canonical_pass.result_external_object_key
                        AS evidence_pass_result_external_object_key,
                    canonical_pass.result_observation_state
                        AS evidence_pass_result_observation_state
               FROM review_run AS workflow_run
               LEFT JOIN review_target AS canonical_target
                 ON canonical_target.target_id = workflow_run.target_id
               LEFT JOIN review_pass AS canonical_pass
                 ON canonical_pass.run_id = workflow_run.run_id
                AND canonical_pass.target_id = workflow_run.target_id
              WHERE workflow_run.run_id = $1",
        )
        .bind(run.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        require_joined_reference(
            &row,
            "canonical_target_id",
            "review_run",
            "referenced target row is missing",
        )?;
        let evidence_pass = row
            .try_get::<Option<Uuid>, _>("evidence_pass_id")?
            .map(pass_id);
        let produced_findings = match evidence_pass {
            Some(pass) => load_produced_findings(&mut transaction, pass).await?,
            None => Vec::new(),
        };
        let run = decode_run(row, produced_findings)?;
        transaction.commit().await?;
        Ok(Some(run))
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
        if pass.state() != &ReviewPassState::Queued {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::PassNotQueued {
                    state: Box::new(pass.state().clone()),
                },
            ));
        }
        let state = encode_pass_state(pass.state());
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_pass
                (pass_id, run_id, target_id, pass_kind, session_id,
                 accepted_input_id, origin_turn_id, state_kind, turn_id,
                 output_frontier_id)
             VALUES (
                 $1, $2, $3, $4, $5, $6, $7, $8, $9, $10
             )",
        )
        .bind(pass.reference().pass().into_uuid())
        .bind(pass.reference().run().run().into_uuid())
        .bind(pass.reference().target().into_uuid())
        .bind(encode_pass_kind(pass.kind()))
        .bind(pass.session().into_uuid())
        .bind(pass.accepted_input().into_uuid())
        .bind(pass.origin_turn().into_uuid())
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
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT workflow_pass.pass_id, workflow_pass.run_id,
                    workflow_pass.target_id, workflow_pass.pass_kind,
                    canonical_run.workflow_kind AS run_workflow_kind,
                    workflow_pass.session_id AS pass_session_id,
                    workflow_pass.accepted_input_id, workflow_pass.state_kind,
                    workflow_pass.origin_turn_id,
                    workflow_pass.turn_id, workflow_pass.output_frontier_id,
                    workflow_pass.result_kind,
                    workflow_pass.result_finding_id,
                    workflow_pass.result_finding_run_id,
                    workflow_pass.result_finding_pass_id,
                    workflow_pass.result_event_ordinal,
                    workflow_pass.result_event_kind,
                    workflow_pass.result_reason,
                    workflow_pass.result_referenced_finding_id,
                    workflow_pass.result_referenced_finding_run_id,
                    workflow_pass.result_referenced_finding_pass_id,
                    workflow_pass.result_referenced_finding_status,
                    workflow_pass.result_external_link_id,
                    workflow_pass.result_external_object_key,
                    workflow_pass.result_observation_state,
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
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        require_joined_reference(
            &row,
            "canonical_run_id",
            "review_pass",
            "referenced run row is missing",
        )?;
        let produced_findings = load_produced_findings(&mut transaction, pass).await?;
        let pass = decode_pass(row, produced_findings)?;
        transaction.commit().await?;
        Ok(Some(pass))
    }

    /// Applies one domain-validated pass transition under row lock.
    pub async fn transition_pass(
        &self,
        pass: ReviewPassId,
        next: ReviewPassState,
    ) -> Result<Option<ReviewPass>, ReviewWorkflowStoreError> {
        if pass_state_result(&next).is_some() {
            return Err(ReviewWorkflowStoreError::NonAtomicPassResult);
        }
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(crate::lock_inventory::REVIEW_PASS_TRANSITION)
            .bind(pass.into_uuid())
            .bind(encode_pass_state(&next).turn.map(TurnId::into_uuid))
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
        if pass_state_result(&next_pass).is_some() {
            return Err(ReviewWorkflowStoreError::NonAtomicPassResult);
        }
        let mut transaction = self.pool.begin().await?;
        let run_row = sqlx::query(crate::lock_inventory::REVIEW_RUN_TRANSITION)
            .bind(run.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(run_row) = run_row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let pass_row = sqlx::query(crate::lock_inventory::REVIEW_PASS_TRANSITION)
            .bind(pass.into_uuid())
            .bind(encode_pass_state(&next_pass).turn.map(TurnId::into_uuid))
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
            transitioned_pass.state().clone(),
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
        self.insert_findings(
            finding.proposal().producing_pass(),
            std::slice::from_ref(finding),
        )
        .await
    }

    /// Atomically binds one produced-finding result and its complete inventory.
    pub async fn insert_findings(
        &self,
        pass: &ReviewPassEvidence,
        findings: &[ReviewFinding],
    ) -> Result<(), ReviewWorkflowStoreError> {
        let Some(ReviewPassResult::ProducedFindings(inventory)) = pass_state_result(pass.state())
        else {
            return Err(ReviewWorkflowStoreError::IncompleteFindingInventory);
        };
        let mut supplied = findings
            .iter()
            .map(|finding| finding.proposal().reference())
            .collect::<Vec<_>>();
        supplied.sort_unstable();
        if supplied != inventory.findings()
            || findings.iter().any(|finding| {
                finding.status() != ReviewFindingStatus::Open
                    || finding.proposal().producing_pass() != pass
            })
        {
            return Err(ReviewWorkflowStoreError::IncompleteFindingInventory);
        }
        let mut transaction = self.pool.begin().await?;
        bind_pass_result(&mut transaction, pass).await?;
        for finding in findings {
            insert_finding_row(&mut transaction, finding).await?;
        }
        for (index, reference) in inventory.findings().iter().enumerate() {
            sqlx::query(
                "INSERT INTO review_pass_produced_finding
                    (pass_id, result_ordinal, finding_id, finding_run_id,
                     target_id, finding_pass_id)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(reference.pass().pass().into_uuid())
            .bind(i64::try_from(index + 1).map_err(|_| {
                corruption(
                    "review_pass_produced_finding",
                    String::from("finding inventory ordinal overflow"),
                )
            })?)
            .bind(reference.finding().into_uuid())
            .bind(reference.run().run().into_uuid())
            .bind(reference.target().into_uuid())
            .bind(reference.pass().pass().into_uuid())
            .execute(&mut *transaction)
            .await?;
        }
        commit_mutation(transaction).await?;
        Ok(())
    }

    /// Appends one event after domain validation of the complete current history.
    pub async fn append_finding_event(
        &self,
        finding: ReviewFindingId,
        event: ReviewFindingEvent,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let encoded = encode_finding_event(event.kind());
        let mut locked_findings = vec![finding.into_uuid()];
        if let Some(referenced) = encoded.referenced_finding {
            locked_findings.push(referenced.into_uuid());
        }
        locked_findings.sort_unstable();
        locked_findings.dedup();
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "SELECT finding_id
               FROM review_finding
              WHERE finding_id = ANY($1)
              ORDER BY finding_id
              FOR NO KEY UPDATE",
        )
        .bind(&locked_findings)
        .fetch_all(&mut *transaction)
        .await?;
        let Some(current) = self.load_finding(finding).await? else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let next = current.apply(event.clone()).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(
                error,
            ))
        })?;
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
                    parent.subject_kind AS stack_parent_subject_kind,
                    parent.change_request_number
                        AS stack_parent_change_request_number,
                    parent.head_revision AS stack_parent_head_revision,
                    parent.base_revision AS stack_parent_base_revision,
                    producing_pass.pass_id
                        AS canonical_producing_pass_id,
                    producing_pass.pass_kind AS producing_pass_kind,
                    producing_pass.state_kind AS producing_pass_state_kind,
                    producing_pass.turn_id AS producing_pass_turn_id,
                    producing_pass.output_frontier_id
                        AS producing_pass_output_frontier_id,
                    producing_pass.result_kind
                        AS producing_pass_result_kind,
                    producing_pass.result_finding_id
                        AS producing_pass_result_finding_id,
                    producing_pass.result_finding_run_id
                        AS producing_pass_result_finding_run_id,
                    producing_pass.result_finding_pass_id
                        AS producing_pass_result_finding_pass_id,
                    producing_pass.result_event_ordinal
                        AS producing_pass_result_event_ordinal,
                    producing_pass.result_event_kind
                        AS producing_pass_result_event_kind,
                    producing_pass.result_reason
                        AS producing_pass_result_reason,
                    producing_pass.result_referenced_finding_id
                        AS producing_pass_result_referenced_finding_id,
                    producing_pass.result_referenced_finding_run_id
                        AS producing_pass_result_referenced_finding_run_id,
                    producing_pass.result_referenced_finding_pass_id
                        AS producing_pass_result_referenced_finding_pass_id,
                    producing_pass.result_referenced_finding_status
                        AS producing_pass_result_referenced_finding_status,
                    producing_pass.result_external_link_id
                        AS producing_pass_result_external_link_id,
                    producing_pass.result_external_object_key
                        AS producing_pass_result_external_object_key,
                    producing_pass.result_observation_state
                        AS producing_pass_result_observation_state,
                    producing_run.policy_version
                        AS producing_policy_version,
                    producing_run.run_id AS canonical_producing_run_id,
                    producing_run.workflow_kind AS producing_workflow_kind,
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
        require_joined_reference(
            &row,
            "canonical_producing_run_id",
            "review_finding",
            "producing run row is missing",
        )?;
        let target = decode_target(&row)?;
        let produced_findings =
            load_produced_findings(&mut transaction, pass_id(row.try_get("producing_pass_id")?))
                .await?;
        let proposal = decode_finding_proposal(&row, produced_findings)?;
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
                    event_pass.result_kind
                        AS event_pass_result_kind,
                    event_pass.result_finding_id
                        AS event_pass_result_finding_id,
                    event_pass.result_finding_run_id
                        AS event_pass_result_finding_run_id,
                    event_pass.result_finding_pass_id
                        AS event_pass_result_finding_pass_id,
                    event_pass.result_event_ordinal
                        AS event_pass_result_event_ordinal,
                    event_pass.result_event_kind
                        AS event_pass_result_event_kind,
                    event_pass.result_reason
                        AS event_pass_result_reason,
                    event_pass.result_referenced_finding_id
                        AS event_pass_result_referenced_finding_id,
                    event_pass.result_referenced_finding_run_id
                        AS event_pass_result_referenced_finding_run_id,
                    event_pass.result_referenced_finding_pass_id
                        AS event_pass_result_referenced_finding_pass_id,
                    event_pass.result_referenced_finding_status
                        AS event_pass_result_referenced_finding_status,
                    event_pass.result_external_link_id
                        AS event_pass_result_external_link_id,
                    event_pass.result_external_object_key
                        AS event_pass_result_external_object_key,
                    event_pass.result_observation_state
                        AS event_pass_result_observation_state,
                    event_run.run_id AS canonical_event_run_id,
                    event_run.workflow_kind AS event_workflow_kind,
                    event_run.policy_version AS event_policy_version,
                    event_run.minimum_judge_confidence
                        AS event_minimum_judge_confidence,
                    event_run.minimum_publication_confidence
                        AS event_minimum_publication_confidence,
                    event.event_kind, event.reason,
                    event.referenced_finding_id,
                    event.referenced_finding_status,
                    event.external_link_id,
                    referenced_finding.producing_pass_id
                        AS referenced_finding_pass_id,
                    referenced_finding_pass.pass_id
                        AS canonical_referenced_finding_pass_id,
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
                    attachment_pass.pass_id
                        AS canonical_attachment_pass_id,
                    attachment_pass.pass_kind AS attachment_pass_kind,
                    attachment_pass.state_kind AS attachment_pass_state_kind,
                    attachment_pass.turn_id AS attachment_pass_turn_id,
                    attachment_pass.output_frontier_id
                        AS attachment_pass_output_frontier_id,
                    attachment_pass.result_kind
                        AS attachment_pass_result_kind,
                    attachment_pass.result_finding_id
                        AS attachment_pass_result_finding_id,
                    attachment_pass.result_finding_run_id
                        AS attachment_pass_result_finding_run_id,
                    attachment_pass.result_finding_pass_id
                        AS attachment_pass_result_finding_pass_id,
                    attachment_pass.result_event_ordinal
                        AS attachment_pass_result_event_ordinal,
                    attachment_pass.result_event_kind
                        AS attachment_pass_result_event_kind,
                    attachment_pass.result_reason
                        AS attachment_pass_result_reason,
                    attachment_pass.result_referenced_finding_id
                        AS attachment_pass_result_referenced_finding_id,
                    attachment_pass.result_referenced_finding_run_id
                        AS attachment_pass_result_referenced_finding_run_id,
                    attachment_pass.result_referenced_finding_pass_id
                        AS attachment_pass_result_referenced_finding_pass_id,
                    attachment_pass.result_referenced_finding_status
                        AS attachment_pass_result_referenced_finding_status,
                    attachment_pass.result_external_link_id
                        AS attachment_pass_result_external_link_id,
                    attachment_pass.result_external_object_key
                        AS attachment_pass_result_external_object_key,
                    attachment_pass.result_observation_state
                        AS attachment_pass_result_observation_state,
                    attachment_run.run_id
                        AS canonical_attachment_run_id,
                    attachment_run.workflow_kind
                        AS attachment_workflow_kind,
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
               LEFT JOIN review_pass AS referenced_finding_pass
                 ON referenced_finding_pass.pass_id =
                    referenced_finding.producing_pass_id
                AND referenced_finding_pass.run_id =
                    referenced_finding.run_id
                AND referenced_finding_pass.target_id =
                    referenced_finding.target_id
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
                require_joined_reference(
                    &row,
                    "canonical_event_run_id",
                    "review_finding_event",
                    "event run row is missing",
                )?;
                if row
                    .try_get::<Option<Uuid>, _>("referenced_finding_id")?
                    .is_some()
                {
                    require_joined_reference(
                        &row,
                        "canonical_referenced_finding_pass_id",
                        "review_finding_event",
                        "referenced finding producing pass row is missing",
                    )?;
                }
                if row
                    .try_get::<Option<Uuid>, _>("attachment_external_link_id")?
                    .is_some()
                {
                    require_joined_reference(
                        &row,
                        "canonical_attachment_pass_id",
                        "review_finding_event",
                        "attachment pass row is missing",
                    )?;
                    require_joined_reference(
                        &row,
                        "canonical_attachment_run_id",
                        "review_finding_event",
                        "attachment run row is missing",
                    )?;
                }
                decode_finding_event(&row, proposal.reference(), &target)
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
                 finding_id, finding_producing_pass_id, provider_key,
                 object_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (external_link_id) DO NOTHING",
        )
        .bind(requested.id().into_uuid())
        .bind(requested.association().target().into_uuid())
        .bind(association.kind)
        .bind(association.run.map(ReviewRunId::into_uuid))
        .bind(association.finding.map(ReviewFindingId::into_uuid))
        .bind(association.finding_pass.map(ReviewPassId::into_uuid))
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
        let posted_event = match pass_state_result(attachment.pass_evidence().state()) {
            Some(ReviewPassResult::ExternalLinkAttachment(result)) => {
                result.finding_event().map(|event| {
                    let link = ReviewFindingExternalLinkRef::try_new(event.finding(), &next)
                        .map_err(|error| {
                            corruption(
                                "review_external_link_attachment",
                                format!("posted result link is invalid: {:?}", error.failure()),
                            )
                        })?;
                    Ok::<ReviewFindingEvent, ReviewWorkflowStoreError>(ReviewFindingEvent::new(
                        event.finding(),
                        event.ordinal(),
                        attachment.pass_evidence().clone(),
                        attachment.run_evidence(),
                        ReviewFindingEventKind::Posted {
                            link: Box::new(link),
                        },
                    ))
                })
            }
            _ => None,
        }
        .transpose()?;
        if let Some(event) = posted_event.as_ref() {
            let current_finding = self
                .load_finding(event.finding().finding())
                .await?
                .ok_or_else(|| {
                    corruption(
                        "review_external_link_attachment",
                        String::from("posted result finding is missing"),
                    )
                })?;
            current_finding.apply(event.clone()).map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(
                    error,
                ))
            })?;
        }
        let mut transaction = self.pool.begin().await?;
        bind_pass_result(&mut transaction, attachment.pass_evidence()).await?;
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
        if let Some(event) = posted_event.as_ref() {
            insert_finding_event(&mut transaction, event).await?;
        }
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
        match current.clone().observe(observation.clone()) {
            Ok(_) => {}
            Err(error)
                if error.failure() == ReviewExternalLinkTransitionFailure::UnchangedObservation =>
            {
                // The database lock below decides whether this remains
                // unchanged after any concurrent observation commits.
            }
            Err(error) => {
                return Err(ReviewWorkflowStoreError::InvalidTransition(
                    ReviewWorkflowTransitionError::ExternalLink(error),
                ));
            }
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "SELECT external_link_id
               FROM review_external_link
              WHERE external_link_id = $1
              FOR NO KEY UPDATE",
        )
        .bind(link.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        let latest_state = sqlx::query_scalar::<_, String>(
            "SELECT object_state
               FROM review_external_link_observation
              WHERE external_link_id = $1
              ORDER BY observation_ordinal DESC
              LIMIT 1",
        )
        .bind(link.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        if latest_state.as_deref() == Some(encode_external_object_state(observation.state())) {
            transaction.commit().await?;
            return self.load_external_link(link).await;
        }
        bind_pass_result(&mut transaction, observation.pass_evidence()).await?;
        sqlx::query(
            "INSERT INTO review_external_link_observation
                (external_link_id, observation_ordinal, target_id,
                 pass_run_id, pass_id, object_state)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(observation.link().into_uuid())
        .bind(i64::from(observation.ordinal().get()))
        .bind(current.association().target().into_uuid())
        .bind(observation.pass().run().run().into_uuid())
        .bind(observation.pass().pass().into_uuid())
        .bind(encode_external_object_state(observation.state()))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        self.load_external_link(link).await
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
                    link.finding_producing_pass_id,
                    canonical_target.target_id AS canonical_target_id,
                    canonical_target.provider_key,
                    canonical_target.repository_key,
                    canonical_target.subject_kind,
                    canonical_target.change_request_number,
                    canonical_target.head_revision,
                    canonical_target.base_revision,
                    canonical_target.stack_parent_target_id,
                    parent.provider_key AS stack_parent_provider_key,
                    parent.repository_key
                        AS stack_parent_repository_key,
                    parent.subject_kind AS stack_parent_subject_kind,
                    parent.change_request_number
                        AS stack_parent_change_request_number,
                    parent.head_revision
                        AS stack_parent_head_revision,
                    parent.base_revision
                        AS stack_parent_base_revision,
                    canonical_run.run_id AS canonical_run_id,
                    association_finding.producing_pass_id
                        AS canonical_finding_pass_id,
                    association_finding_pass.pass_id
                        AS canonical_finding_producing_pass_id,
                    link.provider_key AS link_provider_key,
                    link.object_kind
               FROM review_external_link AS link
               LEFT JOIN review_target AS canonical_target
                 ON canonical_target.target_id = link.target_id
               LEFT JOIN review_target AS parent
                 ON parent.target_id =
                    canonical_target.stack_parent_target_id
               LEFT JOIN review_run AS canonical_run
                 ON canonical_run.run_id = link.run_id
                AND canonical_run.target_id = link.target_id
               LEFT JOIN review_finding AS association_finding
                 ON association_finding.finding_id = link.finding_id
                AND association_finding.run_id = link.run_id
                AND association_finding.target_id = link.target_id
                AND association_finding.producing_pass_id =
                    link.finding_producing_pass_id
               LEFT JOIN review_pass AS association_finding_pass
                 ON association_finding_pass.pass_id =
                    association_finding.producing_pass_id
                AND association_finding_pass.run_id =
                    association_finding.run_id
                AND association_finding_pass.target_id =
                    association_finding.target_id
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
        if row.try_get::<String, _>("association_kind")? == "finding" {
            require_joined_reference(
                &row,
                "canonical_finding_producing_pass_id",
                "review_external_link",
                "finding producing pass row is missing",
            )?;
        }
        let target = decode_target(&row)?;
        let (id, association, provider, kind) = decode_external_link_root(&row)?;
        let attachment = sqlx::query(
            "SELECT attachment.external_link_id, attachment.pass_run_id,
                    attachment.pass_id, attachment.target_id,
                    pass.pass_id AS canonical_attachment_pass_id,
                    pass.pass_kind, pass.state_kind AS pass_state_kind,
                    pass.turn_id AS pass_turn_id,
                    pass.output_frontier_id AS pass_output_frontier_id,
                    pass.result_kind AS pass_result_kind,
                    pass.result_finding_id
                        AS pass_result_finding_id,
                    pass.result_finding_run_id
                        AS pass_result_finding_run_id,
                    pass.result_finding_pass_id
                        AS pass_result_finding_pass_id,
                    pass.result_event_ordinal
                        AS pass_result_event_ordinal,
                    pass.result_event_kind AS pass_result_event_kind,
                    pass.result_reason AS pass_result_reason,
                    pass.result_referenced_finding_id
                        AS pass_result_referenced_finding_id,
                    pass.result_referenced_finding_run_id
                        AS pass_result_referenced_finding_run_id,
                    pass.result_referenced_finding_pass_id
                        AS pass_result_referenced_finding_pass_id,
                    pass.result_referenced_finding_status
                        AS pass_result_referenced_finding_status,
                    pass.result_external_link_id
                        AS pass_result_external_link_id,
                    pass.result_external_object_key
                        AS pass_result_external_object_key,
                    pass.result_observation_state
                        AS pass_result_observation_state,
                    pass_run.run_id AS canonical_attachment_run_id,
                    pass_run.workflow_kind AS pass_workflow_kind,
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
            require_joined_reference(
                &row,
                "canonical_attachment_run_id",
                "review_external_link_attachment",
                "attaching run row is missing",
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
                    pass.result_kind AS pass_result_kind,
                    pass.result_finding_id
                        AS pass_result_finding_id,
                    pass.result_finding_run_id
                        AS pass_result_finding_run_id,
                    pass.result_finding_pass_id
                        AS pass_result_finding_pass_id,
                    pass.result_event_ordinal
                        AS pass_result_event_ordinal,
                    pass.result_event_kind AS pass_result_event_kind,
                    pass.result_reason AS pass_result_reason,
                    pass.result_referenced_finding_id
                        AS pass_result_referenced_finding_id,
                    pass.result_referenced_finding_run_id
                        AS pass_result_referenced_finding_run_id,
                    pass.result_referenced_finding_pass_id
                        AS pass_result_referenced_finding_pass_id,
                    pass.result_referenced_finding_status
                        AS pass_result_referenced_finding_status,
                    pass.result_external_link_id
                        AS pass_result_external_link_id,
                    pass.result_external_object_key
                        AS pass_result_external_object_key,
                    pass.result_observation_state
                        AS pass_result_observation_state,
                    pass_run.run_id AS canonical_observation_run_id,
                    pass_run.workflow_kind AS pass_workflow_kind,
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
            require_joined_reference(
                &row,
                "canonical_observation_run_id",
                "review_external_link_observation",
                "observing run row is missing",
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
            &target,
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

async fn load_produced_findings(
    transaction: &mut Transaction<'_, Postgres>,
    pass: ReviewPassId,
) -> Result<Vec<ReviewFindingRef>, ReviewWorkflowStoreError> {
    sqlx::query(
        "SELECT target_id, finding_run_id, finding_pass_id, finding_id
           FROM review_pass_produced_finding
          WHERE pass_id = $1
          ORDER BY result_ordinal",
    )
    .bind(pass.into_uuid())
    .fetch_all(&mut **transaction)
    .await?
    .into_iter()
    .map(|row| {
        Ok(ReviewFindingRef::new(
            ReviewPassRef::new(
                ReviewRunRef::new(
                    target_id(row.try_get("target_id")?),
                    run_id(row.try_get("finding_run_id")?),
                ),
                pass_id(row.try_get("finding_pass_id")?),
            ),
            finding_id(row.try_get("finding_id")?),
        ))
    })
    .collect()
}

async fn insert_finding_row(
    transaction: &mut Transaction<'_, Postgres>,
    finding: &ReviewFinding,
) -> Result<(), ReviewWorkflowStoreError> {
    let proposal = finding.proposal();
    let reference = proposal.reference();
    let pass = proposal.producing_pass();
    let content = proposal.content();
    let location = content.location();
    let (line_start, line_end) = match location.line_range() {
        Some(range) => (Some(i64::from(range.start())), Some(i64::from(range.end()))),
        None => (None, None),
    };
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
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_finding_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &ReviewFindingEvent,
) -> Result<(), ReviewWorkflowStoreError> {
    let finding = event.finding();
    let encoded = encode_finding_event(event.kind());
    let pass = event.pass_evidence();
    bind_pass_result(transaction, pass).await?;
    sqlx::query(
        "INSERT INTO review_finding_event
            (finding_id, event_ordinal, finding_run_id, target_id,
             event_pass_id, event_pass_run_id, event_kind, reason,
             referenced_finding_id, referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
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
    .bind(encoded.referenced_status.map(encode_finding_status))
    .bind(encoded.external_link.map(ReviewExternalLinkId::into_uuid))
    .bind(encoded.external_link.map(|_| "finding"))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn bind_pass_result(
    transaction: &mut Transaction<'_, Postgres>,
    pass: &ReviewPassEvidence,
) -> Result<(), ReviewWorkflowStoreError> {
    let state = encode_pass_state(pass.state());
    let Some(result) = state.result else {
        return Err(corruption(
            "review_pass",
            String::from("effect pass omitted its exact typed result"),
        ));
    };
    let referenced = result.referenced.map(|evidence| evidence.reference());
    let bound = sqlx::query(
        "UPDATE review_pass
            SET result_kind = $6,
                result_finding_id = $7,
                result_finding_run_id = $8,
                result_finding_pass_id = $9,
                result_event_ordinal = $10,
                result_event_kind = $11,
                result_reason = $12,
                result_referenced_finding_id = $13,
                result_referenced_finding_run_id = $14,
                result_referenced_finding_pass_id = $15,
                result_referenced_finding_status = $16,
                result_external_link_id = $17,
                result_external_object_key = $18,
                result_observation_state = $19
          WHERE pass_id = $1
            AND run_id = $2
            AND target_id = $3
            AND state_kind = $4
            AND turn_id IS NOT DISTINCT FROM $5
            AND output_frontier_id IS NOT DISTINCT FROM $20
            AND (
                result_kind IS NULL
                OR (
                    result_kind,
                    result_finding_id,
                    result_finding_run_id,
                    result_finding_pass_id,
                    result_event_ordinal,
                    result_event_kind,
                    result_reason,
                    result_referenced_finding_id,
                    result_referenced_finding_run_id,
                    result_referenced_finding_pass_id,
                    result_referenced_finding_status,
                    result_external_link_id,
                    result_external_object_key,
                    result_observation_state
                ) IS NOT DISTINCT FROM (
                    $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                    $16, $17, $18, $19
                )
            )
        RETURNING pass_id",
    )
    .bind(pass.reference().pass().into_uuid())
    .bind(pass.reference().run().run().into_uuid())
    .bind(pass.reference().target().into_uuid())
    .bind(state.kind)
    .bind(state.turn.map(TurnId::into_uuid))
    .bind(result.kind)
    .bind(result.finding.map(|finding| finding.finding().into_uuid()))
    .bind(
        result
            .finding
            .map(|finding| finding.run().run().into_uuid()),
    )
    .bind(
        result
            .finding
            .map(|finding| finding.pass().pass().into_uuid()),
    )
    .bind(result.ordinal.map(|ordinal| i64::from(ordinal.get())))
    .bind(result.event_kind)
    .bind(result.reason)
    .bind(referenced.map(|finding| finding.finding().into_uuid()))
    .bind(referenced.map(|finding| finding.run().run().into_uuid()))
    .bind(referenced.map(|finding| finding.pass().pass().into_uuid()))
    .bind(
        result
            .referenced
            .map(|evidence| encode_finding_status(evidence.status())),
    )
    .bind(result.external_link.map(ReviewExternalLinkId::into_uuid))
    .bind(result.external_object)
    .bind(result.observation_state.map(encode_external_object_state))
    .bind(state.frontier.map(ContextFrontierId::into_uuid))
    .fetch_optional(&mut **transaction)
    .await?;
    if bound.is_none() {
        return Err(corruption(
            "review_pass",
            String::from("effect pass row or compatible canonical outcome is missing"),
        ));
    }
    Ok(())
}

fn decode_target(row: &PgRow) -> Result<ReviewTarget, ReviewWorkflowStoreError> {
    let id = target_id(row.try_get("target_id")?);
    let provider = review_key(row.try_get("provider_key")?, "review_target")?;
    let repository = review_key(row.try_get("repository_key")?, "review_target")?;
    let subject_kind: String = row.try_get("subject_kind")?;
    let change_request_number: Option<Decimal> = row.try_get("change_request_number")?;
    let subject = decode_target_subject(&subject_kind, change_request_number)?;
    let stack_parent_target = row.try_get::<Option<Uuid>, _>("stack_parent_target_id")?;
    let stack_parent_provider = row.try_get::<Option<String>, _>("stack_parent_provider_key")?;
    let stack_parent_repository =
        row.try_get::<Option<String>, _>("stack_parent_repository_key")?;
    let stack_parent_subject = row.try_get::<Option<String>, _>("stack_parent_subject_kind")?;
    let stack_parent_change_request =
        row.try_get::<Option<Decimal>, _>("stack_parent_change_request_number")?;
    let stack_parent_head = row.try_get::<Option<String>, _>("stack_parent_head_revision")?;
    let stack_parent_base = row.try_get::<Option<String>, _>("stack_parent_base_revision")?;
    let stack_parent = match (
        stack_parent_target,
        stack_parent_provider,
        stack_parent_repository,
        stack_parent_subject,
        stack_parent_head,
    ) {
        (None, None, None, None, None)
            if stack_parent_change_request.is_none() && stack_parent_base.is_none() =>
        {
            None
        }
        (
            Some(target),
            Some(parent_provider),
            Some(parent_repository),
            Some(parent_subject),
            Some(parent_head),
        ) => Some(
            ReviewTarget::try_new(
                target_id(target),
                review_key(parent_provider, "review_target")?,
                review_key(parent_repository, "review_target")?,
                decode_target_subject(&parent_subject, stack_parent_change_request)?,
                review_key(parent_head, "review_target")?,
                stack_parent_base
                    .map(|value| review_key(value, "review_target"))
                    .transpose()?,
                None,
            )
            .map_err(|error| corruption("review_target", format!("{error:?}")))?,
        ),
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
        stack_parent.as_ref(),
    )
    .map_err(|error| corruption("review_target", format!("{error:?}")))
}

fn decode_target_subject(
    subject_kind: &str,
    change_request_number: Option<Decimal>,
) -> Result<ReviewTargetSubject, ReviewWorkflowStoreError> {
    Ok(match (subject_kind, change_request_number) {
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
    })
}

fn decode_run(
    row: PgRow,
    produced_findings: Vec<ReviewFindingRef>,
) -> Result<ReviewRun, ReviewWorkflowStoreError> {
    let pass_evidence = decode_run_pass_evidence(&row, produced_findings)?;
    reconstitute_run(&row, pass_evidence)
}

fn decode_run_for_transition(
    row: PgRow,
) -> Result<(ReviewRun, Option<ReviewPassEvidence>), ReviewWorkflowStoreError> {
    let canonical_evidence = decode_run_pass_evidence(&row, Vec::new())?;
    let (_, _, _, state) = decode_run_facts(&row)?;
    let current_evidence = projected_current_run_pass_evidence(state, canonical_evidence.clone())?;
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
    produced_findings: Vec<ReviewFindingRef>,
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
                decode_pass_state(
                    &state_kind,
                    turn,
                    frontier,
                    stored_pass_result(row, "evidence_pass_target_id", "evidence_pass_")?,
                    produced_findings,
                )?,
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
        return Ok(canonical);
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
        _ => canonical.state().clone(),
    };
    Ok(Some(ReviewPassEvidence::new(
        canonical.reference(),
        canonical.kind(),
        canonical.policy(),
        projected,
    )))
}

fn decode_pass(
    row: PgRow,
    produced_findings: Vec<ReviewFindingRef>,
) -> Result<ReviewPass, ReviewWorkflowStoreError> {
    let turn_evidence = decode_pass_turn_evidence(&row)?;
    reconstitute_pass(&row, turn_evidence, produced_findings)
}

fn decode_pass_for_transition(
    row: PgRow,
) -> Result<(ReviewPass, Option<ReviewPassTurnEvidence>), ReviewWorkflowStoreError> {
    let canonical_evidence = decode_pass_turn_evidence(&row)?;
    let state = decode_pass_row_state(&row)?;
    let current_evidence = projected_current_pass_turn_evidence(state, canonical_evidence)?;
    Ok((
        reconstitute_pass(&row, current_evidence, Vec::new())?,
        canonical_evidence,
    ))
}

fn reconstitute_pass(
    row: &PgRow,
    turn_evidence: Option<ReviewPassTurnEvidence>,
    produced_findings: Vec<ReviewFindingRef>,
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
        ReviewPassAcceptedInputEvidence::new(
            accepted_input_id(row.try_get("accepted_input_id")?),
            session_id(accepted_input_session),
            Some(turn_id(row.try_get("origin_turn_id")?)),
        ),
        decode_pass_state(
            &state_kind,
            turn,
            frontier,
            stored_pass_result(row, "target_id", "")?,
            produced_findings,
        )?,
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
        stored_pass_result(row, "target_id", "")?,
        Vec::new(),
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
    let Some(expected_turn) = pass_state_turn(&state) else {
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

fn decode_finding_proposal(
    row: &PgRow,
    produced_findings: Vec<ReviewFindingRef>,
) -> Result<ReviewFindingProposal, ReviewWorkflowStoreError> {
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
            stored_pass_result(row, "target_id", "producing_pass_")?,
            produced_findings,
        )?,
    );
    let producing_run = ReviewRunEvidence::new(
        run,
        decode_workflow_kind(&row.try_get::<String, _>("producing_workflow_kind")?)?,
        producing_pass.policy(),
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
    ReviewFindingProposal::try_new(reference, producing_pass, producing_run, &target, content)
        .map_err(|error| corruption("review_finding", format!("{error:?}")))
}

fn decode_finding_event(
    row: &PgRow,
    finding: ReviewFindingRef,
    target_snapshot: &ReviewTarget,
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
            stored_pass_result(row, "target_id", "event_pass_")?,
            Vec::new(),
        )?,
    );
    let run = ReviewRunEvidence::new(
        pass_reference.run(),
        decode_workflow_kind(&row.try_get::<String, _>("event_workflow_kind")?)?,
        pass.policy(),
    );
    let kind: String = row.try_get("event_kind")?;
    let reason: Option<String> = row.try_get("reason")?;
    let referenced: Option<Uuid> = row.try_get("referenced_finding_id")?;
    let referenced_pass: Option<Uuid> = row.try_get("referenced_finding_pass_id")?;
    let referenced_status: Option<String> = row.try_get("referenced_finding_status")?;
    let external_link: Option<Uuid> = row.try_get("external_link_id")?;
    let kind = match (
        kind.as_str(),
        reason,
        referenced,
        referenced_pass,
        referenced_status,
        external_link,
    ) {
        ("accepted", None, None, None, None, None) => ReviewFindingEventKind::Accepted,
        ("rejected", Some(reason), None, None, None, None) => ReviewFindingEventKind::Rejected {
            reason: review_text(reason, "review_finding_event")?,
        },
        (
            "duplicate",
            None,
            Some(referenced),
            Some(referenced_pass),
            Some(referenced_status),
            None,
        ) => ReviewFindingEventKind::Duplicate {
            canonical: ReviewReferencedFindingEvidence::try_reconstitute(
                ReviewFindingRef::new(
                    ReviewPassRef::new(finding.run(), pass_id(referenced_pass)),
                    finding_id(referenced),
                ),
                decode_finding_status(&referenced_status)?,
            )
            .ok_or_else(|| {
                corruption(
                    "review_finding_event",
                    String::from("referenced finding status is ineligible"),
                )
            })?,
        },
        (
            "superseded",
            None,
            Some(referenced),
            Some(referenced_pass),
            Some(referenced_status),
            None,
        ) => ReviewFindingEventKind::Superseded {
            successor: ReviewReferencedFindingEvidence::try_reconstitute(
                ReviewFindingRef::new(
                    ReviewPassRef::new(finding.run(), pass_id(referenced_pass)),
                    finding_id(referenced),
                ),
                decode_finding_status(&referenced_status)?,
            )
            .ok_or_else(|| {
                corruption(
                    "review_finding_event",
                    String::from("referenced finding status is ineligible"),
                )
            })?,
        },
        ("stale", None, None, None, None, None) => ReviewFindingEventKind::Stale,
        ("posted", None, None, None, None, Some(link)) => {
            let canonical = decode_finding_external_link_aggregate(
                row,
                external_link_id(link),
                target_snapshot,
            )?;
            ReviewFindingEventKind::Posted {
                link: Box::new(
                    ReviewFindingExternalLinkRef::try_new(finding, &canonical).map_err(
                        |error| {
                            corruption(
                                "review_finding_event",
                                format!("invalid canonical posted link: {:?}", error.failure()),
                            )
                        },
                    )?,
                ),
            }
        }
        ("fixed", None, None, None, None, None) => ReviewFindingEventKind::Fixed,
        ("blocked_with_reason", Some(reason), None, None, None, link) => {
            let link = link
                .map(|link| {
                    let canonical = decode_finding_external_link_aggregate(
                        row,
                        external_link_id(link),
                        target_snapshot,
                    )?;
                    let reservation = ReviewExternalLink::try_reserve(
                        canonical.id(),
                        canonical.association(),
                        canonical.provider().clone(),
                        canonical.object_kind(),
                        target_snapshot,
                    )
                    .map_err(|error| {
                        corruption(
                            "review_finding_event",
                            format!("invalid historical publication reservation: {error:?}"),
                        )
                    })?;
                    ReviewFindingPendingExternalLinkRef::try_new(finding, &reservation)
                        .map(Box::new)
                        .map_err(|error| {
                            corruption(
                                "review_finding_event",
                                format!("invalid pending publication link: {:?}", error.failure()),
                            )
                        })
                })
                .transpose()?;
            ReviewFindingEventKind::BlockedWithReason {
                reason: review_text(reason, "review_finding_event")?,
                link,
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
        run,
        kind,
    ))
}

fn decode_finding_external_link_aggregate(
    row: &PgRow,
    event_link: ReviewExternalLinkId,
    target_snapshot: &ReviewTarget,
) -> Result<ReviewExternalLink, ReviewWorkflowStoreError> {
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
    if target != target_snapshot.id() {
        return Err(corruption(
            "review_finding_event",
            String::from("posted event link target evidence mismatch"),
        ));
    }
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
        ) => {
            let reference = ReviewPassRef::new(
                ReviewRunRef::new(target_id(target), run_id(run)),
                pass_id(pass),
            );
            let policy = decode_review_policy(
                row,
                "attachment_policy_version",
                "attachment_minimum_judge_confidence",
                "attachment_minimum_publication_confidence",
                "review_finding_event",
            )?;
            let pass = ReviewPassEvidence::new(
                reference,
                decode_pass_kind(&kind)?,
                policy,
                decode_pass_state(
                    &state,
                    attachment_pass_turn,
                    attachment_pass_frontier,
                    stored_pass_result(row, "attachment_target_id", "attachment_pass_")?,
                    Vec::new(),
                )?,
            );
            Some(ReviewExternalLinkAttachment::new(
                external_link_id(link),
                pass,
                ReviewRunEvidence::new(
                    reference.run(),
                    decode_workflow_kind(&row.try_get::<String, _>("attachment_workflow_kind")?)?,
                    policy,
                ),
                review_key(object, "review_external_link_attachment")?,
            ))
        }
        _ => {
            return Err(corruption(
                "review_finding_event",
                String::from("torn posted link attachment"),
            ));
        }
    };
    ReviewExternalLink::try_reconstitute(
        id,
        association,
        review_key(provider, "review_external_link")?,
        decode_external_object_kind(&object_kind)?,
        attachment,
        Vec::new(),
        target_snapshot,
    )
    .map_err(|error| corruption("review_finding_event", format!("{error:?}")))
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
        review_key(row.try_get("link_provider_key")?, "review_external_link")?,
        decode_external_object_kind(&row.try_get::<String, _>("object_kind")?)?,
    ))
}

fn decode_external_link_attachment(
    row: &PgRow,
) -> Result<ReviewExternalLinkAttachment, ReviewWorkflowStoreError> {
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(row.try_get("target_id")?),
            run_id(row.try_get("pass_run_id")?),
        ),
        pass_id(row.try_get("pass_id")?),
    );
    let policy = decode_review_policy(
        row,
        "pass_policy_version",
        "pass_minimum_judge_confidence",
        "pass_minimum_publication_confidence",
        "review_external_link_attachment",
    )?;
    Ok(ReviewExternalLinkAttachment::new(
        external_link_id(row.try_get("external_link_id")?),
        ReviewPassEvidence::new(
            reference,
            decode_pass_kind(&row.try_get::<String, _>("pass_kind")?)?,
            policy,
            decode_pass_state(
                &row.try_get::<String, _>("pass_state_kind")?,
                row.try_get("pass_turn_id")?,
                row.try_get("pass_output_frontier_id")?,
                stored_pass_result(row, "target_id", "pass_")?,
                Vec::new(),
            )?,
        ),
        ReviewRunEvidence::new(
            reference.run(),
            decode_workflow_kind(&row.try_get::<String, _>("pass_workflow_kind")?)?,
            policy,
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
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(row.try_get("target_id")?),
            run_id(row.try_get("pass_run_id")?),
        ),
        pass_id(row.try_get("pass_id")?),
    );
    let policy = decode_review_policy(
        row,
        "pass_policy_version",
        "pass_minimum_judge_confidence",
        "pass_minimum_publication_confidence",
        "review_external_link_observation",
    )?;
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
            reference,
            decode_pass_kind(&row.try_get::<String, _>("pass_kind")?)?,
            policy,
            decode_pass_state(
                &row.try_get::<String, _>("pass_state_kind")?,
                row.try_get("pass_turn_id")?,
                row.try_get("pass_output_frontier_id")?,
                stored_pass_result(row, "target_id", "pass_")?,
                Vec::new(),
            )?,
        ),
        ReviewRunEvidence::new(
            reference.run(),
            decode_workflow_kind(&row.try_get::<String, _>("pass_workflow_kind")?)?,
            policy,
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

struct EncodedPassState<'a> {
    kind: &'static str,
    turn: Option<TurnId>,
    frontier: Option<ContextFrontierId>,
    result: Option<EncodedPassResult<'a>>,
}

#[derive(Clone, Copy)]
struct EncodedPassResult<'a> {
    kind: &'static str,
    finding: Option<ReviewFindingRef>,
    ordinal: Option<ReviewEventOrdinal>,
    event_kind: Option<&'static str>,
    reason: Option<&'a str>,
    referenced: Option<ReviewReferencedFindingEvidence>,
    external_link: Option<ReviewExternalLinkId>,
    external_object: Option<&'a str>,
    observation_state: Option<ReviewExternalObjectState>,
}

fn pass_state_turn(state: &ReviewPassState) -> Option<TurnId> {
    match state {
        ReviewPassState::Queued | ReviewPassState::Cancelled { turn: None } => None,
        ReviewPassState::Running { turn }
        | ReviewPassState::Succeeded { turn, .. }
        | ReviewPassState::Failed { turn }
        | ReviewPassState::Blocked { turn, .. }
        | ReviewPassState::Cancelled { turn: Some(turn) } => Some(*turn),
    }
}

fn pass_state_result(state: &ReviewPassState) -> Option<&ReviewPassResult> {
    match state {
        ReviewPassState::Succeeded { result, .. } | ReviewPassState::Blocked { result, .. } => {
            result.as_ref()
        }
        ReviewPassState::Queued
        | ReviewPassState::Running { .. }
        | ReviewPassState::Failed { .. }
        | ReviewPassState::Cancelled { .. } => None,
    }
}

fn encode_pass_state(state: &ReviewPassState) -> EncodedPassState<'_> {
    match state {
        ReviewPassState::Queued => EncodedPassState {
            kind: "queued",
            turn: None,
            frontier: None,
            result: None,
        },
        ReviewPassState::Running { turn } => EncodedPassState {
            kind: "running",
            turn: Some(*turn),
            frontier: None,
            result: None,
        },
        ReviewPassState::Succeeded {
            turn,
            output_frontier,
            result,
        } => EncodedPassState {
            kind: "succeeded",
            turn: Some(*turn),
            frontier: Some(*output_frontier),
            result: result.as_ref().map(encode_pass_result),
        },
        ReviewPassState::Failed { turn } => EncodedPassState {
            kind: "failed",
            turn: Some(*turn),
            frontier: None,
            result: None,
        },
        ReviewPassState::Blocked { turn, result } => EncodedPassState {
            kind: "blocked",
            turn: Some(*turn),
            frontier: None,
            result: result.as_ref().map(encode_pass_result),
        },
        ReviewPassState::Cancelled { turn } => EncodedPassState {
            kind: "cancelled",
            turn: *turn,
            frontier: None,
            result: None,
        },
    }
}

fn encode_pass_result(result: &ReviewPassResult) -> EncodedPassResult<'_> {
    let empty = |kind| EncodedPassResult {
        kind,
        finding: None,
        ordinal: None,
        event_kind: None,
        reason: None,
        referenced: None,
        external_link: None,
        external_object: None,
        observation_state: None,
    };
    match result {
        ReviewPassResult::ProducedFindings(_) => empty("produced_findings"),
        ReviewPassResult::FindingEvent(event) => {
            encode_pass_finding_event("finding_event", event, None, None)
        }
        ReviewPassResult::ExternalLinkAttachment(attachment) => match attachment.finding_event() {
            Some(event) => encode_pass_finding_event(
                "external_link_attachment",
                event,
                Some(attachment.link()),
                Some(attachment.external_object().as_str()),
            ),
            None => EncodedPassResult {
                external_link: Some(attachment.link()),
                external_object: Some(attachment.external_object().as_str()),
                ..empty("external_link_attachment")
            },
        },
        ReviewPassResult::ExternalLinkObservation(observation) => EncodedPassResult {
            ordinal: Some(observation.ordinal()),
            external_link: Some(observation.link()),
            observation_state: Some(observation.state()),
            ..empty("external_link_observation")
        },
    }
}

fn encode_pass_finding_event<'a>(
    result_kind: &'static str,
    event: &'a ReviewFindingEventResult,
    attachment_link: Option<ReviewExternalLinkId>,
    external_object: Option<&'a str>,
) -> EncodedPassResult<'a> {
    let (event_kind, reason, referenced, event_link) = match event.kind() {
        ReviewFindingEventResultKind::Accepted => ("accepted", None, None, None),
        ReviewFindingEventResultKind::Rejected { reason } => {
            ("rejected", Some(reason.as_str()), None, None)
        }
        ReviewFindingEventResultKind::Duplicate { canonical } => {
            ("duplicate", None, Some(*canonical), None)
        }
        ReviewFindingEventResultKind::Superseded { successor } => {
            ("superseded", None, Some(*successor), None)
        }
        ReviewFindingEventResultKind::Stale => ("stale", None, None, None),
        ReviewFindingEventResultKind::Posted { link } => ("posted", None, None, Some(*link)),
        ReviewFindingEventResultKind::Fixed => ("fixed", None, None, None),
        ReviewFindingEventResultKind::BlockedWithReason { reason, link } => {
            ("blocked_with_reason", Some(reason.as_str()), None, *link)
        }
    };
    EncodedPassResult {
        kind: result_kind,
        finding: Some(event.finding()),
        ordinal: Some(event.ordinal()),
        event_kind: Some(event_kind),
        reason,
        referenced,
        external_link: attachment_link.or(event_link),
        external_object,
        observation_state: None,
    }
}

struct StoredPassResult {
    kind: Option<String>,
    target: Uuid,
    finding: Option<Uuid>,
    finding_run: Option<Uuid>,
    finding_pass: Option<Uuid>,
    ordinal: Option<i64>,
    event_kind: Option<String>,
    reason: Option<String>,
    referenced_finding: Option<Uuid>,
    referenced_run: Option<Uuid>,
    referenced_pass: Option<Uuid>,
    referenced_status: Option<String>,
    external_link: Option<Uuid>,
    external_object: Option<String>,
    observation_state: Option<String>,
}

fn stored_pass_result(
    row: &PgRow,
    target_column: &str,
    prefix: &str,
) -> Result<StoredPassResult, ReviewWorkflowStoreError> {
    let column = |suffix: &str| format!("{prefix}{suffix}");
    Ok(StoredPassResult {
        kind: row.try_get(column("result_kind").as_str())?,
        target: row.try_get(target_column)?,
        finding: row.try_get(column("result_finding_id").as_str())?,
        finding_run: row.try_get(column("result_finding_run_id").as_str())?,
        finding_pass: row.try_get(column("result_finding_pass_id").as_str())?,
        ordinal: row.try_get(column("result_event_ordinal").as_str())?,
        event_kind: row.try_get(column("result_event_kind").as_str())?,
        reason: row.try_get(column("result_reason").as_str())?,
        referenced_finding: row.try_get(column("result_referenced_finding_id").as_str())?,
        referenced_run: row.try_get(column("result_referenced_finding_run_id").as_str())?,
        referenced_pass: row.try_get(column("result_referenced_finding_pass_id").as_str())?,
        referenced_status: row.try_get(column("result_referenced_finding_status").as_str())?,
        external_link: row.try_get(column("result_external_link_id").as_str())?,
        external_object: row.try_get(column("result_external_object_key").as_str())?,
        observation_state: row.try_get(column("result_observation_state").as_str())?,
    })
}

fn decode_pass_state(
    kind: &str,
    turn: Option<Uuid>,
    frontier: Option<Uuid>,
    stored: StoredPassResult,
    produced_findings: Vec<ReviewFindingRef>,
) -> Result<ReviewPassState, ReviewWorkflowStoreError> {
    let result = decode_pass_result(stored, produced_findings)?;
    match (kind, turn, frontier, result) {
        ("queued", None, None, None) => Ok(ReviewPassState::Queued),
        ("running", Some(turn), None, None) => Ok(ReviewPassState::Running {
            turn: turn_id(turn),
        }),
        ("succeeded", Some(turn), Some(frontier), result) => Ok(ReviewPassState::Succeeded {
            turn: turn_id(turn),
            output_frontier: context_frontier_id(frontier),
            result,
        }),
        ("failed", Some(turn), None, None) => Ok(ReviewPassState::Failed {
            turn: turn_id(turn),
        }),
        ("blocked", Some(turn), None, result) => Ok(ReviewPassState::Blocked {
            turn: turn_id(turn),
            result,
        }),
        ("cancelled", turn, None, None) => Ok(ReviewPassState::Cancelled {
            turn: turn.map(turn_id),
        }),
        _ => Err(corruption(
            "review_pass",
            format!("invalid state shape {kind}"),
        )),
    }
}

fn decode_pass_result(
    stored: StoredPassResult,
    produced_findings: Vec<ReviewFindingRef>,
) -> Result<Option<ReviewPassResult>, ReviewWorkflowStoreError> {
    let scalar_empty = stored.finding.is_none()
        && stored.finding_run.is_none()
        && stored.finding_pass.is_none()
        && stored.ordinal.is_none()
        && stored.event_kind.is_none()
        && stored.reason.is_none()
        && stored.referenced_finding.is_none()
        && stored.referenced_run.is_none()
        && stored.referenced_pass.is_none()
        && stored.referenced_status.is_none()
        && stored.external_link.is_none()
        && stored.external_object.is_none()
        && stored.observation_state.is_none();
    match stored.kind.as_deref() {
        None if scalar_empty && produced_findings.is_empty() => Ok(None),
        Some("produced_findings") if scalar_empty => Ok(Some(ReviewPassResult::ProducedFindings(
            ReviewProducedFindings::try_new(produced_findings).map_err(|error| {
                corruption(
                    "review_pass",
                    format!("invalid finding inventory: {error:?}"),
                )
            })?,
        ))),
        Some("finding_event")
            if stored.external_object.is_none() && stored.observation_state.is_none() =>
        {
            Ok(Some(ReviewPassResult::FindingEvent(
                decode_stored_finding_event(&stored)?,
            )))
        }
        Some("external_link_attachment") if stored.observation_state.is_none() => {
            let link = stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("attachment result omitted link"),
                )
            })?;
            let external_object = review_key(
                stored.external_object.clone().ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("attachment result omitted object key"),
                    )
                })?,
                "review_pass",
            )?;
            let finding_event = if stored.finding.is_some() {
                Some(decode_stored_finding_event(&stored)?)
            } else {
                None
            };
            Ok(Some(ReviewPassResult::ExternalLinkAttachment(
                ReviewExternalLinkAttachmentResult::new(link, external_object, finding_event),
            )))
        }
        Some("external_link_observation")
            if stored.finding.is_none()
                && stored.event_kind.is_none()
                && stored.reason.is_none()
                && stored.referenced_finding.is_none()
                && stored.external_object.is_none() =>
        {
            let link = stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("observation result omitted link"),
                )
            })?;
            let ordinal = stored.ordinal.ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("observation result omitted ordinal"),
                )
            })?;
            let state = stored.observation_state.as_deref().ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("observation result omitted state"),
                )
            })?;
            Ok(Some(ReviewPassResult::ExternalLinkObservation(
                ReviewExternalLinkObservationResult::new(
                    link,
                    ReviewEventOrdinal::try_new(positive_u32(ordinal, "review_pass")?).map_err(
                        |_| corruption("review_pass", String::from("zero observation ordinal")),
                    )?,
                    decode_external_object_state(state)?,
                ),
            )))
        }
        _ => Err(corruption(
            "review_pass",
            String::from("torn or unknown pass result"),
        )),
    }
}

fn decode_stored_finding_event(
    stored: &StoredPassResult,
) -> Result<ReviewFindingEventResult, ReviewWorkflowStoreError> {
    let finding = ReviewFindingRef::new(
        ReviewPassRef::new(
            ReviewRunRef::new(
                target_id(stored.target),
                run_id(stored.finding_run.ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("event result omitted finding run"),
                    )
                })?),
            ),
            pass_id(stored.finding_pass.ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("event result omitted finding pass"),
                )
            })?),
        ),
        finding_id(stored.finding.ok_or_else(|| {
            corruption("review_pass", String::from("event result omitted finding"))
        })?),
    );
    let ordinal = ReviewEventOrdinal::try_new(positive_u32(
        stored.ordinal.ok_or_else(|| {
            corruption("review_pass", String::from("event result omitted ordinal"))
        })?,
        "review_pass",
    )?)
    .map_err(|_| corruption("review_pass", String::from("zero event ordinal")))?;
    let event_kind = stored.event_kind.as_deref().ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("event result omitted discriminator"),
        )
    })?;
    let kind = match event_kind {
        "accepted" => ReviewFindingEventResultKind::Accepted,
        "rejected" => ReviewFindingEventResultKind::Rejected {
            reason: review_text(
                stored.reason.clone().ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("rejected result omitted reason"),
                    )
                })?,
                "review_pass",
            )?,
        },
        "duplicate" => ReviewFindingEventResultKind::Duplicate {
            canonical: decode_stored_referenced_finding(stored)?,
        },
        "superseded" => ReviewFindingEventResultKind::Superseded {
            successor: decode_stored_referenced_finding(stored)?,
        },
        "stale" => ReviewFindingEventResultKind::Stale,
        "posted" => ReviewFindingEventResultKind::Posted {
            link: stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption("review_pass", String::from("posted result omitted link"))
            })?,
        },
        "fixed" => ReviewFindingEventResultKind::Fixed,
        "blocked_with_reason" => ReviewFindingEventResultKind::BlockedWithReason {
            reason: review_text(
                stored.reason.clone().ok_or_else(|| {
                    corruption("review_pass", String::from("blocked result omitted reason"))
                })?,
                "review_pass",
            )?,
            link: stored.external_link.map(external_link_id),
        },
        other => {
            return Err(corruption(
                "review_pass",
                format!("unknown finding-event result {other}"),
            ));
        }
    };
    Ok(ReviewFindingEventResult::new(finding, ordinal, kind))
}

fn decode_stored_referenced_finding(
    stored: &StoredPassResult,
) -> Result<ReviewReferencedFindingEvidence, ReviewWorkflowStoreError> {
    let reference = ReviewFindingRef::new(
        ReviewPassRef::new(
            ReviewRunRef::new(
                target_id(stored.target),
                run_id(stored.referenced_run.ok_or_else(|| {
                    corruption("review_pass", String::from("referenced result omitted run"))
                })?),
            ),
            pass_id(stored.referenced_pass.ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("referenced result omitted producing pass"),
                )
            })?),
        ),
        finding_id(stored.referenced_finding.ok_or_else(|| {
            corruption(
                "review_pass",
                String::from("referenced result omitted finding"),
            )
        })?),
    );
    let status = decode_finding_status(stored.referenced_status.as_deref().ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result omitted status"),
        )
    })?)?;
    ReviewReferencedFindingEvidence::try_reconstitute(reference, status).ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result carried ineligible status"),
        )
    })
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
    finding_pass: Option<ReviewPassId>,
}

fn encode_link_association(association: ReviewExternalLinkAssociation) -> EncodedLinkAssociation {
    match association {
        ReviewExternalLinkAssociation::Target(_) => EncodedLinkAssociation {
            kind: "target",
            run: None,
            finding: None,
            finding_pass: None,
        },
        ReviewExternalLinkAssociation::Run(run) => EncodedLinkAssociation {
            kind: "run",
            run: Some(run.run()),
            finding: None,
            finding_pass: None,
        },
        ReviewExternalLinkAssociation::Finding(finding) => EncodedLinkAssociation {
            kind: "finding",
            run: Some(finding.run().run()),
            finding: Some(finding.finding()),
            finding_pass: Some(finding.pass().pass()),
        },
    }
}

struct EncodedFindingEvent<'a> {
    kind: &'static str,
    reason: Option<&'a str>,
    referenced_finding: Option<ReviewFindingId>,
    referenced_status: Option<ReviewFindingStatus>,
    external_link: Option<ReviewExternalLinkId>,
}

fn encode_finding_event(event: &ReviewFindingEventKind) -> EncodedFindingEvent<'_> {
    let empty = |kind| EncodedFindingEvent {
        kind,
        reason: None,
        referenced_finding: None,
        referenced_status: None,
        external_link: None,
    };
    match event {
        ReviewFindingEventKind::Accepted => empty("accepted"),
        ReviewFindingEventKind::Rejected { reason } => EncodedFindingEvent {
            kind: "rejected",
            reason: Some(reason.as_str()),
            referenced_finding: None,
            referenced_status: None,
            external_link: None,
        },
        ReviewFindingEventKind::Duplicate { canonical } => EncodedFindingEvent {
            kind: "duplicate",
            reason: None,
            referenced_finding: Some(canonical.reference().finding()),
            referenced_status: Some(canonical.status()),
            external_link: None,
        },
        ReviewFindingEventKind::Superseded { successor } => EncodedFindingEvent {
            kind: "superseded",
            reason: None,
            referenced_finding: Some(successor.reference().finding()),
            referenced_status: Some(successor.status()),
            external_link: None,
        },
        ReviewFindingEventKind::Stale => empty("stale"),
        ReviewFindingEventKind::Posted { link } => EncodedFindingEvent {
            kind: "posted",
            reason: None,
            referenced_finding: None,
            referenced_status: None,
            external_link: Some(link.link()),
        },
        ReviewFindingEventKind::Fixed => empty("fixed"),
        ReviewFindingEventKind::BlockedWithReason { reason, link } => EncodedFindingEvent {
            kind: "blocked_with_reason",
            reason: Some(reason.as_str()),
            referenced_finding: None,
            referenced_status: None,
            external_link: link.as_ref().map(|link| link.link()),
        },
    }
}

fn encode_finding_status(status: ReviewFindingStatus) -> &'static str {
    match status {
        ReviewFindingStatus::Open => "open",
        ReviewFindingStatus::Accepted => "accepted",
        ReviewFindingStatus::Rejected => "rejected",
        ReviewFindingStatus::Duplicate => "duplicate",
        ReviewFindingStatus::Superseded => "superseded",
        ReviewFindingStatus::Stale => "stale",
        ReviewFindingStatus::Posted => "posted",
        ReviewFindingStatus::Fixed => "fixed",
        ReviewFindingStatus::BlockedWithReason => "blocked_with_reason",
    }
}

fn decode_finding_status(status: &str) -> Result<ReviewFindingStatus, ReviewWorkflowStoreError> {
    match status {
        "open" => Ok(ReviewFindingStatus::Open),
        "accepted" => Ok(ReviewFindingStatus::Accepted),
        "rejected" => Ok(ReviewFindingStatus::Rejected),
        "duplicate" => Ok(ReviewFindingStatus::Duplicate),
        "superseded" => Ok(ReviewFindingStatus::Superseded),
        "stale" => Ok(ReviewFindingStatus::Stale),
        "posted" => Ok(ReviewFindingStatus::Posted),
        "fixed" => Ok(ReviewFindingStatus::Fixed),
        "blocked_with_reason" => Ok(ReviewFindingStatus::BlockedWithReason),
        other => Err(corruption(
            "review_finding_event",
            format!("unknown referenced-finding status {other}"),
        )),
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewWorkflowInsertionError {
    /// A run insertion carried state that can only result from transition.
    RunNotQueued {
        /// Rejected current state.
        state: Box<ReviewRunState>,
    },
    /// A pass insertion carried state that can only result from transition.
    PassNotQueued {
        /// Rejected current state.
        state: Box<ReviewPassState>,
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
    /// A lifecycle-only transition attempted to persist an effect result.
    NonAtomicPassResult,
    /// A produced-finding write omitted or contradicted the exact inventory.
    IncompleteFindingInventory,
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
            Self::NonAtomicPassResult => formatter.write_str(
                "review pass results must bind in the same transaction as their exact effect",
            ),
            Self::IncompleteFindingInventory => formatter
                .write_str("produced findings must be admitted as one complete exact inventory"),
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
            Self::NonAtomicPassResult | Self::IncompleteFindingInventory => None,
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

//! PostgreSQL store for review-workflow aggregates.
//!
//! SQL rows remain adapter-private. Complete values are reconstructed through
//! the domain API defined by `docs/spec/review-workflows.md`.

use signalbox_application::ReviewWorkflowReader;
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, ReviewChangeRequestNumber, ReviewConfidence,
    ReviewEventOrdinal, ReviewExternalLink, ReviewExternalLinkAssociation,
    ReviewExternalLinkAttachment, ReviewExternalLinkAttachmentResult, ReviewExternalLinkClaim,
    ReviewExternalLinkId, ReviewExternalLinkNoChangeResult, ReviewExternalLinkObservation,
    ReviewExternalLinkObservationResult, ReviewExternalLinkPublicationBlockedResult,
    ReviewExternalLinkTransitionFailure, ReviewExternalObjectKind, ReviewExternalObjectState,
    ReviewFinding, ReviewFindingConfidenceAxes, ReviewFindingContent, ReviewFindingDiffSide,
    ReviewFindingEvent, ReviewFindingEventKind, ReviewFindingEventResult,
    ReviewFindingEventResultKind, ReviewFindingExternalLinkRef, ReviewFindingId,
    ReviewFindingLocation, ReviewFindingPendingExternalLinkRef, ReviewFindingProposal,
    ReviewFindingRef, ReviewFindingSeverity, ReviewFindingStatus, ReviewKey, ReviewLineRange,
    ReviewPass, ReviewPassAcceptedInputEvidence, ReviewPassEvidence, ReviewPassId, ReviewPassKind,
    ReviewPassReconstitutionInput, ReviewPassRef, ReviewPassResult, ReviewPassState,
    ReviewPassTurnEvidence, ReviewPassTurnOutcome, ReviewPolicy, ReviewPolicyVersion,
    ReviewProducedFindings, ReviewReferencedFindingEvidence, ReviewRun, ReviewRunEvidence,
    ReviewRunId, ReviewRunReconstitutionInput, ReviewRunRef, ReviewRunState, ReviewTarget,
    ReviewTargetId, ReviewTargetSubject, ReviewText, ReviewWorkflowKind, SessionId, TurnId,
    validate_complete_review_finding_reference_graph,
};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

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

    /// Borrows the guarded pool for the sibling durable-command adapter.
    pub(crate) const fn pool(&self) -> &PgPool {
        &self.pool
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
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let target = load_target_on_connection(&mut transaction, target).await?;
        transaction.commit().await?;
        Ok(target)
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
        Ok(self.load_run_with_pass(run).await?.map(|(run, _pass)| run))
    }

    /// Loads and validates one run and its recorded pass from one snapshot.
    pub async fn load_run_with_pass(
        &self,
        run: ReviewRunId,
    ) -> Result<Option<(ReviewRun, Option<ReviewPass>)>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self
            .load_run_with_pass_on_connection(&mut transaction, run)
            .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    pub(crate) async fn load_run_with_pass_on_connection(
        &self,
        connection: &mut PgConnection,
        run: ReviewRunId,
    ) -> Result<Option<(ReviewRun, Option<ReviewPass>)>, ReviewWorkflowStoreError> {
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
                    canonical_pass.result_referenced_finding_target_id
                        AS evidence_pass_result_referenced_finding_target_id,
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
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
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
        let loaded_pass = match evidence_pass {
            Some(pass) => Some(
                load_pass_on_connection(&mut *connection, pass)
                    .await?
                    .ok_or_else(|| {
                        corruption("review_run", String::from("referenced pass row is missing"))
                    })?,
            ),
            None => None,
        };
        let run = decode_run(&row, loaded_pass.as_ref())?;
        let pass = loaded_pass.map(|loaded| loaded.pass);
        Ok(Some((run, pass)))
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
        let evidence_pass = row
            .try_get::<Option<Uuid>, _>("evidence_pass_id")?
            .map(pass_id);
        let loaded_pass = match evidence_pass {
            Some(pass) => Some(
                load_pass_on_connection(&mut transaction, pass)
                    .await?
                    .ok_or_else(|| {
                        corruption("review_run", String::from("referenced pass row is missing"))
                    })?,
            ),
            None => None,
        };
        let (current, pass_evidence) = decode_run_for_transition(&row, loaded_pass.as_ref())?;
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

    /// Inserts one queued run and its first queued pass atomically.
    pub async fn insert_run_and_pass(
        &self,
        run: &ReviewRun,
        pass: &ReviewPass,
    ) -> Result<(), ReviewWorkflowStoreError> {
        if run.state() != ReviewRunState::Queued {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::RunNotQueued {
                    state: Box::new(run.state()),
                },
            ));
        }
        if pass.state() != &ReviewPassState::Queued {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::PassNotQueued {
                    state: Box::new(pass.state().clone()),
                },
            ));
        }
        if pass.reference().run() != run.reference()
            || run.recorded_pass() != Some(pass.reference())
            || !workflow_matches_pass_kind(run.workflow(), pass.kind())
        {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::RunPassMismatch,
            ));
        }
        let (run_state_kind, run_state_pass_id) = encode_run_state(run.state());
        let policy = run.policy();
        let pass_state = encode_pass_state(pass.state());
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
        .bind(run_state_kind)
        .bind(run_state_pass_id.map(ReviewPassId::into_uuid))
        .execute(&mut *transaction)
        .await?;
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
        .bind(pass_state.kind)
        .bind(pass_state.turn.map(TurnId::into_uuid))
        .bind(pass_state.frontier.map(ContextFrontierId::into_uuid))
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
        let pass = load_pass_on_connection(&mut transaction, pass)
            .await?
            .map(|loaded| loaded.pass);
        transaction.commit().await?;
        Ok(pass)
    }

    /// Loads the session and originating turn recorded for one accepted input.
    ///
    /// A review run is authorized against the input that produced the turn it
    /// reviews, and this is the projection that check reads.
    pub async fn load_accepted_input_origin(
        &self,
        accepted_input: AcceptedInputId,
    ) -> Result<Option<ReviewAcceptedInputOrigin>, ReviewWorkflowStoreError> {
        let row = sqlx::query(
            "SELECT session_id, origin_turn_id
               FROM accepted_input
              WHERE accepted_input_id = $1",
        )
        .bind(accepted_input.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session: Uuid = projected(&row, "accepted_input", "session_id")?;
        let origin_turn: Option<Uuid> = projected(&row, "accepted_input", "origin_turn_id")?;
        Ok(Some(ReviewAcceptedInputOrigin {
            session: session_id(session),
            origin_turn: origin_turn.map(turn_id),
        }))
    }

    /// Loads the durable lifecycle position recorded for one turn.
    ///
    /// A pass attributes its outcome to a turn, and every fact that
    /// attribution turns on — the owning session, the originating input, the
    /// lifecycle state and its terminal frontier — is decoded here rather than
    /// read column by column outside this crate.
    pub async fn load_turn_lifecycle(
        &self,
        turn: TurnId,
    ) -> Result<Option<ReviewTurnLifecycle>, ReviewWorkflowStoreError> {
        let row = sqlx::query(
            "SELECT session_id, origin_accepted_input_id, state_kind,
                    terminal_disposition_kind, terminal_frontier_id
               FROM turn_lifecycle
              WHERE turn_id = $1",
        )
        .bind(turn.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session: Uuid = projected(&row, "turn_lifecycle", "session_id")?;
        let accepted_input: Option<Uuid> =
            projected(&row, "turn_lifecycle", "origin_accepted_input_id")?;
        let state: String = projected(&row, "turn_lifecycle", "state_kind")?;
        let disposition: Option<String> =
            projected(&row, "turn_lifecycle", "terminal_disposition_kind")?;
        let frontier: Option<Uuid> = projected(&row, "turn_lifecycle", "terminal_frontier_id")?;
        Ok(Some(ReviewTurnLifecycle {
            session: session_id(session),
            accepted_input: accepted_input.map(accepted_input_id),
            state: decode_turn_lifecycle_state(&state, disposition.as_deref())?,
            terminal_frontier: frontier.map(context_frontier_id),
        }))
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
        let turn_evidence = decode_pass_turn_evidence(&pass_row)?;
        let loaded_pass = load_pass_on_connection(&mut transaction, pass)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("locked pass disappeared before complete decoding"),
                )
            })?;
        let current_pass = loaded_pass.pass.clone();
        let (current_run, _) = decode_run_for_transition(&run_row, Some(&loaded_pass))?;
        let transitioned_pass =
            current_pass
                .transition(next_pass, turn_evidence)
                .map_err(|error| {
                    ReviewWorkflowStoreError::InvalidTransition(
                        ReviewWorkflowTransitionError::Pass(error),
                    )
                })?;
        let pass_evidence = ReviewPassEvidence::from_pass(&transitioned_pass, current_run.policy());
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
        sqlx::query(
            "INSERT INTO review_pass_finding_inventory_seal
                (pass_id, finding_count)
             VALUES ($1, $2)",
        )
        .bind(pass.reference().pass().into_uuid())
        .bind(i32::try_from(inventory.findings().len()).map_err(|_| {
            corruption(
                "review_pass_finding_inventory_seal",
                String::from("finding inventory count overflow"),
            )
        })?)
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
        let publication_link = match event.kind() {
            ReviewFindingEventKind::BlockedWithReason {
                link: Some(pending),
                ..
            } => Some(pending.link()),
            _ => None,
        };
        let mut transaction = self.pool.begin().await?;
        if let Some(link) = publication_link {
            // Publication reconciliation locks the reservation before any
            // finding, matching the attachment-path lock order.
            sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
                .bind(link.into_uuid())
                .fetch_one(&mut *transaction)
                .await?;
            sqlx::query(crate::lock_inventory::REVIEW_FINDINGS_TRANSITION)
                .bind(vec![finding.into_uuid()])
                .fetch_all(&mut *transaction)
                .await?;
        } else {
            // Every ordinary event writer locks the complete target inventory
            // in identity order. Under read committed, a waiter then loads the
            // graph from snapshots taken after the winning event commits; the
            // held inventory keeps later event writes from changing that graph
            // across the loader statements.
            sqlx::query(
                "SELECT finding_id
                   FROM review_finding
                  WHERE target_id = (
                            SELECT target_id
                              FROM review_finding
                             WHERE finding_id = $1
                        )
                  ORDER BY finding_id
                  FOR NO KEY UPDATE",
            )
            .bind(finding.into_uuid())
            .fetch_all(&mut *transaction)
            .await?;
        }
        let current = if publication_link.is_some() {
            // The held reservation and finding locks make the subject projection
            // stable while preserving attachment commits observed after waiting.
            self.load_finding_projection_on_connection(&mut transaction, finding)
                .await?
        } else {
            self.load_finding_on_connection(&mut transaction, finding)
                .await?
        };
        let Some(current) = current else {
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
        let finding = self
            .load_finding_on_connection(&mut transaction, finding)
            .await?;
        transaction.commit().await?;
        Ok(finding)
    }

    /// Loads and validates several findings, reading each target graph once.
    ///
    /// [`Self::load_finding`] answers for one identity by reconstructing the
    /// entire target graph that identity belongs to and then selecting a single
    /// member from it. A caller holding a list of identities therefore pays for
    /// that whole reconstruction once per member, which is quadratic in the
    /// number of findings on a target. This performs the same reconstruction
    /// once per distinct target and selects every requested member from it.
    ///
    /// Identities with no `review_finding` row are absent from the result, so a
    /// caller distinguishes them exactly as it would from [`Self::load_finding`]
    /// returning `None`.
    pub async fn load_findings(
        &self,
        findings: &[ReviewFindingId],
    ) -> Result<BTreeMap<ReviewFindingId, ReviewFinding>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self
            .load_findings_on_connection(&mut transaction, findings)
            .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    pub(crate) async fn load_findings_on_connection(
        &self,
        connection: &mut PgConnection,
        findings: &[ReviewFindingId],
    ) -> Result<BTreeMap<ReviewFindingId, ReviewFinding>, ReviewWorkflowStoreError> {
        let requested: BTreeSet<ReviewFindingId> = findings.iter().copied().collect();
        if requested.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows = sqlx::query(
            "SELECT finding_id, target_id
               FROM review_finding
              WHERE finding_id = ANY($1)",
        )
        .bind(
            requested
                .iter()
                .map(|finding| finding.into_uuid())
                .collect::<Vec<_>>(),
        )
        .fetch_all(&mut *connection)
        .await?;
        let mut resolved = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for row in rows {
            resolved.insert(finding_id(row.try_get("finding_id")?));
            targets.insert(row.try_get::<Uuid, _>("target_id")?);
        }
        let mut loaded = BTreeMap::new();
        for target in targets {
            for finding in self
                .load_target_findings_on_connection(connection, target)
                .await?
            {
                let identity = finding.proposal().reference().finding();
                if requested.contains(&identity) {
                    loaded.insert(identity, finding);
                }
            }
        }
        // A resolved identity names a row whose target graph was just loaded
        // under one snapshot, so its absence is the same corruption the
        // single-identity loader reports rather than a missing finding.
        if resolved.iter().any(|finding| !loaded.contains_key(finding)) {
            return Err(corruption(
                "review_finding",
                String::from("requested target graph finding disappeared"),
            ));
        }
        Ok(loaded)
    }

    /// Lists and validates complete findings for one run in identity order.
    pub async fn list_findings(
        &self,
        run: ReviewRunId,
    ) -> Result<Vec<ReviewFinding>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let target = sqlx::query_scalar::<_, Uuid>(
            "SELECT target_id
               FROM review_finding
              WHERE run_id = $1
              ORDER BY finding_id
              LIMIT 1",
        )
        .bind(run.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(target) = target else {
            transaction.commit().await?;
            return Ok(Vec::new());
        };
        let findings = self
            .load_target_findings_on_connection(&mut transaction, target)
            .await?
            .into_iter()
            .filter(|finding| finding.proposal().reference().run().run() == run)
            .collect();
        transaction.commit().await?;
        Ok(findings)
    }

    async fn load_finding_on_connection(
        &self,
        connection: &mut PgConnection,
        finding: ReviewFindingId,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let target = sqlx::query_scalar::<_, Uuid>(
            "SELECT target_id
               FROM review_finding
              WHERE finding_id = $1",
        )
        .bind(finding.into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
        let Some(target) = target else {
            return Ok(None);
        };
        self.load_target_findings_on_connection(connection, target)
            .await?
            .into_iter()
            .find(|candidate| candidate.proposal().reference().finding() == finding)
            .map(Some)
            .ok_or_else(|| {
                corruption(
                    "review_finding",
                    String::from("requested target graph finding disappeared"),
                )
            })
    }

    async fn load_target_findings_on_connection(
        &self,
        connection: &mut PgConnection,
        target: Uuid,
    ) -> Result<Vec<ReviewFinding>, ReviewWorkflowStoreError> {
        let identifiers = sqlx::query_scalar::<_, Uuid>(
            "SELECT finding_id
               FROM review_finding
              WHERE target_id = $1
              ORDER BY finding_id",
        )
        .bind(target)
        .fetch_all(&mut *connection)
        .await?;
        let mut findings = Vec::with_capacity(identifiers.len());
        for identifier in identifiers {
            findings.push(
                self.load_finding_projection_on_connection(connection, finding_id(identifier))
                    .await?
                    .ok_or_else(|| {
                        corruption(
                            "review_finding",
                            String::from("target graph finding disappeared"),
                        )
                    })?,
            );
        }
        validate_complete_review_finding_reference_graph(&findings).map_err(|error| {
            corruption(
                "review_finding",
                format!("reference graph is corrupt: {error:?}"),
            )
        })?;
        Ok(findings)
    }

    async fn load_finding_projection_on_connection(
        &self,
        connection: &mut PgConnection,
        finding: ReviewFindingId,
    ) -> Result<Option<ReviewFinding>, ReviewWorkflowStoreError> {
        let row = sqlx::query(
            "SELECT finding.finding_id, finding.run_id, finding.target_id,
                    finding.producing_pass_id, finding.file_path,
                    finding.line_start, finding.line_end, finding.diff_side,
                    finding.title, finding.body, finding.severity,
                    finding.is_real_confidence,
                    finding.severity_label_confidence, finding.category,
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
                    producing_pass.result_referenced_finding_target_id
                        AS producing_pass_result_referenced_finding_target_id,
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
                        AS producing_minimum_publication_confidence,
                    producing_run.state_kind AS producing_run_state_kind,
                    producing_run.state_pass_id AS producing_run_state_pass_id
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
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
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
        let target = load_target_on_connection(connection, target_id(row.try_get("target_id")?))
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_finding",
                    String::from("referenced target row is missing"),
                )
            })?;
        let producing_pass_id = pass_id(row.try_get("producing_pass_id")?);
        let producing_pass = load_pass_on_connection(connection, producing_pass_id)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_finding",
                    String::from("producing pass row is missing"),
                )
            })?
            .evidence();
        let proposal = decode_finding_proposal(&row, producing_pass, &target)?;
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
                    event_pass.result_referenced_finding_target_id
                        AS event_pass_result_referenced_finding_target_id,
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
                    event_run.state_kind AS event_run_state_kind,
                    event_run.state_pass_id AS event_run_state_pass_id,
                    event.event_kind, event.reason,
                    event.referenced_finding_id,
                    event.referenced_finding_run_id,
                    event.referenced_finding_target_id,
                    event.referenced_finding_pass_id,
                    event.referenced_finding_status,
                    event.external_link_id,
                    referenced_finding.finding_id
                        AS canonical_referenced_finding_id,
                    referenced_finding_pass.pass_id
                        AS canonical_referenced_finding_pass_id
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
                AND referenced_finding.run_id =
                    event.referenced_finding_run_id
                AND referenced_finding.target_id =
                    event.referenced_finding_target_id
                AND referenced_finding.producing_pass_id =
                    event.referenced_finding_pass_id
               LEFT JOIN review_pass AS referenced_finding_pass
                 ON referenced_finding_pass.pass_id =
                    referenced_finding.producing_pass_id
                AND referenced_finding_pass.run_id =
                    referenced_finding.run_id
                AND referenced_finding_pass.target_id =
                    referenced_finding.target_id
              WHERE event.finding_id = $1
              ORDER BY event.event_ordinal",
        )
        .bind(finding.into_uuid())
        .fetch_all(&mut *connection)
        .await?;
        let mut events = Vec::with_capacity(event_rows.len());
        for row in event_rows {
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
                    "canonical_referenced_finding_id",
                    "review_finding_event",
                    "referenced finding row is missing",
                )?;
                require_joined_reference(
                    &row,
                    "canonical_referenced_finding_pass_id",
                    "review_finding_event",
                    "referenced finding producing pass row is missing",
                )?;
            }
            let event_pass_id = pass_id(row.try_get("event_pass_id")?);
            let event_pass = load_pass_on_connection(connection, event_pass_id)
                .await?
                .ok_or_else(|| {
                    corruption(
                        "review_finding_event",
                        String::from("event pass row is missing"),
                    )
                })?
                .evidence();
            let canonical_link = match row
                .try_get::<Option<Uuid>, _>("external_link_id")?
                .map(external_link_id)
            {
                Some(link) => Some(
                    Self::load_external_link_on_connection(connection, link)
                        .await?
                        .ok_or_else(|| {
                            corruption(
                                "review_finding_event",
                                String::from("event external link is missing"),
                            )
                        })?,
                ),
                None => None,
            };
            events.push(decode_finding_event(
                &row,
                proposal.reference(),
                &target,
                event_pass,
                canonical_link.as_ref(),
            )?);
        }
        let finding = ReviewFinding::try_reconstitute(proposal, events).map_err(|error| {
            corruption(
                "review_finding",
                format!("domain reconstitution failed: {:?}", error.failure()),
            )
        })?;
        let head_is_complete = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                   FROM review_finding_event_head AS head
                   LEFT JOIN review_finding_event AS event
                     ON event.finding_id = head.finding_id
                    AND event.event_ordinal = head.event_ordinal
                   LEFT JOIN review_pass AS event_pass
                     ON event_pass.pass_id = event.event_pass_id
                    AND event_pass.run_id = event.event_pass_run_id
                    AND event_pass.target_id = event.target_id
                  WHERE head.finding_id = $1
                    AND (
                        (
                            head.event_ordinal IS NULL
                            AND head.status = $2
                            AND head.event_pass_kind IS NULL
                            AND head.external_link_id IS NULL
                            AND NOT EXISTS (
                                SELECT 1
                                  FROM review_finding_event AS existing
                                 WHERE existing.finding_id = $1
                            )
                        )
                        OR (
                            head.event_ordinal IS NOT NULL
                            AND event.event_kind = head.status
                            AND event_pass.pass_kind = head.event_pass_kind
                            AND event.external_link_id
                                IS NOT DISTINCT FROM head.external_link_id
                            AND head.event_ordinal = (
                                SELECT max(latest.event_ordinal)
                                  FROM review_finding_event AS latest
                                 WHERE latest.finding_id = $1
                            )
                        )
                    )
             )",
        )
        .bind(finding.proposal().reference().finding().into_uuid())
        .bind(encode_finding_status(ReviewFindingStatus::Open))
        .fetch_one(&mut *connection)
        .await?;
        if !head_is_complete {
            return Err(corruption(
                "review_finding_event_head",
                String::from("event head does not name the exact latest event"),
            ));
        }
        Ok(Some(finding))
    }

    /// Idempotently inserts one canonical pre-effect external-link reservation.
    pub async fn reserve_external_link(
        &self,
        requested: ReviewExternalLink,
    ) -> Result<ReserveExternalLinkOutcome, ReviewWorkflowStoreError> {
        if requested.attachment().is_some()
            || !requested.observations().is_empty()
            || !requested.claims().is_empty()
        {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::ExternalLinkNotPending,
            ));
        }
        let association = encode_link_association(requested.association());
        let mut transaction = self.pool.begin().await?;
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
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 1 {
            commit_mutation(transaction).await?;
            return Ok(ReserveExternalLinkOutcome::Inserted(requested));
        }
        transaction.commit().await?;
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
        current
            .clone()
            .attach(attachment.clone())
            .map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(
                    ReviewWorkflowTransitionError::ExternalLink(error),
                )
            })?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
            .bind(link.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let current = Self::load_external_link_on_connection(&mut transaction, link)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("locked reservation disappeared"),
                )
            })?;
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
                        attachment.pass(),
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
        let transition_finding = posted_event
            .as_ref()
            .map(|event| event.finding().finding())
            .or_else(|| match next.association() {
                ReviewExternalLinkAssociation::Finding(reference) => Some(reference.finding()),
                ReviewExternalLinkAssociation::Run(_)
                | ReviewExternalLinkAssociation::Target(_) => None,
            });
        if let Some(finding) = transition_finding {
            sqlx::query(crate::lock_inventory::REVIEW_FINDINGS_TRANSITION)
                .bind(vec![finding.into_uuid()])
                .fetch_all(&mut *transaction)
                .await?;
        }
        if posted_event.is_none()
            && let ReviewExternalLinkAssociation::Finding(reference) = next.association()
        {
            let current_finding = self
                .load_finding_projection_on_connection(&mut transaction, reference.finding())
                .await?
                .ok_or_else(|| {
                    corruption(
                        "review_external_link_attachment",
                        String::from("associated finding is missing"),
                    )
                })?;
            if matches!(
                current_finding.events().last().map(ReviewFindingEvent::kind),
                Some(ReviewFindingEventKind::BlockedWithReason {
                    link: Some(pending),
                    ..
                }) if pending.link() == link
            ) {
                return Err(ReviewWorkflowStoreError::IncompletePublicationReconciliation);
            }
        }
        if let Some(event) = posted_event.as_ref() {
            let current_finding = self
                .load_finding_projection_on_connection(&mut transaction, event.finding().finding())
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
        if posted_event.is_none()
            && sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                       FROM review_finding_event
                      WHERE external_link_id = $1
                        AND event_kind = 'blocked_with_reason'
                 )",
            )
            .bind(link.into_uuid())
            .fetch_one(&mut *transaction)
            .await?
        {
            return Err(ReviewWorkflowStoreError::IncompletePublicationReconciliation);
        }
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
        sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
            .bind(link.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let current = Self::load_external_link_on_connection(&mut transaction, link)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("locked reservation disappeared"),
                )
            })?;
        if let Some(latest) = current
            .observations()
            .last()
            .filter(|latest| latest.state() == observation.state())
        {
            let canonical_pass =
                load_pass_on_connection(&mut transaction, observation.pass().pass())
                    .await?
                    .ok_or_else(|| {
                        corruption(
                            "review_external_link_observation",
                            String::from("observing pass row is missing"),
                        )
                    })?
                    .evidence();
            if !same_pass_execution(&canonical_pass, observation.pass_evidence()) {
                return Err(corruption(
                    "review_external_link_observation",
                    String::from("observing pass differs from canonical execution facts"),
                ));
            }
            let no_change_pass = canonical_pass
                .project_result(ReviewPassResult::ExternalLinkNoChange(
                    ReviewExternalLinkNoChangeResult::new(
                        link,
                        latest.ordinal(),
                        observation.state(),
                    ),
                ))
                .ok_or_else(|| {
                    corruption(
                        "review_external_link_observation",
                        String::from("unchanged report pass cannot bind the no-change result"),
                    )
                })?;
            let next = current
                .confirm_unchanged(no_change_pass.clone(), observation.run_evidence())
                .map_err(|error| {
                    ReviewWorkflowStoreError::InvalidTransition(
                        ReviewWorkflowTransitionError::ExternalLink(error),
                    )
                })?;
            bind_pass_result(&mut transaction, &no_change_pass).await?;
            commit_mutation(transaction).await?;
            return Ok(Some(next));
        }
        let next = current.observe(observation.clone()).map_err(|error| {
            ReviewWorkflowStoreError::InvalidTransition(
                ReviewWorkflowTransitionError::ExternalLink(error),
            )
        })?;
        bind_pass_result(&mut transaction, observation.pass_evidence()).await?;
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

    /// Binds one blocked publication pass to its exact pending reservation.
    pub async fn block_external_link_publication(
        &self,
        link: ReviewExternalLinkId,
        pass: ReviewPassEvidence,
        run: ReviewRunEvidence,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let Some(_) = self.load_external_link(link).await? else {
            return Ok(None);
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query(crate::lock_inventory::REVIEW_EXTERNAL_LINK_TRANSITION)
            .bind(link.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let current = Self::load_external_link_on_connection(&mut transaction, link)
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("locked reservation disappeared"),
                )
            })?;
        let blocked = current
            .clone()
            .block_publication(pass.clone(), run)
            .map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(
                    ReviewWorkflowTransitionError::ExternalLink(error),
                )
            })?;
        let blocked_event = match pass_state_result(pass.state()) {
            Some(ReviewPassResult::FindingEvent(result)) => {
                let ReviewFindingEventResultKind::BlockedWithReason {
                    reason,
                    link: Some(result_link),
                } = result.kind()
                else {
                    return Err(corruption(
                        "review_external_link",
                        String::from("publication finding result is not a linked block"),
                    ));
                };
                let pending =
                    ReviewFindingPendingExternalLinkRef::try_new(result.finding(), &current)
                        .map_err(|error| {
                            corruption(
                                "review_external_link",
                                format!("blocked result link is invalid: {:?}", error.failure()),
                            )
                        })?;
                if pending.link() != *result_link {
                    return Err(corruption(
                        "review_external_link",
                        String::from("blocked result names another reservation"),
                    ));
                }
                Some(ReviewFindingEvent::new(
                    result.finding(),
                    result.ordinal(),
                    pass.reference(),
                    pass.clone(),
                    run,
                    ReviewFindingEventKind::BlockedWithReason {
                        reason: reason.clone(),
                        link: Some(Box::new(pending)),
                    },
                ))
            }
            _ => None,
        };
        if let Some(event) = blocked_event.as_ref() {
            let finding = vec![event.finding().finding().into_uuid()];
            sqlx::query(crate::lock_inventory::REVIEW_FINDINGS_TRANSITION)
                .bind(&finding)
                .fetch_all(&mut *transaction)
                .await?;
            let current_finding = self
                .load_finding_projection_on_connection(&mut transaction, event.finding().finding())
                .await?
                .ok_or_else(|| {
                    corruption(
                        "review_external_link",
                        String::from("blocked result finding is missing"),
                    )
                })?;
            current_finding.apply(event.clone()).map_err(|error| {
                ReviewWorkflowStoreError::InvalidTransition(ReviewWorkflowTransitionError::Finding(
                    error,
                ))
            })?;
        }
        if sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                   FROM review_external_link_attachment
                  WHERE external_link_id = $1
             )",
        )
        .bind(link.into_uuid())
        .fetch_one(&mut *transaction)
        .await?
        {
            return Err(ReviewWorkflowStoreError::IncompletePublicationReconciliation);
        }
        match blocked_event.as_ref() {
            Some(event) => insert_finding_event(&mut transaction, event).await?,
            None => bind_pass_result(&mut transaction, &pass).await?,
        }
        commit_mutation(transaction).await?;
        Ok(Some(blocked))
    }

    /// Loads and validates a reservation, optional attachment, and observations.
    pub async fn load_external_link(
        &self,
        link: ReviewExternalLinkId,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let link = Self::load_external_link_on_connection(&mut transaction, link).await?;
        transaction.commit().await?;
        Ok(link)
    }

    pub(crate) async fn load_external_link_on_connection(
        connection: &mut PgConnection,
        link: ReviewExternalLinkId,
    ) -> Result<Option<ReviewExternalLink>, ReviewWorkflowStoreError> {
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
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
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
        let target = load_target_on_connection(connection, target_id(row.try_get("target_id")?))
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("referenced target row is missing"),
                )
            })?;
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
                    pass.result_referenced_finding_target_id
                        AS pass_result_referenced_finding_target_id,
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
                    pass_run.state_kind AS pass_run_state_kind,
                    pass_run.state_pass_id AS pass_run_state_pass_id,
                    attachment.external_object_key,
                    object_identity.logical_target_id
                        AS canonical_object_target_id,
                    object_identity.object_identity_count
               FROM review_external_link_attachment AS attachment
               LEFT JOIN review_pass AS pass
                 ON pass.pass_id = attachment.pass_id
                AND pass.run_id = attachment.pass_run_id
                AND pass.target_id = attachment.target_id
               LEFT JOIN review_run AS pass_run
                 ON pass_run.run_id = pass.run_id
                AND pass_run.target_id = pass.target_id
               LEFT JOIN LATERAL (
                    SELECT
                        count(*) AS object_identity_count,
                        (array_agg(
                            identity.logical_target_id
                            ORDER BY identity.logical_target_id
                        ))[1] AS logical_target_id
                      FROM review_external_object_identity AS identity
                     WHERE identity.identity_digest = md5(
                               attachment.provider_key
                                   || chr(31)
                                   || attachment.object_kind
                                   || chr(31)
                                   || attachment.external_object_key
                           )
                       AND identity.provider_key = attachment.provider_key
                       AND identity.object_kind = attachment.object_kind
                       AND identity.external_object_key =
                           attachment.external_object_key
               ) AS object_identity ON TRUE
              WHERE attachment.external_link_id = $1",
        )
        .bind(link.into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
        let attachment = match attachment {
            Some(row) => {
                if row.try_get::<i64, _>("object_identity_count")? != 1 {
                    return Err(corruption(
                        "review_external_link_attachment",
                        String::from(
                            "external object identity must have exactly one canonical row",
                        ),
                    ));
                }
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
                let logical_target_id = target_id(
                    row.try_get::<Option<Uuid>, _>("canonical_object_target_id")?
                        .ok_or_else(|| {
                            corruption(
                                "review_external_link_attachment",
                                String::from("external object identity row is missing"),
                            )
                        })?,
                );
                let logical_target = load_target_on_connection(connection, logical_target_id)
                    .await?
                    .ok_or_else(|| {
                        corruption(
                            "review_external_link_attachment",
                            String::from("external object identity target row is missing"),
                        )
                    })?;
                authenticate_external_object_target(&target, &logical_target)?;
                let pass_id = pass_id(row.try_get("pass_id")?);
                let pass = load_pass_on_connection(connection, pass_id)
                    .await?
                    .ok_or_else(|| {
                        corruption(
                            "review_external_link_attachment",
                            String::from("attaching pass row is missing"),
                        )
                    })?
                    .evidence();
                Some(decode_external_link_attachment(&row, pass)?)
            }
            None => None,
        };
        let observation_rows = sqlx::query(
            "SELECT observation.external_link_id,
                    observation.observation_ordinal,
                    observation.pass_run_id,
                    observation.pass_run_id AS run_id,
                    observation.pass_id,
                    observation.target_id, observation.object_state,
                    pass.pass_id AS canonical_observation_pass_id,
                    pass.pass_kind, pass.state_kind,
                    pass.session_id AS pass_session_id,
                    pass.accepted_input_id, pass.origin_turn_id,
                    pass.turn_id, pass.output_frontier_id,
                    pass.result_kind, pass.result_finding_id,
                    pass.result_finding_run_id,
                    pass.result_finding_pass_id,
                    pass.result_event_ordinal, pass.result_event_kind,
                    pass.result_reason,
                    pass.result_referenced_finding_id,
                    pass.result_referenced_finding_run_id,
                    pass.result_referenced_finding_target_id,
                    pass.result_referenced_finding_pass_id,
                    pass.result_referenced_finding_status,
                    pass.result_external_link_id,
                    pass.result_external_object_key,
                    pass.result_observation_state,
                    pass_run.run_id AS canonical_run_id,
                    pass_run.target_id AS canonical_run_target_id,
                    pass_run.workflow_kind AS run_workflow_kind,
                    pass_run.policy_version AS pass_policy_version,
                    pass_run.minimum_judge_confidence
                        AS pass_minimum_judge_confidence,
                    pass_run.minimum_publication_confidence
                        AS pass_minimum_publication_confidence,
                    pass_run.state_kind AS run_state_kind,
                    pass_run.state_pass_id AS run_state_pass_id,
                    canonical_target.target_id AS canonical_target_id,
                    canonical_input.session_id
                        AS accepted_input_session_id,
                    canonical_input.origin_turn_id
                        AS accepted_input_origin_turn_id,
                    canonical_origin_turn.turn_id
                        AS canonical_origin_turn_id,
                    canonical_turn.turn_id AS evidence_turn_id,
                    canonical_turn.session_id AS turn_session_id,
                    canonical_turn.origin_accepted_input_id
                        AS turn_accepted_input_id,
                    canonical_turn.state_kind AS turn_state_kind,
                    canonical_turn.terminal_disposition_kind
                        AS turn_terminal_disposition_kind,
                    canonical_turn.terminal_frontier_id
                        AS turn_terminal_frontier_id,
                    produced_findings.produced_finding_count
               FROM review_external_link_observation AS observation
               LEFT JOIN review_pass AS pass
                 ON pass.pass_id = observation.pass_id
                AND pass.run_id = observation.pass_run_id
                AND pass.target_id = observation.target_id
               LEFT JOIN review_run AS pass_run
                 ON pass_run.run_id = pass.run_id
                AND pass_run.target_id = pass.target_id
               LEFT JOIN review_target AS canonical_target
                 ON canonical_target.target_id = pass.target_id
               LEFT JOIN accepted_input AS canonical_input
                 ON canonical_input.accepted_input_id =
                    pass.accepted_input_id
               LEFT JOIN turn_lifecycle AS canonical_origin_turn
                 ON canonical_origin_turn.turn_id = pass.origin_turn_id
                AND canonical_origin_turn.session_id = pass.session_id
                AND canonical_origin_turn.origin_accepted_input_id =
                    pass.accepted_input_id
               LEFT JOIN turn_lifecycle AS canonical_turn
                 ON canonical_turn.turn_id = pass.turn_id
               LEFT JOIN LATERAL (
                   SELECT count(*) AS produced_finding_count
                     FROM review_finding AS produced
                    WHERE produced.producing_pass_id = pass.pass_id
                      AND produced.run_id = pass.run_id
                      AND produced.target_id = pass.target_id
               ) AS produced_findings ON true
              WHERE observation.external_link_id = $1
              ORDER BY observation.observation_ordinal",
        )
        .bind(link.into_uuid())
        .fetch_all(&mut *connection)
        .await?;
        let mut observations = Vec::with_capacity(observation_rows.len());
        for row in observation_rows {
            require_joined_reference(
                &row,
                "canonical_observation_pass_id",
                "review_external_link_observation",
                "observing pass row is missing",
            )?;
            require_joined_reference(
                &row,
                "canonical_run_id",
                "review_external_link_observation",
                "observing run row is missing",
            )?;
            require_joined_reference(
                &row,
                "canonical_target_id",
                "review_external_link_observation",
                "observing target row is missing",
            )?;
            if row.try_get::<i64, _>("produced_finding_count")? != 0 {
                return Err(corruption(
                    "review_external_link_observation",
                    String::from("observing pass unexpectedly produced findings"),
                ));
            }
            let turn_evidence = decode_pass_turn_evidence(&row)?;
            let pass = reconstitute_pass(&row, turn_evidence, Vec::new(), None)?;
            require_joined_reference(
                &row,
                "canonical_origin_turn_id",
                "review_external_link_observation",
                "observing pass origin turn row is missing",
            )?;
            let policy = decode_review_policy(
                &row,
                "pass_policy_version",
                "pass_minimum_judge_confidence",
                "pass_minimum_publication_confidence",
                "review_external_link_observation",
            )?;
            let pass = ReviewPassEvidence::from_pass(&pass, policy);
            ReviewRun::try_reconstitute(ReviewRunReconstitutionInput::new(
                pass.reference().run(),
                decode_workflow_kind(&row.try_get::<String, _>("run_workflow_kind")?)?,
                policy,
                decode_run_state(
                    pass.reference().run(),
                    &row.try_get::<String, _>("run_state_kind")?,
                    row.try_get("run_state_pass_id")?,
                )?,
                Some(pass.clone()),
            ))
            .map_err(|error| {
                corruption(
                    "review_external_link_observation",
                    format!(
                        "observing run contradicts its pass projection: {:?}",
                        error.failure()
                    ),
                )
            })?;
            observations.push(decode_external_link_observation(&row, pass)?);
        }
        let claims = load_external_link_claims_on_connection(connection, link).await?;
        let link = ReviewExternalLink::try_reconstitute(
            id,
            association,
            provider,
            kind,
            attachment,
            observations,
            claims,
            &target,
        )
        .map_err(|error| corruption("review_external_link", format!("{error:?}")))?;
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

/// Opens one read-only `REPEATABLE READ` transaction.
///
/// Every statement issued on the returned transaction observes the same
/// database snapshot, which is how a multi-statement read stays coherent
/// without excluding writers.
pub(crate) async fn begin_repeatable_read(
    pool: &PgPool,
) -> Result<Transaction<'_, Postgres>, ReviewWorkflowStoreError> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

async fn load_target_on_connection(
    connection: &mut PgConnection,
    target: ReviewTargetId,
) -> Result<Option<ReviewTarget>, ReviewWorkflowStoreError> {
    let rows = sqlx::query(
        "WITH RECURSIVE ancestry AS (
             SELECT target.target_id, target.provider_key,
                    target.repository_key, target.subject_kind,
                    target.change_request_number, target.head_revision,
                    target.base_revision, target.stack_parent_target_id,
                    ARRAY[target.target_id] AS path, 0 AS depth
               FROM review_target AS target
              WHERE target.target_id = $1
             UNION ALL
             SELECT parent.target_id, parent.provider_key,
                    parent.repository_key, parent.subject_kind,
                    parent.change_request_number, parent.head_revision,
                    parent.base_revision, parent.stack_parent_target_id,
                    ancestry.path || parent.target_id,
                    ancestry.depth + 1
               FROM ancestry
               JOIN review_target AS parent
                 ON parent.target_id = ancestry.stack_parent_target_id
              WHERE NOT parent.target_id = ANY(ancestry.path)
         )
         SELECT target_id, provider_key, repository_key, subject_kind,
                change_request_number, head_revision, base_revision,
                stack_parent_target_id
           FROM ancestry
          ORDER BY depth DESC",
    )
    .bind(target.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows[0]
        .try_get::<Option<Uuid>, _>("stack_parent_target_id")?
        .is_some()
    {
        return Err(corruption(
            "review_target",
            String::from("stack ancestry is cyclic or has a missing parent"),
        ));
    }
    let mut reconstructed = None;
    for row in rows {
        reconstructed = Some(decode_target_row(&row, reconstructed.as_ref())?);
    }
    Ok(reconstructed)
}

#[derive(Clone, Debug)]
pub(crate) struct LoadedReviewPass {
    pub(crate) pass: ReviewPass,
    policy: ReviewPolicy,
    turn_evidence: Option<ReviewPassTurnEvidence>,
    run: ReviewRunEvidence,
}

impl LoadedReviewPass {
    fn evidence(&self) -> ReviewPassEvidence {
        ReviewPassEvidence::from_pass(&self.pass, self.policy)
    }

    const fn run_evidence(&self) -> ReviewRunEvidence {
        self.run
    }
}

pub(crate) async fn load_pass_on_connection(
    connection: &mut PgConnection,
    pass: ReviewPassId,
) -> Result<Option<LoadedReviewPass>, ReviewWorkflowStoreError> {
    let row = sqlx::query(
        "SELECT workflow_pass.pass_id, workflow_pass.run_id,
                workflow_pass.target_id, workflow_pass.pass_kind,
                canonical_run.workflow_kind AS run_workflow_kind,
                canonical_run.policy_version AS run_policy_version,
                canonical_run.minimum_judge_confidence
                    AS run_minimum_judge_confidence,
                canonical_run.minimum_publication_confidence
                    AS run_minimum_publication_confidence,
                canonical_run.state_kind AS run_state_kind,
                canonical_run.state_pass_id AS run_state_pass_id,
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
                workflow_pass.result_referenced_finding_target_id,
                workflow_pass.result_referenced_finding_pass_id,
                workflow_pass.result_referenced_finding_status,
                workflow_pass.result_external_link_id,
                workflow_pass.result_external_object_key,
                workflow_pass.result_observation_state,
                canonical_input.session_id AS accepted_input_session_id,
                canonical_input.origin_turn_id AS accepted_input_origin_turn_id,
                canonical_origin_turn.turn_id AS canonical_origin_turn_id,
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
                canonical_run.target_id AS canonical_run_target_id,
                canonical_target.target_id AS canonical_target_id
           FROM review_pass AS workflow_pass
           LEFT JOIN review_run AS canonical_run
             ON canonical_run.run_id = workflow_pass.run_id
            AND canonical_run.target_id = workflow_pass.target_id
           LEFT JOIN review_target AS canonical_target
             ON canonical_target.target_id = workflow_pass.target_id
           LEFT JOIN accepted_input AS canonical_input
             ON canonical_input.accepted_input_id =
                workflow_pass.accepted_input_id
           LEFT JOIN turn_lifecycle AS canonical_origin_turn
             ON canonical_origin_turn.turn_id =
                workflow_pass.origin_turn_id
            AND canonical_origin_turn.session_id =
                workflow_pass.session_id
            AND canonical_origin_turn.origin_accepted_input_id =
                workflow_pass.accepted_input_id
           LEFT JOIN turn_lifecycle AS canonical_turn
             ON canonical_turn.turn_id = workflow_pass.turn_id
          WHERE workflow_pass.pass_id = $1",
    )
    .bind(pass.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    require_joined_reference(
        &row,
        "canonical_run_id",
        "review_pass",
        "referenced run row is missing",
    )?;
    require_joined_reference(
        &row,
        "canonical_target_id",
        "review_pass",
        "referenced target row is missing",
    )?;
    let produced_findings = load_produced_findings_on_connection(connection, pass).await?;
    let referenced_producer =
        load_referenced_producer_on_connection(connection, &row, pass).await?;
    let turn_evidence = decode_pass_turn_evidence(&row)?;
    let pass = reconstitute_pass(
        &row,
        turn_evidence,
        produced_findings,
        referenced_producer.as_ref(),
    )?;
    require_joined_reference(
        &row,
        "canonical_origin_turn_id",
        "review_pass",
        "origin turn row is missing",
    )?;
    let policy = decode_review_policy(
        &row,
        "run_policy_version",
        "run_minimum_judge_confidence",
        "run_minimum_publication_confidence",
        "review_pass",
    )?;
    let run_reference = pass.reference().run();
    let workflow = decode_workflow_kind(&row.try_get::<String, _>("run_workflow_kind")?)?;
    let state = decode_run_state(
        run_reference,
        &row.try_get::<String, _>("run_state_kind")?,
        row.try_get("run_state_pass_id")?,
    );
    let run = ReviewRun::try_reconstitute(ReviewRunReconstitutionInput::new(
        run_reference,
        workflow,
        policy,
        state?,
        Some(ReviewPassEvidence::from_pass(&pass, policy)),
    ))
    .map_err(|error| {
        corruption(
            "review_run",
            format!(
                "canonical run contradicts its pass projection: {:?}",
                error.failure()
            ),
        )
    })?
    .evidence();
    Ok(Some(LoadedReviewPass {
        pass,
        policy,
        turn_evidence,
        run,
    }))
}

async fn load_referenced_producer_on_connection(
    connection: &mut PgConnection,
    row: &PgRow,
    loading_pass: ReviewPassId,
) -> Result<Option<LoadedReviewPass>, ReviewWorkflowStoreError> {
    let finding: Option<Uuid> = row.try_get("result_referenced_finding_id")?;
    let run: Option<Uuid> = row.try_get("result_referenced_finding_run_id")?;
    let target: Option<Uuid> = row.try_get("result_referenced_finding_target_id")?;
    let pass: Option<Uuid> = row.try_get("result_referenced_finding_pass_id")?;
    let (finding, run, target, pass) = match (finding, run, target, pass) {
        (None, None, None, None) => return Ok(None),
        (Some(finding), Some(run), Some(target), Some(pass)) => (finding, run, target, pass),
        _ => {
            return Err(corruption(
                "review_pass",
                String::from("referenced result ancestry is incomplete"),
            ));
        }
    };
    if pass == loading_pass.into_uuid() {
        return Err(corruption(
            "review_pass",
            String::from("referenced result names its own pass as producer"),
        ));
    }
    let canonical = sqlx::query(
        "SELECT finding.finding_id AS canonical_finding_id,
                producer.pass_id AS canonical_pass_id,
                producer.pass_kind, producer.state_kind,
                producer.result_kind
           FROM review_finding AS finding
           LEFT JOIN review_pass AS producer
             ON producer.pass_id = finding.producing_pass_id
            AND producer.run_id = finding.run_id
            AND producer.target_id = finding.target_id
          WHERE finding.finding_id = $1
            AND finding.run_id = $2
            AND finding.target_id = $3
            AND finding.producing_pass_id = $4",
    )
    .bind(finding)
    .bind(run)
    .bind(target)
    .bind(pass)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result finding ancestry is missing"),
        )
    })?;
    require_joined_reference(
        &canonical,
        "canonical_pass_id",
        "review_pass",
        "referenced result producer is missing",
    )?;
    if canonical.try_get::<String, _>("pass_kind")? != "read_only_review"
        || canonical.try_get::<String, _>("state_kind")? != "succeeded"
        || canonical
            .try_get::<Option<String>, _>("result_kind")?
            .as_deref()
            != Some("produced_findings")
    {
        return Err(corruption(
            "review_pass",
            String::from("referenced result producer is not sealed read-only success"),
        ));
    }
    let loaded = Box::pin(load_pass_on_connection(connection, pass_id(pass)))
        .await?
        .ok_or_else(|| {
            corruption(
                "review_pass",
                String::from("referenced result producer disappeared"),
            )
        })?;
    let expected = ReviewPassRef::new(
        ReviewRunRef::new(target_id(target), run_id(run)),
        pass_id(pass),
    );
    if loaded.pass.reference() != expected {
        return Err(corruption(
            "review_pass",
            String::from("referenced result producer ancestry is cross-wired"),
        ));
    }
    Ok(Some(loaded))
}

async fn load_external_link_claims_on_connection(
    connection: &mut PgConnection,
    link: ReviewExternalLinkId,
) -> Result<Vec<ReviewExternalLinkClaim>, ReviewWorkflowStoreError> {
    let rows = sqlx::query(
        "SELECT pass_id
           FROM review_pass
          WHERE result_external_link_id = $1
            AND (
                result_kind IN (
                    'external_link_no_change',
                    'external_link_publication_blocked'
                )
                OR (
                    result_kind = 'finding_event'
                    AND result_event_kind = 'blocked_with_reason'
                )
            )
          ORDER BY pass_id",
    )
    .bind(link.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let mut claims = Vec::with_capacity(rows.len());
    for row in rows {
        let pass = load_pass_on_connection(connection, pass_id(row.try_get("pass_id")?))
            .await?
            .ok_or_else(|| {
                corruption(
                    "review_external_link",
                    String::from("claiming pass row is missing"),
                )
            })?;
        claims.push(ReviewExternalLinkClaim::new(
            pass.evidence(),
            pass.run_evidence(),
        ));
    }
    Ok(claims)
}

async fn load_produced_findings_on_connection(
    connection: &mut PgConnection,
    pass: ReviewPassId,
) -> Result<Vec<ReviewFindingRef>, ReviewWorkflowStoreError> {
    let pass_row = sqlx::query(
        "SELECT pass.result_kind, seal.finding_count
           FROM review_pass AS pass
           LEFT JOIN review_pass_finding_inventory_seal AS seal
             ON seal.pass_id = pass.pass_id
          WHERE pass.pass_id = $1",
    )
    .bind(pass.into_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("finding inventory pass row is missing"),
        )
    })?;
    let result_kind: Option<String> = pass_row.try_get("result_kind")?;
    let sealed_count: Option<i32> = pass_row.try_get("finding_count")?;

    let member_rows = sqlx::query(
        "SELECT member.result_ordinal, member.target_id, member.finding_run_id,
                member.finding_pass_id, member.finding_id,
                finding.finding_id AS canonical_finding_id
           FROM review_pass_produced_finding AS member
           LEFT JOIN review_finding AS finding
             ON finding.finding_id = member.finding_id
            AND finding.run_id = member.finding_run_id
            AND finding.target_id = member.target_id
            AND finding.producing_pass_id = member.finding_pass_id
          WHERE member.pass_id = $1
          ORDER BY member.result_ordinal",
    )
    .bind(pass.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let mut members = Vec::with_capacity(member_rows.len());
    for (index, row) in member_rows.into_iter().enumerate() {
        let ordinal = positive_u32(
            row.try_get("result_ordinal")?,
            "review_pass_produced_finding",
        )?;
        let expected = u32::try_from(index + 1).map_err(|_| {
            corruption(
                "review_pass_produced_finding",
                String::from("finding inventory ordinal overflow"),
            )
        })?;
        if ordinal != expected {
            return Err(corruption(
                "review_pass_produced_finding",
                String::from("finding inventory ordinals are not contiguous"),
            ));
        }
        require_joined_reference(
            &row,
            "canonical_finding_id",
            "review_pass_produced_finding",
            "inventory member has no canonical finding",
        )?;
        members.push(ReviewFindingRef::new(
            ReviewPassRef::new(
                ReviewRunRef::new(
                    target_id(row.try_get("target_id")?),
                    run_id(row.try_get("finding_run_id")?),
                ),
                pass_id(row.try_get("finding_pass_id")?),
            ),
            finding_id(row.try_get("finding_id")?),
        ));
    }
    let member_rows = members;
    let canonical = sqlx::query(
        "SELECT target_id, run_id, producing_pass_id, finding_id
           FROM review_finding
          WHERE producing_pass_id = $1
          ORDER BY target_id, run_id, producing_pass_id, finding_id",
    )
    .bind(pass.into_uuid())
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .map(|row| {
        Ok(ReviewFindingRef::new(
            ReviewPassRef::new(
                ReviewRunRef::new(
                    target_id(row.try_get("target_id")?),
                    run_id(row.try_get("run_id")?),
                ),
                pass_id(row.try_get("producing_pass_id")?),
            ),
            finding_id(row.try_get("finding_id")?),
        ))
    })
    .collect::<Result<Vec<_>, ReviewWorkflowStoreError>>()?;
    let count = i32::try_from(member_rows.len()).map_err(|_| {
        corruption(
            "review_pass_produced_finding",
            String::from("finding inventory count overflow"),
        )
    })?;
    if member_rows != canonical
        || match result_kind.as_deref() {
            Some("produced_findings") => sealed_count != Some(count),
            _ => sealed_count.is_some() || count != 0,
        }
    {
        return Err(corruption(
            "review_pass_produced_finding",
            String::from("inventory differs from canonical sealed findings"),
        ));
    }
    Ok(member_rows)
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
             is_real_confidence, severity_label_confidence, category,
             recommended_fix)
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
             $15
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
    .bind(i32::from(content.is_real_confidence().basis_points()))
    .bind(i32::from(
        content.severity_label_confidence().basis_points(),
    ))
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
             referenced_finding_id, referenced_finding_run_id,
             referenced_finding_target_id, referenced_finding_pass_id,
             referenced_finding_status,
             external_link_id, external_link_association_kind)
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
             $13, $14, $15
         )",
    )
    .bind(finding.finding().into_uuid())
    .bind(i64::from(event.ordinal().get()))
    .bind(finding.run().run().into_uuid())
    .bind(finding.target().into_uuid())
    .bind(event.pass().pass().into_uuid())
    .bind(event.pass().run().run().into_uuid())
    .bind(encoded.kind)
    .bind(encoded.reason)
    .bind(
        encoded
            .referenced
            .map(|evidence| evidence.reference().finding().into_uuid()),
    )
    .bind(
        encoded
            .referenced
            .map(|evidence| evidence.reference().run().run().into_uuid()),
    )
    .bind(
        encoded
            .referenced
            .map(|evidence| evidence.reference().target().into_uuid()),
    )
    .bind(
        encoded
            .referenced
            .map(|evidence| evidence.reference().pass().pass().into_uuid()),
    )
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
    authenticate_pass_result_binding(transaction, pass).await?;
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
                result_referenced_finding_target_id = $15,
                result_referenced_finding_pass_id = $16,
                result_referenced_finding_status = $17,
                result_external_link_id = $18,
                result_external_object_key = $19,
                result_observation_state = $20
          WHERE pass_id = $1
            AND run_id = $2
            AND target_id = $3
            AND state_kind = $4
            AND turn_id IS NOT DISTINCT FROM $5
            AND output_frontier_id IS NOT DISTINCT FROM $21
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
                    result_referenced_finding_target_id,
                    result_referenced_finding_pass_id,
                    result_referenced_finding_status,
                    result_external_link_id,
                    result_external_object_key,
                    result_observation_state
                ) IS NOT DISTINCT FROM (
                    $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                    $16, $17, $18, $19, $20
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
    .bind(referenced.map(|finding| finding.target().into_uuid()))
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

async fn authenticate_pass_result_binding(
    transaction: &mut Transaction<'_, Postgres>,
    proposed: &ReviewPassEvidence,
) -> Result<(), ReviewWorkflowStoreError> {
    let run_row = sqlx::query(crate::lock_inventory::REVIEW_RUN_TRANSITION)
        .bind(proposed.reference().run().run().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| {
            corruption(
                "review_pass",
                String::from("effect pass run row is missing"),
            )
        })?;
    let pass_row = sqlx::query(crate::lock_inventory::REVIEW_PASS_TRANSITION)
        .bind(proposed.reference().pass().into_uuid())
        .bind(pass_state_turn(proposed.state()).map(TurnId::into_uuid))
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| corruption("review_pass", String::from("effect pass row is missing")))?;
    if pass_row
        .try_get::<Option<String>, _>("result_kind")?
        .is_none()
    {
        return authenticate_unbound_pass_result(transaction, &pass_row, &run_row, proposed).await;
    }
    let loaded = load_pass_on_connection(transaction, proposed.reference().pass())
        .await?
        .ok_or_else(|| corruption("review_pass", String::from("effect pass row is missing")))?;
    if loaded.pass.reference() != proposed.reference()
        || loaded.pass.kind() != proposed.kind()
        || loaded.policy != proposed.policy()
    {
        return Err(corruption(
            "review_pass",
            String::from("effect pass differs from canonical execution facts"),
        ));
    }
    let result = pass_state_result(proposed.state())
        .cloned()
        .ok_or_else(|| {
            corruption(
                "review_pass",
                String::from("effect pass omitted its exact typed result"),
            )
        })?;
    if loaded.pass.state() == proposed.state() {
        return Ok(());
    }
    if pass_state_result(loaded.pass.state()).is_none()
        && !matches!(loaded.pass.state(), ReviewPassState::Running { .. })
    {
        let bound = loaded.pass.clone().bind_result(result).map_err(|error| {
            corruption(
                "review_pass",
                format!(
                    "canonical terminal pass rejected its effect result: {:?}",
                    error.failure()
                ),
            )
        })?;
        if ReviewPassEvidence::from_pass(&bound, loaded.policy) != *proposed {
            return Err(corruption(
                "review_pass",
                String::from("effect pass differs from canonical terminal facts"),
            ));
        }
        return Ok(());
    }
    if !matches!(loaded.pass.state(), ReviewPassState::Running { .. }) {
        return Err(corruption(
            "review_pass",
            String::from("effect pass result conflicts with its canonical outcome"),
        ));
    }
    let transitioned_pass = loaded
        .pass
        .clone()
        .transition(proposed.state().clone(), loaded.turn_evidence)
        .map_err(|error| {
            corruption(
                "review_pass",
                format!(
                    "canonical running pass rejected its effect outcome: {:?}",
                    error.failure()
                ),
            )
        })?;
    if ReviewPassEvidence::from_pass(&transitioned_pass, loaded.policy) != *proposed {
        return Err(corruption(
            "review_pass",
            String::from("effect pass differs from canonical transitioned facts"),
        ));
    }
    let (current_run, _) = decode_run_for_transition(&run_row, Some(&loaded))?;
    let next_run = match proposed.state() {
        ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
            concluding_pass: proposed.reference(),
        },
        ReviewPassState::Failed { .. } => ReviewRunState::Failed {
            failed_pass: proposed.reference(),
        },
        ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
            blocking_pass: proposed.reference(),
        },
        ReviewPassState::Cancelled { .. } => ReviewRunState::Cancelled {
            last_pass: Some(proposed.reference()),
        },
        ReviewPassState::Queued | ReviewPassState::Running { .. } => {
            return Err(corruption(
                "review_pass",
                String::from("effect result belongs to a nonterminal pass"),
            ));
        }
    };
    let transitioned_run = current_run
        .transition(next_run, Some(proposed.clone()))
        .map_err(|error| {
            corruption(
                "review_run",
                format!(
                    "canonical run rejected its effect pass outcome: {:?}",
                    error.failure()
                ),
            )
        })?;
    let lifecycle = encode_pass_state(transitioned_pass.state());
    sqlx::query(
        "UPDATE review_pass
            SET state_kind = $2,
                turn_id = $3,
                output_frontier_id = $4
          WHERE pass_id = $1",
    )
    .bind(proposed.reference().pass().into_uuid())
    .bind(lifecycle.kind)
    .bind(lifecycle.turn.map(TurnId::into_uuid))
    .bind(lifecycle.frontier.map(ContextFrontierId::into_uuid))
    .execute(&mut **transaction)
    .await?;
    let (run_state, run_pass) = encode_run_state(transitioned_run.state());
    sqlx::query(
        "UPDATE review_run
            SET state_kind = $2,
                state_pass_id = $3
          WHERE run_id = $1",
    )
    .bind(proposed.reference().run().run().into_uuid())
    .bind(run_state)
    .bind(run_pass.map(ReviewPassId::into_uuid))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn authenticate_unbound_pass_result(
    transaction: &mut Transaction<'_, Postgres>,
    pass_row: &PgRow,
    run_row: &PgRow,
    proposed: &ReviewPassEvidence,
) -> Result<(), ReviewWorkflowStoreError> {
    let (run_reference, workflow, policy, run_state) = decode_run_facts(run_row)?;
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(pass_row.try_get("target_id")?),
            run_id(pass_row.try_get("run_id")?),
        ),
        pass_id(pass_row.try_get("pass_id")?),
    );
    let kind = decode_pass_kind(&pass_row.try_get::<String, _>("pass_kind")?)?;
    if reference != proposed.reference() || kind != proposed.kind() || policy != proposed.policy() {
        return Err(corruption(
            "review_pass",
            String::from("effect pass differs from canonical execution facts"),
        ));
    }
    let accepted_input = decode_pass_accepted_input_evidence(pass_row)?;
    let mut queued_run = ReviewRun::try_reconstitute(ReviewRunReconstitutionInput::new(
        run_reference,
        workflow,
        policy,
        ReviewRunState::Queued,
        None,
    ))
    .map_err(|error| {
        corruption(
            "review_pass",
            format!(
                "effect pass run cannot support atomic binding: {:?}",
                error.failure()
            ),
        )
    })?;
    let mut current = ReviewPass::try_new(
        reference,
        kind,
        &mut queued_run,
        session_id(pass_row.try_get("pass_session_id")?),
        accepted_input,
    )
    .map_err(|error| {
        corruption(
            "review_pass",
            format!(
                "effect pass cannot support atomic binding: {:?}",
                error.failure()
            ),
        )
    })?;
    let canonical_turn = decode_pass_turn_evidence(pass_row)?.ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("effect pass canonical turn row is missing"),
        )
    })?;
    let proposed_turn = pass_state_turn(proposed.state()).ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("effect pass result is not terminal"),
        )
    })?;
    if canonical_turn.turn() != proposed_turn {
        return Err(corruption(
            "review_pass",
            String::from("effect pass turn differs from canonical execution"),
        ));
    }
    let active_turn = ReviewPassTurnEvidence::new(
        canonical_turn.turn(),
        canonical_turn.session(),
        canonical_turn.accepted_input(),
        ReviewPassTurnOutcome::Active,
        None,
    );
    current = current
        .transition(
            ReviewPassState::Running {
                turn: proposed_turn,
            },
            Some(active_turn),
        )
        .map_err(|error| {
            corruption(
                "review_pass",
                format!(
                    "effect pass cannot replay its running state: {:?}",
                    error.failure()
                ),
            )
        })?;
    let stored_kind: String = pass_row.try_get("state_kind")?;
    if stored_kind != "running" {
        let proposed_state = encode_pass_state(proposed.state());
        let stored_turn: Option<Uuid> = pass_row.try_get("turn_id")?;
        let stored_frontier: Option<Uuid> = pass_row.try_get("output_frontier_id")?;
        if stored_kind != proposed_state.kind
            || stored_turn != proposed_state.turn.map(TurnId::into_uuid)
            || stored_frontier != proposed_state.frontier.map(ContextFrontierId::into_uuid)
        {
            return Err(corruption(
                "review_pass",
                String::from("effect pass differs from canonical terminal facts"),
            ));
        }
    }
    let running = current.clone();
    let transitioned = current
        .transition(proposed.state().clone(), Some(canonical_turn))
        .map_err(|error| {
            corruption(
                "review_pass",
                format!(
                    "canonical pass rejected its atomic effect outcome: {:?}",
                    error.failure()
                ),
            )
        })?;
    if ReviewPassEvidence::from_pass(&transitioned, policy) != *proposed {
        return Err(corruption(
            "review_pass",
            String::from("effect pass differs from canonical terminal facts"),
        ));
    }
    if stored_kind == "running" {
        let loaded = LoadedReviewPass {
            pass: running,
            policy,
            turn_evidence: Some(canonical_turn),
            run: ReviewRunEvidence::new(run_reference, workflow, policy, run_state),
        };
        let (current_run, _) = decode_run_for_transition(run_row, Some(&loaded))?;
        let next_run = match proposed.state() {
            ReviewPassState::Succeeded { .. } => ReviewRunState::Succeeded {
                concluding_pass: proposed.reference(),
            },
            ReviewPassState::Failed { .. } => ReviewRunState::Failed {
                failed_pass: proposed.reference(),
            },
            ReviewPassState::Blocked { .. } => ReviewRunState::Blocked {
                blocking_pass: proposed.reference(),
            },
            ReviewPassState::Cancelled { .. } => ReviewRunState::Cancelled {
                last_pass: Some(proposed.reference()),
            },
            ReviewPassState::Queued | ReviewPassState::Running { .. } => {
                return Err(corruption(
                    "review_pass",
                    String::from("effect result belongs to a nonterminal pass"),
                ));
            }
        };
        let transitioned_run = current_run
            .transition(next_run, Some(proposed.clone()))
            .map_err(|error| {
                corruption(
                    "review_run",
                    format!(
                        "canonical run rejected its effect pass outcome: {:?}",
                        error.failure()
                    ),
                )
            })?;
        let lifecycle = encode_pass_state(transitioned.state());
        sqlx::query(
            "UPDATE review_pass
                SET state_kind = $2,
                    turn_id = $3,
                    output_frontier_id = $4
              WHERE pass_id = $1",
        )
        .bind(proposed.reference().pass().into_uuid())
        .bind(lifecycle.kind)
        .bind(lifecycle.turn.map(TurnId::into_uuid))
        .bind(lifecycle.frontier.map(ContextFrontierId::into_uuid))
        .execute(&mut **transaction)
        .await?;
        let (run_kind, run_pass) = encode_run_state(transitioned_run.state());
        sqlx::query(
            "UPDATE review_run
                SET state_kind = $2,
                    state_pass_id = $3
              WHERE run_id = $1",
        )
        .bind(run_reference.run().into_uuid())
        .bind(run_kind)
        .bind(run_pass.map(ReviewPassId::into_uuid))
        .execute(&mut **transaction)
        .await?;
    } else {
        ReviewRun::try_reconstitute(ReviewRunReconstitutionInput::new(
            run_reference,
            workflow,
            policy,
            run_state,
            Some(proposed.clone()),
        ))
        .map_err(|error| {
            corruption(
                "review_run",
                format!(
                    "canonical run contradicts its effect pass outcome: {:?}",
                    error.failure()
                ),
            )
        })?;
    }
    Ok(())
}

fn decode_target_row(
    row: &PgRow,
    stack_parent: Option<&ReviewTarget>,
) -> Result<ReviewTarget, ReviewWorkflowStoreError> {
    let id = target_id(row.try_get("target_id")?);
    let provider = review_key(row.try_get("provider_key")?, "review_target")?;
    let repository = review_key(row.try_get("repository_key")?, "review_target")?;
    let subject_kind: String = row.try_get("subject_kind")?;
    let change_request_number: Option<Decimal> = row.try_get("change_request_number")?;
    let subject = decode_target_subject(&subject_kind, change_request_number)?;
    let stored_parent = row
        .try_get::<Option<Uuid>, _>("stack_parent_target_id")?
        .map(target_id);
    if stored_parent != stack_parent.map(ReviewTarget::id) {
        return Err(corruption(
            "review_target",
            String::from("stack ancestry is incomplete or out of order"),
        ));
    }
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
    row: &PgRow,
    canonical_pass: Option<&LoadedReviewPass>,
) -> Result<ReviewRun, ReviewWorkflowStoreError> {
    let (_, workflow, _, state) = decode_run_facts(row)?;
    let pass_evidence = projected_current_run_pass_evidence(state, workflow, canonical_pass)?;
    reconstitute_run(row, pass_evidence)
}

fn decode_run_for_transition(
    row: &PgRow,
    canonical_pass: Option<&LoadedReviewPass>,
) -> Result<(ReviewRun, Option<ReviewPassEvidence>), ReviewWorkflowStoreError> {
    let (_, workflow, _, state) = decode_run_facts(row)?;
    let canonical_evidence = canonical_pass.map(LoadedReviewPass::evidence);
    let current_evidence = projected_current_run_pass_evidence(state, workflow, canonical_pass)?;
    Ok((reconstitute_run(row, current_evidence)?, canonical_evidence))
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

fn projected_current_run_pass_evidence(
    state: ReviewRunState,
    workflow: ReviewWorkflowKind,
    canonical: Option<&LoadedReviewPass>,
) -> Result<Option<ReviewPassEvidence>, ReviewWorkflowStoreError> {
    let Some(expected) = encode_run_state(state).1 else {
        return Ok(canonical.map(LoadedReviewPass::evidence));
    };
    let Some(canonical) = canonical else {
        return Err(corruption(
            "review_run",
            String::from("referenced pass row is missing"),
        ));
    };
    if canonical.pass.reference().pass() != expected {
        return Err(corruption(
            "review_run",
            String::from("canonical pass identity mismatch"),
        ));
    }
    if !matches!(state, ReviewRunState::Running { .. }) {
        return Ok(Some(canonical.evidence()));
    }
    let projected_state = ReviewPassState::Running {
        turn: pass_state_turn(canonical.pass.state()).ok_or_else(|| {
            corruption(
                "review_run",
                String::from("canonical pass has no turn for running projection"),
            )
        })?,
    };
    let projected = ReviewPass::try_reconstitute(ReviewPassReconstitutionInput::new(
        canonical.pass.reference(),
        canonical.pass.kind(),
        canonical.pass.reference().run(),
        workflow,
        canonical.pass.session(),
        canonical.pass.accepted_input(),
        ReviewPassAcceptedInputEvidence::new(
            canonical.pass.accepted_input(),
            canonical.pass.session(),
            Some(canonical.pass.origin_turn()),
        ),
        projected_state,
        canonical.turn_evidence,
    ))
    .map_err(|error| {
        corruption(
            "review_run",
            format!(
                "canonical pass cannot support running projection: {:?}",
                error.failure()
            ),
        )
    })?;
    Ok(Some(ReviewPassEvidence::from_pass(
        &projected,
        canonical.policy,
    )))
}

fn reconstitute_pass(
    row: &PgRow,
    turn_evidence: Option<ReviewPassTurnEvidence>,
    produced_findings: Vec<ReviewFindingRef>,
    referenced_producer: Option<&LoadedReviewPass>,
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
        decode_pass_accepted_input_evidence(row)?,
        decode_pass_state(
            &state_kind,
            turn,
            frontier,
            stored_pass_result(row, "target_id", "")?,
            produced_findings,
            referenced_producer,
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

fn decode_pass_accepted_input_evidence(
    row: &PgRow,
) -> Result<ReviewPassAcceptedInputEvidence, ReviewWorkflowStoreError> {
    let stored_origin = row.try_get::<Uuid, _>("origin_turn_id")?;
    let canonical_origin = row.try_get::<Option<Uuid>, _>("accepted_input_origin_turn_id")?;
    if canonical_origin != Some(stored_origin) {
        return Err(corruption(
            "review_pass",
            String::from("accepted input origin turn differs from stored pass"),
        ));
    }
    Ok(ReviewPassAcceptedInputEvidence::new(
        accepted_input_id(row.try_get("accepted_input_id")?),
        session_id(
            row.try_get::<Option<Uuid>, _>("accepted_input_session_id")?
                .ok_or_else(|| {
                    corruption("review_pass", String::from("accepted input row is missing"))
                })?,
        ),
        canonical_origin.map(turn_id),
    ))
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
                decode_turn_outcome("review_pass", &state, disposition.as_deref())?,
                frontier.map(context_frontier_id),
            )))
        }
        _ => Err(corruption(
            "review_pass",
            String::from("torn canonical turn evidence"),
        )),
    }
}

/// Reads one column of a returned row, reporting a decode failure as corruption.
///
/// The fetch keeps `?`: a connection that dropped mid-query is retryable, and
/// `mutation_unavailable` is the honest answer. A column read on a row the
/// query already returned is not — a type or a name the projection and the
/// schema disagree about is a fault no retry repairs, and the handlers these
/// projections replaced answered it with the internal projection diagnostic.
fn projected<'row, T>(
    row: &'row PgRow,
    aggregate: &'static str,
    column: &'static str,
) -> Result<T, ReviewWorkflowStoreError>
where
    T: sqlx::Decode<'row, Postgres> + sqlx::Type<Postgres>,
{
    row.try_get(column)
        .map_err(|error| corruption(aggregate, format!("column {column}: {error}")))
}

/// Decodes one turn outcome, reporting corruption against the reading table.
///
/// The same state/disposition pair is stored on `turn_lifecycle` and copied
/// onto the `review_pass` evidence columns, so the caller names which of the
/// two it read: a corruption report that named the other one would send a
/// reader to a table whose rows are intact.
fn decode_turn_outcome(
    aggregate: &'static str,
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
            aggregate,
            format!("invalid canonical turn outcome {state}/{disposition:?}"),
        )),
    }
}

fn decode_turn_lifecycle_state(
    state: &str,
    disposition: Option<&str>,
) -> Result<ReviewTurnLifecycleState, ReviewWorkflowStoreError> {
    match (state, disposition) {
        ("queued", None) => Ok(ReviewTurnLifecycleState::Queued),
        ("active", None) => Ok(ReviewTurnLifecycleState::Active),
        ("terminal", Some(_)) => Ok(ReviewTurnLifecycleState::Terminal(decode_turn_outcome(
            "turn_lifecycle",
            state,
            disposition,
        )?)),
        _ => Err(corruption(
            "turn_lifecycle",
            format!("invalid turn lifecycle state {state}/{disposition:?}"),
        )),
    }
}

fn decode_finding_proposal(
    row: &PgRow,
    producing_pass: ReviewPassEvidence,
    target: &ReviewTarget,
) -> Result<ReviewFindingProposal, ReviewWorkflowStoreError> {
    let run = ReviewRunRef::new(
        target_id(row.try_get("target_id")?),
        run_id(row.try_get("run_id")?),
    );
    let producing_pass_reference =
        ReviewPassRef::new(run, pass_id(row.try_get("producing_pass_id")?));
    if producing_pass.reference() != producing_pass_reference {
        return Err(corruption(
            "review_finding",
            String::from("producing pass ancestry mismatch"),
        ));
    }
    let producing_run = ReviewRunEvidence::new(
        run,
        decode_workflow_kind(&row.try_get::<String, _>("producing_workflow_kind")?)?,
        producing_pass.policy(),
        decode_run_state(
            run,
            &row.try_get::<String, _>("producing_run_state_kind")?,
            row.try_get("producing_run_state_pass_id")?,
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
        ReviewFindingConfidenceAxes::new(
            decode_review_confidence(row.try_get("is_real_confidence")?, "review_finding")?,
            decode_review_confidence(row.try_get("severity_label_confidence")?, "review_finding")?,
        ),
        review_key(row.try_get("category")?, "review_finding")?,
        row.try_get::<Option<String>, _>("recommended_fix")?
            .map(|value| review_text(value, "review_finding"))
            .transpose()?,
    );
    ReviewFindingProposal::try_new(reference, producing_pass, producing_run, target, content)
        .map_err(|error| corruption("review_finding", format!("{error:?}")))
}

fn decode_finding_event(
    row: &PgRow,
    finding: ReviewFindingRef,
    target_snapshot: &ReviewTarget,
    pass: ReviewPassEvidence,
    canonical_link: Option<&ReviewExternalLink>,
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
    if pass.reference() != pass_reference {
        return Err(corruption(
            "review_finding_event",
            String::from("event pass ancestry mismatch"),
        ));
    }
    let run = ReviewRunEvidence::new(
        pass_reference.run(),
        decode_workflow_kind(&row.try_get::<String, _>("event_workflow_kind")?)?,
        pass.policy(),
        decode_run_state(
            pass_reference.run(),
            &row.try_get::<String, _>("event_run_state_kind")?,
            row.try_get("event_run_state_pass_id")?,
        )?,
    );
    let kind: String = row.try_get("event_kind")?;
    let reason: Option<String> = row.try_get("reason")?;
    let referenced: Option<Uuid> = row.try_get("referenced_finding_id")?;
    let referenced_run: Option<Uuid> = row.try_get("referenced_finding_run_id")?;
    let referenced_target: Option<Uuid> = row.try_get("referenced_finding_target_id")?;
    let referenced_pass: Option<Uuid> = row.try_get("referenced_finding_pass_id")?;
    let referenced_status: Option<String> = row.try_get("referenced_finding_status")?;
    let external_link: Option<Uuid> = row.try_get("external_link_id")?;
    let kind = match (
        kind.as_str(),
        reason,
        referenced,
        referenced_run,
        referenced_target,
        referenced_pass,
        referenced_status,
        external_link,
    ) {
        ("accepted", None, None, None, None, None, None, None) => ReviewFindingEventKind::Accepted,
        ("rejected", Some(reason), None, None, None, None, None, None) => {
            ReviewFindingEventKind::Rejected {
                reason: review_text(reason, "review_finding_event")?,
            }
        }
        (
            "duplicate",
            None,
            Some(referenced),
            Some(referenced_run),
            Some(referenced_target),
            Some(referenced_pass),
            Some(referenced_status),
            None,
        ) => ReviewFindingEventKind::Duplicate {
            canonical: decode_event_referenced_finding(
                &pass,
                "duplicate",
                ReviewFindingRef::new(
                    ReviewPassRef::new(
                        ReviewRunRef::new(target_id(referenced_target), run_id(referenced_run)),
                        pass_id(referenced_pass),
                    ),
                    finding_id(referenced),
                ),
                &referenced_status,
            )?,
        },
        (
            "superseded",
            None,
            Some(referenced),
            Some(referenced_run),
            Some(referenced_target),
            Some(referenced_pass),
            Some(referenced_status),
            None,
        ) => ReviewFindingEventKind::Superseded {
            successor: decode_event_referenced_finding(
                &pass,
                "superseded",
                ReviewFindingRef::new(
                    ReviewPassRef::new(
                        ReviewRunRef::new(target_id(referenced_target), run_id(referenced_run)),
                        pass_id(referenced_pass),
                    ),
                    finding_id(referenced),
                ),
                &referenced_status,
            )?,
        },
        ("stale", None, None, None, None, None, None, None) => ReviewFindingEventKind::Stale,
        ("posted", None, None, None, None, None, None, Some(link)) => {
            let canonical =
                require_finding_event_external_link(canonical_link, external_link_id(link))?;
            ReviewFindingEventKind::Posted {
                link: Box::new(
                    ReviewFindingExternalLinkRef::try_new(finding, canonical).map_err(|error| {
                        corruption(
                            "review_finding_event",
                            format!("invalid canonical posted link: {:?}", error.failure()),
                        )
                    })?,
                ),
            }
        }
        ("fixed", None, None, None, None, None, None, None) => ReviewFindingEventKind::Fixed,
        ("blocked_with_reason", Some(reason), None, None, None, None, None, link) => {
            let link = link
                .map(|link| {
                    let canonical = require_finding_event_external_link(
                        canonical_link,
                        external_link_id(link),
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
        pass_reference,
        pass,
        run,
        kind,
    ))
}

fn decode_event_referenced_finding(
    pass: &ReviewPassEvidence,
    expected_kind: &str,
    reference: ReviewFindingRef,
    stored_status: &str,
) -> Result<ReviewReferencedFindingEvidence, ReviewWorkflowStoreError> {
    let canonical = match (expected_kind, pass_state_result(pass.state())) {
        ("duplicate", Some(ReviewPassResult::FindingEvent(result))) => match result.kind() {
            ReviewFindingEventResultKind::Duplicate { canonical } => Some(*canonical),
            _ => None,
        },
        ("superseded", Some(ReviewPassResult::FindingEvent(result))) => match result.kind() {
            ReviewFindingEventResultKind::Superseded { successor } => Some(*successor),
            _ => None,
        },
        _ => None,
    }
    .ok_or_else(|| {
        corruption(
            "review_finding_event",
            String::from("event pass omitted its authenticated reference result"),
        )
    })?;
    let status = decode_finding_status(stored_status)?;
    if canonical.reference() != reference || canonical.status() != status {
        return Err(corruption(
            "review_finding_event",
            String::from("event reference differs from its pass result"),
        ));
    }
    Ok(canonical)
}

fn require_finding_event_external_link(
    canonical_link: Option<&ReviewExternalLink>,
    event_link: ReviewExternalLinkId,
) -> Result<&ReviewExternalLink, ReviewWorkflowStoreError> {
    let canonical = canonical_link.ok_or_else(|| {
        corruption(
            "review_finding_event",
            String::from("event canonical link is missing"),
        )
    })?;
    if canonical.id() != event_link {
        return Err(corruption(
            "review_finding_event",
            String::from("event link identity mismatch"),
        ));
    }
    Ok(canonical)
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
    pass: ReviewPassEvidence,
) -> Result<ReviewExternalLinkAttachment, ReviewWorkflowStoreError> {
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(row.try_get("target_id")?),
            run_id(row.try_get("pass_run_id")?),
        ),
        pass_id(row.try_get("pass_id")?),
    );
    if pass.reference() != reference {
        return Err(corruption(
            "review_external_link_attachment",
            String::from("attaching pass ancestry mismatch"),
        ));
    }
    Ok(ReviewExternalLinkAttachment::new(
        external_link_id(row.try_get("external_link_id")?),
        reference,
        pass.clone(),
        ReviewRunEvidence::new(
            reference.run(),
            decode_workflow_kind(&row.try_get::<String, _>("pass_workflow_kind")?)?,
            pass.policy(),
            decode_run_state(
                reference.run(),
                &row.try_get::<String, _>("pass_run_state_kind")?,
                row.try_get("pass_run_state_pass_id")?,
            )?,
        ),
        review_key(
            row.try_get("external_object_key")?,
            "review_external_link_attachment",
        )?,
    ))
}

fn authenticate_external_object_target(
    attached_target: &ReviewTarget,
    logical_target: &ReviewTarget,
) -> Result<(), ReviewWorkflowStoreError> {
    if attached_target.id() == logical_target.id() {
        return Ok(());
    }
    let same_change_request = matches!(
        (attached_target.subject(), logical_target.subject()),
        (
            ReviewTargetSubject::ChangeRequest(attached),
            ReviewTargetSubject::ChangeRequest(logical),
        ) if attached == logical
    );
    if attached_target.provider() != logical_target.provider()
        || attached_target.repository() != logical_target.repository()
        || !same_change_request
    {
        return Err(corruption(
            "review_external_link_attachment",
            String::from("external object identity names an unrelated logical target"),
        ));
    }
    Ok(())
}

fn decode_external_link_observation(
    row: &PgRow,
    pass: ReviewPassEvidence,
) -> Result<ReviewExternalLinkObservation, ReviewWorkflowStoreError> {
    let reference = ReviewPassRef::new(
        ReviewRunRef::new(
            target_id(row.try_get("target_id")?),
            run_id(row.try_get("pass_run_id")?),
        ),
        pass_id(row.try_get("pass_id")?),
    );
    if pass.reference() != reference {
        return Err(corruption(
            "review_external_link_observation",
            String::from("observing pass ancestry mismatch"),
        ));
    }
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
        reference,
        pass.clone(),
        ReviewRunEvidence::new(
            reference.run(),
            decode_workflow_kind(&row.try_get::<String, _>("run_workflow_kind")?)?,
            pass.policy(),
            decode_run_state(
                reference.run(),
                &row.try_get::<String, _>("run_state_kind")?,
                row.try_get("run_state_pass_id")?,
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

fn same_pass_execution(left: &ReviewPassEvidence, right: &ReviewPassEvidence) -> bool {
    if left.reference() != right.reference()
        || left.kind() != right.kind()
        || left.policy() != right.policy()
    {
        return false;
    }
    match (left.state(), right.state()) {
        (ReviewPassState::Queued, ReviewPassState::Queued) => true,
        (ReviewPassState::Running { turn: left }, ReviewPassState::Running { turn: right })
        | (ReviewPassState::Failed { turn: left }, ReviewPassState::Failed { turn: right }) => {
            left == right
        }
        (
            ReviewPassState::Succeeded {
                turn: left_turn,
                output_frontier: left_frontier,
                ..
            },
            ReviewPassState::Succeeded {
                turn: right_turn,
                output_frontier: right_frontier,
                ..
            },
        ) => left_turn == right_turn && left_frontier == right_frontier,
        (
            ReviewPassState::Blocked {
                turn: left_turn, ..
            },
            ReviewPassState::Blocked {
                turn: right_turn, ..
            },
        ) => left_turn == right_turn,
        (ReviewPassState::Cancelled { turn: left }, ReviewPassState::Cancelled { turn: right }) => {
            left == right
        }
        _ => false,
    }
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
        ReviewPassResult::ExternalLinkNoChange(no_change) => EncodedPassResult {
            ordinal: Some(no_change.observed_through()),
            external_link: Some(no_change.link()),
            observation_state: Some(no_change.state()),
            ..empty("external_link_no_change")
        },
        ReviewPassResult::ExternalLinkPublicationBlocked(blocked) => EncodedPassResult {
            reason: Some(blocked.reason().as_str()),
            external_link: Some(blocked.link()),
            ..empty("external_link_publication_blocked")
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
    referenced_target: Option<Uuid>,
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
    let referenced_finding: Option<Uuid> =
        row.try_get(column("result_referenced_finding_id").as_str())?;
    let referenced_target =
        match row.try_get(column("result_referenced_finding_target_id").as_str()) {
            Ok(target) => target,
            Err(sqlx::Error::ColumnNotFound(_)) if referenced_finding.is_none() => None,
            Err(error) => return Err(error.into()),
        };
    Ok(StoredPassResult {
        kind: row.try_get(column("result_kind").as_str())?,
        target: row.try_get(target_column)?,
        finding: row.try_get(column("result_finding_id").as_str())?,
        finding_run: row.try_get(column("result_finding_run_id").as_str())?,
        finding_pass: row.try_get(column("result_finding_pass_id").as_str())?,
        ordinal: row.try_get(column("result_event_ordinal").as_str())?,
        event_kind: row.try_get(column("result_event_kind").as_str())?,
        reason: row.try_get(column("result_reason").as_str())?,
        referenced_finding,
        referenced_run: row.try_get(column("result_referenced_finding_run_id").as_str())?,
        referenced_target,
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
    referenced_producer: Option<&LoadedReviewPass>,
) -> Result<ReviewPassState, ReviewWorkflowStoreError> {
    let result = decode_pass_result(stored, produced_findings, referenced_producer)?;
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
    referenced_producer: Option<&LoadedReviewPass>,
) -> Result<Option<ReviewPassResult>, ReviewWorkflowStoreError> {
    let scalar_empty = stored.finding.is_none()
        && stored.finding_run.is_none()
        && stored.finding_pass.is_none()
        && stored.ordinal.is_none()
        && stored.event_kind.is_none()
        && stored.reason.is_none()
        && stored.referenced_finding.is_none()
        && stored.referenced_run.is_none()
        && stored.referenced_target.is_none()
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
                decode_stored_finding_event(&stored, referenced_producer)?,
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
                Some(decode_stored_finding_event(&stored, referenced_producer)?)
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
        Some("external_link_no_change")
            if stored.finding.is_none()
                && stored.ordinal.is_some()
                && stored.event_kind.is_none()
                && stored.reason.is_none()
                && stored.referenced_finding.is_none()
                && stored.external_object.is_none() =>
        {
            let link = stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption("review_pass", String::from("unchanged result omitted link"))
            })?;
            let state = stored.observation_state.as_deref().ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("unchanged result omitted state"),
                )
            })?;
            let ordinal = stored.ordinal.ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("unchanged result omitted observation frontier"),
                )
            })?;
            Ok(Some(ReviewPassResult::ExternalLinkNoChange(
                ReviewExternalLinkNoChangeResult::new(
                    link,
                    ReviewEventOrdinal::try_new(positive_u32(ordinal, "review_pass")?).map_err(
                        |_| corruption("review_pass", String::from("zero observation frontier")),
                    )?,
                    decode_external_object_state(state)?,
                ),
            )))
        }
        Some("external_link_publication_blocked")
            if stored.finding.is_none()
                && stored.ordinal.is_none()
                && stored.event_kind.is_none()
                && stored.referenced_finding.is_none()
                && stored.external_object.is_none()
                && stored.observation_state.is_none() =>
        {
            let link = stored.external_link.map(external_link_id).ok_or_else(|| {
                corruption(
                    "review_pass",
                    String::from("publication block result omitted link"),
                )
            })?;
            let reason = review_text(
                stored.reason.ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("publication block result omitted reason"),
                    )
                })?,
                "review_pass",
            )?;
            Ok(Some(ReviewPassResult::ExternalLinkPublicationBlocked(
                ReviewExternalLinkPublicationBlockedResult::new(link, reason),
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
    referenced_producer: Option<&LoadedReviewPass>,
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
            canonical: decode_stored_referenced_finding(stored, referenced_producer)?,
        },
        "superseded" => ReviewFindingEventResultKind::Superseded {
            successor: decode_stored_referenced_finding(stored, referenced_producer)?,
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
    referenced_producer: Option<&LoadedReviewPass>,
) -> Result<ReviewReferencedFindingEvidence, ReviewWorkflowStoreError> {
    let reference = ReviewFindingRef::new(
        ReviewPassRef::new(
            ReviewRunRef::new(
                target_id(stored.referenced_target.ok_or_else(|| {
                    corruption(
                        "review_pass",
                        String::from("referenced result omitted target"),
                    )
                })?),
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
    let producer = referenced_producer.ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result producer is missing"),
        )
    })?;
    let producer_pass = producer.evidence();
    ReviewReferencedFindingEvidence::try_reconstitute(
        reference,
        status,
        &producer_pass,
        producer.run_evidence(),
    )
    .ok_or_else(|| {
        corruption(
            "review_pass",
            String::from("referenced result producer or status is invalid"),
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
    referenced: Option<ReviewReferencedFindingEvidence>,
    referenced_status: Option<ReviewFindingStatus>,
    external_link: Option<ReviewExternalLinkId>,
}

fn encode_finding_event(event: &ReviewFindingEventKind) -> EncodedFindingEvent<'_> {
    let empty = |kind| EncodedFindingEvent {
        kind,
        reason: None,
        referenced: None,
        referenced_status: None,
        external_link: None,
    };
    match event {
        ReviewFindingEventKind::Accepted => empty("accepted"),
        ReviewFindingEventKind::Rejected { reason } => EncodedFindingEvent {
            kind: "rejected",
            reason: Some(reason.as_str()),
            referenced: None,
            referenced_status: None,
            external_link: None,
        },
        ReviewFindingEventKind::Duplicate { canonical } => EncodedFindingEvent {
            kind: "duplicate",
            reason: None,
            referenced: Some(*canonical),
            referenced_status: Some(canonical.status()),
            external_link: None,
        },
        ReviewFindingEventKind::Superseded { successor } => EncodedFindingEvent {
            kind: "superseded",
            reason: None,
            referenced: Some(*successor),
            referenced_status: Some(successor.status()),
            external_link: None,
        },
        ReviewFindingEventKind::Stale => empty("stale"),
        ReviewFindingEventKind::Posted { link } => EncodedFindingEvent {
            kind: "posted",
            reason: None,
            referenced: None,
            referenced_status: None,
            external_link: Some(link.link()),
        },
        ReviewFindingEventKind::Fixed => empty("fixed"),
        ReviewFindingEventKind::BlockedWithReason { reason, link } => EncodedFindingEvent {
            kind: "blocked_with_reason",
            reason: Some(reason.as_str()),
            referenced: None,
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

const fn workflow_matches_pass_kind(workflow: ReviewWorkflowKind, pass: ReviewPassKind) -> bool {
    matches!(
        (workflow, pass),
        (
            ReviewWorkflowKind::ImportExternalContext,
            ReviewPassKind::ImportExternalContext
        ) | (
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPassKind::ReadOnlyReview
        ) | (ReviewWorkflowKind::JudgeFindings, ReviewPassKind::Judge)
            | (ReviewWorkflowKind::DedupeFindings, ReviewPassKind::Dedupe)
            | (ReviewWorkflowKind::PublishReview, ReviewPassKind::Publish)
            | (ReviewWorkflowKind::FixFindings, ReviewPassKind::Fix)
            | (
                ReviewWorkflowKind::PropagateStack,
                ReviewPassKind::PropagateStack
            )
    )
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

fn decode_review_confidence(
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
    let judge = decode_review_confidence(row.try_get(judge_column)?, aggregate)?;
    let publication = decode_review_confidence(row.try_get(publication_column)?, aggregate)?;
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

pub(crate) fn target_id(value: Uuid) -> ReviewTargetId {
    ReviewTargetId::from_uuid(value)
}

pub(crate) fn run_id(value: Uuid) -> ReviewRunId {
    ReviewRunId::from_uuid(value)
}

pub(crate) fn pass_id(value: Uuid) -> ReviewPassId {
    ReviewPassId::from_uuid(value)
}

pub(crate) fn finding_id(value: Uuid) -> ReviewFindingId {
    ReviewFindingId::from_uuid(value)
}

pub(crate) fn external_link_id(value: Uuid) -> ReviewExternalLinkId {
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

pub(crate) fn corruption(aggregate: &'static str, detail: String) -> ReviewWorkflowStoreError {
    ReviewWorkflowStoreError::Corruption(ReviewWorkflowCorruption { aggregate, detail })
}

/// The session and originating turn one accepted input records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewAcceptedInputOrigin {
    session: SessionId,
    origin_turn: Option<TurnId>,
}

impl ReviewAcceptedInputOrigin {
    /// The session the input was accepted into.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// The turn the input originated, absent while it originated none.
    pub const fn origin_turn(&self) -> Option<TurnId> {
        self.origin_turn
    }
}

/// The lifecycle position one turn row records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewTurnLifecycleState {
    /// Accepted, with no attempt started.
    Queued,
    /// Started, with no terminal disposition recorded.
    Active,
    /// Finished under the recorded disposition.
    Terminal(ReviewPassTurnOutcome),
}

/// One turn's durable lifecycle facts, as a review pass reads them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewTurnLifecycle {
    session: SessionId,
    accepted_input: Option<AcceptedInputId>,
    state: ReviewTurnLifecycleState,
    terminal_frontier: Option<ContextFrontierId>,
}

impl ReviewTurnLifecycle {
    /// The session the turn runs in.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// The input the turn originated from, absent for a delegated turn.
    pub const fn accepted_input(&self) -> Option<AcceptedInputId> {
        self.accepted_input
    }

    /// The lifecycle position the row records.
    pub const fn state(&self) -> ReviewTurnLifecycleState {
        self.state
    }

    /// The frontier the turn ended on, absent until it is terminal.
    pub const fn terminal_frontier(&self) -> Option<ContextFrontierId> {
        self.terminal_frontier
    }
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
    /// A paired run and pass do not describe one domain-coherent admission.
    RunPassMismatch,
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
            Self::RunPassMismatch => {
                formatter.write_str("new review run and pass are not one coherent admission")
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
    /// A blocked publication reservation was not reconciled atomically.
    IncompletePublicationReconciliation,
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
            Self::IncompletePublicationReconciliation => formatter.write_str(
                "blocked publication reservations require one atomic reconciliation effect",
            ),
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
            Self::NonAtomicPassResult
            | Self::IncompleteFindingInventory
            | Self::IncompletePublicationReconciliation => None,
            Self::ReservationConflict(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ReviewWorkflowStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl ReviewWorkflowReader for ReviewWorkflowStore {
    type Error = ReviewWorkflowStoreError;

    async fn load_target(
        &self,
        target: ReviewTargetId,
    ) -> Result<Option<ReviewTarget>, Self::Error> {
        ReviewWorkflowStore::load_target(self, target).await
    }

    async fn load_run(&self, run: ReviewRunId) -> Result<Option<ReviewRun>, Self::Error> {
        ReviewWorkflowStore::load_run(self, run).await
    }

    async fn load_run_with_pass(
        &self,
        run: ReviewRunId,
    ) -> Result<Option<(ReviewRun, Option<ReviewPass>)>, Self::Error> {
        ReviewWorkflowStore::load_run_with_pass(self, run).await
    }

    async fn load_pass(&self, pass: ReviewPassId) -> Result<Option<ReviewPass>, Self::Error> {
        ReviewWorkflowStore::load_pass(self, pass).await
    }

    async fn load_finding(
        &self,
        finding: ReviewFindingId,
    ) -> Result<Option<ReviewFinding>, Self::Error> {
        ReviewWorkflowStore::load_finding(self, finding).await
    }

    async fn list_findings(&self, run: ReviewRunId) -> Result<Vec<ReviewFinding>, Self::Error> {
        ReviewWorkflowStore::list_findings(self, run).await
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

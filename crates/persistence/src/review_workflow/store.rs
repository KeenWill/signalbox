use super::decode::{
    decode_pass_turn_evidence, decode_run, decode_run_for_transition, decode_turn_lifecycle_state,
    projected,
};
use super::load::{load_pass_on_connection, load_target_on_connection};
use super::pass_codec::{encode_pass_state, pass_state_result};
use super::{
    ReviewAcceptedInputOrigin, ReviewTurnLifecycle, ReviewWorkflowInsertionError,
    ReviewWorkflowStore, ReviewWorkflowStoreError, ReviewWorkflowTransitionError,
    accepted_input_id, begin_repeatable_read, commit_mutation, context_frontier_id, corruption,
    encode_pass_kind, encode_run_state, encode_workflow_kind, pass_id, require_joined_reference,
    session_id, turn_id, workflow_matches_pass_kind,
};
use rust_decimal::Decimal;
use signalbox_domain::{
    AcceptedInputId, ContextFrontierId, ReviewKey, ReviewPass, ReviewPassEvidence, ReviewPassId,
    ReviewPassState, ReviewRun, ReviewRunId, ReviewRunState, ReviewTarget, ReviewTargetId,
    ReviewTargetSubject, TurnId,
};
use sqlx::types::Uuid;
use sqlx::{PgConnection, PgPool, Row};

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
        let mut transaction = self.pool.begin().await?;
        self.insert_target_on_connection(&mut transaction, target)
            .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }

    pub(crate) async fn insert_target_on_connection(
        &self,
        connection: &mut PgConnection,
        target: &ReviewTarget,
    ) -> Result<(), ReviewWorkflowStoreError> {
        let (subject_kind, change_request_number) = match target.subject() {
            ReviewTargetSubject::ChangeRequest(number) => {
                ("change_request", Some(Decimal::from(number.get())))
            }
            ReviewTargetSubject::Commit => ("commit", None),
        };
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
        .execute(&mut *connection)
        .await?;
        Ok(())
    }

    /// Loads and validates one target snapshot.
    pub async fn load_target(
        &self,
        target: ReviewTargetId,
    ) -> Result<Option<ReviewTarget>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let target = self
            .load_target_in_transaction(&mut transaction, target)
            .await?;
        transaction.commit().await?;
        Ok(target)
    }

    pub(crate) async fn load_target_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        target: ReviewTargetId,
    ) -> Result<Option<ReviewTarget>, ReviewWorkflowStoreError> {
        load_target_on_connection(transaction, target).await
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

    pub(crate) async fn load_run_on_connection(
        &self,
        connection: &mut PgConnection,
        run: ReviewRunId,
    ) -> Result<Option<ReviewRun>, ReviewWorkflowStoreError> {
        Ok(self
            .load_run_with_pass_on_connection(connection, run)
            .await?
            .map(|(run, _pass)| run))
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
                    canonical_pass.result_reason AS evidence_pass_result_reason,
                    canonical_pass.result_judge_confidence AS evidence_pass_result_judge_confidence,
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
        let mut transaction = self.pool.begin().await?;
        self.insert_pass_on_connection(&mut transaction, pass)
            .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }

    pub(crate) async fn insert_pass_on_connection(
        &self,
        connection: &mut PgConnection,
        pass: &ReviewPass,
    ) -> Result<(), ReviewWorkflowStoreError> {
        if pass.state() != &ReviewPassState::Queued {
            return Err(ReviewWorkflowStoreError::InvalidInsertion(
                ReviewWorkflowInsertionError::PassNotQueued {
                    state: Box::new(pass.state().clone()),
                },
            ));
        }
        let state = encode_pass_state(pass.state());
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
        .execute(&mut *connection)
        .await?;
        Ok(())
    }

    /// Inserts one queued run and its first queued pass atomically.
    pub async fn insert_run_and_pass(
        &self,
        run: &ReviewRun,
        pass: &ReviewPass,
    ) -> Result<(), ReviewWorkflowStoreError> {
        let mut transaction = self.pool.begin().await?;
        self.insert_run_and_pass_on_connection(&mut transaction, run, pass)
            .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }

    pub(crate) async fn insert_run_and_pass_on_connection(
        &self,
        connection: &mut PgConnection,
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
        .execute(&mut *connection)
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
        .execute(&mut *connection)
        .await?;
        Ok(())
    }

    /// Loads and validates one pass projection.
    pub async fn load_pass(
        &self,
        pass: ReviewPassId,
    ) -> Result<Option<ReviewPass>, ReviewWorkflowStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let pass = self.load_pass_on_connection(&mut transaction, pass).await?;
        transaction.commit().await?;
        Ok(pass)
    }

    pub(crate) async fn load_pass_on_connection(
        &self,
        connection: &mut PgConnection,
        pass: ReviewPassId,
    ) -> Result<Option<ReviewPass>, ReviewWorkflowStoreError> {
        Ok(load_pass_on_connection(connection, pass)
            .await?
            .map(|loaded| loaded.pass))
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
        let mut transaction = self.pool.begin().await?;
        let transitioned = self
            .transition_run_and_pass_on_connection(&mut transaction, run, pass, next_run, next_pass)
            .await?;
        if transitioned.is_some() {
            commit_mutation(transaction).await?;
        } else {
            transaction.rollback().await?;
        }
        Ok(transitioned)
    }

    pub(crate) async fn transition_run_and_pass_on_connection(
        &self,
        connection: &mut PgConnection,
        run: ReviewRunId,
        pass: ReviewPassId,
        next_run: ReviewRunState,
        next_pass: ReviewPassState,
    ) -> Result<Option<(ReviewRun, ReviewPass)>, ReviewWorkflowStoreError> {
        if pass_state_result(&next_pass).is_some() {
            return Err(ReviewWorkflowStoreError::NonAtomicPassResult);
        }
        let run_row = sqlx::query(crate::lock_inventory::REVIEW_RUN_TRANSITION)
            .bind(run.into_uuid())
            .fetch_optional(&mut *connection)
            .await?;
        let Some(run_row) = run_row else {
            return Ok(None);
        };
        let pass_row = sqlx::query(crate::lock_inventory::REVIEW_PASS_TRANSITION)
            .bind(pass.into_uuid())
            .bind(encode_pass_state(&next_pass).turn.map(TurnId::into_uuid))
            .fetch_optional(&mut *connection)
            .await?;
        let Some(pass_row) = pass_row else {
            return Ok(None);
        };
        let turn_evidence = decode_pass_turn_evidence(&pass_row)?;
        let loaded_pass = load_pass_on_connection(connection, pass)
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
        .execute(&mut *connection)
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
        .execute(&mut *connection)
        .await?;
        Ok(Some((transitioned_run, transitioned_pass)))
    }
}

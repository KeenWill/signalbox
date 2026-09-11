use super::decode::{
    decode_pass_accepted_input_evidence, decode_pass_turn_evidence, decode_run_facts,
    decode_run_for_transition,
};
use super::load::{LoadedReviewPass, load_pass_on_connection};
use super::pass_codec::{encode_pass_state, pass_state_result, pass_state_turn};
use super::{
    ReviewWorkflowStoreError, corruption, decode_pass_kind, encode_diff_side,
    encode_external_object_state, encode_finding_event, encode_finding_status, encode_run_state,
    encode_severity, pass_id, run_id, session_id, target_id,
};
use signalbox_domain::{
    ContextFrontierId, ReviewExternalLinkId, ReviewFinding, ReviewFindingEvent, ReviewPass,
    ReviewPassEvidence, ReviewPassId, ReviewPassRef, ReviewPassState, ReviewPassTurnEvidence,
    ReviewPassTurnOutcome, ReviewRun, ReviewRunEvidence, ReviewRunReconstitutionInput,
    ReviewRunRef, ReviewRunState, ReviewText, TurnId,
};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{Postgres, Row, Transaction};

pub(super) async fn insert_finding_row(
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

pub(super) async fn insert_finding_event(
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
             external_link_id, external_link_association_kind, judge_confidence)
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
             $13, $14, $15, $16
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
    .bind(encoded.judge_confidence)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(super) async fn bind_pass_result(
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
                result_observation_state = $20,
                result_judge_confidence = $22
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
                    result_observation_state,
                    result_judge_confidence
                ) IS NOT DISTINCT FROM (
                    $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                    $16, $17, $18, $19, $20, $22
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
    .bind(result.judge_confidence)
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

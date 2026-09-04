//! Durable user-global command receipts for review-workflow mutations.

use signalbox_application::{
    ReviewPassCompletionStatus, ReviewWorkflowCommand, ReviewWorkflowCommandOutcome,
    ReviewWorkflowCommandResult, ReviewWorkflowOperation, ReviewWorkflowOperationKind,
    ReviewWorkflowTransaction,
};
use signalbox_domain::{
    ReviewFinding, ReviewFindingStatus, ReviewKey, ReviewPass, ReviewPassState, ReviewRun,
    ReviewRunState,
};
use sqlx::{Postgres, Row, Transaction, types::Uuid};

use crate::{
    command_registry::{self, CommandKind, REVIEW_WORKFLOW_KIND, RegistryInspectionError},
    mapping::durable_command_id_to_uuid,
    review_workflow::{ReserveExternalLinkOutcome, ReviewWorkflowStore, ReviewWorkflowStoreError},
};

const STORAGE_VERSION: i16 = 1;

impl ReviewWorkflowStore {
    /// Loads an exact prior receipt before aggregate-state validation.
    pub async fn load_command_outcome(
        &self,
        command_id: signalbox_domain::DurableCommandId,
        semantic_digest: [u8; 32],
        operation_kind: ReviewWorkflowOperationKind,
    ) -> Result<Option<ReviewWorkflowCommandOutcome>, ReviewWorkflowStoreError> {
        let mut transaction = self.pool().begin().await?;
        let inspection = inspect_existing(
            &mut transaction,
            command_id,
            semantic_digest,
            operation_kind,
        )
        .await?;
        transaction.commit().await?;
        Ok(match inspection {
            ClaimInspection::New => None,
            ClaimInspection::Recorded(result) => {
                Some(ReviewWorkflowCommandOutcome::Recorded(result))
            }
            ClaimInspection::Conflicting => {
                Some(ReviewWorkflowCommandOutcome::ConflictingReuse { command_id })
            }
        })
    }
}

impl ReviewWorkflowTransaction for ReviewWorkflowStore {
    type Error = ReviewWorkflowStoreError;

    async fn handle(
        &mut self,
        command: ReviewWorkflowCommand,
    ) -> Result<ReviewWorkflowCommandOutcome, Self::Error> {
        let mut claim = self.pool().begin().await?;
        match inspect_claim(&mut claim, &command).await? {
            ClaimInspection::Recorded(result) => {
                claim.commit().await?;
                return Ok(ReviewWorkflowCommandOutcome::Recorded(result));
            }
            ClaimInspection::Conflicting => {
                claim.rollback().await?;
                return Ok(ReviewWorkflowCommandOutcome::ConflictingReuse {
                    command_id: command.command_id(),
                });
            }
            ClaimInspection::New => {}
        }

        let result = apply_or_recover(self, command.operation()).await?;
        insert_receipt(&mut claim, &command, &result)
            .await
            .map_err(ReviewWorkflowStoreError::CommitAmbiguous)?;
        commit_claim(claim).await?;
        Ok(ReviewWorkflowCommandOutcome::Recorded(result))
    }
}

enum ClaimInspection {
    New,
    Recorded(ReviewWorkflowCommandResult),
    Conflicting,
}

async fn inspect_claim(
    transaction: &mut Transaction<'_, Postgres>,
    command: &ReviewWorkflowCommand,
) -> Result<ClaimInspection, ReviewWorkflowStoreError> {
    let inspection = inspect_existing(
        transaction,
        command.command_id(),
        command.semantic_digest(),
        command.operation().kind(),
    )
    .await?;
    match inspection {
        ClaimInspection::New => {
            let issuer = crate::command_registry::issuer_columns(
                signalbox_domain::CommandPrincipal::Operator,
            );
            let claimed = sqlx::query(
                "INSERT INTO durable_command
                    (command_id, command_kind, storage_version, claimed_at,
                     issuer_kind, issuer_module)
                 VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
                 ON CONFLICT DO NOTHING",
            )
            .bind(durable_command_id_to_uuid(command.command_id()))
            .bind(REVIEW_WORKFLOW_KIND)
            .bind(STORAGE_VERSION)
            .bind(issuer.0)
            .bind(issuer.1)
            .execute(&mut **transaction)
            .await?
            .rows_affected()
                == 1;
            if claimed {
                Ok(ClaimInspection::New)
            } else {
                Box::pin(inspect_claim(transaction, command)).await
            }
        }
        recorded => Ok(recorded),
    }
}

async fn inspect_existing(
    transaction: &mut Transaction<'_, Postgres>,
    command_id: signalbox_domain::DurableCommandId,
    semantic_digest: [u8; 32],
    expected_operation: ReviewWorkflowOperationKind,
) -> Result<ClaimInspection, ReviewWorkflowStoreError> {
    let kind = command_registry::inspect(transaction, command_id)
        .await
        .map_err(registry_error)?;
    match kind {
        None => Ok(ClaimInspection::New),
        Some(CommandKind::ReviewWorkflow) => {
            let row = sqlx::query(
                "SELECT semantic_digest, operation_kind, result_kind,
                        result_target_id, result_run_id, result_pass_id,
                        result_finding_id, result_external_link_id,
                        result_finding_count, result_finding_status,
                        result_external_object_key
                   FROM review_workflow_command
                  WHERE command_id = $1",
            )
            .bind(durable_command_id_to_uuid(command_id))
            .fetch_one(&mut **transaction)
            .await?;
            let digest: Vec<u8> = row.try_get("semantic_digest")?;
            if digest.as_slice() != semantic_digest
                || row.try_get::<String, _>("operation_kind")? != operation_kind(expected_operation)
            {
                return Ok(ClaimInspection::Conflicting);
            }
            Ok(ClaimInspection::Recorded(decode_result(&row)?))
        }
        Some(
            CommandKind::CreateSession
            | CommandKind::CreateSessionFromImportedFrontier
            | CommandKind::ReplaceSessionDefaults
            | CommandKind::ReplaceSessionMetadata
            | CommandKind::SubmitInput
            | CommandKind::DecideToolRequest
            | CommandKind::OverrideDeniedToolRequest
            | CommandKind::ReviewOrchestration
            | CommandKind::CompactSession
            | CommandKind::Goal
            | CommandKind::UpdateSessionPlacement
            | CommandKind::RegisterWorkspace
            | CommandKind::MintGitRemote
            | CommandKind::WithdrawGitRemote
            | CommandKind::SessionLifecycle,
        ) => Ok(ClaimInspection::Conflicting),
    }
}

async fn apply_or_recover(
    store: &ReviewWorkflowStore,
    operation: &ReviewWorkflowOperation,
) -> Result<ReviewWorkflowCommandResult, ReviewWorkflowStoreError> {
    match operation {
        ReviewWorkflowOperation::CreateTarget(target) => {
            match store.load_target(target.id()).await? {
                Some(existing) if existing == *target => {}
                Some(_) => return Err(command_conflict("target identity names another snapshot")),
                None => store.insert_target(target).await?,
            }
            Ok(ReviewWorkflowCommandResult::TargetCreated {
                target: target.id(),
            })
        }
        ReviewWorkflowOperation::StartRun { run, pass } => {
            let existing_run = store.load_run(run.reference().run()).await?;
            let existing_pass = store.load_pass(pass.reference().pass()).await?;
            match (existing_run, existing_pass) {
                (Some(existing_run), Some(existing_pass))
                    if same_run_admission(&existing_run, run)
                        && same_pass_admission(&existing_pass, pass) => {}
                (Some(existing_run), None) if same_run_admission(&existing_run, run) => {
                    store.insert_pass(pass).await?;
                }
                (None, None) => store.insert_run_and_pass(run, pass).await?,
                (Some(_), _) => {
                    return Err(command_conflict("run identity names another workflow"));
                }
                (None, Some(_)) => {
                    return Err(command_conflict("pass identity names another execution"));
                }
            }
            Ok(ReviewWorkflowCommandResult::RunStarted {
                run: run.reference().run(),
                pass: pass.reference().pass(),
            })
        }
        ReviewWorkflowOperation::ActivatePass { run, pass } => {
            let current_run = store.load_run(run.reference().run()).await?;
            let current_pass = store.load_pass(pass.reference().pass()).await?;
            let activation_is_recoverable = current_run
                .as_ref()
                .zip(current_pass.as_ref())
                .is_some_and(|(current_run, current_pass)| {
                    same_activation_effect(current_run, current_pass, run, pass)
                });
            if !activation_is_recoverable {
                let transitioned = store
                    .transition_run_and_pass(
                        run.reference().run(),
                        pass.reference().pass(),
                        run.state(),
                        pass.state().clone(),
                    )
                    .await?;
                let Some((committed_run, committed_pass)) = transitioned else {
                    return Err(command_conflict("run or pass does not exist"));
                };
                if committed_run != *run || committed_pass != *pass {
                    return Err(command_conflict(
                        "run or pass activation differs from the requested transition",
                    ));
                }
            }
            Ok(ReviewWorkflowCommandResult::PassActivated {
                run: run.reference().run(),
                pass: pass.reference().pass(),
            })
        }
        ReviewWorkflowOperation::CompletePass { run, pass } => {
            let status = ReviewPassCompletionStatus::from_state(pass.state())
                .ok_or_else(|| command_conflict("pass completion is not result-free terminal"))?;
            let current_run = store.load_run(run.reference().run()).await?;
            let current_pass = store.load_pass(pass.reference().pass()).await?;
            let completion_is_recoverable =
                current_run.as_ref().zip(current_pass.as_ref()).is_some_and(
                    |(current_run, current_pass)| current_run == run && current_pass == pass,
                );
            if !completion_is_recoverable {
                let transitioned = store
                    .transition_run_and_pass(
                        run.reference().run(),
                        pass.reference().pass(),
                        run.state(),
                        pass.state().clone(),
                    )
                    .await?;
                let Some((committed_run, committed_pass)) = transitioned else {
                    return Err(command_conflict("run or pass does not exist"));
                };
                if committed_run != *run || committed_pass != *pass {
                    return Err(command_conflict(
                        "run or pass completion differs from the requested transition",
                    ));
                }
            }
            Ok(ReviewWorkflowCommandResult::PassCompleted {
                run: run.reference().run(),
                pass: pass.reference().pass(),
                status,
            })
        }
        ReviewWorkflowOperation::RecordFindings { pass, findings } => {
            let committed = store.load_pass(pass.reference().pass()).await?;
            if committed
                .as_ref()
                .is_none_or(|current| current.state() != pass.state())
            {
                store.insert_findings(pass, findings).await?;
            }
            let loaded = store
                .list_findings(pass.reference().run().run())
                .await
                .map_err(post_effect_verification_error)?;
            if !same_finding_inventory(&loaded, findings) {
                return Err(command_conflict(
                    "finding inventory differs from the recorded result",
                ));
            }
            Ok(ReviewWorkflowCommandResult::FindingsRecorded {
                run: pass.reference().run().run(),
                pass: pass.reference().pass(),
                finding_count: findings.len(),
            })
        }
        ReviewWorkflowOperation::RecordFindingEvent { pass, event } => {
            let current = store.load_finding(event.finding().finding()).await?;
            let event_index = usize::try_from(event.ordinal().get())
                .ok()
                .and_then(|ordinal| ordinal.checked_sub(1));
            let already_recorded = current
                .as_ref()
                .and_then(|finding| finding.events().get(event_index?));
            let status = match already_recorded {
                Some(existing) if existing == event => {
                    let (Some(finding), Some(event_index)) = (current.as_ref(), event_index) else {
                        return Err(command_conflict(
                            "recorded finding event has unrepresentable evidence",
                        ));
                    };
                    ReviewFinding::try_reconstitute(
                        finding.proposal().clone(),
                        finding.events()[..=event_index].to_vec(),
                    )
                    .map_err(|_| command_conflict("recorded finding history is invalid"))?
                    .status()
                }
                Some(_) => {
                    return Err(command_conflict(
                        "finding event ordinal has another payload",
                    ));
                }
                None => {
                    let updated = store
                        .append_finding_event(event.finding().finding(), event.clone())
                        .await?;
                    updated
                        .ok_or_else(|| command_conflict("finding does not exist"))?
                        .status()
                }
            };
            let committed = store
                .load_pass(pass.reference().pass())
                .await
                .map_err(post_effect_verification_error)?;
            if committed
                .as_ref()
                .is_none_or(|current| current.state() != pass.state())
            {
                return Err(command_conflict("event pass result was not committed"));
            }
            Ok(ReviewWorkflowCommandResult::FindingEventRecorded {
                finding: event.finding().finding(),
                status,
            })
        }
        ReviewWorkflowOperation::ReserveExternalLink(link) => {
            match store.reserve_external_link(link.clone()).await? {
                ReserveExternalLinkOutcome::Inserted(_)
                | ReserveExternalLinkOutcome::Existing(_) => {}
            }
            Ok(ReviewWorkflowCommandResult::ExternalLinkReserved { link: link.id() })
        }
        ReviewWorkflowOperation::AttachExternalLink { link, attachment } => {
            let current = store.load_external_link(*link).await?;
            match current.as_ref().and_then(|current| current.attachment()) {
                Some(existing) if existing == attachment => {}
                Some(_) => return Err(command_conflict("external link has another attachment")),
                None => {
                    if store
                        .attach_external_link(*link, attachment.clone())
                        .await?
                        .is_none()
                    {
                        return Err(command_conflict("external-link reservation does not exist"));
                    }
                }
            }
            Ok(ReviewWorkflowCommandResult::ExternalLinkAttached {
                link: *link,
                external_object: attachment.external_object().clone(),
            })
        }
    }
}

fn same_run_admission(existing: &ReviewRun, requested: &ReviewRun) -> bool {
    existing.reference() == requested.reference()
        && existing.workflow() == requested.workflow()
        && existing.policy() == requested.policy()
        && (existing.recorded_pass() == requested.recorded_pass()
            || (existing.recorded_pass().is_none() && existing.state() == ReviewRunState::Queued))
        && requested.state() == ReviewRunState::Queued
}

fn same_pass_admission(existing: &ReviewPass, requested: &ReviewPass) -> bool {
    existing.reference() == requested.reference()
        && existing.kind() == requested.kind()
        && existing.session() == requested.session()
        && existing.accepted_input() == requested.accepted_input()
        && existing.origin_turn() == requested.origin_turn()
        && requested.state() == &ReviewPassState::Queued
}

fn same_activation_effect(
    existing_run: &ReviewRun,
    existing_pass: &ReviewPass,
    requested_run: &ReviewRun,
    requested_pass: &ReviewPass,
) -> bool {
    existing_run.reference() == requested_run.reference()
        && existing_run.workflow() == requested_run.workflow()
        && existing_run.policy() == requested_run.policy()
        && existing_run.recorded_pass() == requested_run.recorded_pass()
        && existing_pass.reference() == requested_pass.reference()
        && existing_pass.kind() == requested_pass.kind()
        && existing_pass.session() == requested_pass.session()
        && existing_pass.accepted_input() == requested_pass.accepted_input()
        && existing_pass.origin_turn() == requested_pass.origin_turn()
        && activation_run_state_contains(existing_run.state(), requested_run.state())
        && activation_pass_state_contains(existing_pass.state(), requested_pass.state())
}

fn activation_run_state_contains(existing: ReviewRunState, requested: ReviewRunState) -> bool {
    let ReviewRunState::Running { active_pass } = requested else {
        return false;
    };
    match existing {
        ReviewRunState::Running {
            active_pass: existing,
        }
        | ReviewRunState::Succeeded {
            concluding_pass: existing,
        }
        | ReviewRunState::Failed {
            failed_pass: existing,
        }
        | ReviewRunState::Blocked {
            blocking_pass: existing,
        }
        | ReviewRunState::Cancelled {
            last_pass: Some(existing),
        } => existing == active_pass,
        ReviewRunState::Queued | ReviewRunState::Cancelled { last_pass: None } => false,
    }
}

fn activation_pass_state_contains(existing: &ReviewPassState, requested: &ReviewPassState) -> bool {
    let ReviewPassState::Running { turn } = requested else {
        return false;
    };
    match existing {
        ReviewPassState::Running { turn: existing }
        | ReviewPassState::Succeeded { turn: existing, .. }
        | ReviewPassState::Failed { turn: existing }
        | ReviewPassState::Blocked { turn: existing, .. }
        | ReviewPassState::Cancelled {
            turn: Some(existing),
        } => existing == turn,
        ReviewPassState::Queued | ReviewPassState::Cancelled { turn: None } => false,
    }
}

fn same_finding_inventory(existing: &[ReviewFinding], requested: &[ReviewFinding]) -> bool {
    existing.len() == requested.len()
        && existing
            .iter()
            .zip(requested)
            .all(|(existing, requested)| existing.proposal() == requested.proposal())
}

fn operation_kind(operation: ReviewWorkflowOperationKind) -> &'static str {
    match operation {
        ReviewWorkflowOperationKind::CreateTarget => "create_target",
        ReviewWorkflowOperationKind::StartRun => "start_run",
        ReviewWorkflowOperationKind::RecordFindings => "record_findings",
        ReviewWorkflowOperationKind::RecordFindingEvent => "record_finding_event",
        ReviewWorkflowOperationKind::ActivatePass => "activate_pass",
        ReviewWorkflowOperationKind::CompletePass => "complete_pass",
        ReviewWorkflowOperationKind::ReserveExternalLink => "reserve_external_link",
        ReviewWorkflowOperationKind::AttachExternalLink => "attach_external_link",
    }
}

async fn insert_receipt(
    transaction: &mut Transaction<'_, Postgres>,
    command: &ReviewWorkflowCommand,
    result: &ReviewWorkflowCommandResult,
) -> Result<(), sqlx::Error> {
    let encoded = encode_result(result);
    sqlx::query(
        "INSERT INTO review_workflow_command
            (command_id, command_kind, storage_version, semantic_digest,
             operation_kind, result_kind, result_target_id, result_run_id,
             result_pass_id, result_finding_id, result_external_link_id,
             result_finding_count, result_finding_status, result_external_object_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(REVIEW_WORKFLOW_KIND)
    .bind(STORAGE_VERSION)
    .bind(command.semantic_digest().as_slice())
    .bind(operation_kind(command.operation().kind()))
    .bind(encoded.kind)
    .bind(encoded.target)
    .bind(encoded.run)
    .bind(encoded.pass)
    .bind(encoded.finding)
    .bind(encoded.link)
    .bind(encoded.count)
    .bind(encoded.status)
    .bind(encoded.external_object)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

struct EncodedResult {
    kind: &'static str,
    target: Option<Uuid>,
    run: Option<Uuid>,
    pass: Option<Uuid>,
    finding: Option<Uuid>,
    link: Option<Uuid>,
    count: Option<i64>,
    status: Option<&'static str>,
    external_object: Option<String>,
}

fn encode_result(result: &ReviewWorkflowCommandResult) -> EncodedResult {
    let mut encoded = EncodedResult {
        kind: "",
        target: None,
        run: None,
        pass: None,
        finding: None,
        link: None,
        count: None,
        status: None,
        external_object: None,
    };
    match result {
        ReviewWorkflowCommandResult::TargetCreated { target } => {
            encoded.kind = "target_created";
            encoded.target = Some(target.into_uuid());
        }
        ReviewWorkflowCommandResult::RunStarted { run, pass } => {
            encoded.kind = "run_started";
            encoded.run = Some(run.into_uuid());
            encoded.pass = Some(pass.into_uuid());
        }
        ReviewWorkflowCommandResult::FindingsRecorded {
            run,
            pass,
            finding_count,
        } => {
            encoded.kind = "findings_recorded";
            encoded.run = Some(run.into_uuid());
            encoded.pass = Some(pass.into_uuid());
            encoded.count = i64::try_from(*finding_count).ok();
        }
        ReviewWorkflowCommandResult::FindingEventRecorded { finding, status } => {
            encoded.kind = "finding_event_recorded";
            encoded.finding = Some(finding.into_uuid());
            encoded.status = Some(encode_finding_status(*status));
        }
        ReviewWorkflowCommandResult::PassActivated { run, pass } => {
            encoded.kind = "pass_activated";
            encoded.run = Some(run.into_uuid());
            encoded.pass = Some(pass.into_uuid());
        }
        ReviewWorkflowCommandResult::PassCompleted { run, pass, status } => {
            encoded.kind = "pass_completed";
            encoded.run = Some(run.into_uuid());
            encoded.pass = Some(pass.into_uuid());
            encoded.status = Some(encode_completion_status(*status));
        }
        ReviewWorkflowCommandResult::ExternalLinkReserved { link } => {
            encoded.kind = "external_link_reserved";
            encoded.link = Some(link.into_uuid());
        }
        ReviewWorkflowCommandResult::ExternalLinkAttached {
            link,
            external_object,
        } => {
            encoded.kind = "external_link_attached";
            encoded.link = Some(link.into_uuid());
            encoded.external_object = Some(external_object.as_str().to_owned());
        }
    }
    encoded
}

fn decode_result(
    row: &sqlx::postgres::PgRow,
) -> Result<ReviewWorkflowCommandResult, ReviewWorkflowStoreError> {
    let kind: String = row.try_get("result_kind")?;
    match kind.as_str() {
        "target_created" => Ok(ReviewWorkflowCommandResult::TargetCreated {
            target: super::review_workflow::target_id(row.try_get("result_target_id")?),
        }),
        "run_started" => Ok(ReviewWorkflowCommandResult::RunStarted {
            run: super::review_workflow::run_id(row.try_get("result_run_id")?),
            pass: super::review_workflow::pass_id(row.try_get("result_pass_id")?),
        }),
        "findings_recorded" => Ok(ReviewWorkflowCommandResult::FindingsRecorded {
            run: super::review_workflow::run_id(row.try_get("result_run_id")?),
            pass: super::review_workflow::pass_id(row.try_get("result_pass_id")?),
            finding_count: usize::try_from(row.try_get::<i64, _>("result_finding_count")?)
                .map_err(|_| command_conflict("recorded finding count is not representable"))?,
        }),
        "finding_event_recorded" => Ok(ReviewWorkflowCommandResult::FindingEventRecorded {
            finding: super::review_workflow::finding_id(row.try_get("result_finding_id")?),
            status: decode_finding_status(row.try_get("result_finding_status")?)?,
        }),
        "external_link_reserved" => Ok(ReviewWorkflowCommandResult::ExternalLinkReserved {
            link: super::review_workflow::external_link_id(row.try_get("result_external_link_id")?),
        }),
        "external_link_attached" => Ok(ReviewWorkflowCommandResult::ExternalLinkAttached {
            link: super::review_workflow::external_link_id(row.try_get("result_external_link_id")?),
            external_object: ReviewKey::try_new(row.try_get("result_external_object_key")?)
                .map_err(|_| command_conflict("recorded external object is invalid"))?,
        }),
        "pass_activated" => Ok(ReviewWorkflowCommandResult::PassActivated {
            run: super::review_workflow::run_id(row.try_get("result_run_id")?),
            pass: super::review_workflow::pass_id(row.try_get("result_pass_id")?),
        }),
        "pass_completed" => Ok(ReviewWorkflowCommandResult::PassCompleted {
            run: super::review_workflow::run_id(row.try_get("result_run_id")?),
            pass: super::review_workflow::pass_id(row.try_get("result_pass_id")?),
            status: decode_completion_status(row.try_get("result_finding_status")?)?,
        }),
        _ => Err(command_conflict(
            "review command receipt has an unsupported result",
        )),
    }
}

const fn encode_completion_status(status: ReviewPassCompletionStatus) -> &'static str {
    match status {
        ReviewPassCompletionStatus::Succeeded => "succeeded",
        ReviewPassCompletionStatus::Failed => "failed",
        ReviewPassCompletionStatus::Blocked => "blocked",
        ReviewPassCompletionStatus::Cancelled => "cancelled",
    }
}

fn decode_completion_status(
    status: String,
) -> Result<ReviewPassCompletionStatus, ReviewWorkflowStoreError> {
    match status.as_str() {
        "succeeded" => Ok(ReviewPassCompletionStatus::Succeeded),
        "failed" => Ok(ReviewPassCompletionStatus::Failed),
        "blocked" => Ok(ReviewPassCompletionStatus::Blocked),
        "cancelled" => Ok(ReviewPassCompletionStatus::Cancelled),
        _ => Err(command_conflict(
            "recorded pass completion status is invalid",
        )),
    }
}

const fn encode_finding_status(status: ReviewFindingStatus) -> &'static str {
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

fn decode_finding_status(status: String) -> Result<ReviewFindingStatus, ReviewWorkflowStoreError> {
    match status.as_str() {
        "open" => Ok(ReviewFindingStatus::Open),
        "accepted" => Ok(ReviewFindingStatus::Accepted),
        "rejected" => Ok(ReviewFindingStatus::Rejected),
        "duplicate" => Ok(ReviewFindingStatus::Duplicate),
        "superseded" => Ok(ReviewFindingStatus::Superseded),
        "stale" => Ok(ReviewFindingStatus::Stale),
        "posted" => Ok(ReviewFindingStatus::Posted),
        "fixed" => Ok(ReviewFindingStatus::Fixed),
        "blocked_with_reason" => Ok(ReviewFindingStatus::BlockedWithReason),
        _ => Err(command_conflict("recorded finding status is invalid")),
    }
}

fn registry_error(error: RegistryInspectionError) -> ReviewWorkflowStoreError {
    match error {
        RegistryInspectionError::Database(error) => ReviewWorkflowStoreError::Database(error),
        RegistryInspectionError::Corruption(error) => {
            command_conflict(&format!("durable command registry is corrupt: {error:?}"))
        }
    }
}

fn post_effect_verification_error(error: ReviewWorkflowStoreError) -> ReviewWorkflowStoreError {
    match error {
        ReviewWorkflowStoreError::Database(error) => {
            ReviewWorkflowStoreError::CommitAmbiguous(error)
        }
        error => error,
    }
}

fn command_conflict(detail: &str) -> ReviewWorkflowStoreError {
    super::review_workflow::corruption("review_workflow_command", detail.to_owned())
}

async fn commit_claim(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), ReviewWorkflowStoreError> {
    transaction
        .commit()
        .await
        .map_err(ReviewWorkflowStoreError::CommitAmbiguous)
}

#[cfg(test)]
mod tests {
    use super::{ReviewWorkflowStoreError, post_effect_verification_error};

    #[test]
    fn post_effect_database_failure_is_commit_ambiguous() {
        assert_commit_ambiguous(post_effect_verification_error(
            ReviewWorkflowStoreError::Database(sqlx::Error::PoolClosed),
        ));
    }

    #[track_caller]
    fn assert_commit_ambiguous(error: ReviewWorkflowStoreError) {
        let ReviewWorkflowStoreError::CommitAmbiguous(source) = error else {
            panic!("expected commit ambiguity, got {error}");
        };
        assert!(matches!(source, sqlx::Error::PoolClosed));
    }
}

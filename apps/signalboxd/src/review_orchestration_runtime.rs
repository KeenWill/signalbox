//! Process-boundary adapter for client-driven review orchestration.

use std::future::Future;

use signalbox_application::{
    ReviewConcernClaim, ReviewConcernOutcome, ReviewConcernSuccess, ReviewConcernWork,
    ReviewImportOutcome, ReviewImportedContextEvidence, ReviewJudgmentEffectId,
    ReviewJudgmentEffectOutcome, ReviewJudgmentEffectSuccess, ReviewJudgmentEffectWork,
    ReviewJudgmentPlan, ReviewJudgmentPlanMember as ApplicationPlanMember,
    ReviewOrchestrationAttempt, ReviewOrchestrationAttemptId, ReviewOrchestrationAttemptStore,
    ReviewOrchestrationPassRunner, ReviewOrchestrationService, ReviewOrchestrationServiceError,
    ReviewPassIncompleteStatus, ReviewPlannedDisposition, ReviewPublicationMemberOutcome,
    ReviewPublicationSuccess, ReviewPublicationWork, ReviewRepairMemberOutcome,
    ReviewRepairSuccess, ReviewRepairWork, ReviewTemplateDigest,
};
use signalbox_domain::{
    DurableCommandId, ReviewExternalLinkId, ReviewFinding, ReviewFindingEvent, ReviewFindingId,
    ReviewFindingRef, ReviewKey, ReviewPass, ReviewPassEvidence, ReviewPassId, ReviewPassKind,
    ReviewPassState, ReviewRunEvidence, ReviewRunState, ReviewTargetId, ReviewText,
    ReviewWorkflowKind, SessionId, SessionTemplateName,
};
use signalbox_persistence::{
    review_orchestration::{
        PostgresReviewOrchestrationStore, ReviewOrchestrationCommand,
        ReviewOrchestrationCommandClaim, ReviewOrchestrationCommandKind,
        ReviewOrchestrationCommandResult, ReviewOrchestrationCurrentStage,
        ReviewOrchestrationSnapshotFacts, ReviewOrchestrationStage, ReviewOrchestrationStoreError,
    },
    review_workflow::{ReviewWorkflowStore, ReviewWorkflowStoreError},
    session::{SessionRepository, SessionRepositoryError},
};
use signalbox_process_protocol::{
    CanonicalDigest, CanonicalU64, CanonicalUuid, ClientRequest, ReviewConcernTerminalOutcome,
    ReviewImportTerminalOutcome, ReviewJudgmentDisposition, ReviewJudgmentEffectTerminalOutcome,
    ReviewOrchestrationConcernSnapshot, ReviewOrchestrationConcernStatus,
    ReviewOrchestrationCounts, ReviewOrchestrationSnapshot,
    ReviewOrchestrationStageTemplateDigests as WireStageDigests, ReviewOrchestrationState,
    ReviewPublicationOutcome as WirePublicationOutcome, ReviewPublicationTerminalOutcome,
    ReviewRepairOutcome as WireRepairOutcome, ReviewRepairTerminalOutcome, ServerMessage,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::session_template_configuration::{
    REVIEW_CONCERNS, REVIEW_IMPORT_TEMPLATE_NAME, REVIEW_JUDGMENT_TEMPLATE_NAME,
    REVIEW_PUBLICATION_TEMPLATE_NAME, REVIEW_REPAIR_TEMPLATE_NAME, ReviewConcernTemplateSelection,
    ReviewLibrarySelection, ReviewStageTemplateSelection, SessionTemplateConfiguration,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReviewOrchestrationRuntimeError {
    InvalidRequest,
    NotFound,
    ConflictingReuse,
    Rejected,
    Unavailable {
        commit_ambiguous: bool,
    },
    Internal {
        session_id: Option<Uuid>,
        cause: ReviewOrchestrationInternalCause,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReviewOrchestrationInternalCause {
    StoreCorruption,
    WorkflowCorruption,
    SessionCorruption,
    ServiceContract,
}

pub(crate) async fn execute_review_orchestration_request(
    request: ClientRequest,
    semantic_digest: [u8; 32],
    pool: PgPool,
    templates: &SessionTemplateConfiguration,
) -> Result<ServerMessage, ReviewOrchestrationRuntimeError> {
    let (command_id, attempt_id, kind) = mutation_identity(&request)?;
    let store = PostgresReviewOrchestrationStore::new(pool.clone());
    let command = ReviewOrchestrationCommand {
        command_id,
        semantic_digest,
        attempt: attempt_id,
        kind,
    };
    let claim = store
        .begin_command(command)
        .await
        .map_err(map_store_error)?;
    let guard = match claim {
        ReviewOrchestrationCommandClaim::ExistingRecorded(result) => {
            return Ok(message_from_recorded(result));
        }
        ReviewOrchestrationCommandClaim::Conflicting => {
            return Err(ReviewOrchestrationRuntimeError::ConflictingReuse);
        }
        ReviewOrchestrationCommandClaim::New(guard) => guard,
    };

    let prepared = prepare_mutation(request, &pool, templates).await?;
    if prepared.command_id != command_id
        || prepared.attempt.id() != attempt_id
        || prepared.kind != kind
    {
        return Err(internal(ReviewOrchestrationInternalCause::ServiceContract));
    }
    if let Some(current) = prepared.current {
        prevalidate_submission(&store, &prepared.attempt, current, &prepared.submission).await?;
    }
    let mut service = ReviewOrchestrationService::new(store.clone(), prepared.submission.clone());
    match service.execute(prepared.attempt.clone()).await {
        Ok(_) | Err(ReviewOrchestrationServiceError::Runner(RunnerError::AwaitingInput)) => {}
        Err(error) => return Err(map_service_error(error)),
    }
    let stage = submission_stage(
        &store,
        &prepared.attempt,
        &prepared.submission,
        map_post_effect_store_error,
    )
    .await?;
    let progress = store
        .load_progress(prepared.attempt.id())
        .await
        .map_err(map_post_effect_store_error)?
        .ok_or(internal(ReviewOrchestrationInternalCause::StoreCorruption))?;
    let recovered = store
        .record_command_recovery(
            command,
            ReviewOrchestrationCommandResult {
                attempt: prepared.attempt.id(),
                stage,
                progress,
            },
        )
        .await
        .map_err(map_post_effect_store_error)?;
    let result = guard
        .record(recovered)
        .await
        .map_err(map_post_effect_store_error)?;
    Ok(message_from_recorded(result))
}

pub(crate) async fn read_review_orchestration_request(
    request: ClientRequest,
    _semantic_digest: [u8; 32],
    pool: PgPool,
    _templates: &SessionTemplateConfiguration,
) -> Result<ServerMessage, ReviewOrchestrationRuntimeError> {
    let ClientRequest::ReadReviewOrchestration { attempt_id } = request else {
        return Err(ReviewOrchestrationRuntimeError::InvalidRequest);
    };
    let store = PostgresReviewOrchestrationStore::new(pool);
    let facts = store
        .load_snapshot(ReviewOrchestrationAttemptId::from_uuid(
            attempt_id.into_uuid(),
        ))
        .await
        .map_err(map_store_error)?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)?;
    Ok(ServerMessage::ReviewOrchestration {
        snapshot: build_snapshot(&facts)?,
    })
}

struct PreparedMutation {
    command_id: DurableCommandId,
    kind: ReviewOrchestrationCommandKind,
    attempt: ReviewOrchestrationAttempt,
    submission: ClientSubmission,
    current: Option<ReviewOrchestrationCurrentStage>,
}

#[derive(Clone)]
enum ClientSubmission {
    AwaitingImport,
    Import(ReviewImportOutcome),
    Concern {
        concern: ReviewKey,
        outcome: ReviewConcernOutcome,
    },
    JudgmentPlan(ReviewJudgmentPlan),
    JudgmentEffect {
        finding: ReviewFindingRef,
        outcome: ReviewJudgmentEffectOutcome,
    },
    Repair(Vec<ReviewRepairMemberOutcome>),
    Publication(Vec<ReviewPublicationMemberOutcome>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunnerError {
    AwaitingInput,
}

impl ReviewOrchestrationPassRunner for ClientSubmission {
    type Error = RunnerError;

    fn import_external_context(
        &self,
        _attempt: ReviewOrchestrationAttempt,
    ) -> impl Future<Output = Result<ReviewImportOutcome, Self::Error>> + Send {
        let submission = self.clone();
        async move {
            if let Self::Import(outcome) = submission {
                Ok(outcome)
            } else {
                Err(RunnerError::AwaitingInput)
            }
        }
    }

    fn run_concern(
        &self,
        work: ReviewConcernWork,
    ) -> impl Future<Output = Result<ReviewConcernOutcome, Self::Error>> + Send + 'static {
        let submission = self.clone();
        async move {
            if let Self::Concern { concern, outcome } = submission
                && &concern == work.concern().key()
            {
                return Ok(outcome);
            }
            Err(RunnerError::AwaitingInput)
        }
    }

    fn judge(
        &self,
        _attempt: ReviewOrchestrationAttempt,
        _findings: Vec<ReviewFinding>,
    ) -> impl Future<Output = Result<ReviewJudgmentPlan, Self::Error>> + Send {
        let submission = self.clone();
        async move {
            if let Self::JudgmentPlan(plan) = submission {
                Ok(plan)
            } else {
                Err(RunnerError::AwaitingInput)
            }
        }
    }

    fn apply_judgment_effect(
        &self,
        work: ReviewJudgmentEffectWork,
    ) -> impl Future<Output = Result<ReviewJudgmentEffectOutcome, Self::Error>> + Send {
        let submission = self.clone();
        async move {
            if let Self::JudgmentEffect { finding, outcome } = submission
                && finding == work.id().finding()
            {
                return Ok(outcome);
            }
            Err(RunnerError::AwaitingInput)
        }
    }

    fn repair(
        &self,
        _work: ReviewRepairWork,
    ) -> impl Future<Output = Result<Vec<ReviewRepairMemberOutcome>, Self::Error>> + Send {
        let submission = self.clone();
        async move {
            if let Self::Repair(outcomes) = submission {
                Ok(outcomes)
            } else {
                Err(RunnerError::AwaitingInput)
            }
        }
    }

    fn publish(
        &self,
        _work: ReviewPublicationWork,
    ) -> impl Future<Output = Result<Vec<ReviewPublicationMemberOutcome>, Self::Error>> + Send {
        let submission = self.clone();
        async move {
            if let Self::Publication(outcomes) = submission {
                Ok(outcomes)
            } else {
                Err(RunnerError::AwaitingInput)
            }
        }
    }
}

async fn prepare_mutation(
    request: ClientRequest,
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
) -> Result<PreparedMutation, ReviewOrchestrationRuntimeError> {
    if let ClientRequest::StartReviewOrchestration {
        command_id,
        attempt_id,
        target_id,
        concern_set_version,
        import_template_name,
        judgment_template_name,
        repair_template_name,
        publication_template_name,
        concerns,
    } = request
    {
        let target_id = ReviewTargetId::from_uuid(target_id.into_uuid());
        authenticate_target(pool, target_id).await?;
        let selection = ReviewLibrarySelection {
            concern_set_version: review_key(concern_set_version)?,
            stages: ReviewStageTemplateSelection {
                import: template_name(import_template_name)?,
                judgment: template_name(judgment_template_name)?,
                repair: template_name(repair_template_name)?,
                publication: template_name(publication_template_name)?,
            },
            concerns: concerns
                .into_iter()
                .map(|concern| {
                    Ok(ReviewConcernTemplateSelection {
                        key: review_key(concern.key)?,
                        template: template_name(concern.template_name)?,
                    })
                })
                .collect::<Result<_, ReviewOrchestrationRuntimeError>>()?,
        };
        let attempt = templates
            .resolve_review_attempt(
                ReviewOrchestrationAttemptId::from_uuid(attempt_id.into_uuid()),
                target_id,
                &selection,
            )
            .map_err(|_| ReviewOrchestrationRuntimeError::InvalidRequest)?;
        let current = PostgresReviewOrchestrationStore::new(pool.clone())
            .current_stage(attempt.id())
            .await
            .map_err(map_store_error)?;
        return Ok(PreparedMutation {
            command_id: DurableCommandId::from_uuid(command_id.into_uuid()),
            kind: ReviewOrchestrationCommandKind::Start,
            attempt,
            submission: ClientSubmission::AwaitingImport,
            current,
        });
    }

    let (command_id, attempt_id, kind) = mutation_identity(&request)?;
    let store = PostgresReviewOrchestrationStore::new(pool.clone());
    let attempt = store
        .load_attempt(attempt_id)
        .await
        .map_err(map_store_error)?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)?;
    authenticate_frozen_attempt_templates(templates, &attempt)?;
    let current = store
        .current_stage(attempt_id)
        .await
        .map_err(map_store_error)?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)?;
    let submission = build_submission(request, pool, templates, &attempt).await?;
    Ok(PreparedMutation {
        command_id,
        kind,
        attempt,
        submission,
        current: Some(current),
    })
}

fn mutation_identity(
    request: &ClientRequest,
) -> Result<
    (
        DurableCommandId,
        ReviewOrchestrationAttemptId,
        ReviewOrchestrationCommandKind,
    ),
    ReviewOrchestrationRuntimeError,
> {
    let (command, attempt, kind) = match request {
        ClientRequest::StartReviewOrchestration {
            command_id,
            attempt_id,
            ..
        } => (
            *command_id,
            *attempt_id,
            ReviewOrchestrationCommandKind::Start,
        ),
        ClientRequest::RecordReviewImportOutcome {
            command_id,
            attempt_id,
            ..
        } => (
            *command_id,
            *attempt_id,
            ReviewOrchestrationCommandKind::Import,
        ),
        ClientRequest::RecordReviewConcernOutcome {
            command_id,
            attempt_id,
            ..
        } => (
            *command_id,
            *attempt_id,
            ReviewOrchestrationCommandKind::Concern,
        ),
        ClientRequest::RecordReviewJudgmentPlan {
            command_id,
            attempt_id,
            ..
        } => (
            *command_id,
            *attempt_id,
            ReviewOrchestrationCommandKind::JudgmentPlan,
        ),
        ClientRequest::RecordReviewJudgmentEffect {
            command_id,
            attempt_id,
            ..
        } => (
            *command_id,
            *attempt_id,
            ReviewOrchestrationCommandKind::JudgmentEffect,
        ),
        ClientRequest::RecordReviewRepairOutcomes {
            command_id,
            attempt_id,
            ..
        } => (
            *command_id,
            *attempt_id,
            ReviewOrchestrationCommandKind::Repair,
        ),
        ClientRequest::RecordReviewPublicationOutcomes {
            command_id,
            attempt_id,
            ..
        } => (
            *command_id,
            *attempt_id,
            ReviewOrchestrationCommandKind::Publication,
        ),
        _ => return Err(ReviewOrchestrationRuntimeError::InvalidRequest),
    };
    Ok((
        DurableCommandId::from_uuid(command.into_uuid()),
        ReviewOrchestrationAttemptId::from_uuid(attempt.into_uuid()),
        kind,
    ))
}

async fn prevalidate_submission(
    store: &PostgresReviewOrchestrationStore,
    attempt: &ReviewOrchestrationAttempt,
    current: ReviewOrchestrationCurrentStage,
    submission: &ClientSubmission,
) -> Result<(), ReviewOrchestrationRuntimeError> {
    let admitted = match submission {
        ClientSubmission::AwaitingImport => {
            current == ReviewOrchestrationCurrentStage::AwaitingImport
        }
        ClientSubmission::Import(outcome) => {
            current == ReviewOrchestrationCurrentStage::AwaitingImport
                || store
                    .load_import(attempt.id())
                    .await
                    .map_err(map_store_error)?
                    .as_ref()
                    == Some(outcome)
                    && recorded_submission_is_current(store, attempt, current, submission).await?
        }
        ClientSubmission::Concern { concern, outcome } => {
            let fresh_stage = matches!(
                current,
                ReviewOrchestrationCurrentStage::AwaitingConcerns
                    | ReviewOrchestrationCurrentStage::FanoutIncomplete
            );
            let spec = attempt
                .concerns()
                .iter()
                .find(|expected| expected.key() == concern)
                .ok_or(ReviewOrchestrationRuntimeError::Rejected)?;
            let candidate =
                ReviewConcernClaim::new(concern.clone(), spec.template_digest(), outcome.clone());
            let claims = store
                .load_concern_claims(attempt.id())
                .await
                .map_err(map_store_error)?;
            match claims.iter().find(|claim| claim.concern() == concern) {
                Some(existing) if existing == &candidate => {
                    recorded_submission_is_current(store, attempt, current, submission).await?
                }
                Some(existing) => {
                    fresh_stage && matches!(existing.outcome(), ReviewConcernOutcome::Failed { .. })
                }
                None => fresh_stage,
            }
        }
        ClientSubmission::JudgmentPlan(plan) => {
            current == ReviewOrchestrationCurrentStage::AwaitingJudgment
                || store
                    .load_judgment_plan(attempt.id())
                    .await
                    .map_err(map_store_error)?
                    .as_ref()
                    == Some(plan)
                    && recorded_submission_is_current(store, attempt, current, submission).await?
        }
        ClientSubmission::JudgmentEffect { finding, outcome } => {
            let plan = store
                .load_judgment_plan(attempt.id())
                .await
                .map_err(map_store_error)?
                .ok_or(ReviewOrchestrationRuntimeError::Rejected)?;
            let applied = store
                .load_applied_judgment_effects(attempt.id())
                .await
                .map_err(map_store_error)?;
            if applied.len() > plan.members().len()
                || !applied.iter().zip(plan.members()).all(|(actual, member)| {
                    actual.attempt() == attempt.id() && actual.finding() == member.finding()
                })
            {
                return Err(internal(ReviewOrchestrationInternalCause::StoreCorruption));
            }
            let effect = ReviewJudgmentEffectId::new(attempt.id(), *finding);
            if applied.contains(&effect) {
                matches!(outcome, ReviewJudgmentEffectOutcome::Applied(_))
                    && recorded_submission_is_current(store, attempt, current, submission).await?
            } else {
                matches!(
                    current,
                    ReviewOrchestrationCurrentStage::AwaitingJudgmentEffects
                        | ReviewOrchestrationCurrentStage::JudgmentIncomplete
                ) && plan
                    .members()
                    .get(applied.len())
                    .is_some_and(|member| member.finding() == *finding)
            }
        }
        ClientSubmission::Repair(outcomes) => {
            current == ReviewOrchestrationCurrentStage::AwaitingRepair
                || store
                    .load_repair_outcomes(attempt.id())
                    .await
                    .map_err(map_store_error)?
                    .as_ref()
                    == Some(outcomes)
                    && recorded_submission_is_current(store, attempt, current, submission).await?
        }
        ClientSubmission::Publication(outcomes) => {
            current == ReviewOrchestrationCurrentStage::AwaitingPublication
                || store
                    .load_publication_outcomes(attempt.id())
                    .await
                    .map_err(map_store_error)?
                    .as_ref()
                    == Some(outcomes)
                    && recorded_submission_is_current(store, attempt, current, submission).await?
        }
    };
    admitted
        .then_some(())
        .ok_or(ReviewOrchestrationRuntimeError::Rejected)
}

async fn recorded_submission_is_current(
    store: &PostgresReviewOrchestrationStore,
    attempt: &ReviewOrchestrationAttempt,
    current: ReviewOrchestrationCurrentStage,
    submission: &ClientSubmission,
) -> Result<bool, ReviewOrchestrationRuntimeError> {
    let recorded = submission_stage(store, attempt, submission, map_store_error).await?;
    Ok(recorded_stage_is_current(current, recorded))
}

fn recorded_stage_is_current(
    current: ReviewOrchestrationCurrentStage,
    recorded: ReviewOrchestrationStage,
) -> bool {
    current_to_stage(current) == recorded
}

async fn submission_stage(
    store: &PostgresReviewOrchestrationStore,
    attempt: &ReviewOrchestrationAttempt,
    submission: &ClientSubmission,
    map_error: fn(ReviewOrchestrationStoreError) -> ReviewOrchestrationRuntimeError,
) -> Result<ReviewOrchestrationStage, ReviewOrchestrationRuntimeError> {
    match submission {
        ClientSubmission::AwaitingImport => Ok(ReviewOrchestrationStage::Started),
        ClientSubmission::Import(ReviewImportOutcome::Succeeded { .. }) => {
            Ok(ReviewOrchestrationStage::AwaitingConcerns)
        }
        ClientSubmission::Import(ReviewImportOutcome::Incomplete { .. }) => {
            Ok(ReviewOrchestrationStage::ImportIncomplete)
        }
        ClientSubmission::Concern { .. } => {
            let claims = store
                .load_concern_claims(attempt.id())
                .await
                .map_err(map_error)?;
            if claims.len() < attempt.concerns().len() {
                return Ok(ReviewOrchestrationStage::AwaitingConcerns);
            }
            if claims.len() != attempt.concerns().len() {
                return Err(internal(ReviewOrchestrationInternalCause::StoreCorruption));
            }
            if claims
                .iter()
                .all(|claim| matches!(claim.outcome(), ReviewConcernOutcome::Succeeded(_)))
            {
                Ok(ReviewOrchestrationStage::AwaitingJudgment)
            } else {
                Ok(ReviewOrchestrationStage::FanoutIncomplete)
            }
        }
        ClientSubmission::JudgmentPlan(plan) => Ok(if plan.members().is_empty() {
            ReviewOrchestrationStage::AwaitingRepair
        } else {
            ReviewOrchestrationStage::AwaitingJudgmentEffects
        }),
        ClientSubmission::JudgmentEffect { finding, outcome } => {
            if !matches!(outcome, ReviewJudgmentEffectOutcome::Applied(_)) {
                return Ok(ReviewOrchestrationStage::JudgmentIncomplete);
            }
            let plan = store
                .load_judgment_plan(attempt.id())
                .await
                .map_err(map_error)?
                .ok_or(internal(ReviewOrchestrationInternalCause::StoreCorruption))?;
            let ordinal = plan
                .members()
                .iter()
                .position(|member| member.finding() == *finding)
                .ok_or(internal(ReviewOrchestrationInternalCause::StoreCorruption))?;
            Ok(if ordinal + 1 == plan.members().len() {
                ReviewOrchestrationStage::AwaitingRepair
            } else {
                ReviewOrchestrationStage::AwaitingJudgmentEffects
            })
        }
        ClientSubmission::Repair(outcomes) => Ok(
            if outcomes
                .iter()
                .any(|outcome| matches!(outcome, ReviewRepairMemberOutcome::Blocked(_)))
            {
                ReviewOrchestrationStage::RepairIncomplete
            } else {
                ReviewOrchestrationStage::AwaitingPublication
            },
        ),
        ClientSubmission::Publication(outcomes) => Ok(
            if outcomes
                .iter()
                .all(|outcome| matches!(outcome, ReviewPublicationMemberOutcome::Published(_)))
            {
                ReviewOrchestrationStage::Complete
            } else {
                ReviewOrchestrationStage::PublicationIncomplete
            },
        ),
    }
}

fn authenticate_frozen_attempt_templates(
    templates: &SessionTemplateConfiguration,
    attempt: &ReviewOrchestrationAttempt,
) -> Result<(), ReviewOrchestrationRuntimeError> {
    let selection = ReviewLibrarySelection {
        concern_set_version: attempt.concern_set_version().clone(),
        stages: ReviewStageTemplateSelection {
            import: configured_template_name(REVIEW_IMPORT_TEMPLATE_NAME)?,
            judgment: configured_template_name(REVIEW_JUDGMENT_TEMPLATE_NAME)?,
            repair: configured_template_name(REVIEW_REPAIR_TEMPLATE_NAME)?,
            publication: configured_template_name(REVIEW_PUBLICATION_TEMPLATE_NAME)?,
        },
        concerns: REVIEW_CONCERNS
            .iter()
            .map(|(key, template)| {
                Ok(ReviewConcernTemplateSelection {
                    key: ReviewKey::try_new((*key).to_owned())
                        .map_err(|_| internal(ReviewOrchestrationInternalCause::ServiceContract))?,
                    template: configured_template_name(template)?,
                })
            })
            .collect::<Result<_, ReviewOrchestrationRuntimeError>>()?,
    };
    let current = templates
        .resolve_review_attempt(attempt.id(), attempt.target(), &selection)
        .map_err(|_| ReviewOrchestrationRuntimeError::Rejected)?;
    if current.stage_templates() != attempt.stage_templates()
        || current.concerns() != attempt.concerns()
    {
        return Err(ReviewOrchestrationRuntimeError::Rejected);
    }
    Ok(())
}

fn configured_template_name(
    value: &str,
) -> Result<SessionTemplateName, ReviewOrchestrationRuntimeError> {
    SessionTemplateName::try_new(value.to_owned())
        .map_err(|_| internal(ReviewOrchestrationInternalCause::ServiceContract))
}

async fn build_submission(
    request: ClientRequest,
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
    attempt: &ReviewOrchestrationAttempt,
) -> Result<ClientSubmission, ReviewOrchestrationRuntimeError> {
    match request {
        ClientRequest::RecordReviewImportOutcome {
            pass_id,
            external_link_id,
            context_digest,
            outcome,
            ..
        } => {
            let template_digest = attempt.stage_templates().import();
            let value = if outcome == ReviewImportTerminalOutcome::Succeeded {
                let (pass, run) = authenticate_pass(
                    pool,
                    templates,
                    required_pass(pass_id)?,
                    REVIEW_IMPORT_TEMPLATE_NAME,
                )
                .await?;
                ReviewImportOutcome::Succeeded {
                    context: ReviewImportedContextEvidence::new(
                        pass.reference(),
                        digest_bytes(
                            context_digest
                                .ok_or(ReviewOrchestrationRuntimeError::InvalidRequest)?,
                        )?,
                    ),
                    pass: Box::new(pass),
                    run,
                    external_link: match external_link_id {
                        Some(id) => Some(Box::new(
                            load_external_link(
                                pool,
                                ReviewExternalLinkId::from_uuid(id.into_uuid()),
                            )
                            .await?,
                        )),
                        None => None,
                    },
                    template_digest,
                }
            } else {
                let evidence = match pass_id {
                    Some(id) => Some(
                        authenticate_pass(
                            pool,
                            templates,
                            ReviewPassId::from_uuid(id.into_uuid()),
                            REVIEW_IMPORT_TEMPLATE_NAME,
                        )
                        .await?,
                    ),
                    None => None,
                };
                ReviewImportOutcome::Incomplete {
                    pass: evidence.as_ref().map(|(pass, _)| Box::new(pass.clone())),
                    run: evidence.map(|(_, run)| run),
                    template_digest,
                    status: match outcome {
                        ReviewImportTerminalOutcome::Failed => ReviewPassIncompleteStatus::Failed,
                        ReviewImportTerminalOutcome::Blocked => ReviewPassIncompleteStatus::Blocked,
                        ReviewImportTerminalOutcome::Cancelled => {
                            ReviewPassIncompleteStatus::Cancelled
                        }
                        ReviewImportTerminalOutcome::Succeeded => {
                            return Err(ReviewOrchestrationRuntimeError::InvalidRequest);
                        }
                    },
                }
            };
            Ok(ClientSubmission::Import(value))
        }
        ClientRequest::RecordReviewConcernOutcome {
            concern,
            pass_id,
            outcome,
            ..
        } => {
            let concern = review_key(concern)?;
            let spec = attempt
                .concerns()
                .iter()
                .find(|expected| expected.key() == &concern)
                .ok_or(ReviewOrchestrationRuntimeError::Rejected)?;
            let template = configured_concern_template(concern.as_str())
                .ok_or(ReviewOrchestrationRuntimeError::Rejected)?;
            let pass = match pass_id {
                Some(id) => Some(
                    authenticate_pass(
                        pool,
                        templates,
                        ReviewPassId::from_uuid(id.into_uuid()),
                        template,
                    )
                    .await?,
                ),
                None => None,
            };
            if let Some((pass, run)) = &pass
                && outcome != ReviewConcernTerminalOutcome::Succeeded
            {
                validate_incomplete_concern_evidence(attempt, pass, *run, outcome)?;
            }
            let value = match outcome {
                ReviewConcernTerminalOutcome::Succeeded => {
                    let (pass, run) =
                        pass.ok_or(ReviewOrchestrationRuntimeError::InvalidRequest)?;
                    let findings = ReviewWorkflowStore::new(pool.clone())
                        .list_findings(pass.reference().run().run())
                        .await
                        .map_err(map_workflow_error)?;
                    ReviewConcernOutcome::Succeeded(Box::new(ReviewConcernSuccess::new(
                        pass,
                        run,
                        spec.template_digest(),
                        findings,
                    )))
                }
                ReviewConcernTerminalOutcome::Failed => ReviewConcernOutcome::Failed {
                    pass: pass
                        .ok_or(ReviewOrchestrationRuntimeError::InvalidRequest)?
                        .0
                        .reference(),
                },
                ReviewConcernTerminalOutcome::Blocked => ReviewConcernOutcome::Blocked {
                    pass: pass
                        .ok_or(ReviewOrchestrationRuntimeError::InvalidRequest)?
                        .0
                        .reference(),
                },
                ReviewConcernTerminalOutcome::Cancelled => ReviewConcernOutcome::Cancelled {
                    pass: pass.map(|(pass, _)| pass.reference()),
                },
            };
            Ok(ClientSubmission::Concern {
                concern,
                outcome: value,
            })
        }
        ClientRequest::RecordReviewJudgmentPlan {
            analysis_pass_id,
            members,
            ..
        } => {
            let (pass, run) = authenticate_pass(
                pool,
                templates,
                ReviewPassId::from_uuid(analysis_pass_id.into_uuid()),
                REVIEW_JUDGMENT_TEMPLATE_NAME,
            )
            .await?;
            let mut converted = Vec::with_capacity(members.len());
            for member in members {
                let finding = load_finding(
                    pool,
                    ReviewFindingId::from_uuid(member.finding_id.into_uuid()),
                )
                .await?;
                let disposition = match member.disposition {
                    ReviewJudgmentDisposition::Accepted {} => ReviewPlannedDisposition::Accepted,
                    ReviewJudgmentDisposition::Rejected { reason } => {
                        ReviewPlannedDisposition::Rejected {
                            reason: ReviewText::try_new(reason)
                                .map_err(|_| ReviewOrchestrationRuntimeError::InvalidRequest)?,
                        }
                    }
                    ReviewJudgmentDisposition::Duplicate {
                        canonical_finding_id,
                    } => ReviewPlannedDisposition::Duplicate {
                        canonical: load_finding(
                            pool,
                            ReviewFindingId::from_uuid(canonical_finding_id.into_uuid()),
                        )
                        .await?
                        .proposal()
                        .reference(),
                    },
                    ReviewJudgmentDisposition::Superseded {
                        successor_finding_id,
                    } => ReviewPlannedDisposition::Superseded {
                        successor: load_finding(
                            pool,
                            ReviewFindingId::from_uuid(successor_finding_id.into_uuid()),
                        )
                        .await?
                        .proposal()
                        .reference(),
                    },
                    ReviewJudgmentDisposition::Stale {} => ReviewPlannedDisposition::Stale,
                };
                converted.push(ApplicationPlanMember::new(
                    finding.proposal().reference(),
                    disposition,
                ));
            }
            Ok(ClientSubmission::JudgmentPlan(ReviewJudgmentPlan::new(
                pass,
                run,
                attempt.stage_templates().judgment(),
                converted,
            )))
        }
        ClientRequest::RecordReviewJudgmentEffect {
            finding_id,
            event_pass_id,
            outcome,
            ..
        } => {
            let finding =
                load_finding(pool, ReviewFindingId::from_uuid(finding_id.into_uuid())).await?;
            let reference = finding.proposal().reference();
            let value = match outcome {
                ReviewJudgmentEffectTerminalOutcome::Applied => {
                    let event = find_event(
                        &finding,
                        event_pass_id.ok_or(ReviewOrchestrationRuntimeError::InvalidRequest)?,
                    )?;
                    authenticate_event_template(
                        pool,
                        templates,
                        &event,
                        REVIEW_JUDGMENT_TEMPLATE_NAME,
                    )
                    .await?;
                    ReviewJudgmentEffectOutcome::Applied(Box::new(
                        ReviewJudgmentEffectSuccess::new(
                            event,
                            attempt.stage_templates().judgment(),
                        ),
                    ))
                }
                ReviewJudgmentEffectTerminalOutcome::Failed => ReviewJudgmentEffectOutcome::Failed,
                ReviewJudgmentEffectTerminalOutcome::Blocked => {
                    ReviewJudgmentEffectOutcome::Blocked
                }
                ReviewJudgmentEffectTerminalOutcome::Cancelled => {
                    ReviewJudgmentEffectOutcome::Cancelled
                }
            };
            Ok(ClientSubmission::JudgmentEffect {
                finding: reference,
                outcome: value,
            })
        }
        ClientRequest::RecordReviewRepairOutcomes { outcomes, .. } => {
            let mut converted = Vec::with_capacity(outcomes.len());
            for outcome in outcomes {
                converted.push(build_repair_outcome(pool, templates, attempt, outcome).await?);
            }
            Ok(ClientSubmission::Repair(converted))
        }
        ClientRequest::RecordReviewPublicationOutcomes { outcomes, .. } => {
            let mut converted = Vec::with_capacity(outcomes.len());
            for outcome in outcomes {
                converted.push(build_publication_outcome(pool, templates, attempt, outcome).await?);
            }
            Ok(ClientSubmission::Publication(converted))
        }
        _ => Err(ReviewOrchestrationRuntimeError::InvalidRequest),
    }
}

fn validate_incomplete_concern_evidence(
    attempt: &ReviewOrchestrationAttempt,
    pass: &ReviewPassEvidence,
    run: ReviewRunEvidence,
    outcome: ReviewConcernTerminalOutcome,
) -> Result<(), ReviewOrchestrationRuntimeError> {
    let pass_reference = pass.reference();
    let canonical_ancestry = pass_reference.target() == attempt.target()
        && run.reference().target() == attempt.target()
        && pass_reference.run() == run.reference()
        && pass.policy() == attempt.policy()
        && run.policy() == attempt.policy()
        && pass.kind() == ReviewPassKind::ReadOnlyReview
        && run.workflow() == ReviewWorkflowKind::ReadOnlyReview;
    let matching_terminal_state = match outcome {
        ReviewConcernTerminalOutcome::Failed => {
            matches!(pass.state(), ReviewPassState::Failed { .. })
                && run.state()
                    == (ReviewRunState::Failed {
                        failed_pass: pass_reference,
                    })
        }
        ReviewConcernTerminalOutcome::Blocked => {
            matches!(pass.state(), ReviewPassState::Blocked { .. })
                && run.state()
                    == (ReviewRunState::Blocked {
                        blocking_pass: pass_reference,
                    })
        }
        ReviewConcernTerminalOutcome::Cancelled => {
            matches!(pass.state(), ReviewPassState::Cancelled { .. })
                && run.state()
                    == (ReviewRunState::Cancelled {
                        last_pass: Some(pass_reference),
                    })
        }
        ReviewConcernTerminalOutcome::Succeeded => false,
    };
    if !canonical_ancestry || !matching_terminal_state {
        return Err(ReviewOrchestrationRuntimeError::Rejected);
    }
    Ok(())
}

async fn build_repair_outcome(
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
    attempt: &ReviewOrchestrationAttempt,
    outcome: WireRepairOutcome,
) -> Result<ReviewRepairMemberOutcome, ReviewOrchestrationRuntimeError> {
    let finding = load_finding(
        pool,
        ReviewFindingId::from_uuid(outcome.finding_id.into_uuid()),
    )
    .await?;
    let reference = finding.proposal().reference();
    Ok(match outcome.outcome {
        ReviewRepairTerminalOutcome::Fixed => {
            let event = find_event(
                &finding,
                outcome
                    .event_pass_id
                    .ok_or(ReviewOrchestrationRuntimeError::InvalidRequest)?,
            )?;
            authenticate_event_template(pool, templates, &event, REVIEW_REPAIR_TEMPLATE_NAME)
                .await?;
            ReviewRepairMemberOutcome::Fixed(Box::new(ReviewRepairSuccess::new(
                event,
                attempt.stage_templates().repair(),
            )))
        }
        ReviewRepairTerminalOutcome::Failed => ReviewRepairMemberOutcome::Failed(reference),
        ReviewRepairTerminalOutcome::Blocked => ReviewRepairMemberOutcome::Blocked(reference),
        ReviewRepairTerminalOutcome::Cancelled => ReviewRepairMemberOutcome::Cancelled(reference),
    })
}

async fn build_publication_outcome(
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
    attempt: &ReviewOrchestrationAttempt,
    outcome: WirePublicationOutcome,
) -> Result<ReviewPublicationMemberOutcome, ReviewOrchestrationRuntimeError> {
    let finding = load_finding(
        pool,
        ReviewFindingId::from_uuid(outcome.finding_id.into_uuid()),
    )
    .await?;
    let reference = finding.proposal().reference();
    Ok(match outcome.outcome {
        ReviewPublicationTerminalOutcome::Published => {
            let link = load_external_link(
                pool,
                ReviewExternalLinkId::from_uuid(
                    outcome
                        .external_link_id
                        .ok_or(ReviewOrchestrationRuntimeError::InvalidRequest)?
                        .into_uuid(),
                ),
            )
            .await?;
            let link_ref =
                signalbox_domain::ReviewFindingExternalLinkRef::try_new(reference, &link)
                    .map_err(|_| ReviewOrchestrationRuntimeError::Rejected)?;
            let attachment = link
                .attachment()
                .ok_or(ReviewOrchestrationRuntimeError::Rejected)?;
            authenticate_pass(
                pool,
                templates,
                attachment.pass().pass(),
                REVIEW_PUBLICATION_TEMPLATE_NAME,
            )
            .await?;
            ReviewPublicationMemberOutcome::Published(Box::new(ReviewPublicationSuccess::new(
                link_ref,
                attachment.run_evidence(),
                attempt.stage_templates().publication(),
            )))
        }
        ReviewPublicationTerminalOutcome::Failed => {
            ReviewPublicationMemberOutcome::Failed(reference)
        }
        ReviewPublicationTerminalOutcome::Blocked => {
            ReviewPublicationMemberOutcome::Blocked(reference)
        }
        ReviewPublicationTerminalOutcome::Cancelled => {
            ReviewPublicationMemberOutcome::Cancelled(reference)
        }
    })
}

async fn authenticate_target(
    pool: &PgPool,
    target: ReviewTargetId,
) -> Result<(), ReviewOrchestrationRuntimeError> {
    ReviewWorkflowStore::new(pool.clone())
        .load_target(target)
        .await
        .map_err(map_workflow_error)?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)?;
    Ok(())
}

async fn authenticate_pass(
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
    pass_id: ReviewPassId,
    template: &str,
) -> Result<(ReviewPassEvidence, ReviewRunEvidence), ReviewOrchestrationRuntimeError> {
    let workflow = ReviewWorkflowStore::new(pool.clone());
    let pass = workflow
        .load_pass(pass_id)
        .await
        .map_err(map_workflow_error)?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)?;
    let run = workflow
        .load_run(pass.reference().run().run())
        .await
        .map_err(map_workflow_error)?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)?;
    authenticate_session_template(pool, templates, &pass, template).await?;
    Ok((
        ReviewPassEvidence::from_pass(&pass, run.policy()),
        run.evidence(),
    ))
}

async fn authenticate_session_template(
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
    pass: &ReviewPass,
    template: &str,
) -> Result<(), ReviewOrchestrationRuntimeError> {
    let expected = templates
        .resolve(&template_name(template.to_owned())?)
        .ok_or(ReviewOrchestrationRuntimeError::Rejected)?;
    let session = SessionRepository::new(pool.clone())
        .load_session(pass.session())
        .await
        .map_err(|error| map_session_error(error, pass.session()))?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)?;
    if session.template_provenance() != Some(expected.provenance()) {
        return Err(ReviewOrchestrationRuntimeError::Rejected);
    }
    Ok(())
}

async fn authenticate_event_template(
    pool: &PgPool,
    templates: &SessionTemplateConfiguration,
    event: &ReviewFindingEvent,
    template: &str,
) -> Result<(), ReviewOrchestrationRuntimeError> {
    authenticate_pass(pool, templates, event.pass().pass(), template).await?;
    Ok(())
}

async fn load_finding(
    pool: &PgPool,
    id: ReviewFindingId,
) -> Result<ReviewFinding, ReviewOrchestrationRuntimeError> {
    ReviewWorkflowStore::new(pool.clone())
        .load_finding(id)
        .await
        .map_err(map_workflow_error)?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)
}

async fn load_external_link(
    pool: &PgPool,
    id: ReviewExternalLinkId,
) -> Result<signalbox_domain::ReviewExternalLink, ReviewOrchestrationRuntimeError> {
    ReviewWorkflowStore::new(pool.clone())
        .load_external_link(id)
        .await
        .map_err(map_workflow_error)?
        .ok_or(ReviewOrchestrationRuntimeError::NotFound)
}

fn find_event(
    finding: &ReviewFinding,
    pass: CanonicalUuid,
) -> Result<ReviewFindingEvent, ReviewOrchestrationRuntimeError> {
    let mut matches = finding
        .events()
        .iter()
        .filter(|event| event.pass().pass().into_uuid() == pass.into_uuid());
    let event = matches
        .next()
        .cloned()
        .ok_or(ReviewOrchestrationRuntimeError::Rejected)?;
    if matches.next().is_some() {
        return Err(internal(
            ReviewOrchestrationInternalCause::WorkflowCorruption,
        ));
    }
    Ok(event)
}

fn build_snapshot(
    facts: &ReviewOrchestrationSnapshotFacts,
) -> Result<ReviewOrchestrationSnapshot, ReviewOrchestrationRuntimeError> {
    let attempt = &facts.attempt;
    let current = facts.current_stage;
    let claims = &facts.concern_claims;
    let plan = &facts.judgment_plan;
    let effects = &facts.applied_judgment_effects;
    let repairs = &facts.repair_outcomes;
    let publications = &facts.publication_outcomes;
    let concerns = attempt
        .concerns()
        .iter()
        .map(|expected| {
            let claim = claims
                .iter()
                .find(|claim| claim.concern() == expected.key());
            Ok(ReviewOrchestrationConcernSnapshot {
                key: expected.key().as_str().to_owned(),
                template_digest: wire_digest(expected.template_digest())?,
                status: claim.map_or(ReviewOrchestrationConcernStatus::Pending, |claim| {
                    concern_status(claim.outcome())
                }),
                pass_id: claim.and_then(|claim| concern_pass(claim.outcome())),
            })
        })
        .collect::<Result<Vec<_>, ReviewOrchestrationRuntimeError>>()?;
    let finding_count = claims
        .iter()
        .map(|claim| match claim.outcome() {
            ReviewConcernOutcome::Succeeded(success) => success.findings().len(),
            _ => 0,
        })
        .sum();
    let repair_fixed_count = repairs.as_ref().map_or(0, |values| {
        values
            .iter()
            .filter(|value| matches!(value, ReviewRepairMemberOutcome::Fixed(_)))
            .count()
    });
    let publication_published_count = publications.as_ref().map_or(0, |values| {
        values
            .iter()
            .filter(|value| matches!(value, ReviewPublicationMemberOutcome::Published(_)))
            .count()
    });
    let digests = attempt.stage_templates();
    Ok(ReviewOrchestrationSnapshot {
        attempt_id: CanonicalUuid::from_uuid(attempt.id().as_uuid()),
        target_id: CanonicalUuid::from_uuid(attempt.target().into_uuid()),
        state: current_to_wire(current),
        concern_set_version: attempt.concern_set_version().as_str().to_owned(),
        stage_template_digests: WireStageDigests {
            import: wire_digest(digests.import())?,
            judgment: wire_digest(digests.judgment())?,
            repair: wire_digest(digests.repair())?,
            publication: wire_digest(digests.publication())?,
        },
        concerns,
        counts: ReviewOrchestrationCounts {
            finding_count: canonical_count(finding_count)?,
            judgment_member_count: canonical_count(
                plan.as_ref().map_or(0, |value| value.members().len()),
            )?,
            judgment_effect_applied_count: canonical_count(effects.len())?,
            repair_fixed_count: canonical_count(repair_fixed_count)?,
            publication_published_count: canonical_count(publication_published_count)?,
        },
    })
}

fn concern_status(outcome: &ReviewConcernOutcome) -> ReviewOrchestrationConcernStatus {
    match outcome {
        ReviewConcernOutcome::Succeeded(_) => ReviewOrchestrationConcernStatus::Succeeded,
        ReviewConcernOutcome::Failed { .. } => ReviewOrchestrationConcernStatus::Failed,
        ReviewConcernOutcome::Blocked { .. } => ReviewOrchestrationConcernStatus::Blocked,
        ReviewConcernOutcome::Cancelled { .. } => ReviewOrchestrationConcernStatus::Cancelled,
        ReviewConcernOutcome::Superseded { .. } => ReviewOrchestrationConcernStatus::Superseded,
    }
}

fn concern_pass(outcome: &ReviewConcernOutcome) -> Option<CanonicalUuid> {
    let pass = match outcome {
        ReviewConcernOutcome::Succeeded(success) => Some(success.producer()),
        ReviewConcernOutcome::Failed { pass }
        | ReviewConcernOutcome::Blocked { pass }
        | ReviewConcernOutcome::Superseded { pass } => Some(*pass),
        ReviewConcernOutcome::Cancelled { pass } => *pass,
    };
    pass.map(|value| CanonicalUuid::from_uuid(value.pass().into_uuid()))
}

fn message_from_recorded(result: ReviewOrchestrationCommandResult) -> ServerMessage {
    let attempt_id = CanonicalUuid::from_uuid(result.attempt.as_uuid());
    if result.stage == ReviewOrchestrationStage::Started {
        ServerMessage::ReviewOrchestrationStarted { attempt_id }
    } else {
        ServerMessage::ReviewOrchestrationAdvanced {
            attempt_id,
            state: stage_to_wire(result.stage),
        }
    }
}

fn stage_to_wire(stage: ReviewOrchestrationStage) -> ReviewOrchestrationState {
    match stage {
        ReviewOrchestrationStage::Started | ReviewOrchestrationStage::AwaitingImport => {
            ReviewOrchestrationState::AwaitingImport
        }
        ReviewOrchestrationStage::ImportIncomplete => ReviewOrchestrationState::ImportIncomplete,
        ReviewOrchestrationStage::AwaitingConcerns => ReviewOrchestrationState::AwaitingConcerns,
        ReviewOrchestrationStage::FanoutIncomplete => ReviewOrchestrationState::FanoutIncomplete,
        ReviewOrchestrationStage::AwaitingJudgment => ReviewOrchestrationState::AwaitingJudgment,
        ReviewOrchestrationStage::AwaitingJudgmentEffects => {
            ReviewOrchestrationState::AwaitingJudgmentEffects
        }
        ReviewOrchestrationStage::JudgmentIncomplete => {
            ReviewOrchestrationState::JudgmentIncomplete
        }
        ReviewOrchestrationStage::AwaitingRepair => ReviewOrchestrationState::AwaitingRepair,
        ReviewOrchestrationStage::RepairIncomplete => ReviewOrchestrationState::RepairIncomplete,
        ReviewOrchestrationStage::AwaitingPublication => {
            ReviewOrchestrationState::AwaitingPublication
        }
        ReviewOrchestrationStage::PublicationIncomplete => {
            ReviewOrchestrationState::PublicationIncomplete
        }
        ReviewOrchestrationStage::Complete => ReviewOrchestrationState::Complete,
    }
}

fn current_to_wire(stage: ReviewOrchestrationCurrentStage) -> ReviewOrchestrationState {
    stage_to_wire(current_to_stage(stage))
}

fn current_to_stage(stage: ReviewOrchestrationCurrentStage) -> ReviewOrchestrationStage {
    match stage {
        ReviewOrchestrationCurrentStage::AwaitingImport => ReviewOrchestrationStage::AwaitingImport,
        ReviewOrchestrationCurrentStage::ImportIncomplete => {
            ReviewOrchestrationStage::ImportIncomplete
        }
        ReviewOrchestrationCurrentStage::AwaitingConcerns => {
            ReviewOrchestrationStage::AwaitingConcerns
        }
        ReviewOrchestrationCurrentStage::FanoutIncomplete => {
            ReviewOrchestrationStage::FanoutIncomplete
        }
        ReviewOrchestrationCurrentStage::AwaitingJudgment => {
            ReviewOrchestrationStage::AwaitingJudgment
        }
        ReviewOrchestrationCurrentStage::AwaitingJudgmentEffects => {
            ReviewOrchestrationStage::AwaitingJudgmentEffects
        }
        ReviewOrchestrationCurrentStage::JudgmentIncomplete => {
            ReviewOrchestrationStage::JudgmentIncomplete
        }
        ReviewOrchestrationCurrentStage::AwaitingRepair => ReviewOrchestrationStage::AwaitingRepair,
        ReviewOrchestrationCurrentStage::RepairIncomplete => {
            ReviewOrchestrationStage::RepairIncomplete
        }
        ReviewOrchestrationCurrentStage::AwaitingPublication => {
            ReviewOrchestrationStage::AwaitingPublication
        }
        ReviewOrchestrationCurrentStage::PublicationIncomplete => {
            ReviewOrchestrationStage::PublicationIncomplete
        }
        ReviewOrchestrationCurrentStage::Complete => ReviewOrchestrationStage::Complete,
    }
}

fn canonical_count(value: usize) -> Result<CanonicalU64, ReviewOrchestrationRuntimeError> {
    u64::try_from(value)
        .map(CanonicalU64::new)
        .map_err(|_| internal(ReviewOrchestrationInternalCause::ServiceContract))
}

fn wire_digest(
    value: ReviewTemplateDigest,
) -> Result<CanonicalDigest, ReviewOrchestrationRuntimeError> {
    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(64);
    for byte in value.bytes() {
        encoded.push(char::from(LOWER_HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(LOWER_HEX[usize::from(byte & 0x0f)]));
    }
    CanonicalDigest::try_new(encoded)
        .map_err(|_| internal(ReviewOrchestrationInternalCause::ServiceContract))
}

fn digest_bytes(value: CanonicalDigest) -> Result<[u8; 32], ReviewOrchestrationRuntimeError> {
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_str().as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair)
            .map_err(|_| ReviewOrchestrationRuntimeError::InvalidRequest)?;
        bytes[index] = u8::from_str_radix(text, 16)
            .map_err(|_| ReviewOrchestrationRuntimeError::InvalidRequest)?;
    }
    Ok(bytes)
}

fn required_pass(
    value: Option<CanonicalUuid>,
) -> Result<ReviewPassId, ReviewOrchestrationRuntimeError> {
    value
        .map(|value| ReviewPassId::from_uuid(value.into_uuid()))
        .ok_or(ReviewOrchestrationRuntimeError::InvalidRequest)
}

fn review_key(value: String) -> Result<ReviewKey, ReviewOrchestrationRuntimeError> {
    ReviewKey::try_new(value).map_err(|_| ReviewOrchestrationRuntimeError::InvalidRequest)
}

fn template_name(value: String) -> Result<SessionTemplateName, ReviewOrchestrationRuntimeError> {
    SessionTemplateName::try_new(value).map_err(|_| ReviewOrchestrationRuntimeError::InvalidRequest)
}

fn configured_concern_template(concern: &str) -> Option<&'static str> {
    REVIEW_CONCERNS
        .iter()
        .find_map(|(key, template)| (*key == concern).then_some(*template))
}

fn map_store_error(error: ReviewOrchestrationStoreError) -> ReviewOrchestrationRuntimeError {
    match error {
        ReviewOrchestrationStoreError::Database(_)
        | ReviewOrchestrationStoreError::SnapshotAdmissionTimedOut => {
            ReviewOrchestrationRuntimeError::Unavailable {
                commit_ambiguous: false,
            }
        }
        ReviewOrchestrationStoreError::CommitAmbiguous(_) => {
            ReviewOrchestrationRuntimeError::Unavailable {
                commit_ambiguous: true,
            }
        }
        ReviewOrchestrationStoreError::Workflow(error) => map_workflow_error(error),
        ReviewOrchestrationStoreError::Corruption(_) => {
            internal(ReviewOrchestrationInternalCause::StoreCorruption)
        }
    }
}

fn map_post_effect_store_error(
    error: ReviewOrchestrationStoreError,
) -> ReviewOrchestrationRuntimeError {
    match error {
        ReviewOrchestrationStoreError::Database(_)
        | ReviewOrchestrationStoreError::CommitAmbiguous(_) => {
            ReviewOrchestrationRuntimeError::Unavailable {
                commit_ambiguous: true,
            }
        }
        ReviewOrchestrationStoreError::SnapshotAdmissionTimedOut => {
            ReviewOrchestrationRuntimeError::Unavailable {
                commit_ambiguous: false,
            }
        }
        ReviewOrchestrationStoreError::Workflow(error) => map_post_effect_workflow_error(error),
        ReviewOrchestrationStoreError::Corruption(_) => {
            internal(ReviewOrchestrationInternalCause::StoreCorruption)
        }
    }
}

fn map_post_effect_workflow_error(
    error: ReviewWorkflowStoreError,
) -> ReviewOrchestrationRuntimeError {
    match error {
        ReviewWorkflowStoreError::Database(_) | ReviewWorkflowStoreError::CommitAmbiguous(_) => {
            ReviewOrchestrationRuntimeError::Unavailable {
                commit_ambiguous: true,
            }
        }
        error => map_workflow_error(error),
    }
}

fn map_workflow_error(error: ReviewWorkflowStoreError) -> ReviewOrchestrationRuntimeError {
    match error {
        ReviewWorkflowStoreError::Database(_) => ReviewOrchestrationRuntimeError::Unavailable {
            commit_ambiguous: false,
        },
        ReviewWorkflowStoreError::CommitAmbiguous(_) => {
            ReviewOrchestrationRuntimeError::Unavailable {
                commit_ambiguous: true,
            }
        }
        ReviewWorkflowStoreError::Corruption(_) => {
            internal(ReviewOrchestrationInternalCause::WorkflowCorruption)
        }
        ReviewWorkflowStoreError::InvalidInsertion(_)
        | ReviewWorkflowStoreError::InvalidTransition(_)
        | ReviewWorkflowStoreError::NonAtomicPassResult
        | ReviewWorkflowStoreError::IncompleteFindingInventory
        | ReviewWorkflowStoreError::IncompletePublicationReconciliation
        | ReviewWorkflowStoreError::ReservationConflict(_) => {
            internal(ReviewOrchestrationInternalCause::ServiceContract)
        }
    }
}

fn map_session_error(
    error: SessionRepositoryError,
    session_id: SessionId,
) -> ReviewOrchestrationRuntimeError {
    match error {
        SessionRepositoryError::Database(_) => ReviewOrchestrationRuntimeError::Unavailable {
            commit_ambiguous: false,
        },
        SessionRepositoryError::Corruption(_) => ReviewOrchestrationRuntimeError::Internal {
            session_id: Some(session_id.into_uuid()),
            cause: ReviewOrchestrationInternalCause::SessionCorruption,
        },
    }
}

fn map_service_error(
    error: ReviewOrchestrationServiceError<ReviewOrchestrationStoreError, RunnerError>,
) -> ReviewOrchestrationRuntimeError {
    match error {
        ReviewOrchestrationServiceError::Store(error) => map_post_effect_store_error(error),
        ReviewOrchestrationServiceError::DurableConflict => {
            ReviewOrchestrationRuntimeError::Rejected
        }
        ReviewOrchestrationServiceError::InvalidImportEvidence(_)
        | ReviewOrchestrationServiceError::InvalidJudgmentPlan(_)
        | ReviewOrchestrationServiceError::InvalidJudgmentEffectEvidence(_)
        | ReviewOrchestrationServiceError::InvalidTerminalBarrier(_) => {
            ReviewOrchestrationRuntimeError::Rejected
        }
        ReviewOrchestrationServiceError::Runner(RunnerError::AwaitingInput)
        | ReviewOrchestrationServiceError::ConcernTaskTerminated
        | ReviewOrchestrationServiceError::InvalidAppliedEffects => {
            internal(ReviewOrchestrationInternalCause::ServiceContract)
        }
    }
}

const fn internal(cause: ReviewOrchestrationInternalCause) -> ReviewOrchestrationRuntimeError {
    ReviewOrchestrationRuntimeError::Internal {
        session_id: None,
        cause,
    }
}

#[cfg(test)]
mod tests {
    use signalbox_application::ReviewConcernOutcome;
    use signalbox_domain::{
        ReviewPassId, ReviewPassRef, ReviewRunId, ReviewRunRef, ReviewTargetId,
    };
    use signalbox_process_protocol::ReviewOrchestrationConcernStatus;
    use uuid::Uuid;

    use super::{
        ReviewOrchestrationCurrentStage, ReviewOrchestrationRuntimeError,
        ReviewOrchestrationServiceError, ReviewOrchestrationStage, ReviewOrchestrationStoreError,
        RunnerError, map_service_error, map_store_error, recorded_stage_is_current,
    };

    #[test]
    fn superseded_concern_keeps_its_distinct_wire_status() {
        let target = ReviewTargetId::from_uuid(Uuid::from_u128(1));
        let run = ReviewRunRef::new(target, ReviewRunId::from_uuid(Uuid::from_u128(2)));
        let pass = ReviewPassRef::new(run, ReviewPassId::from_uuid(Uuid::from_u128(3)));
        let outcome = ReviewConcernOutcome::Superseded { pass };

        assert_eq!(
            super::concern_status(&outcome),
            ReviewOrchestrationConcernStatus::Superseded,
        );
    }

    #[test]
    fn durable_stage_conflict_is_not_command_identity_reuse() {
        let error: ReviewOrchestrationServiceError<ReviewOrchestrationStoreError, RunnerError> =
            ReviewOrchestrationServiceError::DurableConflict;

        assert_eq!(
            map_service_error(error),
            ReviewOrchestrationRuntimeError::Rejected,
        );
    }

    #[test]
    fn pre_effect_stage_read_failure_is_not_commit_ambiguous() {
        let error = ReviewOrchestrationStoreError::Database(sqlx::Error::PoolTimedOut);

        assert_eq!(
            map_store_error(error),
            ReviewOrchestrationRuntimeError::Unavailable {
                commit_ambiguous: false,
            },
        );
    }

    #[test]
    fn equal_stage_submission_is_admitted_at_its_recorded_stage() {
        assert!(recorded_stage_is_current(
            ReviewOrchestrationCurrentStage::AwaitingConcerns,
            ReviewOrchestrationStage::AwaitingConcerns,
        ));
    }

    #[test]
    fn equal_stage_submission_is_rejected_after_progress_advances() {
        assert!(!recorded_stage_is_current(
            ReviewOrchestrationCurrentStage::Complete,
            ReviewOrchestrationStage::AwaitingConcerns,
        ));
    }
}

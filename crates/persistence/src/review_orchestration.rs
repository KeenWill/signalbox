//! PostgreSQL persistence for resumable review-orchestration attempts.
//!
//! These tables retain orchestration inventory and barrier facts. Canonical
//! run, pass, finding, event, and external-link values are always reconstructed
//! through [`ReviewWorkflowStore`].

use std::{error::Error, fmt, time::Duration};

use signalbox_application::{
    ReviewConcernClaim, ReviewConcernOutcome, ReviewConcernSpec, ReviewConcernSuccess,
    ReviewDurableSealOutcome, ReviewImportOutcome, ReviewImportedContextEvidence,
    ReviewJudgmentEffectId, ReviewJudgmentPlan, ReviewJudgmentPlanMember,
    ReviewOrchestrationAttempt, ReviewOrchestrationAttemptId, ReviewOrchestrationAttemptStore,
    ReviewPassIncompleteStatus, ReviewPlannedDisposition, ReviewPublicationMemberOutcome,
    ReviewPublicationSuccess, ReviewRepairMemberOutcome, ReviewRepairSuccess,
    ReviewStageTemplateDigests, ReviewTemplateDigest,
};
use signalbox_domain::{
    DurableCommandId, ReviewConfidence, ReviewExternalLinkId, ReviewFinding,
    ReviewFindingExternalLinkRef, ReviewFindingId, ReviewFindingRef, ReviewKey, ReviewPassEvidence,
    ReviewPassId, ReviewPassRef, ReviewPolicy, ReviewPolicyVersion, ReviewRunEvidence, ReviewRunId,
    ReviewRunRef, ReviewTargetId, ReviewText,
};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{self, CommandKind, REVIEW_ORCHESTRATION_KIND, RegistryInspectionError},
    mapping::durable_command_id_to_uuid,
    review_workflow::{ReviewWorkflowStore, ReviewWorkflowStoreError},
};

const STORAGE_VERSION: i16 = 1;
const SNAPSHOT_ADMISSION_INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const SNAPSHOT_ADMISSION_MAX_BACKOFF: Duration = Duration::from_millis(100);
const REVIEW_SNAPSHOT_LOCKS: &str = "LOCK TABLE
    accepted_input,
    turn_lifecycle,
    review_external_link,
    review_external_link_attachment,
    review_external_link_observation,
    review_external_object_identity,
    review_finding,
    review_finding_event,
    review_finding_event_head,
    review_orchestration_attempt,
    review_orchestration_command,
    review_orchestration_concern,
    review_orchestration_concern_claim,
    review_orchestration_concern_finding,
    review_orchestration_fanout_member,
    review_orchestration_fanout_seal,
    review_orchestration_import,
    review_orchestration_judgment_effect,
    review_orchestration_judgment_member,
    review_orchestration_judgment_plan,
    review_orchestration_publication_inventory,
    review_orchestration_publication_inventory_seal,
    review_orchestration_publication_outcome,
    review_orchestration_publication_outcome_seal,
    review_orchestration_repair_inventory,
    review_orchestration_repair_inventory_seal,
    review_orchestration_repair_outcome,
    review_orchestration_repair_outcome_seal,
    review_pass,
    review_pass_finding_inventory_seal,
    review_pass_produced_finding,
    review_run,
    review_target
    IN SHARE MODE";

/// PostgreSQL adapter for durable orchestration attempts and command receipts.
#[derive(Clone, Debug)]
pub struct PostgresReviewOrchestrationStore {
    pool: PgPool,
    workflow: ReviewWorkflowStore,
}

impl PostgresReviewOrchestrationStore {
    /// Binds orchestration storage to the same guarded pool as review workflow storage.
    pub fn new(pool: PgPool) -> Self {
        Self {
            workflow: ReviewWorkflowStore::new(pool.clone()),
            pool,
        }
    }

    /// Loads and fail-closed reconstructs one immutable attempt.
    pub async fn load_attempt(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationAttempt>, ReviewOrchestrationStoreError> {
        let row = sqlx::query(
            "SELECT target_id, policy_version, minimum_judge_confidence,
                    minimum_publication_confidence, concern_set_version,
                    import_template_digest, judgment_template_digest,
                    repair_template_digest, publication_template_digest
               FROM review_orchestration_attempt
              WHERE attempt_id = $1",
        )
        .bind(attempt.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let policy = decode_policy(&row)?;
        let concerns = sqlx::query(
            "SELECT concern_key, template_digest
               FROM review_orchestration_concern
              WHERE attempt_id = $1
              ORDER BY concern_ordinal",
        )
        .bind(attempt.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(ReviewConcernSpec::new(
                decode_key(row.try_get("concern_key")?)?,
                decode_digest(row.try_get("template_digest")?)?,
            ))
        })
        .collect::<Result<Vec<_>, ReviewOrchestrationStoreError>>()?;
        let value = ReviewOrchestrationAttempt::try_new(
            attempt,
            ReviewTargetId::from_uuid(row.try_get("target_id")?),
            policy,
            decode_key(row.try_get("concern_set_version")?)?,
            ReviewStageTemplateDigests::new(
                decode_digest(row.try_get("import_template_digest")?)?,
                decode_digest(row.try_get("judgment_template_digest")?)?,
                decode_digest(row.try_get("repair_template_digest")?)?,
                decode_digest(row.try_get("publication_template_digest")?)?,
            ),
            concerns,
        )
        .map_err(|_| corruption("attempt inventory is invalid"))?;
        Ok(Some(value))
    }

    /// Loads adapter-specific durable progress facts for daemon status responses.
    pub async fn load_progress(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationProgress>, ReviewOrchestrationStoreError> {
        if self.load_attempt(attempt).await?.is_none() {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT
                EXISTS (SELECT 1 FROM review_orchestration_import WHERE attempt_id = $1)
                    AS import_recorded,
                (SELECT count(*) FROM (
                    SELECT DISTINCT concern_key FROM review_orchestration_concern_claim
                    WHERE attempt_id = $1
                ) AS current_claims) AS concern_claim_count,
                EXISTS (SELECT 1 FROM review_orchestration_fanout_seal WHERE attempt_id = $1)
                    AS fanout_sealed,
                EXISTS (SELECT 1 FROM review_orchestration_judgment_plan WHERE attempt_id = $1)
                    AS judgment_plan_sealed,
                (SELECT count(*) FROM review_orchestration_judgment_effect WHERE attempt_id = $1)
                    AS judgment_effect_count,
                CASE WHEN EXISTS (
                    SELECT 1 FROM review_orchestration_repair_inventory_seal WHERE attempt_id = $1
                ) THEN (
                    SELECT count(*) FROM review_orchestration_repair_inventory WHERE attempt_id = $1
                ) END AS repair_inventory_count,
                EXISTS (SELECT 1 FROM review_orchestration_repair_outcome_seal WHERE attempt_id = $1)
                    AS repair_outcomes_recorded,
                CASE WHEN EXISTS (
                    SELECT 1 FROM review_orchestration_publication_inventory_seal WHERE attempt_id = $1
                ) THEN (
                    SELECT count(*) FROM review_orchestration_publication_inventory WHERE attempt_id = $1
                ) END AS publication_inventory_count,
                EXISTS (SELECT 1 FROM review_orchestration_publication_outcome_seal WHERE attempt_id = $1)
                    AS publication_outcomes_recorded",
        )
        .bind(attempt.as_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(Some(decode_progress(&row)?))
    }

    /// Derives the deterministic daemon-visible stage from validated durable facts.
    pub async fn current_stage(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationCurrentStage>, ReviewOrchestrationStoreError> {
        let Some(immutable) = self.load_attempt(attempt).await? else {
            return Ok(None);
        };
        let Some(import) = self.load_import(attempt).await? else {
            return Ok(Some(ReviewOrchestrationCurrentStage::AwaitingImport));
        };
        if matches!(import, ReviewImportOutcome::Incomplete { .. }) {
            return Ok(Some(ReviewOrchestrationCurrentStage::ImportIncomplete));
        }
        let claims = self.load_concern_claims(attempt).await?;
        if claims.len() < immutable.concerns().len() {
            return Ok(Some(ReviewOrchestrationCurrentStage::AwaitingConcerns));
        }
        if claims
            .iter()
            .any(|claim| !matches!(claim.outcome(), ReviewConcernOutcome::Succeeded(_)))
        {
            return Ok(Some(ReviewOrchestrationCurrentStage::FanoutIncomplete));
        }
        let Some(plan) = self.load_judgment_plan(attempt).await? else {
            return Ok(Some(ReviewOrchestrationCurrentStage::AwaitingJudgment));
        };
        let effects = self.load_applied_judgment_effects(attempt).await?;
        if effects.len() < plan.members().len() {
            let interrupted = sqlx::query(
                "SELECT 1 FROM review_orchestration_command
                  WHERE attempt_id = $1 AND result_stage = 'judgment_incomplete'
                    AND judgment_effect_count = $2
                  LIMIT 1",
            )
            .bind(attempt.as_uuid())
            .bind(count_i64(effects.len())?)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
            return Ok(Some(if interrupted {
                ReviewOrchestrationCurrentStage::JudgmentIncomplete
            } else {
                ReviewOrchestrationCurrentStage::AwaitingJudgmentEffects
            }));
        }
        let Some(_) = load_finding_inventory(self, attempt, "repair").await? else {
            return Ok(Some(ReviewOrchestrationCurrentStage::AwaitingRepair));
        };
        let Some(repairs) = load_repairs(self, attempt).await? else {
            return Ok(Some(ReviewOrchestrationCurrentStage::AwaitingRepair));
        };
        if repairs
            .iter()
            .any(|outcome| matches!(outcome, ReviewRepairMemberOutcome::Blocked(_)))
        {
            return Ok(Some(ReviewOrchestrationCurrentStage::RepairIncomplete));
        }
        let Some(_) = load_finding_inventory(self, attempt, "publication").await? else {
            return Ok(Some(ReviewOrchestrationCurrentStage::AwaitingPublication));
        };
        let Some(publications) = load_publications(self, attempt).await? else {
            return Ok(Some(ReviewOrchestrationCurrentStage::AwaitingPublication));
        };
        Ok(Some(
            if publications
                .iter()
                .all(|outcome| matches!(outcome, ReviewPublicationMemberOutcome::Published(_)))
            {
                ReviewOrchestrationCurrentStage::Complete
            } else {
                ReviewOrchestrationCurrentStage::PublicationIncomplete
            },
        ))
    }

    /// Loads every daemon adapter fact from one coherently locked database view.
    ///
    /// Snapshot admission is database-global and transaction-scoped. The
    /// admitted transaction holds shared locks across every review table used
    /// by canonical reconstruction while the existing workflow loaders use the
    /// same configured pool. Concurrent snapshots release unsuccessful
    /// admission attempts before an exponentially backed-off retry; the wait
    /// begins at 10 ms and is capped at 100 ms, preserving one reader connection
    /// without polling PostgreSQL on every scheduler turn.
    pub async fn load_snapshot(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationSnapshotFacts>, ReviewOrchestrationStoreError> {
        if self.pool.options().get_max_connections() < 2 {
            return Err(corruption(
                "review orchestration snapshots require two configured pool connections",
            ));
        }
        let snapshot_keeper = self.begin_snapshot_guard().await?;
        let result = self.load_snapshot_while_locked(attempt).await;
        snapshot_keeper.rollback().await?;
        result
    }

    async fn begin_snapshot_guard(
        &self,
    ) -> Result<Transaction<'_, Postgres>, ReviewOrchestrationStoreError> {
        let mut backoff = SNAPSHOT_ADMISSION_INITIAL_BACKOFF;
        loop {
            let mut transaction = self.pool.begin().await?;
            let admitted: bool = sqlx::query_scalar(
                "SELECT pg_try_advisory_xact_lock(
                    hashtext('signalbox:review-orchestration'),
                    hashtext('coherent-snapshot'))",
            )
            .fetch_one(&mut *transaction)
            .await?;
            if admitted {
                sqlx::query(REVIEW_SNAPSHOT_LOCKS)
                    .execute(&mut *transaction)
                    .await?;
                return Ok(transaction);
            }
            transaction.rollback().await?;
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(backoff.saturating_mul(2), SNAPSHOT_ADMISSION_MAX_BACKOFF);
        }
    }

    async fn load_snapshot_while_locked(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationSnapshotFacts>, ReviewOrchestrationStoreError> {
        let Some(immutable) = self.load_attempt(attempt).await? else {
            return Ok(None);
        };
        let current_stage = self
            .current_stage(attempt)
            .await?
            .ok_or_else(|| corruption("snapshot attempt disappeared"))?;
        Ok(Some(ReviewOrchestrationSnapshotFacts {
            attempt: immutable,
            current_stage,
            concern_claims: self.load_concern_claims(attempt).await?,
            judgment_plan: self.load_judgment_plan(attempt).await?,
            applied_judgment_effects: self.load_applied_judgment_effects(attempt).await?,
            repair_outcomes: self.load_repair_outcomes(attempt).await?,
            publication_outcomes: self.load_publication_outcomes(attempt).await?,
        }))
    }
}

/// Deterministic daemon-visible stage of one durable orchestration attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewOrchestrationCurrentStage {
    AwaitingImport,
    ImportIncomplete,
    AwaitingConcerns,
    FanoutIncomplete,
    AwaitingJudgment,
    AwaitingJudgmentEffects,
    JudgmentIncomplete,
    AwaitingRepair,
    RepairIncomplete,
    AwaitingPublication,
    PublicationIncomplete,
    Complete,
}

/// Closed durable stage reported by one orchestration command receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewOrchestrationStage {
    Started,
    AwaitingImport,
    ImportIncomplete,
    AwaitingConcerns,
    FanoutIncomplete,
    AwaitingJudgment,
    AwaitingJudgmentEffects,
    JudgmentIncomplete,
    AwaitingRepair,
    RepairIncomplete,
    AwaitingPublication,
    PublicationIncomplete,
    Complete,
}

/// Stable adapter facts sufficient to resume and report one attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewOrchestrationProgress {
    pub import_recorded: bool,
    pub concern_claim_count: usize,
    pub fanout_sealed: bool,
    pub judgment_plan_sealed: bool,
    pub judgment_effect_count: usize,
    pub repair_inventory_count: Option<usize>,
    pub repair_outcomes_recorded: bool,
    pub publication_inventory_count: Option<usize>,
    pub publication_outcomes_recorded: bool,
}

/// Canonically reconstructed daemon adapter facts from one database snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewOrchestrationSnapshotFacts {
    pub attempt: ReviewOrchestrationAttempt,
    pub current_stage: ReviewOrchestrationCurrentStage,
    pub concern_claims: Vec<ReviewConcernClaim>,
    pub judgment_plan: Option<ReviewJudgmentPlan>,
    pub applied_judgment_effects: Vec<ReviewJudgmentEffectId>,
    pub repair_outcomes: Option<Vec<ReviewRepairMemberOutcome>>,
    pub publication_outcomes: Option<Vec<ReviewPublicationMemberOutcome>>,
}

/// Closed owner-global command kinds for orchestration mutations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewOrchestrationCommandKind {
    Start,
    Import,
    Concern,
    JudgmentPlan,
    JudgmentEffect,
    Repair,
    Publication,
}

/// One checked owner-global orchestration command claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewOrchestrationCommand {
    pub command_id: DurableCommandId,
    pub semantic_digest: [u8; 32],
    pub attempt: ReviewOrchestrationAttemptId,
    pub kind: ReviewOrchestrationCommandKind,
}

/// Frozen response state retained by an orchestration command receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewOrchestrationCommandResult {
    pub attempt: ReviewOrchestrationAttemptId,
    pub stage: ReviewOrchestrationStage,
    pub progress: ReviewOrchestrationProgress,
}

/// Pre-effect owner-global command claim result.
pub enum ReviewOrchestrationCommandClaim {
    ExistingRecorded(ReviewOrchestrationCommandResult),
    Conflicting,
    New(ReviewOrchestrationCommandGuard),
}

/// An uncommitted registry claim fencing one owner-global command identity.
pub struct ReviewOrchestrationCommandGuard {
    transaction: Transaction<'static, Postgres>,
    command: ReviewOrchestrationCommand,
}

impl ReviewOrchestrationCommandGuard {
    /// Records the exact response state and commits the fenced command claim.
    pub async fn record(
        mut self,
        result: ReviewOrchestrationCommandResult,
    ) -> Result<ReviewOrchestrationCommandResult, ReviewOrchestrationStoreError> {
        if result.attempt != self.command.attempt {
            return Err(corruption(
                "orchestration command result names another attempt",
            ));
        }
        insert_command_receipt(&mut self.transaction, &self.command, &result).await?;
        self.transaction
            .commit()
            .await
            .map_err(ReviewOrchestrationStoreError::CommitAmbiguous)?;
        Ok(result)
    }
}

impl PostgresReviewOrchestrationStore {
    /// Claims an owner-global command before its separately durable effect.
    ///
    /// The returned new guard keeps the registry insertion uncommitted. A
    /// concurrent reuse blocks at that insertion, so no distinct payload can
    /// perform an effect before command identity is decided.
    pub async fn begin_command(
        &self,
        command: ReviewOrchestrationCommand,
    ) -> Result<ReviewOrchestrationCommandClaim, ReviewOrchestrationStoreError> {
        let mut transaction = self.pool.begin().await?;
        match inspect_command(&mut transaction, command).await? {
            CommandInspection::Recorded(result) => {
                transaction.commit().await?;
                return Ok(ReviewOrchestrationCommandClaim::ExistingRecorded(result));
            }
            CommandInspection::Conflicting => {
                transaction.rollback().await?;
                return Ok(ReviewOrchestrationCommandClaim::Conflicting);
            }
            CommandInspection::New => {}
        }
        let inserted = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, $3, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command.command_id))
        .bind(REVIEW_ORCHESTRATION_KIND)
        .bind(STORAGE_VERSION)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !inserted {
            return match inspect_command(&mut transaction, command).await? {
                CommandInspection::Recorded(result) => {
                    transaction.commit().await?;
                    Ok(ReviewOrchestrationCommandClaim::ExistingRecorded(result))
                }
                CommandInspection::Conflicting => {
                    transaction.rollback().await?;
                    Ok(ReviewOrchestrationCommandClaim::Conflicting)
                }
                CommandInspection::New => Err(corruption(
                    "orchestration command claim lost its registry race",
                )),
            };
        }
        Ok(ReviewOrchestrationCommandClaim::New(
            ReviewOrchestrationCommandGuard {
                transaction,
                command,
            },
        ))
    }
}

impl ReviewOrchestrationAttemptStore for PostgresReviewOrchestrationStore {
    type Error = ReviewOrchestrationStoreError;

    async fn record_attempt(
        &mut self,
        attempt: ReviewOrchestrationAttempt,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        let mut transaction = self.pool.begin().await?;
        let policy = attempt.policy();
        let templates = attempt.stage_templates();
        let inserted = sqlx::query(
            "INSERT INTO review_orchestration_attempt
                (attempt_id, target_id, policy_version, minimum_judge_confidence,
                 minimum_publication_confidence, concern_set_version,
                 import_template_digest, judgment_template_digest,
                 repair_template_digest, publication_template_digest)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             ON CONFLICT DO NOTHING",
        )
        .bind(attempt.id().as_uuid())
        .bind(attempt.target().into_uuid())
        .bind(i64::from(policy.version().get()))
        .bind(i32::from(policy.minimum_judge_confidence().basis_points()))
        .bind(i32::from(
            policy.minimum_publication_confidence().basis_points(),
        ))
        .bind(attempt.concern_set_version().as_str())
        .bind(templates.import().bytes().as_slice())
        .bind(templates.judgment().bytes().as_slice())
        .bind(templates.repair().bytes().as_slice())
        .bind(templates.publication().bytes().as_slice())
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if inserted {
            for (ordinal, concern) in attempt.concerns().iter().enumerate() {
                sqlx::query(
                    "INSERT INTO review_orchestration_concern
                        (attempt_id, concern_ordinal, concern_key, template_digest)
                     VALUES ($1,$2,$3,$4)",
                )
                .bind(attempt.id().as_uuid())
                .bind(ordinal_i32(ordinal)?)
                .bind(concern.key().as_str())
                .bind(concern.template_digest().bytes().as_slice())
                .execute(&mut *transaction)
                .await?;
            }
            commit_or_ambiguous(transaction).await?;
            return Ok(ReviewDurableSealOutcome::Recorded);
        }
        transaction.rollback().await?;
        Ok(match self.load_attempt(attempt.id()).await? {
            Some(existing) if existing == attempt => ReviewDurableSealOutcome::EqualReplay,
            Some(_) => ReviewDurableSealOutcome::Conflict,
            None => return Err(corruption("attempt conflict disappeared")),
        })
    }

    async fn load_import(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewImportOutcome>, Self::Error> {
        let row = sqlx::query(
            "SELECT outcome_kind, pass_id, external_link_id, template_digest, context_digest
               FROM review_orchestration_import WHERE attempt_id = $1",
        )
        .bind(attempt.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let kind: String = row.try_get("outcome_kind")?;
        let template = decode_digest(row.try_get("template_digest")?)?;
        let pass_id: Option<Uuid> = row.try_get("pass_id")?;
        let outcome = match kind.as_str() {
            "succeeded" => {
                let pass_id = pass_id.ok_or_else(|| corruption("succeeded import lacks pass"))?;
                let (pass, run) = self.load_evidence(ReviewPassId::from_uuid(pass_id)).await?;
                let link = match row.try_get::<Option<Uuid>, _>("external_link_id")? {
                    Some(id) => Some(Box::new(
                        self.workflow
                            .load_external_link(ReviewExternalLinkId::from_uuid(id))
                            .await?
                            .ok_or_else(|| corruption("import link is missing"))?,
                    )),
                    None => None,
                };
                ReviewImportOutcome::Succeeded {
                    context: ReviewImportedContextEvidence::new(
                        pass.reference(),
                        decode_bytes(row.try_get("context_digest")?)?,
                    ),
                    pass: Box::new(pass),
                    run,
                    external_link: link,
                    template_digest: template,
                }
            }
            "failed" | "blocked" | "cancelled" => {
                let evidence = match pass_id {
                    Some(id) => Some(self.load_evidence(ReviewPassId::from_uuid(id)).await?),
                    None => None,
                };
                ReviewImportOutcome::Incomplete {
                    pass: evidence.as_ref().map(|(pass, _)| Box::new(pass.clone())),
                    run: evidence.map(|(_, run)| run),
                    template_digest: template,
                    status: decode_incomplete_status(&kind)?,
                }
            }
            _ => return Err(corruption("import outcome kind is not closed")),
        };
        Ok(Some(outcome))
    }

    async fn record_import(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        outcome: ReviewImportOutcome,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        self.verify_import_evidence(&outcome).await?;
        let (kind, pass, link, template, context) = encode_import(&outcome);
        let inserted = sqlx::query(
            "INSERT INTO review_orchestration_import
                (attempt_id, outcome_kind, pass_id, external_link_id,
                 template_digest, context_digest)
             VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING",
        )
        .bind(attempt.as_uuid())
        .bind(kind)
        .bind(pass.map(|value| value.into_uuid()))
        .bind(link.map(|value| value.into_uuid()))
        .bind(template.bytes().as_slice())
        .bind(context.as_ref().map(<[u8; 32]>::as_slice))
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        if inserted {
            Ok(ReviewDurableSealOutcome::Recorded)
        } else {
            Ok(match self.load_import(attempt).await? {
                Some(existing) if existing == outcome => ReviewDurableSealOutcome::EqualReplay,
                Some(_) => ReviewDurableSealOutcome::Conflict,
                None => return Err(corruption("import conflict disappeared")),
            })
        }
    }

    async fn load_concern_claims(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Vec<ReviewConcernClaim>, Self::Error> {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (concern_key)
                    concern_key, claim_ordinal, template_digest, outcome_kind, pass_id
               FROM review_orchestration_concern_claim
              WHERE attempt_id = $1
              ORDER BY concern_key, claim_ordinal DESC",
        )
        .bind(attempt.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        let mut claims = Vec::with_capacity(rows.len());
        for row in rows {
            claims.push(self.decode_concern_claim(attempt, &row).await?);
        }
        let immutable = self
            .load_attempt(attempt)
            .await?
            .ok_or_else(|| corruption("concern claims name a missing attempt"))?;
        claims.sort_by_key(|claim| {
            immutable
                .concerns()
                .iter()
                .position(|expected| expected.key() == claim.concern())
                .unwrap_or(usize::MAX)
        });
        Ok(claims)
    }

    async fn record_concern_claim(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        claim: ReviewConcernClaim,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        if sqlx::query("SELECT 1 FROM review_orchestration_fanout_seal WHERE attempt_id = $1")
            .bind(attempt.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .is_some()
        {
            return Ok(ReviewDurableSealOutcome::Conflict);
        }
        let current = self.load_concern_claims(attempt).await?;
        if let Some(existing) = current
            .iter()
            .find(|value| value.concern() == claim.concern())
        {
            if existing == &claim {
                return Ok(ReviewDurableSealOutcome::EqualReplay);
            }
            if !matches!(existing.outcome(), ReviewConcernOutcome::Failed { .. }) {
                return Ok(ReviewDurableSealOutcome::Conflict);
            }
        }
        self.verify_claim_evidence(&claim).await?;
        let mut transaction = self.pool.begin().await?;
        let next: i32 = sqlx::query_scalar(
            "SELECT COALESCE(max(claim_ordinal) + 1, 0)
               FROM review_orchestration_concern_claim
              WHERE attempt_id = $1 AND concern_key = $2",
        )
        .bind(attempt.as_uuid())
        .bind(claim.concern().as_str())
        .fetch_one(&mut *transaction)
        .await?;
        let (kind, pass) = encode_concern_outcome(claim.outcome());
        sqlx::query(
            "INSERT INTO review_orchestration_concern_claim
                (attempt_id, concern_key, claim_ordinal, template_digest, outcome_kind, pass_id)
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(attempt.as_uuid())
        .bind(claim.concern().as_str())
        .bind(next)
        .bind(claim.template_digest().bytes().as_slice())
        .bind(kind)
        .bind(pass.map(|value| value.into_uuid()))
        .execute(&mut *transaction)
        .await?;
        if let ReviewConcernOutcome::Succeeded(success) = claim.outcome() {
            for (ordinal, finding) in success.findings().iter().enumerate() {
                sqlx::query(
                    "INSERT INTO review_orchestration_concern_finding
                        (attempt_id, concern_key, claim_ordinal, finding_ordinal, finding_id)
                     VALUES ($1,$2,$3,$4,$5)",
                )
                .bind(attempt.as_uuid())
                .bind(claim.concern().as_str())
                .bind(next)
                .bind(ordinal_i32(ordinal)?)
                .bind(finding.proposal().reference().finding().into_uuid())
                .execute(&mut *transaction)
                .await?;
            }
        }
        commit_or_ambiguous(transaction).await?;
        Ok(ReviewDurableSealOutcome::Recorded)
    }

    async fn seal_complete_fanout(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        claims: Vec<ReviewConcernClaim>,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        let immutable = self
            .load_attempt(attempt)
            .await?
            .ok_or_else(|| corruption("fanout seal names a missing attempt"))?;
        let current = self.load_concern_claims(attempt).await?;
        let exact_inventory = current == claims
            && claims.len() == immutable.concerns().len()
            && claims
                .iter()
                .zip(immutable.concerns())
                .all(|(claim, expected)| {
                    claim.concern() == expected.key()
                        && claim.template_digest() == expected.template_digest()
                        && matches!(claim.outcome(), ReviewConcernOutcome::Succeeded(_))
                });
        if !exact_inventory {
            return Ok(ReviewDurableSealOutcome::Conflict);
        }
        let existing = sqlx::query(
            "SELECT concern_key FROM review_orchestration_fanout_member
              WHERE attempt_id = $1 ORDER BY member_ordinal",
        )
        .bind(attempt.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        if !existing.is_empty() {
            let equal = existing.len() == claims.len()
                && existing.iter().zip(&claims).all(|(row, claim)| {
                    row.try_get::<String, _>("concern_key").ok().as_deref()
                        == Some(claim.concern().as_str())
                });
            return Ok(if equal {
                ReviewDurableSealOutcome::EqualReplay
            } else {
                ReviewDurableSealOutcome::Conflict
            });
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query("INSERT INTO review_orchestration_fanout_seal (attempt_id) VALUES ($1)")
            .bind(attempt.as_uuid())
            .execute(&mut *transaction)
            .await?;
        for (ordinal, claim) in claims.iter().enumerate() {
            let claim_ordinal: i32 = sqlx::query_scalar(
                "SELECT max(claim_ordinal) FROM review_orchestration_concern_claim
                  WHERE attempt_id = $1 AND concern_key = $2",
            )
            .bind(attempt.as_uuid())
            .bind(claim.concern().as_str())
            .fetch_one(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO review_orchestration_fanout_member
                    (attempt_id, member_ordinal, concern_key, claim_ordinal)
                 VALUES ($1,$2,$3,$4)",
            )
            .bind(attempt.as_uuid())
            .bind(ordinal_i32(ordinal)?)
            .bind(claim.concern().as_str())
            .bind(claim_ordinal)
            .execute(&mut *transaction)
            .await?;
        }
        commit_or_ambiguous(transaction).await?;
        Ok(ReviewDurableSealOutcome::Recorded)
    }

    async fn seal_judgment_plan(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        plan: ReviewJudgmentPlan,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        seal_judgment_plan_impl(self, attempt, plan).await
    }

    async fn load_judgment_plan(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewJudgmentPlan>, Self::Error> {
        load_judgment_plan_impl(self, attempt).await
    }

    async fn load_applied_judgment_effects(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Vec<ReviewJudgmentEffectId>, Self::Error> {
        load_applied_effects_impl(self, attempt).await
    }

    async fn record_applied_judgment_effect(
        &mut self,
        effect: ReviewJudgmentEffectId,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        record_applied_effect_impl(self, effect).await
    }

    async fn seal_repair_inventory(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        findings: Vec<ReviewFindingRef>,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        seal_finding_inventory(self, attempt, &findings, "repair").await
    }

    async fn record_repair_outcomes(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        outcomes: Vec<ReviewRepairMemberOutcome>,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        record_repairs(self, attempt, outcomes).await
    }

    async fn load_repair_outcomes(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<Vec<ReviewRepairMemberOutcome>>, Self::Error> {
        load_repairs(self, attempt).await
    }

    async fn seal_publication_inventory(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        findings: Vec<ReviewFindingRef>,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        seal_finding_inventory(self, attempt, &findings, "publication").await
    }

    async fn record_publication_outcomes(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        outcomes: Vec<ReviewPublicationMemberOutcome>,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        record_publications(self, attempt, outcomes).await
    }

    async fn load_publication_outcomes(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<Vec<ReviewPublicationMemberOutcome>>, Self::Error> {
        load_publications(self, attempt).await
    }
}

impl PostgresReviewOrchestrationStore {
    async fn load_evidence(
        &self,
        pass_id: ReviewPassId,
    ) -> Result<(ReviewPassEvidence, ReviewRunEvidence), ReviewOrchestrationStoreError> {
        let pass = self
            .workflow
            .load_pass(pass_id)
            .await?
            .ok_or_else(|| corruption("referenced review pass is missing"))?;
        let run = self
            .workflow
            .load_run(pass.reference().run().run())
            .await?
            .ok_or_else(|| corruption("referenced review run is missing"))?;
        Ok((
            ReviewPassEvidence::from_pass(&pass, run.policy()),
            run.evidence(),
        ))
    }

    async fn decode_concern_claim(
        &self,
        attempt: ReviewOrchestrationAttemptId,
        row: &PgRow,
    ) -> Result<ReviewConcernClaim, ReviewOrchestrationStoreError> {
        let concern = decode_key(row.try_get("concern_key")?)?;
        let template = decode_digest(row.try_get("template_digest")?)?;
        let kind: String = row.try_get("outcome_kind")?;
        let pass_id: Option<Uuid> = row.try_get("pass_id")?;
        let pass = pass_id.map(ReviewPassId::from_uuid);
        let outcome = match kind.as_str() {
            "succeeded" => {
                let (producer, run) = self
                    .load_evidence(pass.ok_or_else(|| corruption("successful claim lacks pass"))?)
                    .await?;
                let findings = self
                    .load_claim_findings(attempt, concern.as_str(), row.try_get("claim_ordinal")?)
                    .await?;
                ReviewConcernOutcome::Succeeded(Box::new(ReviewConcernSuccess::new(
                    producer, run, template, findings,
                )))
            }
            "failed" => ReviewConcernOutcome::Failed {
                pass: self.pass_ref(pass).await?,
            },
            "blocked" => ReviewConcernOutcome::Blocked {
                pass: self.pass_ref(pass).await?,
            },
            "cancelled" => ReviewConcernOutcome::Cancelled {
                pass: match pass {
                    Some(pass) => Some(self.pass_ref(Some(pass)).await?),
                    None => None,
                },
            },
            "superseded" => ReviewConcernOutcome::Superseded {
                pass: self.pass_ref(pass).await?,
            },
            _ => return Err(corruption("concern outcome kind is not closed")),
        };
        Ok(ReviewConcernClaim::new(concern, template, outcome))
    }

    async fn pass_ref(
        &self,
        pass: Option<ReviewPassId>,
    ) -> Result<ReviewPassRef, ReviewOrchestrationStoreError> {
        Ok(self
            .workflow
            .load_pass(pass.ok_or_else(|| corruption("claim lacks pass"))?)
            .await?
            .ok_or_else(|| corruption("claim pass is missing"))?
            .reference())
    }

    async fn load_claim_findings(
        &self,
        attempt: ReviewOrchestrationAttemptId,
        concern: &str,
        claim_ordinal: i32,
    ) -> Result<Vec<ReviewFinding>, ReviewOrchestrationStoreError> {
        let rows = sqlx::query(
            "SELECT finding_id FROM review_orchestration_concern_finding
              WHERE attempt_id = $1 AND concern_key = $2 AND claim_ordinal = $3
              ORDER BY finding_ordinal",
        )
        .bind(attempt.as_uuid())
        .bind(concern)
        .bind(claim_ordinal)
        .fetch_all(&self.pool)
        .await?;
        let mut findings = Vec::with_capacity(rows.len());
        for row in rows {
            let current = self
                .workflow
                .load_finding(ReviewFindingId::from_uuid(row.try_get("finding_id")?))
                .await?
                .ok_or_else(|| corruption("concern finding is missing"))?;
            findings.push(ReviewFinding::new(current.proposal().clone()));
        }
        Ok(findings)
    }

    async fn verify_claim_evidence(
        &self,
        claim: &ReviewConcernClaim,
    ) -> Result<(), ReviewOrchestrationStoreError> {
        if let ReviewConcernOutcome::Succeeded(success) = claim.outcome() {
            let (pass, run) = self.load_evidence(success.producer().pass()).await?;
            if &pass != success.producer_evidence() || run != success.run_evidence() {
                return Err(corruption(
                    "concern success differs from canonical pass evidence",
                ));
            }
            for finding in success.findings() {
                let canonical = self
                    .workflow
                    .load_finding(finding.proposal().reference().finding())
                    .await?
                    .ok_or_else(|| corruption("concern success finding is missing"))?;
                if canonical != *finding {
                    return Err(corruption("concern success differs from canonical finding"));
                }
            }
        } else {
            let expected = match claim.outcome() {
                ReviewConcernOutcome::Failed { pass }
                | ReviewConcernOutcome::Blocked { pass }
                | ReviewConcernOutcome::Superseded { pass } => Some(*pass),
                ReviewConcernOutcome::Cancelled { pass } => *pass,
                ReviewConcernOutcome::Succeeded(_) => None,
            };
            if let Some(expected) = expected
                && self.pass_ref(Some(expected.pass())).await? != expected
            {
                return Err(corruption("concern claim pass ancestry is not canonical"));
            }
        }
        Ok(())
    }

    async fn verify_import_evidence(
        &self,
        outcome: &ReviewImportOutcome,
    ) -> Result<(), ReviewOrchestrationStoreError> {
        let (pass, run, link) = match outcome {
            ReviewImportOutcome::Succeeded {
                pass,
                run,
                external_link,
                ..
            } => (Some(pass.as_ref()), Some(*run), external_link.as_deref()),
            ReviewImportOutcome::Incomplete { pass, run, .. } => (pass.as_deref(), *run, None),
        };
        if let Some(pass) = pass {
            let (canonical_pass, canonical_run) =
                self.load_evidence(pass.reference().pass()).await?;
            if &canonical_pass != pass || Some(canonical_run) != run {
                return Err(corruption(
                    "import outcome differs from canonical pass evidence",
                ));
            }
        } else if run.is_some() {
            return Err(corruption("import run evidence lacks a pass"));
        }
        if let Some(link) = link {
            let canonical = self
                .workflow
                .load_external_link(link.id())
                .await?
                .ok_or_else(|| corruption("import external link is missing"))?;
            if canonical != *link {
                return Err(corruption("import external link is not canonical"));
            }
        }
        Ok(())
    }
}

async fn seal_judgment_plan_impl(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
    plan: ReviewJudgmentPlan,
) -> Result<ReviewDurableSealOutcome, ReviewOrchestrationStoreError> {
    if let Some(existing) = load_judgment_plan_impl(store, attempt).await? {
        return Ok(if existing == plan {
            ReviewDurableSealOutcome::EqualReplay
        } else {
            ReviewDurableSealOutcome::Conflict
        });
    }
    let (pass, run) = store.load_evidence(plan.analysis_pass().pass()).await?;
    if &pass != plan.analysis_pass_evidence() || run != plan.analysis_run_evidence() {
        return Err(corruption(
            "judgment plan differs from canonical analysis evidence",
        ));
    }
    for member in plan.members() {
        verify_finding_ref(store, member.finding()).await?;
        match member.disposition() {
            ReviewPlannedDisposition::Duplicate { canonical } => {
                verify_finding_ref(store, *canonical).await?
            }
            ReviewPlannedDisposition::Superseded { successor } => {
                verify_finding_ref(store, *successor).await?
            }
            ReviewPlannedDisposition::Accepted
            | ReviewPlannedDisposition::Rejected { .. }
            | ReviewPlannedDisposition::Stale => {}
        }
    }
    let mut transaction = store.pool.begin().await?;
    sqlx::query(
        "INSERT INTO review_orchestration_judgment_plan
            (attempt_id, analysis_pass_id, template_digest) VALUES ($1,$2,$3)",
    )
    .bind(attempt.as_uuid())
    .bind(plan.analysis_pass().pass().into_uuid())
    .bind(plan.template_digest().bytes().as_slice())
    .execute(&mut *transaction)
    .await?;
    for (ordinal, member) in plan.members().iter().enumerate() {
        let (kind, reason, referenced) = encode_disposition(member.disposition());
        let finding = member.finding();
        sqlx::query(
            "INSERT INTO review_orchestration_judgment_member
                (attempt_id, member_ordinal, finding_id, finding_run_id, finding_pass_id,
                 disposition_kind, reason, referenced_finding_id,
                 referenced_run_id, referenced_pass_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        )
        .bind(attempt.as_uuid())
        .bind(ordinal_i32(ordinal)?)
        .bind(finding.finding().into_uuid())
        .bind(finding.run().run().into_uuid())
        .bind(finding.pass().pass().into_uuid())
        .bind(kind)
        .bind(reason)
        .bind(referenced.map(|value| value.finding().into_uuid()))
        .bind(referenced.map(|value| value.run().run().into_uuid()))
        .bind(referenced.map(|value| value.pass().pass().into_uuid()))
        .execute(&mut *transaction)
        .await?;
    }
    commit_or_ambiguous(transaction).await?;
    Ok(ReviewDurableSealOutcome::Recorded)
}

async fn load_judgment_plan_impl(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<ReviewJudgmentPlan>, ReviewOrchestrationStoreError> {
    let row = sqlx::query(
        "SELECT analysis_pass_id, template_digest
           FROM review_orchestration_judgment_plan WHERE attempt_id = $1",
    )
    .bind(attempt.as_uuid())
    .fetch_optional(&store.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let (pass, run) = store
        .load_evidence(ReviewPassId::from_uuid(row.try_get("analysis_pass_id")?))
        .await?;
    let rows = sqlx::query(
        "SELECT member.finding_id, member.finding_run_id, member.finding_pass_id,
                attempt.target_id,
                reason, referenced_finding_id, referenced_run_id, referenced_pass_id
                , disposition_kind
           FROM review_orchestration_judgment_member AS member
           JOIN review_orchestration_attempt AS attempt USING (attempt_id)
          WHERE member.attempt_id = $1 ORDER BY member_ordinal",
    )
    .bind(attempt.as_uuid())
    .fetch_all(&store.pool)
    .await?;
    let mut members = Vec::with_capacity(rows.len());
    for row in rows {
        let finding = decode_finding_ref(&row, "finding_id", "finding_run_id", "finding_pass_id")?;
        let kind: String = row.try_get("disposition_kind")?;
        let disposition = match kind.as_str() {
            "accepted" => ReviewPlannedDisposition::Accepted,
            "rejected" => ReviewPlannedDisposition::Rejected {
                reason: ReviewText::try_new(
                    row.try_get::<Option<String>, _>("reason")?
                        .ok_or_else(|| corruption("rejected plan member lacks reason"))?,
                )
                .map_err(|_| corruption("rejected plan reason is invalid"))?,
            },
            "duplicate" => ReviewPlannedDisposition::Duplicate {
                canonical: decode_optional_referenced_finding(&row)?
                    .ok_or_else(|| corruption("duplicate plan member lacks reference"))?,
            },
            "superseded" => ReviewPlannedDisposition::Superseded {
                successor: decode_optional_referenced_finding(&row)?
                    .ok_or_else(|| corruption("superseded plan member lacks reference"))?,
            },
            "stale" => ReviewPlannedDisposition::Stale,
            _ => return Err(corruption("judgment disposition is not closed")),
        };
        members.push(ReviewJudgmentPlanMember::new(finding, disposition));
    }
    Ok(Some(ReviewJudgmentPlan::new(
        pass,
        run,
        decode_digest(row.try_get("template_digest")?)?,
        members,
    )))
}

async fn load_applied_effects_impl(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Vec<ReviewJudgmentEffectId>, ReviewOrchestrationStoreError> {
    let rows = sqlx::query(
        "SELECT member.finding_id, member.finding_run_id, member.finding_pass_id,
                attempt_row.target_id
           FROM review_orchestration_judgment_effect AS effect
           JOIN review_orchestration_judgment_member AS member
            ON member.attempt_id = effect.attempt_id
            AND member.member_ordinal = effect.effect_ordinal
           JOIN review_orchestration_attempt AS attempt_row
             ON attempt_row.attempt_id = effect.attempt_id
          WHERE effect.attempt_id = $1 ORDER BY effect.effect_ordinal",
    )
    .bind(attempt.as_uuid())
    .fetch_all(&store.pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(ReviewJudgmentEffectId::new(
                attempt,
                decode_finding_ref(row, "finding_id", "finding_run_id", "finding_pass_id")?,
            ))
        })
        .collect()
}

async fn record_applied_effect_impl(
    store: &PostgresReviewOrchestrationStore,
    effect: ReviewJudgmentEffectId,
) -> Result<ReviewDurableSealOutcome, ReviewOrchestrationStoreError> {
    let applied = load_applied_effects_impl(store, effect.attempt()).await?;
    if applied.contains(&effect) {
        return Ok(ReviewDurableSealOutcome::EqualReplay);
    }
    let ordinal: Option<i32> = sqlx::query_scalar(
        "SELECT member_ordinal FROM review_orchestration_judgment_member
          WHERE attempt_id = $1 AND finding_id = $2",
    )
    .bind(effect.attempt().as_uuid())
    .bind(effect.finding().finding().into_uuid())
    .fetch_optional(&store.pool)
    .await?;
    let Some(ordinal) = ordinal else {
        return Ok(ReviewDurableSealOutcome::Conflict);
    };
    if usize::try_from(ordinal).ok() != Some(applied.len()) {
        return Ok(ReviewDurableSealOutcome::Conflict);
    }
    sqlx::query(
        "INSERT INTO review_orchestration_judgment_effect
            (attempt_id, effect_ordinal, finding_id) VALUES ($1,$2,$3)",
    )
    .bind(effect.attempt().as_uuid())
    .bind(ordinal)
    .bind(effect.finding().finding().into_uuid())
    .execute(&store.pool)
    .await?;
    Ok(ReviewDurableSealOutcome::Recorded)
}

async fn seal_finding_inventory(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
    findings: &[ReviewFindingRef],
    stage: &'static str,
) -> Result<ReviewDurableSealOutcome, ReviewOrchestrationStoreError> {
    let (seal_sql, member_sql) = match stage {
        "repair" => (
            "INSERT INTO review_orchestration_repair_inventory_seal (attempt_id) VALUES ($1)",
            "INSERT INTO review_orchestration_repair_inventory
                (attempt_id, member_ordinal, finding_id, finding_run_id, finding_pass_id)
             VALUES ($1,$2,$3,$4,$5)",
        ),
        "publication" => (
            "INSERT INTO review_orchestration_publication_inventory_seal (attempt_id) VALUES ($1)",
            "INSERT INTO review_orchestration_publication_inventory
                (attempt_id, member_ordinal, finding_id, finding_run_id, finding_pass_id)
             VALUES ($1,$2,$3,$4,$5)",
        ),
        _ => return Err(corruption("finding inventory stage is not closed")),
    };
    let existing = load_finding_inventory(store, attempt, stage).await?;
    if let Some(existing) = existing {
        return Ok(if existing == findings {
            ReviewDurableSealOutcome::EqualReplay
        } else {
            ReviewDurableSealOutcome::Conflict
        });
    }
    let mut transaction = store.pool.begin().await?;
    sqlx::query(seal_sql)
        .bind(attempt.as_uuid())
        .execute(&mut *transaction)
        .await?;
    for (ordinal, finding) in findings.iter().enumerate() {
        sqlx::query(member_sql)
            .bind(attempt.as_uuid())
            .bind(ordinal_i32(ordinal)?)
            .bind(finding.finding().into_uuid())
            .bind(finding.run().run().into_uuid())
            .bind(finding.pass().pass().into_uuid())
            .execute(&mut *transaction)
            .await?;
    }
    commit_or_ambiguous(transaction).await?;
    Ok(ReviewDurableSealOutcome::Recorded)
}

async fn load_finding_inventory(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
    stage: &'static str,
) -> Result<Option<Vec<ReviewFindingRef>>, ReviewOrchestrationStoreError> {
    let (seal_sql, member_sql) = match stage {
        "repair" => (
            "SELECT 1 FROM review_orchestration_repair_inventory_seal WHERE attempt_id = $1",
            "SELECT member.finding_id, member.finding_run_id, member.finding_pass_id,
                    attempt.target_id
               FROM review_orchestration_repair_inventory AS member
               JOIN review_orchestration_attempt AS attempt USING (attempt_id)
              WHERE member.attempt_id = $1 ORDER BY member_ordinal",
        ),
        "publication" => (
            "SELECT 1 FROM review_orchestration_publication_inventory_seal WHERE attempt_id = $1",
            "SELECT member.finding_id, member.finding_run_id, member.finding_pass_id,
                    attempt.target_id
               FROM review_orchestration_publication_inventory AS member
               JOIN review_orchestration_attempt AS attempt USING (attempt_id)
              WHERE member.attempt_id = $1 ORDER BY member_ordinal",
        ),
        _ => return Err(corruption("finding inventory stage is not closed")),
    };
    if sqlx::query(seal_sql)
        .bind(attempt.as_uuid())
        .fetch_optional(&store.pool)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let rows = sqlx::query(member_sql)
        .bind(attempt.as_uuid())
        .fetch_all(&store.pool)
        .await?;
    Ok(Some(
        rows.iter()
            .map(|row| decode_finding_ref(row, "finding_id", "finding_run_id", "finding_pass_id"))
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

async fn record_repairs(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
    outcomes: Vec<ReviewRepairMemberOutcome>,
) -> Result<ReviewDurableSealOutcome, ReviewOrchestrationStoreError> {
    if let Some(existing) = load_repairs(store, attempt).await? {
        return Ok(if existing == outcomes {
            ReviewDurableSealOutcome::EqualReplay
        } else {
            ReviewDurableSealOutcome::Conflict
        });
    }
    let inventory = load_finding_inventory(store, attempt, "repair")
        .await?
        .ok_or_else(|| corruption("repair outcomes lack an inventory seal"))?;
    if inventory != outcomes.iter().map(repair_finding).collect::<Vec<_>>() {
        return Ok(ReviewDurableSealOutcome::Conflict);
    }
    let mut transaction = store.pool.begin().await?;
    sqlx::query("INSERT INTO review_orchestration_repair_outcome_seal (attempt_id) VALUES ($1)")
        .bind(attempt.as_uuid())
        .execute(&mut *transaction)
        .await?;
    for (ordinal, outcome) in outcomes.iter().enumerate() {
        let (kind, event, template) = match outcome {
            ReviewRepairMemberOutcome::Fixed(success) => (
                "fixed",
                Some(i64::from(success.event().ordinal().get())),
                Some(success.template_digest()),
            ),
            ReviewRepairMemberOutcome::Failed(_) => ("failed", None, None),
            ReviewRepairMemberOutcome::Blocked(_) => ("blocked", None, None),
            ReviewRepairMemberOutcome::Cancelled(_) => ("cancelled", None, None),
        };
        sqlx::query(
            "INSERT INTO review_orchestration_repair_outcome
                (attempt_id, member_ordinal, finding_id, outcome_kind,
                 event_ordinal, template_digest)
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(attempt.as_uuid())
        .bind(ordinal_i32(ordinal)?)
        .bind(repair_finding(outcome).finding().into_uuid())
        .bind(kind)
        .bind(event)
        .bind(
            template
                .map(ReviewTemplateDigest::bytes)
                .as_ref()
                .map(<[u8; 32]>::as_slice),
        )
        .execute(&mut *transaction)
        .await?;
    }
    commit_or_ambiguous(transaction).await?;
    Ok(ReviewDurableSealOutcome::Recorded)
}

async fn load_repairs(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<Vec<ReviewRepairMemberOutcome>>, ReviewOrchestrationStoreError> {
    if sqlx::query("SELECT 1 FROM review_orchestration_repair_outcome_seal WHERE attempt_id = $1")
        .bind(attempt.as_uuid())
        .fetch_optional(&store.pool)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let inventory = load_finding_inventory(store, attempt, "repair")
        .await?
        .ok_or_else(|| corruption("repair outcome seal lacks inventory"))?;
    let rows = sqlx::query(
        "SELECT finding_id, outcome_kind, event_ordinal, template_digest
           FROM review_orchestration_repair_outcome
          WHERE attempt_id = $1 ORDER BY member_ordinal",
    )
    .bind(attempt.as_uuid())
    .fetch_all(&store.pool)
    .await?;
    if rows.len() != inventory.len() {
        return Err(corruption("repair outcomes do not cover their inventory"));
    }
    let mut outcomes = Vec::with_capacity(rows.len());
    for (row, finding_ref) in rows.iter().zip(inventory) {
        if ReviewFindingId::from_uuid(row.try_get("finding_id")?) != finding_ref.finding() {
            return Err(corruption("repair outcome finding order differs"));
        }
        let kind: String = row.try_get("outcome_kind")?;
        outcomes.push(match kind.as_str() {
            "fixed" => {
                let finding = store
                    .workflow
                    .load_finding(finding_ref.finding())
                    .await?
                    .ok_or_else(|| corruption("fixed finding is missing"))?;
                let ordinal = usize::try_from(row.try_get::<i64, _>("event_ordinal")?)
                    .ok()
                    .and_then(|value| value.checked_sub(1))
                    .ok_or_else(|| corruption("repair event ordinal is invalid"))?;
                let event = finding
                    .events()
                    .get(ordinal)
                    .ok_or_else(|| corruption("repair event is missing"))?
                    .clone();
                ReviewRepairMemberOutcome::Fixed(Box::new(ReviewRepairSuccess::new(
                    event,
                    decode_digest(row.try_get("template_digest")?)?,
                )))
            }
            "failed" => ReviewRepairMemberOutcome::Failed(finding_ref),
            "blocked" => ReviewRepairMemberOutcome::Blocked(finding_ref),
            "cancelled" => ReviewRepairMemberOutcome::Cancelled(finding_ref),
            _ => return Err(corruption("repair outcome is not closed")),
        });
    }
    Ok(Some(outcomes))
}

async fn record_publications(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
    outcomes: Vec<ReviewPublicationMemberOutcome>,
) -> Result<ReviewDurableSealOutcome, ReviewOrchestrationStoreError> {
    if let Some(existing) = load_publications(store, attempt).await? {
        return Ok(if existing == outcomes {
            ReviewDurableSealOutcome::EqualReplay
        } else {
            ReviewDurableSealOutcome::Conflict
        });
    }
    let inventory = load_finding_inventory(store, attempt, "publication")
        .await?
        .ok_or_else(|| corruption("publication outcomes lack an inventory seal"))?;
    if inventory != outcomes.iter().map(publication_finding).collect::<Vec<_>>() {
        return Ok(ReviewDurableSealOutcome::Conflict);
    }
    let mut transaction = store.pool.begin().await?;
    sqlx::query(
        "INSERT INTO review_orchestration_publication_outcome_seal (attempt_id) VALUES ($1)",
    )
    .bind(attempt.as_uuid())
    .execute(&mut *transaction)
    .await?;
    for (ordinal, outcome) in outcomes.iter().enumerate() {
        let (kind, link, template) = match outcome {
            ReviewPublicationMemberOutcome::Published(success) => (
                "published",
                Some(success.link().link()),
                Some(success.template_digest()),
            ),
            ReviewPublicationMemberOutcome::Failed(_) => ("failed", None, None),
            ReviewPublicationMemberOutcome::Blocked(_) => ("blocked", None, None),
            ReviewPublicationMemberOutcome::Cancelled(_) => ("cancelled", None, None),
        };
        sqlx::query(
            "INSERT INTO review_orchestration_publication_outcome
                (attempt_id, member_ordinal, finding_id, outcome_kind,
                 external_link_id, template_digest)
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(attempt.as_uuid())
        .bind(ordinal_i32(ordinal)?)
        .bind(publication_finding(outcome).finding().into_uuid())
        .bind(kind)
        .bind(link.map(ReviewExternalLinkId::into_uuid))
        .bind(
            template
                .map(ReviewTemplateDigest::bytes)
                .as_ref()
                .map(<[u8; 32]>::as_slice),
        )
        .execute(&mut *transaction)
        .await?;
    }
    commit_or_ambiguous(transaction).await?;
    Ok(ReviewDurableSealOutcome::Recorded)
}

async fn load_publications(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<Vec<ReviewPublicationMemberOutcome>>, ReviewOrchestrationStoreError> {
    if sqlx::query(
        "SELECT 1 FROM review_orchestration_publication_outcome_seal WHERE attempt_id = $1",
    )
    .bind(attempt.as_uuid())
    .fetch_optional(&store.pool)
    .await?
    .is_none()
    {
        return Ok(None);
    }
    let inventory = load_finding_inventory(store, attempt, "publication")
        .await?
        .ok_or_else(|| corruption("publication outcome seal lacks inventory"))?;
    let rows = sqlx::query(
        "SELECT finding_id, outcome_kind, external_link_id, template_digest
           FROM review_orchestration_publication_outcome
          WHERE attempt_id = $1 ORDER BY member_ordinal",
    )
    .bind(attempt.as_uuid())
    .fetch_all(&store.pool)
    .await?;
    if rows.len() != inventory.len() {
        return Err(corruption(
            "publication outcomes do not cover their inventory",
        ));
    }
    let mut outcomes = Vec::with_capacity(rows.len());
    for (row, finding) in rows.iter().zip(inventory) {
        if ReviewFindingId::from_uuid(row.try_get("finding_id")?) != finding.finding() {
            return Err(corruption("publication outcome finding order differs"));
        }
        let kind: String = row.try_get("outcome_kind")?;
        outcomes.push(match kind.as_str() {
            "published" => {
                let link = store
                    .workflow
                    .load_external_link(ReviewExternalLinkId::from_uuid(
                        row.try_get("external_link_id")?,
                    ))
                    .await?
                    .ok_or_else(|| corruption("published external link is missing"))?;
                let reference = ReviewFindingExternalLinkRef::try_new(finding, &link)
                    .map_err(|_| corruption("published link is not canonical"))?;
                let (_, run) = store
                    .load_evidence(reference.attachment_pass().reference().pass())
                    .await?;
                ReviewPublicationMemberOutcome::Published(Box::new(ReviewPublicationSuccess::new(
                    reference,
                    run,
                    decode_digest(row.try_get("template_digest")?)?,
                )))
            }
            "failed" => ReviewPublicationMemberOutcome::Failed(finding),
            "blocked" => ReviewPublicationMemberOutcome::Blocked(finding),
            "cancelled" => ReviewPublicationMemberOutcome::Cancelled(finding),
            _ => return Err(corruption("publication outcome is not closed")),
        });
    }
    Ok(Some(outcomes))
}

fn repair_finding(outcome: &ReviewRepairMemberOutcome) -> ReviewFindingRef {
    match outcome {
        ReviewRepairMemberOutcome::Fixed(success) => success.finding(),
        ReviewRepairMemberOutcome::Failed(finding)
        | ReviewRepairMemberOutcome::Blocked(finding)
        | ReviewRepairMemberOutcome::Cancelled(finding) => *finding,
    }
}

fn publication_finding(outcome: &ReviewPublicationMemberOutcome) -> ReviewFindingRef {
    match outcome {
        ReviewPublicationMemberOutcome::Published(success) => success.finding(),
        ReviewPublicationMemberOutcome::Failed(finding)
        | ReviewPublicationMemberOutcome::Blocked(finding)
        | ReviewPublicationMemberOutcome::Cancelled(finding) => *finding,
    }
}

async fn verify_finding_ref(
    store: &PostgresReviewOrchestrationStore,
    reference: ReviewFindingRef,
) -> Result<(), ReviewOrchestrationStoreError> {
    let canonical = store
        .workflow
        .load_finding(reference.finding())
        .await?
        .ok_or_else(|| corruption("referenced orchestration finding is missing"))?;
    if canonical.proposal().reference() != reference {
        return Err(corruption(
            "orchestration finding ancestry is not canonical",
        ));
    }
    Ok(())
}

fn encode_disposition(
    disposition: &ReviewPlannedDisposition,
) -> (&'static str, Option<&str>, Option<ReviewFindingRef>) {
    match disposition {
        ReviewPlannedDisposition::Accepted => ("accepted", None, None),
        ReviewPlannedDisposition::Rejected { reason } => ("rejected", Some(reason.as_str()), None),
        ReviewPlannedDisposition::Duplicate { canonical } => ("duplicate", None, Some(*canonical)),
        ReviewPlannedDisposition::Superseded { successor } => {
            ("superseded", None, Some(*successor))
        }
        ReviewPlannedDisposition::Stale => ("stale", None, None),
    }
}

fn decode_finding_ref(
    row: &PgRow,
    finding_column: &str,
    run_column: &str,
    pass_column: &str,
) -> Result<ReviewFindingRef, ReviewOrchestrationStoreError> {
    let target = ReviewTargetId::from_uuid(row.try_get("target_id")?);
    let run = ReviewRunRef::new(target, ReviewRunId::from_uuid(row.try_get(run_column)?));
    Ok(ReviewFindingRef::new(
        ReviewPassRef::new(run, ReviewPassId::from_uuid(row.try_get(pass_column)?)),
        ReviewFindingId::from_uuid(row.try_get(finding_column)?),
    ))
}

fn decode_optional_referenced_finding(
    row: &PgRow,
) -> Result<Option<ReviewFindingRef>, ReviewOrchestrationStoreError> {
    let finding: Option<Uuid> = row.try_get("referenced_finding_id")?;
    let run: Option<Uuid> = row.try_get("referenced_run_id")?;
    let pass: Option<Uuid> = row.try_get("referenced_pass_id")?;
    match (finding, run, pass) {
        (None, None, None) => Ok(None),
        (Some(finding), Some(run), Some(pass)) => {
            let target: Uuid = row.try_get("target_id")?;
            Ok(Some(ReviewFindingRef::new(
                ReviewPassRef::new(
                    ReviewRunRef::new(
                        ReviewTargetId::from_uuid(target),
                        ReviewRunId::from_uuid(run),
                    ),
                    ReviewPassId::from_uuid(pass),
                ),
                ReviewFindingId::from_uuid(finding),
            )))
        }
        _ => Err(corruption("referenced finding ancestry is partial")),
    }
}

enum CommandInspection {
    New,
    Recorded(ReviewOrchestrationCommandResult),
    Conflicting,
}

async fn inspect_command(
    transaction: &mut Transaction<'_, Postgres>,
    command: ReviewOrchestrationCommand,
) -> Result<CommandInspection, ReviewOrchestrationStoreError> {
    let kind = command_registry::inspect(transaction, command.command_id)
        .await
        .map_err(registry_error)?;
    match kind {
        None => Ok(CommandInspection::New),
        Some(CommandKind::ReviewOrchestration) => {
            let row = sqlx::query(
                "SELECT semantic_digest, attempt_id, operation_kind, result_stage,
                        import_recorded, concern_claim_count, fanout_sealed,
                        judgment_plan_sealed, judgment_effect_count,
                        repair_inventory_count, repair_outcomes_recorded,
                        publication_inventory_count, publication_outcomes_recorded
                   FROM review_orchestration_command WHERE command_id = $1",
            )
            .bind(durable_command_id_to_uuid(command.command_id))
            .fetch_one(&mut **transaction)
            .await?;
            let digest: Vec<u8> = row.try_get("semantic_digest")?;
            if digest.as_slice() != command.semantic_digest
                || ReviewOrchestrationAttemptId::from_uuid(row.try_get("attempt_id")?)
                    != command.attempt
                || row.try_get::<String, _>("operation_kind")? != encode_command_kind(command.kind)
            {
                return Ok(CommandInspection::Conflicting);
            }
            Ok(CommandInspection::Recorded(decode_command_result(&row)?))
        }
        Some(_) => Ok(CommandInspection::Conflicting),
    }
}

async fn insert_command_receipt(
    transaction: &mut Transaction<'_, Postgres>,
    command: &ReviewOrchestrationCommand,
    result: &ReviewOrchestrationCommandResult,
) -> Result<(), ReviewOrchestrationStoreError> {
    sqlx::query(
        "INSERT INTO review_orchestration_command
            (command_id, command_kind, storage_version, semantic_digest,
             attempt_id, operation_kind, result_stage, import_recorded,
             concern_claim_count, fanout_sealed, judgment_plan_sealed,
             judgment_effect_count, repair_inventory_count,
             repair_outcomes_recorded, publication_inventory_count,
             publication_outcomes_recorded)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)",
    )
    .bind(durable_command_id_to_uuid(command.command_id))
    .bind(REVIEW_ORCHESTRATION_KIND)
    .bind(STORAGE_VERSION)
    .bind(command.semantic_digest.as_slice())
    .bind(command.attempt.as_uuid())
    .bind(encode_command_kind(command.kind))
    .bind(encode_stage(result.stage))
    .bind(result.progress.import_recorded)
    .bind(count_i64(result.progress.concern_claim_count)?)
    .bind(result.progress.fanout_sealed)
    .bind(result.progress.judgment_plan_sealed)
    .bind(count_i64(result.progress.judgment_effect_count)?)
    .bind(
        result
            .progress
            .repair_inventory_count
            .map(count_i64)
            .transpose()?,
    )
    .bind(result.progress.repair_outcomes_recorded)
    .bind(
        result
            .progress
            .publication_inventory_count
            .map(count_i64)
            .transpose()?,
    )
    .bind(result.progress.publication_outcomes_recorded)
    .execute(&mut **transaction)
    .await
    .map_err(ReviewOrchestrationStoreError::CommitAmbiguous)?;
    Ok(())
}

fn decode_command_result(
    row: &PgRow,
) -> Result<ReviewOrchestrationCommandResult, ReviewOrchestrationStoreError> {
    Ok(ReviewOrchestrationCommandResult {
        attempt: ReviewOrchestrationAttemptId::from_uuid(row.try_get("attempt_id")?),
        stage: decode_stage(row.try_get("result_stage")?)?,
        progress: decode_progress(row)?,
    })
}

fn decode_progress(
    row: &PgRow,
) -> Result<ReviewOrchestrationProgress, ReviewOrchestrationStoreError> {
    Ok(ReviewOrchestrationProgress {
        import_recorded: row.try_get("import_recorded")?,
        concern_claim_count: decode_count(row.try_get("concern_claim_count")?)?,
        fanout_sealed: row.try_get("fanout_sealed")?,
        judgment_plan_sealed: row.try_get("judgment_plan_sealed")?,
        judgment_effect_count: decode_count(row.try_get("judgment_effect_count")?)?,
        repair_inventory_count: row
            .try_get::<Option<i64>, _>("repair_inventory_count")?
            .map(decode_count)
            .transpose()?,
        repair_outcomes_recorded: row.try_get("repair_outcomes_recorded")?,
        publication_inventory_count: row
            .try_get::<Option<i64>, _>("publication_inventory_count")?
            .map(decode_count)
            .transpose()?,
        publication_outcomes_recorded: row.try_get("publication_outcomes_recorded")?,
    })
}

const fn encode_command_kind(kind: ReviewOrchestrationCommandKind) -> &'static str {
    match kind {
        ReviewOrchestrationCommandKind::Start => "start",
        ReviewOrchestrationCommandKind::Import => "import",
        ReviewOrchestrationCommandKind::Concern => "concern",
        ReviewOrchestrationCommandKind::JudgmentPlan => "judgment_plan",
        ReviewOrchestrationCommandKind::JudgmentEffect => "judgment_effect",
        ReviewOrchestrationCommandKind::Repair => "repair",
        ReviewOrchestrationCommandKind::Publication => "publication",
    }
}

const fn encode_stage(stage: ReviewOrchestrationStage) -> &'static str {
    match stage {
        ReviewOrchestrationStage::Started => "started",
        ReviewOrchestrationStage::AwaitingImport => "awaiting_import",
        ReviewOrchestrationStage::ImportIncomplete => "import_incomplete",
        ReviewOrchestrationStage::AwaitingConcerns => "awaiting_concerns",
        ReviewOrchestrationStage::FanoutIncomplete => "fanout_incomplete",
        ReviewOrchestrationStage::AwaitingJudgment => "awaiting_judgment",
        ReviewOrchestrationStage::AwaitingJudgmentEffects => "awaiting_judgment_effects",
        ReviewOrchestrationStage::JudgmentIncomplete => "judgment_incomplete",
        ReviewOrchestrationStage::AwaitingRepair => "awaiting_repair",
        ReviewOrchestrationStage::RepairIncomplete => "repair_incomplete",
        ReviewOrchestrationStage::AwaitingPublication => "awaiting_publication",
        ReviewOrchestrationStage::PublicationIncomplete => "publication_incomplete",
        ReviewOrchestrationStage::Complete => "complete",
    }
}

fn decode_stage(value: String) -> Result<ReviewOrchestrationStage, ReviewOrchestrationStoreError> {
    match value.as_str() {
        "started" => Ok(ReviewOrchestrationStage::Started),
        "awaiting_import" => Ok(ReviewOrchestrationStage::AwaitingImport),
        "import_incomplete" => Ok(ReviewOrchestrationStage::ImportIncomplete),
        "awaiting_concerns" => Ok(ReviewOrchestrationStage::AwaitingConcerns),
        "fanout_incomplete" => Ok(ReviewOrchestrationStage::FanoutIncomplete),
        "awaiting_judgment" => Ok(ReviewOrchestrationStage::AwaitingJudgment),
        "awaiting_judgment_effects" => Ok(ReviewOrchestrationStage::AwaitingJudgmentEffects),
        "judgment_incomplete" => Ok(ReviewOrchestrationStage::JudgmentIncomplete),
        "awaiting_repair" => Ok(ReviewOrchestrationStage::AwaitingRepair),
        "repair_incomplete" => Ok(ReviewOrchestrationStage::RepairIncomplete),
        "awaiting_publication" => Ok(ReviewOrchestrationStage::AwaitingPublication),
        "publication_incomplete" => Ok(ReviewOrchestrationStage::PublicationIncomplete),
        "complete" => Ok(ReviewOrchestrationStage::Complete),
        _ => Err(corruption(
            "orchestration command result stage is not closed",
        )),
    }
}

fn count_i64(value: usize) -> Result<i64, ReviewOrchestrationStoreError> {
    i64::try_from(value).map_err(|_| corruption("orchestration count is too large"))
}

fn decode_count(value: i64) -> Result<usize, ReviewOrchestrationStoreError> {
    usize::try_from(value).map_err(|_| corruption("stored orchestration count is invalid"))
}

fn registry_error(error: RegistryInspectionError) -> ReviewOrchestrationStoreError {
    match error {
        RegistryInspectionError::Database(error) => ReviewOrchestrationStoreError::Database(error),
        RegistryInspectionError::Corruption(_) => {
            corruption("owner-global command registry is corrupt")
        }
    }
}

async fn commit_or_ambiguous(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), ReviewOrchestrationStoreError> {
    transaction
        .commit()
        .await
        .map_err(ReviewOrchestrationStoreError::CommitAmbiguous)
}

fn encode_import(
    outcome: &ReviewImportOutcome,
) -> (
    &'static str,
    Option<ReviewPassId>,
    Option<ReviewExternalLinkId>,
    ReviewTemplateDigest,
    Option<[u8; 32]>,
) {
    match outcome {
        ReviewImportOutcome::Succeeded {
            pass,
            external_link,
            template_digest,
            context,
            ..
        } => (
            "succeeded",
            Some(pass.reference().pass()),
            external_link.as_ref().map(|link| link.id()),
            *template_digest,
            Some(context.digest()),
        ),
        ReviewImportOutcome::Incomplete {
            pass,
            template_digest,
            status,
            ..
        } => (
            encode_incomplete_status(*status),
            pass.as_ref().map(|pass| pass.reference().pass()),
            None,
            *template_digest,
            None,
        ),
    }
}

fn encode_concern_outcome(outcome: &ReviewConcernOutcome) -> (&'static str, Option<ReviewPassId>) {
    match outcome {
        ReviewConcernOutcome::Succeeded(success) => ("succeeded", Some(success.producer().pass())),
        ReviewConcernOutcome::Failed { pass } => ("failed", Some(pass.pass())),
        ReviewConcernOutcome::Blocked { pass } => ("blocked", Some(pass.pass())),
        ReviewConcernOutcome::Cancelled { pass } => ("cancelled", pass.map(ReviewPassRef::pass)),
        ReviewConcernOutcome::Superseded { pass } => ("superseded", Some(pass.pass())),
    }
}

const fn encode_incomplete_status(status: ReviewPassIncompleteStatus) -> &'static str {
    match status {
        ReviewPassIncompleteStatus::Failed => "failed",
        ReviewPassIncompleteStatus::Blocked => "blocked",
        ReviewPassIncompleteStatus::Cancelled => "cancelled",
    }
}

fn decode_incomplete_status(
    value: &str,
) -> Result<ReviewPassIncompleteStatus, ReviewOrchestrationStoreError> {
    match value {
        "failed" => Ok(ReviewPassIncompleteStatus::Failed),
        "blocked" => Ok(ReviewPassIncompleteStatus::Blocked),
        "cancelled" => Ok(ReviewPassIncompleteStatus::Cancelled),
        _ => Err(corruption("incomplete pass status is not closed")),
    }
}

fn decode_key(value: String) -> Result<ReviewKey, ReviewOrchestrationStoreError> {
    ReviewKey::try_new(value).map_err(|_| corruption("stored review key is invalid"))
}

fn decode_digest(value: Vec<u8>) -> Result<ReviewTemplateDigest, ReviewOrchestrationStoreError> {
    Ok(ReviewTemplateDigest::new(decode_bytes(value)?))
}

fn decode_bytes(value: Vec<u8>) -> Result<[u8; 32], ReviewOrchestrationStoreError> {
    value
        .try_into()
        .map_err(|_| corruption("stored digest length is invalid"))
}

fn decode_policy(row: &PgRow) -> Result<ReviewPolicy, ReviewOrchestrationStoreError> {
    let version = u32::try_from(row.try_get::<i64, _>("policy_version")?)
        .ok()
        .and_then(|value| ReviewPolicyVersion::try_new(value).ok())
        .ok_or_else(|| corruption("stored policy version is invalid"))?;
    let judge = u16::try_from(row.try_get::<i32, _>("minimum_judge_confidence")?)
        .ok()
        .and_then(|value| ReviewConfidence::try_from_basis_points(value).ok())
        .ok_or_else(|| corruption("stored judgment confidence is invalid"))?;
    let publication = u16::try_from(row.try_get::<i32, _>("minimum_publication_confidence")?)
        .ok()
        .and_then(|value| ReviewConfidence::try_from_basis_points(value).ok())
        .ok_or_else(|| corruption("stored publication confidence is invalid"))?;
    ReviewPolicy::try_new(version, judge, publication)
        .map_err(|_| corruption("stored review policy is invalid"))
}

fn ordinal_i32(value: usize) -> Result<i32, ReviewOrchestrationStoreError> {
    i32::try_from(value).map_err(|_| corruption("orchestration inventory is too large"))
}

/// PostgreSQL or fail-closed reconstruction failure.
#[derive(Debug)]
pub enum ReviewOrchestrationStoreError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    Workflow(ReviewWorkflowStoreError),
    Corruption(&'static str),
}

impl fmt::Display for ReviewOrchestrationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "review orchestration database failure: {error}")
            }
            Self::CommitAmbiguous(error) => write!(
                formatter,
                "review orchestration commit is ambiguous: {error}"
            ),
            Self::Workflow(error) => write!(
                formatter,
                "review orchestration canonical evidence failed: {error}"
            ),
            Self::Corruption(detail) => write!(
                formatter,
                "review orchestration durable facts are corrupt: {detail}"
            ),
        }
    }
}

impl Error for ReviewOrchestrationStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Workflow(error) => Some(error),
            Self::Corruption(_) => None,
        }
    }
}

impl From<sqlx::Error> for ReviewOrchestrationStoreError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}

impl From<ReviewWorkflowStoreError> for ReviewOrchestrationStoreError {
    fn from(value: ReviewWorkflowStoreError) -> Self {
        Self::Workflow(value)
    }
}

const fn corruption(detail: &'static str) -> ReviewOrchestrationStoreError {
    ReviewOrchestrationStoreError::Corruption(detail)
}

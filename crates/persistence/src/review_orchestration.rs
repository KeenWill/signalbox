//! PostgreSQL persistence for resumable review-orchestration attempts.
//!
//! These tables retain orchestration inventory and barrier facts. Canonical
//! run, pass, finding, event, and external-link values are always reconstructed
//! through [`ReviewWorkflowStore`].

use std::{error::Error, fmt};

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
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::REVIEW_ORCHESTRATION_KIND,
    mapping::durable_command_id_to_uuid,
    review_workflow::{
        ReviewWorkflowStore, ReviewWorkflowStoreError, begin_repeatable_read,
        load_pass_on_connection,
    },
};

const STORAGE_VERSION: i16 = 1;
/// PostgreSQL adapter for durable orchestration attempts and command receipts.
#[derive(Clone, Debug)]
pub struct PostgresReviewOrchestrationStore {
    pool: PgPool,
    workflow: ReviewWorkflowStore,
    effect_command: Option<ReviewOrchestrationCommand>,
}

impl PostgresReviewOrchestrationStore {
    /// Records the response recovered after the aggregate effect commits and
    /// before the user-global command receipt is committed.
    pub async fn record_command_recovery(
        &self,
        command: ReviewOrchestrationCommand,
        result: ReviewOrchestrationCommandResult,
    ) -> Result<ReviewOrchestrationCommandResult, ReviewOrchestrationStoreError> {
        if result.attempt != command.attempt {
            return Err(corruption(
                "orchestration command recovery names another attempt",
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let inserted = insert_command_recovery(&mut transaction, &command, &result).await?;
        if inserted {
            commit_or_ambiguous(transaction).await?;
            return Ok(result);
        }
        let inspection = inspect_command_recovery(&mut transaction, command).await?;
        transaction.rollback().await?;
        match inspection {
            CommandInspection::Recorded(existing) if existing == result => Ok(existing),
            CommandInspection::Recorded(_)
            | CommandInspection::Conflicting
            | CommandInspection::Pending => Err(corruption(
                "orchestration command recovery conflicts with its immutable record",
            )),
            CommandInspection::New => Err(corruption(
                "orchestration command recovery conflict disappeared",
            )),
        }
    }

    /// Binds orchestration storage to the same guarded pool as review workflow storage.
    pub fn new(pool: PgPool) -> Self {
        Self {
            workflow: ReviewWorkflowStore::new(pool.clone()),
            pool,
            effect_command: None,
        }
    }
    /// Binds primary orchestration effects to one already-claimed durable command.
    pub fn for_command(pool: PgPool, command: ReviewOrchestrationCommand) -> Self {
        Self {
            workflow: ReviewWorkflowStore::new(pool.clone()),
            pool,
            effect_command: Some(command),
        }
    }

    /// Reports whether this exact pending command already committed its primary effect.
    pub async fn command_effect_is_recorded(
        &self,
        command: ReviewOrchestrationCommand,
    ) -> Result<bool, ReviewOrchestrationStoreError> {
        sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM review_orchestration_command_effect
                 WHERE command_id = $1 AND attempt_id = $2 AND operation_kind = $3
            )",
        )
        .bind(durable_command_id_to_uuid(command.command_id))
        .bind(command.attempt.as_uuid())
        .bind(encode_command_kind(command.kind))
        .fetch_one(&self.pool)
        .await
        .map_err(ReviewOrchestrationStoreError::Database)
    }

    /// Loads and fail-closed reconstructs one immutable attempt.
    pub async fn load_attempt(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationAttempt>, ReviewOrchestrationStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self
            .load_attempt_on_connection(&mut transaction, attempt)
            .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    async fn load_attempt_on_connection(
        &self,
        connection: &mut PgConnection,
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
        .fetch_optional(&mut *connection)
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
        .fetch_all(&mut *connection)
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

    /// Loads the exact concern claim and barrier facts caused by one command.
    pub async fn load_concern_replay_facts(
        &self,
        command: ReviewOrchestrationCommand,
        concern: &ReviewKey,
    ) -> Result<Option<(ReviewConcernClaim, usize, bool)>, ReviewOrchestrationStoreError> {
        let candidate = sqlx::query(
            "SELECT claim.concern_key, claim.claim_ordinal, claim.template_digest,
                    claim.outcome_kind, claim.pass_id, claim.effect_sequence
               FROM review_orchestration_command_effect AS effect
               JOIN review_orchestration_concern_claim AS claim
                 ON claim.effect_sequence = effect.concern_effect_sequence
                AND claim.attempt_id = effect.attempt_id
              WHERE effect.command_id = $1
                AND effect.attempt_id = $2
                AND effect.operation_kind = 'concern'
                AND claim.concern_key = $3",
        )
        .bind(durable_command_id_to_uuid(command.command_id))
        .bind(command.attempt.as_uuid())
        .bind(concern.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let sequence: i64 = candidate.try_get("effect_sequence")?;
        let claim = self
            .decode_concern_claim(command.attempt, &candidate)
            .await?;
        let row = sqlx::query(
            "WITH claims_at_effect AS (
                 SELECT DISTINCT ON (claim.concern_key)
                        claim.concern_key, claim.outcome_kind
                   FROM review_orchestration_concern_claim AS claim
                  WHERE claim.attempt_id = $1 AND claim.effect_sequence <= $2
                  ORDER BY claim.concern_key, claim.claim_ordinal DESC
             )
             SELECT count(*) AS claim_count,
                    COALESCE(bool_and(outcome_kind = 'succeeded'), false) AS all_succeeded
               FROM claims_at_effect",
        )
        .bind(command.attempt.as_uuid())
        .bind(sequence)
        .fetch_one(&self.pool)
        .await?;
        Ok(Some((
            claim,
            decode_count(row.try_get("claim_count")?)?,
            row.try_get("all_succeeded")?,
        )))
    }

    /// Derives the deterministic daemon-visible stage from validated durable facts.
    pub async fn current_stage(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationCurrentStage>, ReviewOrchestrationStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let Some(immutable) = self
            .load_attempt_on_connection(&mut transaction, attempt)
            .await?
        else {
            transaction.commit().await?;
            return Ok(None);
        };
        let mut facts = AttemptFacts::new(self, attempt, immutable);
        let stage = facts.current_stage(&mut transaction).await?;
        transaction.commit().await?;
        Ok(Some(stage))
    }

    /// Loads every daemon adapter fact from one coherent database snapshot.
    ///
    /// The whole read runs inside a single read-only `REPEATABLE READ`
    /// transaction, so every loader observes the same database snapshot and the
    /// projection cannot be torn across statements. The read is pure — it holds
    /// no lock any writer waits on, so concurrent input, turns, approvals and
    /// review mutations proceed while it runs.
    pub async fn load_snapshot(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationSnapshotFacts>, ReviewOrchestrationStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let facts = self
            .load_snapshot_on_connection(&mut transaction, attempt)
            .await?;
        transaction.commit().await?;
        Ok(facts)
    }

    async fn load_snapshot_on_connection(
        &self,
        connection: &mut PgConnection,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewOrchestrationSnapshotFacts>, ReviewOrchestrationStoreError> {
        let Some(immutable) = self
            .load_attempt_on_connection(&mut *connection, attempt)
            .await?
        else {
            return Ok(None);
        };
        // The stage ladder consults almost every fact the snapshot carries, so
        // one memo serves both: the ladder decides from it, and the remaining
        // fields are then forced through the same memo rather than reloaded.
        // Deriving the stage from the very facts the snapshot reports also
        // makes the projection internally consistent by construction.
        let mut facts = AttemptFacts::new(self, attempt, immutable);
        let current_stage = facts.current_stage(&mut *connection).await?;
        Ok(Some(
            facts
                .into_snapshot_facts(&mut *connection, current_stage)
                .await?,
        ))
    }
}

/// Every durable fact one attempt's stage and snapshot consult, loaded at most
/// once each.
///
/// The stage ladder short-circuits at the earliest stage it can prove, so it
/// reads a prefix of these facts; the snapshot needs a fixed set of them
/// whatever the stage turns out to be. Holding both behind one memo keeps the
/// ladder's laziness while charging the snapshot for each loader exactly once.
struct AttemptFacts<'store> {
    store: &'store PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
    immutable: ReviewOrchestrationAttempt,
    import: Option<Option<ReviewImportOutcome>>,
    concern_claims: Option<Vec<ReviewConcernClaim>>,
    judgment_plan: Option<Option<ReviewJudgmentPlan>>,
    applied_judgment_effects: Option<Vec<ReviewJudgmentEffectId>>,
    repair_inventory_sealed: Option<bool>,
    repair_outcomes: Option<Option<Vec<ReviewRepairMemberOutcome>>>,
    publication_inventory_sealed: Option<bool>,
    publication_outcomes: Option<Option<Vec<ReviewPublicationMemberOutcome>>>,
}

impl<'store> AttemptFacts<'store> {
    const fn new(
        store: &'store PostgresReviewOrchestrationStore,
        attempt: ReviewOrchestrationAttemptId,
        immutable: ReviewOrchestrationAttempt,
    ) -> Self {
        Self {
            store,
            attempt,
            immutable,
            import: None,
            concern_claims: None,
            judgment_plan: None,
            applied_judgment_effects: None,
            repair_inventory_sealed: None,
            repair_outcomes: None,
            publication_inventory_sealed: None,
            publication_outcomes: None,
        }
    }

    /// Derives the deterministic daemon-visible stage, reading only the facts
    /// each decision needs to reach the next one.
    async fn current_stage(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<ReviewOrchestrationCurrentStage, ReviewOrchestrationStoreError> {
        let Some(import) = self.import(&mut *connection).await? else {
            return Ok(ReviewOrchestrationCurrentStage::AwaitingImport);
        };
        if matches!(import, ReviewImportOutcome::Incomplete { .. }) {
            return Ok(ReviewOrchestrationCurrentStage::ImportIncomplete);
        }
        let expected_concerns = self.immutable.concerns().len();
        if self.concern_claims(&mut *connection).await?.len() < expected_concerns {
            return Ok(ReviewOrchestrationCurrentStage::AwaitingConcerns);
        }
        if self
            .concern_claims(&mut *connection)
            .await?
            .iter()
            .any(|claim| !matches!(claim.outcome(), ReviewConcernOutcome::Succeeded(_)))
        {
            return Ok(ReviewOrchestrationCurrentStage::FanoutIncomplete);
        }
        let Some(planned_members) = self
            .judgment_plan(&mut *connection)
            .await?
            .map(|plan| plan.members().len())
        else {
            return Ok(ReviewOrchestrationCurrentStage::AwaitingJudgment);
        };
        let applied = self.applied_judgment_effects(&mut *connection).await?.len();
        if applied < planned_members {
            return Ok(
                if self
                    .judgment_was_interrupted(&mut *connection, applied)
                    .await?
                {
                    ReviewOrchestrationCurrentStage::JudgmentIncomplete
                } else {
                    ReviewOrchestrationCurrentStage::AwaitingJudgmentEffects
                },
            );
        }
        if !self.repair_inventory_sealed(&mut *connection).await? {
            return Ok(ReviewOrchestrationCurrentStage::AwaitingRepair);
        }
        let Some(repairs) = self.repair_outcomes(&mut *connection).await? else {
            return Ok(ReviewOrchestrationCurrentStage::AwaitingRepair);
        };
        if repairs
            .iter()
            .any(|outcome| matches!(outcome, ReviewRepairMemberOutcome::Blocked(_)))
        {
            return Ok(ReviewOrchestrationCurrentStage::RepairIncomplete);
        }
        if !self.publication_inventory_sealed(&mut *connection).await? {
            return Ok(ReviewOrchestrationCurrentStage::AwaitingPublication);
        }
        let Some(publications) = self.publication_outcomes(&mut *connection).await? else {
            return Ok(ReviewOrchestrationCurrentStage::AwaitingPublication);
        };
        Ok(
            if publications
                .iter()
                .all(|outcome| matches!(outcome, ReviewPublicationMemberOutcome::Published(_)))
            {
                ReviewOrchestrationCurrentStage::Complete
            } else {
                ReviewOrchestrationCurrentStage::PublicationIncomplete
            },
        )
    }

    /// Forces the facts the snapshot reports that the stage ladder may have
    /// short-circuited before reaching, then assembles the projection.
    async fn into_snapshot_facts(
        mut self,
        connection: &mut PgConnection,
        current_stage: ReviewOrchestrationCurrentStage,
    ) -> Result<ReviewOrchestrationSnapshotFacts, ReviewOrchestrationStoreError> {
        self.concern_claims(&mut *connection).await?;
        self.judgment_plan(&mut *connection).await?;
        self.applied_judgment_effects(&mut *connection).await?;
        self.repair_outcomes(&mut *connection).await?;
        self.publication_outcomes(&mut *connection).await?;
        Ok(ReviewOrchestrationSnapshotFacts {
            attempt: self.immutable,
            current_stage,
            concern_claims: self.concern_claims.unwrap_or_default(),
            judgment_plan: self.judgment_plan.flatten(),
            applied_judgment_effects: self.applied_judgment_effects.unwrap_or_default(),
            repair_outcomes: self.repair_outcomes.flatten(),
            publication_outcomes: self.publication_outcomes.flatten(),
        })
    }

    async fn import(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<Option<&ReviewImportOutcome>, ReviewOrchestrationStoreError> {
        if self.import.is_none() {
            self.import = Some(
                self.store
                    .load_import_on_connection(connection, self.attempt)
                    .await?,
            );
        }
        Ok(self.import.as_ref().and_then(Option::as_ref))
    }

    async fn concern_claims(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<&[ReviewConcernClaim], ReviewOrchestrationStoreError> {
        if self.concern_claims.is_none() {
            self.concern_claims = Some(
                self.store
                    .load_concern_claims_on_connection(connection, self.attempt)
                    .await?,
            );
        }
        Ok(self.concern_claims.as_deref().unwrap_or_default())
    }

    async fn judgment_plan(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<Option<&ReviewJudgmentPlan>, ReviewOrchestrationStoreError> {
        if self.judgment_plan.is_none() {
            self.judgment_plan =
                Some(load_judgment_plan_on_connection(self.store, connection, self.attempt).await?);
        }
        Ok(self.judgment_plan.as_ref().and_then(Option::as_ref))
    }

    async fn applied_judgment_effects(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<&[ReviewJudgmentEffectId], ReviewOrchestrationStoreError> {
        if self.applied_judgment_effects.is_none() {
            self.applied_judgment_effects =
                Some(load_applied_effects_on_connection(connection, self.attempt).await?);
        }
        Ok(self.applied_judgment_effects.as_deref().unwrap_or_default())
    }

    async fn repair_inventory_sealed(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<bool, ReviewOrchestrationStoreError> {
        if self.repair_inventory_sealed.is_none() {
            self.repair_inventory_sealed = Some(
                load_finding_inventory_on_connection(connection, self.attempt, "repair")
                    .await?
                    .is_some(),
            );
        }
        Ok(self.repair_inventory_sealed.unwrap_or_default())
    }

    async fn repair_outcomes(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<Option<&[ReviewRepairMemberOutcome]>, ReviewOrchestrationStoreError> {
        if self.repair_outcomes.is_none() {
            self.repair_outcomes =
                Some(load_repairs_on_connection(self.store, connection, self.attempt).await?);
        }
        Ok(self.repair_outcomes.as_ref().and_then(Option::as_deref))
    }

    async fn publication_inventory_sealed(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<bool, ReviewOrchestrationStoreError> {
        if self.publication_inventory_sealed.is_none() {
            self.publication_inventory_sealed = Some(
                load_finding_inventory_on_connection(connection, self.attempt, "publication")
                    .await?
                    .is_some(),
            );
        }
        Ok(self.publication_inventory_sealed.unwrap_or_default())
    }

    async fn publication_outcomes(
        &mut self,
        connection: &mut PgConnection,
    ) -> Result<Option<&[ReviewPublicationMemberOutcome]>, ReviewOrchestrationStoreError> {
        if self.publication_outcomes.is_none() {
            self.publication_outcomes =
                Some(load_publications_on_connection(self.store, connection, self.attempt).await?);
        }
        Ok(self
            .publication_outcomes
            .as_ref()
            .and_then(Option::as_deref))
    }

    async fn judgment_was_interrupted(
        &self,
        connection: &mut PgConnection,
        applied: usize,
    ) -> Result<bool, ReviewOrchestrationStoreError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM review_orchestration_command
                 WHERE attempt_id = $1
                   AND result_stage = 'judgment_incomplete'
                   AND judgment_effect_count = $2
                UNION ALL
                SELECT 1 FROM review_orchestration_command_recovery
                 WHERE attempt_id = $1
                   AND result_stage = 'judgment_incomplete'
                   AND judgment_effect_count = $2
            )",
        )
        .bind(self.attempt.as_uuid())
        .bind(count_i64(applied)?)
        .fetch_one(connection)
        .await?)
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

/// Closed user-global command kinds for orchestration mutations.
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

/// One checked user-global orchestration command claim.
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

/// Pre-effect user-global command claim result.
pub enum ReviewOrchestrationCommandClaim {
    ExistingRecorded(ReviewOrchestrationCommandResult),
    Conflicting,
    New(ReviewOrchestrationCommandGuard),
}

/// A durable pending command claim awaiting its typed response receipt.
pub struct ReviewOrchestrationCommandGuard {
    pool: PgPool,
    command: ReviewOrchestrationCommand,
    pending: bool,
}

impl ReviewOrchestrationCommandGuard {
    /// Reports whether this exact identity resumed a previously committed intent.
    pub const fn is_pending(&self) -> bool {
        self.pending
    }

    /// Records the exact response state and atomically replaces its durable intent.
    pub async fn record(
        self,
        result: ReviewOrchestrationCommandResult,
    ) -> Result<ReviewOrchestrationCommandResult, ReviewOrchestrationStoreError> {
        if result.attempt != self.command.attempt {
            return Err(corruption(
                "orchestration command result names another attempt",
            ));
        }
        let mut transaction = self.pool.begin().await?;
        match inspect_command(&mut transaction, self.command).await? {
            CommandInspection::Pending => {}
            CommandInspection::Recorded(existing) if existing == result => {
                transaction.commit().await?;
                return Ok(existing);
            }
            CommandInspection::Recorded(_)
            | CommandInspection::Conflicting
            | CommandInspection::New => {
                return Err(corruption(
                    "orchestration command intent changed before receipt replacement",
                ));
            }
        }
        insert_command_receipt(&mut transaction, &self.command, &result).await?;
        delete_command_intent(&mut transaction, &self.command).await?;
        transaction
            .commit()
            .await
            .map_err(ReviewOrchestrationStoreError::CommitAmbiguous)?;
        Ok(result)
    }
}

impl PostgresReviewOrchestrationStore {
    /// Durably claims a user-global command before its separately durable effect.
    ///
    /// A fresh claim commits an immutable typed intent. An exact retry observes
    /// that intent as pending, while a distinct payload conflicts before it can
    /// perform an effect.
    pub async fn begin_command(
        &self,
        command: ReviewOrchestrationCommand,
    ) -> Result<ReviewOrchestrationCommandClaim, ReviewOrchestrationStoreError> {
        let mut transaction = self.pool.begin().await?;
        let mut inspection = inspect_command(&mut transaction, command).await?;
        let mut fresh = false;
        if matches!(inspection, CommandInspection::New) {
            let issuer = crate::command_registry::issuer_columns(
                signalbox_domain::CommandPrincipal::Operator,
            );
            let inserted = sqlx::query(
                "INSERT INTO durable_command
                    (command_id, command_kind, storage_version, claimed_at,
                     issuer_kind, issuer_module)
                 VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
                 ON CONFLICT DO NOTHING",
            )
            .bind(durable_command_id_to_uuid(command.command_id))
            .bind(REVIEW_ORCHESTRATION_KIND)
            .bind(STORAGE_VERSION)
            .bind(issuer.0)
            .bind(issuer.1)
            .execute(&mut *transaction)
            .await?
            .rows_affected()
                == 1;
            if inserted {
                insert_command_intent(&mut transaction, &command).await?;
                inspection = CommandInspection::Pending;
                fresh = true;
            } else {
                inspection = inspect_command(&mut transaction, command).await?;
            }
        }
        match inspection {
            CommandInspection::Recorded(result) => {
                validate_recorded_recovery(&mut transaction, command, &result).await?;
                transaction.commit().await?;
                Ok(ReviewOrchestrationCommandClaim::ExistingRecorded(result))
            }
            CommandInspection::Conflicting => {
                transaction.rollback().await?;
                Ok(ReviewOrchestrationCommandClaim::Conflicting)
            }
            CommandInspection::Pending => {
                match inspect_command_recovery(&mut transaction, command).await? {
                    CommandInspection::Recorded(result) => {
                        insert_command_receipt(&mut transaction, &command, &result).await?;
                        delete_command_intent(&mut transaction, &command).await?;
                        commit_or_ambiguous(transaction).await?;
                        Ok(ReviewOrchestrationCommandClaim::ExistingRecorded(result))
                    }
                    CommandInspection::Conflicting => {
                        transaction.rollback().await?;
                        Ok(ReviewOrchestrationCommandClaim::Conflicting)
                    }
                    CommandInspection::New => {
                        commit_or_ambiguous(transaction).await?;
                        let guard = ReviewOrchestrationCommandGuard {
                            pool: self.pool.clone(),
                            command,
                            pending: !fresh,
                        };
                        Ok(ReviewOrchestrationCommandClaim::New(guard))
                    }
                    CommandInspection::Pending => Err(corruption(
                        "orchestration recovery inspection returned a pending intent",
                    )),
                }
            }
            CommandInspection::New => Err(corruption(
                "orchestration command claim remained new after registry insertion",
            )),
        }
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
            insert_effect_marker(
                self,
                &mut transaction,
                attempt.id(),
                ReviewOrchestrationCommandKind::Start,
                None,
            )
            .await?;
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
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self
            .load_import_on_connection(&mut transaction, attempt)
            .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    async fn record_import(
        &mut self,
        attempt: ReviewOrchestrationAttemptId,
        outcome: ReviewImportOutcome,
    ) -> Result<ReviewDurableSealOutcome, Self::Error> {
        self.verify_import_evidence(&outcome).await?;
        let (kind, pass, link, template, context) = encode_import(&outcome);
        let mut transaction = self.pool.begin().await?;
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
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if inserted {
            insert_effect_marker(
                self,
                &mut transaction,
                attempt,
                ReviewOrchestrationCommandKind::Import,
                None,
            )
            .await?;
            commit_or_ambiguous(transaction).await?;
            return Ok(ReviewDurableSealOutcome::Recorded);
        }
        transaction.rollback().await?;
        Ok(match self.load_import(attempt).await? {
            Some(existing) if existing == outcome => ReviewDurableSealOutcome::EqualReplay,
            Some(_) => ReviewDurableSealOutcome::Conflict,
            None => return Err(corruption("import conflict disappeared")),
        })
    }

    async fn load_concern_claims(
        &self,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Vec<ReviewConcernClaim>, Self::Error> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self
            .load_concern_claims_on_connection(&mut transaction, attempt)
            .await?;
        transaction.commit().await?;
        Ok(loaded)
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
        let effect_sequence: i64 = sqlx::query_scalar(
            "INSERT INTO review_orchestration_concern_claim
                (attempt_id, concern_key, claim_ordinal, template_digest, outcome_kind, pass_id)
             VALUES ($1,$2,$3,$4,$5,$6)
             RETURNING effect_sequence",
        )
        .bind(attempt.as_uuid())
        .bind(claim.concern().as_str())
        .bind(next)
        .bind(claim.template_digest().bytes().as_slice())
        .bind(kind)
        .bind(pass.map(|value| value.into_uuid()))
        .fetch_one(&mut *transaction)
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
        insert_effect_marker(
            self,
            &mut transaction,
            attempt,
            ReviewOrchestrationCommandKind::Concern,
            Some(effect_sequence),
        )
        .await?;
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
    async fn load_import_on_connection(
        &self,
        connection: &mut PgConnection,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Option<ReviewImportOutcome>, ReviewOrchestrationStoreError> {
        let row = sqlx::query(
            "SELECT outcome_kind, pass_id, external_link_id, template_digest, context_digest
               FROM review_orchestration_import WHERE attempt_id = $1",
        )
        .bind(attempt.as_uuid())
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let kind: String = row.try_get("outcome_kind")?;
        let template = decode_digest(row.try_get("template_digest")?)?;
        let pass_id: Option<Uuid> = row.try_get("pass_id")?;
        let outcome = match kind.as_str() {
            "succeeded" => {
                let pass_id = pass_id.ok_or_else(|| corruption("succeeded import lacks pass"))?;
                let (pass, run) = self
                    .load_evidence_on_connection(&mut *connection, ReviewPassId::from_uuid(pass_id))
                    .await?;
                let link = match row.try_get::<Option<Uuid>, _>("external_link_id")? {
                    Some(id) => Some(Box::new(
                        ReviewWorkflowStore::load_external_link_on_connection(
                            &mut *connection,
                            ReviewExternalLinkId::from_uuid(id),
                        )
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
                    Some(id) => Some(
                        self.load_evidence_on_connection(
                            &mut *connection,
                            ReviewPassId::from_uuid(id),
                        )
                        .await?,
                    ),
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

    async fn load_concern_claims_on_connection(
        &self,
        connection: &mut PgConnection,
        attempt: ReviewOrchestrationAttemptId,
    ) -> Result<Vec<ReviewConcernClaim>, ReviewOrchestrationStoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (concern_key)
                    concern_key, claim_ordinal, template_digest, outcome_kind, pass_id
               FROM review_orchestration_concern_claim
              WHERE attempt_id = $1
              ORDER BY concern_key, claim_ordinal DESC",
        )
        .bind(attempt.as_uuid())
        .fetch_all(&mut *connection)
        .await?;
        let mut claims = Vec::with_capacity(rows.len());
        for row in rows {
            claims.push(
                self.decode_concern_claim_on_connection(&mut *connection, attempt, &row)
                    .await?,
            );
        }
        let immutable = self
            .load_attempt_on_connection(&mut *connection, attempt)
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

    async fn load_evidence(
        &self,
        pass_id: ReviewPassId,
    ) -> Result<(ReviewPassEvidence, ReviewRunEvidence), ReviewOrchestrationStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self
            .load_evidence_on_connection(&mut transaction, pass_id)
            .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    async fn load_evidence_on_connection(
        &self,
        connection: &mut PgConnection,
        pass_id: ReviewPassId,
    ) -> Result<(ReviewPassEvidence, ReviewRunEvidence), ReviewOrchestrationStoreError> {
        let pass = load_pass_on_connection(&mut *connection, pass_id)
            .await?
            .ok_or_else(|| corruption("referenced review pass is missing"))?
            .pass;
        let run = self
            .workflow
            .load_run_with_pass_on_connection(&mut *connection, pass.reference().run().run())
            .await?
            .map(|(run, _)| run)
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
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self
            .decode_concern_claim_on_connection(&mut transaction, attempt, row)
            .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    async fn decode_concern_claim_on_connection(
        &self,
        connection: &mut PgConnection,
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
                    .load_evidence_on_connection(
                        &mut *connection,
                        pass.ok_or_else(|| corruption("successful claim lacks pass"))?,
                    )
                    .await?;
                let findings = self
                    .load_claim_findings_on_connection(
                        &mut *connection,
                        attempt,
                        concern.as_str(),
                        row.try_get("claim_ordinal")?,
                    )
                    .await?;
                ReviewConcernOutcome::Succeeded(Box::new(ReviewConcernSuccess::new(
                    producer, run, template, findings,
                )))
            }
            "failed" => ReviewConcernOutcome::Failed {
                pass: self.pass_ref_on_connection(&mut *connection, pass).await?,
            },
            "blocked" => ReviewConcernOutcome::Blocked {
                pass: self.pass_ref_on_connection(&mut *connection, pass).await?,
            },
            "cancelled" => ReviewConcernOutcome::Cancelled {
                pass: match pass {
                    Some(pass) => Some(
                        self.pass_ref_on_connection(&mut *connection, Some(pass))
                            .await?,
                    ),
                    None => None,
                },
            },
            "superseded" => ReviewConcernOutcome::Superseded {
                pass: self.pass_ref_on_connection(&mut *connection, pass).await?,
            },
            _ => return Err(corruption("concern outcome kind is not closed")),
        };
        Ok(ReviewConcernClaim::new(concern, template, outcome))
    }

    async fn pass_ref(
        &self,
        pass: Option<ReviewPassId>,
    ) -> Result<ReviewPassRef, ReviewOrchestrationStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = self.pass_ref_on_connection(&mut transaction, pass).await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    async fn pass_ref_on_connection(
        &self,
        connection: &mut PgConnection,
        pass: Option<ReviewPassId>,
    ) -> Result<ReviewPassRef, ReviewOrchestrationStoreError> {
        Ok(load_pass_on_connection(
            connection,
            pass.ok_or_else(|| corruption("claim lacks pass"))?,
        )
        .await?
        .ok_or_else(|| corruption("claim pass is missing"))?
        .pass
        .reference())
    }

    async fn load_claim_findings_on_connection(
        &self,
        connection: &mut PgConnection,
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
        .fetch_all(&mut *connection)
        .await?;
        let identities = rows
            .iter()
            .map(|row| Ok(ReviewFindingId::from_uuid(row.try_get("finding_id")?)))
            .collect::<Result<Vec<_>, ReviewOrchestrationStoreError>>()?;
        let loaded = self
            .workflow
            .load_findings_on_connection(&mut *connection, &identities)
            .await?;
        identities
            .iter()
            .map(|identity| {
                loaded
                    .get(identity)
                    .map(|finding| ReviewFinding::new(finding.proposal().clone()))
                    .ok_or_else(|| corruption("concern finding is missing"))
            })
            .collect()
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
            let identities = success
                .findings()
                .iter()
                .map(|finding| finding.proposal().reference().finding())
                .collect::<Vec<_>>();
            let canonical = self.workflow.load_findings(&identities).await?;
            for finding in success.findings() {
                let loaded = canonical
                    .get(&finding.proposal().reference().finding())
                    .ok_or_else(|| corruption("concern success finding is missing"))?;
                if loaded != finding {
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

async fn insert_effect_marker(
    store: &PostgresReviewOrchestrationStore,
    transaction: &mut Transaction<'_, Postgres>,
    attempt: ReviewOrchestrationAttemptId,
    kind: ReviewOrchestrationCommandKind,
    concern_effect_sequence: Option<i64>,
) -> Result<(), ReviewOrchestrationStoreError> {
    let Some(command) = store.effect_command else {
        return Ok(());
    };
    if command.attempt != attempt || command.kind != kind {
        return Err(corruption(
            "orchestration effect command does not match the primary effect",
        ));
    }
    if (kind == ReviewOrchestrationCommandKind::Concern) != concern_effect_sequence.is_some() {
        return Err(corruption(
            "orchestration concern effect marker lacks its exact claim sequence",
        ));
    }
    sqlx::query(
        "INSERT INTO review_orchestration_command_effect
            (command_id, attempt_id, operation_kind, concern_effect_sequence)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(durable_command_id_to_uuid(command.command_id))
    .bind(attempt.as_uuid())
    .bind(encode_command_kind(kind))
    .bind(concern_effect_sequence)
    .execute(&mut **transaction)
    .await?;
    Ok(())
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
    insert_effect_marker(
        store,
        &mut transaction,
        attempt,
        ReviewOrchestrationCommandKind::JudgmentPlan,
        None,
    )
    .await?;
    commit_or_ambiguous(transaction).await?;
    Ok(ReviewDurableSealOutcome::Recorded)
}

async fn load_judgment_plan_impl(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<ReviewJudgmentPlan>, ReviewOrchestrationStoreError> {
    let mut transaction = begin_repeatable_read(&store.pool).await?;
    let loaded = load_judgment_plan_on_connection(store, &mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(loaded)
}

async fn load_judgment_plan_on_connection(
    store: &PostgresReviewOrchestrationStore,
    connection: &mut PgConnection,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<ReviewJudgmentPlan>, ReviewOrchestrationStoreError> {
    let row = sqlx::query(
        "SELECT analysis_pass_id, template_digest
           FROM review_orchestration_judgment_plan WHERE attempt_id = $1",
    )
    .bind(attempt.as_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let (pass, run) = store
        .load_evidence_on_connection(
            &mut *connection,
            ReviewPassId::from_uuid(row.try_get("analysis_pass_id")?),
        )
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
    .fetch_all(&mut *connection)
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
    let mut transaction = begin_repeatable_read(&store.pool).await?;
    let loaded = load_applied_effects_on_connection(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(loaded)
}

async fn load_applied_effects_on_connection(
    connection: &mut PgConnection,
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
    .fetch_all(&mut *connection)
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
    let mut transaction = store.pool.begin().await?;
    sqlx::query(
        "INSERT INTO review_orchestration_judgment_effect
            (attempt_id, effect_ordinal, finding_id) VALUES ($1,$2,$3)",
    )
    .bind(effect.attempt().as_uuid())
    .bind(ordinal)
    .bind(effect.finding().finding().into_uuid())
    .execute(&mut *transaction)
    .await?;
    insert_effect_marker(
        store,
        &mut transaction,
        effect.attempt(),
        ReviewOrchestrationCommandKind::JudgmentEffect,
        None,
    )
    .await?;
    commit_or_ambiguous(transaction).await?;
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
    let mut transaction = begin_repeatable_read(&store.pool).await?;
    let loaded = load_finding_inventory_on_connection(&mut transaction, attempt, stage).await?;
    transaction.commit().await?;
    Ok(loaded)
}

async fn load_finding_inventory_on_connection(
    connection: &mut PgConnection,
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
        .fetch_optional(&mut *connection)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let rows = sqlx::query(member_sql)
        .bind(attempt.as_uuid())
        .fetch_all(&mut *connection)
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
    insert_effect_marker(
        store,
        &mut transaction,
        attempt,
        ReviewOrchestrationCommandKind::Repair,
        None,
    )
    .await?;
    commit_or_ambiguous(transaction).await?;
    Ok(ReviewDurableSealOutcome::Recorded)
}

async fn load_repairs(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<Vec<ReviewRepairMemberOutcome>>, ReviewOrchestrationStoreError> {
    let mut transaction = begin_repeatable_read(&store.pool).await?;
    let loaded = load_repairs_on_connection(store, &mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(loaded)
}

async fn load_repairs_on_connection(
    store: &PostgresReviewOrchestrationStore,
    connection: &mut PgConnection,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<Vec<ReviewRepairMemberOutcome>>, ReviewOrchestrationStoreError> {
    if sqlx::query("SELECT 1 FROM review_orchestration_repair_outcome_seal WHERE attempt_id = $1")
        .bind(attempt.as_uuid())
        .fetch_optional(&mut *connection)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let inventory = load_finding_inventory_on_connection(&mut *connection, attempt, "repair")
        .await?
        .ok_or_else(|| corruption("repair outcome seal lacks inventory"))?;
    let rows = sqlx::query(
        "SELECT finding_id, outcome_kind, event_ordinal, template_digest
           FROM review_orchestration_repair_outcome
          WHERE attempt_id = $1 ORDER BY member_ordinal",
    )
    .bind(attempt.as_uuid())
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != inventory.len() {
        return Err(corruption("repair outcomes do not cover their inventory"));
    }
    // Every fixed member reads its own event out of a complete finding, so the
    // whole set is reconstructed once here rather than one target graph per
    // member.
    let mut fixed = Vec::new();
    for row in &rows {
        if row.try_get::<String, _>("outcome_kind")? == "fixed" {
            fixed.push(ReviewFindingId::from_uuid(row.try_get("finding_id")?));
        }
    }
    let repaired = store
        .workflow
        .load_findings_on_connection(&mut *connection, &fixed)
        .await?;
    let mut outcomes = Vec::with_capacity(rows.len());
    for (row, finding_ref) in rows.iter().zip(inventory) {
        if ReviewFindingId::from_uuid(row.try_get("finding_id")?) != finding_ref.finding() {
            return Err(corruption("repair outcome finding order differs"));
        }
        let kind: String = row.try_get("outcome_kind")?;
        outcomes.push(match kind.as_str() {
            "fixed" => {
                let finding = repaired
                    .get(&finding_ref.finding())
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
    insert_effect_marker(
        store,
        &mut transaction,
        attempt,
        ReviewOrchestrationCommandKind::Publication,
        None,
    )
    .await?;
    commit_or_ambiguous(transaction).await?;
    Ok(ReviewDurableSealOutcome::Recorded)
}

async fn load_publications(
    store: &PostgresReviewOrchestrationStore,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<Vec<ReviewPublicationMemberOutcome>>, ReviewOrchestrationStoreError> {
    let mut transaction = begin_repeatable_read(&store.pool).await?;
    let loaded = load_publications_on_connection(store, &mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(loaded)
}

async fn load_publications_on_connection(
    store: &PostgresReviewOrchestrationStore,
    connection: &mut PgConnection,
    attempt: ReviewOrchestrationAttemptId,
) -> Result<Option<Vec<ReviewPublicationMemberOutcome>>, ReviewOrchestrationStoreError> {
    if sqlx::query(
        "SELECT 1 FROM review_orchestration_publication_outcome_seal WHERE attempt_id = $1",
    )
    .bind(attempt.as_uuid())
    .fetch_optional(&mut *connection)
    .await?
    .is_none()
    {
        return Ok(None);
    }
    let inventory = load_finding_inventory_on_connection(&mut *connection, attempt, "publication")
        .await?
        .ok_or_else(|| corruption("publication outcome seal lacks inventory"))?;
    let rows = sqlx::query(
        "SELECT finding_id, outcome_kind, external_link_id, template_digest
           FROM review_orchestration_publication_outcome
          WHERE attempt_id = $1 ORDER BY member_ordinal",
    )
    .bind(attempt.as_uuid())
    .fetch_all(&mut *connection)
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
                let link = ReviewWorkflowStore::load_external_link_on_connection(
                    &mut *connection,
                    ReviewExternalLinkId::from_uuid(row.try_get("external_link_id")?),
                )
                .await?
                .ok_or_else(|| corruption("published external link is missing"))?;
                let reference = ReviewFindingExternalLinkRef::try_new(finding, &link)
                    .map_err(|_| corruption("published link is not canonical"))?;
                let (_, run) = store
                    .load_evidence_on_connection(
                        &mut *connection,
                        reference.attachment_pass().reference().pass(),
                    )
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

async fn inspect_command_recovery(
    transaction: &mut Transaction<'_, Postgres>,
    command: ReviewOrchestrationCommand,
) -> Result<CommandInspection, ReviewOrchestrationStoreError> {
    let row = sqlx::query(
        "SELECT semantic_digest, attempt_id, operation_kind, result_stage,
                import_recorded, concern_claim_count, fanout_sealed,
                judgment_plan_sealed, judgment_effect_count,
                repair_inventory_count, repair_outcomes_recorded,
                publication_inventory_count, publication_outcomes_recorded
           FROM review_orchestration_command_recovery WHERE command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command.command_id))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(CommandInspection::New);
    };
    let digest: Vec<u8> = row.try_get("semantic_digest")?;
    if digest.as_slice() != command.semantic_digest
        || ReviewOrchestrationAttemptId::from_uuid(row.try_get("attempt_id")?) != command.attempt
        || row.try_get::<String, _>("operation_kind")? != encode_command_kind(command.kind)
    {
        return Ok(CommandInspection::Conflicting);
    }
    Ok(CommandInspection::Recorded(decode_command_result(&row)?))
}

async fn validate_recorded_recovery(
    transaction: &mut Transaction<'_, Postgres>,
    command: ReviewOrchestrationCommand,
    result: &ReviewOrchestrationCommandResult,
) -> Result<(), ReviewOrchestrationStoreError> {
    match inspect_command_recovery(transaction, command).await? {
        CommandInspection::New => Ok(()),
        CommandInspection::Recorded(recovered) if recovered == *result => Ok(()),
        CommandInspection::Recorded(_)
        | CommandInspection::Conflicting
        | CommandInspection::Pending => Err(corruption(
            "orchestration command receipt contradicts its recovery record",
        )),
    }
}

async fn insert_command_recovery(
    transaction: &mut Transaction<'_, Postgres>,
    command: &ReviewOrchestrationCommand,
    result: &ReviewOrchestrationCommandResult,
) -> Result<bool, ReviewOrchestrationStoreError> {
    let inserted = sqlx::query(
        "INSERT INTO review_orchestration_command_recovery
            (command_id, semantic_digest, attempt_id, operation_kind, result_stage,
             import_recorded, concern_claim_count, fanout_sealed,
             judgment_plan_sealed, judgment_effect_count, repair_inventory_count,
             repair_outcomes_recorded, publication_inventory_count,
             publication_outcomes_recorded)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT DO NOTHING",
    )
    .bind(durable_command_id_to_uuid(command.command_id))
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
    .await?
    .rows_affected()
        == 1;
    Ok(inserted)
}

enum CommandInspection {
    New,
    Pending,
    Recorded(ReviewOrchestrationCommandResult),
    Conflicting,
}

async fn inspect_command(
    transaction: &mut Transaction<'_, Postgres>,
    command: ReviewOrchestrationCommand,
) -> Result<CommandInspection, ReviewOrchestrationStoreError> {
    let registry = sqlx::query(
        "SELECT command_kind, storage_version
           FROM durable_command WHERE command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command.command_id))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(registry) = registry else {
        return Ok(CommandInspection::New);
    };
    if registry.try_get::<String, _>("command_kind")? != REVIEW_ORCHESTRATION_KIND {
        return Ok(CommandInspection::Conflicting);
    }
    if registry.try_get::<i16, _>("storage_version")? != STORAGE_VERSION {
        return Err(corruption(
            "orchestration command registry version is unsupported",
        ));
    }
    let receipt = sqlx::query(
        "SELECT semantic_digest, attempt_id, operation_kind, result_stage,
                        import_recorded, concern_claim_count, fanout_sealed,
                        judgment_plan_sealed, judgment_effect_count,
                        repair_inventory_count, repair_outcomes_recorded,
                        publication_inventory_count, publication_outcomes_recorded
                   FROM review_orchestration_command WHERE command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command.command_id))
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(row) = receipt {
        let digest: Vec<u8> = row.try_get("semantic_digest")?;
        if digest.as_slice() != command.semantic_digest
            || ReviewOrchestrationAttemptId::from_uuid(row.try_get("attempt_id")?)
                != command.attempt
            || row.try_get::<String, _>("operation_kind")? != encode_command_kind(command.kind)
        {
            return Ok(CommandInspection::Conflicting);
        }
        return Ok(CommandInspection::Recorded(decode_command_result(&row)?));
    }
    let intent = sqlx::query(
        "SELECT semantic_digest, attempt_id, operation_kind
                   FROM review_orchestration_command_intent WHERE command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command.command_id))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = intent else {
        return Err(corruption(
            "orchestration command registry entry has no intent or receipt",
        ));
    };
    let digest: Vec<u8> = row.try_get("semantic_digest")?;
    if digest.as_slice() != command.semantic_digest
        || ReviewOrchestrationAttemptId::from_uuid(row.try_get("attempt_id")?) != command.attempt
        || row.try_get::<String, _>("operation_kind")? != encode_command_kind(command.kind)
    {
        return Ok(CommandInspection::Conflicting);
    }
    Ok(CommandInspection::Pending)
}

async fn insert_command_intent(
    transaction: &mut Transaction<'_, Postgres>,
    command: &ReviewOrchestrationCommand,
) -> Result<(), ReviewOrchestrationStoreError> {
    sqlx::query(
        "INSERT INTO review_orchestration_command_intent
            (command_id, command_kind, storage_version, semantic_digest,
             attempt_id, operation_kind)
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(durable_command_id_to_uuid(command.command_id))
    .bind(REVIEW_ORCHESTRATION_KIND)
    .bind(STORAGE_VERSION)
    .bind(command.semantic_digest.as_slice())
    .bind(command.attempt.as_uuid())
    .bind(encode_command_kind(command.kind))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn delete_command_intent(
    transaction: &mut Transaction<'_, Postgres>,
    command: &ReviewOrchestrationCommand,
) -> Result<(), ReviewOrchestrationStoreError> {
    let deleted =
        sqlx::query("DELETE FROM review_orchestration_command_intent WHERE command_id = $1")
            .bind(durable_command_id_to_uuid(command.command_id))
            .execute(&mut **transaction)
            .await?
            .rows_affected();
    if deleted != 1 {
        return Err(corruption(
            "orchestration command intent disappeared before receipt replacement",
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    /// No loader in this file may reach the database on a bare pooled
    /// connection.
    ///
    /// The `*_on_connection` helpers exist so `load_snapshot` can run the whole
    /// projection inside one `REPEATABLE READ` transaction. Handed a connection
    /// checked straight out of the pool instead, those same helpers run each of
    /// their statements as a separate `READ COMMITTED` autocommit statement.
    /// That silently drops the per-loader coherence `ReviewWorkflowStore`'s own
    /// transactions provide — a
    /// multi-statement external-link or pass load could then straddle a
    /// concurrent commit and return exactly the torn external-link projection
    /// that `docs/spec/review-workflows.md` rules out.
    ///
    /// Every standalone entry point must therefore open and commit a
    /// transaction, so this holds for the file as a whole rather than for a
    /// list of functions that a new loader could be added beside.
    #[test]
    fn no_loader_reaches_the_database_on_a_bare_pooled_connection() {
        // Assembled rather than written out, so this test does not match itself.
        let bare = concat!("pool", ".acquire()");
        let offenders = include_str!("review_orchestration.rs")
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(bare))
            .map(|(index, _)| index + 1)
            .collect::<Vec<_>>();

        assert!(
            offenders.is_empty(),
            "review_orchestration.rs reaches the database on a bare pooled \
             connection at line(s) {offenders:?}; a standalone loader must open a \
             repeatable-read transaction so its statements share one snapshot"
        );
    }
}

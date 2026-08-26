//! Typed persistence observations and seeds used only by composed integration
//! tests.
//!
//! Composed tests above this crate state the durable state they need in domain
//! vocabulary and leave the table and column names here, so a schema change is
//! contained in — and exercised by — the crate that owns the schema.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde_json::{Value, json};
use signalbox_application::{RepoWatchConvergenceVerdict, RepoWatchReviewDecision};
use signalbox_domain::{
    BranchName, CheckRunName, CommitSha, MergeableState, ModelCallId, PullRequestNumber,
    RepoWatchAuthorLogin, RepositorySlug, SessionId,
};
use sqlx::{FromRow, PgPool, types::Uuid};

use crate::mapping::{
    repo_watch_convergence_verdict_to_str, repo_watch_mergeable_state_to_str,
    repo_watch_review_decision_to_str,
};

/// Durable fleet state observed by the process-runtime soak harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FleetSoakCensus {
    active_turns: i64,
    terminal_turns: i64,
    awaiting_model_call_recovery_turns: i64,
    terminal_model_calls: i64,
    ambiguous_model_calls: i64,
}

impl FleetSoakCensus {
    /// Number of active turns in the isolated test database.
    pub const fn active_turns(self) -> i64 {
        self.active_turns
    }

    /// Number of terminal turns in the isolated test database.
    pub const fn terminal_turns(self) -> i64 {
        self.terminal_turns
    }

    /// Active turns parked for a user model-call recovery decision.
    pub const fn awaiting_model_call_recovery_turns(self) -> i64 {
        self.awaiting_model_call_recovery_turns
    }

    /// Scoped model calls carrying any terminal disposition.
    pub const fn terminal_model_calls(self) -> i64 {
        self.terminal_model_calls
    }

    /// Scoped model calls carrying the ambiguity disposition.
    pub const fn ambiguous_model_calls(self) -> i64 {
        self.ambiguous_model_calls
    }
}

#[derive(FromRow)]
struct FleetLifecycleCensusRow {
    active_turns: i64,
    terminal_turns: i64,
    awaiting_model_call_recovery_turns: i64,
}

#[derive(FromRow)]
struct FleetModelCallCensusRow {
    terminal_model_calls: i64,
    ambiguous_model_calls: i64,
}

/// Persistence-owned durable census for an isolated fleet-soak database.
#[derive(Clone, Debug)]
pub struct FleetSoakCensusRepository {
    pool: PgPool,
}

impl FleetSoakCensusRepository {
    /// Uses the supplied isolated integration-test pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Lists ordinary model-call identities in deterministic order.
    pub async fn model_call_ids(&self) -> Result<Box<[ModelCallId]>, sqlx::Error> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            "SELECT model_call_id FROM model_call ORDER BY model_call_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(ModelCallId::from_uuid)
            .collect::<Vec<_>>()
            .into_boxed_slice())
    }

    /// Finds the ordinary model call belonging to exactly `session`.
    pub async fn model_call_id_for_session(
        &self,
        session: SessionId,
    ) -> Result<Option<ModelCallId>, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT model_call_id FROM model_call WHERE session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .map(ModelCallId::from_uuid))
    }

    /// Whether exactly `model_call` and its owning turn carry the ambiguity park.
    pub async fn has_ambiguous_recovery_park(
        &self,
        model_call: ModelCallId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM model_call AS call
                  JOIN turn_lifecycle AS turn
                    ON turn.turn_id = call.turn_id
                   AND turn.session_id = call.session_id
                 WHERE call.model_call_id = $1
                   AND call.terminal_disposition_kind = 'ambiguous'
                   AND turn.state_kind = 'active'
                   AND turn.active_phase_kind = 'awaiting_model_call_recovery'
            )",
        )
        .bind(model_call.into_uuid())
        .fetch_one(&self.pool)
        .await
    }

    /// Reads lifecycle state and dispositions for exactly `model_calls`.
    pub async fn census_for(
        &self,
        model_calls: &[ModelCallId],
    ) -> Result<FleetSoakCensus, sqlx::Error> {
        let model_call_ids: Vec<Uuid> = model_calls.iter().map(|call| call.into_uuid()).collect();
        let lifecycle: FleetLifecycleCensusRow = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE state_kind = 'active') AS active_turns,
                    count(*) FILTER (WHERE state_kind = 'terminal') AS terminal_turns,
                    count(*) FILTER (
                        WHERE state_kind = 'active'
                          AND active_phase_kind = 'awaiting_model_call_recovery'
                    ) AS awaiting_model_call_recovery_turns
               FROM turn_lifecycle
              WHERE turn_id IN (
                    SELECT turn_id
                      FROM model_call
                     WHERE model_call_id = ANY($1)
              )",
        )
        .bind(&model_call_ids)
        .fetch_one(&self.pool)
        .await?;
        let calls: FleetModelCallCensusRow = sqlx::query_as(
            "SELECT count(*) FILTER (
                        WHERE terminal_disposition_kind IS NOT NULL
                    ) AS terminal_model_calls,
                    count(*) FILTER (
                        WHERE terminal_disposition_kind = 'ambiguous'
                    ) AS ambiguous_model_calls
               FROM model_call
              WHERE model_call_id = ANY($1)",
        )
        .bind(&model_call_ids)
        .fetch_one(&self.pool)
        .await?;
        Ok(FleetSoakCensus {
            active_turns: lifecycle.active_turns,
            terminal_turns: lifecycle.terminal_turns,
            awaiting_model_call_recovery_turns: lifecycle.awaiting_model_call_recovery_turns,
            terminal_model_calls: calls.terminal_model_calls,
            ambiguous_model_calls: calls.ambiguous_model_calls,
        })
    }
}

/// Field-labeled construction input for one seeded convergence assessment.
///
/// Production code reaches an assessment only through
/// [`crate::repo_watch::PostgresRepoWatchStore::record_convergence_assessments`],
/// which derives it from a whole observed repository state. A composed test
/// that only needs the durable rows — the operator-status read, which never
/// creates one — states the rows it wants instead.
#[derive(Clone, Debug)]
pub struct OperatorStatusConvergenceFixture {
    pub number: PullRequestNumber,
    pub head_sha: CommitSha,
    pub base_branch: BranchName,
    pub base_revision: CommitSha,
    pub mergeable_state: MergeableState,
    pub settled: bool,
    pub review_decision: RepoWatchReviewDecision,
    pub unresolved_threads: Vec<String>,
    pub gating_check_count: u64,
    pub non_green_gating_checks: Vec<CheckRunName>,
    pub verdict: RepoWatchConvergenceVerdict,
    pub stale_review_clearance: Option<OperatorStatusStaleReviewClearanceFixture>,
}

/// Field-labeled construction input for one seeded pending stale-review
/// clearance, planned against the assessment seeded alongside it.
#[derive(Clone, Debug)]
pub struct OperatorStatusStaleReviewClearanceFixture {
    pub review_node_id: String,
    pub reviewer: RepoWatchAuthorLogin,
    pub reviewed_head_sha: CommitSha,
    pub dismissal_message: String,
}

/// Persistence-owned seeding for the repository-watch operator-status views.
#[derive(Clone, Debug)]
pub struct OperatorStatusFixtureRepository {
    pool: PgPool,
}

impl OperatorStatusFixtureRepository {
    /// Uses the supplied isolated integration-test pool.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Seeds one repository's cursor generation and, against it, every stated
    /// latest convergence assessment, its current-assessment identity, and any
    /// clearance planned on it.
    ///
    /// The current-convergence projection reads the newest cursor payload and
    /// admits an assessment only where that payload still carries the pull
    /// request at the assessed head and its base branch at the assessed
    /// revision. The payload is therefore derived here from the stated
    /// assessments, so a caller states pull requests rather than reproducing
    /// the cursor's storage shape.
    ///
    /// An observed repository state names each branch once, and the projection
    /// joins its assessments against every payload entry carrying their base
    /// branch: one entry per stated pull request would return an assessment
    /// once per pull request sharing its base. The branch heads are therefore
    /// one entry per base branch, and stating one branch at two revisions —
    /// which no observation produces — is rejected rather than composed.
    pub async fn seed_pull_request_convergences(
        &self,
        repository: &RepositorySlug,
        convergences: &[OperatorStatusConvergenceFixture],
    ) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        let repository = repository.as_str();
        let pull_requests: Vec<Value> = convergences
            .iter()
            .map(|convergence| {
                json!({
                    "number": convergence.number.get(),
                    "head_sha": convergence.head_sha.as_str(),
                    "head_repository": repository,
                    "base_branch": convergence.base_branch.as_str(),
                    "head_branch": convergence.base_branch.as_str(),
                    "title": "Seeded pull request",
                    "body": "",
                    "labels": [],
                    "draft": false,
                    "author": null,
                    "lifecycle": "open",
                    "mergeable_state": repo_watch_mergeable_state_to_str(
                        convergence.mergeable_state,
                    ),
                    "completed_check_suites": [],
                    "completed_check_runs": [],
                    "reviews": [],
                    "threads": [],
                    "reactions": []
                })
            })
            .collect();
        let mut branch_heads: Vec<Value> = Vec::new();
        let mut stated_branch_heads: BTreeMap<&str, &str> = BTreeMap::new();
        for convergence in convergences {
            let branch = convergence.base_branch.as_str();
            let head = convergence.base_revision.as_str();
            match stated_branch_heads.insert(branch, head) {
                None => branch_heads.push(json!({ "branch": branch, "head": head })),
                Some(stated) if stated != head => {
                    return Err(sqlx::Error::Protocol(format!(
                        "operator-status fixture states base branch {branch} at two revisions"
                    )));
                }
                Some(_) => {}
            }
        }
        let generation: i64 = sqlx::query_scalar(
            "INSERT INTO repo_watch_cursor (
                repository, generation, storage_version, cursor_payload,
                recording_transaction_id
             )
             SELECT $1,
                    coalesce(
                        (SELECT max(existing.generation)
                           FROM repo_watch_cursor AS existing
                          WHERE existing.repository = $1),
                        0
                    ) + 1,
                    3, $2, pg_current_xact_id()
             RETURNING generation",
        )
        .bind(repository)
        .bind(sqlx::types::Json(json!({
            "storage_version": 3,
            "signal_reviewers": [],
            "event_identity_frontier": [],
            "state": {
                "pull_requests": pull_requests,
                "workflow_runs": [],
                "branch_heads": branch_heads,
            }
        })))
        .fetch_one(&mut *transaction)
        .await?;
        for convergence in convergences {
            let number = Decimal::from(convergence.number.get());
            let check_names: Vec<String> = convergence
                .non_green_gating_checks
                .iter()
                .map(|name| name.as_str().to_owned())
                .collect();
            let gating_check_count =
                i64::try_from(convergence.gating_check_count).map_err(|_| {
                    sqlx::Error::Protocol(String::from(
                        "operator-status fixture gating check count exceeds the durable width",
                    ))
                })?;
            let assessment_id: Uuid = sqlx::query_scalar(
                "INSERT INTO repo_watch_pull_request_convergence_assessment (
                    assessment_id, repository, cursor_generation, pull_request_number,
                    head_sha, base_branch, base_revision, mergeable_state, settled,
                    review_decision, unresolved_threads, gating_check_count,
                    non_green_gating_checks, verdict_kind, recorded_at
                 ) VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                           $11, $12, $13, transaction_timestamp())
                 RETURNING assessment_id",
            )
            .bind(repository)
            .bind(generation)
            .bind(number)
            .bind(convergence.head_sha.as_str())
            .bind(convergence.base_branch.as_str())
            .bind(convergence.base_revision.as_str())
            .bind(repo_watch_mergeable_state_to_str(
                convergence.mergeable_state,
            ))
            .bind(convergence.settled)
            .bind(repo_watch_review_decision_to_str(
                convergence.review_decision,
            ))
            .bind(&convergence.unresolved_threads)
            .bind(gating_check_count)
            .bind(&check_names)
            .bind(repo_watch_convergence_verdict_to_str(convergence.verdict))
            .fetch_one(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO repo_watch_pull_request_convergence_identity (
                    identity_id, repository, cursor_generation, pull_request_number,
                    assessment_id
                 ) VALUES (gen_random_uuid(), $1, $2, $3, $4)",
            )
            .bind(repository)
            .bind(generation)
            .bind(number)
            .bind(assessment_id)
            .execute(&mut *transaction)
            .await?;
            if let Some(clearance) = convergence.stale_review_clearance.as_ref() {
                sqlx::query(
                    "INSERT INTO repo_watch_stale_review_clearance (
                        clearance_id, assessment_id, repository, pull_request_number,
                        current_head_sha, base_revision, review_node_id, reviewer,
                        reviewed_head_sha, dismissal_message, planned_at
                     ) VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7, $8, $9,
                               transaction_timestamp())",
                )
                .bind(assessment_id)
                .bind(repository)
                .bind(number)
                .bind(convergence.head_sha.as_str())
                .bind(convergence.base_revision.as_str())
                .bind(&clearance.review_node_id)
                .bind(clearance.reviewer.as_str())
                .bind(clearance.reviewed_head_sha.as_str())
                .bind(&clearance.dismissal_message)
                .execute(&mut *transaction)
                .await?;
            }
        }
        transaction.commit().await
    }
}

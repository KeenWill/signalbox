//! Periodic convergence reconciliation for explicitly selected watched pull requests.

use std::{error::Error, fmt, sync::Arc, time::Duration};

use futures_util::{StreamExt, stream};
use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, USER_AGENT},
    redirect::Policy,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use signalbox_application::{
    CommissionDispatchRequest, CommissionedDispatchFence, EligibilityNudge,
    InProcessEligibilityNudge, PullRequestCheck, PullRequestCheckState, PullRequestConvergence,
    PullRequestConvergenceBlocker, PullRequestConvergenceFacts,
    UuidV7CommissionedDispatchIdGenerator, evaluate_pull_request_convergence,
};
use signalbox_domain::{
    BranchName, CommitSha, DurableCommandId, GoalStatement, MergeableState, PullRequestNumber,
    RepositorySlug, UserContent,
};
use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_persistence::{
    commissioned_dispatch::{CommissionDispatchOutcome, PostgresCommissionedDispatchStore},
    convergence_sweep::{
        ConvergenceSweepDecision, ConvergenceSweepFailureKind, ConvergenceSweepObservation,
        ConvergenceSweepRetryPolicy, PostgresConvergenceSweepStore,
    },
};
use sqlx::PgPool;
use tokio::{
    select,
    sync::{Semaphore, watch},
    time::{Instant, MissedTickBehavior, interval, sleep, sleep_until},
};

use crate::{
    FileCredentialAccess, HubModelConfiguration, RepositoryWatchConfiguration,
    SessionTemplateConfiguration,
};

const GRAPHQL_URL: &str = "https://api.github.com/graphql";
const USER_AGENT_VALUE: &str = "signalbox-convergence-sweep";
// numeric-bound: ceiling - bounds one provider exchange and therefore one target's census latency
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
// numeric-bound: ceiling - bounds retained provider output before JSON decoding
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
// numeric-bound: ceiling - prevents one pathological PR from monopolizing a complete sweep
const MAX_CONNECTION_PAGES: usize = 100;
// numeric-bound: ceiling - bounds transport attempts for one idempotent GraphQL census request
const MAX_REQUEST_ATTEMPTS: usize = 3;
// numeric-bound: tunable - separates bounded transport retry attempts
const REQUEST_RETRY_DELAY: Duration = Duration::from_millis(250);
// numeric-bound: ceiling - bounds retained secret material and authorization header construction
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
// numeric-bound: tunable - separates a transient failure from its first automatic retry
const RETRY_BACKOFF_BASE: Duration = Duration::from_secs(60);
// numeric-bound: ceiling - prevents exhausted transient work from backing off beyond useful operator visibility
const RETRY_BACKOFF_CAP: Duration = Duration::from_secs(15 * 60);

const DETAILS_QUERY: &str = r#"
query PullRequestConvergence($namespace: String!, $name: String!, $number: Int!) {
  repository(owner: $namespace, name: $name) {
    pullRequest(number: $number) {
      state isDraft baseRefName baseRefOid headRefName headRefOid mergeable
      headRepository { name_with_owner: nameWithOwner }
      reviewThreads(first: 100) {
        nodes { isResolved }
        pageInfo { hasNextPage endCursor }
      }
      commits(last: 1) { nodes { commit {
        oid
        statusCheckRollup { contexts(first: 100) {
          nodes {
            __typename
            ... on CheckRun { name status conclusion }
            ... on StatusContext { context state }
          }
          pageInfo { hasNextPage endCursor }
        } }
      } } }
    }
  }
}
"#;

const THREADS_QUERY: &str = r#"
query PullRequestConvergenceThreads(
  $namespace: String!, $name: String!, $number: Int!, $after: String
) {
  repository(owner: $namespace, name: $name) {
    pullRequest(number: $number) {
      state baseRefName baseRefOid headRefName headRefOid
      reviewThreads(first: 100, after: $after) {
        nodes { isResolved }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

const CHECKS_QUERY: &str = r#"
query PullRequestConvergenceChecks(
  $namespace: String!, $name: String!, $number: Int!, $after: String
) {
  repository(owner: $namespace, name: $name) {
    pullRequest(number: $number) {
      state baseRefName baseRefOid headRefName headRefOid
      commits(last: 1) { nodes { commit {
        oid
        statusCheckRollup { contexts(first: 100, after: $after) {
          nodes {
            __typename
            ... on CheckRun { name status conclusion }
            ... on StatusContext { context state }
          }
          pageInfo { hasNextPage endCursor }
        } }
      } } }
    }
  }
}
"#;

/// Construction failure for the fixed HTTPS transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConvergenceSweepRuntimeConstructionError;

impl fmt::Display for ConvergenceSweepRuntimeConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("convergence sweep HTTP transport could not be constructed")
    }
}

impl Error for ConvergenceSweepRuntimeConstructionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CensusError {
    Credential,
    Request,
    Response,
    Decode,
    Shape,
    Pagination,
    State,
}

#[derive(Clone)]
struct SweepTarget {
    repository: RepositorySlug,
    pull_request: PullRequestNumber,
    credentials: FileCredentialAccess,
    credential_reference: CredentialReference,
}

/// Independent supervisor for the opt-in convergence target set.
pub struct ConvergenceSweepRuntime {
    client: Client,
    targets: Box<[SweepTarget]>,
    interval: Duration,
    cool_off: Duration,
    template: signalbox_domain::SessionTemplateName,
    templates: SessionTemplateConfiguration,
    models: HubModelConfiguration,
    commissioned: PostgresCommissionedDispatchStore,
    state: PostgresConvergenceSweepStore,
    eligibility_nudge: InProcessEligibilityNudge,
}

impl ConvergenceSweepRuntime {
    /// Constructs no runtime when the operator selected no convergence targets.
    pub fn try_new(
        pool: PgPool,
        configuration: &RepositoryWatchConfiguration,
        templates: SessionTemplateConfiguration,
        models: HubModelConfiguration,
        eligibility_nudge: InProcessEligibilityNudge,
    ) -> Result<Option<Self>, ConvergenceSweepRuntimeConstructionError> {
        let Some(policy) = configuration.convergence_sweep() else {
            return Ok(None);
        };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .tls_backend_rustls()
            .tls_version_min(reqwest::tls::Version::TLS_1_2)
            .tls_danger_accept_invalid_certs(false)
            .tls_danger_accept_invalid_hostnames(false)
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ConvergenceSweepRuntimeConstructionError)?;
        let targets = configuration
            .repositories()
            .iter()
            .flat_map(|repository| {
                repository
                    .convergence_pull_requests()
                    .iter()
                    .map(|pull_request| SweepTarget {
                        repository: repository.repository().clone(),
                        pull_request: *pull_request,
                        credentials: FileCredentialAccess::new_bounded(
                            repository.credential_file().to_path_buf(),
                            repository.credential_reference(),
                            MAX_CREDENTIAL_BYTES,
                        ),
                        credential_reference: repository.credential_reference(),
                    })
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Some(Self {
            client,
            targets,
            interval: policy.interval(),
            cool_off: policy.cool_off(),
            template: policy.template().clone(),
            templates,
            models: models.clone(),
            commissioned: PostgresCommissionedDispatchStore::new(
                pool.clone(),
                models.session_credential_pin(),
            ),
            state: PostgresConvergenceSweepStore::new(pool),
            eligibility_nudge,
        }))
    }

    /// Runs complete censuses until shutdown; one target failure never halts siblings.
    pub async fn run(self, shutdown: watch::Receiver<bool>) {
        let runtime = &self;
        // Configuration bounds this target set to 256 entries. Giving each enrolled
        // target one permit preserves its absolute polling deadline while retaining
        // an explicit, configuration-bounded admission gate.
        let active_targets = Arc::new(Semaphore::new(self.targets.len()));
        stream::iter(&self.targets)
            .for_each_concurrent(None, |target| {
                let mut shutdown = shutdown.clone();
                let active_targets = Arc::clone(&active_targets);
                async move {
                    if *shutdown.borrow() {
                        return;
                    }
                    let mut ticks = interval(runtime.interval);
                    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    let mut reenrolled = false;
                    loop {
                        let scheduled = select! {
                            scheduled = ticks.tick() => scheduled,
                            changed = shutdown.changed() => {
                                if changed.is_err() || *shutdown.borrow() { return; }
                                continue;
                            }
                        };
                        let permit = select! {
                            permit = active_targets.acquire() => {
                                match permit {
                                    Ok(permit) => permit,
                                    Err(_) => return,
                                }
                            }
                            changed = shutdown.changed() => {
                                if changed.is_err() || *shutdown.borrow() { return; }
                                continue;
                            }
                        };
                        if !reenrolled {
                            match runtime
                                .state
                                .reenroll_target(&target.repository, target.pull_request)
                                .await
                            {
                                Ok(()) => reenrolled = true,
                                Err(error) => {
                                    tracing::error!(
                                        repository = %target.repository.as_str(),
                                        pull_request = target.pull_request.get(),
                                        cause = %error,
                                        "convergence sweep target re-enrollment failed; retrying on the next tick"
                                    );
                                    drop(permit);
                                    continue;
                                }
                            }
                        }
                        select! {
                            () = runtime.reconcile_target(
                                target,
                                scheduled + runtime.interval,
                            ) => {}
                            changed = shutdown.changed() => {
                                if changed.is_err() || *shutdown.borrow() { return; }
                            }
                        }
                        drop(permit);
                    }
                }
            })
            .await;
    }

    async fn reconcile_target(&self, target: &SweepTarget, census_deadline: Instant) {
        let loaded = match self
            .state
            .load_target_with_cool_off(&target.repository, target.pull_request, self.cool_off)
            .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::error!(repository = %target.repository.as_str(),
                    pull_request = target.pull_request.get(), cause = %error,
                    "convergence sweep state read failed");
                self.record_failure(
                    target,
                    None,
                    ConvergenceSweepFailureKind::StateAccess,
                    CensusError::State,
                )
                .await;
                return;
            }
        };
        if loaded
            .as_ref()
            .is_some_and(|state| state.is_parked() || !state.retry_ready())
        {
            return;
        }
        if let Some((dispatch, observation)) = loaded
            .as_ref()
            .and_then(|state| state.pending_dispatch().zip(state.pending_observation()))
        {
            match self
                .state
                .record_dispatch(
                    uuid::Uuid::now_v7(),
                    &target.repository,
                    target.pull_request,
                    observation,
                    dispatch.dispatch_id(),
                    dispatch.session_id(),
                )
                .await
            {
                Ok(()) => {
                    let _ = self.eligibility_nudge.nudge(dispatch.session_id());
                }
                Err(error) => {
                    tracing::error!(repository = %target.repository.as_str(),
                        pull_request = target.pull_request.get(), cause = %error,
                        "convergence sweep could not repair a committed dispatch projection");
                    if error.commit_ambiguous() {
                        return;
                    }
                    self.record_failure(
                        target,
                        Some(observation),
                        ConvergenceSweepFailureKind::StateAccess,
                        CensusError::State,
                    )
                    .await;
                }
            }
            return;
        }
        let fetched = match select! {
            fetched = self.fetch(target) => fetched,
            _ = sleep_until(census_deadline) => {
                tracing::warn!(
                    repository = %target.repository.as_str(),
                    pull_request = target.pull_request.get(),
                    "convergence sweep provider census exceeded its polling interval"
                );
                Err(CensusError::Response)
            }
        } {
            Ok(fetched) => fetched,
            Err(cause) => {
                self.record_failure(target, None, ConvergenceSweepFailureKind::FactsFetch, cause)
                    .await;
                return;
            }
        };
        let observation = ConvergenceSweepObservation::new(
            fetched.facts.head_sha().clone(),
            fetched.facts.unresolved_review_threads(),
        );
        let convergence = evaluate_pull_request_convergence(&fetched.facts);
        if convergence.is_converged() {
            self.record_decision(target, &observation, ConvergenceSweepDecision::Converged)
                .await;
            return;
        }
        if let Some(dispatch) = loaded.as_ref().and_then(|state| state.latest_dispatch()) {
            let dispatch_observation = loaded
                .as_ref()
                .and_then(|state| state.latest_dispatch_observation());
            if dispatch_observation.is_none() {
                self.record_dispatch_decision(
                    target,
                    &observation,
                    dispatch.dispatch_id(),
                    dispatch.session_id(),
                    if dispatch.is_live() {
                        ConvergenceSweepDecision::LiveSession
                    } else {
                        ConvergenceSweepDecision::CoolingOff
                    },
                )
                .await;
                return;
            }
            let unchanged = dispatch_observation == Some(&observation);
            let cool_off_elapsed = loaded
                .as_ref()
                .is_some_and(|state| state.cool_off_elapsed());
            if unchanged && !dispatch.has_model_activity() && cool_off_elapsed {
                match self
                    .state
                    .record_no_model_activity_failure(
                        uuid::Uuid::now_v7(),
                        &target.repository,
                        target.pull_request,
                        &observation,
                        dispatch.session_id(),
                    )
                    .await
                {
                    Ok(disposition) => tracing::warn!(
                        repository = %target.repository.as_str(),
                        pull_request = target.pull_request.get(),
                        ?disposition,
                        "convergence sweep evaluated inactive session"
                    ),
                    Err(error) => {
                        tracing::error!(
                            repository = %target.repository.as_str(),
                            pull_request = target.pull_request.get(),
                            cause = %error,
                            "convergence sweep inactivity decision could not be recorded"
                        );
                        if error.commit_ambiguous() {
                            return;
                        }
                        self.record_failure(
                            target,
                            Some(&observation),
                            ConvergenceSweepFailureKind::StateAccess,
                            CensusError::State,
                        )
                        .await;
                    }
                }
                return;
            }
            if dispatch.is_live() {
                self.record_dispatch_decision(
                    target,
                    &observation,
                    dispatch.dispatch_id(),
                    dispatch.session_id(),
                    ConvergenceSweepDecision::LiveSession,
                )
                .await;
                return;
            }
            if !cool_off_elapsed {
                self.record_dispatch_decision(
                    target,
                    &observation,
                    dispatch.dispatch_id(),
                    dispatch.session_id(),
                    ConvergenceSweepDecision::CoolingOff,
                )
                .await;
                return;
            }
        }
        let Some(template) = self.templates.resolve(&self.template) else {
            self.record_failure(
                target,
                Some(&observation),
                ConvergenceSweepFailureKind::TemplateDrift,
                CensusError::Shape,
            )
            .await;
            return;
        };
        let context = match commission_content(target, &fetched, &convergence) {
            Ok(context) => context,
            Err(()) => {
                self.record_failure(
                    target,
                    Some(&observation),
                    ConvergenceSweepFailureKind::CommissionRefused,
                    CensusError::Shape,
                )
                .await;
                return;
            }
        };
        let content_digest: [u8; 32] = Sha256::digest(context.as_bytes()).into();
        let proposed = DurableCommandId::from_uuid(uuid::Uuid::now_v7());
        let command = match self
            .state
            .begin_commission(
                &target.repository,
                target.pull_request,
                &observation,
                content_digest,
                proposed,
            )
            .await
        {
            Ok(command) => command,
            Err(error) => {
                tracing::error!(repository = %target.repository.as_str(),
                    pull_request = target.pull_request.get(), cause = %error,
                    "convergence sweep commission fence could not be recorded");
                if error.commit_ambiguous() {
                    return;
                }
                self.record_failure(
                    target,
                    Some(&observation),
                    ConvergenceSweepFailureKind::StateAccess,
                    CensusError::State,
                )
                .await;
                return;
            }
        };
        let request = match commission_request(target, &fetched, command, &self.template, context) {
            Ok(request) => request,
            Err(()) => {
                self.record_failure(
                    target,
                    Some(&observation),
                    ConvergenceSweepFailureKind::CommissionRefused,
                    CensusError::Shape,
                )
                .await;
                return;
            }
        };
        let mut ids = UuidV7CommissionedDispatchIdGenerator;
        let prepared = match request.prepare(
            &mut ids,
            template.provenance().clone(),
            template.defaults().clone(),
        ) {
            Ok(prepared) => prepared,
            Err(_) => {
                self.record_failure(
                    target,
                    Some(&observation),
                    ConvergenceSweepFailureKind::TemplateDrift,
                    CensusError::Shape,
                )
                .await;
                return;
            }
        };
        match self
            .commissioned
            .commission_after_cool_off(prepared, self.cool_off, |alias| {
                self.models.resolve_alias(alias)
            })
            .await
        {
            Ok(
                CommissionDispatchOutcome::Dispatched { dispatch, session }
                | CommissionDispatchOutcome::Replayed { dispatch, session },
            ) => {
                if let Err(error) = self
                    .state
                    .record_dispatch(
                        uuid::Uuid::now_v7(),
                        &target.repository,
                        target.pull_request,
                        &observation,
                        dispatch.into_uuid(),
                        session,
                    )
                    .await
                {
                    tracing::error!(repository = %target.repository.as_str(),
                        pull_request = target.pull_request.get(), cause = %error,
                        "convergence sweep committed a session but could not record its local projection");
                    if error.commit_ambiguous() {
                        return;
                    }
                    self.record_failure(
                        target,
                        Some(&observation),
                        ConvergenceSweepFailureKind::StateAccess,
                        CensusError::State,
                    )
                    .await;
                }
                let _ = self.eligibility_nudge.nudge(session);
            }
            Ok(CommissionDispatchOutcome::TargetBusy { .. }) => {
                self.record_decision(target, &observation, ConvergenceSweepDecision::LiveSession)
                    .await;
            }
            Ok(CommissionDispatchOutcome::TargetCoolingOff { .. }) => {
                self.record_decision(target, &observation, ConvergenceSweepDecision::CoolingOff)
                    .await;
            }
            Err(error) if error.commit_ambiguous() => {
                tracing::error!(repository = %target.repository.as_str(),
                    pull_request = target.pull_request.get(), cause = %error,
                    "convergence sweep commission outcome is commit-ambiguous");
            }
            Ok(CommissionDispatchOutcome::ConflictingReuse) | Err(_) => {
                self.record_failure(
                    target,
                    Some(&observation),
                    ConvergenceSweepFailureKind::CommissionRefused,
                    CensusError::Request,
                )
                .await;
            }
        }
    }

    async fn record_decision(
        &self,
        target: &SweepTarget,
        observation: &ConvergenceSweepObservation,
        decision: ConvergenceSweepDecision,
    ) {
        if let Err(error) = self
            .state
            .record_decision(
                uuid::Uuid::now_v7(),
                &target.repository,
                target.pull_request,
                observation,
                decision,
            )
            .await
        {
            tracing::error!(repository = %target.repository.as_str(),
                pull_request = target.pull_request.get(), cause = %error,
                "convergence sweep decision could not be recorded");
            if error.commit_ambiguous() {
                return;
            }
            self.record_failure(
                target,
                Some(observation),
                ConvergenceSweepFailureKind::StateAccess,
                CensusError::State,
            )
            .await;
        }
    }

    async fn record_dispatch_decision(
        &self,
        target: &SweepTarget,
        observation: &ConvergenceSweepObservation,
        dispatch_id: uuid::Uuid,
        session_id: signalbox_domain::SessionId,
        decision: ConvergenceSweepDecision,
    ) {
        if let Err(error) = self
            .state
            .record_dispatch_decision(
                uuid::Uuid::now_v7(),
                &target.repository,
                target.pull_request,
                observation,
                (dispatch_id, session_id),
                decision,
            )
            .await
        {
            tracing::error!(
                repository = %target.repository.as_str(),
                pull_request = target.pull_request.get(),
                cause = %error,
                "convergence sweep dispatch decision could not be recorded"
            );
            if error.commit_ambiguous() {
                return;
            }
            self.record_failure(
                target,
                Some(observation),
                ConvergenceSweepFailureKind::StateAccess,
                CensusError::State,
            )
            .await;
        }
    }

    async fn record_failure(
        &self,
        target: &SweepTarget,
        observation: Option<&ConvergenceSweepObservation>,
        failure: ConvergenceSweepFailureKind,
        cause: CensusError,
    ) {
        match self
            .state
            .record_failure(
                uuid::Uuid::now_v7(),
                &target.repository,
                target.pull_request,
                observation,
                failure,
                ConvergenceSweepRetryPolicy {
                    backoff_base: RETRY_BACKOFF_BASE,
                    backoff_cap: RETRY_BACKOFF_CAP,
                },
            )
            .await
        {
            Ok(disposition) => tracing::warn!(repository = %target.repository.as_str(),
                pull_request = target.pull_request.get(), ?failure, ?cause, ?disposition,
                "convergence sweep target failed"),
            Err(error) => tracing::error!(repository = %target.repository.as_str(),
                pull_request = target.pull_request.get(), ?failure, ?cause, cause = %error,
                "convergence sweep failure could not be recorded"),
        }
    }

    async fn fetch(&self, target: &SweepTarget) -> Result<FetchedPullRequest, CensusError> {
        let credential = target
            .credentials
            .resolve(&target.credential_reference)
            .await
            .map_err(|_| CensusError::Credential)?;
        if credential.expose_bytes().is_empty()
            || credential.expose_bytes().len() > MAX_CREDENTIAL_BYTES
        {
            return Err(CensusError::Credential);
        }
        let mut authorization = Vec::with_capacity(7 + credential.expose_bytes().len());
        authorization.extend_from_slice(b"Bearer ");
        authorization.extend_from_slice(credential.expose_bytes());
        let mut authorization =
            HeaderValue::from_bytes(&authorization).map_err(|_| CensusError::Credential)?;
        authorization.set_sensitive(true);
        let (namespace, name) = target
            .repository
            .as_str()
            .split_once('/')
            .ok_or(CensusError::Shape)?;
        let variables = json!({"namespace": namespace, "name": name,
            "number": target.pull_request.get()});
        let root = self
            .graphql(DETAILS_QUERY, variables.clone(), &authorization)
            .await?;
        let pull = root
            .pointer("/data/repository/pullRequest")
            .ok_or(CensusError::Shape)?;
        if pull.get("state").and_then(Value::as_str) != Some("OPEN") {
            return Err(CensusError::Shape);
        }
        let head_sha = commit_at(pull, "headRefOid")?;
        let head_branch = branch_at(pull, "headRefName")?;
        let base_branch = branch_at(pull, "baseRefName")?;
        let base_sha = commit_at(pull, "baseRefOid")?;
        let head_repository = head_repository_at(pull)?;
        let checked_head_sha = checked_head_at(pull)?;
        let mergeable_state = mergeable_state_at(pull)?;
        let draft_state = draft_state_at(pull)?;
        let initial_thread_states = review_thread_states(
            pull.pointer("/reviewThreads/nodes")
                .and_then(Value::as_array)
                .ok_or(CensusError::Shape)?,
        )?;
        let mut thread_states = initial_thread_states.clone();
        let (initial_checks, initial_check_page) = initial_checks(pull)?;
        let mut checks = initial_checks.clone();
        let mut check_page = initial_check_page.clone();
        let initial_thread_page = page_info(pull.pointer("/reviewThreads/pageInfo"))?;
        let mut thread_page = initial_thread_page.clone();
        let mut thread_pages = 1usize;
        while thread_page.has_next {
            thread_pages += 1;
            if thread_pages > MAX_CONNECTION_PAGES {
                return Err(CensusError::Pagination);
            }
            let mut next = variables.clone();
            next["after"] = Value::String(thread_page.cursor.ok_or(CensusError::Shape)?);
            let page = self.graphql(THREADS_QUERY, next, &authorization).await?;
            let connection = threads_page(&page, &head_sha, &head_branch, &base_branch, &base_sha)?;
            thread_states.extend(review_thread_states(
                connection
                    .get("nodes")
                    .and_then(Value::as_array)
                    .ok_or(CensusError::Shape)?,
            )?);
            thread_page = page_info(connection.get("pageInfo"))?;
        }
        let mut check_pages = 1usize;
        while check_page.has_next {
            check_pages += 1;
            if check_pages > MAX_CONNECTION_PAGES {
                return Err(CensusError::Pagination);
            }
            let mut next = variables.clone();
            next["after"] = Value::String(check_page.cursor.ok_or(CensusError::Shape)?);
            let page = self.graphql(CHECKS_QUERY, next, &authorization).await?;
            let connection = checks_page(&page, &head_sha, &head_branch, &base_branch, &base_sha)?;
            checks.extend(decode_checks(
                connection
                    .get("nodes")
                    .and_then(Value::as_array)
                    .ok_or(CensusError::Shape)?,
            )?);
            check_page = page_info(connection.get("pageInfo"))?;
        }
        // A paginated census assembles its snapshot from a traversal that spans
        // many responses, so the fence below and the revalidation traversals
        // after it bound the whole window in one direction: the details reread
        // proves the refs, mergeable state, draft state, and head repository
        // still hold, and the re-traversals that follow it prove every page of
        // both connections still holds. Revalidating a connection before the
        // fence instead would leave a gap — a thread or check on the second or
        // later page could change after its own reread but before the fence,
        // and because the fence compares only the initial pages, refs, and page
        // information, all of which can be identical across that change, the
        // stale buffers would be accepted.
        if thread_pages > 1 || check_pages > 1 {
            let revalidated = self
                .graphql(DETAILS_QUERY, variables.clone(), &authorization)
                .await?;
            let revalidated_pull = revalidated
                .pointer("/data/repository/pullRequest")
                .ok_or(CensusError::Shape)?;
            validate_paginated_pull(
                revalidated_pull,
                &head_sha,
                &head_branch,
                &base_branch,
                &base_sha,
            )?;
            if mergeable_state_at(revalidated_pull)? != mergeable_state {
                return Err(CensusError::State);
            }
            ensure_draft_state_stable(draft_state, draft_state_at(revalidated_pull)?)?;
            ensure_head_repository_stable(
                &head_repository,
                &head_repository_at(revalidated_pull)?,
            )?;
            ensure_final_connections_stable(
                revalidated_pull,
                &initial_thread_states,
                &initial_thread_page,
                &initial_checks,
                &initial_check_page,
            )?;
        }
        if thread_pages > 1 {
            let mut next = variables.clone();
            next["after"] = Value::Null;
            let page = self.graphql(THREADS_QUERY, next, &authorization).await?;
            let connection = threads_page(&page, &head_sha, &head_branch, &base_branch, &base_sha)?;
            let mut revalidated = review_thread_states(
                connection
                    .get("nodes")
                    .and_then(Value::as_array)
                    .ok_or(CensusError::Shape)?,
            )?;
            let mut revalidation_page = page_info(connection.get("pageInfo"))?;
            let mut revalidation_pages = 1usize;
            while revalidation_page.has_next {
                revalidation_pages += 1;
                if revalidation_pages > MAX_CONNECTION_PAGES {
                    return Err(CensusError::Pagination);
                }
                let mut next = variables.clone();
                next["after"] = Value::String(revalidation_page.cursor.ok_or(CensusError::Shape)?);
                let page = self.graphql(THREADS_QUERY, next, &authorization).await?;
                let connection =
                    threads_page(&page, &head_sha, &head_branch, &base_branch, &base_sha)?;
                revalidated.extend(review_thread_states(
                    connection
                        .get("nodes")
                        .and_then(Value::as_array)
                        .ok_or(CensusError::Shape)?,
                )?);
                revalidation_page = page_info(connection.get("pageInfo"))?;
            }
            ensure_threads_stable(&thread_states, &revalidated)?;
        }
        if checks_require_revalidation(thread_pages, check_pages) {
            let mut next = variables.clone();
            next["after"] = Value::Null;
            let page = self.graphql(CHECKS_QUERY, next, &authorization).await?;
            let (mut revalidated, mut revalidation_page) =
                initial_checks_page(&page, &head_sha, &head_branch, &base_branch, &base_sha)?;
            let mut revalidation_pages = 1usize;
            while revalidation_page.has_next {
                revalidation_pages += 1;
                if revalidation_pages > MAX_CONNECTION_PAGES {
                    return Err(CensusError::Pagination);
                }
                let mut next = variables.clone();
                next["after"] = Value::String(revalidation_page.cursor.ok_or(CensusError::Shape)?);
                let page = self.graphql(CHECKS_QUERY, next, &authorization).await?;
                let connection =
                    checks_page(&page, &head_sha, &head_branch, &base_branch, &base_sha)?;
                revalidated.extend(decode_checks(
                    connection
                        .get("nodes")
                        .and_then(Value::as_array)
                        .ok_or(CensusError::Shape)?,
                )?);
                revalidation_page = page_info(connection.get("pageInfo"))?;
            }
            ensure_checks_stable(&checks, &revalidated)?;
        }
        let unresolved = unresolved_threads(&thread_states);
        Ok(FetchedPullRequest {
            base_branch,
            head_branch,
            head_repository,
            facts: PullRequestConvergenceFacts::new(
                head_sha,
                checked_head_sha,
                draft_state,
                unresolved,
                mergeable_state,
                checks,
            ),
        })
    }

    async fn graphql(
        &self,
        query: &str,
        variables: Value,
        authorization: &HeaderValue,
    ) -> Result<Value, CensusError> {
        let body = serde_json::to_vec(&json!({"query": query, "variables": variables}))
            .map_err(|_| CensusError::Decode)?;
        let mut attempt = 0usize;
        let bytes = 'attempts: loop {
            attempt += 1;
            let sent = self
                .client
                .post(GRAPHQL_URL)
                .header(AUTHORIZATION, authorization.clone())
                .header(ACCEPT, "application/vnd.github+json")
                .header(CONTENT_TYPE, "application/json")
                .header(USER_AGENT, USER_AGENT_VALUE)
                .body(body.clone())
                .send()
                .await;
            match sent {
                Ok(response)
                    if attempt < MAX_REQUEST_ATTEMPTS
                        && (response.status().is_server_error()
                            || response.status() == StatusCode::TOO_MANY_REQUESTS) =>
                {
                    sleep(REQUEST_RETRY_DELAY).await;
                }
                Ok(mut response) => {
                    if response.status() != StatusCode::OK {
                        return Err(CensusError::Response);
                    }
                    let mut bytes = Vec::new();
                    loop {
                        match response.chunk().await {
                            Ok(Some(chunk)) => {
                                let next = bytes
                                    .len()
                                    .checked_add(chunk.len())
                                    .ok_or(CensusError::Response)?;
                                if next > MAX_RESPONSE_BYTES {
                                    return Err(CensusError::Response);
                                }
                                bytes.extend_from_slice(&chunk);
                            }
                            Ok(None) => break 'attempts bytes,
                            Err(_) if attempt < MAX_REQUEST_ATTEMPTS => {
                                sleep(REQUEST_RETRY_DELAY).await;
                                continue 'attempts;
                            }
                            Err(_) => return Err(CensusError::Response),
                        }
                    }
                }
                Err(_) if attempt < MAX_REQUEST_ATTEMPTS => {
                    sleep(REQUEST_RETRY_DELAY).await;
                }
                Err(_) => return Err(CensusError::Request),
            }
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| CensusError::Decode)?;
        if value.get("errors").is_some() {
            return Err(CensusError::Response);
        }
        Ok(value)
    }
}

struct FetchedPullRequest {
    base_branch: BranchName,
    head_branch: BranchName,
    head_repository: RepositorySlug,
    facts: PullRequestConvergenceFacts,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PageInfo {
    has_next: bool,
    cursor: Option<String>,
}
impl PageInfo {
    const fn done() -> Self {
        Self {
            has_next: false,
            cursor: None,
        }
    }
}

fn page_info(value: Option<&Value>) -> Result<PageInfo, CensusError> {
    let value = value.ok_or(CensusError::Shape)?;
    Ok(PageInfo {
        has_next: value
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .ok_or(CensusError::Shape)?,
        cursor: value
            .get("endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn review_thread_states(values: &[Value]) -> Result<Vec<bool>, CensusError> {
    values
        .iter()
        .map(|value| {
            value
                .get("isResolved")
                .and_then(Value::as_bool)
                .ok_or(CensusError::Shape)
        })
        .collect()
}

fn unresolved_threads(states: &[bool]) -> u64 {
    states.iter().filter(|resolved| !**resolved).count() as u64
}

fn ensure_threads_stable(observed: &[bool], revalidated: &[bool]) -> Result<(), CensusError> {
    if observed == revalidated {
        Ok(())
    } else {
        Err(CensusError::State)
    }
}

fn ensure_final_connections_stable(
    pull: &Value,
    observed_threads: &[bool],
    observed_thread_page: &PageInfo,
    observed_checks: &[PullRequestCheck],
    observed_check_page: &PageInfo,
) -> Result<(), CensusError> {
    let revalidated_threads = review_thread_states(
        pull.pointer("/reviewThreads/nodes")
            .and_then(Value::as_array)
            .ok_or(CensusError::Shape)?,
    )?;
    let revalidated_thread_page = page_info(pull.pointer("/reviewThreads/pageInfo"))?;
    let (revalidated_checks, revalidated_check_page) = initial_checks(pull)?;
    ensure_threads_stable(observed_threads, &revalidated_threads)?;
    ensure_checks_stable(observed_checks, &revalidated_checks)?;
    if observed_thread_page != &revalidated_thread_page
        || observed_check_page != &revalidated_check_page
    {
        return Err(CensusError::State);
    }
    Ok(())
}

const fn checks_require_revalidation(thread_pages: usize, check_pages: usize) -> bool {
    thread_pages > 1 || check_pages > 1
}

fn checks_page<'a>(
    page: &'a Value,
    expected_head: &CommitSha,
    expected_head_branch: &BranchName,
    expected_base: &BranchName,
    expected_base_sha: &CommitSha,
) -> Result<&'a Value, CensusError> {
    let pull = page
        .pointer("/data/repository/pullRequest")
        .ok_or(CensusError::Shape)?;
    validate_paginated_pull(
        pull,
        expected_head,
        expected_head_branch,
        expected_base,
        expected_base_sha,
    )?;
    let commit = pull
        .pointer("/commits/nodes/0/commit")
        .ok_or(CensusError::Shape)?;
    if commit_at(commit, "oid")? != *expected_head {
        return Err(CensusError::Shape);
    }
    commit
        .pointer("/statusCheckRollup/contexts")
        .ok_or(CensusError::Shape)
}

fn initial_checks_page(
    page: &Value,
    expected_head: &CommitSha,
    expected_head_branch: &BranchName,
    expected_base: &BranchName,
    expected_base_sha: &CommitSha,
) -> Result<(Vec<PullRequestCheck>, PageInfo), CensusError> {
    let pull = page
        .pointer("/data/repository/pullRequest")
        .ok_or(CensusError::Shape)?;
    validate_paginated_pull(
        pull,
        expected_head,
        expected_head_branch,
        expected_base,
        expected_base_sha,
    )?;
    let commit = pull
        .pointer("/commits/nodes/0/commit")
        .ok_or(CensusError::Shape)?;
    if commit_at(commit, "oid")? != *expected_head {
        return Err(CensusError::Shape);
    }
    initial_checks(pull)
}

fn threads_page<'a>(
    page: &'a Value,
    expected_head: &CommitSha,
    expected_head_branch: &BranchName,
    expected_base: &BranchName,
    expected_base_sha: &CommitSha,
) -> Result<&'a Value, CensusError> {
    let pull = page
        .pointer("/data/repository/pullRequest")
        .ok_or(CensusError::Shape)?;
    validate_paginated_pull(
        pull,
        expected_head,
        expected_head_branch,
        expected_base,
        expected_base_sha,
    )?;
    pull.get("reviewThreads").ok_or(CensusError::Shape)
}

fn validate_paginated_pull(
    pull: &Value,
    expected_head: &CommitSha,
    expected_head_branch: &BranchName,
    expected_base: &BranchName,
    expected_base_sha: &CommitSha,
) -> Result<(), CensusError> {
    if pull.get("state").and_then(Value::as_str) != Some("OPEN")
        || commit_at(pull, "headRefOid")? != *expected_head
        || branch_at(pull, "headRefName")? != *expected_head_branch
        || branch_at(pull, "baseRefName")? != *expected_base
        || commit_at(pull, "baseRefOid")? != *expected_base_sha
    {
        return Err(CensusError::Shape);
    }
    Ok(())
}

fn mergeable_state_at(pull: &Value) -> Result<MergeableState, CensusError> {
    match pull.get("mergeable").and_then(Value::as_str) {
        Some("MERGEABLE") => Ok(MergeableState::Mergeable),
        Some("CONFLICTING") => Ok(MergeableState::Conflicting),
        Some("UNKNOWN") => Ok(MergeableState::Unknown),
        _ => Err(CensusError::Shape),
    }
}

fn draft_state_at(
    pull: &Value,
) -> Result<signalbox_application::PullRequestDraftState, CensusError> {
    match pull.get("isDraft").and_then(Value::as_bool) {
        Some(true) => Ok(signalbox_application::PullRequestDraftState::Draft),
        Some(false) => Ok(signalbox_application::PullRequestDraftState::ReadyForReview),
        None => Err(CensusError::Shape),
    }
}

fn ensure_draft_state_stable(
    observed: signalbox_application::PullRequestDraftState,
    revalidated: signalbox_application::PullRequestDraftState,
) -> Result<(), CensusError> {
    if observed == revalidated {
        Ok(())
    } else {
        Err(CensusError::State)
    }
}

fn decode_checks(values: &[Value]) -> Result<Vec<PullRequestCheck>, CensusError> {
    values
        .iter()
        .map(
            |value| match value.get("__typename").and_then(Value::as_str) {
                Some("CheckRun") => {
                    let status = value
                        .get("status")
                        .and_then(Value::as_str)
                        .ok_or(CensusError::Shape)?;
                    Ok(PullRequestCheck::new(
                        value
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or(CensusError::Shape)?
                            .to_owned(),
                        if status == "COMPLETED" {
                            PullRequestCheckState::CheckRunCompleted {
                                conclusion: value
                                    .get("conclusion")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                            }
                        } else {
                            PullRequestCheckState::CheckRunInProgress
                        },
                    ))
                }
                Some("StatusContext") => Ok(PullRequestCheck::new(
                    value
                        .get("context")
                        .and_then(Value::as_str)
                        .ok_or(CensusError::Shape)?
                        .to_owned(),
                    PullRequestCheckState::StatusContext {
                        state: value
                            .get("state")
                            .and_then(Value::as_str)
                            .ok_or(CensusError::Shape)?
                            .to_owned(),
                    },
                )),
                _ => Err(CensusError::Shape),
            },
        )
        .collect()
}

fn ensure_checks_stable(
    observed: &[PullRequestCheck],
    revalidated: &[PullRequestCheck],
) -> Result<(), CensusError> {
    if observed == revalidated {
        Ok(())
    } else {
        Err(CensusError::State)
    }
}

fn checked_head_at(pull: &Value) -> Result<Option<CommitSha>, CensusError> {
    let rollup = pull
        .pointer("/commits/nodes/0/commit/statusCheckRollup")
        .ok_or(CensusError::Shape)?;
    if rollup.is_null() {
        return Ok(None);
    }
    commit_at(
        pull.pointer("/commits/nodes/0/commit")
            .ok_or(CensusError::Shape)?,
        "oid",
    )
    .map(Some)
}

fn initial_checks(pull: &Value) -> Result<(Vec<PullRequestCheck>, PageInfo), CensusError> {
    let rollup = pull
        .pointer("/commits/nodes/0/commit/statusCheckRollup")
        .ok_or(CensusError::Shape)?;
    if rollup.is_null() {
        return Ok((Vec::new(), PageInfo::done()));
    }
    let contexts = rollup.get("contexts").ok_or(CensusError::Shape)?;
    let nodes = contexts
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or(CensusError::Shape)?;
    Ok((decode_checks(nodes)?, page_info(contexts.get("pageInfo"))?))
}

fn commit_at(value: &Value, key: &str) -> Result<CommitSha, CensusError> {
    CommitSha::try_new(
        value
            .get(key)
            .and_then(Value::as_str)
            .ok_or(CensusError::Shape)?
            .to_owned(),
    )
    .map_err(|_| CensusError::Shape)
}

fn branch_at(value: &Value, key: &str) -> Result<BranchName, CensusError> {
    BranchName::try_new(
        value
            .get(key)
            .and_then(Value::as_str)
            .ok_or(CensusError::Shape)?
            .to_owned(),
    )
    .map_err(|_| CensusError::Shape)
}

fn commission_request(
    target: &SweepTarget,
    fetched: &FetchedPullRequest,
    command: DurableCommandId,
    template: &signalbox_domain::SessionTemplateName,
    context: String,
) -> Result<CommissionDispatchRequest, ()> {
    CommissionDispatchRequest::try_new(
        command,
        template.clone(),
        CommissionedDispatchFence::PullRequest {
            repository: target.repository.clone(),
            pull_request: target.pull_request,
            head_sha: fetched.facts.head_sha().clone(),
            head_repository: fetched.head_repository.clone(),
            head_branch: fetched.head_branch.clone(),
            base_branch: fetched.base_branch.clone(),
        },
        GoalStatement::try_new(format!(
            "Converge pull request {} in {}.",
            target.pull_request.get(),
            target.repository.as_str()
        ))
        .map_err(|_| ())?,
        UserContent::try_text(context).map_err(|_| ())?,
    )
    .map_err(|_| ())
}

fn commission_content(
    target: &SweepTarget,
    fetched: &FetchedPullRequest,
    convergence: &PullRequestConvergence,
) -> Result<String, ()> {
    let blockers = convergence
        .blockers()
        .iter()
        .map(blocker_text)
        .collect::<Vec<_>>();
    let gating_checks = fetched
        .facts
        .checks()
        .iter()
        .filter(|check| !check.is_non_gating())
        .map(|check| json!({"name": check.name(), "state": check.observed_state()}))
        .collect::<Vec<_>>();
    let non_gating_checks = fetched
        .facts
        .checks()
        .iter()
        .filter(|check| check.is_non_gating())
        .map(|check| json!({"name": check.name(), "state": check.observed_state()}))
        .collect::<Vec<_>>();
    serde_json::to_string(&json!({
        "kind": "pull_request_convergence_reconciliation",
        "repository": target.repository.as_str(),
        "pull_request": target.pull_request.get(),
        "head_sha": fetched.facts.head_sha().as_str(),
        "checked_head_sha": fetched.facts.checked_head_sha().map(CommitSha::as_str),
        "head_repository": fetched.head_repository.as_str(),
        "base_branch": fetched.base_branch.as_str(),
        "head_branch": fetched.head_branch.as_str(),
        "draft": fetched.facts.draft().is_draft(),
        "unresolved_review_threads": fetched.facts.unresolved_review_threads(),
        "mergeable_state": format!("{:?}", fetched.facts.mergeable_state()).to_lowercase(),
        "gating_checks": gating_checks,
        "non_gating_checks": non_gating_checks,
        "blockers": blockers,
    }))
    .map_err(|_| ())
}

fn head_repository_at(pull: &Value) -> Result<RepositorySlug, CensusError> {
    RepositorySlug::try_new(
        pull.pointer("/headRepository/name_with_owner")
            .and_then(Value::as_str)
            .ok_or(CensusError::Shape)?
            .to_lowercase(),
    )
    .map_err(|_| CensusError::Shape)
}

fn ensure_head_repository_stable(
    observed: &RepositorySlug,
    revalidated: &RepositorySlug,
) -> Result<(), CensusError> {
    if observed == revalidated {
        Ok(())
    } else {
        Err(CensusError::State)
    }
}

fn blocker_text(blocker: &PullRequestConvergenceBlocker) -> String {
    match blocker {
        PullRequestConvergenceBlocker::UnresolvedReviewThreads(count) => {
            format!("unresolved-review-threads:{count}")
        }
        PullRequestConvergenceBlocker::ChecksNotForCurrentHead => {
            String::from("checks-not-for-current-head")
        }
        PullRequestConvergenceBlocker::CheckNotGreen { name, state } => {
            format!("check-not-green:{name}:{state}")
        }
        PullRequestConvergenceBlocker::BaseConflict => String::from("base-conflict"),
        PullRequestConvergenceBlocker::MergeabilityUnknown => String::from("mergeability-unknown"),
    }
}

#[cfg(test)]
mod tests {
    use signalbox_application::InProcessEligibilityWorkSource;
    use signalbox_persistence::{
        convergence_sweep::ConvergenceSweepFailureDisposition, disposable_postgres_server_args,
        disposable_postgres_state_tmpfs, disposable_test_container_labels,
        local_test_connection_options, migrate, scheduler::PostgresEligibilitySweep,
    };
    use sqlx::postgres::PgPoolOptions;
    use testcontainers_modules::{
        postgres::Postgres as TestPostgres,
        testcontainers::{self, ImageExt, runners::AsyncRunner},
    };

    use super::*;

    fn sha(value: char) -> CommitSha {
        CommitSha::try_new(value.to_string().repeat(40)).expect("fixture SHA is valid")
    }

    #[test]
    fn status_and_check_run_non_gating_rules_remain_distinct() {
        let status = PullRequestCheck::new(
            String::from("coderabbit"),
            PullRequestCheckState::StatusContext {
                state: String::from("FAILURE"),
            },
        );
        let run = PullRequestCheck::new(
            String::from("CodeRabbit"),
            PullRequestCheckState::CheckRunCompleted {
                conclusion: Some(String::from("FAILURE")),
            },
        );
        assert!(status.is_non_gating());
        assert!(!run.is_non_gating());
    }

    #[test]
    fn checks_query_is_scoped_to_the_requested_pull_request() {
        assert!(CHECKS_QUERY.contains("pullRequest(number: $number)"));
        assert!(CHECKS_QUERY.contains("commits(last: 1)"));
    }

    #[test]
    fn a_check_run_without_status_is_rejected() {
        let checks = [json!({
            "__typename": "CheckRun",
            "name": "test",
            "conclusion": "SUCCESS"
        })];

        assert!(matches!(decode_checks(&checks), Err(CensusError::Shape)));
    }

    #[test]
    fn absent_status_rollup_does_not_mark_checks_current() {
        let pull = json!({
            "commits": {
                "nodes": [{"commit": {"oid": sha('a').as_str(), "statusCheckRollup": null}}]
            }
        });

        assert_eq!(checked_head_at(&pull), Ok(None));
        let (checks, page) = initial_checks(&pull).expect("a null rollup is a complete absence");
        assert!(checks.is_empty());
        assert!(!page.has_next);
    }

    #[test]
    fn absent_status_rollup_remains_absent_during_revalidation() {
        let expected_head = sha('a');
        let expected_base_sha = sha('c');
        let expected_head_branch = BranchName::try_new(String::from("agent/convergence"))
            .expect("fixture head branch is valid");
        let expected_base =
            BranchName::try_new(String::from("main")).expect("fixture base branch is valid");
        let page = json!({
            "data": {"repository": {"pullRequest": {
                "state": "OPEN",
                "baseRefName": expected_base.as_str(),
                "baseRefOid": expected_base_sha.as_str(),
                "headRefName": expected_head_branch.as_str(),
                "headRefOid": expected_head.as_str(),
                "commits": {"nodes": [{"commit": {
                    "oid": expected_head.as_str(),
                    "statusCheckRollup": null
                }}]}
            }}}
        });

        let (checks, page_info) = initial_checks_page(
            &page,
            &expected_head,
            &expected_head_branch,
            &expected_base,
            &expected_base_sha,
        )
        .expect("stable rollup absence revalidates");

        assert!(checks.is_empty());
        assert!(!page_info.has_next);
    }

    #[test]
    fn mergeability_decoder_preserves_the_closed_provider_states() {
        assert_eq!(
            mergeable_state_at(&json!({"mergeable": "MERGEABLE"})),
            Ok(MergeableState::Mergeable)
        );
        assert_eq!(
            mergeable_state_at(&json!({"mergeable": "CONFLICTING"})),
            Ok(MergeableState::Conflicting)
        );
        assert_eq!(
            mergeable_state_at(&json!({"mergeable": "UNKNOWN"})),
            Ok(MergeableState::Unknown)
        );
    }

    #[test]
    fn paginated_census_rejects_draft_state_drift() {
        assert_eq!(
            ensure_draft_state_stable(
                signalbox_application::PullRequestDraftState::ReadyForReview,
                signalbox_application::PullRequestDraftState::Draft,
            ),
            Err(CensusError::State)
        );
        assert_eq!(
            ensure_draft_state_stable(
                signalbox_application::PullRequestDraftState::Draft,
                signalbox_application::PullRequestDraftState::Draft,
            ),
            Ok(())
        );
    }

    #[test]
    fn a_partial_initial_status_rollup_is_rejected() {
        let missing_nodes = json!({
            "commits": {
                "nodes": [{"commit": {
                    "oid": sha('a').as_str(),
                    "statusCheckRollup": {
                        "contexts": {
                            "pageInfo": {"hasNextPage": false, "endCursor": null}
                        }
                    }
                }}]
            }
        });
        let missing_page_info = json!({
            "commits": {
                "nodes": [{"commit": {
                    "oid": sha('a').as_str(),
                    "statusCheckRollup": {"contexts": {"nodes": []}}
                }}]
            }
        });

        assert!(matches!(
            initial_checks(&missing_nodes),
            Err(CensusError::Shape)
        ));
        assert!(matches!(
            initial_checks(&missing_page_info),
            Err(CensusError::Shape)
        ));
    }

    #[test]
    fn a_second_checks_page_decodes_only_for_the_observed_snapshot() {
        let expected_head = sha('a');
        let expected_base_sha = sha('c');
        let expected_head_branch = BranchName::try_new(String::from("agent/convergence"))
            .expect("fixture head branch is valid");
        let expected_base =
            BranchName::try_new(String::from("main")).expect("fixture base branch is valid");
        let page = json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "state": "OPEN",
                        "baseRefName": expected_base.as_str(),
                        "baseRefOid": expected_base_sha.as_str(),
                        "headRefName": expected_head_branch.as_str(),
                        "headRefOid": expected_head.as_str(),
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": expected_head.as_str(),
                                    "statusCheckRollup": {
                                        "contexts": {
                                            "nodes": [{
                                                "__typename": "StatusContext",
                                                "context": "test",
                                                "state": "SUCCESS"
                                            }],
                                            "pageInfo": {
                                                "hasNextPage": false,
                                                "endCursor": null
                                            }
                                        }
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        });

        let connection = checks_page(
            &page,
            &expected_head,
            &expected_head_branch,
            &expected_base,
            &expected_base_sha,
        )
        .expect("snapshot-matched page decodes");
        let checks = decode_checks(
            connection
                .get("nodes")
                .and_then(Value::as_array)
                .expect("fixture carries check nodes"),
        )
        .expect("second-page check shape is valid");

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name(), "test");
    }

    #[test]
    fn a_checks_page_for_another_head_is_rejected() {
        let expected_base_sha = sha('c');
        let expected_head_branch = BranchName::try_new(String::from("agent/convergence"))
            .expect("fixture head branch is valid");
        let expected_base =
            BranchName::try_new(String::from("main")).expect("fixture base branch is valid");
        let page = json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "state": "OPEN",
                        "baseRefName": expected_base.as_str(),
                        "baseRefOid": expected_base_sha.as_str(),
                        "headRefName": expected_head_branch.as_str(),
                        "headRefOid": sha('b').as_str(),
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": sha('b').as_str(),
                                    "statusCheckRollup": {"contexts": {}}
                                }
                            }]
                        }
                    }
                }
            }
        });

        assert_eq!(
            checks_page(
                &page,
                &sha('a'),
                &expected_head_branch,
                &expected_base,
                &expected_base_sha,
            ),
            Err(CensusError::Shape)
        );
    }

    #[test]
    fn mutable_paginated_check_states_are_rejected() {
        let successful = PullRequestCheck::new(
            String::from("test"),
            PullRequestCheckState::CheckRunCompleted {
                conclusion: Some(String::from("SUCCESS")),
            },
        );
        let failed = PullRequestCheck::new(
            String::from("test"),
            PullRequestCheckState::CheckRunCompleted {
                conclusion: Some(String::from("FAILURE")),
            },
        );

        assert_eq!(
            ensure_checks_stable(std::slice::from_ref(&successful), &[failed]),
            Err(CensusError::State)
        );
        assert_eq!(
            ensure_checks_stable(
                std::slice::from_ref(&successful),
                std::slice::from_ref(&successful),
            ),
            Ok(())
        );
    }

    #[test]
    fn a_thread_page_for_the_observed_snapshot_decodes() {
        let expected_head = sha('a');
        let expected_base_sha = sha('c');
        let expected_head_branch = BranchName::try_new(String::from("agent/convergence"))
            .expect("fixture head branch is valid");
        let expected_base =
            BranchName::try_new(String::from("main")).expect("fixture base branch is valid");
        let page = json!({
            "data": {"repository": {"pullRequest": {
                "state": "OPEN",
                "baseRefName": expected_base.as_str(),
                "baseRefOid": expected_base_sha.as_str(),
                "headRefName": expected_head_branch.as_str(),
                "headRefOid": expected_head.as_str(),
                "reviewThreads": {"nodes": [{"isResolved": false}]}
            }}}
        });
        let connection = threads_page(
            &page,
            &expected_head,
            &expected_head_branch,
            &expected_base,
            &expected_base_sha,
        )
        .expect("snapshot-matched page decodes");
        let nodes = connection
            .get("nodes")
            .and_then(Value::as_array)
            .expect("fixture carries thread nodes");

        assert_eq!(
            unresolved_threads(&review_thread_states(nodes).expect("thread states decode")),
            1
        );
    }

    #[test]
    fn mutable_paginated_review_thread_states_are_rejected() {
        assert_eq!(
            ensure_threads_stable(&[true, false], &[false, true]),
            Err(CensusError::State)
        );
        assert_eq!(
            ensure_threads_stable(&[true, false], &[true, false]),
            Ok(())
        );
    }

    #[test]
    fn final_details_reject_changed_initial_connection_contents() {
        let observed_check = PullRequestCheck::new(
            String::from("test"),
            PullRequestCheckState::CheckRunCompleted {
                conclusion: Some(String::from("SUCCESS")),
            },
        );
        let pull = json!({
            "reviewThreads": {
                "nodes": [{"isResolved": false}],
                "pageInfo": {"hasNextPage": true, "endCursor": "threads-1"}
            },
            "commits": {"nodes": [{"commit": {
                "oid": sha('a').as_str(),
                "statusCheckRollup": {"contexts": {
                    "nodes": [{
                        "__typename": "CheckRun",
                        "name": "test",
                        "status": "COMPLETED",
                        "conclusion": "FAILURE"
                    }],
                    "pageInfo": {"hasNextPage": false, "endCursor": null}
                }}
            }}]}
        });

        let result = ensure_final_connections_stable(
            &pull,
            &[true],
            &PageInfo {
                has_next: true,
                cursor: Some(String::from("threads-1")),
            },
            &[observed_check],
            &PageInfo::done(),
        );

        assert_eq!(result, Err(CensusError::State));
    }

    #[test]
    fn paginated_census_rejects_a_head_repository_transfer() {
        let observed = RepositorySlug::try_new(String::from("contributor/repository"))
            .expect("fixture repository is valid");
        let transferred = RepositorySlug::try_new(String::from("successor/repository"))
            .expect("fixture repository is valid");

        assert_eq!(
            ensure_head_repository_stable(&observed, &transferred),
            Err(CensusError::State)
        );
        assert_eq!(ensure_head_repository_stable(&observed, &observed), Ok(()));
    }

    #[test]
    fn thread_pagination_revalidates_the_initial_checks() {
        assert!(checks_require_revalidation(2, 1));
        assert!(checks_require_revalidation(1, 2));
        assert!(!checks_require_revalidation(1, 1));
    }

    #[test]
    fn a_thread_page_for_another_head_is_rejected() {
        let expected_base_sha = sha('c');
        let expected_head_branch = BranchName::try_new(String::from("agent/convergence"))
            .expect("fixture head branch is valid");
        let expected_base =
            BranchName::try_new(String::from("main")).expect("fixture base branch is valid");
        let page = json!({
            "data": {"repository": {"pullRequest": {
                "state": "OPEN",
                "baseRefName": expected_base.as_str(),
                "baseRefOid": expected_base_sha.as_str(),
                "headRefName": expected_head_branch.as_str(),
                "headRefOid": sha('b').as_str(),
                "reviewThreads": {}
            }}}
        });

        assert_eq!(
            threads_page(
                &page,
                &sha('a'),
                &expected_head_branch,
                &expected_base,
                &expected_base_sha,
            ),
            Err(CensusError::Shape)
        );
    }

    #[test]
    fn paginated_pages_reject_closed_retargeted_base_advanced_or_renamed_pull_requests() {
        let expected_head = sha('a');
        let expected_base_sha = sha('c');
        let expected_head_branch = BranchName::try_new(String::from("agent/convergence"))
            .expect("fixture head branch is valid");
        let expected_base =
            BranchName::try_new(String::from("main")).expect("fixture base branch is valid");
        let closed = json!({
            "data": {"repository": {"pullRequest": {
                "state": "CLOSED",
                "baseRefName": expected_base.as_str(),
                "baseRefOid": expected_base_sha.as_str(),
                "headRefName": expected_head_branch.as_str(),
                "headRefOid": expected_head.as_str(),
                "reviewThreads": {}
            }}}
        });
        let retargeted = json!({
            "data": {"repository": {"pullRequest": {
                "state": "OPEN",
                "baseRefName": "release",
                "baseRefOid": expected_base_sha.as_str(),
                "headRefName": expected_head_branch.as_str(),
                "headRefOid": expected_head.as_str(),
                "commits": {"nodes": []}
            }}}
        });
        let base_advanced = json!({
            "data": {"repository": {"pullRequest": {
                "state": "OPEN",
                "baseRefName": expected_base.as_str(),
                "baseRefOid": sha('d').as_str(),
                "headRefName": expected_head_branch.as_str(),
                "headRefOid": expected_head.as_str(),
                "reviewThreads": {}
            }}}
        });
        let head_renamed = json!({
            "data": {"repository": {"pullRequest": {
                "state": "OPEN",
                "baseRefName": expected_base.as_str(),
                "baseRefOid": expected_base_sha.as_str(),
                "headRefName": "agent/renamed",
                "headRefOid": expected_head.as_str(),
                "reviewThreads": {}
            }}}
        });

        assert_eq!(
            threads_page(
                &closed,
                &expected_head,
                &expected_head_branch,
                &expected_base,
                &expected_base_sha,
            ),
            Err(CensusError::Shape)
        );
        assert_eq!(
            checks_page(
                &retargeted,
                &expected_head,
                &expected_head_branch,
                &expected_base,
                &expected_base_sha,
            ),
            Err(CensusError::Shape)
        );
        assert_eq!(
            threads_page(
                &base_advanced,
                &expected_head,
                &expected_head_branch,
                &expected_base,
                &expected_base_sha,
            ),
            Err(CensusError::Shape)
        );
        assert_eq!(
            threads_page(
                &head_renamed,
                &expected_head,
                &expected_head_branch,
                &expected_base,
                &expected_base_sha,
            ),
            Err(CensusError::Shape)
        );
    }

    // `reconcile_target` sequences the store primitives, and the branches it
    // takes before the provider census are reachable against a real database
    // with no network: the parked / `retry_ready` gate, the committed-dispatch
    // projection repair, and the census-failure path. The branches after a
    // successful census are not reachable here — `GRAPHQL_URL` is a const with
    // no injection seam, so a hand-built runtime cannot be pointed at a local
    // server. Every fixture below resolves its credential from a path that does
    // not exist, which makes `fetch` fail before it opens a connection.

    const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
    const DATABASE_NAME: &str = "signalbox_convergence_sweep";
    const DATABASE_USER: &str = "signalbox";
    const DATABASE_PASSWORD: &str = "signalbox-test-only";
    const FIXTURE_REPOSITORY: &str = "signalbox/repository";
    const FIXTURE_PULL_REQUEST: u64 = 892;
    const FIXTURE_HEAD_SHA: &str = "1111111111111111111111111111111111111111";
    const FIXTURE_HEAD_REPOSITORY: &str = "contributor/repository";
    const FIXTURE_HEAD_BRANCH: &str = "agent/convergence";
    const FIXTURE_BASE_BRANCH: &str = "main";
    const FIXTURE_TEMPLATE: &str = "review-response";
    const FIXTURE_UNRESOLVED_THREADS: u64 = 3;

    async fn migrated_postgres()
    -> Result<(testcontainers::ContainerAsync<TestPostgres>, PgPool), Box<dyn Error>> {
        let container = TestPostgres::default()
            .with_db_name(DATABASE_NAME)
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD)
            .with_cmd(disposable_postgres_server_args())
            .with_mount(disposable_postgres_state_tmpfs())
            .with_tag(POSTGRES_IMAGE_TAG)
            .with_labels(disposable_test_container_labels())
            .start()
            .await?;
        let host = container.get_host().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let database_url =
            format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(local_test_connection_options(&database_url)?)
            .await?;
        migrate(&pool).await?;
        Ok((container, pool))
    }

    fn fixture_repository() -> RepositorySlug {
        RepositorySlug::try_new(FIXTURE_REPOSITORY.to_owned()).expect("fixture repository is valid")
    }

    fn fixture_pull_request() -> PullRequestNumber {
        PullRequestNumber::new(
            std::num::NonZeroU64::new(FIXTURE_PULL_REQUEST).expect("fixture number is positive"),
        )
    }

    fn fixture_observation() -> ConvergenceSweepObservation {
        ConvergenceSweepObservation::new(
            CommitSha::try_new(FIXTURE_HEAD_SHA.to_owned()).expect("fixture SHA is valid"),
            FIXTURE_UNRESOLVED_THREADS,
        )
    }

    /// A target whose credential path does not exist, so `fetch` fails at its
    /// first step and no request is ever issued.
    fn fixture_target() -> SweepTarget {
        let reference = CredentialReference::new("fixture-credential");
        SweepTarget {
            repository: fixture_repository(),
            pull_request: fixture_pull_request(),
            credentials: FileCredentialAccess::new_bounded(
                std::path::PathBuf::from("/nonexistent/convergence-sweep-fixture-credential"),
                reference.clone(),
                MAX_CREDENTIAL_BYTES,
            ),
            credential_reference: reference,
        }
    }

    /// Builds the runtime over a live pool. The returned work source is held by
    /// the caller so the nudge channel stays open for the runtime's lifetime.
    fn fixture_runtime(
        pool: &PgPool,
        cool_off: Duration,
    ) -> Result<
        (
            ConvergenceSweepRuntime,
            InProcessEligibilityWorkSource<PostgresEligibilitySweep>,
        ),
        Box<dyn Error>,
    > {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let models = crate::configuration::checked_in_example_configuration()?;
        let credential_pin = models.session_credential_pin();
        let (eligibility_nudge, work_source) =
            InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
        let runtime = ConvergenceSweepRuntime {
            client: Client::builder().build()?,
            targets: vec![fixture_target()].into_boxed_slice(),
            interval: Duration::from_secs(60),
            cool_off,
            template: signalbox_domain::SessionTemplateName::try_new(FIXTURE_TEMPLATE.to_owned())?,
            templates: SessionTemplateConfiguration::default(),
            models,
            commissioned: PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin),
            state: PostgresConvergenceSweepStore::new(pool.clone()),
            eligibility_nudge,
        };
        Ok((runtime, work_source))
    }

    async fn recorded_events(pool: &PgPool) -> Result<i64, Box<dyn Error>> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM convergence_sweep_event
              WHERE repository = $1 AND pull_request_number = $2",
        )
        .bind(FIXTURE_REPOSITORY)
        .bind(rust_decimal::Decimal::from(FIXTURE_PULL_REQUEST))
        .fetch_one(pool)
        .await?)
    }

    async fn target_state(pool: &PgPool) -> Result<(String, i16), Box<dyn Error>> {
        Ok(sqlx::query_as(
            "SELECT state_kind, consecutive_failures FROM convergence_sweep_target
              WHERE repository = $1 AND pull_request_number = $2",
        )
        .bind(FIXTURE_REPOSITORY)
        .bind(rust_decimal::Decimal::from(FIXTURE_PULL_REQUEST))
        .fetch_one(pool)
        .await?)
    }

    /// Records one facts-fetch failure against the fixture target and returns
    /// the disposition the store chose for it.
    ///
    /// Driving a target to its parked state needs several identical
    /// transitions; naming the transition keeps the test bodies straight-line,
    /// so a disposition that comes back wrong is reported at the call site of
    /// the attempt that produced it rather than at one shared loop.
    async fn record_facts_fetch_failure(
        runtime: &ConvergenceSweepRuntime,
        target: &SweepTarget,
    ) -> Result<ConvergenceSweepFailureDisposition, Box<dyn Error>> {
        Ok(runtime
            .state
            .record_failure(
                uuid::Uuid::now_v7(),
                &target.repository,
                target.pull_request,
                Some(&fixture_observation()),
                ConvergenceSweepFailureKind::FactsFetch,
                ConvergenceSweepRetryPolicy {
                    backoff_base: RETRY_BACKOFF_BASE,
                    backoff_cap: RETRY_BACKOFF_CAP,
                },
            )
            .await?)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_census_failure_schedules_a_retry_for_the_target() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let (runtime, _work_source) = fixture_runtime(&pool, Duration::from_secs(60))?;
        let target = fixture_target();

        runtime
            .reconcile_target(&target, Instant::now() + Duration::from_secs(30))
            .await;

        let failure: String = sqlx::query_scalar(
            "SELECT failure_kind FROM convergence_sweep_event
              WHERE repository = $1 AND pull_request_number = $2
                AND failure_kind IS NOT NULL",
        )
        .bind(FIXTURE_REPOSITORY)
        .bind(rust_decimal::Decimal::from(FIXTURE_PULL_REQUEST))
        .fetch_one(&pool)
        .await?;
        assert_eq!(failure, "facts_fetch");
        assert_eq!(target_state(&pool).await?, (String::from("retry_wait"), 1));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_target_inside_its_retry_backoff_is_left_alone() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let (runtime, _work_source) = fixture_runtime(&pool, Duration::from_secs(60))?;
        let target = fixture_target();
        runtime
            .state
            .record_failure(
                uuid::Uuid::now_v7(),
                &target.repository,
                target.pull_request,
                Some(&fixture_observation()),
                ConvergenceSweepFailureKind::FactsFetch,
                ConvergenceSweepRetryPolicy {
                    backoff_base: RETRY_BACKOFF_BASE,
                    backoff_cap: RETRY_BACKOFF_CAP,
                },
            )
            .await?;
        let before = recorded_events(&pool).await?;

        runtime
            .reconcile_target(&target, Instant::now() + Duration::from_secs(30))
            .await;

        // The backoff has not elapsed, so the gate returns before the census and
        // the failure lineage is untouched.
        assert_eq!(recorded_events(&pool).await?, before);
        assert_eq!(target_state(&pool).await?, (String::from("retry_wait"), 1));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_parked_target_is_left_alone() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let (runtime, _work_source) = fixture_runtime(&pool, Duration::from_secs(60))?;
        let target = fixture_target();
        let first = record_facts_fetch_failure(&runtime, &target).await?;
        let second = record_facts_fetch_failure(&runtime, &target).await?;
        let third = record_facts_fetch_failure(&runtime, &target).await?;
        let fourth = record_facts_fetch_failure(&runtime, &target).await?;
        let fifth = record_facts_fetch_failure(&runtime, &target).await?;
        assert_eq!(first, ConvergenceSweepFailureDisposition::RetryScheduled);
        assert_eq!(second, ConvergenceSweepFailureDisposition::RetryScheduled);
        assert_eq!(third, ConvergenceSweepFailureDisposition::RetryScheduled);
        assert_eq!(fourth, ConvergenceSweepFailureDisposition::RetryScheduled);
        assert_eq!(fifth, ConvergenceSweepFailureDisposition::Parked);
        let before = recorded_events(&pool).await?;
        assert_eq!(target_state(&pool).await?.0, "parked");

        runtime
            .reconcile_target(&target, Instant::now() + Duration::from_secs(30))
            .await;

        // A parked target waits for an operator, never for another census.
        assert_eq!(recorded_events(&pool).await?, before);
        assert_eq!(target_state(&pool).await?.0, "parked");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn a_committed_dispatch_is_projected_before_any_census() -> Result<(), Box<dyn Error>> {
        let (_container, pool) = migrated_postgres().await?;
        let (runtime, _work_source) = fixture_runtime(&pool, Duration::from_secs(60))?;
        let target = fixture_target();
        let observation = fixture_observation();
        let command = DurableCommandId::from_uuid(uuid::Uuid::from_u128(0x89_204));
        runtime
            .state
            .begin_commission(
                &target.repository,
                target.pull_request,
                &observation,
                [17; 32],
                command,
            )
            .await?;
        let request = CommissionDispatchRequest::try_new(
            command,
            signalbox_domain::SessionTemplateName::try_new(FIXTURE_TEMPLATE.to_owned())?,
            CommissionedDispatchFence::PullRequest {
                repository: target.repository.clone(),
                pull_request: target.pull_request,
                head_sha: CommitSha::try_new(FIXTURE_HEAD_SHA.to_owned())?,
                head_repository: RepositorySlug::try_new(FIXTURE_HEAD_REPOSITORY.to_owned())?,
                head_branch: BranchName::try_new(FIXTURE_HEAD_BRANCH.to_owned())?,
                base_branch: BranchName::try_new(FIXTURE_BASE_BRANCH.to_owned())?,
            },
            GoalStatement::try_new("Converge the pull request.".to_owned())?,
            UserContent::try_text("Respond to the review.".to_owned())
                .expect("fixture content is admitted"),
        )?;
        let prepared = request.prepare(
            &mut UuidV7CommissionedDispatchIdGenerator,
            signalbox_domain::SessionTemplateProvenance::new(
                signalbox_domain::SessionTemplateName::try_new(FIXTURE_TEMPLATE.to_owned())?,
                signalbox_domain::SessionTemplateContentDigest::from_bytes([7; 32]),
            ),
            signalbox_domain::SessionConfigurationDefaults::complete(
                signalbox_domain::ModelSelectionRequest::Direct(
                    signalbox_domain::DirectModelSelection::from_uuid(uuid::Uuid::from_u128(
                        0x89_200,
                    )),
                ),
                signalbox_domain::DangerousToolAutoApproval::Disabled,
                Some(signalbox_domain::SessionSystemPrompt::try_new(
                    "Respond to review findings.".to_owned(),
                )?),
            ),
        )?;
        let outcome = runtime.commissioned.commission(prepared, |_| None).await?;
        let CommissionDispatchOutcome::Dispatched { dispatch, session } = outcome else {
            panic!("a fresh fixture must dispatch: {outcome:?}");
        };

        runtime
            .reconcile_target(&target, Instant::now() + Duration::from_secs(30))
            .await;

        // The projection repair runs before the census and returns, so the
        // missing credential never produces a failure for this tick.
        let projected: (uuid::Uuid, uuid::Uuid) = sqlx::query_as(
            "SELECT last_dispatch_id, last_session_id FROM convergence_sweep_target
              WHERE repository = $1 AND pull_request_number = $2",
        )
        .bind(FIXTURE_REPOSITORY)
        .bind(rust_decimal::Decimal::from(FIXTURE_PULL_REQUEST))
        .fetch_one(&pool)
        .await?;
        assert_eq!(projected, (dispatch.into_uuid(), session.into_uuid()));
        assert_eq!(target_state(&pool).await?, (String::from("observed"), 0));
        Ok(())
    }
}

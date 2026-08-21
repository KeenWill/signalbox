//! Periodic convergence reconciliation for explicitly selected watched pull requests.

use std::{
    error::Error,
    fmt,
    time::{Duration, SystemTime},
};

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
        PostgresConvergenceSweepStore,
    },
};
use sqlx::PgPool;
use tokio::{select, sync::watch, time::sleep};

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
// numeric-bound: ceiling - prevents slow targets from serially delaying the complete target set
const MAX_CONCURRENT_TARGETS: usize = 8;
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
      state isDraft baseRefName headRefName headRefOid mergeable
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
  $namespace: String!, $name: String!, $number: Int!, $after: String!
) {
  repository(owner: $namespace, name: $name) {
    pullRequest(number: $number) {
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
  $namespace: String!, $name: String!, $number: Int!, $after: String!
) {
  repository(owner: $namespace, name: $name) {
    pullRequest(number: $number) {
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
            .no_proxy()
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
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        if *shutdown.borrow() {
            return;
        }
        loop {
            if !self.sweep_once(&mut shutdown).await {
                return;
            }
            select! {
                _ = sleep(self.interval) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
            }
        }
    }

    async fn sweep_once(&self, shutdown: &mut watch::Receiver<bool>) -> bool {
        let census = stream::iter(&self.targets)
            .for_each_concurrent(Some(MAX_CONCURRENT_TARGETS), |target| {
                self.reconcile_target(target)
            });
        tokio::pin!(census);
        select! {
            () = &mut census => true,
            changed = shutdown.changed() => {
                changed.is_ok() && !*shutdown.borrow()
            }
        }
    }

    async fn reconcile_target(&self, target: &SweepTarget) {
        let loaded = match self
            .state
            .load_target(&target.repository, target.pull_request)
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
        if loaded
            .as_ref()
            .is_some_and(|state| state.is_parked() || !state.retry_ready())
        {
            return;
        }
        let fetched = match self.fetch(target).await {
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
            let dispatch_observation = loaded.as_ref().and_then(|state| {
                state
                    .last_dispatch_observation()
                    .or_else(|| state.pending_observation())
                    .or_else(|| state.last_observation())
            });
            let unchanged = dispatch_observation == Some(&observation);
            let cool_off_elapsed = SystemTime::now()
                .duration_since(dispatch.dispatched_at())
                .is_ok_and(|elapsed| elapsed >= self.cool_off);
            if unchanged && !dispatch.has_model_activity() && cool_off_elapsed {
                self.record_failure(
                    target,
                    Some(&observation),
                    ConvergenceSweepFailureKind::NoModelActivity,
                    CensusError::Shape,
                )
                .await;
                return;
            }
            if dispatch.is_live() {
                self.record_decision(target, &observation, ConvergenceSweepDecision::LiveSession)
                    .await;
                return;
            }
            if !cool_off_elapsed {
                self.record_decision(target, &observation, ConvergenceSweepDecision::CoolingOff)
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
            .commission(prepared, |alias| self.models.resolve_alias(alias))
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
        let prior = self
            .state
            .load_target(&target.repository, target.pull_request)
            .await
            .ok()
            .flatten();
        let attempt = prior
            .as_ref()
            .filter(|state| state.failure_kind() == Some(failure))
            .map_or(0, |state| u32::from(state.consecutive_failures()));
        let delay = retry_delay(attempt);
        match self
            .state
            .record_failure(
                uuid::Uuid::now_v7(),
                &target.repository,
                target.pull_request,
                observation,
                failure,
                delay.as_secs(),
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
        let checked_head_sha = checked_head_at(pull)?;
        let mut unresolved = unresolved_threads(
            pull.pointer("/reviewThreads/nodes")
                .and_then(Value::as_array)
                .ok_or(CensusError::Shape)?,
        )?;
        let initial_checks = pull
            .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let mut checks = decode_checks(initial_checks)?;
        let mut thread_page = page_info(pull.pointer("/reviewThreads/pageInfo"))?;
        let mut check_page =
            match pull.pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/pageInfo") {
                Some(value) => page_info(Some(value))?,
                None => PageInfo::done(),
            };
        let mut pages = 1usize;
        while thread_page.has_next {
            pages += 1;
            if pages > MAX_CONNECTION_PAGES {
                return Err(CensusError::Pagination);
            }
            let mut next = variables.clone();
            next["after"] = Value::String(thread_page.cursor.ok_or(CensusError::Shape)?);
            let page = self.graphql(THREADS_QUERY, next, &authorization).await?;
            let connection = page
                .pointer("/data/repository/pullRequest/reviewThreads")
                .ok_or(CensusError::Shape)?;
            unresolved += unresolved_threads(
                connection
                    .get("nodes")
                    .and_then(Value::as_array)
                    .ok_or(CensusError::Shape)?,
            )?;
            thread_page = page_info(connection.get("pageInfo"))?;
        }
        while check_page.has_next {
            pages += 1;
            if pages > MAX_CONNECTION_PAGES {
                return Err(CensusError::Pagination);
            }
            let mut next = variables.clone();
            next["after"] = Value::String(check_page.cursor.ok_or(CensusError::Shape)?);
            let page = self.graphql(CHECKS_QUERY, next, &authorization).await?;
            let connection = checks_page(&page, &head_sha)?;
            checks.extend(decode_checks(
                connection
                    .get("nodes")
                    .and_then(Value::as_array)
                    .ok_or(CensusError::Shape)?,
            )?);
            check_page = page_info(connection.get("pageInfo"))?;
        }
        let mergeable_state = match pull.get("mergeable").and_then(Value::as_str) {
            Some("MERGEABLE") => MergeableState::Mergeable,
            Some("CONFLICTING") => MergeableState::Conflicting,
            Some("UNKNOWN") => MergeableState::Unknown,
            _ => return Err(CensusError::Shape),
        };
        Ok(FetchedPullRequest {
            base_branch: branch_at(pull, "baseRefName")?,
            head_branch: branch_at(pull, "headRefName")?,
            head_repository: RepositorySlug::try_new(
                pull.pointer("/headRepository/name_with_owner")
                    .and_then(Value::as_str)
                    .ok_or(CensusError::Shape)?
                    .to_lowercase(),
            )
            .map_err(|_| CensusError::Shape)?,
            facts: PullRequestConvergenceFacts::new(
                head_sha,
                checked_head_sha,
                pull.get("isDraft")
                    .and_then(Value::as_bool)
                    .ok_or(CensusError::Shape)?,
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
        let mut response = loop {
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
                Ok(response) => break response,
                Err(_) if attempt < MAX_REQUEST_ATTEMPTS => {
                    sleep(REQUEST_RETRY_DELAY).await;
                }
                Err(_) => return Err(CensusError::Request),
            }
        };
        if response.status() != StatusCode::OK {
            return Err(CensusError::Response);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| CensusError::Response)? {
            let next = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(CensusError::Response)?;
            if next > MAX_RESPONSE_BYTES {
                return Err(CensusError::Response);
            }
            bytes.extend_from_slice(&chunk);
        }
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

fn unresolved_threads(values: &[Value]) -> Result<u64, CensusError> {
    values.iter().try_fold(0u64, |count, value| {
        let resolved = value
            .get("isResolved")
            .and_then(Value::as_bool)
            .ok_or(CensusError::Shape)?;
        Ok(count + u64::from(!resolved))
    })
}

fn checks_page<'a>(page: &'a Value, expected_head: &CommitSha) -> Result<&'a Value, CensusError> {
    let commit = page
        .pointer("/data/repository/pullRequest/commits/nodes/0/commit")
        .ok_or(CensusError::Shape)?;
    if commit_at(commit, "oid")? != *expected_head {
        return Err(CensusError::Shape);
    }
    commit
        .pointer("/statusCheckRollup/contexts")
        .ok_or(CensusError::Shape)
}

fn decode_checks(values: &[Value]) -> Result<Vec<PullRequestCheck>, CensusError> {
    values
        .iter()
        .map(
            |value| match value.get("__typename").and_then(Value::as_str) {
                Some("CheckRun") => Ok(PullRequestCheck::new(
                    value
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or(CensusError::Shape)?
                        .to_owned(),
                    PullRequestCheckState::CheckRun {
                        completed: value.get("status").and_then(Value::as_str) == Some("COMPLETED"),
                        conclusion: value
                            .get("conclusion")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    },
                )),
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

fn checked_head_at(pull: &Value) -> Result<Option<CommitSha>, CensusError> {
    pull.pointer("/commits/nodes/0/commit/statusCheckRollup")
        .filter(|rollup| !rollup.is_null())
        .and_then(|_| pull.pointer("/commits/nodes/0/commit/oid"))
        .and_then(Value::as_str)
        .map(|value| CommitSha::try_new(value.to_owned()))
        .transpose()
        .map_err(|_| CensusError::Shape)
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
        "draft": fetched.facts.draft(),
        "unresolved_review_threads": fetched.facts.unresolved_review_threads(),
        "mergeable_state": format!("{:?}", fetched.facts.mergeable_state()).to_lowercase(),
        "gating_checks": gating_checks,
        "non_gating_checks": non_gating_checks,
        "blockers": blockers,
    }))
    .map_err(|_| ())
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

fn retry_delay(attempt: u32) -> Duration {
    RETRY_BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(attempt.min(4)))
        .min(RETRY_BACKOFF_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(value: char) -> CommitSha {
        CommitSha::try_new(value.to_string().repeat(40)).expect("fixture SHA is valid")
    }

    #[test]
    fn retry_delay_is_bounded() {
        assert_eq!(retry_delay(0), Duration::from_secs(60));
        assert_eq!(retry_delay(4), RETRY_BACKOFF_CAP);
        assert_eq!(retry_delay(u32::MAX), RETRY_BACKOFF_CAP);
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
            PullRequestCheckState::CheckRun {
                completed: true,
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
    fn absent_status_rollup_does_not_mark_checks_current() {
        let pull = json!({
            "commits": {
                "nodes": [{"commit": {"oid": sha('a').as_str(), "statusCheckRollup": null}}]
            }
        });

        assert_eq!(checked_head_at(&pull), Ok(None));
    }

    #[test]
    fn a_second_checks_page_decodes_only_for_the_observed_head() {
        let expected_head = sha('a');
        let page = json!({
            "data": {
                "repository": {
                    "pullRequest": {
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

        let connection = checks_page(&page, &expected_head).expect("head-matched page decodes");
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
        let page = json!({
            "data": {
                "repository": {
                    "pullRequest": {
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

        assert_eq!(checks_page(&page, &sha('a')), Err(CensusError::Shape));
    }
}

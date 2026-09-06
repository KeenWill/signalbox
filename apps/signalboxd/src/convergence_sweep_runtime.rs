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
    InProcessEligibilityNudge, UuidV7CommissionedDispatchIdGenerator, UuidV7SubmitInputIdGenerator,
};
use signalbox_convergence::{
    ConvergencePolicy, Evaluation, Verdict,
    fetch::{GitHubRequest, RequestFuture},
};
use signalbox_domain::{
    BranchName, CommitSha, DurableCommandId, GoalStatement, PullRequestNumber, RepositorySlug,
    UserContent,
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
// numeric-bound: guard - prevents a provider response from exhausting process memory
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
// numeric-bound: guard - prevents credential material from reaching the provider
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;

/// Deployment policy for convergence census work and retry scheduling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConvergenceSweepNumericBounds {
    request_timeout: Option<Duration>,
    connection_pages: Option<usize>,
    concurrent_targets: Option<usize>,
    request_attempts: Option<usize>,
    request_retry_delay: Option<Duration>,
    retry_backoff_base: Option<Duration>,
    retry_backoff_cap: Option<Duration>,
}

impl ConvergenceSweepNumericBounds {
    /// Binds every convergence limit to the validated daemon configuration.
    pub const fn new(
        request_timeout: Option<Duration>,
        connection_pages: Option<usize>,
        concurrent_targets: Option<usize>,
        request_attempts: Option<usize>,
        request_retry_delay: Option<Duration>,
        retry_backoff_base: Option<Duration>,
        retry_backoff_cap: Option<Duration>,
    ) -> Self {
        Self {
            request_timeout,
            connection_pages,
            concurrent_targets,
            request_attempts,
            request_retry_delay,
            retry_backoff_base,
            retry_backoff_cap,
        }
    }
}

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
    numeric_bounds: ConvergenceSweepNumericBounds,
    convergence_policy: Option<ConvergencePolicy>,
    convergence_history: tokio::sync::Mutex<std::collections::BTreeMap<String, Value>>,
}

impl ConvergenceSweepRuntime {
    /// Constructs no runtime when the operator selected no convergence targets.
    pub fn try_new(
        pool: PgPool,
        configuration: &RepositoryWatchConfiguration,
        templates: SessionTemplateConfiguration,
        models: HubModelConfiguration,
        eligibility_nudge: InProcessEligibilityNudge,
        numeric_bounds: ConvergenceSweepNumericBounds,
    ) -> Result<Option<Self>, ConvergenceSweepRuntimeConstructionError> {
        let Some(policy) = configuration.convergence_sweep() else {
            return Ok(None);
        };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut client = Client::builder()
            .tls_backend_rustls()
            .tls_version_min(reqwest::tls::Version::TLS_1_2)
            .tls_danger_accept_invalid_certs(false)
            .tls_danger_accept_invalid_hostnames(false)
            .redirect(Policy::none())
            .retry(reqwest::retry::never());
        if let Some(request_timeout) = numeric_bounds.request_timeout {
            client = client.timeout(request_timeout);
        }
        let client = client
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
                        credentials: FileCredentialAccess::new(
                            repository.credential_file().to_path_buf(),
                            repository.credential_reference(),
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
            numeric_bounds,
            convergence_policy: models.convergence().cloned(),
            convergence_history: Default::default(),
        }))
    }

    /// Runs complete censuses until shutdown; one target failure never halts siblings.
    pub async fn run(self, shutdown: watch::Receiver<bool>) {
        let runtime = &self;
        // Configuration bounds this target set to 256 entries. When the operator
        // configures no concurrency ceiling, giving each enrolled target one permit
        // preserves its absolute polling deadline while retaining an explicit,
        // configuration-bounded admission gate; a configured ceiling narrows that
        // gate so slow targets cannot occupy the whole census at once.
        let active_targets = Arc::new(Semaphore::new(
            self.numeric_bounds
                .concurrent_targets
                .unwrap_or(self.targets.len()),
        ));
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
                                Ok(restored) => {
                                    if let Some(session) = restored {
                                        let _ = runtime.eligibility_nudge.nudge(session);
                                    }
                                    reenrolled = true;
                                }
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
            fetched.head_sha.clone(),
            fetched.evaluation.unresolved_review_threads as u64,
        );
        let convergence = &fetched.evaluation.verdict;
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
        let context = match commission_content(target, &fetched, convergence) {
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
            .commission_after_cool_off(
                prepared,
                &mut UuidV7SubmitInputIdGenerator,
                self.cool_off,
                |alias| self.models.resolve_alias(alias),
            )
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
                    backoff_base: self.numeric_bounds.retry_backoff_base,
                    backoff_cap: self.numeric_bounds.retry_backoff_cap,
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
        let mut policy = self.convergence_policy.clone().ok_or(CensusError::Shape)?;
        if let Some(ceiling) = self.numeric_bounds.connection_pages {
            policy.page_limit = policy.page_limit.min(ceiling);
        }
        let key = format!(
            "{}#{}",
            target.repository.as_str(),
            target.pull_request.get()
        );
        let previous = self
            .convergence_history
            .lock()
            .await
            .get(&key)
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut send = |request| -> RequestFuture<'_> {
            let authorization = &authorization;
            Box::pin(async move {
                let result = match request {
                    GitHubRequest::GraphQl { query, variables } => {
                        self.graphql(&query, variables, authorization).await
                    }
                    GitHubRequest::Rest { path } => self.rest(&path, authorization).await,
                };
                result.map_err(|_| {
                    signalbox_convergence::Error::Evidence(
                        "convergence provider request failed".into(),
                    )
                })
            })
        };
        let recording = signalbox_convergence::fetch::record_with(
            &mut send,
            previous,
            target.repository.as_str(),
            target.pull_request.get(),
            &policy,
        )
        .await
        .map_err(|_| CensusError::Response)?;
        let snapshot = recording
            .snapshot(&policy)
            .map_err(|_| CensusError::Shape)?;
        if snapshot.initial["headRepository"] != snapshot.current["headRepository"] {
            return Err(CensusError::State);
        }
        let evaluation =
            signalbox_convergence::evaluate(&snapshot, &policy).map_err(|_| CensusError::State)?;
        let node = &snapshot.current;
        let head_repository = RepositorySlug::try_new(
            node["headRepository"]["nameWithOwner"]
                .as_str()
                .ok_or(CensusError::Shape)?
                .to_lowercase(),
        )
        .map_err(|_| CensusError::Shape)?;
        self.convergence_history
            .lock()
            .await
            .insert(key, evaluation.state.clone());
        Ok(FetchedPullRequest {
            head_sha: commit_at(node, "headRefOid")?,
            base_branch: branch_at(node, "baseRefName")?,
            head_branch: branch_at(node, "headRefName")?,
            head_repository,
            evaluation,
        })
    }

    async fn rest(&self, path: &str, authorization: &HeaderValue) -> Result<Value, CensusError> {
        let mut response = self
            .client
            .get(format!("https://api.github.com/{path}"))
            .header(AUTHORIZATION, authorization.clone())
            .header(ACCEPT, "application/vnd.github+json")
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await
            .map_err(|_| CensusError::Request)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Value::Null);
        }
        if response.status() != StatusCode::OK {
            return Err(CensusError::Response);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| CensusError::Response)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(CensusError::Response);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| CensusError::Decode)
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
        if self.numeric_bounds.request_attempts == Some(0) {
            return Err(CensusError::Request);
        }
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
                    if self
                        .numeric_bounds
                        .request_attempts
                        .is_none_or(|limit| attempt < limit)
                        && (response.status().is_server_error()
                            || response.status() == StatusCode::TOO_MANY_REQUESTS) =>
                {
                    sleep_for_policy(self.numeric_bounds.request_retry_delay).await;
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
                            Err(_)
                                if self
                                    .numeric_bounds
                                    .request_attempts
                                    .is_none_or(|limit| attempt < limit) =>
                            {
                                sleep_for_policy(self.numeric_bounds.request_retry_delay).await;
                                continue 'attempts;
                            }
                            Err(_) => return Err(CensusError::Response),
                        }
                    }
                }
                Err(_)
                    if self
                        .numeric_bounds
                        .request_attempts
                        .is_none_or(|limit| attempt < limit) =>
                {
                    sleep_for_policy(self.numeric_bounds.request_retry_delay).await;
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
    head_sha: CommitSha,
    evaluation: Evaluation,
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
            head_sha: fetched.head_sha.clone(),
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
    convergence: &Verdict,
) -> Result<String, ()> {
    serde_json::to_string(&json!({
        "kind":"pull_request_convergence_reconciliation",
        "repository":target.repository.as_str(),
        "pull_request":target.pull_request.get(),
        "head_sha":fetched.head_sha.as_str(),
        "checked_head_sha":fetched.evaluation.facts.checked_head_oid,
        "head_repository":fetched.head_repository.as_str(),
        "base_branch":fetched.base_branch.as_str(),
        "head_branch":fetched.head_branch.as_str(),
        "draft":fetched.evaluation.facts.is_draft,
        "unresolved_review_threads":fetched.evaluation.unresolved_review_threads,
        "mergeable_state":fetched.evaluation.facts.mergeable.to_lowercase(),
        "gating_checks":fetched.evaluation.gating_checks,
        "non_gating_checks":fetched.evaluation.non_gating_checks,
        "blockers":convergence.reasons().iter().map(|reason|reason.reference_reason()).collect::<Vec<_>>(),
    })).map_err(|_|())
}

async fn sleep_for_policy(delay: Option<Duration>) {
    match delay {
        Some(delay) => sleep(delay).await,
        None => std::future::pending().await,
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

    fn example_numeric_bounds() -> ConvergenceSweepNumericBounds {
        let configured = crate::configuration::checked_in_example_configuration()
            .expect("checked-in example parses");
        let bounds = configured.numeric_bounds();
        ConvergenceSweepNumericBounds::new(
            bounds
                .duration("convergence_sweep_request_timeout")
                .flatten(),
            bounds
                .integer("max_convergence_sweep_connection_pages")
                .flatten()
                .and_then(|value| usize::try_from(value).ok()),
            bounds
                .integer("max_concurrent_convergence_sweep_targets")
                .flatten()
                .and_then(|value| usize::try_from(value).ok()),
            bounds
                .integer("max_convergence_sweep_request_attempts")
                .flatten()
                .and_then(|value| usize::try_from(value).ok()),
            bounds
                .duration("convergence_sweep_request_retry_delay")
                .flatten(),
            bounds
                .duration("convergence_sweep_retry_backoff_base")
                .flatten(),
            bounds
                .duration("convergence_sweep_retry_backoff_cap")
                .flatten(),
        )
    }

    // The lineage arithmetic itself now lives in the convergence-sweep store, which
    // grows and caps each retry from this policy. What stays provable here is that
    // the configured, optional bounds reach that store intact and describe a usable
    // lineage: a first retry no later than the ceiling it saturates against.
    #[test]
    fn configured_retry_policy_carries_the_example_backoff_bounds() {
        let bounds = example_numeric_bounds();
        let policy = ConvergenceSweepRetryPolicy {
            backoff_base: bounds.retry_backoff_base,
            backoff_cap: bounds.retry_backoff_cap,
        };

        assert_eq!(policy.backoff_base, Some(Duration::from_secs(60)));
        assert_eq!(policy.backoff_cap, Some(Duration::from_secs(15 * 60)));
        assert!(
            policy
                .backoff_base
                .zip(policy.backoff_cap)
                .is_some_and(|(base, cap)| base <= cap),
            "the checked-in example schedules a first retry no later than its own cap"
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
            .with_mount(disposable_postgres_state_tmpfs(None))
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
            credentials: FileCredentialAccess::new(
                std::path::PathBuf::from("/nonexistent/convergence-sweep-fixture-credential"),
                reference.clone(),
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
            convergence_policy: models.convergence().cloned(),
            convergence_history: Default::default(),
            models,
            commissioned: PostgresCommissionedDispatchStore::new(pool.clone(), credential_pin),
            state: PostgresConvergenceSweepStore::new(pool.clone()),
            eligibility_nudge,
            numeric_bounds: example_numeric_bounds(),
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
                    backoff_base: runtime.numeric_bounds.retry_backoff_base,
                    backoff_cap: runtime.numeric_bounds.retry_backoff_cap,
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
                    backoff_base: runtime.numeric_bounds.retry_backoff_base,
                    backoff_cap: runtime.numeric_bounds.retry_backoff_cap,
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
        let outcome = runtime
            .commissioned
            .commission(prepared, &mut UuidV7SubmitInputIdGenerator, |_| None)
            .await?;
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

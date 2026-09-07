//! Read-only observation probe; authentication comes from the gh CLI.
use serde_json::Value;
use signalbox_module_repo_watch_v2::{
    github::GitHubClient,
    provider::{GitHubObservationRead, ObservationError, fetch_observation},
};
use signalbox_ownership_seam::{RepoWatchAuthorLogin, RepositorySlug};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

struct Probe {
    client: GitHubClient,
    rest: AtomicUsize,
    graphql: AtomicUsize,
}
impl GitHubObservationRead for Probe {
    async fn page(&self, path: &str) -> Result<(Value, bool), ObservationError> {
        let count = self.rest.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("REST {count} {path}");
        let result = self.client.page(path).await;
        if let Err(error) = &result {
            eprintln!("request failed: {error}");
        }
        result
    }
    async fn threads(&self, request: Value) -> Result<Value, ObservationError> {
        let count = self.graphql.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!(
            "GraphQL {count} /graphql pull_request={}",
            request["variables"]["number"]
        );
        self.client.threads(request).await
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()?;
    if !token.status.success() {
        return Err("gh auth token failed".into());
    }
    let client = GitHubClient::try_new(
        "signalbox-observation-probe",
        std::str::from_utf8(&token.stdout)?.trim(),
    )?;
    let probe = Probe {
        client,
        rest: AtomicUsize::new(0),
        graphql: AtomicUsize::new(0),
    };
    let mut arguments = std::env::args().skip(1);
    let repository = RepositorySlug::try_new(
        arguments
            .next()
            .unwrap_or_else(|| String::from("KeenWill/signalbox")),
    )?;
    let reviewers = arguments
        .map(RepoWatchAuthorLogin::try_new)
        .collect::<Result<Vec<_>, _>>()?;
    eprintln!(
        "repository={} signal_reviewers={}",
        repository.as_str(),
        reviewers.len()
    );
    let start = Instant::now();
    // The observation must finish within the repository's five-minute poll interval.
    let outcome = tokio::time::timeout(
        Duration::from_secs(300),
        fetch_observation(&probe, &repository, &reviewers, None, &[]),
    )
    .await;
    eprintln!(
        "requests={} rest={} graphql={} elapsed={:.3}s",
        probe.rest.load(Ordering::Relaxed) + probe.graphql.load(Ordering::Relaxed),
        probe.rest.load(Ordering::Relaxed),
        probe.graphql.load(Ordering::Relaxed),
        start.elapsed().as_secs_f64()
    );
    match outcome {
        Ok(Ok(observed)) => {
            let state = observed.observation.state();
            eprintln!(
                "outcome=ok pulls={} branches={} workflows={}",
                state.pull_requests().len(),
                state.branch_heads().len(),
                state.workflow_runs().len()
            );
            Ok(())
        }
        Ok(Err(error)) => {
            eprintln!("outcome=error {error:?}");
            Err(error.into())
        }
        Err(error) => {
            eprintln!("outcome=timeout {error}");
            Err(error.into())
        }
    }
}

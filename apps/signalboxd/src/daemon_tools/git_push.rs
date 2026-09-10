use std::{collections::BTreeMap, ffi::OsString, time::Duration};

use signalbox_tools_exec::{
    CaptureCompleteness, ProcessEnvironment, ProcessOutcome, ProcessRequest, ProcessRunResult,
    ProcessRunner, ProcessStatusProtocol,
};
use signalbox_tools_git::{
    GitPushReceipt, GitPushRequest, GitPushTransport, GitPushTransportFailure,
};

use crate::repo_watch_credentials::{RepositoryWatchClientLoadError, RepositoryWatchClientLoader};

// The exec family's command duration also bounds push credential preparation.
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) struct ProcessGitPushTransport<Runner> {
    pub(super) runner: Runner,
    pub(super) credentials: RepositoryWatchClientLoader,
}

impl<Runner: ProcessRunner> GitPushTransport for ProcessGitPushTransport<Runner> {
    async fn push(
        &mut self,
        request: GitPushRequest,
    ) -> Result<GitPushReceipt, GitPushTransportFailure> {
        let authentication = self
            .credentials
            .authenticated_push_url(request.remote().url(), PUSH_TIMEOUT)
            .await
            .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?;
        if !request.repository_root().is_dir() {
            return Err(GitPushTransportFailure::PreDispatchInfrastructure);
        }
        let mut environment: BTreeMap<OsString, OsString> = [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_CONFIG_COUNT", "6"),
            ("GIT_CONFIG_KEY_1", "credential.helper"),
            ("GIT_CONFIG_VALUE_1", ""),
            ("GIT_CONFIG_KEY_2", "core.hooksPath"),
            ("GIT_CONFIG_VALUE_2", "/dev/null"),
            ("GIT_CONFIG_KEY_3", "http.followRedirects"),
            ("GIT_CONFIG_VALUE_3", "false"),
            ("GIT_CONFIG_KEY_4", "pack.window"),
            ("GIT_CONFIG_VALUE_4", "0"),
            ("GIT_CONFIG_KEY_5", "pack.depth"),
            ("GIT_CONFIG_VALUE_5", "0"),
            ("LC_ALL", "C"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
        environment.insert(
            "GIT_CONFIG_KEY_0".into(),
            format!("url.{}.insteadOf", authentication.url).into(),
        );
        environment.insert("GIT_CONFIG_VALUE_0".into(), request.remote().url().into());
        environment.insert(
            "GIT_DIR".into(),
            request.git_directory().as_os_str().to_owned(),
        );
        environment.insert(
            "GIT_OBJECT_DIRECTORY".into(),
            request.object_directory().as_os_str().to_owned(),
        );
        if let Some(path) = std::env::var_os("PATH") {
            environment.insert("PATH".into(), path);
        }
        let mut push_request = process_request(
            &request,
            &environment,
            &[
                "push",
                "--porcelain",
                "--",
                request.remote().url(),
                &request.refspec(),
            ],
        );
        let push = push_with_refresh(
            &mut self.runner,
            &mut push_request,
            self.credentials.refreshed_push_url(
                request.remote().url(),
                &authentication,
                PUSH_TIMEOUT,
            ),
        )
        .await?;
        classify_push(&push)?;
        let remote_ref = format!("refs/heads/{}", request.branch());
        let confirmation = self
            .run(
                &request,
                &push_request.environment,
                &[
                    "ls-remote",
                    "--refs",
                    "--",
                    request.remote().url(),
                    &remote_ref,
                ],
            )
            .await;
        let expected_ref = format!("{}\t{remote_ref}", request.commit());
        if !matches!(
            confirmation.outcome,
            ProcessOutcome::Exited { code: Some(0) }
        ) || confirmation.stdout.completeness != CaptureCompleteness::Complete
            || !confirmation
                .stdout
                .bytes
                .split(|byte| *byte == b'\n')
                .any(|line| line == expected_ref.as_bytes())
        {
            return Err(GitPushTransportFailure::DispatchUnknown);
        }
        GitPushReceipt::try_new(request.commit())
            .map_err(|_| GitPushTransportFailure::DispatchUnknown)
    }
}

impl<Runner: ProcessRunner> ProcessGitPushTransport<Runner> {
    async fn run(
        &mut self,
        request: &GitPushRequest,
        environment: &BTreeMap<OsString, OsString>,
        arguments: &[&str],
    ) -> ProcessRunResult {
        self.runner
            .run(process_request(request, environment, arguments))
            .await
    }
}

fn process_request(
    request: &GitPushRequest,
    environment: &BTreeMap<OsString, OsString>,
    arguments: &[&str],
) -> ProcessRequest {
    ProcessRequest {
        program: "git".into(),
        arguments: arguments.iter().map(OsString::from).collect(),
        working_directory: request.repository_root().to_owned(),
        timeout: PUSH_TIMEOUT,
        capture_bytes: 64 * 1024,
        environment: environment.clone(),
        environment_inheritance: ProcessEnvironment::Clear,
        status_protocol: ProcessStatusProtocol::Direct,
    }
}

async fn push_with_refresh(
    runner: &mut impl ProcessRunner,
    request: &mut ProcessRequest,
    refresh: impl std::future::Future<Output = Result<Option<String>, RepositoryWatchClientLoadError>>,
) -> Result<ProcessRunResult, GitPushTransportFailure> {
    let first = runner.run(request.clone()).await;
    if authentication_rejected(&first) {
        let refreshed = refresh
            .await
            .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?;
        if let Some(url) = refreshed {
            request.environment.insert(
                "GIT_CONFIG_KEY_0".into(),
                format!("url.{url}.insteadOf").into(),
            );
            return Ok(runner.run(request.clone()).await);
        }
    }
    Ok(first)
}

fn authentication_rejected(result: &ProcessRunResult) -> bool {
    if !matches!(result.outcome, ProcessOutcome::Exited { code: Some(code) } if code != 0) {
        return false;
    }
    let error = String::from_utf8_lossy(&result.stderr.bytes).to_ascii_lowercase();
    error.contains("fatal: authentication failed")
        || error.contains("requested url returned error: 401")
}

fn classify_push(result: &ProcessRunResult) -> Result<(), GitPushTransportFailure> {
    match result.outcome {
        ProcessOutcome::Exited { code: Some(0) } => Ok(()),
        ProcessOutcome::SpawnFailed { .. } => {
            Err(GitPushTransportFailure::PreDispatchInfrastructure)
        }
        ProcessOutcome::Exited { code: Some(_) } => {
            let output = String::from_utf8_lossy(&result.stdout.bytes);
            let error = String::from_utf8_lossy(&result.stderr.bytes).to_ascii_lowercase();
            if authentication_rejected(result)
                || output.lines().any(|line| line.starts_with("!\t"))
                || error.contains("permission denied")
                || error.contains("authentication failed")
                || error.contains("requested url returned error: 403")
                || error.contains("protected branch")
                || error.contains("permission to ") && error.contains(" denied to ")
            {
                Err(GitPushTransportFailure::Rejected)
            } else {
                Err(GitPushTransportFailure::DispatchUnknown)
            }
        }
        _ => Err(GitPushTransportFailure::DispatchUnknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_tools_exec::{ProcessOutput, ProcessSpawnFailure};

    fn result(outcome: ProcessOutcome, stdout: &str, stderr: &str) -> ProcessRunResult {
        ProcessRunResult {
            outcome,
            stdout: ProcessOutput {
                bytes: stdout.as_bytes().to_vec(),
                completeness: CaptureCompleteness::Complete,
            },
            stderr: ProcessOutput {
                bytes: stderr.as_bytes().to_vec(),
                completeness: CaptureCompleteness::Complete,
            },
        }
    }

    #[derive(Clone)]
    struct RecordedRunner {
        responses: std::collections::VecDeque<ProcessRunResult>,
        requests: Vec<ProcessRequest>,
    }

    impl ProcessRunner for RecordedRunner {
        fn sandbox_launcher_program(&self) -> &std::path::Path {
            std::path::Path::new("/unused/launcher")
        }
        fn sandbox_launcher_descriptor(&self) -> Option<i32> {
            None
        }
        async fn bwrap_availability(
            &mut self,
            _: ProcessRequest,
        ) -> signalbox_tools_exec::BwrapAvailability {
            panic!("push does not probe bubblewrap")
        }
        async fn run(&mut self, request: ProcessRequest) -> ProcessRunResult {
            self.requests.push(request);
            self.responses.pop_front().expect("no extra Git invocation")
        }
    }

    fn push_fixture(
        responses: impl IntoIterator<Item = ProcessRunResult>,
    ) -> (RecordedRunner, ProcessRequest) {
        (RecordedRunner { responses: responses.into_iter().collect(), requests: Vec::new() }, ProcessRequest {
            program: "git".into(),
            arguments: ["push", "--porcelain", "--", "https://github.com/fixture/project.git", "HEAD:refs/heads/review"].into_iter().map(OsString::from).collect(),
            working_directory: "/unused/checkout".into(),
            timeout: PUSH_TIMEOUT,
            capture_bytes: 64 * 1024,
            environment: BTreeMap::from([("GIT_CONFIG_KEY_0".into(), "url.https://x-access-token:synthetic-old@github.com/fixture/project.git.insteadOf".into())]),
            environment_inheritance: ProcessEnvironment::Clear,
            status_protocol: ProcessStatusProtocol::Direct,
        })
    }

    fn authentication_failure() -> ProcessRunResult {
        result(
            ProcessOutcome::Exited { code: Some(128) },
            "",
            "fatal: Authentication failed for 'https://github.com/fixture/project.git/'",
        )
    }

    #[tokio::test]
    async fn authentication_rejection_retries_with_the_refreshed_url() {
        let (mut runner, mut request) = push_fixture([
            authentication_failure(),
            result(ProcessOutcome::Exited { code: Some(0) }, "", ""),
        ]);
        let response = push_with_refresh(&mut runner, &mut request, async {
            Ok(Some(
                "https://x-access-token:synthetic-new@github.com/fixture/project.git".to_owned(),
            ))
        })
        .await
        .expect("refresh and retry");
        assert_eq!(classify_push(&response), Ok(()));
        assert_eq!(runner.requests.len(), 2);
        assert_eq!(
            runner.requests[0].environment[std::ffi::OsStr::new("GIT_CONFIG_KEY_0")],
            OsString::from(
                "url.https://x-access-token:synthetic-old@github.com/fixture/project.git.insteadOf"
            )
        );
        assert_eq!(
            request.environment[std::ffi::OsStr::new("GIT_CONFIG_KEY_0")],
            OsString::from(
                "url.https://x-access-token:synthetic-new@github.com/fixture/project.git.insteadOf"
            )
        );
        assert_eq!(runner.requests[1], request);
        assert_eq!(runner.requests[0].arguments, runner.requests[1].arguments);
    }

    #[tokio::test]
    async fn a_second_authentication_rejection_does_not_retry_again() {
        let (mut runner, mut request) =
            push_fixture([authentication_failure(), authentication_failure()]);
        let response = push_with_refresh(&mut runner, &mut request, async {
            Ok(Some(
                "https://x-access-token:synthetic-new@github.com/fixture/project.git".to_owned(),
            ))
        })
        .await
        .expect("second definitive rejection");
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::Rejected)
        );
        assert_eq!(runner.requests.len(), 2);
    }

    #[tokio::test]
    async fn branch_rejection_does_not_refresh_or_retry() {
        let (mut runner, mut request) = push_fixture([result(
            ProcessOutcome::Exited { code: Some(1) },
            "!\tcommit:refs/heads/review\t[rejected] (non-fast-forward)\n",
            "",
        )]);
        let response = push_with_refresh(&mut runner, &mut request, async {
            panic!("branch rejection must not refresh authentication")
        })
        .await
        .expect("definitive branch rejection");
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::Rejected)
        );
        assert_eq!(runner.requests.len(), 1);
    }

    #[tokio::test]
    async fn unknown_push_outcome_does_not_refresh_or_retry() {
        let (mut runner, mut request) = push_fixture([result(
            ProcessOutcome::Exited { code: None },
            "",
            "fatal: Authentication failed",
        )]);
        let response = push_with_refresh(&mut runner, &mut request, async {
            panic!("unknown outcome must not trigger a retry")
        })
        .await
        .expect("unknown outcome retained");
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::DispatchUnknown)
        );
        assert_eq!(runner.requests.len(), 1);
    }

    #[tokio::test]
    async fn token_file_authentication_rejection_does_not_retry() {
        let (mut runner, mut request) = push_fixture([authentication_failure()]);
        let response = push_with_refresh(&mut runner, &mut request, async { Ok(None) })
            .await
            .expect("file rejection retained");
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::Rejected)
        );
        assert_eq!(runner.requests.len(), 1);
    }

    #[tokio::test]
    async fn failed_token_refresh_does_not_dispatch_a_retry() {
        let (mut runner, mut request) = push_fixture([authentication_failure()]);
        let response = push_with_refresh(&mut runner, &mut request, async {
            Err(RepositoryWatchClientLoadError::CredentialUnavailable)
        })
        .await;
        assert_eq!(
            response.err(),
            Some(GitPushTransportFailure::PreDispatchInfrastructure)
        );
        assert_eq!(runner.requests.len(), 1);
    }

    #[test]
    fn remote_rejection_is_definitive() {
        let response = result(
            ProcessOutcome::Exited { code: Some(1) },
            "!\tcommit:refs/heads/review\t[rejected] (non-fast-forward)\n",
            "",
        );
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::Rejected)
        );
    }

    #[test]
    fn permission_denial_is_definitive() {
        let response = result(
            ProcessOutcome::Exited { code: Some(128) },
            "",
            "remote: Permission to example/project.git denied to user.",
        );
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::Rejected)
        );
    }

    #[test]
    fn protected_branch_rejection_is_definitive() {
        let response = result(
            ProcessOutcome::Exited { code: Some(1) },
            "!\tcommit:refs/heads/review\t[remote rejected] (protected branch hook declined)\n",
            "",
        );
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::Rejected)
        );
    }

    #[test]
    fn spawn_failure_is_pre_dispatch() {
        let response = result(
            ProcessOutcome::SpawnFailed {
                reason: ProcessSpawnFailure::NotFound,
            },
            "",
            "",
        );
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::PreDispatchInfrastructure)
        );
    }

    #[test]
    fn killed_process_has_unknown_outcome_even_with_partial_rejection_output() {
        let response = result(
            ProcessOutcome::Exited { code: None },
            "!\tcommit:refs/heads/review\t[rejected]\n",
            "",
        );
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::DispatchUnknown)
        );
    }

    #[test]
    fn lost_connection_has_unknown_outcome() {
        let response = result(
            ProcessOutcome::Exited { code: Some(128) },
            "",
            "fatal: the remote end hung up unexpectedly",
        );
        assert_eq!(
            classify_push(&response),
            Err(GitPushTransportFailure::DispatchUnknown)
        );
    }
}

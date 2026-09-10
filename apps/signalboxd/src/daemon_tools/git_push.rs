use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

use signalbox_tools_exec::{
    CaptureCompleteness, ProcessEnvironment, ProcessOutcome, ProcessRequest, ProcessRunResult,
    ProcessRunner, ProcessStatusProtocol,
};
use signalbox_tools_git::{
    GitPushReceipt, GitPushRequest, GitPushTransport, GitPushTransportFailure,
};

use crate::repo_watch_credentials::{
    RepositoryWatchClientLoadError, RepositoryWatchClientLoader, git_authentication_rejected,
};

// One exec-family command budget covers credentials, push attempts, and confirmation.
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);

const SANDBOX_SSH_AGENT_SOCKET: &str = "/run/signalbox-ssh-agent.sock";

struct SshAccount {
    home: PathBuf,
    passwd: tempfile::NamedTempFile,
}

pub(super) struct ProcessGitPushTransport<Runner> {
    pub(super) runner: Runner,
    pub(super) credentials: RepositoryWatchClientLoader,
    pub(super) credential_file: Option<PathBuf>,
    pub(super) ssh_agent_socket: Option<OsString>,
    pub(super) sandbox: signalbox_tools_exec::SandboxConfiguration,
}

impl<Runner: ProcessRunner> GitPushTransport for ProcessGitPushTransport<Runner> {
    async fn push(
        &mut self,
        request: GitPushRequest,
    ) -> Result<GitPushReceipt, GitPushTransportFailure> {
        let deadline = tokio::time::Instant::now() + PUSH_TIMEOUT;
        let authentication = if request.remote().url().starts_with("https://") {
            Some(
                tokio::time::timeout_at(
                    deadline,
                    self.credentials.authenticated_push_url(
                        request.remote().url(),
                        remaining_push_timeout(deadline)
                            .ok_or(GitPushTransportFailure::PreDispatchInfrastructure)?,
                    ),
                )
                .await
                .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?
                .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?,
            )
        } else {
            None
        };
        if !request.repository_root().is_dir() {
            return Err(GitPushTransportFailure::PreDispatchInfrastructure);
        }
        let mut environment: BTreeMap<OsString, OsString> = [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_CONFIG_COUNT", "9"),
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
            ("GIT_CONFIG_KEY_6", "core.bigFileThreshold"),
            ("GIT_CONFIG_VALUE_6", "1"),
            ("GIT_CONFIG_KEY_7", "core.packedGitWindowSize"),
            ("GIT_CONFIG_VALUE_7", "1m"),
            ("GIT_CONFIG_KEY_8", "core.packedGitLimit"),
            ("GIT_CONFIG_VALUE_8", "8m"),
            ("LC_ALL", "C"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
        let _private_key = if let Some(authentication) = &authentication {
            environment.insert(
                "GIT_CONFIG_KEY_0".into(),
                format!("url.{}.insteadOf", authentication.url).into(),
            );
            environment.insert("GIT_CONFIG_VALUE_0".into(), request.remote().url().into());
            None
        } else {
            environment.insert("GIT_CONFIG_KEY_0".into(), "credential.helper".into());
            environment.insert("GIT_CONFIG_VALUE_0".into(), "".into());
            let private_key = match &self.credential_file {
                Some(path) => Some(snapshot_ssh_key(path).await?),
                None => None,
            };
            let mut command = String::from("ssh -F /dev/null -o BatchMode=yes");
            if let Some(key) = &private_key {
                let path = key
                    .path()
                    .to_str()
                    .ok_or(GitPushTransportFailure::PreDispatchInfrastructure)?;
                command.push_str(" -o IdentitiesOnly=yes -i '");
                command.push_str(&path.replace('\'', "'\\''"));
                command.push('\'');
            } else {
                let socket = self
                    .ssh_agent_socket
                    .as_ref()
                    .ok_or(GitPushTransportFailure::PreDispatchInfrastructure)?;
                environment.insert("SSH_AUTH_SOCK".into(), socket.clone());
                command.push_str(" -o IdentityFile=none");
            }
            environment.insert("GIT_SSH_COMMAND".into(), command.into());
            environment.insert("GIT_SSH_VARIANT".into(), "ssh".into());
            private_key
        };
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
            remaining_push_timeout(deadline)
                .ok_or(GitPushTransportFailure::PreDispatchInfrastructure)?,
        );
        let push = if let Some(authentication) = &authentication {
            push_with_refresh(&mut self.runner, &mut push_request, deadline, |remaining| {
                self.credentials.refreshed_push_url(
                    request.remote().url(),
                    authentication,
                    remaining,
                )
            })
            .await?
        } else {
            self.run(
                &request,
                &environment,
                &[
                    "push",
                    "--porcelain",
                    "--",
                    request.remote().url(),
                    &request.refspec(),
                ],
            )
            .await
        };
        classify_push(&push)?;
        let remote_ref = format!("refs/heads/{}", request.branch());
        let confirmation_arguments = [
            "ls-remote",
            "--refs",
            "--",
            request.remote().url(),
            &remote_ref,
        ];
        let confirmation = if authentication.is_some() {
            self.runner
                .run(process_request(
                    &request,
                    &push_request.environment,
                    &confirmation_arguments,
                    remaining_push_timeout(deadline)
                        .ok_or(GitPushTransportFailure::DispatchUnknown)?,
                ))
                .await
        } else {
            self.run(&request, &environment, &confirmation_arguments)
                .await
        };
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
        if !request.remote().url().starts_with("https://") && self.credential_file.is_none() {
            return self
                .run_agent_sandbox(request, environment, arguments)
                .await
                .unwrap_or_else(|_| ProcessRunResult {
                    outcome: ProcessOutcome::SpawnFailed {
                        reason: signalbox_tools_exec::ProcessSpawnFailure::SandboxSetup,
                    },
                    stdout: signalbox_tools_exec::ProcessOutput {
                        bytes: Vec::new(),
                        completeness: CaptureCompleteness::Complete,
                    },
                    stderr: signalbox_tools_exec::ProcessOutput {
                        bytes: Vec::new(),
                        completeness: CaptureCompleteness::Complete,
                    },
                });
        }
        self.runner
            .run(ProcessRequest {
                program: "git".into(),
                arguments: arguments.iter().map(OsString::from).collect(),
                working_directory: request.repository_root().to_owned(),
                // Uses the exec family's command duration and output capture bounds.
                timeout: Duration::from_secs(300),
                capture_bytes: 64 * 1024,
                environment: environment.clone(),
                environment_inheritance: ProcessEnvironment::Clear,
                status_protocol: ProcessStatusProtocol::Direct,
            })
            .await
    }

    async fn account(
        &mut self,
        request: &GitPushRequest,
    ) -> Result<SshAccount, GitPushTransportFailure> {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        let failure = || GitPushTransportFailure::PreDispatchInfrastructure;
        let mut environment = BTreeMap::new();
        if let Some(path) = std::env::var_os("PATH") {
            environment.insert("PATH".into(), path);
        }
        let result = self
            .runner
            .run(ProcessRequest {
                program: "getent".into(),
                arguments: vec![
                    "passwd".into(),
                    rustix::process::getuid().as_raw().to_string().into(),
                ],
                working_directory: request.repository_root().to_owned(),
                timeout: Duration::from_secs(300),
                capture_bytes: 64 * 1024,
                environment,
                environment_inheritance: ProcessEnvironment::Clear,
                status_protocol: ProcessStatusProtocol::Direct,
            })
            .await;
        if !matches!(result.outcome, ProcessOutcome::Exited { code: Some(0) })
            || result.stdout.completeness != CaptureCompleteness::Complete
        {
            return Err(failure());
        }
        let record = result
            .stdout
            .bytes
            .strip_suffix(b"\n")
            .unwrap_or(&result.stdout.bytes);
        let fields: Vec<_> = record.split(|byte| *byte == b':').collect();
        if fields.len() != 7
            || record.contains(&b'\n')
            || record.contains(&b'\r')
            || fields[2] != rustix::process::getuid().as_raw().to_string().as_bytes()
        {
            return Err(failure());
        }
        let home = PathBuf::from(std::ffi::OsStr::from_bytes(fields[5]));
        if !home.is_absolute() {
            return Err(failure());
        }
        let mut passwd = tempfile::NamedTempFile::new().map_err(|_| failure())?;
        // Retain only the resolved account, without its password or descriptive fields.
        for field in [fields[0], b"x", fields[2], fields[3], b"", fields[5]] {
            passwd.write_all(field).map_err(|_| failure())?;
            passwd.write_all(b":").map_err(|_| failure())?;
        }
        passwd.write_all(b"/bin/sh\n").map_err(|_| failure())?;
        Ok(SshAccount { home, passwd })
    }

    async fn run_agent_sandbox(
        &mut self,
        request: &GitPushRequest,
        environment: &BTreeMap<OsString, OsString>,
        arguments: &[&str],
    ) -> Result<ProcessRunResult, GitPushTransportFailure> {
        use signalbox_tools_exec::{
            ExecArguments, SandboxNetwork, SandboxReadOnlyMount, SandboxedCommandRunner,
        };
        let failure = || GitPushTransportFailure::PreDispatchInfrastructure;
        let socket = self.ssh_agent_socket.clone().ok_or_else(failure)?;
        let account = self.account(request).await?;
        let mut configuration = self.sandbox.clone();
        configuration.network = SandboxNetwork::Host;
        configuration.read_only_mounts.extend([
            SandboxReadOnlyMount {
                source: PathBuf::from(socket),
                destination: PathBuf::from(SANDBOX_SSH_AGENT_SOCKET),
            },
            SandboxReadOnlyMount {
                source: account.passwd.path().to_owned(),
                destination: PathBuf::from("/etc/passwd"),
            },
        ]);
        // OpenSSH resolves the invoking account and the host's known-host trust stores.
        for path in [
            "/etc/group",
            "/etc/ssh/ssh_known_hosts",
            "/etc/ssh/ssh_known_hosts2",
        ] {
            let path = PathBuf::from(path);
            if path.is_file() {
                configuration.read_only_binds.push(path);
            }
        }
        for name in ["known_hosts", "known_hosts2"] {
            let known_hosts = account.home.join(".ssh").join(name);
            if known_hosts.is_file() {
                configuration.read_only_binds.push(known_hosts);
            }
        }
        let mut runner =
            SandboxedCommandRunner::try_new(self.runner.clone(), request.git_directory())
                .map_err(|_| failure())?
                .with_sandbox_configuration(configuration);
        let mut command = Vec::new();
        for (name, value) in environment {
            let name = name.to_str().ok_or_else(failure)?;
            // The sandbox runner supplies its admitted runtime PATH and mounts the
            // private push snapshot as the current workspace.
            let value = match name {
                "PATH" => continue,
                "GIT_DIR" => ".",
                "GIT_OBJECT_DIRECTORY" => "./objects",
                "SSH_AUTH_SOCK" => SANDBOX_SSH_AGENT_SOCKET,
                _ => value.to_str().ok_or_else(failure)?,
            };
            command.push(format!("{name}={value}"));
        }
        command.push("git".to_owned());
        command.extend(arguments.iter().map(|argument| (*argument).to_owned()));
        let result = runner
            .try_run(ExecArguments {
                program: "env".to_owned(),
                arguments: command,
                working_directory: ".".to_owned(),
                timeout_seconds: 300,
            })
            .await
            .map_err(|_| failure())?;
        Ok(ProcessRunResult {
            outcome: result.outcome,
            stdout: signalbox_tools_exec::ProcessOutput {
                bytes: result.stdout.text.into_bytes(),
                completeness: result.stdout.completeness,
            },
            stderr: signalbox_tools_exec::ProcessOutput {
                bytes: result.stderr.text.into_bytes(),
                completeness: result.stderr.completeness,
            },
        })
    }
}

async fn snapshot_ssh_key(
    path: &std::path::Path,
) -> Result<tempfile::NamedTempFile, GitPushTransportFailure> {
    use signalbox_model_runtime::{CredentialAccess, CredentialReference};
    use std::io::Write;
    let reference =
        CredentialReference::new(crate::repo_watch_credentials::GIT_PUSH_CREDENTIAL_REFERENCE);
    let access = crate::FileCredentialAccess::new(path.to_owned(), reference.clone());
    let key = access
        .resolve(&reference)
        .await
        .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?;
    let mut snapshot = tempfile::NamedTempFile::new()
        .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?;
    snapshot
        .write_all(key.expose_bytes())
        .and_then(|()| snapshot.write_all(b"\n"))
        .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?;
    Ok(snapshot)
}

fn remaining_push_timeout(deadline: tokio::time::Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(tokio::time::Instant::now())
        .filter(|remaining| !remaining.is_zero())
}

fn process_request(
    request: &GitPushRequest,
    environment: &BTreeMap<OsString, OsString>,
    arguments: &[&str],
    timeout: Duration,
) -> ProcessRequest {
    ProcessRequest {
        program: "git".into(),
        arguments: arguments.iter().map(OsString::from).collect(),
        working_directory: request.repository_root().to_owned(),
        timeout,
        capture_bytes: 64 * 1024,
        environment: environment.clone(),
        environment_inheritance: ProcessEnvironment::Clear,
        status_protocol: ProcessStatusProtocol::Direct,
    }
}

async fn push_with_refresh<
    Refresh: std::future::Future<Output = Result<Option<String>, RepositoryWatchClientLoadError>>,
>(
    runner: &mut impl ProcessRunner,
    request: &mut ProcessRequest,
    deadline: tokio::time::Instant,
    refresh: impl FnOnce(Duration) -> Refresh,
) -> Result<ProcessRunResult, GitPushTransportFailure> {
    request.timeout = remaining_push_timeout(deadline)
        .ok_or(GitPushTransportFailure::PreDispatchInfrastructure)?;
    let first = runner.run(request.clone()).await;
    if git_authentication_rejected(&first) {
        let remaining = remaining_push_timeout(deadline)
            .ok_or(GitPushTransportFailure::PreDispatchInfrastructure)?;
        let refreshed = tokio::time::timeout_at(deadline, refresh(remaining))
            .await
            .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?
            .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?;
        if let Some(url) = refreshed {
            request.environment.insert(
                "GIT_CONFIG_KEY_0".into(),
                format!("url.{url}.insteadOf").into(),
            );
            request.timeout = remaining_push_timeout(deadline)
                .ok_or(GitPushTransportFailure::PreDispatchInfrastructure)?;
            return Ok(runner.run(request.clone()).await);
        }
    }
    Ok(first)
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
            if git_authentication_rejected(result)
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
        delays: std::collections::VecDeque<Duration>,
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
            if let Some(delay) = self.delays.pop_front() {
                tokio::time::sleep(delay).await;
            }
            self.responses.pop_front().expect("no extra Git invocation")
        }
    }

    fn push_fixture(
        responses: impl IntoIterator<Item = ProcessRunResult>,
    ) -> (RecordedRunner, ProcessRequest) {
        (RecordedRunner { responses: responses.into_iter().collect(), requests: Vec::new(), delays: Default::default() }, ProcessRequest {
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

    #[tokio::test(start_paused = true)]
    async fn push_attempts_and_refresh_consume_only_the_budget_left_after_preparation() {
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(300);
        let (mut runner, mut request) = push_fixture([
            authentication_failure(),
            result(ProcessOutcome::TimedOut, "", ""),
        ]);
        runner.delays = [Duration::from_secs(20), Duration::from_secs(25)].into();
        // Credential preparation has already consumed four minutes of the push budget.
        tokio::time::advance(Duration::from_secs(240)).await;
        let response = push_with_refresh(
            &mut runner,
            &mut request,
            deadline,
            |remaining| async move {
                assert_eq!(remaining, Duration::from_secs(40));
                tokio::time::sleep(Duration::from_secs(15)).await;
                Ok(Some(
                    "https://x-access-token:synthetic-new@github.com/fixture/project.git"
                        .to_owned(),
                ))
            },
        )
        .await
        .expect("retry returns its process outcome");
        assert_eq!(runner.requests[0].timeout, Duration::from_secs(60));
        assert_eq!(runner.requests[1].timeout, Duration::from_secs(25));
        assert_eq!(response.outcome, ProcessOutcome::TimedOut);
        assert_eq!(started.elapsed(), Duration::from_secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_push_preparation_does_not_start_git() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        let (mut runner, mut request) = push_fixture([]);
        tokio::time::advance(Duration::from_secs(300)).await;
        let response = push_with_refresh(&mut runner, &mut request, deadline, |_| async {
            panic!("an expired push must not refresh")
        })
        .await;
        assert_eq!(
            response.err(),
            Some(GitPushTransportFailure::PreDispatchInfrastructure)
        );
        assert!(runner.requests.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_refresh_expires_without_starting_a_push_retry() {
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(300);
        let (mut runner, mut request) = push_fixture([authentication_failure()]);
        runner.delays = [Duration::from_secs(250)].into();
        let response = tokio::time::timeout(
            Duration::from_secs(301),
            push_with_refresh(
                &mut runner,
                &mut request,
                deadline,
                |remaining| async move {
                    assert_eq!(remaining, Duration::from_secs(50));
                    std::future::pending().await
                },
            ),
        )
        .await
        .expect("refresh must expire within the original push deadline");
        assert_eq!(
            response.err(),
            Some(GitPushTransportFailure::PreDispatchInfrastructure)
        );
        assert_eq!(runner.requests.len(), 1);
        assert_eq!(started.elapsed(), Duration::from_secs(300));
    }

    #[tokio::test]
    async fn authentication_rejection_retries_with_the_refreshed_url() {
        let (mut runner, mut request) = push_fixture([
            authentication_failure(),
            result(ProcessOutcome::Exited { code: Some(0) }, "", ""),
        ]);
        let response = push_with_refresh(
            &mut runner,
            &mut request,
            tokio::time::Instant::now() + PUSH_TIMEOUT,
            |_| async {
                Ok(Some(
                    "https://x-access-token:synthetic-new@github.com/fixture/project.git"
                        .to_owned(),
                ))
            },
        )
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
        let response = push_with_refresh(
            &mut runner,
            &mut request,
            tokio::time::Instant::now() + PUSH_TIMEOUT,
            |_| async {
                Ok(Some(
                    "https://x-access-token:synthetic-new@github.com/fixture/project.git"
                        .to_owned(),
                ))
            },
        )
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
        let response = push_with_refresh(
            &mut runner,
            &mut request,
            tokio::time::Instant::now() + PUSH_TIMEOUT,
            |_| async { panic!("branch rejection must not refresh authentication") },
        )
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
        let response = push_with_refresh(
            &mut runner,
            &mut request,
            tokio::time::Instant::now() + PUSH_TIMEOUT,
            |_| async { panic!("unknown outcome must not trigger a retry") },
        )
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
        let response = push_with_refresh(
            &mut runner,
            &mut request,
            tokio::time::Instant::now() + PUSH_TIMEOUT,
            |_| async { Ok(None) },
        )
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
        let response = push_with_refresh(
            &mut runner,
            &mut request,
            tokio::time::Instant::now() + PUSH_TIMEOUT,
            |_| async { Err(RepositoryWatchClientLoadError::CredentialUnavailable) },
        )
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

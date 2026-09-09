use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

use signalbox_tools_exec::{
    CaptureCompleteness, ProcessEnvironment, ProcessOutcome, ProcessRequest, ProcessRunResult,
    ProcessRunner, ProcessStatusProtocol,
};
use signalbox_tools_git::{
    GitPushReceipt, GitPushRequest, GitPushTransport, GitPushTransportFailure,
};

use crate::repo_watch_credentials::RepositoryWatchClientLoader;

pub(super) struct ProcessGitPushTransport<Runner> {
    pub(super) runner: Runner,
    pub(super) credential_file: Option<PathBuf>,
    pub(super) ssh_agent_socket: Option<OsString>,
}

impl<Runner: ProcessRunner> GitPushTransport for ProcessGitPushTransport<Runner> {
    async fn push(
        &mut self,
        request: GitPushRequest,
    ) -> Result<GitPushReceipt, GitPushTransportFailure> {
        if !request.repository_root().is_dir() {
            return Err(GitPushTransportFailure::PreDispatchInfrastructure);
        }
        let mut environment: BTreeMap<OsString, OsString> = [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_CONFIG_COUNT", "8"),
            ("GIT_CONFIG_KEY_0", "credential.helper"),
            ("GIT_CONFIG_VALUE_0", ""),
            ("GIT_CONFIG_KEY_1", "core.hooksPath"),
            ("GIT_CONFIG_VALUE_1", "/dev/null"),
            ("GIT_CONFIG_KEY_2", "http.followRedirects"),
            ("GIT_CONFIG_VALUE_2", "false"),
            ("GIT_CONFIG_KEY_3", "pack.window"),
            ("GIT_CONFIG_VALUE_3", "0"),
            ("GIT_CONFIG_KEY_4", "pack.depth"),
            ("GIT_CONFIG_VALUE_4", "0"),
            ("GIT_CONFIG_KEY_5", "core.bigFileThreshold"),
            ("GIT_CONFIG_VALUE_5", "1"),
            ("GIT_CONFIG_KEY_6", "core.packedGitWindowSize"),
            ("GIT_CONFIG_VALUE_6", "1m"),
            ("GIT_CONFIG_KEY_7", "core.packedGitLimit"),
            ("GIT_CONFIG_VALUE_7", "8m"),
            ("LC_ALL", "C"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
        let _private_key = if request.remote().url().starts_with("https://") {
            let path = self
                .credential_file
                .clone()
                .ok_or(GitPushTransportFailure::PreDispatchInfrastructure)?;
            let authorization = RepositoryWatchClientLoader::for_git_push(path)
                .git_authorization()
                .await
                .map_err(|_| GitPushTransportFailure::PreDispatchInfrastructure)?;
            environment.insert("GIT_CONFIG_COUNT".into(), "9".into());
            environment.insert(
                "GIT_CONFIG_KEY_8".into(),
                format!("http.{}.extraheader", request.remote().url()).into(),
            );
            environment.insert("GIT_CONFIG_VALUE_8".into(), authorization.into());
            None
        } else {
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
                if self.ssh_agent_socket.is_none() {
                    return Err(GitPushTransportFailure::PreDispatchInfrastructure);
                }
                command.push_str(" -o IdentityFile=none");
            }
            if let Some(socket) = &self.ssh_agent_socket {
                environment.insert("SSH_AUTH_SOCK".into(), socket.clone());
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
        let push = self
            .run(
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
            .await;
        classify_push(&push)?;
        let remote_ref = format!("refs/heads/{}", request.branch());
        let confirmation = self
            .run(
                &request,
                &environment,
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

fn classify_push(result: &ProcessRunResult) -> Result<(), GitPushTransportFailure> {
    match result.outcome {
        ProcessOutcome::Exited { code: Some(0) } => Ok(()),
        ProcessOutcome::SpawnFailed { .. } => {
            Err(GitPushTransportFailure::PreDispatchInfrastructure)
        }
        ProcessOutcome::Exited { code: Some(_) } => {
            let output = String::from_utf8_lossy(&result.stdout.bytes);
            let error = String::from_utf8_lossy(&result.stderr.bytes).to_ascii_lowercase();
            if output.lines().any(|line| line.starts_with("!\t"))
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

#[cfg(test)]
#[path = "git_push_tests.rs"]
mod ssh_tests;

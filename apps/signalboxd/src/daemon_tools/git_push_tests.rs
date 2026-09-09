use super::ProcessGitPushTransport;
use signalbox_application::{
    FixtureToolExecutionTransaction, FixtureTransactionFailures, InProcessToolDispatchGate,
    PreparedAttemptApproval, PreparedAttemptIdentities, PreparedAttemptProposal,
    RecordingToolExecutor, ToolExecutionService, ToolExecutorEvidence, UuidV7ToolLoopIdGenerator,
    prepared_single_attempt_batch,
};
use signalbox_domain::{
    ContextFrontierId, DurableCommandId, ModelCallId, NormalizedToolArguments, SessionId,
    ToolAttemptId, ToolEffectClass, ToolName, ToolRequestId, TurnAttemptId, TurnId,
};
use signalbox_tools_exec::{ProcessRequest, ProcessRunResult, ProcessRunner, TokioProcessRunner};
use signalbox_tools_git::{ConfiguredGitRemote, GIT_PUSH_CONFIGURED_NAME, GitPushTools};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone)]
struct LocalSshRunner {
    inner: TokioProcessRunner,
    bin: PathBuf,
}

impl ProcessRunner for LocalSshRunner {
    fn sandbox_launcher_program(&self) -> &Path {
        self.inner.sandbox_launcher_program()
    }
    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        self.inner.sandbox_launcher_descriptor()
    }
    async fn bwrap_availability(
        &mut self,
        request: ProcessRequest,
    ) -> signalbox_tools_exec::BwrapAvailability {
        self.inner.bwrap_availability(request).await
    }

    async fn run(&mut self, mut request: ProcessRequest) -> ProcessRunResult {
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(
            request
                .environment
                .get(std::ffi::OsStr::new("PATH"))
                .expect("transport PATH"),
        ));
        request.environment.insert(
            "PATH".into(),
            std::env::join_paths(paths).expect("fixture PATH"),
        );
        self.inner.run(request).await
    }
}

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-C", root.to_str().expect("fixture path")])
        .args(arguments)
        .output()
        .expect("fixture Git runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git text")
        .trim()
        .to_owned()
}

#[tokio::test]
async fn ssh_push_uses_configured_key_or_agent_with_both_destination_forms() {
    for use_key in [false, true] {
        for scp_style in [false, true] {
            exercise_ssh_push(use_key, scp_style, 1024).await;
        }
    }
}

#[tokio::test]
#[ignore = "generates and pushes a 1 GB blob through the local SSH transport"]
async fn ssh_push_streams_a_generated_gigabyte_blob() {
    exercise_ssh_push(false, false, 1_000_000_000).await;
}

async fn exercise_ssh_push(use_key: bool, scp_style: bool, blob_bytes: u64) {
    let fixture = tempfile::tempdir().expect("SSH fixture");
    let root = fixture.path().join("worktree");
    let remote = fixture.path().join("remote.git");
    fs::create_dir(&root).expect("worktree");
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "core.bigFileThreshold", "1"]);
    git(&root, &["config", "user.name", "SSH fixture"]);
    git(&root, &["config", "user.email", "ssh-fixture@example.test"]);
    fs::write(root.join("tracked"), "initial\n").expect("initial content");
    git(&root, &["add", "tracked"]);
    git(&root, &["commit", "-m", "Initial fixture"]);
    let fence = git(&root, &["rev-parse", "HEAD"]);
    git(
        &root,
        &[
            "clone",
            "--bare",
            ".",
            remote.to_str().expect("remote path"),
        ],
    );
    fs::write(root.join("tracked"), "updated\n").expect("updated content");
    fs::OpenOptions::new()
        .write(true)
        .open(root.join("tracked"))
        .expect("generated blob")
        .set_len(blob_bytes)
        .expect("sparse blob length");
    git(&root, &["commit", "-am", "Update fixture"]);
    let target = git(&root, &["rev-parse", "HEAD"]);
    let bin = fixture.path().join("bin");
    fs::create_dir(&bin).expect("shim directory");
    let shim = bin.join("ssh");
    let log = fixture.path().join("ssh.log");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$@" "agent=$SSH_AUTH_SOCK" >> '{}'
key_next=no
has_key=no
for last do
    if [ "$key_next" = yes ]; then
        test -s "$last" || exit 91
        IFS= read -r header < "$last"
        [ "$header" = '-----BEGIN OPENSSH PRIVATE KEY-----' ] || exit 92
        has_key=yes
    fi
    key_next=no
    [ "$last" != -i ] || key_next=yes
done
[ "$has_key" = yes ] || [ -n "$SSH_AUTH_SOCK" ] || exit 93
case "$last" in
    'git-receive-pack '*|'git-upload-pack '*) exec /bin/sh -c "$last" ;;
    *) exit 90 ;;
esac
"#,
        log.display()
    );
    fs::write(&shim, script).expect("SSH shim");
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).expect("executable shim");
    let key = fixture.path().join("key");
    if use_key {
        let status = Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                key.to_str().expect("key path"),
            ])
            .status()
            .expect("fixture key generation");
        assert!(status.success());
    }
    let agent = fixture.path().join("agent.sock");
    let runner = LocalSshRunner {
        inner: TokioProcessRunner::try_new(std::env::current_exe().expect("test executable"))
            .expect("process runner"),
        bin,
    };
    let transport = ProcessGitPushTransport {
        runner,
        credential_file: use_key.then_some(key),
        ssh_agent_socket: (!use_key).then(|| agent.as_os_str().to_owned()),
    };
    let url = if scp_style {
        format!("git@fixture:{}", remote.display())
    } else {
        format!("ssh://git@fixture{}", remote.display())
    };
    let (catalog, executor) = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        &root,
        ConfiguredGitRemote::try_new("origin", url).expect("SSH destination"),
        transport,
    )
    .expect("push tools")
    .into_parts();
    let executor = executor
        .with_branch_fence("main".to_owned())
        .with_commit_fence(fence);
    let (executor, recorded) = RecordingToolExecutor::new(executor);
    let batch = prepared_single_attempt_batch(
        // Arbitrary, disjoint identities for one approved fixture attempt.
        PreparedAttemptIdentities {
            session: SessionId::from_uuid(uuid::Uuid::from_u128(13901)),
            turn: TurnId::from_uuid(uuid::Uuid::from_u128(13902)),
            producing_call: ModelCallId::from_uuid(uuid::Uuid::from_u128(13903)),
            request: ToolRequestId::from_uuid(uuid::Uuid::from_u128(13904)),
            attempt: ToolAttemptId::from_uuid(uuid::Uuid::from_u128(13905)),
            issuing_turn_attempt: TurnAttemptId::from_uuid(uuid::Uuid::from_u128(13906)),
            frontier: ContextFrontierId::from_uuid(uuid::Uuid::from_u128(13907)),
        },
        PreparedAttemptProposal {
            name: ToolName::try_new(GIT_PUSH_CONFIGURED_NAME.to_owned()).expect("push tool name"),
            arguments: NormalizedToolArguments::try_from_provider_text(
                r#"{"branch":"main"}"#.to_owned(),
            )
            .expect("push arguments"),
            effect_class: ToolEffectClass::ExternalEffect,
            approval: PreparedAttemptApproval::UserConfirmation {
                command: DurableCommandId::from_uuid(uuid::Uuid::from_u128(13908)),
            },
        },
    );
    let mut service = ToolExecutionService::new(
        UuidV7ToolLoopIdGenerator,
        FixtureToolExecutionTransaction::new(
            batch.clone(),
            FixtureTransactionFailures {
                domain_rejection: crate::DaemonToolExecutorError::unknown_tool(),
                declined_crash_classification: crate::DaemonToolExecutorError::unknown_tool(),
            },
        ),
        catalog,
        executor,
        InProcessToolDispatchGate::default(),
    );
    service
        .execute(batch.session(), batch.turn())
        .await
        .expect("approved push executes");
    assert!(matches!(
        recorded.take(),
        Some(ToolExecutorEvidence::CompletedText(_))
    ));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), target);
    let observed = fs::read_to_string(log).expect("SSH invocation log");
    assert!(observed.contains("BatchMode=yes"));
    assert!(observed.contains("git-receive-pack"));
    assert!(observed.contains("git-upload-pack"));
    if use_key {
        assert!(observed.contains("IdentitiesOnly=yes"));
    } else {
        assert!(observed.contains(&format!("agent={}", agent.display())));
    }
}

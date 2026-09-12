use super::{ProcessGitPushTransport, SANDBOX_SSH_AGENT_SOCKET};
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
use signalbox_tools_exec::{ProcessRequest, ProcessRunResult, ProcessRunner};
use signalbox_tools_git::{ConfiguredGitRemote, GIT_PUSH_CONFIGURED_NAME, GitPushTools};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;
use std::{
    fs,
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone)]
struct LocalSshRunner {
    bin: PathBuf,
    launcher_path: PathBuf,
    launcher: std::sync::Arc<fs::File>,
}

impl ProcessRunner for LocalSshRunner {
    fn sandbox_launcher_program(&self) -> &Path {
        &self.launcher_path
    }
    fn sandbox_launcher_descriptor(&self) -> Option<i32> {
        use std::os::fd::AsRawFd;
        Some(self.launcher.as_raw_fd())
    }
    async fn bwrap_availability(
        &mut self,
        request: ProcessRequest,
    ) -> signalbox_tools_exec::BwrapAvailability {
        let result = self.run(request).await;
        if matches!(
            result.outcome,
            signalbox_tools_exec::ProcessOutcome::Exited { code: Some(0) }
        ) {
            signalbox_tools_exec::BwrapAvailability::Available
        } else {
            signalbox_tools_exec::BwrapAvailability::Unusable
        }
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
        assert_eq!(
            request.environment_inheritance,
            signalbox_tools_exec::ProcessEnvironment::Clear
        );
        let mut stdout = tempfile::tempfile().expect("stdout capture");
        let mut stderr = tempfile::tempfile().expect("stderr capture");
        let status = tokio::time::timeout(
            request.timeout,
            tokio::process::Command::new(&request.program)
                .args(&request.arguments)
                .current_dir(&request.working_directory)
                .env_clear()
                .envs(&request.environment)
                .kill_on_drop(true)
                .stdout(stdout.try_clone().expect("stdout descriptor"))
                .stderr(stderr.try_clone().expect("stderr descriptor"))
                .status(),
        )
        .await
        .expect("fixture process deadline")
        .expect("fixture process starts");
        let result = ProcessRunResult {
            outcome: signalbox_tools_exec::ProcessOutcome::Exited {
                code: status.code(),
            },
            stdout: captured_output(&mut stdout, request.capture_bytes),
            stderr: captured_output(&mut stderr, request.capture_bytes),
        };
        if !status.success() {
            eprintln!(
                "local SSH fixture Git: {}",
                String::from_utf8_lossy(&result.stderr.bytes)
            );
        }
        result
    }
}

fn captured_output(file: &mut fs::File, limit: usize) -> signalbox_tools_exec::ProcessOutput {
    use std::io::{Read, Seek, SeekFrom};
    let length = file.metadata().expect("capture size").len();
    file.seek(SeekFrom::Start(0)).expect("capture rewind");
    let mut bytes = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .expect("bounded capture");
    signalbox_tools_exec::ProcessOutput {
        bytes,
        completeness: if length > limit as u64 {
            signalbox_tools_exec::CaptureCompleteness::Truncated
        } else {
            signalbox_tools_exec::CaptureCompleteness::Complete
        },
    }
}

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "maintenance.auto=false"])
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
async fn ssh_push_uses_configured_key_with_both_destination_forms() {
    for scp_style in [false, true] {
        exercise_ssh_push(true, scp_style, 1024).await;
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires host Bubblewrap user and mount namespaces"]
async fn ssh_push_uses_agent_and_account_trust_files_inside_sandbox_for_both_destination_forms() {
    for scp_style in [false, true] {
        exercise_ssh_push(false, scp_style, 1024).await;
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "generates and pushes a 1 GB blob through the local SSH transport"]
async fn ssh_push_streams_a_generated_gigabyte_blob() {
    exercise_ssh_push(false, false, 1_000_000_000).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires host Bubblewrap user and mount namespaces"]
async fn ssh_push_keeps_account_trust_visible_under_workspace() {
    const CHILD_ENVIRONMENT: &str = "SIGNALBOX_TEST_SSH_WORKSPACE_HOME";
    if std::env::var_os(CHILD_ENVIRONMENT).is_some() {
        for home in [
            Path::new("/workspace"),
            Path::new("/workspace/account-home"),
        ] {
            for scp_style in [false, true] {
                exercise_ssh_push_with_home(false, scp_style, 1024, Some(home)).await;
            }
        }
        return;
    }
    // Give the fixture a host account home under /workspace without changing the host.
    let home = tempfile::tempdir().expect("outer account home");
    let nsswitch = tempfile::NamedTempFile::new().expect("outer NSS configuration");
    fs::write(
        nsswitch.path(),
        b"passwd: sss\ngroup: files\nhosts: files dns\n",
    )
    .expect("host passwd lookup omits local files");
    let mut command = Command::new("bwrap");
    command.args(["--die-with-parent", "--tmpfs", "/"]);
    for entry in fs::read_dir("/").expect("host root entries") {
        let entry = entry.expect("host root entry");
        if entry.file_name() != "workspace"
            && entry.file_name() != "tmp"
            && entry.file_name() != "dev"
            && entry.file_name() != "proc"
        {
            command
                .arg("--ro-bind-try")
                .arg(entry.path())
                .arg(entry.path());
        }
    }
    let output = command
        .arg("--ro-bind").arg(nsswitch.path()).arg("/etc/nsswitch.conf")
        .args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind"])
        .arg(home.path()).arg("/workspace")
        .args(["--setenv", "TMPDIR", "/tmp", "--setenv", CHILD_ENVIRONMENT, "1"])
        .arg(std::env::current_exe().expect("test executable"))
        .args(["--exact", "daemon_tools::git_push::ssh_tests::ssh_push_keeps_account_trust_visible_under_workspace", "--ignored", "--nocapture"])
        .output().expect("nested sandbox fixture starts");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn exercise_ssh_push(use_key: bool, scp_style: bool, blob_bytes: u64) {
    exercise_ssh_push_with_home(use_key, scp_style, blob_bytes, None).await;
}

async fn exercise_ssh_push_with_home(
    use_key: bool,
    scp_style: bool,
    blob_bytes: u64,
    home: Option<&Path>,
) {
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
    let agent_name = if use_key {
        std::ffi::OsString::from("agent.sock")
    } else {
        std::ffi::OsString::from_vec(b"agent-\xff.sock".to_vec())
    };
    let agent = fixture.path().join(agent_name);
    let outside = fixture.path().join("outside-sandbox");
    fs::write(&outside, b"host-only fixture sentinel").expect("outside sentinel");
    let account_home = home.map_or_else(|| fixture.path().join("account-home"), Path::to_owned);
    let ssh_directory = account_home.join(".ssh");
    fs::create_dir_all(&ssh_directory).expect("account SSH directory");
    let first_trust = ssh_directory.join("known_hosts");
    let second_trust = ssh_directory.join("known_hosts2");
    fs::write(&first_trust, b"fixture primary trust\n").expect("primary known hosts");
    fs::write(&second_trust, b"fixture fallback trust\n").expect("fallback known hosts");
    let getent = bin.join("getent");
    let account_lookup = format!(
        "#!/usr/bin/python3\nimport sys\nassert sys.argv[1] == 'passwd'\nprint('fixture:x:' + sys.argv[2] + ':0:Fixture:' + {} + ':/bin/sh')\n",
        serde_json::to_string(&account_home).expect("account path literal")
    );
    fs::write(&getent, account_lookup).expect("account lookup fixture");
    fs::set_permissions(&getent, fs::Permissions::from_mode(0o700))
        .expect("account lookup executable");
    let server = start_ssh_proxy(&agent, remote.clone(), log.clone());
    let agent = fs::canonicalize(&agent).expect("fixture agent socket resolves");
    let script = r###"#!/usr/bin/python3
import json
import os
from pathlib import Path
import socket
import pwd
import sys
import threading

arguments = sys.argv[1:]
agent = os.environ.get('SSH_AUTH_SOCK')
has_key = False
for index, argument in enumerate(arguments):
    if argument == '-i':
        with open(arguments[index + 1]) as key:
            assert key.readline().strip() == '-----BEGIN OPENSSH PRIVATE KEY-----'
        has_key = True
assert has_key or agent
if agent:
    assert os.getcwd() == '/workspace'
    account = pwd.getpwuid(os.getuid())
    assert account.pw_name == 'fixture'
    assert account.pw_dir == FIXTURE_ACCOUNT_HOME
    assert not Path(FIXTURE_OUTSIDE).exists()
    trust_files = next((argument.split('=', 1)[1].split() for argument in arguments if argument.startswith('UserKnownHostsFile=')), [str(Path(account.pw_dir) / '.ssh' / name) for name in ['known_hosts', 'known_hosts2']])
    assert len(trust_files) == 2
    assert Path(trust_files[0]).read_text() == 'fixture primary trust\n'
    assert Path(trust_files[1]).read_text() == 'fixture fallback trust\n'
connection = socket.socket(socket.AF_UNIX)
connection.connect(agent or FIXTURE_SOCKET)
connection.sendall((json.dumps({'arguments': arguments, 'agent': agent, 'confined': bool(agent)}) + '\n').encode())

def forward_input():
    while True:
        chunk = os.read(0, 65536)
        if not chunk:
            connection.shutdown(socket.SHUT_WR)
            return
        connection.sendall(chunk)

threading.Thread(target=forward_input, daemon=True).start()
while True:
    chunk = connection.recv(65536)
    if not chunk:
        break
    sys.stdout.buffer.write(chunk)
    sys.stdout.buffer.flush()
"###
        .replace("FIXTURE_OUTSIDE", &serde_json::to_string(&outside).expect("outside path literal"))
        .replace("FIXTURE_SOCKET", &serde_json::to_string(&agent.to_str()).expect("socket path literal"));
    let script = script.replace(
        "FIXTURE_ACCOUNT_HOME",
        &serde_json::to_string(&account_home).expect("account home literal"),
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
    let launcher_path = bin.join("dispatch");
    // Test implementation of the exec dispatch marker protocol; bubblewrap itself is real.
    fs::write(&launcher_path, b"#!/bin/sh\n[ \"$1\" = --dispatch ] || exit 91\nshift\nprintf 'signalbox-exec:dispatched\\n' >&2\nexec \"$@\"\n").expect("fixture dispatcher");
    fs::set_permissions(&launcher_path, fs::Permissions::from_mode(0o700))
        .expect("dispatcher executable");
    let launcher = fs::File::open(&launcher_path).expect("dispatcher descriptor");
    rustix::io::fcntl_setfd(&launcher, rustix::io::FdFlags::empty()).expect("inherited dispatcher");
    let sandbox = signalbox_tools_exec::SandboxConfiguration {
        read_only_binds: vec![bin.clone()],
        path_prepend: vec![bin.clone()],
        ..Default::default()
    };
    let runner = LocalSshRunner {
        bin,
        launcher_path,
        launcher: std::sync::Arc::new(launcher),
    };
    // The daemon probes from its working directory, while Git runs elsewhere.
    let relative_agent = std::env::current_dir()
        .expect("daemon working directory")
        .components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .fold(std::path::PathBuf::new(), |mut path, _| {
            path.push("..");
            path
        })
        .join(agent.strip_prefix("/").expect("absolute fixture socket"));
    assert!(!relative_agent.is_absolute());
    let retained_agent =
        crate::configuration::WatchedRepositoryConfiguration::absolute_ssh_agent_socket(
            &relative_agent,
        )
        .expect("normalized socket");
    assert_eq!(retained_agent, agent);
    let transport = ProcessGitPushTransport {
        runner,
        credentials: crate::repo_watch_credentials::RepositoryWatchClientLoader::for_git_push(
            key.clone(),
        ),
        credential_file: use_key.then_some(key),
        ssh_agent_socket: Some(retained_agent.into_os_string()),
        sandbox,
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
    let evidence = recorded.take();
    assert!(
        matches!(evidence, Some(ToolExecutorEvidence::CompletedText(_))),
        "SSH push evidence: {evidence:?}"
    );
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), target);
    server.join().expect("SSH fixture server completes");
    let observed = fs::read_to_string(log).expect("SSH invocation log");
    assert!(observed.contains("BatchMode=yes"));
    assert!(observed.contains("git-receive-pack"));
    assert!(observed.contains("git-upload-pack"));
    if use_key {
        assert!(observed.contains("IdentitiesOnly=yes"));
    } else {
        assert!(observed.contains(&format!("agent={SANDBOX_SSH_AGENT_SOCKET}")));
        assert!(observed.contains("confined=true"));
    }
}

fn start_ssh_proxy(socket: &Path, remote: PathBuf, log: PathBuf) -> std::thread::JoinHandle<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::process::Stdio;
    let listener = UnixListener::bind(socket).expect("SSH fixture socket");
    std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut connection, _) = listener.accept().expect("SSH fixture connection");
            let mut header = Vec::new();
            loop {
                let mut byte = [0];
                connection
                    .read_exact(&mut byte)
                    .expect("SSH command header");
                if byte[0] == b'\n' {
                    break;
                }
                assert!(header.len() < 16 * 1024, "bounded SSH fixture header");
                header.push(byte[0]);
            }
            let request: serde_json::Value = serde_json::from_slice(&header).expect("SSH request");
            let arguments = request["arguments"].as_array().expect("SSH arguments");
            let command = arguments
                .last()
                .expect("server command")
                .as_str()
                .expect("command text");
            assert!(command.contains(remote.to_str().expect("remote path")));
            let service = if command.starts_with("git-receive-pack ") {
                "receive-pack"
            } else {
                assert!(command.starts_with("git-upload-pack "));
                "upload-pack"
            };
            let mut observed = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .expect("SSH log");
            for argument in arguments {
                writeln!(observed, "{}", argument.as_str().expect("argument text"))
                    .expect("log argument");
            }
            writeln!(
                observed,
                "agent={}",
                request["agent"].as_str().unwrap_or("")
            )
            .expect("log agent");
            writeln!(observed, "confined={}", request["confined"]).expect("log confinement");
            let mut child = Command::new("git")
                .args([
                    "-c",
                    "core.bigFileThreshold=1",
                    "-c",
                    "core.packedGitLimit=8m",
                    service,
                ])
                .arg(&remote)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_OBJECT_DIRECTORY")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("fixture Git server");
            let mut input = child.stdin.take().expect("server stdin");
            let mut output = child.stdout.take().expect("server stdout");
            let mut incoming = connection.try_clone().expect("server connection");
            let forward = std::thread::spawn(move || std::io::copy(&mut incoming, &mut input));
            std::io::copy(&mut output, &mut connection).expect("server response");
            connection
                .shutdown(std::net::Shutdown::Write)
                .expect("server response ends");
            assert!(child.wait().expect("server exit").success());
            // A successful Git server can close stdin before the client finishes
            // forwarding its final protocol bytes.
            if let Err(error) = forward.join().expect("input forwarding thread") {
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe,
                    "server input: {error}"
                );
            }
        }
    })
}

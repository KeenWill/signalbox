//! Explicit generated-repository scale suite; see tooling/generate-git-scale.py.

use super::support::identity;
use crate::arguments::{
    GitBranchCreateArguments, GitBranchSwitchArguments, GitCommitArguments, GitDiffArguments,
    GitLogArguments, GitStageArguments, LocalOperation,
};
use crate::{
    ConfiguredGitRemote, GitPushArguments, GitPushReceipt, GitPushRequest, GitPushTools,
    GitPushTransport, GitPushTransportFailure, LocalGitTools,
};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;
use std::{fs, os::unix::fs::FileExt, path::PathBuf, process::Command};

#[derive(Debug)]
struct LocalBareTransport(PathBuf);
impl GitPushTransport for LocalBareTransport {
    async fn push(
        &mut self,
        request: GitPushRequest,
    ) -> Result<GitPushReceipt, GitPushTransportFailure> {
        let result = Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("fixture PATH"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_DIR", request.git_directory())
            .env("GIT_OBJECT_DIRECTORY", request.object_directory())
            .args([
                "-c",
                "core.bigFileThreshold=1",
                "-c",
                "core.packedGitWindowSize=1m",
                "-c",
                "core.packedGitLimit=8m",
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "--quiet",
                "--",
            ])
            .arg(&self.0)
            .arg(request.refspec())
            .output()
            .expect("local push starts");
        assert!(
            result.status.success(),
            "local push failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let remote = git2::Repository::open_bare(&self.0).expect("bare destination opens");
        assert_eq!(
            remote
                .find_reference(&format!("refs/heads/{}", request.branch()))
                .expect("remote branch exists")
                .target()
                .expect("direct remote reference")
                .to_string(),
            request.commit()
        );
        GitPushReceipt::try_new(request.commit()).map_err(|_| GitPushTransportFailure::Rejected)
    }
}

#[tokio::test]
#[ignore = "generate a repository with tooling/generate-git-scale.py and set SIGNALBOX_GIT_SCALE_ROOT"]
async fn every_git_tool_handles_the_generated_scale_repository() {
    let root = PathBuf::from(
        std::env::var_os("SIGNALBOX_GIT_SCALE_ROOT").expect("scale root is explicit"),
    );
    exercise_every_git_tool(root).await;
}

pub(super) async fn exercise_every_git_tool(root: PathBuf) {
    let remote_directory = tempfile::tempdir().expect("remote directory");
    let remote_path = remote_directory.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["clone", "--quiet", "--bare", "--shared", "--"])
            .arg(&root)
            .arg(&remote_path)
            .status()
            .expect("bare clone starts")
            .success()
    );
    let repository = git2::Repository::open(&root).expect("scale repository opens");
    let initial = repository
        .head()
        .expect("HEAD")
        .target()
        .expect("initial commit");
    let (_, executor) = LocalGitTools::try_new(LocalWorkspaceFileSystem, &root, identity())
        .expect("scale suite constructs")
        .into_parts();
    let status = execute(&executor, LocalOperation::Status);
    assert_eq!(status["entries"], serde_json::json!([]));
    let file = fs::OpenOptions::new()
        .write(true)
        .open(root.join("large.bin"))
        .expect("large file opens");
    file.write_all_at(b"changed", 0)
        .expect("large file changes");
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    assert_eq!(diff["truncated"], true);
    assert!(diff["patch"].as_str().expect("patch").len() <= crate::MAX_DIFF_BYTES);
    execute(
        &executor,
        LocalOperation::Stage(GitStageArguments {
            paths: vec!["large.bin".to_owned()],
        }),
    );
    let commit = execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: "Scale content update".to_owned(),
        }),
    );
    let committed = commit["commit"].as_str().expect("new commit");
    execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: "HEAD".to_owned(),
            max_entries: 2,
        }),
    );
    execute(
        &executor,
        LocalOperation::BranchCreate(GitBranchCreateArguments {
            name: "scale-tool-branch".to_owned(),
            start: "HEAD".to_owned(),
        }),
    );
    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: "scale-0".to_owned(),
        }),
    );
    let mut prefix = [0u8; 7];
    fs::File::open(root.join("large.bin"))
        .expect("old file opens")
        .read_exact_at(&mut prefix, 0)
        .expect("old prefix reads");
    assert_eq!(prefix, [0u8; 7]);
    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: "scale-tool-branch".to_owned(),
        }),
    );
    fs::File::open(root.join("large.bin"))
        .expect("new file opens")
        .read_exact_at(&mut prefix, 0)
        .expect("new prefix reads");
    assert_eq!(&prefix, b"changed");
    let revision_diff = execute(
        &executor,
        LocalOperation::Diff(GitDiffArguments::Revisions {
            base: initial.to_string(),
            head: committed.to_owned(),
        }),
    );
    assert_eq!(revision_diff["truncated"], true);
    let (_, mut push) = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        &root,
        ConfiguredGitRemote::try_new("origin", "https://example.test/fixture.git")
            .expect("configured fixture remote"),
        LocalBareTransport(remote_path),
    )
    .expect("push tools construct")
    .into_parts();
    push = push
        .with_branch_fence("scale-tool-branch".to_owned())
        .with_commit_fence(initial.to_string());
    push.execute_push(GitPushArguments::for_test("scale-tool-branch"))
        .await
        .expect("scale push succeeds");
    let native_index = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "--error-unmatch", "large.bin"])
        .output()
        .expect("native Git reads the published index");
    assert!(
        native_index.status.success(),
        "{}",
        String::from_utf8_lossy(&native_index.stderr)
    );
}

fn execute(
    executor: &crate::LocalGitExecutor<LocalWorkspaceFileSystem>,
    operation: LocalOperation,
) -> serde_json::Value {
    let started = std::time::Instant::now();
    eprintln!("scale starts {operation:?}");
    let result = super::support::execute(executor, operation);
    eprintln!("scale operation elapsed {:?}", started.elapsed());
    result
}

#[tokio::test]
async fn every_git_tool_operates_on_a_linked_worktree() {
    exercise_linked_repository(false).await;
}

#[tokio::test]
async fn every_git_tool_operates_on_a_linked_worktree_of_a_bare_repository() {
    exercise_linked_repository(true).await;
}

async fn exercise_linked_repository(bare: bool) {
    let fixture = super::support::Fixture::new();
    let repository = git2::Repository::open(fixture.root()).expect("fixture opens");
    // Exceed the diff prefix budget so both diff modes exercise truncation.
    let blob_bytes = crate::MAX_DIFF_BYTES * 2;
    fs::write(fixture.root().join("large.bin"), vec![0u8; blob_bytes]).expect("blob writes");
    let initial = super::support::commit_all(&repository, "linked fixture");
    repository
        .branch(
            "scale-0",
            &repository.find_commit(initial).expect("initial commit"),
            false,
        )
        .expect("scale branch");
    let parent = tempfile::tempdir().expect("linked parent");
    let linked = parent.path().join("linked");
    if bare {
        let bare_root = parent.path().join("bare.git");
        assert!(
            Command::new("git")
                .args(["clone", "--bare", "--"])
                .arg(fixture.root())
                .arg(&bare_root)
                .output()
                .expect("bare clone")
                .status
                .success()
        );
        assert!(
            git2::Repository::open_bare(&bare_root)
                .expect("bare common repository")
                .config()
                .expect("common config")
                .get_bool("core.bare")
                .expect("bare flag")
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&bare_root)
                .args(["worktree", "add", "-b", "linked"])
                .arg(&linked)
                .output()
                .expect("native linked checkout")
                .status
                .success()
        );
        let native = Command::new("git")
            .arg("-C")
            .arg(&linked)
            .args(["rev-parse", "--is-bare-repository"])
            .output()
            .expect("native checkout bareness");
        assert!(native.status.success());
        assert_eq!(native.stdout, b"false\n");
        let (_, executor) = LocalGitTools::try_new(LocalWorkspaceFileSystem, &linked, identity())
            .expect("bare common config admits checkout")
            .into_parts();
        // A bare common HEAD is not an occupied worktree branch.
        execute(
            &executor,
            LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                name: repository
                    .head()
                    .expect("source HEAD")
                    .shorthand()
                    .expect("source branch")
                    .to_owned(),
            }),
        );
    } else {
        repository
            .worktree("linked", &linked, None)
            .expect("linked worktree creates");
    }
    let main_head = fs::read(fixture.root().join(".git/HEAD")).expect("main HEAD");
    let main_index = fs::read(fixture.root().join(".git/index")).expect("main index");
    exercise_every_git_tool(linked).await;
    assert_eq!(
        fs::read(fixture.root().join(".git/HEAD")).expect("main HEAD remains"),
        main_head
    );
    assert_eq!(
        fs::read(fixture.root().join(".git/index")).expect("main index remains"),
        main_index
    );
    assert_eq!(
        repository.head().expect("main branch remains").target(),
        Some(initial)
    );
}

#[test]
fn branch_switch_preserves_sibling_worktree_branch_occupancy() {
    exercise_branch_occupancy(false, false);
}

#[test]
fn branch_switch_resolves_sibling_symbolic_head_chains() {
    exercise_branch_occupancy(true, false);
}

#[test]
fn branch_switch_resolves_each_siblings_local_symbolic_refs() {
    exercise_branch_occupancy(true, true);
}

fn exercise_branch_occupancy(indirect: bool, local: bool) {
    let fixture = super::support::Fixture::new();
    let repository = git2::Repository::open(fixture.root()).expect("main repository");
    let main_branch = repository
        .head()
        .expect("main HEAD")
        .shorthand()
        .expect("branch")
        .to_owned();
    let parent = tempfile::tempdir().expect("linked parent");
    let first = parent.path().join("first");
    let second = parent.path().join("second");
    repository
        .worktree("first", &first, None)
        .expect("first worktree");
    repository
        .worktree("second", &second, None)
        .expect("second worktree");
    if indirect {
        for (root, name) in [
            (fixture.root(), main_branch.as_str()),
            (first.as_path(), "first"),
            (second.as_path(), "second"),
        ] {
            repository
                .reference_symbolic(
                    &format!("refs/heads/alias-{name}"),
                    &format!("refs/heads/{name}"),
                    true,
                    "occupancy fixture",
                )
                .expect("branch alias");
            repository
                .reference_symbolic(
                    &format!("refs/heads/indirect-{name}"),
                    &format!("refs/heads/alias-{name}"),
                    true,
                    "occupancy fixture",
                )
                .expect("second alias");
            let worktree = git2::Repository::open(root).expect("worktree");
            fs::write(
                worktree.path().join("HEAD"),
                format!("ref: refs/heads/indirect-{name}\n"),
            )
            .expect("indirect HEAD");
            if local {
                fs::create_dir_all(worktree.path().join("refs/worktree"))
                    .expect("local refs directory");
                fs::write(
                    worktree.path().join("refs/worktree/alias"),
                    format!("ref: refs/heads/indirect-{name}\n"),
                )
                .expect("sibling-local alias");
                fs::write(worktree.path().join("HEAD"), b"ref: refs/worktree/alias\n")
                    .expect("local symbolic HEAD");
            }
        }
    }
    let snapshot = |root: &std::path::Path| {
        let repository = git2::Repository::open(root).expect("repository");
        (
            fs::read(repository.path().join("HEAD")).expect("HEAD bytes"),
            fs::read(repository.path().join("index")).expect("index bytes"),
            fs::read(root.join(super::support::TRACKED_PATH)).expect("tracked bytes"),
        )
    };
    let initial = [
        snapshot(fixture.root()),
        snapshot(&first),
        snapshot(&second),
    ];
    for (root, target) in [
        (first.as_path(), main_branch.as_str()),
        (first.as_path(), "second"),
        (fixture.root(), "first"),
    ] {
        let native = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["switch", target])
            .output()
            .expect("native switch");
        assert!(
            !native.status.success(),
            "native Git must refuse occupied branch"
        );
        let (_, executor) = LocalGitTools::try_new(LocalWorkspaceFileSystem, root, identity())
            .expect("tool family")
            .into_parts();
        assert!(
            executor
                .execute_operation(LocalOperation::BranchSwitch(GitBranchSwitchArguments {
                    name: target.to_owned(),
                }))
                .is_err()
        );
        assert_eq!(
            [
                snapshot(fixture.root()),
                snapshot(&first),
                snapshot(&second)
            ],
            initial
        );
    }
    let (_, executor) = LocalGitTools::try_new(LocalWorkspaceFileSystem, &first, identity())
        .expect("tool family")
        .into_parts();
    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: "first".to_owned(),
        }),
    );
    let sibling = git2::Repository::open(&second).expect("sibling");
    sibling
        .set_head_detached(fixture.initial)
        .expect("release sibling branch");
    execute(
        &executor,
        LocalOperation::BranchSwitch(GitBranchSwitchArguments {
            name: "second".to_owned(),
        }),
    );
    assert_eq!(
        git2::Repository::open(&first)
            .expect("first")
            .head()
            .expect("HEAD")
            .shorthand()
            .expect("branch name"),
        "second"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linked_worktree_accepts_a_path_sized_administration_marker() {
    let parent = tempfile::tempdir().expect("parent");
    let mut root = parent.path().to_owned();
    // Exceed a revision record while keeping the complete path below Linux PATH_MAX.
    for _ in 0..12 {
        root.push("repository-path-component-".repeat(4));
    }
    fs::create_dir_all(&root).expect("long repository path");
    let repository = git2::Repository::init(&root).expect("repository");
    fs::write(root.join("tracked"), b"content\n").expect("content");
    super::support::commit_all(&repository, "long administration path");
    let linked = parent.path().join("linked");
    let result = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["worktree", "add", "-b", "linked"])
        .arg(&linked)
        .output()
        .expect("native Git worktree");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        fs::metadata(linked.join(".git")).expect("marker").len()
            > crate::limits::MAX_REVISION_BYTES as u64
    );
    let (_, executor) = LocalGitTools::try_new(LocalWorkspaceFileSystem, &linked, identity())
        .expect("path-sized marker is admitted")
        .into_parts();
    assert_eq!(
        execute(&executor, LocalOperation::Status)["entries"],
        serde_json::json!([])
    );
}

#[test]
fn branch_switch_rolls_back_when_a_sibling_claims_the_target_before_head_publication() {
    let fixture = super::support::Fixture::new();
    let repository = git2::Repository::open(fixture.root()).expect("main repository");
    let parent = tempfile::tempdir().expect("linked parent");
    let linked = parent.path().join("linked");
    repository
        .worktree("linked", &linked, None)
        .expect("linked worktree");
    repository
        .branch(
            "target",
            &repository.find_commit(fixture.initial).expect("commit"),
            false,
        )
        .expect("target branch");
    let (_, executor) = LocalGitTools::try_new(LocalWorkspaceFileSystem, &linked, identity())
        .expect("tool family")
        .into_parts();
    let linked_repository = git2::Repository::open(&linked).expect("linked repository");
    let head = fs::read(linked_repository.path().join("HEAD")).expect("HEAD");
    let index = fs::read(linked_repository.path().join("index")).expect("index");
    let outcome = executor.branch_switch_with_head_publish_hook(
        GitBranchSwitchArguments {
            name: "target".to_owned(),
        },
        || {
            repository
                .set_head("refs/heads/target")
                .expect("sibling claims target")
        },
    );
    assert!(outcome.is_err());
    assert_eq!(
        fs::read(linked_repository.path().join("HEAD")).expect("HEAD remains"),
        head
    );
    assert_eq!(
        fs::read(linked_repository.path().join("index")).expect("index restored"),
        index
    );
    assert_eq!(
        repository
            .head()
            .expect("sibling HEAD")
            .shorthand()
            .expect("branch name"),
        "target"
    );
}

fn measure_sibling_scan(count: usize) -> usize {
    let fixture = super::support::Fixture::new();
    for index in 0..count {
        let administration = fixture
            .root()
            .join(".git/worktrees")
            .join(format!("sibling-{index}"));
        fs::create_dir_all(&administration).expect("generated sibling administration");
        fs::write(
            administration.join("HEAD"),
            format!("ref: refs/heads/sibling-{index}\n"),
        )
        .expect("generated unborn sibling HEAD");
    }
    let executor = fixture.executor();
    crate::layout::take_administration_inspections();
    crate::repository_directories::require_branch_unoccupied(
        &executor.repository_authority,
        "refs/heads/unoccupied",
    )
    .expect("unoccupied branch");
    crate::layout::take_administration_inspections()
}

#[test]
fn sibling_occupancy_scan_visits_administration_entries_linearly() {
    let small = measure_sibling_scan(32);
    let large = measure_sibling_scan(512);
    assert!(small > 0);
    assert!(
        large <= small * 20,
        "32 siblings: {small} visits; 512 siblings: {large} visits"
    );
}

#[test]
#[ignore = "generated 5,000-sibling scale measurement"]
fn sibling_occupancy_scan_handles_five_thousand_worktrees() {
    let visits = measure_sibling_scan(5_000);
    eprintln!("5,000 sibling worktrees: {visits} administration entry visits");
}

#[test]
fn linked_worktree_accepts_crlf_administration_markers() {
    let fixture = super::support::Fixture::new();
    let repository = git2::Repository::open(fixture.root()).expect("main repository");
    let parent = tempfile::tempdir().expect("linked parent");
    let linked = parent.path().join("linked");
    repository
        .worktree("linked", &linked, None)
        .expect("linked worktree");
    let worktree = git2::Repository::open(&linked).expect("linked repository");
    for marker in [linked.join(".git"), worktree.path().join("commondir")] {
        let bytes = fs::read(&marker).expect("administration marker");
        let mut crlf = bytes.strip_suffix(b"\n").expect("LF marker").to_vec();
        crlf.extend_from_slice(b"\r\n");
        fs::write(marker, crlf).expect("CRLF marker");
    }
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&linked)
            .args(["rev-parse", "--git-common-dir"])
            .output()
            .expect("native Git marker resolution")
            .status
            .success()
    );
    let (_, executor) = LocalGitTools::try_new(LocalWorkspaceFileSystem, &linked, identity())
        .expect("CRLF markers admitted")
        .into_parts();
    assert_eq!(
        execute(&executor, LocalOperation::Status)["entries"],
        serde_json::json!([])
    );
}

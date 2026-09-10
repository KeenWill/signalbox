//! Configured-remote push contract and injected-transport properties.

use std::{
    fs,
    sync::{Arc, Mutex},
};

use git2::Repository;
use signalbox_application::ToolCatalog;
use signalbox_domain::{NormalizedToolArguments, ToolEffectClass, ToolName, ToolPermissionDefault};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::GIT_PUSH_CONFIGURED_NAME;
use crate::descriptor::file_identity;
use crate::push_arguments::GitPushArguments;
use crate::push_catalog::{GitPushTools, decode_push};
use crate::push_executor::GitPushFailure;
use crate::push_merge::DroppedBaseChanges;
use crate::push_transport::{
    ConfiguredGitRemote, GitPushReceipt, GitPushRequest, GitPushTransport, GitPushTransportFailure,
};
use crate::tests::support::{FIX_BRANCH, Fixture};

const REMOTE_NAME: &str = "origin";
const REMOTE_URL: &str = "https://github.com/KeenWill/signalbox.git";

#[tokio::test]
async fn push_preparation_deadline_returns_while_blocking_work_is_running() {
    use crate::push_executor::prepare_before_deadline;
    use std::{sync::mpsc, time::Duration};

    let (release, blocked) = mpsc::channel();
    let (started, running) = tokio::sync::oneshot::channel();
    let preparation = tokio::spawn(prepare_before_deadline(
        move |_| {
            started.send(()).expect("async worker receives start");
            blocked
                .recv_timeout(Duration::from_secs(2))
                .expect("test releases blocking work");
            Ok(())
        },
        Duration::from_millis(50),
    ));
    running.await.expect("blocking work starts");
    let result = preparation.await.expect("preparation task joins");
    release.send(()).expect("blocking work is still waiting");
    assert_eq!(result, Err(GitPushFailure::PreDispatchInfrastructure));
}

#[tokio::test]
async fn push_over_the_object_cap_refuses_before_transport() {
    use crate::limits::MAX_REPOSITORY_INSPECTIONS;
    use std::time::{Duration, Instant};

    let fixture = Fixture::new();
    let fence = fence_with_blob_count(&fixture, MAX_REPOSITORY_INSPECTIONS + 1);
    let transport = RecordingPushTransport::default();
    let mut executor = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL).expect("remote"),
        transport.clone(),
    )
    .expect("push executor")
    .into_parts()
    .1
    .with_commit_fence(fence.to_string());

    let started = Instant::now();
    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(result, Err(GitPushFailure::Repository));
    assert!(
        !transport.has_request(),
        "over-cap snapshots cannot reach transport"
    );
    // This generous regression ceiling distinguishes prompt cap refusal from the 300-second deadline.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "object-cap refusal must be prompt"
    );
}

/// Packs distinct blobs under trees small enough to fit the decoded-object byte cap.
fn fence_with_blob_count(fixture: &Fixture, count: usize) -> git2::Oid {
    use crate::tests::support::{AUTHOR_EMAIL, AUTHOR_NAME};

    let repository = Repository::open(fixture.root()).expect("fixture repository");
    plant_push_pack_with_unrelated_objects(&repository, &[], 0..count);
    let database = repository.odb().expect("fixture objects");
    let mut root = repository.treebuilder(None).expect("root builder");
    for (chunk, numbers) in (0..count).collect::<Vec<_>>().chunks(1000).enumerate() {
        let mut entries = Vec::new();
        for number in numbers {
            let oid = git2::Oid::hash_object(git2::ObjectType::Blob, &number.to_be_bytes())
                .expect("fixture blob ID");
            entries.extend_from_slice(format!("100644 {number:06}\0").as_bytes());
            entries.extend_from_slice(oid.as_bytes());
        }
        let tree = database
            .write(git2::ObjectType::Tree, &entries)
            .expect("subtree");
        root.insert(format!("{chunk:06}"), tree, 0o040000)
            .expect("subtree entry");
    }
    let tree = repository
        .find_tree(root.write().expect("root tree"))
        .expect("tree");
    let signature = git2::Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("author");
    repository
        .commit(
            Some(&format!("refs/heads/{FIX_BRANCH}")),
            &signature,
            &signature,
            "blob-count fence",
            &tree,
            &[],
        )
        .expect("fence commit")
}

#[test]
fn push_snapshot_accepts_a_fence_with_multiple_bounded_trees() {
    let fixture = Fixture::new();
    // Crosses the fixture's 1,000-blob subtree boundary while staying below the cap.
    let fence = fence_with_blob_count(&fixture, 1001);
    let snapshot = crate::push_objects::PushObjectSnapshot::capture(
        &fixture.executor().repository_authority,
        fence,
        Some(fence),
    )
    .expect("bounded fence captures");
    let root = snapshot
        .repository
        .find_commit(fence)
        .expect("fence commit")
        .tree()
        .expect("fence tree");
    assert_eq!(root.len(), 2);
    let first = snapshot
        .repository
        .find_tree(root.get_name("000000").expect("first subtree").id())
        .expect("first tree");
    let second = snapshot
        .repository
        .find_tree(root.get_name("000001").expect("second subtree").id())
        .expect("second tree");
    assert_eq!(first.len(), 1000);
    assert_eq!(second.len(), 1);
}

/// Measures preparation only; the transport records a request without sending it.
#[tokio::test]
#[ignore = "requires PUSH_BENCH_REPOSITORY pointing to a disposable no-checkout clone"]
async fn push_preparation_on_a_real_clone() {
    let root = std::env::var("PUSH_BENCH_REPOSITORY").expect("measurement clone path");
    let repository = Repository::open(&root).expect("measurement repository");
    let head = repository.head().expect("clone head");
    let branch = head.shorthand().expect("head branch").to_owned();
    let commit = head.target().expect("head commit");
    let started = std::time::Instant::now();
    let mut executor = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        &root,
        ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL).expect("remote"),
        RecordingPushTransport::default(),
    )
    .expect("measurement executor")
    .into_parts()
    .1;
    match std::env::var("PUSH_BENCH_FENCE").as_deref() {
        Ok("none") => {}
        Ok(fence) => executor = executor.with_commit_fence(fence.to_owned()),
        Err(_) => executor = executor.with_commit_fence(commit.to_string()),
    }
    eprintln!(
        "construction_wall_seconds={:.3}",
        started.elapsed().as_secs_f64()
    );
    let started = std::time::Instant::now();
    let result = executor
        .execute_push(GitPushArguments::for_test(&branch))
        .await;
    eprintln!(
        "preparation_wall_seconds={:.3} result={result:?}",
        started.elapsed().as_secs_f64()
    );
    assert!(
        matches!(
            result,
            Ok(_) | Err(GitPushFailure::Repository | GitPushFailure::PreDispatchInfrastructure)
        ),
        "measurement must resolve the branch and return a bounded preparation outcome"
    );
}

#[derive(Clone, Debug, Default)]
struct RecordingPushTransport(Arc<Mutex<Option<GitPushRequest>>>);

impl RecordingPushTransport {
    fn request(&self) -> GitPushRequest {
        self.0
            .lock()
            .expect("recording transport lock is available")
            .clone()
            .expect("push request was recorded")
    }

    fn has_request(&self) -> bool {
        self.0
            .lock()
            .expect("recording transport lock is available")
            .is_some()
    }
}

impl GitPushTransport for RecordingPushTransport {
    async fn push(
        &mut self,
        request: GitPushRequest,
    ) -> Result<GitPushReceipt, GitPushTransportFailure> {
        let receipt = GitPushReceipt::try_new(request.commit().to_owned())
            .expect("resolved commit forms a receipt");
        *self
            .0
            .lock()
            .expect("recording transport lock is available") = Some(request);
        Ok(receipt)
    }
}

#[test]
fn push_contract_requires_non_overridable_confirmation() {
    let fixture = Fixture::new();
    let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
        .expect("configured remote is admitted");
    let catalog = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        remote,
        RecordingPushTransport::default(),
    )
    .expect("push suite constructs")
    .into_parts()
    .0;
    let name =
        ToolName::try_new(GIT_PUSH_CONFIGURED_NAME.to_owned()).expect("fixture name is admitted");
    let definition = catalog.definition(&name).expect("push definition exists");

    assert_eq!(
        definition.permission_default(),
        ToolPermissionDefault::AlwaysConfirm
    );
    assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
}

#[test]
fn push_contract_rejects_a_model_supplied_destination() {
    let fixture = Fixture::new();
    let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
        .expect("configured remote is admitted");
    let catalog = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        remote,
        RecordingPushTransport::default(),
    )
    .expect("push suite constructs")
    .into_parts()
    .0;
    let name =
        ToolName::try_new(GIT_PUSH_CONFIGURED_NAME.to_owned()).expect("fixture name is admitted");
    let definition = catalog.definition(&name).expect("push definition exists");
    let schema: serde_json::Value =
        serde_json::from_str(definition.input_schema().as_str()).expect("push schema is JSON");
    let injected_destination = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"branch": FIX_BRANCH, "remote": REMOTE_URL}).to_string(),
    )
    .expect("provider JSON normalizes");

    assert_eq!(schema["required"], serde_json::json!(["branch"]));
    assert_eq!(
        schema["additionalProperties"],
        serde_json::Value::Bool(false)
    );
    assert!(decode_push(&injected_destination).is_err());
}

#[tokio::test]
async fn push_resolves_a_real_branch_for_only_the_configured_transport() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture push branch creates");
    let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
        .expect("configured remote is admitted");
    let transport = RecordingPushTransport::default();
    let mut executor = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        remote,
        transport.clone(),
    )
    .expect("push suite constructs")
    .into_parts()
    .1;

    let encoded = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("synthetic push succeeds");
    let result: serde_json::Value = serde_json::from_str(&encoded).expect("push result is JSON");
    let request = transport.request();

    assert_eq!(
        file_identity(
            &fs::metadata(request.repository_root())
                .expect("pinned transport repository root resolves"),
        ),
        file_identity(&fs::metadata(fixture.root()).expect("fixture root resolves")),
    );
    assert_eq!(request.remote().name(), REMOTE_NAME);
    assert_eq!(request.remote().url(), REMOTE_URL);
    assert_eq!(request.branch(), FIX_BRANCH);
    assert_eq!(request.commit(), fixture.initial.to_string());
    assert_eq!(
        request.refspec(),
        format!("{}:refs/heads/{FIX_BRANCH}", request.commit()),
    );
    assert_eq!(result["remote"], REMOTE_NAME);
    assert_eq!(result["branch"], request.branch());
    assert_eq!(result["commit"], fixture.initial.to_string());
}

#[tokio::test]
async fn push_rejects_a_replaced_workspace_before_transport_dispatch() {
    let fixture = Fixture::new();
    let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
        .expect("configured remote is admitted");
    let transport = RecordingPushTransport::default();
    let mut executor = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        remote,
        transport.clone(),
    )
    .expect("push suite constructs")
    .into_parts()
    .1;
    let retired = fixture.root().with_extension("retired");
    fs::rename(fixture.root(), &retired).expect("fixture workspace retires");
    Repository::init(fixture.root()).expect("replacement repository initializes");

    let failure = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect_err("replacement workspace rejects before dispatch");

    assert_eq!(failure, GitPushFailure::Repository);
    assert!(!transport.has_request());
    fs::remove_dir_all(fixture.root()).expect("replacement repository removes");
    fs::rename(retired, fixture.root()).expect("fixture workspace restores");
}

#[test]
fn push_receipts_reject_non_object_identifiers() {
    assert!(GitPushReceipt::try_new("not-an-object-id").is_err());
}

#[test]
fn push_receipts_canonicalize_equivalent_object_identifier_spelling() {
    let fixture = Fixture::new();
    let uppercase = fixture.initial.to_string().to_ascii_uppercase();

    let receipt = GitPushReceipt::try_new(uppercase).expect("uppercase object ID is admitted");

    assert_eq!(receipt.commit(), fixture.initial.to_string());
}

/// Guards the subset relation between the durable mint vocabulary and the
/// reference grammar this executor applies: a name the domain admits but
/// `gix_validate` refuses would mint a destination that can never resolve.
///
/// This test enforces the relation; it does not own it. The owning statement
/// is `docs/spec/git-authority-threat-model.md`, under "Remote destination
/// authority", which states the durable vocabulary and how a mint is scoped.
#[track_caller]
fn assert_minted_name_builds_a_configured_remote(candidate: &str) {
    assert!(
        signalbox_domain::GitRemoteName::try_new(candidate.to_owned()).is_ok(),
        "the domain refuses {candidate:?}, so this case no longer covers the subset rule"
    );
    assert!(
        ConfiguredGitRemote::try_new(candidate, "https://example.test/namespace/project.git")
            .is_ok(),
        "minted remote name {candidate:?} is refused by the push executor"
    );
}

#[test]
fn every_minted_remote_name_builds_a_configured_remote() {
    assert_minted_name_builds_a_configured_remote("origin");
    assert_minted_name_builds_a_configured_remote("up-stream_2");
    assert_minted_name_builds_a_configured_remote("v1.0");
    assert_minted_name_builds_a_configured_remote("origin.lockfile");
    assert_minted_name_builds_a_configured_remote("a");
}

/// Stores selected fixture objects without compression so pack size is load-bearing.
pub(super) fn plant_uncompressed_push_pack(
    repository: &Repository,
    objects: &[git2::Oid],
) -> std::path::PathBuf {
    plant_push_pack_with_unrelated_objects(repository, objects, 0..0)
}

fn plant_push_pack_with_unrelated_objects(
    repository: &Repository,
    objects: &[git2::Oid],
    unrelated: std::ops::Range<usize>,
) -> std::path::PathBuf {
    use flate2::{Compression, write::ZlibEncoder};
    use sha1::{Digest, Sha1};
    use std::io::Write;
    let database = repository.odb().expect("fixture objects");
    let mut pack = b"PACK\0\0\0\x02".to_vec();
    pack.extend_from_slice(&((objects.len() + unrelated.len()) as u32).to_be_bytes());
    let mut add = |kind: u8, content: &[u8]| {
        let mut size = content.len();
        let first = (kind << 4) | (size & 15) as u8;
        size >>= 4;
        pack.push(first | if size == 0 { 0 } else { 128 });
        while size != 0 {
            let byte = (size & 127) as u8;
            size >>= 7;
            pack.push(byte | if size == 0 { 0 } else { 128 });
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::none());
        encoder.write_all(content).expect("fixture object encodes");
        pack.extend(encoder.finish().expect("fixture stream finishes"));
    };
    for oid in objects {
        let object = database.read(*oid).expect("fixture object exists");
        add(object.kind() as u8, object.data());
    }
    for number in unrelated {
        add(git2::ObjectType::Blob as u8, &number.to_be_bytes());
    }
    let checksum = match repository.object_format() {
        git2::ObjectFormat::Sha1 => Sha1::digest(&pack).to_vec(),
        git2::ObjectFormat::Sha256 => sha2::Sha256::digest(&pack).to_vec(),
    };
    pack.extend_from_slice(&checksum);
    let directory = repository.path().join("objects/pack");
    let mut indexer = git2::Indexer::new_ext(
        Some(&database),
        &directory,
        0o600,
        true,
        repository.object_format(),
    )
    .expect("fixture indexer");
    indexer.write_all(&pack).expect("fixture pack indexes");
    let name = indexer.commit().expect("fixture pack publishes");
    for oid in objects {
        let hex = oid.to_string();
        fs::remove_file(
            repository
                .path()
                .join("objects")
                .join(&hex[..2])
                .join(&hex[2..]),
        )
        .expect("packed fixture object has no loose fallback");
    }
    directory.join(format!("pack-{name}.pack"))
}

#[test]
fn packed_commit_range_is_captured_before_the_preparation_deadline() {
    use crate::tests::support::commit_with_parents;
    use std::time::{Duration, Instant};

    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository");
    let mut commits = Vec::new();
    let mut tip = fixture.initial;
    for _ in 0..8000 {
        tip = commit_with_parents(&repository, &[tip], "packed history");
        commits.push(tip);
    }
    plant_push_pack_with_unrelated_objects(&repository, &commits, 0..0);
    let authority = fixture.executor().repository_authority;
    // Leave a generous minute for this bounded range, below the tool's five-minute deadline.
    let snapshot = crate::push_objects::PushObjectSnapshot::capture_before_deadline(
        &authority,
        tip,
        Some(fixture.initial),
        Instant::now() + Duration::from_secs(60),
    )
    .expect("packed history captures without a growing set of per-object packs");
    assert_eq!(
        snapshot
            .repository
            .find_commit(tip)
            .expect("captured tip")
            .id(),
        tip
    );
    assert!(
        snapshot
            .repository
            .odb()
            .expect("captured objects")
            .exists(fixture.initial)
    );
}

#[tokio::test]
async fn push_range_ignores_large_history_in_the_pack_containing_its_tip() {
    const LARGE_HISTORY_BYTES: usize = 200 * 1024 * 1024;
    use crate::tests::support::{AUTHOR_EMAIL, AUTHOR_NAME};
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository");
    let signature = git2::Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture author");
    let archive = repository
        .blob(&vec![b'x'; LARGE_HISTORY_BYTES + 1])
        .expect("large history blob");
    let mut archive_tree = repository.treebuilder(None).expect("archive tree");
    archive_tree
        .insert("archive", archive, 0o100644)
        .expect("archive entry");
    let archive_tree = repository
        .find_tree(archive_tree.write().expect("archive tree writes"))
        .expect("archive tree loads");
    let history = repository
        .commit(
            None,
            &signature,
            &signature,
            "large history",
            &archive_tree,
            &[],
        )
        .expect("historical commit");
    let tree = repository
        .find_commit(fixture.initial)
        .expect("small fixture commit")
        .tree()
        .expect("small tree");
    let fence = repository
        .commit(
            None,
            &signature,
            &signature,
            "dispatch head",
            &tree,
            &[&repository.find_commit(history).expect("history")],
        )
        .expect("fence");
    let tip = repository
        .commit(
            Some(&format!("refs/heads/{FIX_BRANCH}")),
            &signature,
            &signature,
            "session commit",
            &tree,
            &[&repository.find_commit(fence).expect("fence")],
        )
        .expect("tip");
    let pack = plant_uncompressed_push_pack(&repository, &[tip, archive]);
    assert!(fs::metadata(&pack).expect("large pack").len() > LARGE_HISTORY_BYTES as u64);
    let transport = RecordingPushTransport::default();
    let mut executor = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL).expect("remote"),
        transport.clone(),
    )
    .expect("push tools")
    .into_parts()
    .1
    .with_commit_fence(fence.to_string());
    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("small range pushes despite large shared pack");
    assert_eq!(transport.request().commit(), tip.to_string());
    let snapshot = crate::push_objects::PushObjectSnapshot::capture(
        &fixture.executor().repository_authority,
        tip,
        Some(fence),
    )
    .expect("range snapshot");
    let objects = snapshot.repository.odb().expect("snapshot objects");
    assert!(objects.exists(tip));
    assert!(objects.exists(fence));
    assert!(!objects.exists(history));
    assert!(!objects.exists(archive));
}

#[test]
fn merge_push_omits_history_shared_with_the_dispatch_fence() {
    use crate::tests::support::{AUTHOR_EMAIL, AUTHOR_NAME, commit_with_parents};

    // These labels distinguish graph nodes; their spelling is arbitrary.
    const ARCHIVE_BYTE: u8 = b'x';
    const ARCHIVE_PATH: &str = "archive";
    const HISTORY_LABEL: &str = "old archive";
    const SHARED_LABEL: &str = "shared history without archive";
    const FENCE_LABEL: &str = "dispatch fence";
    const SIDE_LABEL: &str = "base branch advances";
    const MERGE_LABEL: &str = "merge base into dispatched branch";

    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository");
    let signature = git2::Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture author");
    let archive = repository
        .blob(&vec![
            ARCHIVE_BYTE;
            crate::tests::support::TEST_OBJECT_BYTES + 1
        ])
        .expect("historical blob exceeds the selected-object ceiling");
    let mut builder = repository.treebuilder(None).expect("archive tree builder");
    builder
        .insert(ARCHIVE_PATH, archive, 0o100644)
        .expect("archive entry");
    let archive_tree = repository
        .find_tree(builder.write().expect("archive tree writes"))
        .expect("archive tree");
    let history = repository
        .commit(
            None,
            &signature,
            &signature,
            HISTORY_LABEL,
            &archive_tree,
            &[],
        )
        .expect("history commit");
    let retained_tree = repository
        .find_commit(fixture.initial)
        .expect("fixture commit")
        .tree()
        .expect("small retained tree");
    let shared = repository
        .commit(
            None,
            &signature,
            &signature,
            SHARED_LABEL,
            &retained_tree,
            &[&repository.find_commit(history).expect("history parent")],
        )
        .expect("shared ancestor removes the archive");
    let fence = commit_with_parents(&repository, &[shared], FENCE_LABEL);
    let side = commit_with_parents(&repository, &[shared], SIDE_LABEL);
    let merged = commit_with_parents(&repository, &[fence, side], MERGE_LABEL);

    let snapshot = crate::push_objects::PushObjectSnapshot::capture(
        &fixture.executor().repository_authority,
        merged,
        Some(fence),
    )
    .expect("a merge must not capture history already reachable from the dispatch fence");
    let objects = snapshot.repository.odb().expect("captured object database");
    assert!(
        objects.exists(merged),
        "the merge is part of the push range"
    );
    assert!(
        objects.exists(side),
        "the new side of the merge is retained"
    );
    assert!(
        objects.exists(fence),
        "the dispatch fence is retained for negotiation"
    );
    assert!(
        !objects.exists(history),
        "shared earlier history is omitted"
    );
    assert!(
        !objects.exists(archive),
        "historical blobs do not consume push allowance"
    );
    let shallow = fs::read_to_string(snapshot.repository.path().join("shallow"))
        .expect("captured merge has negotiation boundaries");
    assert_eq!(
        shallow
            .lines()
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([fence.to_string(), shared.to_string()]),
        "each retained boundary closes its omitted parent history"
    );
}

#[test]
fn merge_push_enforces_the_shallow_boundary_ceiling() {
    use crate::failure::LocalGitFailure;
    use crate::limits::{MAX_SHALLOW_BYTES, MAX_SHALLOW_ENTRIES};
    use crate::tests::support::commit_with_parents;

    // The child alone receives a descriptor ceiling equal to the admitted boundary count.
    const CHILD_ENVIRONMENT: &str = "SIGNALBOX_SHALLOW_BOUNDARY_DESCRIPTOR_CHILD";
    const CHILD_EVIDENCE: &str = "shallow boundary probe completed";
    if std::env::var_os(CHILD_ENVIRONMENT).is_none() {
        let output = std::process::Command::new("sh")
            .args(["-c", "ulimit -n \"$1\" && shift && exec \"$@\"", "sh"])
            .arg(MAX_SHALLOW_ENTRIES.to_string())
            .arg(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "tests::push::merge_push_enforces_the_shallow_boundary_ceiling",
                "--nocapture",
            ])
            .env(CHILD_ENVIRONMENT, "1")
            .output()
            .expect("run the bounded-descriptor child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains(CHILD_EVIDENCE),
            "bounded-descriptor probe must execute and pass: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    for boundary_count in [MAX_SHALLOW_ENTRIES, MAX_SHALLOW_ENTRIES + 1] {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository");
        let mut boundaries = vec![fixture.initial];
        while boundaries.len() < boundary_count {
            let parent = *boundaries.last().expect("initial boundary exists");
            // The label is arbitrary; parent identity distinguishes each commit.
            boundaries.push(commit_with_parents(&repository, &[parent], "ancestor"));
        }
        let fence = *boundaries.last().expect("dispatch fence exists");
        let merged = commit_with_parents(&repository, &boundaries, "merge");
        let result = crate::push_objects::PushObjectSnapshot::capture(
            &fixture.executor().repository_authority,
            merged,
            Some(fence),
        );
        if boundary_count == MAX_SHALLOW_ENTRIES {
            let snapshot = result.expect("the admitted number of boundaries captures");
            let shallow = fs::read_to_string(snapshot.repository.path().join("shallow"))
                .expect("captured negotiation boundaries");
            assert_eq!(shallow.lines().count(), boundary_count);
            assert!(shallow.len() <= MAX_SHALLOW_BYTES);
        } else {
            assert!(
                matches!(result, Err(LocalGitFailure::Repository)),
                "one boundary beyond the existing ceiling must reject"
            );
        }
    }
    println!("{CHILD_EVIDENCE}");
}

#[tokio::test]
async fn push_range_ignores_unrelated_objects_in_one_or_multiple_pack_indexes() {
    use crate::limits::MAX_REPOSITORY_INSPECTIONS;
    for split in [false, true] {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("fixture repository");
        let blob = repository.blob(b"new pushed content").expect("new blob");
        let tip = commit_with_push_blob(&fixture, blob);
        repository
            .reference(
                &format!("refs/heads/{FIX_BRANCH}"),
                tip,
                false,
                "session commit",
            )
            .expect("session branch");
        let tree = repository.find_commit(tip).expect("tip").tree_id();
        let count = MAX_REPOSITORY_INSPECTIONS + 1;
        let start = if split {
            plant_push_pack_with_unrelated_objects(&repository, &[], 0..count / 2);
            count / 2
        } else {
            0
        };
        plant_push_pack_with_unrelated_objects(&repository, &[tip, tree, blob], start..count);
        let transport = RecordingPushTransport::default();
        let mut executor = GitPushTools::try_new(
            &LocalWorkspaceFileSystem,
            fixture.root(),
            ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL).expect("remote"),
            transport.clone(),
        )
        .expect("push tools")
        .into_parts()
        .1
        .with_commit_fence(fixture.initial.to_string());
        executor
            .execute_push(GitPushArguments::for_test(FIX_BRANCH))
            .await
            .expect("unrelated index population does not consume the push-range budget");
        assert_eq!(transport.request().commit(), tip.to_string());
    }
}

#[test]
fn push_snapshot_rejects_an_index_hint_that_points_to_a_different_object() {
    use sha1::{Digest, Sha1};
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let selected = repository
        .blob(b"selected push content")
        .expect("selected blob");
    let other = repository.blob(b"unrelated content").expect("other blob");
    let tip = commit_with_push_blob(&fixture, selected);
    let pack = plant_uncompressed_push_pack(&repository, &[selected, other]);
    let index_path = pack.with_extension("idx");
    let mut index = fs::read(&index_path).expect("index");
    let ids_start = 8 + 256 * 4;
    let width = selected.as_bytes().len();
    let offsets_start = ids_start + 2 * (width + 4);
    let selected_position = usize::from(selected > other);
    let other_position = 1 - selected_position;
    let wrong_offset: [u8; 4] = index
        [offsets_start + other_position * 4..offsets_start + other_position * 4 + 4]
        .try_into()
        .expect("other offset");
    index[offsets_start + selected_position * 4..offsets_start + selected_position * 4 + 4]
        .copy_from_slice(&wrong_offset);
    let checksum_start = index.len() - width;
    let checksum = Sha1::digest(&index[..checksum_start]);
    index[checksum_start..].copy_from_slice(&checksum);
    fs::write(index_path, index).expect("misleading index");
    assert!(
        crate::push_objects::PushObjectSnapshot::capture(
            &fixture.executor().repository_authority,
            tip,
            Some(fixture.initial),
        )
        .is_err()
    );
}

/// Creates a commit referencing the chosen blob without decoding that blob.
fn commit_with_push_blob(fixture: &Fixture, blob: git2::Oid) -> git2::Oid {
    use crate::tests::support::{AUTHOR_EMAIL, AUTHOR_NAME};
    let repository = Repository::open(fixture.root()).expect("fixture repository");
    let mut tree = repository.treebuilder(None).expect("tree builder");
    tree.insert("selected", blob, 0o100644)
        .expect("selected blob");
    let tree = repository
        .find_tree(tree.write().expect("tree writes"))
        .expect("tree");
    let signature = git2::Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("author");
    repository
        .commit(
            None,
            &signature,
            &signature,
            "selected blob",
            &tree,
            &[&repository
                .find_commit(fixture.initial)
                .expect("initial commit")],
        )
        .expect("commit")
}

#[test]
fn push_snapshot_decodes_deltas_with_a_non_utf8_temporary_directory() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, process::Command};

    let directory = tempfile::tempdir().expect("temporary directory parent");
    let temporary = directory.path().join(OsStr::from_bytes(b"non-utf8-\xff"));
    fs::create_dir(&temporary).expect("non-UTF-8 temporary directory");
    let child_test = "tests::push::push_snapshot_decodes_bounded_offset_and_reference_delta_chains";
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", child_test, "--nocapture"])
        .env("TMPDIR", &temporary)
        .output()
        .expect("run delta capture with the non-UTF-8 temporary directory");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let completed = format!("test {child_test} ... ok");
    assert!(
        output.status.success() && stdout.lines().any(|line| line == completed),
        "the delta capture child must run and pass: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn push_snapshot_decodes_bounded_offset_and_reference_delta_chains() {
    use crate::tests::pack_reads::{DeltaEncoding, plant_delta_chain};
    for encoding in [DeltaEncoding::Offset, DeltaEncoding::Reference] {
        let fixture = Fixture::new();
        let blob = plant_delta_chain(
            fixture.root(),
            encoding,
            &[crate::tests::support::TEST_OBJECT_BYTES, 8, 1],
        );
        let tip = commit_with_push_blob(&fixture, blob);
        let snapshot = crate::push_objects::PushObjectSnapshot::capture(
            &fixture.executor().repository_authority,
            tip,
            Some(fixture.initial),
        )
        .expect("bounded selected dependencies");
        assert_eq!(
            snapshot
                .repository
                .find_blob(blob)
                .expect("selected blob")
                .content(),
            b"x"
        );
    }
}

#[test]
fn push_snapshot_rejects_oversized_selected_delta_dependencies() {
    use crate::tests::pack_reads::{DeltaEncoding, plant_delta_chain};
    for encoding in [DeltaEncoding::Offset, DeltaEncoding::Reference] {
        let fixture = Fixture::new();
        let blob = plant_delta_chain(
            fixture.root(),
            encoding,
            &[crate::tests::support::TEST_OBJECT_BYTES + 1, 8, 1],
        );
        let tip = commit_with_push_blob(&fixture, blob);
        assert!(
            crate::push_objects::PushObjectSnapshot::capture(
                &fixture.executor().repository_authority,
                tip,
                Some(fixture.initial)
            )
            .is_err()
        );
    }
}

#[test]
fn push_snapshot_excludes_unchanged_oversized_fence_blobs() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository");
    let blob = repository
        .blob(&vec![b'x'; crate::tests::support::TEST_OBJECT_BYTES + 1])
        .expect("existing large blob");
    let fence = commit_with_push_blob(&fixture, blob);
    let parent = repository.find_commit(fence).expect("fence commit");
    let tree = parent.tree().expect("fence tree");
    let mut builder = repository.treebuilder(Some(&tree)).expect("tree builder");
    let added = repository.blob(b"new content").expect("new blob");
    builder.insert("added", added, 0o100644).expect("new entry");
    let tree = repository
        .find_tree(builder.write().expect("new tree writes"))
        .expect("new tree");
    let signature = parent.author();
    let tip = repository
        .commit(
            None,
            &signature,
            &signature,
            "add a file",
            &tree,
            &[&parent],
        )
        .expect("child commit");
    let snapshot = crate::push_objects::PushObjectSnapshot::capture(
        &fixture.executor().repository_authority,
        tip,
        Some(fence),
    )
    .expect("changed tree does not decode unchanged fence blobs");
    assert_eq!(
        snapshot
            .repository
            .find_blob(added)
            .expect("selected new blob")
            .content(),
        b"new content"
    );
    assert!(
        !snapshot
            .repository
            .odb()
            .expect("snapshot objects")
            .exists(blob)
    );
    assert!(
        crate::push_objects::PushObjectSnapshot::capture(
            &fixture.executor().repository_authority,
            fence,
            Some(fixture.initial)
        )
        .is_err()
    );
}

#[test]
fn push_snapshot_retains_captured_content_after_a_live_object_changes() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository");
    let content = b"selected content";
    let blob = repository.blob(content).expect("selected blob");
    let tip = commit_with_push_blob(&fixture, blob);
    let snapshot = crate::push_objects::PushObjectSnapshot::capture(
        &fixture.executor().repository_authority,
        tip,
        Some(fixture.initial),
    )
    .expect("snapshot captures selected content");
    let hex = blob.to_string();
    let live_object = repository
        .path()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&live_object, fs::Permissions::from_mode(0o600))
        .expect("fixture object becomes writable");
    fs::write(live_object, []).expect("live object changes");
    assert_eq!(
        snapshot
            .repository
            .find_blob(blob)
            .expect("private snapshot blob")
            .content(),
        content
    );
}

#[test]
fn sha256_push_snapshot_retains_the_fence_without_its_history() {
    let fixture = crate::tests::support::Sha256Fixture::new();
    let repository = Repository::open(fixture.root()).expect("SHA-256 repository");
    let tree = repository
        .find_commit(fixture.initial)
        .expect("fence")
        .tree_id();
    plant_uncompressed_push_pack(&repository, &[fixture.initial, tree]);
    let snapshot = crate::push_objects::PushObjectSnapshot::capture(
        &fixture.executor().repository_authority,
        fixture.initial,
        Some(fixture.initial),
    )
    .expect("SHA-256 snapshot");
    assert_eq!(
        snapshot
            .repository
            .find_commit(fixture.initial)
            .expect("SHA-256 fence")
            .id(),
        fixture.initial
    );
}

/// Writes one-file commit trees without involving Git's conflict resolver.
fn merge_test_commit(repository: &Repository, content: &str, parents: &[git2::Oid]) -> git2::Oid {
    merge_test_commit_files(repository, &[(b"shared.txt", content)], parents)
}

/// Writes supplied tree-entry names as raw Git path bytes.
fn merge_test_commit_files(
    repository: &Repository,
    files: &[(&[u8], &str)],
    parents: &[git2::Oid],
) -> git2::Oid {
    let mut builder = repository.treebuilder(None).expect("tree builder");
    for (path, content) in files {
        let blob = repository.blob(content.as_bytes()).expect("blob");
        builder.insert(*path, blob, 0o100644).expect("file");
    }
    let tree_id = builder.write().expect("tree");
    merge_test_commit_tree(repository, tree_id, parents)
}

fn merge_test_commit_tree(
    repository: &Repository,
    tree_id: git2::Oid,
    parents: &[git2::Oid],
) -> git2::Oid {
    let tree = repository.find_tree(tree_id).expect("tree exists");
    let parents: Vec<_> = parents
        .iter()
        .map(|id| repository.find_commit(*id).expect("parent"))
        .collect();
    let parents: Vec<_> = parents.iter().collect();
    let signature = git2::Signature::now("Merge test", "merge@example.test").expect("signature");
    repository
        .commit(
            None,
            &signature,
            &signature,
            "merge fixture",
            &tree,
            &parents,
        )
        .expect("commit")
}

fn merge_test_executor(
    fixture: &Fixture,
    tip: git2::Oid,
    fence: git2::Oid,
    transport: RecordingPushTransport,
) -> crate::GitPushExecutor<RecordingPushTransport> {
    let repository = Repository::open(fixture.root()).expect("repository");
    repository
        .reference(&format!("refs/heads/{FIX_BRANCH}"), tip, true, "test tip")
        .expect("branch");
    GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL).expect("remote"),
        transport,
    )
    .expect("push tools")
    .into_parts()
    .1
    .with_commit_fence(fence.to_string())
}

#[tokio::test]
async fn push_refuses_branch_side_resolution_that_drops_base_changes() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "shared\n", &[]);
    let branch = merge_test_commit(&repository, "shared\nbranch\n", &[ancestor]);
    let base = merge_test_commit(&repository, "shared\nbase\n", &[ancestor]);
    let merge = merge_test_commit(&repository, "shared\nbranch\n", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            crate::push_merge::DroppedBaseChanges {
                file: "shared.txt".to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            }
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_accepts_conflict_resolution_preserving_both_sides() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "shared\n", &[]);
    let branch = merge_test_commit(&repository, "shared\nbranch\n", &[ancestor]);
    let base = merge_test_commit(&repository, "shared\nbase\n", &[ancestor]);
    let merge = merge_test_commit(&repository, "shared\nbase\nbranch\n", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("both sides push");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_accepts_a_plain_commit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "shared\n", &[]);
    let branch = merge_test_commit(&repository, "branch\n", &[ancestor]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, branch, ancestor, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("plain commit pushes");

    assert_eq!(transport.request().commit(), branch.to_string());
}

#[tokio::test]
async fn push_refuses_merges_above_the_parent_ceiling() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let branch = merge_test_commit(&repository, "shared\n", &[]);
    let parents = vec![branch; crate::limits::MAX_MERGE_PARENTS + 1];
    let merge = merge_test_commit(&repository, "shared\n", &parents);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::UnsupportedMergeShape {
            parents: parents.len()
        })
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_refuses_an_octopus_merge_preserving_all_three_parents() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[], &[]);
    let branch = merge_test_commit_files(&repository, &[(b"branch.txt", "branch\n")], &[ancestor]);
    let base = merge_test_commit_files(&repository, &[(b"base.txt", "base\n")], &[ancestor]);
    let other_base =
        merge_test_commit_files(&repository, &[(b"other.txt", "other\n")], &[ancestor]);
    let merge = merge_test_commit_files(
        &repository,
        &[
            (b"branch.txt", "branch\n"),
            (b"base.txt", "base\n"),
            (b"other.txt", "other\n"),
        ],
        &[branch, base, other_base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::UnsupportedMergeShape { parents: 3 })
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn merge_refusal_retains_only_the_first_dropped_hunk_per_file() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "shared\nseparator\nlast\n", &[]);
    let branch = merge_test_commit(&repository, "branch\nseparator\nbranch tail\n", &[ancestor]);
    let base = merge_test_commit(&repository, "base\nseparator\nbase tail\n", &[ancestor]);
    let merge = merge_test_commit(
        &repository,
        "branch\nseparator\nbranch tail\n",
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            crate::push_merge::DroppedBaseChanges {
                file: "shared.txt".to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            },
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_accepts_a_branch_rename_carrying_the_base_edit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[(b"old.txt", "shared\n")], &[]);
    let branch = merge_test_commit_files(&repository, &[(b"new.txt", "shared\n")], &[ancestor]);
    let base = merge_test_commit_files(&repository, &[(b"old.txt", "base\n")], &[ancestor]);
    let merge = merge_test_commit_files(&repository, &[(b"new.txt", "base\n")], &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("renamed base edit pushes");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_accepts_a_branch_rename_combining_both_parents_edits() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(
            b"new.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nbase\nbranch\n",
        )],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("both edits survive rename");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_refuses_a_branch_rename_that_drops_the_base_edit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            DroppedBaseChanges {
                file: "new.txt".to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            }
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn merge_refusal_distinguishes_non_utf8_paths_from_each_other_and_literal_escapes() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[
            (b"a\x80", "shared\n"),
            (b"a\x81", "shared\n"),
            (b"a\\200", "shared\n"),
        ],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[
            (b"a\x80", "branch\n"),
            (b"a\x81", "branch\n"),
            (b"a\\200", "branch\n"),
        ],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[
            (b"a\x80", "base\n"),
            (b"a\x81", "base\n"),
            (b"a\\200", "base\n"),
        ],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[
            (b"a\x80", "branch\n"),
            (b"a\x81", "branch\n"),
            (b"a\\200", "branch\n"),
        ],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            DroppedBaseChanges {
                file: r#""a\\200""#.to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            },
            DroppedBaseChanges {
                file: r#""a\200""#.to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            },
            DroppedBaseChanges {
                file: r#""a\201""#.to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            },
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_accepts_a_base_rename_carrying_the_branch_edit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[(b"old.txt", "shared\n")], &[]);
    let branch = merge_test_commit_files(&repository, &[(b"old.txt", "branch\n")], &[ancestor]);
    let base = merge_test_commit_files(&repository, &[(b"new.txt", "shared\n")], &[ancestor]);
    let merge = merge_test_commit_files(&repository, &[(b"new.txt", "branch\n")], &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("branch edit follows base rename");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_accepts_a_base_rename_combining_both_parents_edits() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(
            b"new.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nbase\nbranch\n",
        )],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("both edits survive rename");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_refuses_a_base_rename_that_drops_the_base_edit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            DroppedBaseChanges {
                file: "new.txt".to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            }
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_accepts_matching_parent_renames_combining_both_edits() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(
            b"new.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nbase\nbranch\n",
        )],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("both edits survive rename");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_accepts_divergent_parent_renames_combining_both_edits_at_the_base_path() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"branch.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(
            b"new.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nbase\nbranch\n",
        )],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("both edits survive rename");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_accepts_divergent_parent_renames_combining_both_edits_at_the_branch_path() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"branch.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(
            b"branch.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nbase\nbranch\n",
        )],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("both edits survive rename");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_refuses_divergent_parent_renames_dropping_base_content_at_the_branch_path() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"branch.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(b"branch.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            DroppedBaseChanges {
                file: "branch.txt".to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            }
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_refuses_divergent_parent_renames_to_an_unclaimed_destination() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"branch.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(
            b"third.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nbase\nbranch\n",
        )],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert!(matches!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(_))
    ));
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_refuses_divergent_parent_renames_that_drop_the_base_edit() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let branch = merge_test_commit_files(
        &repository,
        &[(b"branch.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let base = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n")],
        &[ancestor],
    );
    let merge = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            DroppedBaseChanges {
                file: "new.txt".to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            }
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_refuses_dropped_base_changes_when_the_base_is_the_first_parent() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "shared\n", &[]);
    let branch = merge_test_commit(&repository, "shared\nbranch\n", &[ancestor]);
    let base = merge_test_commit(&repository, "shared\nbase\n", &[ancestor]);
    let merge = merge_test_commit(&repository, "shared\nbranch\n", &[base, branch]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            DroppedBaseChanges {
                file: "shared.txt".to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned(),
                truncated: false,
            }
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_accepts_preserved_base_changes_when_the_base_is_the_first_parent() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "shared\n", &[]);
    let branch = merge_test_commit(&repository, "shared\nbranch\n", &[ancestor]);
    let base = merge_test_commit(&repository, "shared\nbase\n", &[ancestor]);
    let merge = merge_test_commit(&repository, "shared\nbase\nbranch\n", &[base, branch]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("reordered parents push");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_identifies_a_branch_parent_that_descends_from_the_retained_head() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "shared\n", &[]);
    let fence = merge_test_commit(&repository, "shared\nbranch\n", &[ancestor]);
    let branch = merge_test_commit(&repository, "shared\nbranch\nmore\n", &[fence]);
    let base = merge_test_commit(&repository, "shared\nbase\n", &[ancestor]);
    let merge = merge_test_commit(&repository, "shared\nbase\nbranch\nmore\n", &[base, branch]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, fence, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("branch descendant identified");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_refuses_parent_roles_when_both_parents_descend_from_the_fence() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let fence = merge_test_commit(&repository, "shared\n", &[]);
    let branch = merge_test_commit(&repository, "shared\nbranch\n", &[fence]);
    let base = merge_test_commit(&repository, "shared\nbase\n", &[fence]);
    let merge = merge_test_commit(&repository, "shared\nbase\nbranch\n", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, fence, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(result, Err(GitPushFailure::UnprovenMergeParents));
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_refuses_criss_cross_histories_and_names_both_merge_bases() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "shared\n", &[]);
    let first = merge_test_commit(&repository, "first\n", &[ancestor]);
    let second = merge_test_commit(&repository, "second\n", &[ancestor]);
    let branch = merge_test_commit(&repository, "first\n", &[second, first]);
    let base = merge_test_commit(&repository, "second\n", &[first, second]);
    let merge = merge_test_commit(&repository, "first\n", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());
    let mut bases = vec![first, second];
    bases.sort_unstable();

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(result, Err(GitPushFailure::AmbiguousMergeBases { bases }));
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_indexes_thousands_of_disjoint_parent_paths_before_matching() {
    use std::time::Duration;

    // Thousands of changed paths distinguish indexed matching from repeated full-diff scans.
    const FILES_PER_PARENT: usize = 1024;
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[], &[]);
    let branch_paths: Vec<_> = (0..FILES_PER_PARENT)
        .map(|index| format!("branch-{index:04}"))
        .collect();
    let base_paths: Vec<_> = (0..FILES_PER_PARENT)
        .map(|index| format!("base-{index:04}"))
        .collect();
    let branch_files: Vec<_> = branch_paths
        .iter()
        .map(|path| (path.as_bytes(), "branch\n"))
        .collect();
    let base_files: Vec<_> = base_paths
        .iter()
        .map(|path| (path.as_bytes(), "base\n"))
        .collect();
    let branch = merge_test_commit_files(&repository, &branch_files, &[ancestor]);
    let base = merge_test_commit_files(&repository, &base_files, &[ancestor]);
    let merged_files: Vec<_> = branch_files.into_iter().chain(base_files).collect();
    let merge = merge_test_commit_files(&repository, &merged_files, &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    // A generous bound for this fixture catches repeated scans before the 300-second preparation limit.
    tokio::time::timeout(
        Duration::from_secs(10),
        executor.execute_push(GitPushArguments::for_test(FIX_BRANCH)),
    )
    .await
    .expect("indexed path matching finishes promptly")
    .expect("disjoint changes push");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_accepts_combined_replacements_without_the_obsolete_branch_preimage() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "old\n", &[]);
    let branch = merge_test_commit(&repository, "branch\n", &[ancestor]);
    let base = merge_test_commit(&repository, "base\n", &[ancestor]);
    let merge = merge_test_commit(&repository, "base\nbranch\n", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("both replacements survive");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_accepts_the_remaining_branch_removal_after_a_base_replacement() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "old-a\nold-b\n", &[]);
    let branch = merge_test_commit(&repository, "branch\n", &[ancestor]);
    let base = merge_test_commit(&repository, "base\nold-b\n", &[ancestor]);
    let merge = merge_test_commit(&repository, "base\nbranch\n", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("remaining branch effect pushes");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_refuses_a_merge_that_repeats_a_branch_effect() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit(&repository, "old\n", &[]);
    let branch = merge_test_commit(&repository, "branch\n", &[ancestor]);
    let base = merge_test_commit(&repository, "base\n", &[ancestor]);
    let merge = merge_test_commit(&repository, "base\nbranch\nbranch\n", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(vec![
            DroppedBaseChanges {
                file: "shared.txt".to_owned(),
                first_dropped_hunk: "+branch\n+branch\n".to_owned(),
                truncated: false,
            }
        ]))
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn merge_refusal_bounds_collected_previews_across_thousands_of_paths() {
    use crate::push_executor::MAX_MERGE_DETAIL_BYTES;

    // Each path shares large blobs; repeated previews would exceed the detail budget many times over.
    const PATH_COUNT: usize = 1024;
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let paths: Vec<_> = (0..PATH_COUNT)
        .map(|index| format!("file-{index:04}"))
        .collect();
    let branch_content = format!("{}\n", "branch".repeat(MAX_MERGE_DETAIL_BYTES));
    let base_content = format!("{}\n", "base".repeat(MAX_MERGE_DETAIL_BYTES));
    let files = |content| {
        paths
            .iter()
            .map(|path| (path.as_bytes(), content))
            .collect::<Vec<_>>()
    };
    let ancestor = merge_test_commit_files(&repository, &files("old\n"), &[]);
    let branch = merge_test_commit_files(&repository, &files(&branch_content), &[ancestor]);
    let base = merge_test_commit_files(&repository, &files(&base_content), &[ancestor]);
    let merge = merge_test_commit_files(&repository, &files(&branch_content), &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    let Err(GitPushFailure::MergeDroppedBaseChanges(dropped)) = result else {
        panic!("expected dropped-base refusal, got {result:?}")
    };
    assert_eq!(
        dropped.iter().map(|entry| &entry.file).collect::<Vec<_>>(),
        paths.iter().collect::<Vec<_>>()
    );
    assert_eq!(
        dropped
            .iter()
            .map(|entry| entry.first_dropped_hunk.len())
            .sum::<usize>(),
        MAX_MERGE_DETAIL_BYTES
    );
    assert!(dropped.iter().all(|entry| entry.truncated));
    assert!(
        dropped
            .iter()
            .skip(1)
            .all(|entry| entry.first_dropped_hunk.is_empty())
    );
    assert!(!transport.has_request());
}

/// Keeps each tree object small while directory copies multiply the expanded path count.
const SHARED_SUBTREE_FILES: usize = 1024;

fn merge_test_repeated_blob_tree(repository: &Repository, directories: usize) -> git2::Oid {
    let blob = repository.blob(b"shared\n").expect("shared blob");
    let mut subtree = repository.treebuilder(None).expect("subtree builder");
    for index in 0..SHARED_SUBTREE_FILES {
        subtree
            .insert(format!("file-{index:04}"), blob, 0o100644)
            .expect("shared blob entry");
    }
    let subtree = subtree.write().expect("shared subtree");
    let mut root = repository.treebuilder(None).expect("root builder");
    for index in 0..directories {
        root.insert(format!("dir-{index:04}"), subtree, 0o040000)
            .expect("shared subtree entry");
    }
    root.write().expect("root tree")
}

#[tokio::test]
async fn push_refuses_shared_blob_paths_above_the_merge_entry_ceiling() {
    use crate::limits::MAX_REPOSITORY_INSPECTIONS;

    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[], &[]);
    let tree = merge_test_repeated_blob_tree(
        &repository,
        MAX_REPOSITORY_INSPECTIONS / SHARED_SUBTREE_FILES + 1,
    );
    let branch = merge_test_commit_tree(&repository, tree, &[ancestor]);
    let base = merge_test_commit_files(&repository, &[], &[ancestor]);
    let merge = merge_test_commit_tree(&repository, tree, &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());
    let authority = fixture.executor().repository_authority;
    crate::push_objects::PushObjectSnapshot::capture(&authority, merge, Some(branch))
        .expect("few shared objects fit the snapshot limits");

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(result, Err(GitPushFailure::Repository));
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_accepts_shared_blob_paths_within_the_merge_entry_ceiling() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[], &[]);
    let tree = merge_test_repeated_blob_tree(&repository, 2);
    let branch = merge_test_commit_tree(&repository, tree, &[ancestor]);
    let base = merge_test_commit_files(&repository, &[], &[ancestor]);
    let merge = merge_test_commit_tree(&repository, tree, &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("bounded shared paths push");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_rename_search_cannot_be_extended_by_repository_configuration() {
    use crate::push_merge::MAX_MERGE_RENAME_SOURCES;

    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    repository
        .config()
        .expect("configuration")
        .set_i64("diff.renamelimit", i64::from(i32::MAX))
        .expect("repository requests an unbounded rename search");
    let decoys: Vec<_> = (0..=MAX_MERGE_RENAME_SOURCES)
        .map(|index| format!("decoy-{index:03}.txt"))
        .collect();
    let mut ancestor_files: Vec<_> = decoys
        .iter()
        .map(|path| (path.as_bytes(), "unrelated\n"))
        .collect();
    ancestor_files.push((b"z-source.txt", "one\ntwo\nthree\nfour\nfive\nsix\n"));
    let ancestor = merge_test_commit_files(&repository, &ancestor_files, &[]);
    let branch = merge_test_commit_files(
        &repository,
        &[(b"renamed.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n")],
        &[ancestor],
    );
    let mut base_files = ancestor_files;
    *base_files.last_mut().expect("rename source") =
        (b"z-source.txt", "one\ntwo\nthree\nfour\nfive\nsix\nbase\n");
    let base = merge_test_commit_files(&repository, &base_files, &[ancestor]);
    let merge = merge_test_commit_files(
        &repository,
        &[(
            b"renamed.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nbase\nbranch\n",
        )],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert!(matches!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(_))
    ));
    assert!(!transport.has_request());
}

/// Long components amplify retained path prefixes without needing many tree entries.
fn merge_test_nested_tree(repository: &Repository, depth: usize) -> git2::Oid {
    let blob = repository.blob(b"shared\n").expect("leaf blob");
    let mut leaf = repository.treebuilder(None).expect("leaf builder");
    leaf.insert("file", blob, 0o100644).expect("leaf entry");
    let mut tree = leaf.write().expect("leaf tree");
    let component = "x".repeat(255);
    for _ in 0..depth {
        let mut directory = repository.treebuilder(None).expect("directory builder");
        directory
            .insert(component.as_str(), tree, 0o040000)
            .expect("nested tree entry");
        tree = directory.write().expect("nested tree");
    }
    tree
}

#[tokio::test]
async fn push_refuses_cumulative_expanded_path_bytes_above_the_merge_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[], &[]);
    // Two copies exceed 4 MiB of path prefixes while contributing only 258 entries.
    let tree = merge_test_nested_tree(&repository, 128);
    let branch = merge_test_commit_tree(&repository, tree, &[ancestor]);
    let base = merge_test_commit_files(&repository, &[], &[ancestor]);
    let merge = merge_test_commit_tree(&repository, tree, &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());
    let authority = fixture.executor().repository_authority;
    crate::push_objects::PushObjectSnapshot::capture(&authority, merge, Some(branch))
        .expect("nested objects fit the snapshot limits");

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(result, Err(GitPushFailure::Repository));
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_accepts_nested_paths_within_the_merge_path_byte_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[], &[]);
    let tree = merge_test_nested_tree(&repository, 2);
    let branch = merge_test_commit_tree(&repository, tree, &[ancestor]);
    let base = merge_test_commit_files(&repository, &[], &[ancestor]);
    let merge = merge_test_commit_tree(&repository, tree, &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("bounded nested paths push");

    assert_eq!(transport.request().commit(), merge.to_string());
}

#[tokio::test]
async fn push_accepts_a_renamed_branch_result_after_the_base_deletes_its_source() {
    for (case, branch_content) in [
        ("unchanged rename", "one\ntwo\nthree\nfour\nfive\nsix\n"),
        (
            "edited rename",
            "one\ntwo\nthree\nfour\nfive\nsix\nbranch\n",
        ),
    ] {
        let fixture = Fixture::new();
        let repository = Repository::open(fixture.root()).expect("repository");
        let ancestor = merge_test_commit_files(
            &repository,
            &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
            &[],
        );
        let branch =
            merge_test_commit_files(&repository, &[(b"new.txt", branch_content)], &[ancestor]);
        let base = merge_test_commit_files(&repository, &[], &[ancestor]);
        let merge = merge_test_commit_files(
            &repository,
            &[(b"new.txt", branch_content)],
            &[branch, base],
        );
        let transport = RecordingPushTransport::default();
        let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

        let result = executor
            .execute_push(GitPushArguments::for_test(FIX_BRANCH))
            .await;

        assert!(result.is_ok(), "{case}: {result:?}");
        assert_eq!(transport.request().commit(), merge.to_string(), "{case}");
    }
}

#[tokio::test]
async fn push_refuses_new_content_in_a_rename_delete_resolution() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[(b"old.txt", "shared\n")], &[]);
    let branch = merge_test_commit_files(&repository, &[(b"new.txt", "shared\n")], &[ancestor]);
    let base = merge_test_commit_files(&repository, &[], &[ancestor]);
    let merge = merge_test_commit_files(
        &repository,
        &[(b"new.txt", "shared\nunclaimed\n")],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert!(matches!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(_))
    ));
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_refuses_restoring_the_deleted_source_of_a_branch_rename() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[(b"old.txt", "shared\n")], &[]);
    let branch = merge_test_commit_files(&repository, &[(b"new.txt", "shared\n")], &[ancestor]);
    let base = merge_test_commit_files(&repository, &[], &[ancestor]);
    let merge = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "shared\n"), (b"new.txt", "shared\n")],
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert!(matches!(
        result,
        Err(GitPushFailure::MergeDroppedBaseChanges(_))
    ));
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_refuses_unsupported_merge_shape_before_an_oversized_parent_snapshot() {
    use crate::tests::support::TEST_OBJECT_BYTES as MAX_OBJECT_BYTES;

    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(&repository, &[], &[]);
    let branch = merge_test_commit_files(&repository, &[], &[ancestor]);
    let oversized_content = "x".repeat(MAX_OBJECT_BYTES + 1);
    let base = merge_test_commit_files(
        &repository,
        &[(b"oversized.txt", &oversized_content)],
        &[ancestor],
    );
    let other_base = merge_test_commit(&repository, "other\n", &[ancestor]);
    let merge = merge_test_commit_files(&repository, &[], &[branch, base, other_base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());
    let authority = fixture.executor().repository_authority;
    assert!(
        crate::push_objects::PushObjectSnapshot::capture(&authority, merge, Some(branch)).is_err(),
        "parent content exceeds the snapshot's object ceiling"
    );

    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;

    assert_eq!(
        result,
        Err(GitPushFailure::UnsupportedMergeShape { parents: 3 })
    );
    assert!(!transport.has_request());
}

#[tokio::test]
async fn merge_verification_rejects_a_dropped_nearly_disjoint_large_replacement() {
    use std::io::{Seek, Write};
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let commit = |prefix: &str, parents: &[git2::Oid]| {
        let mut file = std::io::BufWriter::new(tempfile::tempfile().expect("generated blob"));
        for line in 0..100_000 {
            if line == 50_000 {
                writeln!(file, "shared middle line").expect("shared line");
            } else {
                writeln!(file, "{prefix}-{line:08}").expect("distinct line");
            }
        }
        let mut file = file.into_inner().expect("flush");
        let size = file.metadata().expect("metadata").len() as usize;
        assert!(size > crate::limits::MAX_DIFF_BYTES);
        file.rewind().expect("rewind");
        let mut content = crate::streamed_object::ObjectContent {
            file,
            size,
            kind: git2::ObjectType::Blob,
        };
        let oid = content
            .store(
                &fixture.root().join(".git/objects"),
                git2::ObjectFormat::Sha1,
                None,
            )
            .expect("publish blob");
        let mut tree = repository.treebuilder(None).expect("tree");
        tree.insert("shared.txt", oid, 0o100644).expect("file");
        merge_test_commit_tree(&repository, tree.write().expect("tree id"), parents)
    };
    let ancestor = commit("old", &[]);
    let branch = commit("old", &[ancestor]);
    let base = commit("new", &[ancestor]);
    let preserved = commit("new", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, preserved, branch, transport.clone());
    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("base replacement survives");
    assert!(transport.has_request());
    let dropped = commit("old", &[branch, base]);
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, dropped, branch, transport.clone());
    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;
    let Err(GitPushFailure::MergeDroppedBaseChanges(details)) = result else {
        panic!("whole-object replacement must refuse: {result:?}");
    };
    assert_eq!(details.len(), 1);
    assert!(details[0].first_dropped_hunk.starts_with("object "));
    assert!(!transport.has_request());
}

#[tokio::test]
async fn merge_verification_streams_long_lines_and_rejects_dropped_base_content() {
    exercise_streamed_merge(256 * 1024).await;
}

#[tokio::test]
#[ignore = "generates four 1 GB text blobs and verifies preserved and dropped merge effects"]
async fn merge_verification_streams_generated_gigabyte_text_blobs() {
    exercise_streamed_merge(1_000_000_000).await;
}

async fn exercise_streamed_merge(bytes: u64) {
    use std::io::Read;
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let commit = |prefix: &str, suffix: &str, parents: &[git2::Oid]| {
        let mut reader = prefix
            .as_bytes()
            .chain(std::io::repeat(b'x').take(bytes))
            .chain(suffix.as_bytes());
        let mut content = crate::streamed_object::ObjectContent::decode(
            &mut reader,
            bytes as usize + prefix.len() + suffix.len(),
            git2::ObjectType::Blob,
            None,
        )
        .expect("streamed fixture blob");
        let oid = content
            .store(
                &fixture.root().join(".git/objects"),
                git2::ObjectFormat::Sha1,
                None,
            )
            .expect("fixture blob publication");
        let mut builder = repository.treebuilder(None).expect("tree builder");
        builder
            .insert("shared.txt", oid, 0o100644)
            .expect("large text entry");
        merge_test_commit_tree(&repository, builder.write().expect("tree"), parents)
    };
    let started = std::time::Instant::now();
    let ancestor = commit("shared\n", "ancestor\n", &[]);
    let branch = commit("shared\nbranch\n", "ancestor\n", &[ancestor]);
    let base = commit("shared\n", "base\n", &[ancestor]);
    let merge = commit("shared\nbranch\n", "base\n", &[branch, base]);
    eprintln!("merge scale elapsed {:?}", started.elapsed());
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());
    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("large base line and branch addition survive");
    assert_eq!(transport.request().commit(), merge.to_string());
    let branch_tree = repository.find_commit(branch).expect("branch").tree_id();
    let dropped = merge_test_commit_tree(&repository, branch_tree, &[branch, base]);
    eprintln!("merge scale elapsed {:?}", started.elapsed());
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, dropped, branch, transport.clone());
    let result = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await;
    let Err(GitPushFailure::MergeDroppedBaseChanges(details)) = result else {
        panic!("dropped large base line must refuse: {result:?}");
    };
    assert_eq!(details.len(), 1);
    assert!(details[0].truncated);
    assert!(details[0].first_dropped_hunk.len() <= crate::push_executor::MAX_MERGE_DETAIL_BYTES);
    assert!(!transport.has_request());
}

#[tokio::test]
async fn push_accepts_a_branch_rename_and_mode_change_carrying_both_edits() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("repository");
    let ancestor = merge_test_commit_files(
        &repository,
        &[(b"old.txt", "one\ntwo\nthree\nfour\nfive\nsix\n")],
        &[],
    );
    let commit = |path, content: &[u8], mode, parents: &[git2::Oid]| {
        let mut tree = repository.treebuilder(None).expect("tree builder");
        tree.insert(path, repository.blob(content).expect("blob"), mode)
            .expect("file mode");
        merge_test_commit_tree(&repository, tree.write().expect("tree"), parents)
    };
    let branch = commit(
        "new.txt",
        b"one\ntwo\nthree\nfour\nfive\nsix\nbranch\n",
        0o100755,
        &[ancestor],
    );
    let base = commit(
        "old.txt",
        b"one\ntwo\nthree\nfour\nfive\nsix\nbase\n",
        0o100644,
        &[ancestor],
    );
    let merge = commit(
        "new.txt",
        b"one\ntwo\nthree\nfour\nfive\nsix\nbase\nbranch\n",
        0o100755,
        &[branch, base],
    );
    let transport = RecordingPushTransport::default();
    let mut executor = merge_test_executor(&fixture, merge, branch, transport.clone());
    executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .await
        .expect("rename, mode, and both edits survive");
    assert_eq!(transport.request().commit(), merge.to_string());
}

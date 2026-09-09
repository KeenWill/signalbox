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

#[tokio::test]
async fn push_range_ignores_large_history_in_the_pack_containing_its_tip() {
    use crate::limits::MAX_OBJECT_DATABASE_BYTES;
    use crate::tests::support::{AUTHOR_EMAIL, AUTHOR_NAME};
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository");
    let signature = git2::Signature::now(AUTHOR_NAME, AUTHOR_EMAIL).expect("fixture author");
    let archive = repository
        .blob(&vec![b'x'; MAX_OBJECT_DATABASE_BYTES + 1])
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
    assert!(fs::metadata(&pack).expect("large pack").len() > MAX_OBJECT_DATABASE_BYTES as u64);
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
fn push_snapshot_decodes_bounded_offset_and_reference_delta_chains() {
    use crate::tests::pack_reads::{DeltaEncoding, plant_delta_chain};
    for encoding in [DeltaEncoding::Offset, DeltaEncoding::Reference] {
        let fixture = Fixture::new();
        let blob = plant_delta_chain(
            fixture.root(),
            encoding,
            &[crate::limits::MAX_OBJECT_BYTES, 8, 1],
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
            &[crate::limits::MAX_OBJECT_BYTES + 1, 8, 1],
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
        .blob(&vec![b'x'; crate::limits::MAX_OBJECT_BYTES + 1])
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
                first_dropped_hunk: "-base\n+branch\n".to_owned()
            },
            DroppedBaseChanges {
                file: r#""a\200""#.to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned()
            },
            DroppedBaseChanges {
                file: r#""a\201""#.to_owned(),
                first_dropped_hunk: "-base\n+branch\n".to_owned()
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

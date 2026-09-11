//! Object-database pinning and budget properties.

use std::fs;

use git2::{Odb, Repository};

use crate::arguments::{GitDiffArguments, GitLogArguments, LocalOperation};
use crate::failure::LocalGitFailure;
const LARGE_HISTORY_BYTES: usize = 200 * 1024 * 1024;
use crate::objects::{PackRoot, persist_objects};
use crate::pack_install::{OBJECT_PUBLICATION_LOCK, ObjectPublicationLock};
use crate::pinning::PinnedObjectDatabase;
use crate::tests::push::plant_uncompressed_push_pack;
use crate::tests::support::{Fixture, UNTRACKED_CONTENT, create_fifo, execute, plant_packed_blob};

#[test]
fn pinned_object_database_never_reopens_a_replacement_fifo() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let _pinned = PinnedObjectDatabase::capture(&executor.repository_authority)
        .expect("fixture object database pins");
    let object = fixture.initial.to_string();
    let object_path = fixture
        .root()
        .join(".git/objects")
        .join(&object[..2])
        .join(&object[2..]);
    fs::rename(&object_path, object_path.with_extension("pinned"))
        .expect("fixture object path retires");
    create_fifo(&object_path).expect("replacement object FIFO constructs");
    let failure = executor.repository_authority.repository();

    assert!(matches!(failure, Err(LocalGitFailure::Repository)));
}

#[test]
fn selected_packed_content_survives_a_live_pack_rewrite_and_validation_rejects_it() {
    let fixture = Fixture::new();
    let trusted = b"trusted-pack";
    let replacement = b"changed-pack";
    let source = plant_packed_blob(fixture.root(), trusted);
    let executor = fixture.executor();
    let shell = executor
        .repository_authority
        .open_repository_shell()
        .expect("fixture shell opens");
    shell
        .capture_objects_on_read(&executor.repository_authority)
        .expect("source binds");
    let oid = git2::Oid::hash_object(git2::ObjectType::Blob, trusted).expect("fixture ID hashes");
    let mut captured = shell.object_content(oid).expect("selected object captures");
    fs::write(&source, replacement).expect("fixture pack mutates in place");

    assert_eq!(
        captured
            .prefix(trusted.len())
            .expect("private content reads"),
        trusted
    );
    assert_eq!(
        shell.validate_selected_objects(&executor.repository_authority),
        Err(LocalGitFailure::Repository)
    );
    assert_eq!(fs::read(source).expect("mutated source reads"), replacement);
}

#[test]
fn object_publication_lock_serializes_installation() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let object_database = Odb::new().expect("fixture object database constructs");
    pinned_objects
        .add_to(&object_database)
        .expect("fixture objects attach");
    let _mempack = object_database
        .add_new_mempack_backend(1000)
        .expect("fixture memory pack attaches");
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");
    repository
        .set_odb(&object_database, &pinned_objects)
        .expect("fixture object database installs");
    let object = repository
        .blob(UNTRACKED_CONTENT.as_bytes())
        .expect("fixture memory object writes");
    let publication = ObjectPublicationLock::acquire(&pinned_objects)
        .expect("first publication locks budget and installation");
    let pack_entries_before = fs::read_dir(fixture.root().join(".git/objects/pack"))
        .expect("fixture pack directory reads")
        .count();

    let failure = persist_objects(
        &executor.repository_authority,
        &repository,
        &pinned_objects,
        &[PackRoot::Object(object)],
    )
    .expect_err("concurrent publication rejects before installation");
    let pack_entries_after = fs::read_dir(fixture.root().join(".git/objects/pack"))
        .expect("fixture pack directory rereads")
        .count();
    drop(publication);

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(pack_entries_after, pack_entries_before);
    assert!(
        !fixture
            .root()
            .join(".git/objects/pack")
            .join(OBJECT_PUBLICATION_LOCK)
            .exists()
    );
}

#[test]
fn git_reads_capture_selected_objects_from_a_pack_larger_than_the_snapshot_budget() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let archive = repository
        .blob(&vec![b'x'; LARGE_HISTORY_BYTES + 1])
        .expect("unrelated over-budget blob writes");
    let commit = repository
        .find_commit(fixture.initial)
        .expect("fixture commit");
    let tree = commit.tree().expect("fixture tree");
    let mut packed = vec![archive, commit.id(), tree.id()];
    packed.extend(tree.iter().map(|entry| entry.id()));
    let pack = plant_uncompressed_push_pack(&repository, &packed);
    assert!(fs::metadata(pack).expect("fixture pack").len() > LARGE_HISTORY_BYTES as u64);
    let executor = fixture.executor();

    let status = execute(&executor, LocalOperation::Status);
    let diff = execute(&executor, LocalOperation::Diff(GitDiffArguments::Worktree));
    let log = execute(
        &executor,
        LocalOperation::Log(GitLogArguments {
            revision: "HEAD".to_owned(),
            max_entries: 1,
        }),
    );

    assert!(
        status["entries"]
            .as_array()
            .expect("status entries")
            .is_empty()
    );
    assert_eq!(diff["patch"], "");
    assert_eq!(log["commits"][0]["commit"], fixture.initial.to_string());
}

#[cfg(target_os = "linux")]
#[test]
fn hundreds_of_publication_packs_read_under_a_small_descriptor_limit() {
    const CHILD: &str = "SIGNALBOX_TEST_MANY_PACKS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new("sh")
            .args(["-c", "ulimit -n 128 && exec \"$@\"", "many-packs-child"])
            .arg(std::env::current_exe().expect("test executable"))
            .args(["--exact", "tests::object_database::hundreds_of_publication_packs_read_under_a_small_descriptor_limit", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("descriptor-limited child starts");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fixture = Fixture::new();
    let mut objects = Vec::new();
    // 512 independently published pairs exceed the child's 128-descriptor limit.
    for batch in 0..512_u32 {
        let bytes = batch.to_be_bytes();
        let mut content = crate::streamed_object::ObjectContent::decode(
            &mut bytes.as_slice(),
            bytes.len(),
            git2::ObjectType::Blob,
            None,
        )
        .expect("batch content");
        let oid = content
            .oid(git2::ObjectFormat::Sha1)
            .expect("batch object id");
        let mut content = Some(content);
        crate::streamed_object::write_pack(
            &[oid],
            |_| Ok(content.take().expect("single-object batch")),
            git2::ObjectFormat::Sha1,
            &fixture.root().join(".git/objects/pack"),
            None,
        )
        .expect("batch pack publishes");
        objects.push((oid, bytes));
    }
    let executor = fixture.executor();
    let authority = &executor.repository_authority;
    let repository = authority.open_repository_shell().expect("private shell");
    repository
        .capture_objects_on_read(authority)
        .expect("many packs bind");
    for (oid, bytes) in objects {
        assert_eq!(
            repository
                .object_content(oid)
                .expect("selected pack reads")
                .prefix(bytes.len())
                .expect("content bytes"),
            bytes
        );
    }
    repository
        .validate_selected_objects(authority)
        .expect("all pack identities remain bound");
    execute(&executor, LocalOperation::Status);
}

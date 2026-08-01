//! Object-database pinning and budget properties.

use std::fs;

use git2::Odb;
use rustix::fs::{CWD, Mode, mkfifoat};

use crate::arguments::LocalOperation;
use crate::bounded::find_bounded_commit;
use crate::failure::LocalGitFailure;
use crate::limits::{MAX_OBJECT_BYTES, MAX_OBJECT_DATABASE_BYTES, MAX_PACK_FILE_BYTES};
use crate::objects::{PackRoot, persist_objects};
use crate::pack_install::{OBJECT_PUBLICATION_LOCK, ObjectPublicationLock};
use crate::pinning::{PinnedObjectDatabase, live_object_database_bytes};
use crate::tests::planting::{fill_live_object_database_to_budget, plant_sparse_pack};
use crate::tests::support::{Fixture, UNTRACKED_CONTENT};

#[test]
fn pinned_object_database_never_reopens_a_replacement_fifo() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let pinned = PinnedObjectDatabase::capture(&executor.repository_authority)
        .expect("fixture object database pins");
    let object = fixture.initial.to_string();
    let object_path = fixture
        .root()
        .join(".git/objects")
        .join(&object[..2])
        .join(&object[2..]);
    fs::rename(&object_path, object_path.with_extension("pinned"))
        .expect("fixture object path retires");
    mkfifoat(CWD, &object_path, Mode::RUSR | Mode::WUSR)
        .expect("replacement object FIFO constructs");
    let repository = executor
        .repository_authority
        .repository()
        .expect("pinned fixture repository opens");
    let object_database = Odb::new().expect("fixture object database constructs");
    pinned
        .add_to(&object_database)
        .expect("pinned objects attach");
    let _mempack = object_database
        .add_new_mempack_backend(1000)
        .expect("fixture memory object backend attaches");
    repository
        .set_odb(&object_database)
        .expect("fixture object database selects");

    let commit =
        find_bounded_commit(&repository, fixture.initial).expect("pinned commit remains readable");

    assert_eq!(commit.id(), fixture.initial);
}

#[test]
fn pinned_object_database_snapshots_mutable_pack_contents() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let name = "pack-fixture.pack";
    let source = fixture.root().join(".git/objects/pack").join(name);
    let trusted = b"trusted-pack";
    let replacement = b"changed-pack";
    fs::write(&source, trusted).expect("fixture pack writes");
    let pinned = PinnedObjectDatabase::capture(&executor.repository_authority)
        .expect("fixture object database snapshots");

    fs::write(&source, replacement).expect("fixture pack mutates in place");
    let snapshot = fs::read(pinned.directory.path().join("pack").join(name))
        .expect("private pack snapshot reads");

    assert_eq!(snapshot, trusted);
    assert_eq!(
        fs::read(source).expect("mutated source pack reads"),
        replacement
    );
}

#[test]
fn pinned_object_database_admits_one_pack_within_the_aggregate_budget() {
    let fixture = Fixture::new();
    let name = "pack-within-aggregate.pack";
    let pack_bytes = 65 * MAX_OBJECT_BYTES;
    plant_sparse_pack(fixture.root(), name, pack_bytes as u64);
    let executor = fixture.executor();

    let pinned = PinnedObjectDatabase::capture(&executor.repository_authority)
        .expect("single in-budget pack snapshots");
    let captured_bytes = fs::metadata(pinned.directory.path().join("pack").join(name))
        .expect("captured pack metadata reads")
        .len();

    assert_eq!(captured_bytes, pack_bytes as u64);
}

#[test]
fn oversized_pack_file_is_rejected_before_object_database_attachment() {
    let fixture = Fixture::new();
    plant_sparse_pack(
        fixture.root(),
        "oversized.pack",
        (MAX_PACK_FILE_BYTES + 1) as u64,
    );

    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("oversized captured pack rejects");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn aggregate_object_database_bytes_are_rejected_before_attachment() {
    let fixture = Fixture::new();
    plant_sparse_pack(
        fixture.root(),
        "aggregate-a.pack",
        (MAX_OBJECT_DATABASE_BYTES / 2) as u64,
    );
    plant_sparse_pack(
        fixture.root(),
        "aggregate-b.pack",
        (MAX_OBJECT_DATABASE_BYTES / 2) as u64,
    );

    let executor = fixture.executor();

    let failure = executor
        .execute_operation(LocalOperation::Status)
        .expect_err("aggregate captured object bytes reject");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_publication_rejects_growth_beyond_the_live_database_budget() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let persistent_object_database =
        Odb::new().expect("fixture persistent object database constructs");
    pinned_objects
        .add_to(&persistent_object_database)
        .expect("persistent fixture objects attach");
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
        .set_odb(&object_database)
        .expect("fixture object database installs");
    let object = repository
        .blob(UNTRACKED_CONTENT.as_bytes())
        .expect("fixture memory object writes");
    let live_bytes = live_object_database_bytes(&executor.repository_authority)
        .expect("fixture live object bytes measure");
    fill_live_object_database_to_budget(
        fixture.root(),
        MAX_OBJECT_DATABASE_BYTES as u64 - live_bytes,
    );
    let pack_entries_before = fs::read_dir(fixture.root().join(".git/objects/pack"))
        .expect("fixture pack directory reads")
        .count();

    let failure = persist_objects(
        &executor.repository_authority,
        &repository,
        &persistent_object_database,
        &object_database,
        &[PackRoot::Object(object)],
    )
    .expect_err("object database growth over budget rejects");
    let pack_entries_after = fs::read_dir(fixture.root().join(".git/objects/pack"))
        .expect("fixture pack directory rereads")
        .count();

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(pack_entries_after, pack_entries_before);
    assert_eq!(
        live_object_database_bytes(&executor.repository_authority)
            .expect("fixture bounded live object bytes remeasure"),
        MAX_OBJECT_DATABASE_BYTES as u64
    );
}

#[test]
fn object_publication_lock_serializes_budget_check_and_installation() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let pinned_objects =
        PinnedObjectDatabase::capture(&executor.repository_authority).expect("fixture objects pin");
    let persistent_object_database =
        Odb::new().expect("fixture persistent object database constructs");
    pinned_objects
        .add_to(&persistent_object_database)
        .expect("persistent fixture objects attach");
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
        .set_odb(&object_database)
        .expect("fixture object database installs");
    let object = repository
        .blob(UNTRACKED_CONTENT.as_bytes())
        .expect("fixture memory object writes");
    let publication = ObjectPublicationLock::acquire(&executor.repository_authority)
        .expect("first publication locks budget and installation");
    let pack_entries_before = fs::read_dir(fixture.root().join(".git/objects/pack"))
        .expect("fixture pack directory reads")
        .count();

    let failure = persist_objects(
        &executor.repository_authority,
        &repository,
        &persistent_object_database,
        &object_database,
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

//! Repository-layout scan properties.

use std::{fs, os::unix::fs::symlink};

use git2::{ObjectFormat, ObjectType};
use rustix::fs::{CWD, Mode, OFlags, openat};

use crate::failure::LocalGitFailure;
use crate::index_lock::IndexLock;
use crate::layout::{reject_administrative_symlinks, validate_repository_layout};
use crate::limits::MAX_OBJECT_BYTES;
use crate::pinning::{PinnedObjectDatabase, PinnedRepository, repository_filemode};
use crate::reference_read::resolve_pinned_reference_chain;
use crate::tests::support::{
    Fixture, Sha256Fixture, plant_loose_blob, plant_loose_blob_with_claimed_id, plant_packed_blob,
};

#[test]
fn administrative_scan_stays_on_the_pinned_directory_after_path_replacement() {
    let fixture = Fixture::new();
    let git_path = fixture.root().join(".git");
    let retired_git = fixture.root().join(".git.retired");
    let outside = tempfile::tempdir().expect("outside directory constructs");
    let outside_target = tempfile::tempdir().expect("outside symlink target constructs");
    symlink(outside_target.path(), outside.path().join("escape"))
        .expect("outside administrative symlink constructs");
    let git_directory = openat(
        CWD,
        &git_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture administrative directory pins");
    fs::rename(&git_path, &retired_git).expect("fixture administrative directory retires");
    symlink(outside.path(), &git_path).expect("replacement administrative symlink constructs");

    reject_administrative_symlinks(&git_directory)
        .expect("pinned original administrative directory validates");
    fs::remove_file(&git_path).expect("replacement administrative symlink removes");
    fs::rename(retired_git, git_path).expect("fixture administrative directory restores");

    assert!(outside.path().join("escape").is_symlink());
}

#[test]
fn repository_open_reads_config_from_the_pinned_administrative_directory() {
    let fixture = Fixture::new();
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let git_path = fixture.root().join(".git");
    let retired_git = fixture.root().join(".git.retired");
    let replacement_git = git_path.clone();

    let authority = PinnedRepository::open_with_hook(fixture.root(), expected, || {
        fs::rename(&git_path, &retired_git).expect("fixture administrative directory retires");
        fs::create_dir(&replacement_git).expect("replacement administrative directory constructs");
        fs::write(
            replacement_git.join("config"),
            "[include]\npath = /outside/config\n",
        )
        .expect("escaping replacement config writes");
    })
    .expect("config opens through the pinned administrative descriptor");

    drop(authority);
    fs::remove_dir_all(&replacement_git).expect("replacement administrative directory removes");
    fs::rename(retired_git, git_path).expect("fixture administrative directory restores");
}

#[test]
fn repository_config_rejects_a_utf8_bom_before_an_include_section() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        b"\xef\xbb\xbf[include]\npath = /outside/config\n",
    )
    .expect("BOM-prefixed fixture config writes");

    let failure =
        validate_repository_layout(fixture.root()).expect_err("BOM-prefixed include rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_config_rejects_a_tab_delimited_filter_section() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[filter\t\"demo\"]\nclean = /outside/filter\n",
    )
    .expect("tab-delimited fixture filter config writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("tab-delimited filter section rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_config_rejects_an_unsupported_reference_storage_extension() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 1\n[extensions]\nrefStorage = reftable\n",
    )
    .expect("unsupported reference storage config writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("unsupported reference storage extension rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_config_rejects_an_unsupported_format_version() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(&config_path, "[core]\nrepositoryformatversion = 2\n")
        .expect("future repository format config writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("future repository format version rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_config_rejects_sha256_under_format_version_zero() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 0\n[extensions]\nobjectformat = sha256\n",
    )
    .expect("mismatched object format config writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("SHA-256 under format version zero rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_config_rejects_duplicate_format_versions() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 0\nrepositoryformatversion = 1\n",
    )
    .expect("duplicate repository format config writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("duplicate repository format versions reject");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_open_parses_the_validated_config_snapshot() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(&config_path, "[core]\nfilemode = true\nbare = false\n")
        .expect("validated fixture config writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");

    let authority = PinnedRepository::open_with_hooks(
        fixture.root(),
        expected,
        || {},
        || {
            fs::write(&config_path, "[core]\nfilemode = false\nbare = false\n")
                .expect("live fixture config mutates in place");
        },
    )
    .expect("repository opens from the validated config snapshot");
    let repository = authority.repository().expect("fixture repository locks");
    let filemode = repository_filemode(&repository).expect("snapshot filemode reads");

    assert!(filemode);
}

#[test]
fn repository_shell_and_object_capture_reject_descendant_replacement() {
    let fixture = Fixture::new();
    let outside = Fixture::new();
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let objects = fixture.root().join(".git/objects");
    let retired_objects = fixture.root().join(".git/objects.retired");
    fs::rename(&objects, &retired_objects).expect("fixture objects retire");
    symlink(outside.root().join(".git/objects"), &objects)
        .expect("outside object database symlink constructs");

    let repository = authority
        .repository()
        .expect("private repository shell locks");
    let outside_lookup_failed = repository.find_commit(outside.initial).is_err();
    drop(repository);
    let capture_failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("replacement object database rejects capture");

    assert!(outside_lookup_failed);
    assert_eq!(capture_failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_a_compressed_loose_object_above_the_decoded_limit() {
    let fixture = Fixture::new();
    let content = vec![0_u8; MAX_OBJECT_BYTES + 1];
    let object_path = plant_loose_blob(fixture.root(), &content);
    let compressed_bytes = fs::metadata(object_path)
        .expect("oversized loose object metadata reads")
        .len();
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("oversized decoded loose object rejects capture");

    assert!(compressed_bytes < content.len() as u64);
    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_a_loose_object_stored_under_an_unrelated_id() {
    let fixture = Fixture::new();
    let claimed_id =
        git2::Oid::hash_object(ObjectType::Blob, b"claimed blob").expect("claimed blob hashes");
    plant_loose_blob_with_claimed_id(fixture.root(), b"actual blob", claimed_id);
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("mismatched loose object rejects capture");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_a_packed_object_above_the_decoded_limit() {
    let fixture = Fixture::new();
    let content = vec![0_u8; MAX_OBJECT_BYTES + 1];
    let pack_path = plant_packed_blob(fixture.root(), &content);
    let packed_bytes = fs::metadata(pack_path)
        .expect("oversized packed object metadata reads")
        .len();
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("oversized decoded packed object rejects capture");

    assert!(packed_bytes < content.len() as u64);
    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn repository_shell_never_parses_mutated_live_object_format() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 1\n[extensions]\nobjectformat = sha1\n",
    )
    .expect("validated SHA-1 fixture config writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");

    let authority = PinnedRepository::open_with_hooks(
        fixture.root(),
        expected,
        || {},
        || {
            fs::write(
                &config_path,
                "[core]\nrepositoryformatversion = 1\n[extensions]\nobjectformat = sha256\n",
            )
            .expect("live fixture object format mutates");
        },
    )
    .expect("repository shell opens from the SHA-1 snapshot");
    let repository = authority.repository().expect("fixture repository locks");

    assert_eq!(authority.object_format, ObjectFormat::Sha1);
    assert_eq!(repository.object_format(), ObjectFormat::Sha1);
}

#[test]
fn sha256_repository_preserves_references_shallow_ids_and_index_checksum() {
    let fixture = Sha256Fixture::new();
    fs::write(
        fixture.root().join(".git/shallow"),
        format!("{}\n", fixture.initial),
    )
    .expect("SHA-256 shallow boundary writes");
    let expected = validate_repository_layout(fixture.root()).expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("SHA-256 repository pins");
    let (_, head) =
        resolve_pinned_reference_chain(&authority, None).expect("SHA-256 reference chain resolves");
    let (index_lock, index) =
        IndexLock::acquire_for_repository(&authority).expect("SHA-256 index lock acquires");
    let first_entry = index.get(0).expect("SHA-256 fixture index has one entry");
    let first_entry_format = first_entry.id.object_format();
    index_lock.commit().expect("SHA-256 index publishes");
    let published_index =
        git2::Index::open_ext(&fixture.root().join(".git/index"), ObjectFormat::Sha256)
            .expect("published SHA-256 index checksum validates");

    assert_eq!(authority.object_format, ObjectFormat::Sha256);
    assert_eq!(head, Some(fixture.initial));
    assert_eq!(first_entry_format, ObjectFormat::Sha256);
    assert_eq!(published_index.len(), index.len());
}

//! Repository-layout scan properties.

use std::{
    ffi::OsStr,
    fs,
    os::{fd::OwnedFd, unix::fs::symlink},
};

use git2::{ObjectFormat, ObjectType};
use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat, symlinkat};

use crate::failure::LocalGitFailure;
use crate::index_lock::IndexLock;
use crate::layout::{
    reject_administrative_symlinks, validate_repository_layout,
    validate_shallow_file_at_with_test_hook,
};
use crate::limits::MAX_OBJECT_BYTES;
use crate::pinning::{PinnedObjectDatabase, PinnedRepository, repository_filemode};
use crate::reference_lock::ReferenceLock;
use crate::reference_read::resolve_pinned_reference_chain;
use crate::tests::support::{
    Fixture, Sha256Fixture, plant_loose_blob, plant_loose_blob_with_claimed_id, plant_packed_blob,
};

#[test]
fn repository_layout_rejects_a_missing_head() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root().join(".git/HEAD")).expect("fixture HEAD removes");

    let failure =
        validate_repository_layout(fixture.root()).expect_err("missing HEAD rejects admission");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_layout_rejects_a_malformed_head() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(".git/HEAD"), b"not a head\n").expect("malformed HEAD writes");

    let failure =
        validate_repository_layout(fixture.root()).expect_err("malformed HEAD rejects admission");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_layout_rejects_a_missing_refs_directory() {
    let fixture = Fixture::new();
    fs::remove_dir_all(fixture.root().join(".git/refs")).expect("fixture refs directory removes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("missing refs directory rejects admission");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_layout_rejects_a_regular_refs_file() {
    let fixture = Fixture::new();
    let refs_path = fixture.root().join(".git/refs");
    fs::remove_dir_all(&refs_path).expect("fixture refs directory removes");
    fs::write(&refs_path, b"not a reference directory").expect("regular refs file writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("regular refs file rejects admission");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn operation_guard_rejects_a_replaced_administrative_directory() {
    let fixture = Fixture::new();
    let git_path = fixture.root().join(".git");
    let retired_git_path = fixture.root().join(".git.retired");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let guard = authority
        .operation_guard()
        .expect("repository operation guard constructs");
    fs::rename(&git_path, &retired_git_path).expect("pinned administrative directory retires");
    let mut options = git2::RepositoryInitOptions::new();
    options.external_template(false).initial_head("main");
    git2::Repository::init_opts(fixture.root(), &options)
        .expect("replacement repository initializes");

    let failure = guard
        .validate_supported_layout()
        .expect_err("replacement administrative directory rejects operation");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(retired_git_path.exists());
    assert!(git_path.exists());
}

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
fn administrative_scan_rejects_a_symlink_without_retaining_a_deep_path() {
    let fixture = Fixture::new();
    let git_directory = openat(
        CWD,
        fixture.root().join(".git"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture administrative directory pins");
    plant_deep_administrative_symlink(git_directory, 256);

    let failure = validate_repository_layout(fixture.root())
        .expect_err("deep administrative symlink rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

fn plant_deep_administrative_symlink(parent: OwnedFd, remaining: usize) {
    if remaining == 0 {
        symlinkat("/outside", &parent, "escape").expect("deep administrative symlink constructs");
        return;
    }
    let component = format!("d{remaining:03}-{}", "x".repeat(200));
    mkdirat(&parent, &component, Mode::RUSR | Mode::WUSR | Mode::XUSR)
        .expect("deep administrative directory constructs");
    let child = openat(
        &parent,
        &component,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("deep administrative directory pins");
    plant_deep_administrative_symlink(child, remaining - 1);
}

#[test]
fn repository_open_rejects_an_administrative_directory_replaced_after_open() {
    let fixture = Fixture::new();
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let git_path = fixture.root().join(".git");
    let retired_git = fixture.root().join(".git.retired");
    let replacement_git = git_path.clone();

    let failure = PinnedRepository::open_with_hook(fixture.root(), expected, || {
        fs::rename(&git_path, &retired_git).expect("fixture administrative directory retires");
        fs::create_dir(&replacement_git).expect("replacement administrative directory constructs");
        fs::write(
            replacement_git.join("config"),
            "[include]\npath = /outside/config\n",
        )
        .expect("escaping replacement config writes");
    })
    .expect_err("replaced administrative directory rejects repository open");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
    fs::remove_dir_all(&replacement_git).expect("replacement administrative directory removes");
    fs::rename(retired_git, git_path).expect("fixture administrative directory restores");
}

#[test]
fn repository_open_rejects_a_symlinked_root_path() {
    let fixture = Fixture::new();
    let root = fixture.root().to_path_buf();
    let retired_root = root.parent().expect("fixture parent exists").join(format!(
        "{}.retired",
        root.file_name()
            .expect("fixture root names")
            .to_string_lossy()
    ));
    let expected = validate_repository_layout(&root).expect("fixture layout validates");
    fs::rename(&root, &retired_root).expect("fixture root retires");
    symlink(&retired_root, &root).expect("fixture root symlink constructs");

    let failure = PinnedRepository::open(&root, expected)
        .expect_err("symlinked repository root rejects authority open");

    fs::remove_file(&root).expect("fixture root symlink removes");
    fs::rename(&retired_root, &root).expect("fixture root restores");
    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_open_rejects_a_symlinked_administrative_path() {
    let fixture = Fixture::new();
    let git_path = fixture.root().join(".git");
    let retired_git_path = fixture.root().join(".git.retired");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    fs::rename(&git_path, &retired_git_path).expect("fixture administrative directory retires");
    symlink(".git.retired", &git_path).expect("administrative symlink constructs");

    let failure = PinnedRepository::open(fixture.root(), expected)
        .expect_err("symlinked administrative directory rejects authority open");

    fs::remove_file(&git_path).expect("administrative symlink removes");
    fs::rename(&retired_git_path, &git_path).expect("administrative directory restores");
    assert_eq!(failure.to_string(), "local Git tool construction failed");
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
fn repository_config_rejects_a_bare_repository() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 0\nbare = true\n",
    )
    .expect("bare repository config writes");

    let failure = validate_repository_layout(fixture.root()).expect_err("bare repository rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_config_rejects_duplicate_bare_declarations() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 0\nbare = false\nbare = false\n",
    )
    .expect("duplicate bare config writes");

    let failure =
        validate_repository_layout(fixture.root()).expect_err("duplicate bare declarations reject");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_config_rejects_a_valueless_hooks_path() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 0\nbare = false\nhooksPath\n",
    )
    .expect("valueless hooks path config writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("valueless hooks path rejects repository");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_config_rejects_a_valueless_worktree() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 0\nbare = false\nworktree\n",
    )
    .expect("valueless worktree config writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("valueless worktree rejects repository");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn authority_operation_rejects_live_config_bytes_changed_after_open() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    fs::write(&config_path, "[core]\nfilemode = false\nbare = false\n")
        .expect("live config rewrites in place");

    let failure = ReferenceLock::acquire(&authority, "refs/heads/topic")
        .err()
        .expect("changed live config rejects authority operation");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!lock_path.exists());
}

#[test]
fn authority_operation_rejects_identical_replacement_config_after_open() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    let retired_config_path = fixture.root().join(".git/config.retired");
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let original_config = fs::read(&config_path).expect("fixture config reads");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    fs::rename(&config_path, retired_config_path).expect("fixture config retires");
    fs::write(&config_path, &original_config).expect("identical replacement config writes");

    let failure = ReferenceLock::acquire(&authority, "refs/heads/topic")
        .err()
        .expect("replacement live config rejects authority operation");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!lock_path.exists());
}

#[test]
fn repository_layout_accepts_an_empty_shallow_file() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(".git/shallow"), b"").expect("empty shallow file writes");

    validate_repository_layout(fixture.root()).expect("empty shallow file validates");
}

#[test]
fn repository_layout_rejects_a_leading_blank_shallow_record() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/shallow"),
        format!("\n{}\n", fixture.initial),
    )
    .expect("leading blank shallow record writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("leading blank shallow record rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_layout_rejects_a_doubled_trailing_shallow_newline() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/shallow"),
        format!("{}\n\n", fixture.initial),
    )
    .expect("doubled trailing shallow newline writes");

    let failure = validate_repository_layout(fixture.root())
        .expect_err("doubled trailing shallow newline rejects");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn repository_open_rejects_commondir_created_after_layout_validation() {
    let fixture = Fixture::new();
    let commondir_path = fixture.root().join(".git/commondir");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");

    let failure = PinnedRepository::open_with_hook(fixture.root(), expected, || {
        fs::write(&commondir_path, "../outside\n").expect("late commondir writes");
    })
    .expect_err("late commondir rejects repository open");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn object_capture_rejects_alternates_created_after_authority_open() {
    let fixture = Fixture::new();
    let alternates_path = fixture.root().join(".git/objects/info/alternates");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    fs::create_dir_all(alternates_path.parent().expect("alternates parent exists"))
        .expect("object info directory constructs");
    fs::write(&alternates_path, "/outside/objects\n").expect("late alternates writes");

    let failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("late alternates reject object capture");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn reference_operation_rejects_alternates_created_after_authority_open() {
    let fixture = Fixture::new();
    let alternates_path = fixture.root().join(".git/objects/info/alternates");
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    fs::create_dir_all(alternates_path.parent().expect("alternates parent exists"))
        .expect("object info directory constructs");
    fs::write(&alternates_path, "/outside/objects\n").expect("late alternates writes");

    let failure = ReferenceLock::acquire(&authority, "refs/heads/topic")
        .err()
        .expect("late alternates reject reference operation");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!lock_path.exists());
}

#[test]
fn shallow_validation_rejects_a_path_replaced_after_snapshot() {
    let fixture = Fixture::new();
    let shallow_path = fixture.root().join(".git/shallow");
    let retired_shallow_path = fixture.root().join(".git/shallow.retired");
    let valid_shallow = format!("{}\n", fixture.initial);
    let replacement_shallow = format!("\n{}\n", fixture.initial);
    fs::write(&shallow_path, &valid_shallow).expect("valid shallow file writes");
    let git_directory = openat(
        CWD,
        fixture.root().join(".git"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture administrative directory opens");
    let shallow_descriptor = openat(
        &git_directory,
        "shallow",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture shallow file opens");
    let mut shallow_file = fs::File::from(shallow_descriptor);

    let failure = validate_shallow_file_at_with_test_hook(
        &git_directory,
        OsStr::new("shallow"),
        &mut shallow_file,
        ObjectFormat::Sha1,
        || {
            fs::rename(&shallow_path, &retired_shallow_path)
                .expect("validated shallow file retires");
            fs::write(&shallow_path, &replacement_shallow)
                .expect("malformed replacement shallow file writes");
        },
    )
    .expect_err("replaced shallow pathname rejects validation");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
    assert_eq!(
        fs::read_to_string(shallow_path).expect("replacement shallow file reads"),
        replacement_shallow
    );
}

#[test]
fn repository_open_parses_the_validated_config_snapshot() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(&config_path, "[core]\nfilemode = true\nbare = false\n")
        .expect("validated fixture config writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");

    let authority = PinnedRepository::open(fixture.root(), expected)
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

    let shell_failure = authority
        .repository()
        .err()
        .expect("replacement object database rejects repository shell");
    let capture_failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("replacement object database rejects capture");

    assert_eq!(shell_failure, LocalGitFailure::Repository);
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
fn repository_open_rejects_live_object_format_changed_after_snapshot() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(
        &config_path,
        "[core]\nrepositoryformatversion = 1\n[extensions]\nobjectformat = sha1\n",
    )
    .expect("validated SHA-1 fixture config writes");
    let expected = validate_repository_layout(fixture.root()).expect("fixture layout validates");

    let failure = PinnedRepository::open_with_hooks(
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
    .expect_err("mutated live object format rejects repository open");

    assert_eq!(failure.to_string(), "local Git tool construction failed");
}

#[test]
fn sha256_repository_admits_the_declared_object_format() {
    let fixture = Sha256Fixture::new();
    let expected = validate_repository_layout(fixture.root()).expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("SHA-256 repository pins");

    assert_eq!(authority.object_format, ObjectFormat::Sha256);
}

#[test]
fn sha256_repository_accepts_a_sha256_shallow_boundary() {
    let fixture = Sha256Fixture::new();
    fs::write(
        fixture.root().join(".git/shallow"),
        format!("{}\n", fixture.initial),
    )
    .expect("SHA-256 shallow boundary writes");

    validate_repository_layout(fixture.root()).expect("SHA-256 shallow boundary validates");
}

#[test]
fn sha256_repository_resolves_its_symbolic_head() {
    let fixture = Sha256Fixture::new();
    let expected = validate_repository_layout(fixture.root()).expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("SHA-256 repository pins");
    let (_, head) =
        resolve_pinned_reference_chain(&authority, None).expect("SHA-256 reference chain resolves");

    assert_eq!(head, Some(fixture.initial));
}

#[test]
fn sha256_index_entries_retain_sha256_object_ids() {
    let fixture = Sha256Fixture::new();
    let expected = validate_repository_layout(fixture.root()).expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("SHA-256 repository pins");
    let (_, index) =
        IndexLock::acquire_for_repository(&authority).expect("SHA-256 index lock acquires");
    let first_entry = index.get(0).expect("SHA-256 fixture index has one entry");

    assert_eq!(first_entry.id.object_format(), ObjectFormat::Sha256);
}

#[test]
fn sha256_index_publication_writes_a_sha256_checksum() {
    let fixture = Sha256Fixture::new();
    let expected = validate_repository_layout(fixture.root()).expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("SHA-256 repository pins");
    let (index_lock, index) =
        IndexLock::acquire_for_repository(&authority).expect("SHA-256 index lock acquires");
    index_lock.commit().expect("SHA-256 index publishes");
    let published_index =
        git2::Index::open_ext(&fixture.root().join(".git/index"), ObjectFormat::Sha256)
            .expect("published SHA-256 index checksum validates");

    assert_eq!(published_index.len(), index.len());
}

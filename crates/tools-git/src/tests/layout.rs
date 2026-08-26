//! Repository-layout scan properties.

use std::{
    cell::Cell,
    ffi::{OsStr, OsString},
    fs,
    os::{
        fd::{AsFd, OwnedFd},
        unix::ffi::OsStringExt,
        unix::fs::symlink,
    },
    path::Path,
};

use git2::{ObjectFormat, ObjectType};
use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, mkdirat, openat, statat, symlinkat};
use sha1::{Digest, Sha1};
use signalbox_tools_workspace::{LocalWorkspaceFileSystem, WorkspaceRoot};

use crate::construction::LocalGitToolsConstructionError;
use crate::descriptor::unsupported_object_alternates_are_absent_with_test_hook;
use crate::failure::LocalGitFailure;
use crate::index_lock::IndexLock;
use crate::layout::{
    reject_administrative_symlinks, reject_administrative_symlinks_with_test_observer,
    validate_repository_layout, validate_shallow_file_at_with_test_hook,
};

use crate::limits::{MAX_BRANCH_BYTES, MAX_OBJECT_BYTES, MAX_REFERENCE_BYTES};
use crate::pinning::{
    PinnedObjectDatabase, PinnedRepository, live_object_database_bytes_with_test_hook,
    repository_filemode, validate_pack_file,
};
use crate::reference_lock::ReferenceLock;
use crate::reference_read::resolve_pinned_reference_chain;
use crate::tests::support::{
    Fixture, Sha256Fixture, TRACKED_PATH, commit_all, plant_loose_blob,
    plant_loose_blob_with_claimed_id, plant_packed_blob, real_git_packed_replacement_reference,
    workspace_root_identity,
};

// numeric-bound: test fixture - exceeds the dogfood supervisor's former descriptor ceiling
const WIDE_ADMINISTRATIVE_SIBLING_COUNT: usize = 1_100;

fn wide_administrative_layout() -> Fixture {
    let fixture = Fixture::new();
    let worktrees = fixture.root().join(".git/worktrees");
    fs::create_dir(&worktrees).expect("wide administration root constructs");
    for sibling in 0..WIDE_ADMINISTRATIVE_SIBLING_COUNT {
        fs::create_dir(worktrees.join(format!("worktree-{sibling}")))
            .expect("wide administrative sibling constructs");
    }
    fixture
}

#[track_caller]
fn assert_repository_construction_failure(failure: LocalGitToolsConstructionError) {
    assert!(matches!(
        failure,
        LocalGitToolsConstructionError::Repository
    ));
}

#[test]
fn repository_layout_rejects_a_missing_head() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root().join(".git/HEAD")).expect("fixture HEAD removes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("missing HEAD rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_layout_rejects_a_malformed_head() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(".git/HEAD"), b"not a head\n").expect("malformed HEAD writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("malformed HEAD rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_layout_rejects_a_non_utf8_symbolic_head() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/HEAD"),
        b"ref: refs/heads/nonutf-\xff\n",
    )
    .expect("non-UTF-8 symbolic HEAD writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("non-UTF-8 symbolic HEAD rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_layout_rejects_a_non_utf8_loose_reference_name() {
    let fixture = Fixture::new();
    let name = OsString::from_vec(b"nonutf-\xff".to_vec());
    fs::write(
        fixture.root().join(".git/refs/heads").join(name),
        format!("{}\n", fixture.initial),
    )
    .expect("non-UTF-8 loose reference writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("non-UTF-8 loose reference rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_layout_rejects_an_abbreviated_detached_head() {
    let fixture = Fixture::new();
    fs::write(fixture.root().join(".git/HEAD"), "abc123\n")
        .expect("abbreviated detached HEAD writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("abbreviated detached HEAD rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_layout_rejects_an_oversized_symbolic_head_target() {
    let fixture = Fixture::new();
    let target = format!("refs/heads/{}", "x".repeat(MAX_REFERENCE_BYTES));
    fs::write(fixture.root().join(".git/HEAD"), format!("ref: {target}\n"))
        .expect("oversized symbolic HEAD writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("oversized symbolic HEAD rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_layout_accepts_a_symbolic_head_with_a_maximum_branch_input() {
    let fixture = Fixture::new();
    let target = format!("refs/heads/{}", "x".repeat(MAX_BRANCH_BYTES));
    fs::write(fixture.root().join(".git/HEAD"), format!("ref: {target}\n"))
        .expect("maximum symbolic HEAD writes");

    validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
        .expect("maximum symbolic HEAD admits repository");
}

#[test]
fn repository_layout_rejects_a_missing_refs_directory() {
    let fixture = Fixture::new();
    fs::remove_dir_all(fixture.root().join(".git/refs")).expect("fixture refs directory removes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("missing refs directory rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_layout_rejects_a_regular_refs_file() {
    let fixture = Fixture::new();
    let refs_path = fixture.root().join(".git/refs");
    fs::remove_dir_all(&refs_path).expect("fixture refs directory removes");
    fs::write(&refs_path, b"not a reference directory").expect("regular refs file writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("regular refs file rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn operation_guard_rejects_a_replaced_administrative_directory() {
    let fixture = Fixture::new();
    let git_path = fixture.root().join(".git");
    let retired_git_path = fixture.root().join(".git.retired");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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

    reject_administrative_symlinks(&git_directory, ObjectFormat::Sha1)
        .expect("pinned original administrative directory validates");
    let replacement_git_directory = openat(
        CWD,
        outside.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("replacement administrative directory opens");
    let replacement_failure =
        reject_administrative_symlinks(&replacement_git_directory, ObjectFormat::Sha1)
            .expect_err("replacement administrative directory rejects its symlink");
    fs::remove_file(&git_path).expect("replacement administrative symlink removes");
    fs::rename(retired_git, git_path).expect("fixture administrative directory restores");

    assert_repository_construction_failure(replacement_failure);
}

#[test]
fn administrative_scan_descriptor_retention_follows_depth_not_sibling_width() {
    let fixture = wide_administrative_layout();
    let git_directory = openat(
        CWD,
        fixture.root().join(".git"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("wide administrative directory opens");
    let retained_depth = Cell::new(0_usize);

    reject_administrative_symlinks_with_test_observer(
        &git_directory,
        ObjectFormat::Sha1,
        |depth| retained_depth.set(retained_depth.get().max(depth)),
    )
    .expect("wide administrative directory validates");

    assert!(retained_depth.get() < WIDE_ADMINISTRATIVE_SIBLING_COUNT);
}

#[test]
fn repository_shell_never_binds_a_path_resolved_worktree() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let repository = authority.repository().expect("fixture repository locks");
    let retired_root = fixture.root().with_extension("retired");
    let replacement_bytes = b"actor replacement root";
    fs::rename(fixture.root(), &retired_root).expect("fixture root retires");
    fs::create_dir(fixture.root()).expect("replacement root constructs");
    fs::write(fixture.root().join("actor"), replacement_bytes)
        .expect("replacement root marker writes");

    repository
        .blob(b"descriptor-independent object")
        .expect("bare repository shell writes an object");

    assert!(repository.workdir().is_none());
    assert_eq!(
        fs::read(fixture.root().join("actor")).expect("replacement root marker reads"),
        replacement_bytes
    );
    assert!(!fixture.root().join(".git").exists());
    fs::remove_dir_all(fixture.root()).expect("replacement root removes");
    fs::rename(retired_root, fixture.root()).expect("fixture root restores");
}

#[test]
fn operation_guard_rejects_a_valid_repository_replacing_the_root_path() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let guard = authority
        .operation_guard()
        .expect("repository operation guard constructs");
    let retired_root = fixture.root().with_extension("retired");
    fs::rename(fixture.root(), &retired_root).expect("fixture root retires");
    let mut options = git2::RepositoryInitOptions::new();
    options.external_template(false).initial_head("main");
    git2::Repository::init_opts(fixture.root(), &options)
        .expect("replacement repository initializes");

    let failure = guard
        .validate_supported_layout()
        .expect_err("replacement root rejects operation");

    assert_eq!(failure, LocalGitFailure::Repository);
    fs::remove_dir_all(fixture.root()).expect("replacement repository removes");
    fs::rename(retired_root, fixture.root()).expect("fixture root restores");
}

#[test]
fn object_capture_rejects_an_object_database_replaced_after_scan() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let objects = fixture.root().join(".git/objects");
    let retired_objects = fixture.root().join(".git/objects.retired");

    let failure = PinnedObjectDatabase::capture_with_test_hook(&authority, || {
        fs::rename(&objects, &retired_objects).expect("object database retires");
        fs::create_dir(&objects).expect("replacement object database constructs");
    })
    .err()
    .expect("replacement object database rejects capture");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(objects.is_dir());
    fs::remove_dir(&objects).expect("replacement object database removes");
    fs::rename(retired_objects, objects).expect("object database restores");
}

#[test]
fn object_capture_rejects_a_loose_directory_replaced_after_scan() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let objects = fixture.root().join(".git/objects");
    let object_id = fixture.initial.to_string();
    let prefix = object_id.get(..2).expect("fixture object prefix exists");
    let loose = objects.join(prefix);
    let retired_loose = objects.join(format!("{prefix}.retired"));

    let failure = PinnedObjectDatabase::capture_with_test_hook(&authority, || {
        fs::rename(&loose, &retired_loose).expect("loose object directory retires");
        fs::create_dir(&loose).expect("replacement loose object directory constructs");
    })
    .err()
    .expect("replacement loose object directory rejects capture");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(loose.is_dir());
    fs::remove_dir(&loose).expect("replacement loose object directory removes");
    fs::rename(retired_loose, loose).expect("loose object directory restores");
}

#[test]
fn object_capture_rejects_a_loose_object_replaced_after_scan() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let object_id = fixture.initial.to_string();
    let object_path = fixture
        .root()
        .join(".git/objects")
        .join(&object_id[..2])
        .join(&object_id[2..]);
    let original = fs::read(&object_path).expect("fixture loose object reads");
    let replacement = vec![b'x'; original.len()];

    let failure = PinnedObjectDatabase::capture_with_test_hook(&authority, || {
        fs::remove_file(&object_path).expect("loose object removes after scan");
        fs::write(&object_path, &replacement).expect("loose object replaces after scan")
    })
    .err()
    .expect("replaced loose object rejects capture");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_a_loose_object_added_after_scan() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let object_id = fixture.initial.to_string();
    let added_path = fixture
        .root()
        .join(".git/objects")
        .join(&object_id[..2])
        .join("0".repeat(object_id.len() - 2));

    let failure = PinnedObjectDatabase::capture_with_test_hook(&authority, || {
        fs::write(&added_path, b"actor object").expect("loose object adds after scan")
    })
    .err()
    .expect("added loose object rejects capture");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_a_pack_file_rewritten_after_scan() {
    let fixture = Fixture::new();
    let pack_path = plant_packed_blob(fixture.root(), b"packed fixture content");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let pack_length = fs::metadata(&pack_path)
        .expect("fixture pack metadata reads")
        .len();
    let replacement =
        vec![0_u8; usize::try_from(pack_length).expect("fixture pack length fits memory")];

    let failure = PinnedObjectDatabase::capture_with_test_hook(&authority, || {
        fs::write(&pack_path, &replacement).expect("pack file rewrites after scan")
    })
    .err()
    .expect("rewritten pack file rejects capture");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn pack_validation_rejects_a_trailer_that_overlaps_the_header() {
    let mut bytes = b"PACK\0\0\0\x02\0\0\0".to_vec();
    let checksum = Sha1::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    let expected = git2::Oid::from_bytes(&checksum).expect("fixture checksum parses");

    let failure = validate_pack_file(&bytes, expected, ObjectFormat::Sha1)
        .expect_err("overlapping pack trailer rejects");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_an_alternate_added_after_final_leaf_validation() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let info = fixture.root().join(".git/objects/info");
    fs::create_dir_all(&info).expect("object info directory exists");

    let failure = PinnedObjectDatabase::capture_with_post_bindings_test_hook(&authority, || {
        fs::write(info.join("alternates"), b"../outside\n")
            .expect("racing alternate writes after final leaf validation");
    })
    .err()
    .expect("late object alternate rejects capture");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_byte_count_rejects_a_loose_object_added_after_measurement() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = live_object_database_bytes_with_test_hook(&authority, || {
        plant_loose_blob(fixture.root(), b"actor object added after measurement");
    })
    .expect_err("added loose object rejects byte count");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_byte_count_rejects_a_pack_directory_replaced_after_measurement() {
    let fixture = Fixture::new();
    plant_packed_blob(fixture.root(), b"packed fixture content");
    let pack = fixture.root().join(".git/objects/pack");
    let retired_pack = fixture.root().join(".git/objects/pack.retired");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = live_object_database_bytes_with_test_hook(&authority, || {
        fs::rename(&pack, &retired_pack).expect("measured pack directory retires");
        fs::create_dir(&pack).expect("replacement pack directory constructs");
    })
    .expect_err("replacement pack directory rejects byte count");

    assert_eq!(failure, LocalGitFailure::Repository);
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
    plant_deep_administrative_symlink(&git_directory, 256);
    assert_deep_administrative_symlink(git_directory, 256);

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("deep administrative symlink rejects");

    assert_repository_construction_failure(failure);
}

fn plant_deep_administrative_symlink(parent: &OwnedFd, remaining: usize) {
    if remaining == 0 {
        symlinkat("/outside", parent, "escape").expect("deep administrative symlink constructs");
        return;
    }
    let component = format!("d{remaining:03}-{}", "x".repeat(200));
    mkdirat(parent, &component, Mode::RUSR | Mode::WUSR | Mode::XUSR)
        .expect("deep administrative directory constructs");
    let child = openat(
        parent,
        &component,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("deep administrative directory pins");
    plant_deep_administrative_symlink(&child, remaining - 1);
}

#[track_caller]
fn assert_deep_administrative_symlink(mut parent: OwnedFd, depth: usize) {
    for remaining in (1..=depth).rev() {
        let component = format!("d{remaining:03}-{}", "x".repeat(200));
        parent = openat(
            &parent,
            &component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("expected deep administrative directory opens");
    }
    let status = statat(&parent, "escape", AtFlags::SYMLINK_NOFOLLOW)
        .expect("deep administrative escape metadata reads");
    assert_eq!(FileType::from_raw_mode(status.st_mode), FileType::Symlink);
}

#[test]
fn repository_open_rejects_an_administrative_directory_replaced_after_open() {
    let fixture = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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

    assert_repository_construction_failure(failure);
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
    let expected = validate_repository_layout(&root, workspace_root_identity(&root))
        .expect("fixture layout validates");
    fs::rename(&root, &retired_root).expect("fixture root retires");
    symlink(&retired_root, &root).expect("fixture root symlink constructs");

    let failure = PinnedRepository::open(&root, expected)
        .expect_err("symlinked repository root rejects authority open");

    fs::remove_file(&root).expect("fixture root symlink removes");
    fs::rename(&retired_root, &root).expect("fixture root restores");
    assert_repository_construction_failure(failure);
}

#[test]
fn repository_admission_rejects_a_path_replacing_the_injected_workspace_root() {
    let parent = tempfile::tempdir().expect("workspace parent constructs");
    let root = parent.path().join("workspace");
    let retired = parent.path().join("retired");
    let repository = git2::Repository::init(&root).expect("injected repository initializes");
    fs::write(root.join(TRACKED_PATH), "injected\n").expect("injected fixture file writes");
    commit_all(&repository, "injected");
    let workspace = WorkspaceRoot::try_new(&LocalWorkspaceFileSystem, &root)
        .expect("injected workspace root pins");
    fs::rename(&root, &retired).expect("injected workspace retires");
    git2::Repository::init(&root).expect("replacement repository initializes");

    let failure = validate_repository_layout(&root, workspace.identity())
        .expect_err("replacement repository root rejects admission");

    assert_repository_construction_failure(failure);
    assert!(retired.join(".git").exists());
    assert!(root.join(".git").exists());
}

#[test]
fn repository_open_rejects_a_symlinked_administrative_path() {
    let fixture = Fixture::new();
    let git_path = fixture.root().join(".git");
    let retired_git_path = fixture.root().join(".git.retired");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    fs::rename(&git_path, &retired_git_path).expect("fixture administrative directory retires");
    symlink(".git.retired", &git_path).expect("administrative symlink constructs");

    let failure = PinnedRepository::open(fixture.root(), expected)
        .expect_err("symlinked administrative directory rejects authority open");

    fs::remove_file(&git_path).expect("administrative symlink removes");
    fs::rename(&retired_git_path, &git_path).expect("administrative directory restores");
    assert_repository_construction_failure(failure);
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
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("BOM-prefixed include rejects");

    assert_repository_construction_failure(failure);
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

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("tab-delimited filter section rejects");

    assert_repository_construction_failure(failure);
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

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("unsupported reference storage extension rejects");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_config_rejects_an_unsupported_format_version() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    fs::write(&config_path, "[core]\nrepositoryformatversion = 2\n")
        .expect("future repository format config writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("future repository format version rejects");

    assert_repository_construction_failure(failure);
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

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("SHA-256 under format version zero rejects");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_config_accepts_quoted_object_format_with_an_inline_comment() {
    let fixture = Sha256Fixture::new();
    fs::write(
        fixture.root().join(".git/config"),
        "[core]\nrepositoryformatversion = 1\nbare = false\n[extensions]\nobjectFormat = \"sha256\" # format\n",
    )
    .expect("quoted SHA-256 config writes");

    validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
        .expect("git2-decoded SHA-256 object format admits repository");
}

#[test]
fn repository_config_rejects_an_uppercase_sha1_object_format_value() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/config"),
        "[core]\nrepositoryformatversion = 1\nbare = false\n[extensions]\nobjectFormat = SHA1\n",
    )
    .expect("uppercase SHA-1 config writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("uppercase SHA-1 object-format value rejects");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_config_rejects_an_uppercase_sha256_object_format_value() {
    let fixture = Sha256Fixture::new();
    fs::write(
        fixture.root().join(".git/config"),
        "[core]\nrepositoryformatversion = 1\nbare = false\n[extensions]\nobjectFormat = SHA256\n",
    )
    .expect("uppercase SHA-256 config writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("uppercase SHA-256 object-format value rejects");

    assert_repository_construction_failure(failure);
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

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("duplicate repository format versions reject");

    assert_repository_construction_failure(failure);
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

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("bare repository rejects");

    assert_repository_construction_failure(failure);
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
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("duplicate bare declarations reject");

    assert_repository_construction_failure(failure);
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

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("valueless hooks path rejects repository");

    assert_repository_construction_failure(failure);
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

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("valueless worktree rejects repository");

    assert_repository_construction_failure(failure);
}

#[test]
fn authority_operation_rejects_live_config_bytes_changed_after_open() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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
fn authority_operation_rejects_an_administrative_symlink_created_after_open() {
    let fixture = Fixture::new();
    let administrative_symlink = fixture.root().join(".git/hooks/escape");
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    symlink("/outside", &administrative_symlink).expect("late administrative symlink constructs");

    let failure = ReferenceLock::acquire(&authority, "refs/heads/topic")
        .err()
        .expect("late administrative symlink rejects authority operation");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert!(!lock_path.exists());
    assert_eq!(
        fs::read_link(administrative_symlink).expect("administrative symlink remains"),
        Path::new("/outside")
    );
}

#[test]
fn authority_operation_rejects_identical_replacement_config_after_open() {
    let fixture = Fixture::new();
    let config_path = fixture.root().join(".git/config");
    let retired_config_path = fixture.root().join(".git/config.retired");
    let lock_path = fixture.root().join(".git/refs/heads/topic.lock");
    let original_config = fs::read(&config_path).expect("fixture config reads");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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

    validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
        .expect("empty shallow file validates");
}

#[test]
fn repository_layout_rejects_a_leading_blank_shallow_record() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/shallow"),
        format!("\n{}\n", fixture.initial),
    )
    .expect("leading blank shallow record writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("leading blank shallow record rejects");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_layout_rejects_a_doubled_trailing_shallow_newline() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/shallow"),
        format!("{}\n\n", fixture.initial),
    )
    .expect("doubled trailing shallow newline writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("doubled trailing shallow newline rejects");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_open_rejects_commondir_created_after_layout_validation() {
    let fixture = Fixture::new();
    let commondir_path = fixture.root().join(".git/commondir");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");

    let failure = PinnedRepository::open_with_hook(fixture.root(), expected, || {
        fs::write(&commondir_path, "../outside\n").expect("late commondir writes");
    })
    .expect_err("late commondir rejects repository open");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_admission_rejects_info_grafts() {
    let fixture = Fixture::new();
    let info = fixture.root().join(".git/info");
    fs::create_dir_all(&info).expect("repository info directory constructs");
    fs::write(info.join("grafts"), format!("{}\n", fixture.initial))
        .expect("grafts control writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("grafts control rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_admission_rejects_replacement_references() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root().join(".git/refs/replace"))
        .expect("replacement-reference namespace constructs");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("replacement-reference namespace rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn repository_admission_rejects_packed_replacement_references() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join(".git/packed-refs"),
        real_git_packed_replacement_reference(),
    )
    .expect("real Git packed replacement fixture writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("packed replacement reference rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn alternates_check_rejects_an_objects_info_path_replacement() {
    let fixture = Fixture::new();
    let git_path = fixture.root().join(".git");
    let info_path = git_path.join("objects/info");
    let retired_info = git_path.join("objects/info.retired");
    let alternates = info_path.join("alternates");
    let actor_alternates = "/outside/objects\n";
    fs::create_dir_all(&info_path).expect("object info directory constructs");
    let git_directory = openat(
        CWD,
        &git_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("administrative directory opens");

    let failure =
        unsupported_object_alternates_are_absent_with_test_hook(git_directory.as_fd(), || {
            fs::rename(&info_path, &retired_info).expect("object info directory retires");
            fs::create_dir(&info_path).expect("replacement object info directory constructs");
            fs::write(&alternates, actor_alternates).expect("replacement alternates writes");
        })
        .expect_err("replaced object info directory rejects absence check");

    assert_eq!(failure, LocalGitFailure::Repository);
    assert_eq!(
        fs::read_to_string(alternates).expect("replacement alternates reads"),
        actor_alternates
    );
    assert!(retired_info.is_dir());
}

#[test]
fn object_capture_rejects_alternates_created_after_authority_open() {
    let fixture = Fixture::new();
    let alternates_path = fixture.root().join(".git/objects/info/alternates");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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
    let valid_shallow = String::new();
    let replacement_shallow = format!("\n{}\n", fixture.initial);
    fs::write(&shallow_path, &valid_shallow).expect("empty shallow file writes");
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

    assert_repository_construction_failure(failure);
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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");

    let authority = PinnedRepository::open(fixture.root(), expected)
        .expect("repository opens from the validated config snapshot");
    let repository = authority.repository().expect("fixture repository locks");
    let filemode = repository_filemode(&repository).expect("snapshot filemode reads");

    assert!(filemode);
}

#[test]
fn repository_shell_rejects_an_object_database_symlink_replacement() {
    let fixture = Fixture::new();
    let outside = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let objects = fixture.root().join(".git/objects");
    let retired_objects = fixture.root().join(".git/objects.retired");
    fs::rename(&objects, &retired_objects).expect("fixture objects retire");
    symlink(outside.root().join(".git/objects"), &objects)
        .expect("outside object database symlink constructs");

    let failure = authority
        .repository()
        .err()
        .expect("replacement object database rejects repository shell");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_an_object_database_symlink_replacement() {
    let fixture = Fixture::new();
    let outside = Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");
    let objects = fixture.root().join(".git/objects");
    let retired_objects = fixture.root().join(".git/objects.retired");
    fs::rename(&objects, &retired_objects).expect("fixture objects retire");
    symlink(outside.root().join(".git/objects"), &objects)
        .expect("outside object database symlink constructs");

    let failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("replacement object database rejects capture");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_a_compressed_loose_object_above_the_decoded_limit() {
    let fixture = Fixture::new();
    let content = vec![0_u8; MAX_OBJECT_BYTES + 1];
    let object_path = plant_loose_blob(fixture.root(), &content);
    let compressed_bytes = fs::metadata(object_path)
        .expect("oversized loose object metadata reads")
        .len();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("oversized decoded loose object rejects capture");

    assert!(compressed_bytes < content.len() as u64);
    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_trailing_bytes_after_a_loose_object_stream() {
    let fixture = Fixture::new();
    let object_path = plant_loose_blob(fixture.root(), b"fixture object");
    let mut bytes = fs::read(&object_path).expect("loose object reads");
    bytes.extend_from_slice(b"trailing bytes");
    fs::remove_file(&object_path).expect("loose object removes");
    fs::write(&object_path, bytes).expect("loose object with trailing bytes writes");
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("fixture repository pins");

    let failure = PinnedObjectDatabase::capture(&authority)
        .err()
        .expect("trailing loose-object bytes reject capture");

    assert_eq!(failure, LocalGitFailure::Repository);
}

#[test]
fn object_capture_rejects_a_loose_object_stored_under_an_unrelated_id() {
    let fixture = Fixture::new();
    let claimed_id =
        git2::Oid::hash_object(ObjectType::Blob, b"claimed blob").expect("claimed blob hashes");
    plant_loose_blob_with_claimed_id(fixture.root(), b"actual blob", claimed_id);
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");
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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("fixture layout validates");

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

    assert_repository_construction_failure(failure);
}

#[test]
fn sha256_repository_admits_the_declared_object_format() {
    let fixture = Sha256Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("SHA-256 repository pins");

    assert_eq!(authority.object_format, ObjectFormat::Sha256);
}

#[test]
fn sha256_repository_rejects_a_nonempty_shallow_boundary() {
    let fixture = Sha256Fixture::new();
    fs::write(
        fixture.root().join(".git/shallow"),
        format!("{}\n", fixture.initial),
    )
    .expect("SHA-256 shallow boundary writes");

    let failure =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect_err("nonempty SHA-256 shallow boundary rejects admission");

    assert_repository_construction_failure(failure);
}

#[test]
fn sha256_repository_resolves_its_symbolic_head() {
    let fixture = Sha256Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("SHA-256 layout validates");
    let authority =
        PinnedRepository::open(fixture.root(), expected).expect("SHA-256 repository pins");
    let (_, head) =
        resolve_pinned_reference_chain(&authority, None).expect("SHA-256 reference chain resolves");

    assert_eq!(head, Some(fixture.initial));
}

#[test]
fn sha256_index_entries_retain_sha256_object_ids() {
    let fixture = Sha256Fixture::new();
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("SHA-256 layout validates");
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
    let expected =
        validate_repository_layout(fixture.root(), workspace_root_identity(fixture.root()))
            .expect("SHA-256 layout validates");
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

//! Repository-layout scan properties.

use std::{fs, os::unix::fs::symlink};

use rustix::fs::{CWD, Mode, OFlags, openat};

use crate::layout::{reject_administrative_symlinks, validate_repository_layout};
use crate::pinning::{PinnedRepository, repository_filemode};
use crate::tests::support::Fixture;

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

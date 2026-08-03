//! Packed-object installation and rollback properties.

use std::{fs, io::Write, os::unix::fs::PermissionsExt};

use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat, unlinkat};

use crate::failure::LocalGitFailure;
use crate::pack_install::{
    install_packed_object_file, install_packed_object_file_with_copy_and_hook,
    install_packed_object_file_with_hook, install_packed_object_pair, pack_installation_mode,
};

#[test]
fn packed_object_install_rejects_same_length_replacement_content() {
    let source = tempfile::tempdir().expect("fixture source directory constructs");
    let destination = tempfile::tempdir().expect("fixture pack directory constructs");
    let name = "pack-fixture.pack";
    let source_path = source.path().join(name);
    let destination_path = destination.path().join(name);
    let replacement_content = b"hostile";
    fs::write(&source_path, b"trusted").expect("fixture source pack writes");
    fs::write(&destination_path, replacement_content).expect("fixture replacement pack writes");
    let directory = openat(
        CWD,
        destination.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture pack directory pins");

    let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");
    let failure = install_packed_object_file(&directory, &source_path, mode)
        .expect_err("different same-length pack rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(destination_path).expect("fixture replacement pack reads"),
        replacement_content
    );
}

#[test]
fn packed_object_install_uses_the_pack_directory_shared_mode() {
    let source = tempfile::tempdir().expect("fixture source directory constructs");
    let destination = tempfile::tempdir().expect("fixture pack directory constructs");
    let directory_mode = 0o2770;
    let expected_file_mode = (directory_mode & 0o666) | 0o600;
    let name = "pack-shared-mode.pack";
    let source_path = source.path().join(name);
    let destination_path = destination.path().join(name);
    fs::write(&source_path, b"shared pack fixture").expect("fixture source pack writes");
    fs::set_permissions(
        destination.path(),
        fs::Permissions::from_mode(directory_mode),
    )
    .expect("fixture pack directory permissions set");
    let directory = openat(
        CWD,
        destination.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture pack directory pins");
    let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");

    install_packed_object_file(&directory, &source_path, mode).expect("fixture pack installs");
    let installed_mode = fs::metadata(destination_path)
        .expect("installed pack metadata reads")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(installed_mode, expected_file_mode);
}

#[test]
fn packed_object_pair_rolls_back_the_pack_when_index_installation_fails() {
    let source = tempfile::tempdir().expect("fixture source directory constructs");
    let destination = tempfile::tempdir().expect("fixture pack directory constructs");
    let pack_name = "pack-fixture.pack";
    let index_name = "pack-fixture.idx";
    let index_lock_name = format!("{index_name}.lock");
    let pack_source = source.path().join(pack_name);
    let index_source = source.path().join(index_name);
    let pack_destination = destination.path().join(pack_name);
    let index_destination = destination.path().join(index_name);
    let index_lock = destination.path().join(&index_lock_name);
    let occupied_lock = b"occupied pack index lock";
    fs::write(&pack_source, b"trusted pack").expect("fixture pack source writes");
    fs::write(&index_source, b"trusted index").expect("fixture index source writes");
    fs::write(&index_lock, occupied_lock).expect("occupied index lock writes");
    let directory = openat(
        CWD,
        destination.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture pack directory pins");
    let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");

    let failure = install_packed_object_pair(&directory, &pack_source, &index_source, mode)
        .expect_err("occupied index lock rejects pair publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!pack_destination.exists());
    assert!(!index_destination.exists());
    assert_eq!(
        fs::read(index_lock).expect("occupied index lock reads"),
        occupied_lock
    );
}

#[test]
fn packed_object_install_removes_its_lock_after_copy_failure() {
    let source = tempfile::tempdir().expect("fixture source directory constructs");
    let destination = tempfile::tempdir().expect("fixture pack directory constructs");
    let name = "pack-copy-failure.pack";
    let lock_name = format!("{name}.lock");
    let source_path = source.path().join(name);
    let destination_path = destination.path().join(name);
    let lock_path = destination.path().join(lock_name);
    let source_content = b"trusted pack fixture";
    fs::write(&source_path, source_content).expect("fixture source pack writes");
    let directory = openat(
        CWD,
        destination.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture pack directory pins");
    let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");

    let failure = install_packed_object_file_with_copy_and_hook(
        &directory,
        &source_path,
        mode,
        |_source, destination| {
            destination.write_all(b"partial pack lock")?;
            Err(std::io::Error::other("fixture copy failure"))
        },
        || {},
    )
    .expect_err("failed pack copy rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!lock_path.exists());
    assert!(!destination_path.exists());
    assert_eq!(
        fs::read(source_path).expect("fixture source pack reads"),
        source_content
    );
}

#[test]
fn packed_object_install_rejects_a_replaced_source_lock() {
    let source = tempfile::tempdir().expect("fixture source directory constructs");
    let destination = tempfile::tempdir().expect("fixture pack directory constructs");
    let name = "pack-replaced-lock.pack";
    let lock_name = format!("{name}.lock");
    let source_path = source.path().join(name);
    let destination_path = destination.path().join(name);
    let replacement_lock_path = destination.path().join(&lock_name);
    let source_content = b"trusted pack fixture";
    let replacement_content = b"replacement pack lock";
    fs::write(&source_path, source_content).expect("fixture source pack writes");
    let directory = openat(
        CWD,
        destination.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("fixture pack directory pins");
    let mode = pack_installation_mode(&directory).expect("fixture pack mode resolves");

    let failure = install_packed_object_file_with_hook(&directory, &source_path, mode, || {
        unlinkat(&directory, &lock_name, AtFlags::empty()).expect("fixture source lock unlinks");
        fs::write(&replacement_lock_path, replacement_content)
            .expect("fixture replacement lock writes");
    })
    .expect_err("replaced source lock rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!destination_path.exists());
    assert_eq!(
        fs::read(replacement_lock_path).expect("replacement lock reads"),
        replacement_content
    );
    assert_eq!(
        fs::read(source_path).expect("source pack reads"),
        source_content
    );
}

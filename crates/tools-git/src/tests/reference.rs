//! Reference-hierarchy creation and rollback properties.

use std::ffi::OsStr;

use rustix::fs::{CWD, Mode, OFlags, openat};

use crate::failure::LocalGitFailure;
use crate::reference_lock::open_or_create_ref_directory_with_mode_tracked_and_hook;

#[test]
fn created_reference_directory_is_removed_when_post_create_capture_fails() {
    let parent = tempfile::tempdir().expect("reference parent constructs");
    let directory = openat(
        CWD,
        parent.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("reference parent opens");
    let created_name = OsStr::new("created");

    let failure = open_or_create_ref_directory_with_mode_tracked_and_hook(
        &directory,
        created_name,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
        || Err(LocalGitFailure::Operation),
    )
    .expect_err("post-create capture failure rejects");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!parent.path().join(created_name).exists());
}

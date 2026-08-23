//! Safe, descriptor-native Linux permission restoration for runner cleanup.
#![allow(unsafe_code)]

use std::{
    ffi::{c_char, c_int, c_long},
    io,
    os::fd::{AsRawFd as _, BorrowedFd},
};

const AT_EMPTY_PATH: c_int = 0x1000;
#[cfg(target_arch = "mips")]
const SYS_FCHMODAT2: c_long = 4452;
#[cfg(target_arch = "mips64")]
const SYS_FCHMODAT2: c_long = 5452;
#[cfg(not(any(target_arch = "mips", target_arch = "mips64")))]
const SYS_FCHMODAT2: c_long = 452;
const INVALID_DESCRIPTOR: c_int = -1;
const EBADF: c_int = 9;

unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
}

/// Verifies that the host admits descriptor-native permission restoration.
///
/// The deliberately invalid descriptor makes a supported kernel return
/// `EBADF` without changing any filesystem object. Older kernels return
/// `ENOSYS`, while a seccomp profile that rejects the syscall returns its own
/// error. Callers run this check before beginning any workspace operation.
pub fn ensure_available() -> io::Result<()> {
    static EMPTY_PATH: &[u8] = b"\0";

    // SAFETY: `EMPTY_PATH` is a static NUL-terminated C string, all remaining
    // arguments are integers, and the invalid descriptor prevents mutation.
    let result = unsafe {
        syscall(
            SYS_FCHMODAT2,
            INVALID_DESCRIPTOR,
            EMPTY_PATH.as_ptr().cast::<c_char>(),
            0,
            AT_EMPTY_PATH,
        )
    };
    let error = io::Error::last_os_error();
    if result == -1 && error.raw_os_error() == Some(EBADF) {
        Ok(())
    } else if result == -1 {
        Err(error)
    } else {
        Err(io::Error::other(
            "fchmodat2 capability probe unexpectedly succeeded",
        ))
    }
}

/// Changes the mode of the object referenced by `descriptor`.
///
/// The empty pathname and `AT_EMPTY_PATH` make the kernel operate directly on
/// the supplied descriptor. No pathname, symbolic link, or procfs magic link is
/// resolved. Callers remain responsible for authenticating the descriptor as
/// the object whose permissions they intend to change.
pub fn chmod_descriptor(descriptor: BorrowedFd<'_>, mode: u32) -> io::Result<()> {
    static EMPTY_PATH: &[u8] = b"\0";

    // SAFETY: `EMPTY_PATH` is a static NUL-terminated C string, the remaining
    // arguments are integers, and `fchmodat2` neither reads nor writes Rust
    // memory through any other pointer. The borrowed descriptor remains valid
    // for the duration of the call.
    let result = unsafe {
        syscall(
            SYS_FCHMODAT2,
            descriptor.as_raw_fd(),
            EMPTY_PATH.as_ptr().cast::<c_char>(),
            mode,
            AT_EMPTY_PATH,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::{
            fd::AsFd as _,
            unix::fs::{MetadataExt as _, PermissionsExt as _},
        },
    };

    use rustix::fs::{Mode, OFlags, open};

    #[test]
    fn host_supports_descriptor_chmod_before_workspace_operations() {
        super::ensure_available().expect("the host admits descriptor-native chmod");
    }

    #[test]
    fn descriptor_chmod_restores_a_mode_zero_directory() {
        let parent = tempfile::tempdir().expect("the descriptor chmod fixture exists");
        let directory = parent.path().join("mode-zero");
        fs::create_dir(&directory).expect("the mode-zero fixture directory exists");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o000))
            .expect("the fixture directory has mode zero");
        let descriptor = open(
            &directory,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("the mode-zero fixture opens without read access");

        super::chmod_descriptor(descriptor.as_fd(), 0o700)
            .expect("descriptor chmod restores effective-user access");

        assert_eq!(
            fs::symlink_metadata(&directory)
                .expect("the restored fixture directory has metadata")
                .mode()
                & 0o7777,
            0o700
        );
    }
}

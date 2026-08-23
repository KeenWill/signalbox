//! Narrow Linux syscall boundary for the file-media worker sandbox.
//!
//! The main workspace forbids unsafe code. This separately governed crate owns
//! only pre-exec registration, descriptor operations, keyring detachment, and
//! sealed executable-memory operations that require unsafe Linux APIs.

use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd as _, FromRawFd as _},
        unix::process::CommandExt as _,
    },
    process::Command,
};

/// Labeled child setup applied after fork and before bubblewrap executes.
#[derive(Clone, Copy, Debug)]
pub struct ChildSetup {
    /// Address-space byte limit inherited by the sandbox tree.
    pub address_space_bytes: u64,
    /// CPU-second limit inherited by the sandbox tree.
    pub cpu_seconds: u64,
    /// Descriptor limit inherited by the sandbox tree.
    pub file_descriptors: u64,
    /// Seccomp descriptor made inheritable only in the forked child.
    pub seccomp_fd: i32,
    /// Startup-gate descriptor made inheritable only in the forked child.
    pub startup_gate_fd: i32,
    /// Writable `cgroup.procs` descriptor for this invocation's delegated cgroup.
    pub cgroup_procs_fd: i32,
}

/// Registers the reviewed child-only setup on one command.
pub fn install_pre_exec(command: &mut Command, setup: ChildSetup) {
    // SAFETY: the closure captures only copyable scalar values and invokes only
    // async-signal-safe Linux syscalls before exec. Every descriptor remains
    // owned by the parent command setup until spawn completes.
    unsafe {
        command.pre_exec(move || prepare_child(setup));
    }
}

fn prepare_child(setup: ChildSetup) -> io::Result<()> {
    enter_cgroup(setup.cgroup_procs_fd).map_err(|error| child_setup_error("cgroup", error))?;
    set_limit(libc::RLIMIT_AS, setup.address_space_bytes)
        .map_err(|error| child_setup_error("address-space limit", error))?;
    set_limit(libc::RLIMIT_CPU, setup.cpu_seconds)
        .map_err(|error| child_setup_error("CPU limit", error))?;
    set_limit(libc::RLIMIT_CORE, 0).map_err(|error| child_setup_error("core-dump limit", error))?;
    set_limit(libc::RLIMIT_NOFILE, setup.file_descriptors)
        .map_err(|error| child_setup_error("descriptor limit", error))?;
    inherit_descriptor(setup.seccomp_fd)
        .map_err(|error| child_setup_error("seccomp descriptor", error))?;
    inherit_descriptor(setup.startup_gate_fd)
        .map_err(|error| child_setup_error("startup-gate descriptor", error))?;
    detach_session_keyring().map_err(|error| child_setup_error("session keyring", error))
}

fn child_setup_error(stage: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("file-media child {stage}: {error}"))
}

fn enter_cgroup(cgroup_procs_fd: i32) -> io::Result<()> {
    let current_process = b"0\n";
    // SAFETY: the parent retains an open writable `cgroup.procs` descriptor;
    // writing zero moves this child into that cgroup before bubblewrap executes.
    let written = unsafe {
        libc::write(
            cgroup_procs_fd,
            current_process.as_ptr().cast(),
            current_process.len(),
        )
    };
    if written == current_process.len() as isize {
        Ok(())
    } else if written == -1 {
        Err(io::Error::last_os_error())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short write while entering invocation cgroup",
        ))
    }
}

fn set_limit(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: `limit` points to a fully initialized value for this resource.
    if unsafe { libc::setrlimit(resource, &limit) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn inherit_descriptor(raw_fd: i32) -> io::Result<()> {
    // SAFETY: command setup keeps the descriptor alive through this callback.
    if unsafe { libc::fcntl(raw_fd, libc::F_SETFD, 0) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn detach_session_keyring() -> io::Result<()> {
    const KEYCTL_JOIN_SESSION_KEYRING: libc::c_long = 1;
    // SAFETY: a null name creates and joins a fresh anonymous session keyring.
    let result = unsafe {
        libc::syscall(
            libc::SYS_keyctl,
            KEYCTL_JOIN_SESSION_KEYRING,
            std::ptr::null::<libc::c_char>(),
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Creates one close-on-exec sealable anonymous executable snapshot.
pub fn create_executable_snapshot() -> io::Result<File> {
    let name = b"signalbox-file-media-worker\0";
    // SAFETY: memfd_create receives a valid nul-terminated name and fixed flags.
    let raw_fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr().cast::<libc::c_char>(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful memfd_create returns a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(raw_fd as i32) })
}

/// Seals one snapshot against writes, growth, truncation, and seal changes.
pub fn seal_executable_snapshot(file: &File) -> io::Result<()> {
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    // SAFETY: fcntl receives an owned descriptor and the documented seal mask.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

//! Kernel process start identities used with retained process-group IDs.
use std::io;

#[cfg(target_os = "linux")]
pub(super) fn start_time(group: u32) -> io::Result<Option<String>> {
    let stat = match std::fs::read_to_string(format!("/proc/{group}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // proc_pid_stat(5), field 22: start time in clock ticks since boot.
    // The parenthesized command can contain spaces and closing parentheses.
    let ticks = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "process start time is missing")
        })?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(Some(format!("{}:{ticks}", boot.trim())))
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(super) fn start_time(group: u32) -> io::Result<Option<String>> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-p", &group.to_string(), "-o", "lstart="])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let start = String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let start = start.trim();
    Ok((!start.is_empty()).then(|| start.to_owned()))
}

#[cfg(not(unix))]
pub(super) fn start_time(_group: u32) -> io::Result<Option<String>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process-group identity is unavailable",
    ))
}

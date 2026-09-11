//! Startup verification of the exact binary named by the release manifest.

use std::path::Path;

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

use crate::runtime::CodexCliVersionProbeError;

#[cfg(test)]
pub(crate) async fn verify_executable_digest(
    executable: &Path,
    expected: &str,
    deadline: tokio::time::Instant,
) -> Result<(), CodexCliVersionProbeError> {
    let observed = executable_digest(executable, deadline).await?;
    if observed != expected {
        return Err(CodexCliVersionProbeError::VersionMismatch);
    }
    Ok(())
}

pub(crate) async fn executable_digest(
    executable: &Path,
    deadline: tokio::time::Instant,
) -> Result<String, CodexCliVersionProbeError> {
    tokio::time::timeout_at(deadline, async {
        let mut file = tokio::fs::File::open(executable)
            .await
            .map_err(|_| CodexCliVersionProbeError::ExecutableReadFailed)?;
        let mut digest = Sha256::new();
        // Stream the executable without retaining its contents in daemon memory.
        let mut buffer = [0; 8192];
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|_| CodexCliVersionProbeError::ExecutableReadFailed)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(hex::encode(digest.finalize()))
    })
    .await
    .map_err(|_| CodexCliVersionProbeError::TimedOut)?
}

#[cfg(test)]
#[path = "executable_pin_tests.rs"]
mod tests;

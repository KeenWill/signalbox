use std::time::Duration;

use super::verify_executable_digest;
use crate::runtime::CodexCliVersionProbeError;

/// SHA-256 of the independent fixture bytes `abc`.
const FIXTURE_DIGEST: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[tokio::test]
async fn accepts_the_pinned_contents_and_rejects_a_changed_file() -> std::io::Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(file.path(), b"abc")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    assert_eq!(
        verify_executable_digest(file.path(), FIXTURE_DIGEST, deadline).await,
        Ok(())
    );

    std::fs::write(file.path(), b"abd")?;
    assert_eq!(
        verify_executable_digest(file.path(), FIXTURE_DIGEST, deadline).await,
        Err(CodexCliVersionProbeError::VersionMismatch)
    );
    Ok(())
}

#[tokio::test]
async fn refuses_a_file_that_cannot_be_read() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing-codex");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    assert_eq!(
        verify_executable_digest(&missing, FIXTURE_DIGEST, deadline).await,
        Err(CodexCliVersionProbeError::ExecutableReadFailed)
    );
    Ok(())
}

#[tokio::test]
async fn digest_reading_respects_the_probe_deadline() -> std::io::Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    // A file larger than one read ensures deadline polling continues while hashing.
    file.as_file().set_len(1024 * 1024)?;
    let deadline = tokio::time::Instant::now() - Duration::from_secs(1);
    assert_eq!(
        verify_executable_digest(file.path(), FIXTURE_DIGEST, deadline).await,
        Err(CodexCliVersionProbeError::TimedOut)
    );
    Ok(())
}

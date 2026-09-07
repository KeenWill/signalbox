//! Exports the pinned upstream version and fork executable SHA-256 from
//! `../../tooling/codex-cli/release.json` for startup verification.

mod version_pin;

use std::path::PathBuf;

const PIN_MANIFEST: &str = "../../tooling/codex-cli/release.json";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed={PIN_MANIFEST}");

    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(PIN_MANIFEST);
    let manifest = std::fs::read_to_string(&manifest_path).map_err(|error| {
        std::io::Error::other(format!("{} is readable: {error}", manifest_path.display()))
    })?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).map_err(|error| {
        std::io::Error::other(format!(
            "{} is valid JSON: {error}",
            manifest_path.display()
        ))
    })?;
    let pinned = manifest
        .get("release")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "{} declares a fork release tag",
                manifest_path.display()
            ))
        })?;
    let pinned = version_pin::upstream_version(pinned).ok_or_else(|| {
        std::io::Error::other(format!(
            "{} must pin an exact rust-vX.Y.Z-signalbox.N release",
            manifest_path.display()
        ))
    })?;

    let executable_sha256 = manifest
        .get("executableSha256")
        .and_then(serde_json::Value::as_str)
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| std::io::Error::other("pin manifest must declare the executable SHA-256"))?;

    println!("cargo:rustc-env=SIGNALBOX_CODEX_CLI_SHA256={executable_sha256}");
    println!("cargo:rustc-env=SIGNALBOX_CODEX_CLI_VERSION={pinned}");
    Ok(())
}

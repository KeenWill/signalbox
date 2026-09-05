//! Reads the exact pinned `@anthropic-ai/claude-code` version from
//! `package.json` and exports it as the `SIGNALBOX_CLAUDE_CLI_VERSION` build
//! environment variable the adapter and its `tests/pin.rs` compare against.

mod version_pin;

use std::path::PathBuf;

const PIN_MANIFEST: &str = "package.json";
const PIN_PACKAGE: &str = "@anthropic-ai/claude-code";

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
        .get("dependencies")
        .and_then(|dependencies| dependencies.get(PIN_PACKAGE))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "{} declares a {PIN_PACKAGE} dependency",
                manifest_path.display()
            ))
        })?;
    if !version_pin::is_exact_pin(pinned) {
        return Err(std::io::Error::other(format!(
            "{} must pin {PIN_PACKAGE} at an exact major.minor.patch release",
            manifest_path.display()
        ))
        .into());
    }

    println!("cargo:rustc-env=SIGNALBOX_CLAUDE_CLI_VERSION={pinned}");
    Ok(())
}

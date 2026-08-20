//! Regenerates browser artifacts from Rust DTO definitions.

use std::{env, error::Error, fs};

fn main() -> Result<(), Box<dyn Error>> {
    let repository_root = env::current_dir()?;
    for artifact in signalbox_web_contract::generated_artifacts()? {
        let path = repository_root.join(artifact.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, artifact.contents)?;
    }
    Ok(())
}

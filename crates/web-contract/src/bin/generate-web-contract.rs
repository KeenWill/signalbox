//! Regenerates browser artifacts from Rust DTO definitions.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

fn main() -> Result<(), Box<dyn Error>> {
    let repository_root = repository_root();
    for artifact in signalbox_web_contract::generated_artifacts()? {
        let path = repository_root.join(artifact.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, artifact.contents)?;
    }
    Ok(())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[cfg(test)]
mod tests {
    use super::repository_root;

    #[test]
    fn generation_root_contains_workspace_and_browser_client() {
        let root = repository_root();

        assert!(root.join("Cargo.toml").is_file());
        assert!(root.join("clients/web").is_dir());
    }
}

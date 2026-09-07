//! Regenerates browser artifacts from Rust DTO definitions.

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let repository_root = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(repository_root);
    if arguments.next().is_some() {
        return Err("usage: generate-web-contract [OUTPUT_ROOT]".into());
    }
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

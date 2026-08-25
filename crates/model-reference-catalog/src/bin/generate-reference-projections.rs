use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use signalbox_model_reference_catalog::{Projection, bundled_catalog, render_projections};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mode = env::args()
        .nth(1)
        .ok_or_else(|| String::from("usage: generate-reference-projections (--check|--write)"))?;
    if env::args().nth(2).is_some() || !matches!(mode.as_str(), "--check" | "--write") {
        return Err(String::from(
            "usage: generate-reference-projections (--check|--write)",
        ));
    }

    let catalog = bundled_catalog().map_err(|error| error.to_string())?;
    let output_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("projections");
    if mode == "--write" {
        fs::create_dir_all(&output_directory)
            .map_err(|error| format!("cannot create {}: {error}", output_directory.display()))?;
    }

    let projections = render_projections(&catalog);
    reconcile_projection_files(&output_directory, &projections, mode.as_str())?;

    for projection in projections {
        let path = output_directory.join(projection.filename);
        if mode == "--write" {
            fs::write(&path, projection.contents)
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        } else {
            let checked_in = fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            if checked_in != projection.contents {
                return Err(format!(
                    "{} is stale; run generate-reference-projections --write",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn reconcile_projection_files(
    output_directory: &Path,
    projections: &[Projection],
    mode: &str,
) -> Result<(), String> {
    let expected = projections
        .iter()
        .map(|projection| projection.filename)
        .collect::<BTreeSet<_>>();
    let entries = fs::read_dir(output_directory)
        .map_err(|error| format!("cannot read {}: {error}", output_directory.display()))?;

    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "cannot read an entry in {}: {error}",
                output_directory.display()
            )
        })?;
        let path = entry.path();
        let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.extension().and_then(|extension| extension.to_str()) != Some("md")
            || expected.contains(filename)
        {
            continue;
        }
        if mode == "--write" {
            fs::remove_file(&path)
                .map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
        } else {
            return Err(format!(
                "{} is an obsolete projection; run generate-reference-projections --write",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reconcile_projection_files;
    use signalbox_model_reference_catalog::Projection;
    use std::{fs, path::PathBuf};

    fn temporary_projection_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "signalbox-reference-projections-{name}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn check_rejects_obsolete_projection_file() {
        let directory = temporary_projection_directory("check");
        let obsolete = directory.join("obsolete.md");
        fs::write(&obsolete, "obsolete").unwrap();

        let error = reconcile_projection_files(&directory, &[], "--check").unwrap_err();

        assert!(error.contains("obsolete projection"));
        assert!(obsolete.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn write_removes_obsolete_projection_file() {
        let directory = temporary_projection_directory("write");
        let obsolete = directory.join("obsolete.md");
        fs::write(&obsolete, "obsolete").unwrap();
        let projections = [Projection {
            filename: "current.md",
            contents: String::new(),
        }];

        reconcile_projection_files(&directory, &projections, "--write").unwrap();

        assert!(!obsolete.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}

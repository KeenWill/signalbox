use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use signalbox_model_reference_catalog::{
    GENERATED_PROJECTION_BANNER, Projection, bundled_catalog, render_projections,
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Check,
    Write,
}

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

    let mode = if mode == "--check" {
        Mode::Check
    } else {
        Mode::Write
    };
    let catalog = bundled_catalog().map_err(|error| error.to_string())?;
    let output_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("projections");
    synchronize_projections(mode, &output_directory, render_projections(&catalog))
}

fn synchronize_projections(
    mode: Mode,
    output_directory: &Path,
    projections: Vec<Projection>,
) -> Result<(), String> {
    if mode == Mode::Write {
        fs::create_dir_all(output_directory)
            .map_err(|error| format!("cannot create {}: {error}", output_directory.display()))?;
    }

    reconcile_projection_inventory(mode, output_directory, &projections)?;
    for projection in projections {
        let path = output_directory.join(projection.filename);
        if mode == Mode::Write {
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

fn reconcile_projection_inventory(
    mode: Mode,
    output_directory: &Path,
    projections: &[Projection],
) -> Result<(), String> {
    let expected = projections
        .iter()
        .map(|projection| projection.filename)
        .collect::<BTreeSet<_>>();
    let mut paths = fs::read_dir(output_directory)
        .map_err(|error| format!("cannot list {}: {error}", output_directory.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("cannot inspect {}: {error}", output_directory.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();

    for path in paths {
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if !contents.starts_with(GENERATED_PROJECTION_BANNER) {
            continue;
        }
        let filename = path
            .file_name()
            .and_then(|filename| filename.to_str())
            .ok_or_else(|| format!("{} has a non-UTF-8 filename", path.display()))?;
        if expected.contains(filename) {
            continue;
        }
        if mode == Mode::Check {
            return Err(format!(
                "{} is an obsolete generated projection; run generate-reference-projections --write",
                path.display()
            ));
        }
        fs::remove_file(&path)
            .map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use signalbox_model_reference_catalog::{GENERATED_PROJECTION_BANNER, Projection};
    use tempfile::tempdir;

    use super::{Mode, synchronize_projections};

    fn one_current_projection() -> Vec<Projection> {
        vec![Projection {
            filename: "current.md",
            contents: format!("{GENERATED_PROJECTION_BANNER}# Current\n"),
        }]
    }

    #[test]
    fn check_rejects_an_obsolete_generated_projection() {
        let directory = tempdir().unwrap();
        let current = directory.path().join("current.md");
        let obsolete = directory.path().join("obsolete.md");
        fs::write(
            &current,
            format!("{GENERATED_PROJECTION_BANNER}# Current\n"),
        )
        .unwrap();
        fs::write(
            &obsolete,
            format!("{GENERATED_PROJECTION_BANNER}# Obsolete\n"),
        )
        .unwrap();

        let error =
            synchronize_projections(Mode::Check, directory.path(), one_current_projection())
                .unwrap_err();

        assert_eq!(
            error,
            format!(
                "{} is an obsolete generated projection; run generate-reference-projections --write",
                obsolete.display()
            )
        );
    }

    #[test]
    fn write_removes_an_obsolete_generated_projection() {
        let directory = tempdir().unwrap();
        let obsolete = directory.path().join("obsolete.md");
        fs::write(
            &obsolete,
            format!("{GENERATED_PROJECTION_BANNER}# Obsolete\n"),
        )
        .unwrap();

        synchronize_projections(Mode::Write, directory.path(), one_current_projection()).unwrap();

        assert!(!obsolete.exists());
        assert_eq!(
            fs::read_to_string(directory.path().join("current.md")).unwrap(),
            format!("{GENERATED_PROJECTION_BANNER}# Current\n")
        );
    }
}

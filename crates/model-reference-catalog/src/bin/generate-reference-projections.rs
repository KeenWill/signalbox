use std::{env, fs, path::PathBuf, process::ExitCode};

use signalbox_model_reference_catalog::{bundled_catalog, render_projections};

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

    for projection in render_projections(&catalog) {
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

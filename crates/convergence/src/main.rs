//! Recording, evaluation, and operator reconciliation commands.
mod reconcile;

use signalbox_convergence::{ConvergencePolicy, Error, Recording, evaluate, fetch};
use std::{collections::BTreeMap, io::Write, path::Path, process::ExitCode};

fn save_state(path: &Path, state: &serde_json::Value) -> Result<(), Error> {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, state)?;
    file.flush()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn run() -> Result<u8, Error> {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().ok_or_else(|| {
        Error::Evidence("usage: signalbox-converge record|evaluate|reconcile --policy file".into())
    })?;
    if command == "reconcile" {
        return reconcile::run(arguments.collect());
    }
    if !matches!(command.as_str(), "record" | "evaluate") {
        return Err(Error::Evidence("expected record or evaluate".into()));
    }
    let mut options = BTreeMap::new();
    while let Some(key) = arguments.next() {
        if !matches!(
            key.as_str(),
            "--policy" | "--state" | "--fixture" | "--pr" | "--out" | "--repo"
        ) {
            return Err(Error::Evidence(format!("unknown option {key}")));
        }
        let value = arguments
            .next()
            .ok_or_else(|| Error::Evidence(format!("missing value for {key}")))?;
        options.insert(key, value);
    }
    let policy_path = options
        .get("--policy")
        .map(String::as_str)
        .unwrap_or("crates/convergence/examples/repository.toml");
    let policy = ConvergencePolicy::read(Path::new(policy_path))?;
    let previous = match options.get("--state") {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        },
        None => None,
    };
    let mut recording: Recording = if let Some(path) = options.get("--fixture") {
        Recording::read(Path::new(path))?
    } else {
        let number = options
            .get("--pr")
            .and_then(|number| number.parse().ok())
            .ok_or_else(|| Error::Evidence("--pr requires a pull request number".into()))?;
        fetch::record(
            options
                .get("--repo")
                .map(String::as_str)
                .unwrap_or(&policy.repository),
            number,
            &policy,
            previous.clone().unwrap_or_else(|| serde_json::json!({})),
        )?
    };
    if let Some(previous) = previous {
        recording.previous = previous;
    }
    match command.as_str() {
        "record" => {
            let path = options
                .get("--out")
                .ok_or_else(|| Error::Evidence("record requires --out".into()))?;
            recording.write(Path::new(path))?;
            Ok(0)
        }
        "evaluate" => {
            let snapshot = recording.snapshot(&policy)?;
            let result = evaluate(&snapshot, &policy)?;
            if let Some(path) = options.get("--state") {
                save_state(Path::new(path), &result.state)?;
            }
            let mut output = serde_json::to_value(&result)?;
            let node = &snapshot.current;
            output["pull_request"] = serde_json::json!({
                "id":node["id"], "number":node["number"], "title":node["title"],
                "url":node["url"], "baseRefName":node["baseRefName"],
                "headRefName":node["headRefName"], "headRepository":node["headRepository"],
            });
            serde_json::to_writer(std::io::stdout().lock(), &output)?;
            Ok(if result.converged { 0 } else { 1 })
        }
        _ => Err(Error::Evidence("expected record or evaluate".into())),
    }
}
fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

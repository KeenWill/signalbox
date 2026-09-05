use signalbox_convergence::{ConvergencePolicy, Error, Recording, evaluate, fetch};
use std::{collections::BTreeMap, path::Path, process::ExitCode};

fn run() -> Result<u8, Error> {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().ok_or_else(|| Error::Evidence("usage: signalbox-converge record|evaluate --pr N|--fixture file --policy file [--out file]".into()))?;
    let mut options = BTreeMap::new();
    while let Some(key) = arguments.next() {
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
    let recording: Recording = if let Some(path) = options.get("--fixture") {
        Recording::read(Path::new(path))?
    } else {
        let number = options
            .get("--pr")
            .and_then(|number| number.parse().ok())
            .ok_or_else(|| Error::Evidence("--pr requires a pull request number".into()))?;
        fetch::record(&policy.repository, number, &policy)?
    };
    match command.as_str() {
        "record" => {
            let path = options
                .get("--out")
                .ok_or_else(|| Error::Evidence("record requires --out".into()))?;
            recording.write(Path::new(path))?;
            Ok(0)
        }
        "evaluate" => {
            let result = evaluate(&recording.snapshot(&policy)?, &policy)?;
            serde_json::to_writer(std::io::stdout().lock(), &result)?;
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

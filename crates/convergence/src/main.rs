//! Recording, evaluation, and operator reconciliation commands.
mod reconcile;

use clap::{Args, Parser, Subcommand};
use signalbox_convergence::{ConvergencePolicy, Error, Recording, evaluate, fetch};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

/// Record GitHub evidence, evaluate convergence, or reconcile pull requests.
#[derive(Debug, Parser)]
#[command(name = "signalbox-converge")]
struct Cli {
    /// Operation to perform.
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Save a GitHub evidence recording to --out.
    Record(EvidenceArgs),
    /// Print an evaluation; exit 0 for convergence, 1 for a negative verdict, 2 for an error.
    Evaluate(EvidenceArgs),
    /// Reconcile matching pull requests using operator-supplied commands.
    Reconcile(reconcile::ReconcileArgs),
}

#[derive(Debug, Args)]
struct EvidenceArgs {
    /// TOML or JSON convergence policy.
    #[arg(
        long,
        value_name = "FILE",
        default_value = "crates/convergence/examples/repository.toml"
    )]
    policy: PathBuf,
    /// Prior observation state; omitted: use fixture history or start new live history.
    #[arg(long, value_name = "FILE")]
    state: Option<PathBuf>,
    /// Recorded evidence input; omitted: fetch the pull request selected by --pr.
    #[arg(long, value_name = "FILE")]
    fixture: Option<PathBuf>,
    /// Unsigned 64-bit pull-request number for live reads; omitted: use --fixture.
    #[arg(long, value_name = "NUMBER")]
    pr: Option<u64>,
    /// Recording destination; required for record, unused by evaluate.
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Repository for live reads; omitted: use the policy's repository.
    #[arg(long, value_name = "OWNER/NAME")]
    repo: Option<String>,
}

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

fn load_evidence(options: &EvidenceArgs) -> Result<(ConvergencePolicy, Recording), Error> {
    let policy = ConvergencePolicy::read(&options.policy)?;
    let previous = match &options.state {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        },
        None => None,
    };
    let mut recording: Recording = if let Some(path) = &options.fixture {
        Recording::read(path)?
    } else {
        let number = options
            .pr
            .ok_or_else(|| Error::Evidence("--pr requires a pull request number".into()))?;
        fetch::record(
            options.repo.as_deref().unwrap_or(&policy.repository),
            number,
            &policy,
            previous.clone().unwrap_or_else(|| serde_json::json!({})),
        )?
    };
    if let Some(previous) = previous {
        recording.previous = previous;
    }
    Ok((policy, recording))
}

fn run(command: Command) -> Result<u8, Error> {
    match command {
        Command::Record(options) => {
            let (_, recording) = load_evidence(&options)?;
            let path = options
                .out
                .as_deref()
                .ok_or_else(|| Error::Evidence("record requires --out".into()))?;
            recording.write(path)?;
            Ok(0)
        }
        Command::Evaluate(options) => {
            let (policy, recording) = load_evidence(&options)?;
            let snapshot = recording.snapshot(&policy)?;
            let result = evaluate(&snapshot, &policy)?;
            if let Some(path) = &options.state {
                save_state(path, &result.state)?;
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
        Command::Reconcile(args) => reconcile::run(&args),
    }
}
fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_evaluate_preserve_policy_defaults_and_optional_flags() {
        for command in ["record", "evaluate"] {
            let cli = Cli::try_parse_from(["signalbox-converge", command, "--pr", "1566"]).unwrap();
            let (Command::Record(args) | Command::Evaluate(args)) = cli.command else {
                panic!("evidence command expected")
            };
            assert_eq!(
                args.policy,
                PathBuf::from("crates/convergence/examples/repository.toml")
            );
            assert_eq!(args.pr, Some(1566));
            assert_eq!(args.repo, None);
            assert_eq!(args.fixture, None);
            assert_eq!(args.state, None);
            assert_eq!(args.out, None);
            let cli = Cli::try_parse_from([
                "signalbox-converge",
                command,
                "--fixture",
                "input.json",
                "--state",
                "state.json",
                "--out",
                "output.json",
                "--repo",
                "owner/name",
                "--policy",
                "policy.toml",
            ])
            .unwrap();
            let (Command::Record(args) | Command::Evaluate(args)) = cli.command else {
                panic!("evidence command expected")
            };
            assert_eq!(args.policy, PathBuf::from("policy.toml"));
            assert_eq!(args.fixture, Some(PathBuf::from("input.json")));
            assert_eq!(args.state, Some(PathBuf::from("state.json")));
            assert_eq!(args.out, Some(PathBuf::from("output.json")));
            assert_eq!(args.repo.as_deref(), Some("owner/name"));
        }
    }

    #[test]
    fn help_is_available_without_configuration_for_every_subcommand() {
        for command in ["record", "evaluate", "reconcile"] {
            let error = Cli::try_parse_from(["signalbox-converge", command, "--help"]).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp,
                "{command}"
            );
            assert!(error.to_string().contains("--policy"));
        }
    }
}

//! Operator client for live-provider evaluation through the daemon.

use signalbox_approval_judge_eval::live::CorpusCase;
use signalbox_process_protocol::EvaluationCorpusFormat;
use signalboxd::eval_client::{EvalClientError, launch_and_report};
use std::{env, fs, io, path::PathBuf};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), EvalClientError> {
    let mut socket = None;
    let mut path = None;
    let mut repeats = 3;
    let mut filter = None;
    let mut limit = None;
    let mut responses = None;
    let mut arguments = env::args().skip(1);
    while let Some(flag) = arguments.next() {
        if flag == "--help" || flag == "-h" {
            println!(
                "usage: approval-judge-eval --socket PATH --cases CORPUS.jsonl [--repeats N] [--filter TEXT] [--limit N] [--responses RESPONSES.json]\nThe daemon selects its configured approval judge. Repeats default to 3; the workflow admits at most 1000 trials. --responses uses recorded answers without provider access."
            );
            return Ok(());
        }
        let value = arguments.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{flag} requires a value"),
            )
        })?;
        match flag.as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--cases" => path = Some(value),
            "--repeats" => repeats = value.parse()?,
            "--filter" => filter = Some(value),
            "--limit" => limit = Some(value.parse::<usize>()?),
            "--responses" => responses = Some(fs::read(value)?),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown flag: {flag}"),
                )
                .into());
            }
        }
    }
    let required =
        |name| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required"));
    let socket = socket.ok_or_else(|| required("--socket"))?;
    let corpus = fs::read(path.ok_or_else(|| required("--cases"))?)?;
    if limit == Some(0) || repeats == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "limit and repeats must be positive",
        )
        .into());
    }
    let mut cases = Vec::new();
    for (position, line) in std::str::from_utf8(&corpus)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        if limit.is_some_and(|bound| cases.len() >= bound) {
            break;
        }
        let case: CorpusCase = serde_json::from_str(line)?;
        if filter.as_ref().is_none_or(|text| {
            case.name.contains(text.as_str()) || case.category.as_str().contains(text.as_str())
        }) {
            cases.push(u32::try_from(position)?);
        }
    }
    let scorecard = launch_and_report(
        &socket,
        &corpus,
        responses.as_deref(),
        EvaluationCorpusFormat::Live,
        cases,
        repeats,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&scorecard)?);
    Ok(())
}

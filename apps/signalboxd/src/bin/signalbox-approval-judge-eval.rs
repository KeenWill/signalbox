//! Operator client for recorded-response evaluation through the daemon.

use signalbox_process_protocol::EvaluationCorpusFormat;
use signalboxd::eval_client::{EvalClientError, launch_and_report};
use std::{env, fs, io, path::PathBuf};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), EvalClientError> {
    let mut arguments = env::args().skip(1);
    let usage = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: signalbox-approval-judge-eval --socket PATH CORPUS.json RESPONSES.json",
        )
    };
    if arguments.next().as_deref() != Some("--socket") {
        return Err(usage().into());
    }
    let socket = PathBuf::from(arguments.next().ok_or_else(usage)?);
    let corpus = fs::read(arguments.next().ok_or_else(usage)?)?;
    let responses = fs::read(arguments.next().ok_or_else(usage)?)?;
    if arguments.next().is_some() {
        return Err(usage().into());
    }
    let decoded = signalbox_approval_judge_eval::decode_corpus(&corpus)?;
    let cases = (0..u32::try_from(decoded.cases.len())?).collect();
    let scorecard = launch_and_report(
        &socket,
        &corpus,
        Some(&responses),
        EvaluationCorpusFormat::Offline,
        cases,
        1,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&scorecard)?);
    Ok(())
}

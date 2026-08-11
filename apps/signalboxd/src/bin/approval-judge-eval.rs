//! Approval-judge eval runner.
//!
//! Replays a labeled JSONL corpus through the exact deployed judge path —
//! system prompt, payload rendering, structured-output contract, and the
//! configured provider adapter — and prints a scorecard measuring accuracy by
//! category, verdict stability across repeats, and escalation calibration.
//!
//! This spends real provider quota on every call; it is a user-invoked
//! measurement harness, never part of daemon or CI execution. Usage:
//!
//! ```text
//! approval-judge-eval --config <config.toml> --cases <cases.jsonl> \
//!     [--repeats N] [--filter SUBSTRING] [--limit N]
//! ```
//!
//! Each corpus line is one JSON object: `name`, `category`, `tool`,
//! `arguments` (string), `expected` (`approve` | `deny` |
//! `escalate_to_human`), and optional `goal`, `template`, `system_prompt`,
//! `notes`. The judge model comes from the configuration's `approval_judge`
//! table; the run refuses configurations without one so a scorecard always
//! names the model it measured.

use std::{collections::BTreeMap, env, fs, path::PathBuf, process::ExitCode};

use serde::Deserialize;
use signalbox_domain::DelegateApprovalRecommendation;
use signalbox_model_provider_runtime::RuntimeApprovalJudgeModel;
use signalbox_model_runtime_anthropic::AnthropicRuntime;
use signalbox_model_runtime_openai::OpenAiRuntime;
use signalboxd::{
    FileCredentialAccess, HubModelConfiguration,
    approval_judge_eval::{
        ApprovalJudgeEvalBinding, ApprovalJudgeEvalCase, ApprovalJudgeEvalVerdict, judge_eval_case,
    },
    model_adapter::ConfiguredModelRuntime,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusCase {
    name: String,
    category: String,
    tool: String,
    arguments: String,
    expected: String,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

struct RunOptions {
    configuration: PathBuf,
    cases: PathBuf,
    repeats: usize,
    filter: Option<String>,
    limit: Option<usize>,
}

fn parse_arguments() -> Result<RunOptions, String> {
    let mut configuration = None;
    let mut cases = None;
    let mut repeats = 3_usize;
    let mut filter = None;
    let mut limit = None;
    let mut arguments = env::args().skip(1);
    while let Some(flag) = arguments.next() {
        let mut value = |flag: &str| {
            arguments
                .next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--config" => configuration = Some(PathBuf::from(value("--config")?)),
            "--cases" => cases = Some(PathBuf::from(value("--cases")?)),
            "--repeats" => {
                repeats = value("--repeats")?
                    .parse()
                    .map_err(|_| String::from("--repeats requires an integer"))?;
            }
            "--filter" => filter = Some(value("--filter")?),
            "--limit" => {
                limit = Some(
                    value("--limit")?
                        .parse()
                        .map_err(|_| String::from("--limit requires an integer"))?,
                );
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(RunOptions {
        configuration: configuration.ok_or_else(|| String::from("--config is required"))?,
        cases: cases.ok_or_else(|| String::from("--cases is required"))?,
        repeats: repeats.max(1),
        filter,
        limit,
    })
}

fn recommendation_label(recommendation: DelegateApprovalRecommendation) -> &'static str {
    match recommendation {
        DelegateApprovalRecommendation::Approve => "approve",
        DelegateApprovalRecommendation::Deny => "deny",
        DelegateApprovalRecommendation::EscalateToHuman => "escalate_to_human",
    }
}

#[derive(Default)]
struct CategoryScore {
    cases: usize,
    correct_majorities: usize,
    unstable_cases: usize,
    failed_calls: usize,
}

fn main() -> ExitCode {
    let options = match parse_arguments() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("usage error: {message}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("tokio runtime construction failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("eval run failed: {message}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run(options: RunOptions) -> Result<(), String> {
    let configuration = HubModelConfiguration::read(&options.configuration)
        .map_err(|error| format!("configuration rejected: {error:?}"))?;
    let selection = configuration
        .configured_approval_judge_selection()
        .ok_or_else(|| String::from("configuration has no [approval_judge] selection"))?;
    let route = configuration
        .resolve_direct_model(selection)
        .ok_or_else(|| String::from("approval_judge selection has no configured route"))?;
    let binding = ApprovalJudgeEvalBinding {
        selection,
        target: route.target(),
        credential_reference: format!("eval:{}", route.credential_profile()),
    };
    let adapters: ConfiguredModelRuntime<
        AnthropicRuntime<FileCredentialAccess>,
        OpenAiRuntime<FileCredentialAccess>,
    > = ConfiguredModelRuntime::new(None, None, &configuration)
        .map_err(|error| format!("adapter construction failed: {error}"))?;
    let model = RuntimeApprovalJudgeModel::new(adapters, configuration.runtime_model_catalog());

    let corpus = fs::read_to_string(&options.cases)
        .map_err(|error| format!("corpus read failed: {error}"))?;
    let mut cases = Vec::new();
    for (index, line) in corpus.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let case: CorpusCase = serde_json::from_str(line)
            .map_err(|error| format!("corpus line {} rejected: {error}", index + 1))?;
        if !matches!(
            case.expected.as_str(),
            "approve" | "deny" | "escalate_to_human"
        ) {
            return Err(format!(
                "corpus line {} names unknown expected verdict {}",
                index + 1,
                case.expected
            ));
        }
        if let Some(filter) = &options.filter
            && !case.name.contains(filter.as_str())
            && !case.category.contains(filter.as_str())
        {
            continue;
        }
        cases.push(case);
        if options.limit.is_some_and(|limit| cases.len() >= limit) {
            break;
        }
    }
    if cases.is_empty() {
        return Err(String::from("no corpus cases selected"));
    }
    eprintln!(
        "replaying {} cases x{} repeats against judge selection {}",
        cases.len(),
        options.repeats,
        selection.into_uuid(),
    );

    let mut scores: BTreeMap<String, CategoryScore> = BTreeMap::new();
    let mut case_reports = Vec::new();
    for case in &cases {
        let eval_case = ApprovalJudgeEvalCase {
            name: case.name.clone(),
            tool: case.tool.clone(),
            arguments: case.arguments.clone(),
            goal: case.goal.clone(),
            template: case.template.clone(),
            system_prompt: case.system_prompt.clone(),
        };
        let mut verdicts: Vec<ApprovalJudgeEvalVerdict> = Vec::new();
        let mut failures = 0_usize;
        for _ in 0..options.repeats {
            match judge_eval_case(&model, &binding, &eval_case).await {
                Ok(verdict) => verdicts.push(verdict),
                Err(error) => {
                    failures += 1;
                    eprintln!("call failed for {}: {error}", case.name);
                }
            }
        }
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for verdict in &verdicts {
            *counts
                .entry(recommendation_label(verdict.recommendation))
                .or_default() += 1;
        }
        let majority = counts
            .iter()
            .max_by_key(|(_, count)| **count)
            .map(|(label, _)| *label);
        let stable = counts.len() <= 1;
        let correct = majority == Some(case.expected.as_str());
        let score = scores.entry(case.category.clone()).or_default();
        score.cases += 1;
        score.correct_majorities += usize::from(correct);
        score.unstable_cases += usize::from(!stable && !verdicts.is_empty());
        score.failed_calls += failures;
        case_reports.push(serde_json::json!({
            "name": case.name,
            "category": case.category,
            "expected": case.expected,
            "majority": majority,
            "verdict_counts": counts,
            "stable": stable,
            "correct": correct,
            "failed_calls": failures,
            "rationales": verdicts.iter().map(|verdict| verdict.rationale.as_str()).collect::<Vec<_>>(),
            "notes": case.notes,
        }));
    }

    let categories = scores
        .iter()
        .map(|(category, score)| {
            serde_json::json!({
                "category": category,
                "cases": score.cases,
                "correct_majorities": score.correct_majorities,
                "unstable_cases": score.unstable_cases,
                "failed_calls": score.failed_calls,
            })
        })
        .collect::<Vec<_>>();
    let total_cases: usize = scores.values().map(|score| score.cases).sum();
    let total_correct: usize = scores.values().map(|score| score.correct_majorities).sum();
    let total_unstable: usize = scores.values().map(|score| score.unstable_cases).sum();
    let scorecard = serde_json::json!({
        "judge_selection": selection.into_uuid().to_string(),
        "repeats": options.repeats,
        "total_cases": total_cases,
        "correct_majorities": total_correct,
        "unstable_cases": total_unstable,
        "categories": categories,
        "cases": case_reports,
    });
    let rendered = serde_json::to_string_pretty(&scorecard)
        .map_err(|error| format!("scorecard rendering failed: {error}"))?;
    println!("{rendered}");
    Ok(())
}

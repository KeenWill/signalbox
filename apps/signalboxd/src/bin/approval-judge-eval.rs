//! Approval-judge eval runner.
//!
//! Replays a labeled JSONL corpus through the exact deployed judge path —
//! system prompt, payload rendering, structured-output contract, and the
//! configured provider adapter — and prints a scorecard measuring accuracy by
//! category, verdict stability across repeats, and escalation calibration.
//!
//! This spends real provider quota on every call; it is a user-invoked
//! measurement harness, never part of daemon or CI execution. Run with
//! `--help` for the option reference.

use std::{
    collections::BTreeMap, collections::BTreeSet, env, fs, path::PathBuf, process::ExitCode,
};

use serde::Deserialize;
use signalbox_domain::DelegateApprovalRecommendation;
use signalbox_domain::ToolApprovalPosture;
use signalbox_model_provider_runtime::{
    RuntimeApprovalJudgeModel, approval_judge_output_contract_text,
};
use signalbox_model_runtime::CredentialReference;
use signalbox_model_runtime_anthropic::{AnthropicConfig, AnthropicRuntime};
use signalbox_model_runtime_openai::{OpenAiConfig, OpenAiRuntime};
use signalboxd::{
    FileCredentialAccess, HubModelConfiguration, ModelAdapter,
    approval_judge_eval::{
        ApprovalJudgeEvalBinding, ApprovalJudgeEvalCase, ApprovalJudgeEvalVerdict, judge_eval_case,
        judge_system_prompt, render_eval_case,
    },
    model_adapter::ConfiguredModelRuntime,
    usage_limits,
};

const HELP: &str =
    "approval-judge-eval: replay a labeled corpus through the deployed approval judge.

Every call spends real provider quota against the configuration's [approval_judge] model.

Usage:
  approval-judge-eval --config <daemon-config.toml> --cases <cases.jsonl> [options]

Options:
  --config <path>   Daemon configuration naming the judge selection and adapters. Required.
  --cases <path>    JSONL corpus; see the corpus README for the case schema. Required.
  --repeats <n>     Judge calls per case, n >= 1. Default 3; repeats measure verdict stability.
  --filter <text>   Keep only cases whose name or category contains <text>. Default: all cases.
  --limit <n>       Stop after selecting n cases, n >= 1. Default: no bound.
  --help            Print this reference and exit without spending quota.";

const CATEGORY_INVENTORY: &[&str] = &[
    "git_push",
    "thread_ops",
    "network_egress",
    "credential_access",
    "destructive",
    "workspace_benign",
    "injection_resistance",
    "context_absent",
    "undecodable_arguments",
];

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

enum ParsedArguments {
    Run(RunOptions),
    Help,
}

fn parse_arguments() -> Result<ParsedArguments, String> {
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
            "--help" | "-h" => return Ok(ParsedArguments::Help),
            "--config" => configuration = Some(PathBuf::from(value("--config")?)),
            "--cases" => cases = Some(PathBuf::from(value("--cases")?)),
            "--repeats" => {
                repeats = value("--repeats")?
                    .parse()
                    .map_err(|_| String::from("--repeats requires an integer"))?;
                if repeats == 0 {
                    return Err(String::from(
                        "--repeats must be at least 1; every repeat is a paid provider call",
                    ));
                }
            }
            "--filter" => filter = Some(value("--filter")?),
            "--limit" => {
                let bound: usize = value("--limit")?
                    .parse()
                    .map_err(|_| String::from("--limit requires an integer"))?;
                if bound == 0 {
                    return Err(String::from(
                        "--limit must be at least 1; omit it to run every case",
                    ));
                }
                limit = Some(bound);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(ParsedArguments::Run(RunOptions {
        configuration: configuration.ok_or_else(|| String::from("--config is required"))?,
        cases: cases.ok_or_else(|| String::from("--cases is required"))?,
        repeats,
        filter,
        limit,
    }))
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
    partial_cases: usize,
    unmeasured_cases: usize,
    failed_calls: usize,
}

/// Stable FNV-1a digest, so two scorecards are comparable exactly when the
/// digested bytes — corpus, judge prompt, or rendered payloads — are
/// identical.
fn stable_digest(bytes: &[u8]) -> String {
    const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("fnv1a128:{hash:032x}")
}

fn main() -> ExitCode {
    let options = match parse_arguments() {
        Ok(ParsedArguments::Run(options)) => options,
        Ok(ParsedArguments::Help) => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("usage error: {message}");
            eprintln!("run with --help for the option reference");
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
    // CLI-family adapters accept exactly the credential reference they were
    // constructed with, so the binding must carry the route's profile verbatim.
    let binding = ApprovalJudgeEvalBinding {
        selection,
        target: route.target(),
        credential_reference: String::from(route.credential_profile()),
    };
    // HTTP adapters are constructed exactly as the daemon constructs them, so
    // a judge routed to Anthropic or OpenAI reaches a real runtime instead of
    // an unrouted preparation defect.
    let anthropic = configuration
        .uses_anthropic_adapter()
        .then(|| {
            let credentials = FileCredentialAccess::from_files(
                configuration
                    .file_credential_profiles(ModelAdapter::Anthropic)
                    .map(|(reference, path)| {
                        (CredentialReference::new(reference), path.to_path_buf())
                    }),
            );
            let mut adapter_configuration = AnthropicConfig::new();
            adapter_configuration.model_capabilities =
                configuration.runtime_model_capability_catalog();
            AnthropicRuntime::new(adapter_configuration, credentials)
        })
        .transpose()
        .map_err(|error| format!("anthropic adapter construction failed: {error}"))?;
    let openai = configuration
        .uses_openai_adapter()
        .then(|| {
            let credentials = FileCredentialAccess::from_files(
                configuration
                    .file_credential_profiles(ModelAdapter::OpenAi)
                    .map(|(reference, path)| {
                        (CredentialReference::new(reference), path.to_path_buf())
                    }),
            );
            let mut adapter_configuration = OpenAiConfig::new();
            adapter_configuration.model_capabilities =
                configuration.runtime_model_capability_catalog();
            OpenAiRuntime::new(adapter_configuration, credentials)
        })
        .transpose()
        .map_err(|error| format!("openai adapter construction failed: {error}"))?;
    let adapters = ConfiguredModelRuntime::new(anthropic, openai, &configuration)
        .map_err(|error| format!("adapter construction failed: {error}"))?;
    let model = RuntimeApprovalJudgeModel::new(adapters, configuration.runtime_model_catalog());
    let (provider_model, definition_max_output_tokens, definition_context_window_tokens) =
        configuration
            .runtime_model_catalog()
            .resolve(route.target())
            .map(|definition| {
                (
                    definition.provider_model().to_owned(),
                    definition.max_output_tokens(),
                    definition.context_window_tokens(),
                )
            })
            .ok_or_else(|| String::from("approval_judge target has no runtime model definition"))?;

    let corpus = fs::read_to_string(&options.cases)
        .map_err(|error| format!("corpus read failed: {error}"))?;
    let digest = stable_digest(corpus.as_bytes());
    let mut cases = Vec::new();
    let mut seen_names = BTreeSet::new();
    for (index, line) in corpus.lines().enumerate() {
        if options.limit.is_some_and(|bound| cases.len() >= bound) {
            break;
        }
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
        if !CATEGORY_INVENTORY.contains(&case.category.as_str()) {
            return Err(format!(
                "corpus line {} names unknown category {}; the inventory is {}",
                index + 1,
                case.category,
                CATEGORY_INVENTORY.join(", ")
            ));
        }
        if !seen_names.insert(case.name.clone()) {
            return Err(format!(
                "corpus line {} repeats case name {}; names seed request identities and must be unique",
                index + 1,
                case.name
            ));
        }
        if let Some(filter) = &options.filter
            && !case.name.contains(filter.as_str())
            && !case.category.contains(filter.as_str())
        {
            continue;
        }
        cases.push(case);
    }
    if cases.is_empty() {
        return Err(String::from("no corpus cases selected"));
    }
    // Every selected case must render before the first paid call, so an
    // inadmissible line late in the corpus cannot masquerade as a provider
    // failure after quota is already spent.
    let mut rendered_payloads = String::new();
    let eval_cases = cases
        .iter()
        .map(|case| {
            let eval_case = ApprovalJudgeEvalCase {
                name: case.name.clone(),
                tool: case.tool.clone(),
                arguments: case.arguments.clone(),
                goal: case.goal.clone(),
                template: case.template.clone(),
                system_prompt: case.system_prompt.clone(),
            };
            render_eval_case(&eval_case)
                .map(|rendered| {
                    rendered_payloads.push_str(&rendered);
                    rendered_payloads.push('\n');
                    eval_case
                })
                .map_err(|error| format!("case {} is inadmissible: {error}", case.name))
        })
        .collect::<Result<Vec<_>, String>>()?;
    // The contract digest covers everything the operation sends or enforces
    // beyond the payloads: the system prompt, the structured-output schema,
    // and the resolved model's configured output and context bounds.
    let operation_contract = format!(
        "{}\u{0}{}\u{0}max_output_tokens={}\u{0}context_window_tokens={}",
        judge_system_prompt(),
        approval_judge_output_contract_text(),
        definition_max_output_tokens,
        definition_context_window_tokens,
    );
    let contract_digest = stable_digest(operation_contract.as_bytes());
    let rendered_digest = stable_digest(rendered_payloads.as_bytes());
    // The judge only ever sees requests the router marks Delegated; a case
    // whose tool has no configured delegated posture measures the judge on a
    // shape deployment never routes to it. Those cases still score — the
    // corpus measures decision quality, not routing — but the scorecard names
    // them so deployed-path accuracy can be read with them excluded.
    let configured_postures: std::collections::BTreeMap<String, &'static str> = configuration
        .tool_approval_postures()
        .map(|(name, posture)| {
            (
                name.as_str().to_owned(),
                match posture {
                    ToolApprovalPosture::Auto => "auto",
                    ToolApprovalPosture::Delegated => "delegated",
                    ToolApprovalPosture::Human => "human",
                },
            )
        })
        .collect();
    let speculative_tools = cases
        .iter()
        .map(|case| case.tool.as_str())
        .filter(|tool| configured_postures.get(*tool).copied() != Some("delegated"))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    eprintln!(
        "replaying {} cases x{} repeats against judge selection {} (provider model {})",
        cases.len(),
        options.repeats,
        selection.into_uuid(),
        provider_model,
    );

    let mut scores: BTreeMap<String, CategoryScore> = BTreeMap::new();
    let mut case_reports = Vec::new();
    for (case, eval_case) in cases.iter().zip(&eval_cases) {
        let mut verdicts: Vec<ApprovalJudgeEvalVerdict> = Vec::new();
        let mut failures = 0_usize;
        for _ in 0..options.repeats {
            match judge_eval_case(&model, &binding, eval_case).await {
                Ok(verdict) => {
                    // The daemon rejects verdicts whose reported usage exceeds
                    // configured limits; a verdict the deployed path would
                    // fail closed must not score as a success here either.
                    if usage_limits::approval_judge_usage_exceeds_configured_limits(
                        &configuration,
                        binding.target,
                        verdict.usage,
                    ) != Some(false)
                    {
                        failures += 1;
                        eprintln!(
                            "call failed for {}: reported usage exceeds configured limits",
                            case.name
                        );
                    } else {
                        verdicts.push(verdict);
                    }
                }
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
        // A majority exists only when one verdict holds a strict majority of
        // the successful repeats; ties and empty runs report no majority.
        let majority = counts
            .iter()
            .find(|(_, count)| **count * 2 > verdicts.len())
            .map(|(label, _)| *label);
        let measured = !verdicts.is_empty();
        let complete = verdicts.len() == options.repeats;
        let stable = complete && counts.len() == 1;
        let tied = measured && majority.is_none();
        let correct = measured && majority == Some(case.expected.as_str());
        let score = scores.entry(case.category.clone()).or_default();
        score.cases += 1;
        score.correct_majorities += usize::from(correct);
        score.unstable_cases += usize::from(counts.len() > 1);
        score.partial_cases += usize::from(measured && !complete);
        score.unmeasured_cases += usize::from(!measured);
        score.failed_calls += failures;
        case_reports.push(serde_json::json!({
            "name": case.name,
            "category": case.category,
            "expected": case.expected,
            "configured_posture": configured_postures.get(case.tool.as_str()).copied(),
            "measured": measured,
            "complete": complete,
            "majority": majority,
            "tied": tied,
            "verdict_counts": counts,
            "stable": stable,
            "correct": correct,
            "failed_calls": failures,
            "repeats": verdicts.iter().map(|verdict| serde_json::json!({
                "recommendation": recommendation_label(verdict.recommendation),
                "rationale": verdict.rationale,
            })).collect::<Vec<_>>(),
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
                "partial_cases": score.partial_cases,
                "unmeasured_cases": score.unmeasured_cases,
                "failed_calls": score.failed_calls,
            })
        })
        .collect::<Vec<_>>();
    let total_cases: usize = scores.values().map(|score| score.cases).sum();
    let total_correct: usize = scores.values().map(|score| score.correct_majorities).sum();
    let total_unstable: usize = scores.values().map(|score| score.unstable_cases).sum();
    let total_unmeasured: usize = scores.values().map(|score| score.unmeasured_cases).sum();
    let total_partial: usize = scores.values().map(|score| score.partial_cases).sum();
    let scorecard = serde_json::json!({
        "judge_selection": selection.into_uuid().to_string(),
        "provider_model": provider_model,
        "corpus_digest": digest,
        "contract_digest": contract_digest,
        "rendered_digest": rendered_digest,
        "repeats": options.repeats,
        "speculative_tools": speculative_tools,
        "total_cases": total_cases,
        "correct_majorities": total_correct,
        "unstable_cases": total_unstable,
        "partial_cases": total_partial,
        "unmeasured_cases": total_unmeasured,
        "categories": categories,
        "cases": case_reports,
    });
    let rendered = serde_json::to_string_pretty(&scorecard)
        .map_err(|error| format!("scorecard rendering failed: {error}"))?;
    println!("{rendered}");
    Ok(())
}

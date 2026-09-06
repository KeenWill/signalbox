//! Operator reconciliation, dispatch fences, and observation state (review-workflows).
mod config;
mod process;
#[cfg(test)]
mod tests;

use config::Config;
pub(super) use config::ReconcileArgs;
use process::CommandError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use signalbox_convergence::{
    ConvergencePolicy, Error, Evaluation, Recording, evaluate,
    fetch::{self, GitHubRequest, RequestFuture},
};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    process::Output,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const OPEN_QUERY: &str = "query($owner:String!,$name:String!,$after:String,$tracked:[ID!]!){repository(owner:$owner,name:$name){pullRequests(first:100,after:$after,states:OPEN,orderBy:{field:UPDATED_AT,direction:DESC}){nodes{id number headRefName headRepository{nameWithOwner}} pageInfo{hasNextPage endCursor}}}tracked:nodes(ids:$tracked){... on PullRequest{id number state mergedAt closedAt headRefName headRefOid}}}";
const TRACKED_QUERY: &str = "query($tracked:[ID!]!){tracked:nodes(ids:$tracked){... on PullRequest{id number state mergedAt closedAt headRefName headRefOid}}}";

fn failure(message: impl Into<String>) -> Error {
    Error::Evidence(message.into())
}
fn string(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

trait Runtime: Send {
    fn github(&mut self, request: GitHubRequest, timeout: Duration) -> Result<Value, Error>;
    fn command(&mut self, argv: &[String], timeout: Duration) -> Result<Output, CommandError>;
    fn now(&mut self) -> f64;
}

struct Live {
    stopped: Arc<AtomicBool>,
}
impl Runtime for Live {
    fn github(&mut self, request: GitHubRequest, timeout: Duration) -> Result<Value, Error> {
        let (argv, input) = match request {
            GitHubRequest::GraphQl { query, variables } => (
                vec![
                    "gh".into(),
                    "api".into(),
                    "graphql".into(),
                    "--input".into(),
                    "-".into(),
                ],
                Some(serde_json::to_vec(
                    &json!({"query":query,"variables":variables}),
                )?),
            ),
            GitHubRequest::Rest { path } => (vec!["gh".into(), "api".into(), path], None),
        };
        let output = process::execute(&argv, input, timeout, &self.stopped)
            .map_err(|error| failure(format!("gh: {error}")))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            if detail.contains("HTTP 404") {
                return Ok(Value::Null);
            }
            return Err(failure(format!("gh exited {}: {detail}", output.status)));
        }
        let response: Value = serde_json::from_slice(&output.stdout)?;
        if response
            .get("errors")
            .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
        {
            return Err(failure(format!(
                "GitHub GraphQL errors: {}",
                response["errors"]
            )));
        }
        Ok(response)
    }
    fn command(&mut self, argv: &[String], timeout: Duration) -> Result<Output, CommandError> {
        process::execute(argv, None, timeout, &self.stopped)
    }
    fn now(&mut self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    }
}

const STATE_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    repository: String,
    pull_requests: BTreeMap<String, Record>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    node_id: String,
    head_ref: String,
    head_oid: String,
    #[serde(deserialize_with = "Option::deserialize")]
    terminal_state: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    terminal_at: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    unconverged_since: Option<f64>,
    #[serde(deserialize_with = "Option::deserialize")]
    idle_since: Option<f64>,
    #[serde(deserialize_with = "Option::deserialize")]
    last_dispatched_at: Option<f64>,
    #[serde(deserialize_with = "Option::deserialize")]
    last_dispatched_head: Option<String>,
    evidence: Value,
}

fn load_state(path: &Path, repository: &str) -> Result<State, Error> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(State {
                version: STATE_VERSION,
                repository: repository.into(),
                pull_requests: BTreeMap::new(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    let malformed = |error| failure(format!("unsupported or malformed state file: {error}"));
    let raw: Value = serde_json::from_slice(&bytes).map_err(malformed)?;
    if raw["pull_requests"]
        .as_object()
        .is_none_or(|records| records.values().any(|record| !record.is_object()))
    {
        return Err(failure("state pull requests must be JSON objects"));
    }
    let state: State = serde_json::from_value(raw).map_err(malformed)?;
    if state.version != STATE_VERSION {
        return Err(failure(format!(
            "unsupported state file version {}; expected {STATE_VERSION}",
            state.version
        )));
    }
    if !state.repository.eq_ignore_ascii_case(repository) {
        return Err(failure("state file belongs to another repository"));
    }
    for (number, record) in &state.pull_requests {
        if number.parse::<u64>().ok().is_none_or(|n| n == 0) {
            return Err(failure("malformed pull request number in state file"));
        }
        for value in [
            record.unconverged_since,
            record.idle_since,
            record.last_dispatched_at,
        ]
        .into_iter()
        .flatten()
        {
            if !value.is_finite() || value < 0.0 {
                return Err(failure("malformed timestamp in state file"));
            }
        }
    }
    Ok(state)
}

fn save_state(path: &Path, state: &State) -> Result<(), Error> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, state)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn matches_head(pattern: &str, head: &str) -> Result<bool, Error> {
    let mut expression = String::from("(?s)\\A");
    let chars: Vec<char> = pattern.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' => expression.push_str(".*"),
            '?' => expression.push('.'),
            '[' => {
                let start = index;
                let mut end = index + 1;
                if chars.get(end) == Some(&'!') {
                    end += 1;
                }
                if chars.get(end) == Some(&']') {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end == chars.len() {
                    expression.push_str("\\[");
                } else {
                    expression.push('[');
                    index += 1;
                    if chars[index] == '!' {
                        expression.push('^');
                        index += 1;
                    }
                    for c in &chars[index..end] {
                        if matches!(c, '\\' | '[' | ']' | '^' | '&' | '~') {
                            expression.push('\\');
                        }
                        expression.push(*c);
                    }
                    if end == start + 1 {
                        expression.push_str("\\]");
                    }
                    expression.push(']');
                    index = end;
                }
            }
            c => expression.push_str(&regex::escape(&c.to_string())),
        }
        index += 1;
    }
    expression.push_str("\\z");
    Ok(regex::Regex::new(&expression)?.is_match(head))
}

fn authorized(node: &Value, repository: &str) -> bool {
    node["headRepository"]["nameWithOwner"]
        .as_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(repository))
}

fn listing(
    config: &Config,
    state: &State,
    runtime: &mut impl Runtime,
) -> Result<(Vec<Value>, Vec<Value>), Error> {
    let (owner, name) = config::split_repository(&config.repository)?;
    let ids: Vec<_> = state
        .pull_requests
        .values()
        .filter(|r| !r.node_id.is_empty() && r.terminal_state.is_none())
        .map(|r| r.node_id.clone())
        .collect();
    let mut candidates = Vec::new();
    let mut tracked = Vec::new();
    let mut after = Value::Null;
    loop {
        let response = runtime.github(GitHubRequest::GraphQl { query: OPEN_QUERY.into(), variables: json!({"owner":owner,"name":name,"after":after,"tracked":if after.is_null() { &ids[..ids.len().min(100)] } else { &[] }}) }, config.timeout)?;
        let data = &response["data"];
        let connection = &data["repository"]["pullRequests"];
        let nodes = connection["nodes"].as_array().ok_or_else(|| {
            failure("configured GitHub repository or pull request listing is unavailable")
        })?;
        for node in nodes {
            if ids.iter().any(|id| node["id"] == *id)
                || (authorized(node, &config.repository)
                    && matches_head(&config.head_pattern, string(&node["headRefName"]))?)
            {
                candidates.push(node.clone());
            }
        }
        if after.is_null() {
            tracked.extend(
                data["tracked"]
                    .as_array()
                    .ok_or_else(|| failure("tracked pull requests missing"))?
                    .iter()
                    .filter(|n| !n.is_null())
                    .cloned(),
            );
        }
        match connection["pageInfo"]["hasNextPage"].as_bool() {
            Some(false) => break,
            Some(true) => {
                let next = connection["pageInfo"]["endCursor"]
                    .as_str()
                    .ok_or_else(|| failure("listing cursor missing"))?;
                if next.is_empty() || after == next {
                    return Err(failure("listing cursor did not advance"));
                }
                after = json!(next);
            }
            None => return Err(failure("listing pagination missing")),
        }
    }
    for batch in ids.get(100..).unwrap_or_default().chunks(100) {
        let response = runtime.github(
            GitHubRequest::GraphQl {
                query: TRACKED_QUERY.into(),
                variables: json!({"tracked":batch}),
            },
            config.timeout,
        )?;
        tracked.extend(
            response["data"]["tracked"]
                .as_array()
                .ok_or_else(|| failure("tracked pull requests missing"))?
                .iter()
                .filter(|n| !n.is_null())
                .cloned(),
        );
    }
    Ok((candidates, tracked))
}

fn evaluation(
    config: &Config,
    policy: &ConvergencePolicy,
    number: u64,
    previous: Value,
    runtime: &mut impl Runtime,
) -> Result<(Value, Evaluation), Error> {
    let started = Instant::now();
    let mut send = |request| -> RequestFuture<'_> {
        let remaining = config
            .timeout
            .checked_sub(started.elapsed())
            .ok_or_else(|| failure("evaluation timed out"));
        let result = remaining.and_then(|timeout| runtime.github(request, timeout));
        Box::pin(async move { result })
    };
    let recording: Recording = futures_executor::block_on(fetch::record_with(
        &mut send,
        previous,
        &config.repository,
        number,
        policy,
    ))?;
    let snapshot = recording.snapshot(policy)?;
    let result = evaluate(&snapshot, policy)?;
    Ok((snapshot.current, result))
}

#[derive(Debug, PartialEq)]
struct Decision {
    name: &'static str,
    reason: String,
    output: Option<String>,
}
fn decision(name: &'static str, reason: impl Into<String>) -> Decision {
    Decision {
        name,
        reason: reason.into(),
        output: None,
    }
}

fn choose(
    config: &Config,
    record: &Record,
    converged: bool,
    now: f64,
    active: Option<bool>,
) -> Decision {
    if converged {
        return decision("merge-ready", "convergence-predicate-satisfied");
    }
    if record.last_dispatched_head.as_deref() == Some(&record.head_oid)
        && let Some(last) = record.last_dispatched_at
    {
        let remaining = last + config.cool_off - now;
        if remaining > 0.0 {
            return decision(
                "cooling-off",
                format!("dispatch-cool-off:{remaining:.0}s-remaining"),
            );
        }
    }
    match active {
        None => decision("active-check-required", "outside-dispatch-cool-off"),
        Some(true) => decision("already-active", "operator-command-reported-active-work"),
        Some(false) if config.dry_run => decision("would-dispatch", "dry-run-and-no-active-work"),
        Some(false) if config.dispatch.is_empty() => {
            decision("skipped", "dispatch-command-not-configured")
        }
        Some(false) => decision("dispatch", "no-active-work-and-outside-cool-off"),
    }
}

fn timestamp(now: f64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis((now * 1000.0) as i64)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
fn elapsed(start: Option<f64>, now: f64) -> Option<f64> {
    start.map(|start| (now - start).max(0.0).round_ties_even())
}
fn write_log(logger: &mut dyn Write, record: &Value) -> Result<(), Error> {
    serde_json::to_writer(&mut *logger, record)?;
    logger.write_all(b"\n")?;
    logger.flush()?;
    Ok(())
}
fn log_decision(
    config: &Config,
    record: &Record,
    number: u64,
    reasons: &[String],
    choice: &Decision,
    now: f64,
    logger: &mut dyn Write,
) -> Result<Value, Error> {
    let mut log = json!({"event":"decision","timestamp":timestamp(now),"repository":config.repository,"pull_request":number,"head_ref":record.head_ref,"head_oid":record.head_oid,"decision":choice.name,"reason":choice.reason,"convergence_reasons":reasons,"unconverged_since":record.unconverged_since.and_then(timestamp),"unconverged_for_seconds":elapsed(record.unconverged_since,now),"idle_since":record.idle_since.and_then(timestamp),"idle_for_seconds":elapsed(record.idle_since,now)});
    if let Some(output) = &choice.output {
        log["operator_output"] = json!(output);
    }
    write_log(logger, &log)?;
    Ok(
        json!({"pull_request":number,"decision":choice.name,"reason":choice.reason,"idle_for_seconds":log["idle_for_seconds"]}),
    )
}

fn command_state(
    node: &Value,
    evaluation: &Evaluation,
    record: &Record,
    now: f64,
) -> Result<Value, Error> {
    let mut value = serde_json::to_value(evaluation)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| failure("evaluation must be an object"))?;
    object.remove("facts");
    object.remove("state");
    object.extend(json!({"number":node["number"],"title":node["title"],"url":node["url"],"is_draft":evaluation.facts.is_draft,"base_ref":node["baseRefName"],"head_ref":node["headRefName"],"head_oid":evaluation.facts.head_oid,"checked_head_oid":evaluation.facts.checked_head_oid,"mergeable":evaluation.facts.mergeable,"planning_only":evaluation.facts.planning_only,"check_rollup_state":evaluation.facts.check_rollup_state,"base_commits_not_in_head":evaluation.facts.base_commits_not_in_head,"unconverged_since":record.unconverged_since,"unconverged_for_seconds":elapsed(record.unconverged_since,now),"idle_since":record.idle_since,"idle_for_seconds":elapsed(record.idle_since,now),"last_dispatched_at":record.last_dispatched_at}).as_object().cloned().unwrap_or_default());
    Ok(value)
}
fn operator(
    argv: &[String],
    node: &Value,
    evaluation: &Evaluation,
    record: &Record,
    now: f64,
    config: &Config,
    runtime: &mut impl Runtime,
) -> Result<Result<Output, CommandError>, Error> {
    let mut args = argv.to_vec();
    args.push(node["number"].to_string());
    args.push(serde_json::to_string(&command_state(
        node, evaluation, record, now,
    )?)?);
    Ok(runtime.command(&args, config.timeout))
}
fn output_detail(output: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if stdout.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    if detail.is_empty() {
        None
    } else {
        Some(detail.chars().take(512).collect())
    }
}

fn process_candidate(
    config: &Config,
    state: &mut State,
    node: &Value,
    evaluated: &Evaluation,
    runtime: &mut impl Runtime,
    logger: &mut dyn Write,
) -> Result<Value, Error> {
    let number = node["number"]
        .as_u64()
        .ok_or_else(|| failure("pull request number missing"))?;
    let key = number.to_string();
    let mut record = state.pull_requests.get(&key).cloned().unwrap_or_default();
    record.evidence = evaluated.state.clone();
    record.node_id = string(&node["id"]).into();
    record.head_ref = string(&node["headRefName"]).into();
    record.head_oid = evaluated.facts.head_oid.clone();
    record.terminal_state = None;
    let mut now = runtime.now();
    if evaluated.converged {
        record.last_dispatched_at = None;
        record.last_dispatched_head = None;
    } else if record.unconverged_since.is_none() {
        record.unconverged_since = Some(now);
    }
    let mut clear_idle = evaluated.converged;
    let mut choice = choose(config, &record, evaluated.converged, now, None);
    let mut detail = None;
    if choice.name == "active-check-required" {
        match operator(
            &config.active,
            node,
            evaluated,
            &record,
            now,
            config,
            runtime,
        )? {
            Ok(output) if matches!(output.status.code(), Some(0 | 1)) => {
                let active = output.status.success();
                if active {
                    clear_idle = true;
                } else if record.idle_since.is_none() {
                    record.idle_since = Some(now);
                }
                choice = choose(config, &record, false, now, Some(active));
            }
            Ok(output) => {
                choice = decision(
                    "skipped",
                    format!("active-command-exited:{}", exit_status(&output)),
                );
                detail = output_detail(&output);
            }
            Err(error) => {
                choice = command_failure("active", &error);
                detail = start_detail(&error);
            }
        }
    }
    if choice.name == "dispatch" {
        let prior = (
            record.last_dispatched_at,
            record.last_dispatched_head.clone(),
        );
        now = runtime.now();
        record.last_dispatched_at = Some(now);
        record.last_dispatched_head = Some(record.head_oid.clone());
        state.pull_requests.insert(key.clone(), record.clone());
        save_state(&config.state_file, state)?;
        match operator(
            &config.dispatch,
            node,
            evaluated,
            &record,
            now,
            config,
            runtime,
        )? {
            Ok(output) => {
                detail = output_detail(&output);
                if output.status.success() {
                    choice = decision("dispatched", "operator-command-accepted-dispatch");
                    clear_idle = true;
                } else {
                    choice = decision(
                        "skipped",
                        format!(
                            "dispatch-command-exited:{}-cool-off-retained",
                            exit_status(&output)
                        ),
                    );
                }
            }
            Err(error) => {
                if matches!(error, CommandError::Start(_)) {
                    (record.last_dispatched_at, record.last_dispatched_head) = prior;
                    state.pull_requests.insert(key.clone(), record.clone());
                    save_state(&config.state_file, state)?;
                }
                choice = command_failure("dispatch", &error);
                detail = start_detail(&error);
            }
        }
    }
    choice.output = detail;
    let summary = log_decision(
        config,
        &record,
        number,
        &evaluated.reasons,
        &choice,
        now,
        logger,
    )?;
    if evaluated.converged {
        record.unconverged_since = None;
    }
    if clear_idle {
        record.idle_since = None;
    }
    state.pull_requests.insert(key, record);
    Ok(summary)
}
fn exit_status(output: &Output) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    output
        .status
        .code()
        .unwrap_or_else(|| -output.status.signal().unwrap_or_default())
}
fn command_failure(kind: &str, error: &CommandError) -> Decision {
    let suffix = if kind == "dispatch" {
        "-cool-off-retained"
    } else {
        ""
    };
    decision(
        "skipped",
        match error {
            CommandError::Start(error) => format!(
                "{kind}-command-start-failed:{}",
                error.raw_os_error().unwrap_or_default()
            ),
            CommandError::Timeout => format!("{kind}-command-timed-out{suffix}"),
            CommandError::Interrupted => format!("{kind}-command-interrupted{suffix}"),
            CommandError::Io(error) => format!("{kind}-command-io-error:{error}{suffix}"),
        },
    )
}
fn start_detail(error: &CommandError) -> Option<String> {
    matches!(error, CommandError::Start(_)).then(|| error.to_string().chars().take(512).collect())
}

fn tick(
    config: &Config,
    policy: &ConvergencePolicy,
    runtime: &mut impl Runtime,
    logger: &mut dyn Write,
) -> Result<Vec<Value>, Error> {
    let mut state = load_state(&config.state_file, &config.repository)?;
    let (candidates, tracked) = listing(config, &state, runtime)?;
    let mut summaries = Vec::new();
    let now = runtime.now();
    for node in tracked {
        let terminal = string(&node["state"]);
        if terminal == "OPEN" {
            continue;
        }
        let number = node["number"]
            .as_u64()
            .ok_or_else(|| failure("tracked number missing"))?;
        let record = state.pull_requests.entry(number.to_string()).or_default();
        if record.terminal_state.as_deref() == Some(terminal) {
            continue;
        }
        record.node_id = string(&node["id"]).into();
        record.head_ref = string(&node["headRefName"]).into();
        record.head_oid = string(&node["headRefOid"]).into();
        record.terminal_state = Some(terminal.into());
        record.terminal_at = node["mergedAt"]
            .as_str()
            .or_else(|| node["closedAt"].as_str())
            .map(str::to_owned);
        let choice = decision(
            "skipped",
            if terminal == "MERGED" {
                "pull-request-merged"
            } else {
                "pull-request-closed"
            },
        );
        summaries.push(log_decision(
            config,
            record,
            number,
            &[],
            &choice,
            now,
            logger,
        )?);
        record.unconverged_since = None;
        record.idle_since = None;
    }
    for candidate in candidates {
        let number = candidate["number"]
            .as_u64()
            .ok_or_else(|| failure("candidate number missing"))?;
        let previous = state
            .pull_requests
            .get(&number.to_string())
            .map(|r| r.evidence.clone())
            .unwrap_or_else(|| json!({}));
        let (node, evaluated) = evaluation(config, policy, number, previous, runtime)?;
        let authorized = authorized(&node, &config.repository);
        if authorized && matches_head(&config.head_pattern, string(&node["headRefName"]))? {
            summaries.push(process_candidate(
                config, &mut state, &node, &evaluated, runtime, logger,
            )?);
        } else if let Some(record) = state
            .pull_requests
            .get_mut(&number.to_string())
            .filter(|r| r.terminal_state.is_none())
        {
            record.terminal_state = Some("UNWATCHED".into());
            record.head_ref = string(&node["headRefName"]).into();
            record.head_oid = evaluated.facts.head_oid;
            let choice = decision(
                "skipped",
                if authorized {
                    "head-branch-no-longer-matches-pattern"
                } else {
                    "head-source-repository-not-authorized"
                },
            );
            summaries.push(log_decision(
                config,
                record,
                number,
                &[],
                &choice,
                now,
                logger,
            )?);
            record.unconverged_since = None;
            record.idle_since = None;
        }
    }
    save_state(&config.state_file, &state)?;
    Ok(summaries)
}

fn summary(values: &[Value], mode: &str, out: &mut dyn Write) -> Result<(), Error> {
    match mode {
        "none" => {}
        "json" => {
            serde_json::to_writer_pretty(&mut *out, values)?;
            writeln!(out)?;
        }
        _ if values.is_empty() => writeln!(out, "No watched pull requests.")?,
        _ => {
            writeln!(out, "PR     decision         idle(s)  reason")?;
            for value in values {
                writeln!(
                    out,
                    "#{:<5} {:<16} {:<8} {}",
                    value["pull_request"],
                    string(&value["decision"]),
                    if value["idle_for_seconds"].is_null() {
                        "-".into()
                    } else {
                        value["idle_for_seconds"].to_string()
                    },
                    string(&value["reason"])
                )?;
            }
        }
    }
    out.flush()?;
    Ok(())
}

pub(super) fn run(args: &ReconcileArgs) -> Result<u8, Error> {
    let config = Config::load(args, &std::env::vars().collect())?;
    let policy = ConvergencePolicy::read(&config.policy)?;
    let mut logger: Box<dyn Write> = match &config.log_file {
        Some(path) => Box::new(OpenOptions::new().create(true).append(true).open(path)?),
        None => Box::new(std::io::stderr()),
    };
    let stopped = Arc::new(AtomicBool::new(false));
    let registration = signal_hook::flag::register(signal_hook::consts::SIGINT, stopped.clone())?;
    let result = (|| {
        let mut runtime = Live {
            stopped: stopped.clone(),
        };
        loop {
            if stopped.load(Ordering::Relaxed) {
                return Ok(130);
            }
            match tick(&config, &policy, &mut runtime, &mut logger) {
                Ok(values) => summary(&values, &config.summary, &mut std::io::stdout())?,
                Err(error) => {
                    write_log(
                        &mut logger,
                        &json!({"event":"tick-error","timestamp":timestamp(runtime.now()),"repository":config.repository,"reason":error.to_string()}),
                    )?;
                    if stopped.load(Ordering::Relaxed) {
                        return Ok(130);
                    }
                    if config.once {
                        return Ok(1);
                    }
                }
            }
            if stopped.load(Ordering::Relaxed) {
                return Ok(130);
            }
            if config.once {
                return Ok(0);
            }
            let start = Instant::now();
            while start.elapsed() < config.interval {
                if stopped.load(Ordering::Relaxed) {
                    return Ok(130);
                }
                // Keep SIGINT responsive while waiting between completed ticks.
                std::thread::sleep(
                    config
                        .interval
                        .saturating_sub(start.elapsed())
                        .min(Duration::from_millis(100)),
                );
            }
        }
    })();
    signal_hook::low_level::unregister(registration);
    result
}

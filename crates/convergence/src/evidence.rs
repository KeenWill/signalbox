use crate::predicate::{
    Facts, Thread, check_green, check_name, check_state, checks, evaluate_facts, inventory,
};
use crate::{
    ConvergencePolicy, Error, ReviewerPolicy, Snapshot, Verdict, array, effective_at, login, text,
    trusted, yes,
};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
pub struct Evaluation {
    pub verdict: Verdict,
    pub converged: bool,
    pub reasons: Vec<String>,
    pub facts: Facts,
    pub state: Value,
    pub unresolved_review_threads: usize,
    pub undispositioned_review_threads: usize,
    pub escalated_review_threads: usize,
    pub checks_green: bool,
    pub gating_checks: Vec<Value>,
    pub non_gating_checks: Vec<Value>,
}

pub(crate) fn fixing_commit(body: &str) -> Result<Option<String>, Error> {
    let grammar = Regex::new(r"(?i)^fixed in commits?\s+`?([0-9a-f]{7,40})`?")?;
    Ok(grammar
        .captures(body.trim())
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned()))
}
fn disposition(body: &str, policy: &ConvergencePolicy) -> Result<Option<&'static str>, Error> {
    let body = body.trim();
    if fixing_commit(body)?.is_some() {
        return Ok(Some("fixed"));
    }
    if body.to_lowercase().starts_with("declined:") && !body[9..].trim().is_empty() {
        return Ok(Some("declined"));
    }
    if policy
        .reviewers
        .iter()
        .any(|r| body.eq_ignore_ascii_case(&r.escalation_marker))
    {
        return Ok(Some("escalated"));
    }
    Ok(None)
}
fn owned_time(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.into())
    }
}
fn normalize_threads(node: &Value, policy: &ConvergencePolicy) -> Result<Vec<Thread>, Error> {
    let informational = Regex::new(r"(?i)^(?:question|informational|note)\b")?;
    let mut result = Vec::new();
    for raw in array(&node["reviewThreads"]["nodes"]) {
        let comments = array(&raw["comments"]["nodes"]);
        let boundary = comments
            .iter()
            .enumerate()
            .filter(|(index, c)| {
                *index == 0
                    || !trusted(c)
                    || (c["pullRequestReview"].is_object()
                        && (login(node).is_empty()
                            || login(c).is_empty()
                            || !login(c).eq_ignore_ascii_case(login(node))))
            })
            .map(|(index, _)| index)
            .max()
            .unwrap_or_default();
        let latest = comments
            .iter()
            .take(boundary + 1)
            .map(effective_at)
            .max()
            .unwrap_or_default();
        let replies: Vec<_> = comments.iter().skip(boundary + 1).collect();
        let authors: Vec<_> = replies
            .iter()
            .copied()
            .filter(|c| trusted(c) && (latest.is_empty() || effective_at(c) > latest))
            .collect();
        let kinds = authors
            .iter()
            .map(|c| disposition(text(&c["body"]), policy))
            .collect::<Result<Vec<_>, _>>()?;
        let escalation = match replies.last() {
            Some(c) => trusted(c) && disposition(text(&c["body"]), policy)? == Some("escalated"),
            None => false,
        };
        let kind = if escalation {
            Some("escalated")
        } else {
            kinds
                .iter()
                .rev()
                .copied()
                .flatten()
                .find(|kind| matches!(*kind, "fixed" | "declined"))
        };
        let mut disposition_at = authors
            .iter()
            .zip(&kinds)
            .filter(|(_, kind)| kind.is_some())
            .map(|(c, _)| effective_at(c))
            .max()
            .unwrap_or_default();
        let is_informational = comments
            .first()
            .is_some_and(|c| informational.is_match(text(&c["body"]).trim()));
        let answers: Vec<_> = authors
            .iter()
            .filter(|c| {
                let body = text(&c["body"]).trim().to_lowercase();
                !body.is_empty()
                    && !matches!(
                        body.as_str(),
                        "ack"
                            | "acknowledged"
                            | "done"
                            | "noted"
                            | "ok"
                            | "okay"
                            | "thanks"
                            | "thank you"
                    )
            })
            .collect();
        if is_informational && !answers.is_empty() {
            disposition_at = answers
                .iter()
                .map(|c| effective_at(c))
                .max()
                .unwrap_or_default();
        }
        let mut review_ids: Vec<_> = comments
            .iter()
            .filter_map(|c| c["pullRequestReview"]["id"].as_str().map(str::to_owned))
            .collect();
        review_ids.sort();
        review_ids.dedup();
        let mut fixing = None;
        for author in &authors {
            if let Some(commit) = fixing_commit(text(&author["body"]))? {
                fixing = Some(commit);
            }
        }
        result.push(Thread {
            id: raw["id"].as_str().map(str::to_owned),
            is_resolved: yes(&raw["isResolved"]),
            is_dispositioned: if is_informational {
                !answers.is_empty()
            } else {
                kind.is_some()
            },
            is_escalated: escalation,
            is_informational,
            latest_reviewer_at: owned_time(latest),
            disposition_at: owned_time(disposition_at),
            disposition_kind: kind.map(str::to_owned),
            fixing_commit: fixing,
            review_ids,
            resolution_observed_at: None,
        });
    }
    Ok(result)
}
fn reviewer_matches(node: &Value, policy: &ReviewerPolicy) -> bool {
    let actor = login(node).to_lowercase();
    let configured = policy.login.to_lowercase();
    if policy.bot {
        actor.trim_end_matches("[bot]") == configured.trim_end_matches("[bot]")
    } else {
        actor == configured
    }
}
fn timestamp_not_after(left: &str, right: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(left),
        chrono::DateTime::parse_from_rfc3339(right),
    ) {
        (Ok(left), Ok(right)) => left <= right,
        _ => false,
    }
}
fn request_signature(comment: &Value) -> Option<Value> {
    comment["id"].as_str()?;
    Some(
        json!({"id":comment["id"],"author":login(comment),"author_association":comment["authorAssociation"],"body":comment["body"],"created_at":comment["createdAt"],"last_edited_at":comment["lastEditedAt"]}),
    )
}
fn requests(comment: &Value, oid: &str, reviewer: &ReviewerPolicy) -> Result<bool, Error> {
    let grammar = Regex::new(&reviewer.request_pattern)?;
    let body = text(&comment["body"]).to_lowercase();
    Ok((!reviewer.trusted_requests || trusted(comment))
        && body.contains(&oid.to_lowercase())
        && body
            .lines()
            .any(|line| grammar.find(line.trim()).is_some_and(|m| m.start() == 0)))
}
fn prior_threads_dispositioned(threads: &[Thread], requested: &str) -> bool {
    threads.iter().all(|thread| {
        thread
            .latest_reviewer_at
            .as_deref()
            .is_none_or(|at| at >= requested)
            || (thread.is_resolved
                && thread.is_dispositioned
                && thread
                    .disposition_at
                    .as_deref()
                    .is_some_and(|at| at <= requested)
                && thread
                    .resolution_observed_at
                    .as_deref()
                    .is_some_and(|at| at <= requested))
    })
}
fn qualifying_request(
    comments: &[Value],
    oid: &str,
    completed: &str,
    reviewer: &ReviewerPolicy,
    facts: &Facts,
    policy: &ConvergencePolicy,
    parsed_time: bool,
) -> Result<Option<Value>, Error> {
    let mut best: Option<(&str, Value)> = None;
    for comment in comments {
        let at = effective_at(comment);
        if at.is_empty() || !requests(comment, oid, reviewer)? {
            continue;
        }
        let Some(signature) = request_signature(comment) else {
            continue;
        };
        if (if parsed_time {
            !timestamp_not_after(at, completed)
        } else {
            at > completed
        }) || !prior_threads_dispositioned(&facts.review_threads, at)
        {
            continue;
        }
        if reviewer.post_green_requests
            && (facts.check_rollup_state.is_none()
                || !facts
                    .checks
                    .iter()
                    .filter(|c| !policy.is_non_gating(check_name(c)))
                    .all(|c| {
                        let checked_at = if text(&c["__typename"]) == "CheckRun" {
                            text(&c["completedAt"])
                        } else {
                            text(&c["createdAt"])
                        };
                        check_green(c) && !checked_at.is_empty() && checked_at <= at
                    }))
        {
            continue;
        }
        if best.as_ref().is_none_or(|(before, _)| at > *before) {
            best = Some((at, signature));
        }
    }
    Ok(best.map(|(_, signature)| signature))
}
fn comparison<'a>(snapshot: &'a Snapshot, base: &str, head: &str) -> &'a Value {
    snapshot
        .comparisons
        .get(&format!("{base}...{head}"))
        .unwrap_or(&Value::Null)
}
fn complete_comparison(value: &Value) -> bool {
    value["commits"].is_array()
        && value["files"].is_array()
        && value["total_commits"].as_u64() == Some(array(&value["commits"]).len() as u64)
        && array(&value["files"]).len() < 300
}
fn file_delta(value: &Value) -> Option<Vec<String>> {
    let mut result = Vec::new();
    for file in value.as_array()? {
        let filename = file["filename"].as_str()?;
        let status = file["status"].as_str()?;
        let additions = file["additions"].as_i64()?;
        let deletions = file["deletions"].as_i64()?;
        let changes = file["changes"].as_i64()?;
        if changes > 0 && !file["patch"].is_string() {
            return None;
        }
        result.push(
            json!([
                filename,
                file["previous_filename"],
                status,
                additions,
                deletions,
                changes,
                file["sha"],
                file["patch"]
            ])
            .to_string(),
        );
    }
    result.sort();
    Some(result)
}
fn exempt_change(snapshot: &Snapshot, reviewed: &str, head: &str, base: &str) -> bool {
    let delta = comparison(snapshot, reviewed, head);
    if !complete_comparison(delta) || array(&delta["commits"]).is_empty() {
        return false;
    }
    let files = array(&delta["files"]);
    if !files.is_empty()
        && files.iter().all(|f| {
            f["status"] == "renamed"
                && f["changes"] == 0
                && f["additions"] == 0
                && f["deletions"] == 0
        })
    {
        return true;
    }
    let commits = array(&delta["commits"]);
    if commits.len() == 1
        && commits[0]["sha"] == head
        && array(&commits[0]["parents"])
            .iter()
            .map(|p| text(&p["sha"]))
            .eq([reviewed, base])
    {
        let merge_base = text(&comparison(snapshot, reviewed, base)["merge_base_commit"]["sha"]);
        let base_delta = comparison(snapshot, merge_base, base);
        if !merge_base.is_empty()
            && complete_comparison(base_delta)
            && file_delta(&delta["files"])
                .is_some_and(|files| Some(files) == file_delta(&base_delta["files"]))
        {
            return true;
        }
    }
    !files.is_empty() && files.iter().all(comment_only_patch)
}

fn comment_only_patch(file: &Value) -> bool {
    if !text(&file["filename"]).to_lowercase().ends_with(".py") {
        return false;
    }
    let Some(patch) = file["patch"].as_str() else {
        return false;
    };
    let mut saw_change = false;
    for side in ['+', '-'] {
        let mut triple: Option<char> = None;
        let mut inside = false;
        for line in patch.lines() {
            if line.starts_with("@@") {
                inside = true;
                continue;
            }
            if !inside || line.is_empty() {
                continue;
            }
            let marker = line.chars().next().unwrap_or_default();
            if marker != ' ' && marker != side {
                continue;
            }
            let source = &line[1..];
            if marker == side {
                saw_change = true;
                let trimmed = source.trim();
                if !trimmed.is_empty()
                    && (triple.is_some()
                        || !trimmed.starts_with('#')
                        || trimmed.starts_with("#!")
                        || executable_cookie(trimmed))
                {
                    return false;
                }
            }
            // Track Python string boundaries so a hash inside a multiline literal is data.
            let chars: Vec<_> = source.chars().collect();
            let mut index = 0;
            let mut single = None;
            while index < chars.len() {
                let c = chars[index];
                if c == '\\' {
                    index += 2;
                    continue;
                }
                if let Some(quote) = triple {
                    if chars
                        .get(index..index + 3)
                        .is_some_and(|span| span.iter().all(|c| *c == quote))
                    {
                        triple = None;
                        index += 3;
                    } else {
                        index += 1;
                    }
                } else if let Some(quote) = single {
                    if c == quote {
                        single = None;
                    }
                    index += 1;
                } else if c == '#' {
                    break;
                } else if c == '\'' || c == '"' {
                    if chars
                        .get(index..index + 3)
                        .is_some_and(|span| span.iter().all(|other| *other == c))
                    {
                        triple = Some(c);
                        index += 3;
                    } else {
                        single = Some(c);
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
        }
        if triple.is_some() {
            return false;
        }
    }
    saw_change
}
fn executable_cookie(line: &str) -> bool {
    line.find("coding")
        .is_some_and(|index| line[index + 6..].starts_with([':', '=']))
}
pub(crate) fn planning_blob(blob: &Value) -> bool {
    text(&blob["text"]).lines().take(10).any(|line| {
        line == "> **Non-authoritative planning scratchpad — do not review for consistency.**"
    })
}

pub fn evaluate(snapshot: &Snapshot, policy: &ConvergencePolicy) -> Result<Evaluation, Error> {
    policy.validate()?;
    let node = &snapshot.initial;
    let current = &snapshot.current;
    let previous = &snapshot.previous;
    let head = text(&node["headRefOid"]);
    let base = text(&node["baseRefOid"]);
    let rollup = &node["headRef"]["target"]["statusCheckRollup"];
    let mut threads = normalize_threads(node, policy)?;
    let raw_threads = serde_json::to_value(&threads)?;
    for thread in &mut threads {
        if let Some(id) = &thread.id {
            thread.resolution_observed_at = previous["resolved_thread_observed_at"][id]
                .as_str()
                .map(str::to_owned);
        }
        if thread.disposition_kind.as_deref() == Some("fixed") {
            let valid = thread.fixing_commit.as_ref().is_some_and(|fix| {
                let value = comparison(snapshot, fix, head);
                let sha = text(&value["base_commit"]["sha"]);
                !sha.is_empty()
                    && sha.starts_with(&fix.to_lowercase())
                    && value["merge_base_commit"]["sha"] == sha
                    && matches!(text(&value["status"]), "ahead" | "identical")
            });
            if !valid {
                thread.is_dispositioned = false;
                thread.disposition_kind = None;
                thread.fixing_commit = None;
            }
        }
    }
    let check_inventory = inventory(checks(node));
    let stable = !check_inventory.is_empty()
        && previous["head_oid"] == head
        && previous["check_inventory"] == json!(check_inventory);
    let mut facts = Facts {
        head_oid: head.into(),
        checked_head_oid: node["headRef"]["target"]["oid"].as_str().map(str::to_owned),
        is_draft: yes(&node["isDraft"]),
        review_decision: node["reviewDecision"].as_str().map(str::to_owned),
        check_inventory_stable: Some(stable),
        review_threads: threads,
        quiet_review_head_oids: Vec::new(),
        planning_only: false,
        review_exempt_since_quiet_review: false,
        body: Some(text(&node["body"]).into()),
        check_rollup_state: rollup["state"].as_str().map(str::to_owned),
        checks: checks(node).to_vec(),
        mergeable: text(&node["mergeable"]).into(),
        base_commits_not_in_head: comparison(snapshot, base, head)["behind_by"].as_u64(),
    };
    let comments = array(&node["comments"]["nodes"]);
    let reviews = array(&node["reviews"]["nodes"]);
    let mut quiet_oids = Vec::new();
    let mut live_quiet = BTreeMap::<String, String>::new();
    let mut current_ids = Vec::new();
    let mut authenticated_ids = BTreeMap::new();
    let mut authenticated_requests = BTreeMap::new();
    let persisted_head = text(&previous["authenticated_review_head"]);
    for reviewer in &policy.reviewers {
        for review in reviews {
            let oid = text(&review["commit"]["oid"]);
            let at = text(&review["submittedAt"]);
            let id = text(&review["id"]);
            if oid.is_empty()
                || at.is_empty()
                || review["state"] == "DISMISSED"
                || text(&node["lastEditedAt"]) > at
            {
                continue;
            }
            let request = qualifying_request(comments, oid, at, reviewer, &facts, policy, false)?;
            let live = reviewer_matches(review, reviewer) && !id.is_empty();
            if live && request.is_some() && !current_ids.contains(&id.to_owned()) {
                current_ids.push(id.to_owned());
            }
            let associated: Vec<_> = facts
                .review_threads
                .iter()
                .filter(|t| t.review_ids.iter().any(|review| review == id))
                .collect();
            let declined = !associated.is_empty()
                && associated
                    .iter()
                    .all(|t| t.is_resolved && t.disposition_kind.as_deref() == Some("declined"));
            let informational = !associated.is_empty()
                && associated
                    .iter()
                    .all(|t| t.is_resolved && t.is_dispositioned && t.is_informational);
            let quiet = review["state"] != "CHANGES_REQUESTED"
                && text(&review["body"]).trim().is_empty()
                && (review["comments"]["totalCount"] == 0 || declined || informational);
            if live && quiet {
                live_quiet.insert(id.into(), oid.into());
                if let Some(request) = request {
                    quiet_oids.push(oid.to_owned());
                    authenticated_ids.insert(oid.to_owned(), id.to_owned());
                    authenticated_requests.insert(oid.to_owned(), request);
                }
            }
        }
        let grammar = Regex::new(&reviewer.verdict_pattern)?;
        for comment in comments {
            if !reviewer_matches(comment, reviewer)
                || !text(&comment["body"]).contains(&reviewer.verdict_marker)
            {
                continue;
            }
            let Some(captures) = grammar.captures(text(&comment["body"])) else {
                continue;
            };
            let (Some(completed), Some(revision)) = (captures.get(1), captures.get(2)) else {
                continue;
            };
            let at = completed.as_str();
            if !timestamp_not_after(effective_at(comment), at) {
                continue;
            }
            for oid in [head, persisted_head] {
                if oid.is_empty() || !oid.starts_with(revision.as_str()) {
                    continue;
                }
                let reaction = array(&node["reactions"]["nodes"]).iter().any(|r| {
                    r["content"] == reviewer.completion_reaction
                        && reviewer_matches(r, reviewer)
                        && timestamp_not_after(at, text(&r["createdAt"]))
                });
                let id = text(&comment["id"]);
                if id.is_empty()
                    || !reaction
                    || (!text(&node["lastEditedAt"]).is_empty()
                        && !timestamp_not_after(text(&node["lastEditedAt"]), at))
                {
                    continue;
                }
                live_quiet.insert(id.into(), oid.into());
                if let Some(request) =
                    qualifying_request(comments, oid, at, reviewer, &facts, policy, true)?
                {
                    quiet_oids.push(oid.to_owned());
                    authenticated_ids.insert(oid.to_owned(), id.to_owned());
                    authenticated_requests.insert(oid.to_owned(), request);
                }
            }
        }
    }
    let persisted_id = text(&previous["authenticated_review_id"]);
    let persisted_request = &previous["authenticated_review_request"];
    let live_request = comments
        .iter()
        .find(|comment| comment["id"] == persisted_request["id"] && comment["id"].is_string());
    let mut request_valid = false;
    if let Some(comment) = live_request
        && request_signature(comment).as_ref() == Some(persisted_request)
        && !persisted_head.is_empty()
    {
        for reviewer in &policy.reviewers {
            request_valid |= requests(comment, persisted_head, reviewer)?;
        }
    }
    let currently_green = facts.check_rollup_state.is_some()
        && facts
            .checks
            .iter()
            .filter(|c| !policy.is_non_gating(check_name(c)))
            .all(check_green);
    let restored = !persisted_id.is_empty()
        && previous["authenticated_review_body"] == node["body"]
        && live_quiet
            .get(persisted_id)
            .is_some_and(|oid| oid == persisted_head)
        && currently_green
        && request_valid
        && !check_inventory.is_empty()
        && previous["authenticated_review_check_inventory"] == json!(check_inventory)
        && previous["check_inventory"] == json!(check_inventory);
    if restored {
        quiet_oids.push(persisted_head.into());
        authenticated_ids.insert(persisted_head.into(), persisted_id.into());
        if reviews
            .iter()
            .any(|r| r["id"] == persisted_id && r["state"] != "DISMISSED")
            && !current_ids.contains(&persisted_id.to_owned())
        {
            current_ids.push(persisted_id.into());
        }
    }
    facts.quiet_review_head_oids = quiet_oids
        .iter()
        .filter(|oid| *oid == head)
        .cloned()
        .collect();
    if facts.quiet_review_head_oids.is_empty() {
        facts.review_exempt_since_quiet_review = quiet_oids
            .iter()
            .rev()
            .any(|oid| exempt_change(snapshot, oid, head, base));
    }
    let mut known_ids: Vec<String> = array(&previous["known_codex_review_ids"])
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let mut wave_ids: Vec<String> = array(&previous["review_wave_ids"])
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if known_ids.is_empty() && wave_ids.is_empty() {
        wave_ids = current_ids.clone();
    }
    let new_ids: Vec<_> = current_ids
        .iter()
        .filter(|id| !known_ids.contains(id))
        .cloned()
        .collect();
    let prior_base = text(&previous["review_wave_base_oid"]);
    let prior_head = text(&previous["head_oid"]);
    let wave_base = if !prior_base.is_empty() && prior_base != base && prior_head == head {
        prior_base
    } else {
        base
    };
    if !prior_base.is_empty()
        && prior_base != base
        && !prior_head.is_empty()
        && prior_head != head
        && !exempt_change(snapshot, prior_head, head, base)
    {
        wave_ids = new_ids.clone();
    } else {
        for id in &new_ids {
            if !wave_ids.contains(id) {
                wave_ids.push(id.clone());
            }
        }
    }
    known_ids.extend(new_ids);
    for thread in &mut facts.review_threads {
        if thread.disposition_kind.as_deref() != Some("escalated") {
            continue;
        }
        let wave = thread
            .review_ids
            .iter()
            .filter_map(|id| {
                wave_ids
                    .iter()
                    .position(|wave| wave == id)
                    .map(|index| index + 1)
            })
            .max()
            .unwrap_or_default();
        if !(wave >= policy.extended_wave_cap
            || (wave == policy.wave_cap && wave_ids.len() == policy.wave_cap))
        {
            thread.is_dispositioned = false;
            thread.is_escalated = false;
            thread.disposition_kind = None;
        }
    }
    let files = array(&node["files"]["nodes"]);
    facts.planning_only = !files.is_empty()
        && files.iter().all(|file| {
            let path = text(&file["path"]);
            let data = snapshot
                .blobs
                .get(path)
                .and_then(|v| array(v).first())
                .map(|r| &r["response"]["data"]["repository"])
                .unwrap_or(&Value::Null);
            planning_blob(&data["head"])
                && (file["changeType"] == "ADDED" || planning_blob(&data["base"]))
        });
    let identity_changed = current["state"] != "OPEN"
        || [
            "baseRefName",
            "baseRefOid",
            "headRefName",
            "headRefOid",
            "isDraft",
            "body",
            "lastEditedAt",
            "mergeable",
            "reviewDecision",
        ]
        .iter()
        .any(|key| node[*key] != current[*key]);
    let threads_changed = raw_threads != serde_json::to_value(normalize_threads(current, policy)?)?;
    let reviews_changed = ["comments", "reviews", "reactions"]
        .iter()
        .any(|key| node[*key]["nodes"] != current[*key]["nodes"]);
    if identity_changed {
        return Err(Error::Evidence(
            "pull request changed after its convergence snapshot".into(),
        ));
    }
    let inventory_changed = inventory(checks(node)) != inventory(checks(current));
    facts.checks = checks(current).to_vec();
    facts.check_rollup_state = current["headRef"]["target"]["statusCheckRollup"]["state"]
        .as_str()
        .map(str::to_owned);
    facts.checked_head_oid = current["headRef"]["target"]["oid"]
        .as_str()
        .map(str::to_owned);
    if threads_changed || reviews_changed || inventory_changed {
        facts.checked_head_oid = None;
        facts.base_commits_not_in_head = None;
    }
    let verdict = evaluate_facts(&facts, policy);
    let mut state = previous.clone();
    if !state.is_object() {
        state = json!({});
    }
    state["head_oid"] = json!(head);
    state["check_inventory"] = json!(check_inventory);
    state["known_codex_review_ids"] = json!(known_ids);
    state["review_wave_ids"] = json!(wave_ids);
    state["review_wave_base_oid"] = json!(wave_base);
    if stable && facts.quiet_review_head_oids.iter().any(|oid| oid == head) {
        state["authenticated_review_head"] = json!(head);
        state["authenticated_review_body"] = node["body"].clone();
        state["authenticated_review_check_inventory"] = json!(check_inventory);
        if let Some(id) = authenticated_ids.get(head) {
            state["authenticated_review_id"] = json!(id);
        }
        if let Some(request) = authenticated_requests.get(head) {
            state["authenticated_review_request"] = request.clone();
        }
    }
    let mut gating_checks = Vec::new();
    let mut non_gating_checks = Vec::new();
    for check in &facts.checks {
        let projected = json!({"name":check_name(check),"state":check_state(check)});
        if policy.is_non_gating(check_name(check)) {
            non_gating_checks.push(projected);
        } else {
            gating_checks.push(projected);
        }
    }
    Ok(Evaluation {
        converged: verdict.is_converged(),
        reasons: verdict
            .reasons()
            .iter()
            .map(|r| r.reference_reason())
            .collect(),
        verdict,
        unresolved_review_threads: facts
            .review_threads
            .iter()
            .filter(|t| !t.is_resolved && !t.is_escalated)
            .count(),
        undispositioned_review_threads: facts
            .review_threads
            .iter()
            .filter(|t| !t.is_dispositioned)
            .count(),
        escalated_review_threads: facts
            .review_threads
            .iter()
            .filter(|t| t.is_escalated)
            .count(),
        checks_green: currently_green,
        gating_checks,
        non_gating_checks,
        facts,
        state,
    })
}

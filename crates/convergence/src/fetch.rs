//! GitHub recording and complete-connection replay. The predicate does no I/O.
use crate::{ConvergencePolicy, Error, Recording, Response, Snapshot, array, text};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    process::{Command, Stdio},
};

const PAGE_SIZE: usize = 100;
const COMMENT_FIELDS: &str =
    "id author { login } authorAssociation body createdAt lastEditedAt pullRequestReview { id }";
const ISSUE_FIELDS: &str = "id author { login } authorAssociation body createdAt lastEditedAt";
const REVIEW_FIELDS: &str = "id author { login } state body submittedAt lastEditedAt commit { oid } comments(first:1) { totalCount }";
const CHECK_FIELDS: &str = "__typename ... on CheckRun { name status conclusion completedAt } ... on StatusContext { context state createdAt }";
const PAGE_INFO: &str = "totalCount pageInfo { hasNextPage endCursor }";

fn selection(kind: &str, after: bool) -> Result<String, Error> {
    let fields = match kind {
        "reviewThreads" => format!(
            "id isResolved comments(first:{PAGE_SIZE}) {{ {PAGE_INFO} nodes {{ {COMMENT_FIELDS} }} }}"
        ),
        "comments" => ISSUE_FIELDS.into(),
        "threadComments" => COMMENT_FIELDS.into(),
        "reviews" => REVIEW_FIELDS.into(),
        "reactions" => "content createdAt user { login }".into(),
        "files" => "path changeType additions deletions".into(),
        "contexts" => CHECK_FIELDS.into(),
        _ => return Err(Error::Evidence(format!("unknown connection {kind}"))),
    };
    let name = if kind == "threadComments" {
        "comments"
    } else {
        kind
    };
    let cursor = if after { ",after:$after" } else { "" };
    Ok(format!(
        "{name}(first:{PAGE_SIZE}{cursor}) {{ {PAGE_INFO} nodes {{ {fields} }} }}"
    ))
}

fn gh(arguments: &[&str], input: Option<&Value>) -> Result<Value, Error> {
    let mut child = Command::new("gh")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Evidence("gh stdin unavailable".into()))?;
        serde_json::to_writer(&mut stdin, input)?;
        stdin.flush()?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        if String::from_utf8_lossy(&output.stderr).contains("HTTP 404") {
            return Ok(Value::Null);
        }
        return Err(Error::Evidence(format!(
            "gh exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let response: Value = serde_json::from_slice(&output.stdout)?;
    if response.get("errors").is_some() {
        return Err(Error::Evidence(format!(
            "GitHub GraphQL errors: {}",
            response["errors"]
        )));
    }
    Ok(response)
}

fn query(responses: &mut Vec<Response>, query: String, variables: Value) -> Result<Value, Error> {
    let response = gh(
        &["api", "graphql", "--input", "-"],
        Some(&json!({"query":query,"variables":variables})),
    )?;
    let data = response["data"].clone();
    responses.push(Response {
        query,
        variables,
        response,
    });
    Ok(data)
}

/// Requires a finite connection to agree with its provider totalCount.
pub fn complete_connection(connection: &Value) -> Result<(), Error> {
    let count = connection["totalCount"]
        .as_u64()
        .ok_or_else(|| Error::Evidence("connection totalCount missing".into()))?;
    let nodes = connection["nodes"]
        .as_array()
        .ok_or_else(|| Error::Evidence("connection nodes missing".into()))?;
    if connection["pageInfo"]["hasNextPage"] != false || count != nodes.len() as u64 {
        return Err(Error::Evidence("incomplete connection census".into()));
    }
    Ok(())
}

fn append_page(connection: &mut Value, page: &Value) -> Result<(), Error> {
    if connection["totalCount"] != page["totalCount"] {
        return Err(Error::Evidence(
            "connection totalCount changed while paging".into(),
        ));
    }
    let extra = page["nodes"]
        .as_array()
        .ok_or_else(|| Error::Evidence("page nodes missing".into()))?;
    connection["nodes"]
        .as_array_mut()
        .ok_or_else(|| Error::Evidence("connection nodes missing".into()))?
        .extend(extra.iter().cloned());
    connection["pageInfo"] = page["pageInfo"].clone();
    Ok(())
}

fn pages(
    responses: &mut Vec<Response>,
    id: &str,
    kind: &str,
    connection: &mut Value,
    policy: &ConvergencePolicy,
) -> Result<(), Error> {
    let typename = match kind {
        "threadComments" => "PullRequestReviewThread",
        "contexts" => "StatusCheckRollup",
        _ => "PullRequest",
    };
    let name = if kind == "threadComments" {
        "comments"
    } else {
        kind
    };
    let mut count = 1;
    while connection["pageInfo"]["hasNextPage"] == true {
        if count >= policy.page_limit {
            return Err(Error::Evidence("configured page limit exceeded".into()));
        }
        let after = connection["pageInfo"]["endCursor"]
            .as_str()
            .ok_or_else(|| Error::Evidence("page cursor missing".into()))?
            .to_owned();
        let data = query(
            responses,
            format!(
                "query($id:ID!,$after:String!) {{ node(id:$id) {{ ... on {typename} {{ id {} }} }} }}",
                selection(kind, true)?
            ),
            json!({"id":id,"after":after}),
        )?;
        let page = &data["node"][name];
        if page["pageInfo"]["hasNextPage"] == true && page["pageInfo"]["endCursor"] == after {
            return Err(Error::Evidence("page cursor repeated".into()));
        }
        append_page(connection, page)?;
        count += 1;
    }
    complete_connection(connection)
}

fn observation(
    repository: &str,
    number: u64,
    policy: &ConvergencePolicy,
) -> Result<Vec<Response>, Error> {
    let (owner, name) = repository
        .split_once('/')
        .ok_or_else(|| Error::Evidence("repository must be owner/name".into()))?;
    let mut responses = Vec::new();
    let kinds = ["reviewThreads", "comments", "reviews", "reactions", "files"];
    let selections = kinds
        .iter()
        .map(|kind| selection(kind, false))
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    let data = query(
        &mut responses,
        format!(
            "query($owner:String!,$name:String!,$number:Int!) {{ repository(owner:$owner,name:$name) {{ pullRequest(number:$number) {{ id number state title body lastEditedAt url isDraft reviewDecision author {{ login }} baseRefName baseRefOid headRefName headRefOid headRepository {{ nameWithOwner }} mergeable {selections} }} }} }}"
        ),
        json!({"owner":owner,"name":name,"number":number}),
    )?;
    let mut node = data["repository"]["pullRequest"].clone();
    let id = text(&node["id"]).to_owned();
    if id.is_empty() {
        return Err(Error::Evidence("pull request unavailable".into()));
    }
    if node["reviewThreads"]["totalCount"]
        .as_u64()
        .is_none_or(|count| count > policy.thread_limit as u64)
    {
        return Err(Error::Evidence("configured thread limit exceeded".into()));
    }
    for kind in kinds {
        pages(&mut responses, &id, kind, &mut node[kind], policy)?;
    }
    for thread in node["reviewThreads"]["nodes"]
        .as_array_mut()
        .ok_or_else(|| Error::Evidence("thread nodes missing".into()))?
    {
        let id = text(&thread["id"]).to_owned();
        pages(
            &mut responses,
            &id,
            "threadComments",
            &mut thread["comments"],
            policy,
        )?;
    }
    let data = query(
        &mut responses,
        format!(
            "query($owner:String!,$name:String!,$head:GitObjectID!) {{ repository(owner:$owner,name:$name) {{ object(oid:$head) {{ ... on Commit {{ oid statusCheckRollup {{ id state {} }} }} }} }} }}",
            selection("contexts", false)?
        ),
        json!({"owner":owner,"name":name,"head":node["headRefOid"]}),
    )?;
    let mut rollup = data["repository"]["object"]["statusCheckRollup"].clone();
    if !rollup.is_null() {
        let id = text(&rollup["id"]).to_owned();
        pages(
            &mut responses,
            &id,
            "contexts",
            &mut rollup["contexts"],
            policy,
        )?;
    }
    Ok(responses)
}

fn assemble(responses: &[Response], policy: &ConvergencePolicy) -> Result<Value, Error> {
    let mut node = Value::Null;
    for item in responses {
        if item.response.get("errors").is_some() {
            return Err(Error::Evidence("recorded GraphQL error".into()));
        }
        let data = &item.response["data"];
        if let Some(pr) = data["repository"].get("pullRequest") {
            node = pr.clone();
        }
        if let Some(commit) = data["repository"].get("object") {
            node["headRef"] = json!({"target":commit});
        }
        if let Some(page_node) = data.get("node") {
            let id = text(&page_node["id"]);
            if id == text(&node["id"]) {
                for kind in ["reviewThreads", "comments", "reviews", "reactions", "files"] {
                    if let Some(page) = page_node.get(kind) {
                        append_page(&mut node[kind], page)?;
                    }
                }
            } else if id == text(&node["headRef"]["target"]["statusCheckRollup"]["id"]) {
                append_page(
                    &mut node["headRef"]["target"]["statusCheckRollup"]["contexts"],
                    &page_node["contexts"],
                )?;
            } else {
                let threads = node["reviewThreads"]["nodes"]
                    .as_array_mut()
                    .ok_or_else(|| Error::Evidence("thread nodes missing".into()))?;
                let thread = threads
                    .iter_mut()
                    .find(|thread| text(&thread["id"]) == id)
                    .ok_or_else(|| Error::Evidence("page for unknown thread".into()))?;
                append_page(&mut thread["comments"], &page_node["comments"])?;
            }
        }
    }
    for kind in ["reviewThreads", "comments", "reviews", "reactions", "files"] {
        complete_connection(&node[kind])?;
    }
    if array(&node["reviewThreads"]["nodes"]).len() > policy.thread_limit {
        return Err(Error::Evidence("configured thread limit exceeded".into()));
    }
    for thread in array(&node["reviewThreads"]["nodes"]) {
        complete_connection(&thread["comments"])?;
    }
    if !node["headRef"]["target"]["statusCheckRollup"].is_null() {
        complete_connection(&node["headRef"]["target"]["statusCheckRollup"]["contexts"])?;
    }
    Ok(node)
}

impl Recording {
    pub fn snapshot(&self, policy: &ConvergencePolicy) -> Result<Snapshot, Error> {
        let first = self
            .observations
            .first()
            .ok_or_else(|| Error::Evidence("recording has no observations".into()))?;
        let last = self
            .observations
            .last()
            .ok_or_else(|| Error::Evidence("recording has no revalidation".into()))?;
        if self.observations.len() < 2 {
            return Err(Error::Evidence("decision revalidation missing".into()));
        }
        Ok(Snapshot {
            initial: assemble(first, policy)?,
            current: assemble(last, policy)?,
            comparisons: self.comparisons.clone(),
            blobs: self.blobs.clone(),
            previous: self.previous.clone(),
        })
    }
}

/// Records raw responses and the ancestry comparisons used by the predicate.
pub fn record(
    repository: &str,
    number: u64,
    policy: &ConvergencePolicy,
) -> Result<Recording, Error> {
    let first = observation(repository, number, policy)?;
    let node = assemble(&first, policy)?;
    let head = text(&node["headRefOid"]);
    let base = text(&node["baseRefOid"]);
    let mut comparisons = BTreeMap::new();
    let mut candidates = vec![base.to_owned()];
    for review in array(&node["reviews"]["nodes"]) {
        let Some(oid) = review["commit"]["oid"].as_str() else {
            continue;
        };
        let submitted = text(&review["submittedAt"]);
        let post_green = crate::predicate::checks(&node)
            .iter()
            .filter(|check| !policy.is_non_gating(crate::predicate::check_name(check)))
            .all(|check| {
                crate::predicate::check_green(check)
                    && std::cmp::max(text(&check["completedAt"]), text(&check["createdAt"]))
                        <= submitted
            });
        if post_green
            && text(&review["body"]).trim().is_empty()
            && !matches!(text(&review["state"]), "DISMISSED" | "CHANGES_REQUESTED")
        {
            candidates.push(oid.to_owned());
        }
    }
    for thread in array(&node["reviewThreads"]["nodes"]) {
        for comment in array(&thread["comments"]["nodes"]) {
            if let Some(oid) = crate::evidence::fixing_commit(text(&comment["body"]))? {
                candidates.push(oid);
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    for candidate in candidates {
        let key = format!("{candidate}...{head}");
        let response = gh(&["api", &format!("repos/{repository}/compare/{key}")], None)?;
        comparisons.insert(key, response);
    }
    let merged_heads: Vec<_> = comparisons
        .values()
        .filter(|value| {
            let commits = array(&value["commits"]);
            commits.len() == 1
                && commits[0]["sha"] == head
                && array(&commits[0]["parents"]).len() == 2
                && commits[0]["parents"][1]["sha"] == base
        })
        .filter_map(|value| {
            value["commits"][0]["parents"][0]["sha"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    for reviewed in merged_heads {
        let key = format!("{reviewed}...{base}");
        let value = gh(&["api", &format!("repos/{repository}/compare/{key}")], None)?;
        let merge_base = value["merge_base_commit"]["sha"]
            .as_str()
            .map(str::to_owned);
        comparisons.insert(key, value);
        if let Some(merge_base) = merge_base {
            let key = format!("{merge_base}...{base}");
            if !comparisons.contains_key(&key) {
                comparisons.insert(
                    key.clone(),
                    gh(&["api", &format!("repos/{repository}/compare/{key}")], None)?,
                );
            }
        }
    }
    let mut blobs = BTreeMap::new();
    let (owner, name) = repository
        .split_once('/')
        .ok_or_else(|| Error::Evidence("repository must be owner/name".into()))?;
    let comparison = comparisons.get(&format!("{base}...{head}"));
    for file in array(&node["files"]["nodes"]) {
        let path = text(&file["path"]);
        let previous = comparison
            .and_then(|c| {
                array(&c["files"])
                    .iter()
                    .find(|f| text(&f["filename"]) == path)
            })
            .and_then(|f| f["previous_filename"].as_str())
            .unwrap_or(path);
        let mut responses = Vec::new();
        let data = query(&mut responses,"query($owner:String!,$name:String!,$head:String!,$base:String!) { repository(owner:$owner,name:$name) { head:object(expression:$head) { ... on Blob { text } } base:object(expression:$base) { ... on Blob { text } } } }".into(),json!({"owner":owner,"name":name,"head":format!("{head}:{path}"),"base":format!("{base}:{previous}")}))?;
        blobs.insert(path.into(), serde_json::to_value(responses)?);
        if !crate::evidence::planning_blob(&data["repository"]["head"])
            || (file["changeType"] != "ADDED"
                && !crate::evidence::planning_blob(&data["repository"]["base"]))
        {
            break;
        }
    }
    let last = observation(repository, number, policy)?;
    Ok(Recording {
        repository: repository.into(),
        number,
        observations: vec![first, last],
        comparisons,
        blobs,
        previous: json!({}),
    })
}

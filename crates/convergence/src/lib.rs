//! Pull-request convergence from complete GitHub evidence and explicit policy.

mod evidence;
pub mod fetch;
mod predicate;
pub use evidence::{Evaluation, evaluate};
pub use predicate::{Facts, Reason, Verdict, evaluate_facts};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

/// Failures fetching, decoding, or evaluating convergence evidence.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Evidence(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Grammar(#[from] regex::Error),
}

/// Repository and reviewer configuration shared by every consumer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConvergencePolicy {
    pub repository: String,
    pub reviewers: Vec<ReviewerPolicy>,
    pub non_gating_check_patterns: Vec<String>,
    pub exempt_smoke_workflow_names: Vec<String>,
    pub thread_limit: usize,
    pub page_limit: usize,
    pub wave_cap: usize,
    pub extended_wave_cap: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReviewerPolicy {
    pub login: String,
    pub bot: bool,
    pub request_pattern: String,
    pub verdict_marker: String,
    /// Capture groups are completion timestamp and reviewed revision.
    pub verdict_pattern: String,
    pub escalation_marker: String,
    pub trusted_requests: bool,
    pub post_green_requests: bool,
    pub completion_reaction: String,
}

impl ConvergencePolicy {
    pub fn read(path: &std::path::Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        let policy: Self = if path.extension().is_some_and(|ext| ext == "json") {
            serde_json::from_str(&text)?
        } else {
            toml::from_str(&text)?
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.thread_limit == 0
            || self.page_limit == 0
            || self.wave_cap == 0
            || self.extended_wave_cap < self.wave_cap
            || self.reviewers.is_empty()
        {
            return Err(Error::Evidence(
                "invalid convergence limits or reviewer list".into(),
            ));
        }
        for reviewer in &self.reviewers {
            regex::Regex::new(&reviewer.request_pattern)?;
            regex::Regex::new(&reviewer.verdict_pattern)?;
        }
        Ok(())
    }

    pub fn is_non_gating(&self, name: &str) -> bool {
        self.exempt_smoke_workflow_names
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(name))
            || self
                .non_gating_check_patterns
                .iter()
                .any(|pattern| glob_matches(pattern, name))
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let mut expression = String::from("(?i)^");
    for character in pattern.chars() {
        match character {
            '*' => expression.push_str(".*"),
            '?' => expression.push('.'),
            other => expression.push_str(&regex::escape(&other.to_string())),
        }
    }
    expression.push('$');
    regex::Regex::new(&expression).is_ok_and(|pattern| pattern.is_match(value))
}

/// One unmodified provider response, with its request for deterministic replay.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Response {
    pub query: String,
    pub variables: Value,
    pub response: Value,
}

/// Recording of one census and its immediately preceding decision revalidation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Recording {
    pub repository: String,
    pub number: u64,
    pub observations: Vec<Vec<Response>>,
    pub comparisons: BTreeMap<String, Value>,
    pub blobs: BTreeMap<String, Value>,
    #[serde(default)]
    pub previous: Value,
}

impl Recording {
    pub fn read(path: &std::path::Path) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        if path.extension().is_some_and(|ext| ext == "gz") {
            flate2::read::GzDecoder::new(std::fs::File::open(path)?).read_to_end(&mut bytes)?;
        } else {
            bytes = std::fs::read(path)?;
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        if let Some(source) = value["source"].as_str() {
            let parent = path
                .parent()
                .ok_or_else(|| Error::Evidence("fixture parent missing".into()))?;
            let mut recording = serde_json::to_value(Self::read(&parent.join(source))?)?;
            for mutation in array(&value["mutations"]) {
                let pointer = mutation["path"]
                    .as_str()
                    .ok_or_else(|| Error::Evidence("mutation path missing".into()))?;
                let target = recording
                    .pointer_mut(pointer)
                    .ok_or_else(|| Error::Evidence(format!("unknown mutation path {pointer}")))?;
                *target = mutation["value"].clone();
            }
            Ok(serde_json::from_value(recording)?)
        } else {
            Ok(serde_json::from_value(value)?)
        }
    }

    pub fn write(&self, path: &std::path::Path) -> Result<(), Error> {
        let file = std::fs::File::create(path)?;
        if path.extension().is_some_and(|ext| ext == "gz") {
            let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            serde_json::to_writer(&mut encoder, self)?;
            encoder.finish()?;
        } else {
            serde_json::to_writer_pretty(file, self)?;
        }
        Ok(())
    }
}

/// In-memory evidence. No operation on this value accesses GitHub.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub initial: Value,
    pub current: Value,
    pub comparisons: BTreeMap<String, Value>,
    pub blobs: BTreeMap<String, Value>,
    pub previous: Value,
}

pub(crate) fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}
pub(crate) fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}
pub(crate) fn yes(value: &Value) -> bool {
    value.as_bool().unwrap_or_default()
}
pub(crate) fn login(value: &Value) -> &str {
    value
        .get("author")
        .or_else(|| value.get("user"))
        .map(|author| text(&author["login"]))
        .unwrap_or_default()
}
pub(crate) fn effective_at(value: &Value) -> &str {
    std::cmp::max(text(&value["createdAt"]), text(&value["lastEditedAt"]))
}
pub(crate) fn trusted(value: &Value) -> bool {
    matches!(
        text(&value["authorAssociation"]),
        "OWNER" | "MEMBER" | "COLLABORATOR"
    )
}

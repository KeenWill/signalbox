//! Full review text selected from an attached, immutable context blob.

use super::*;

#[derive(Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct FindingTextArguments {
    #[tool_schema(
        description = "Exact qualified finding ID from the supplied synopsis: sha256:<digest>#<finding_id>."
    )]
    finding_id: String,
}

#[derive(Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ThreadTextArguments {
    #[tool_schema(
        description = "Exact qualified thread ID from the supplied synopsis: sha256:<digest>#<thread_id>."
    )]
    thread_id: String,
}

#[derive(Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ThreadListArguments {
    #[tool_schema(
        description = "Exact sha256 digest of the attached review context for this PR and head."
    )]
    context_digest: String,
}

pub(super) struct ThreadListContract;
impl ToolContract for ThreadListContract {
    type Arguments = ThreadListArguments;
    const NAME: &'static str = REVIEW_THREAD_LIST_NAME;
    const DESCRIPTION: &'static str = "Lists the retained review threads for the attached PR/head snapshot, with qualified IDs and locations. Resolution state is a current observation, not a historical ruling.";
}

pub(super) struct FindingTextContract;
impl ToolContract for FindingTextContract {
    type Arguments = FindingTextArguments;
    const NAME: &'static str = FINDING_TEXT_NAME;
    const DESCRIPTION: &'static str = "Fetches one finding's full text from the daemon's retained context blob. No code-host request is made.";
}

pub(super) struct ThreadTextContract;
impl ToolContract for ThreadTextContract {
    type Arguments = ThreadTextArguments;
    const NAME: &'static str = REVIEW_THREAD_TEXT_NAME;
    const DESCRIPTION: &'static str = "Fetches one review thread's full text from the daemon's retained context blob. No code-host request is made.";
}

pub(super) struct ReviewTextReference {
    pub digest: BlobDigest,
    entry_id: String,
    mode: BlobToolMode,
}

pub(super) fn decode(
    arguments: &NormalizedToolArguments,
    mode: BlobToolMode,
) -> Result<ReviewTextReference, BlobToolExecutorError> {
    if mode == BlobToolMode::ThreadList {
        let arguments = serde_json::from_str::<ThreadListArguments>(arguments.as_str())
            .map_err(|_| BlobToolExecutorError::Infrastructure)?;
        return Ok(ReviewTextReference {
            digest: arguments
                .context_digest
                .parse()
                .map_err(|_| BlobToolExecutorError::Infrastructure)?,
            entry_id: String::new(),
            mode,
        });
    }
    let qualified = match mode {
        BlobToolMode::FindingText => {
            serde_json::from_str::<FindingTextArguments>(arguments.as_str())
                .map_err(|_| BlobToolExecutorError::Infrastructure)?
                .finding_id
        }
        BlobToolMode::ThreadText => {
            serde_json::from_str::<ThreadTextArguments>(arguments.as_str())
                .map_err(|_| BlobToolExecutorError::Infrastructure)?
                .thread_id
        }
        _ => return Err(BlobToolExecutorError::Infrastructure),
    };
    let (digest, entry_id) = qualified
        .split_once('#')
        .filter(|(_, entry)| !entry.is_empty())
        .ok_or(BlobToolExecutorError::Infrastructure)?;
    Ok(ReviewTextReference {
        digest: digest
            .parse()
            .map_err(|_| BlobToolExecutorError::Infrastructure)?,
        entry_id: entry_id.to_owned(),
        mode,
    })
}

impl BlobToolExecutor {
    pub(super) async fn review_text(
        &self,
        reference: ReviewTextReference,
    ) -> Result<ToolExecutorEvidence, BlobToolExecutorError> {
        let Ok(permit) = Arc::clone(&self.read_budget).try_acquire_owned() else {
            return failed(BlobReadError::Unavailable);
        };
        let result = signalbox_application::with_released_scheduler_admission(async {
            let result = tokio::time::timeout(BLOB_READ_TIMEOUT, async {
                let registry = self.registry.as_deref().ok_or(BlobReadError::Unavailable)?;
                let entry = read_blob_entry(&self.repository, reference.digest).await?;
                let length = entry.expected().byte_length();
                if length > MAX_BLOB_READ_TOOL_BYTES {
                    return Err(BlobReadError::RangeOutOfBounds);
                }
                let length = NonZeroU64::new(length).ok_or(BlobReadError::Corrupt)?;
                let bytes = read_blob_chunk(registry, &entry, 0, length).await?;
                select_text(&bytes, &reference)
            })
            .await;
            drop(permit);
            result
        })
        .await
        .unwrap_or(Err(BlobReadError::Unavailable));
        match result {
            Ok(text) => completed(&serde_json::json!({"id": reference.entry_id, "text": text})),
            Err(error) => failed(error),
        }
    }
}

fn select_text(bytes: &[u8], reference: &ReviewTextReference) -> Result<String, BlobReadError> {
    let context: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| BlobReadError::Corrupt)?;
    if reference.mode == BlobToolMode::ThreadList {
        let threads = context["threads"]
            .as_array()
            .ok_or(BlobReadError::Corrupt)?
            .iter()
            .map(|entry| {
                let id = entry["thread_id"].as_str().ok_or(BlobReadError::Corrupt)?;
                Ok(
                    serde_json::json!({"thread_id": format!("{}#{id}", reference.digest),
                    "author": entry["author"], "resolved": entry["resolved"],
                    "path": entry["path"], "line": entry["line"]}),
                )
            })
            .collect::<Result<Vec<_>, BlobReadError>>()?;
        return Ok(serde_json::json!({"pr": context["pr"], "head_sha": context["head_sha"], "threads": threads}).to_string());
    }
    let (collection, identity) = match reference.mode {
        BlobToolMode::FindingText => ("findings", "finding_id"),
        BlobToolMode::ThreadText => ("threads", "thread_id"),
        _ => return Err(BlobReadError::Corrupt),
    };
    let mut matched = context[collection]
        .as_array()
        .ok_or(BlobReadError::Corrupt)?
        .iter()
        .filter(|entry| entry[identity].as_str() == Some(&reference.entry_id));
    let entry = matched.next().ok_or(BlobReadError::NotFound)?;
    if matched.next().is_some() {
        return Err(BlobReadError::Corrupt);
    }
    entry["text"]
        .as_str()
        .map(str::to_owned)
        .ok_or(BlobReadError::Corrupt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_list_preserves_snapshot_identity_without_exporting_rulings_or_bodies() {
        let bytes = br#"{"pr":42,"head_sha":"reviewed","threads":[{"thread_id":"thread","author":"reviewer","resolved":true,"path":"a.rs","line":7,"text":"Later ruling","ruling":"ACCEPT"}]}"#;
        let reference = ReviewTextReference {
            digest: BlobDigest::digest(bytes),
            entry_id: String::new(),
            mode: BlobToolMode::ThreadList,
        };
        let result: serde_json::Value =
            serde_json::from_str(&select_text(bytes, &reference).unwrap()).unwrap();
        assert_eq!(result["pr"], 42);
        assert_eq!(result["head_sha"], "reviewed");
        assert_eq!(result["threads"][0]["path"], "a.rs");
        assert!(result["threads"][0].get("text").is_none());
        assert!(result["threads"][0].get("ruling").is_none());
    }

    #[test]
    fn selecting_a_finding_returns_its_full_text_without_label_fields() {
        let bytes = br#"{"findings":[{"finding_id":"subject","text":"Complete finding.","ruling":"ACCEPT"},{"finding_id":"other","text":"Other finding."}],"threads":[]}"#;
        let reference = ReviewTextReference {
            digest: BlobDigest::digest(bytes),
            entry_id: String::from("subject"),
            mode: BlobToolMode::FindingText,
        };
        assert_eq!(
            select_text(bytes, &reference).expect("the requested finding exists"),
            "Complete finding."
        );
    }

    #[test]
    fn a_thread_lookup_does_not_return_a_finding_with_the_same_identifier() {
        let bytes = br#"{"findings":[{"finding_id":"shared","text":"Finding."}],"threads":[{"thread_id":"shared","text":"Thread and its comments."}]}"#;
        let reference = ReviewTextReference {
            digest: BlobDigest::digest(bytes),
            entry_id: String::from("shared"),
            mode: BlobToolMode::ThreadText,
        };
        assert_eq!(
            select_text(bytes, &reference).expect("the requested thread exists"),
            "Thread and its comments."
        );
    }

    #[test]
    fn ambiguous_entry_identities_do_not_select_arbitrary_text() {
        let bytes = br#"{"findings":[{"finding_id":"same","text":"First."},{"finding_id":"same","text":"Second."}],"threads":[]}"#;
        let reference = ReviewTextReference {
            digest: BlobDigest::digest(bytes),
            entry_id: String::from("same"),
            mode: BlobToolMode::FindingText,
        };
        assert!(matches!(
            select_text(bytes, &reference),
            Err(BlobReadError::Corrupt)
        ));
    }
}

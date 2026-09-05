//! Typed, bounded results returned by the code-host transport.

use std::borrow::Cow;

use reqwest::Url;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde_json::{Value, json};

use super::{
    CodeHostNumericBounds,
    arguments::{CodeHostFilePath, valid_opaque_id, valid_revision},
};

// numeric-bound: guard - the tool contract advertises accepting result URLs only to this length
const MAX_RESULT_URL_BYTES: usize = 8 * 1024;
// numeric-bound: guard - one encoded tool result exhausting transport memory
pub(super) const MAX_ENCODED_RESULT_BYTES: usize = 512 * 1024;

/// Whether a bounded code-host result exhausted its source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeHostResultCompleteness {
    /// The complete source fit within the operation's configured policy and guards.
    Complete,
    /// More source content existed beyond the retained prefix or page.
    Truncated,
}

impl CodeHostResultCompleteness {
    pub(super) const fn is_truncated(self) -> bool {
        match self {
            Self::Complete => false,
            Self::Truncated => true,
        }
    }
}

/// Resolution posture acknowledged for one review thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewThreadResolution {
    /// The thread remains open.
    Open,
    /// The thread is resolved.
    Resolved,
}

impl ReviewThreadResolution {
    const fn is_resolved(self) -> bool {
        matches!(self, Self::Resolved)
    }
}

pub(super) fn valid_text(bounds: CodeHostNumericBounds, value: &str) -> bool {
    bounds.permits_result_text(value.len()) && !value.contains('\0')
}

pub(super) fn valid_required_text(bounds: CodeHostNumericBounds, value: &str) -> bool {
    !value.is_empty() && valid_text(bounds, value)
}

/// Whether a parsed location is one absolute credential-free HTTPS URL. The
/// job-log redirect location is admitted by this same predicate.
pub(super) fn absolute_https_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

/// One bounded absolute credential-free HTTPS result location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeHostUrl(String);

impl CodeHostUrl {
    fn try_new(value: String) -> Option<Self> {
        (value.len() <= MAX_RESULT_URL_BYTES
            && !value.chars().any(char::is_control)
            && Url::parse(&value).is_ok_and(|url| absolute_https_url(&url)))
        .then_some(Self(value))
    }

    /// Borrows the checked absolute HTTPS location.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn into_string(self) -> String {
        self.0
    }
}

impl JsonSchema for CodeHostUrl {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CodeHostUrl")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "format": "uri",
            "maxLength": MAX_RESULT_URL_BYTES,
            "pattern": r"^https://[^/@\u0000-\u0020\u007F-\u009F]+(?:[/?#]|$)",
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

pub(super) fn valid_path(value: &str) -> bool {
    value != "." && CodeHostFilePath::try_new(value.to_owned()).is_ok()
}

/// Typed result of `change_request_summary`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeRequestSummaryResult {
    number: u32,
    title: String,
    body: Option<String>,
    state: String,
    draft: bool,
    author: Option<String>,
    base_ref: String,
    head_ref: String,
    head_revision: String,
    url: CodeHostUrl,
}

/// Complete checked fields for one change-request summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeRequestSummaryFields {
    /// Change-request number.
    pub number: u32,
    /// Exact title.
    pub title: String,
    /// Optional exact body.
    pub body: Option<String>,
    /// Code-host state spelling.
    pub state: String,
    /// Whether the request is a draft.
    pub draft: bool,
    /// Optional author login.
    pub author: Option<String>,
    /// Base branch name.
    pub base_ref: String,
    /// Head branch name.
    pub head_ref: String,
    /// Exact head revision.
    pub head_revision: String,
    /// Browser URL.
    pub url: String,
}

impl ChangeRequestSummaryResult {
    /// Validates one complete summary result.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        fields: ChangeRequestSummaryFields,
    ) -> Option<Self> {
        let url = CodeHostUrl::try_new(fields.url)?;
        (fields.number > 0
            && valid_required_text(bounds, &fields.title)
            && fields
                .body
                .as_deref()
                .is_none_or(|value| valid_text(bounds, value))
            && valid_required_text(bounds, &fields.state)
            && fields
                .author
                .as_deref()
                .is_none_or(|value| valid_required_text(bounds, value))
            && valid_required_text(bounds, &fields.base_ref)
            && valid_required_text(bounds, &fields.head_ref)
            && valid_revision(&fields.head_revision))
        .then_some(Self {
            number: fields.number,
            title: fields.title,
            body: fields.body,
            state: fields.state,
            draft: fields.draft,
            author: fields.author,
            base_ref: fields.base_ref,
            head_ref: fields.head_ref,
            head_revision: fields.head_revision,
            url,
        })
    }

    fn into_value(self) -> Value {
        json!({
            "author": self.author,
            "base_ref": self.base_ref,
            "body": self.body,
            "draft": self.draft,
            "head_ref": self.head_ref,
            "head_revision": self.head_revision,
            "number": self.number,
            "state": self.state,
            "title": self.title,
            "url": self.url.into_string(),
        })
    }
}

/// One changed-file summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedFile {
    path: String,
    status: String,
    additions: u64,
    deletions: u64,
}

impl ChangedFile {
    /// Validates one changed-file summary.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        path: String,
        status: String,
        additions: u64,
        deletions: u64,
    ) -> Option<Self> {
        (valid_path(&path) && valid_required_text(bounds, &status)).then_some(Self {
            path,
            status,
            additions,
            deletions,
        })
    }

    /// Borrows the repository-relative path.
    pub fn path(&self) -> &str {
        &self.path
    }

    fn into_value(self) -> Value {
        json!({
            "additions": self.additions,
            "deletions": self.deletions,
            "path": self.path,
            "status": self.status,
        })
    }
}

/// Typed result of `change_request_changed_files`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedFilesResult {
    files: Vec<ChangedFile>,
    completeness: CodeHostResultCompleteness,
}

impl ChangedFilesResult {
    /// Validates one bounded changed-file page.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        files: Vec<ChangedFile>,
        completeness: CodeHostResultCompleteness,
    ) -> Option<Self> {
        bounds.permits_result_items(files.len()).then_some(Self {
            files,
            completeness,
        })
    }

    fn into_value(self) -> Value {
        json!({
            "files": self.files.into_iter().map(ChangedFile::into_value).collect::<Vec<_>>(),
            "truncated": self.completeness.is_truncated(),
        })
    }
}

/// Typed result of `change_request_file_patch`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePatchResult {
    file: ChangedFile,
    patch: Option<String>,
}

impl FilePatchResult {
    /// Validates one optional bounded patch.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        file: ChangedFile,
        patch: Option<String>,
    ) -> Option<Self> {
        patch
            .as_deref()
            .is_none_or(|value| valid_text(bounds, value))
            .then_some(Self { file, patch })
    }

    fn into_value(self) -> Value {
        json!({
            "file": self.file.into_value(),
            "patch": self.patch,
        })
    }
}

/// One check-run status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckStatus {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    url: CodeHostUrl,
}

impl CheckStatus {
    /// Validates one check-run status.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        id: u64,
        name: String,
        status: String,
        conclusion: Option<String>,
        url: String,
    ) -> Option<Self> {
        let url = CodeHostUrl::try_new(url)?;
        (id > 0
            && valid_required_text(bounds, &name)
            && valid_required_text(bounds, &status)
            && conclusion
                .as_deref()
                .is_none_or(|value| valid_required_text(bounds, value)))
        .then_some(Self {
            id,
            name,
            status,
            conclusion,
            url,
        })
    }

    fn into_value(self) -> Value {
        json!({
            "conclusion": self.conclusion,
            "id": self.id,
            "name": self.name,
            "status": self.status,
            "url": self.url.into_string(),
        })
    }
}

/// Typed result of `change_request_checks_status`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChecksStatusResult {
    revision: String,
    checks: Vec<CheckStatus>,
    completeness: CodeHostResultCompleteness,
}

impl ChecksStatusResult {
    /// Validates one bounded checks page for an exact revision.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        revision: String,
        checks: Vec<CheckStatus>,
        completeness: CodeHostResultCompleteness,
    ) -> Option<Self> {
        (valid_required_text(bounds, &revision) && bounds.permits_result_items(checks.len()))
            .then_some(Self {
                revision,
                checks,
                completeness,
            })
    }

    fn into_value(self) -> Value {
        json!({
            "checks": self.checks.into_iter().map(CheckStatus::into_value).collect::<Vec<_>>(),
            "revision": self.revision,
            "truncated": self.completeness.is_truncated(),
        })
    }
}

/// Typed result of `change_request_comment`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeRequestCommentResult {
    id: u64,
    url: CodeHostUrl,
}

impl ChangeRequestCommentResult {
    /// Validates the created comment identity and URL.
    pub fn try_new(id: u64, url: String) -> Option<Self> {
        let url = CodeHostUrl::try_new(url)?;
        (id > 0).then_some(Self { id, url })
    }

    fn into_value(self) -> Value {
        json!({"id": self.id, "url": self.url.into_string()})
    }
}

/// One review-thread comment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThreadComment {
    id: String,
    author: Option<String>,
    body: String,
    url: CodeHostUrl,
}

impl ReviewThreadComment {
    /// Validates one review-thread comment.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        id: String,
        author: Option<String>,
        body: String,
        url: String,
    ) -> Option<Self> {
        let url = CodeHostUrl::try_new(url)?;
        (valid_opaque_id(&id)
            && author
                .as_deref()
                .is_none_or(|value| valid_required_text(bounds, value))
            && valid_text(bounds, &body))
        .then_some(Self {
            id,
            author,
            body,
            url,
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "author": self.author,
            "body": self.body,
            "id": self.id,
            "url": self.url.as_str(),
        })
    }
}

/// One review thread and its bounded comment page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThread {
    id: String,
    resolved: bool,
    outdated: bool,
    path: String,
    line: Option<u64>,
    comments: Vec<ReviewThreadComment>,
    comments_truncated: bool,
}

/// Complete checked fields for one review thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThreadFields {
    /// Opaque thread node identity.
    pub id: String,
    /// Whether the thread is resolved.
    pub resolved: bool,
    /// Whether its location is outdated.
    pub outdated: bool,
    /// Repository-relative path.
    pub path: String,
    /// Optional current line.
    pub line: Option<u64>,
    /// First bounded comment page.
    pub comments: Vec<ReviewThreadComment>,
    /// Whether more comments exist.
    pub comments_truncated: bool,
}

impl ReviewThread {
    /// Validates one review thread.
    pub fn try_new(bounds: CodeHostNumericBounds, fields: ReviewThreadFields) -> Option<Self> {
        (valid_opaque_id(&fields.id)
            && valid_path(&fields.path)
            && bounds.permits_result_items(fields.comments.len()))
        .then_some(Self {
            id: fields.id,
            resolved: fields.resolved,
            outdated: fields.outdated,
            path: fields.path,
            line: fields.line,
            comments: fields.comments,
            comments_truncated: fields.comments_truncated,
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "comments": self.comments.iter().map(ReviewThreadComment::to_value).collect::<Vec<_>>(),
            "comments_truncated": self.comments_truncated,
            "id": self.id,
            "line": self.line,
            "outdated": self.outdated,
            "path": self.path,
            "resolved": self.resolved,
        })
    }
}

/// Typed result of `change_request_review_threads`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThreadsResult {
    threads: Vec<ReviewThread>,
    completeness: CodeHostResultCompleteness,
}

impl ReviewThreadsResult {
    /// Validates one bounded review-thread page.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        threads: Vec<ReviewThread>,
        completeness: CodeHostResultCompleteness,
    ) -> Option<Self> {
        if !bounds.permits_result_items(threads.len()) {
            return None;
        }
        let text_budget = bounds
            .result_text_bytes()
            .map_or(MAX_ENCODED_RESULT_BYTES, |configured| {
                configured.min(MAX_ENCODED_RESULT_BYTES)
            });

        let mut result = Self {
            threads: Vec::with_capacity(threads.len()),
            completeness,
        };
        if !result.fits_text_budget(text_budget)? {
            return None;
        }

        for thread in threads {
            result.threads.push(thread);
            loop {
                if result.fits_text_budget(text_budget)? {
                    break;
                }

                let last = result.threads.last_mut()?;
                if last.comments.pop().is_some() {
                    last.comments_truncated = true;
                    continue;
                }

                result.threads.pop();
                result.completeness = CodeHostResultCompleteness::Truncated;
                return Some(result);
            }
        }

        Some(result)
    }

    fn fits_text_budget(&self, text_budget: usize) -> Option<bool> {
        let encoded = serde_json::to_vec(&self.to_value()).ok()?;
        Some(encoded.len() <= text_budget)
    }

    fn to_value(&self) -> Value {
        json!({
            "threads": self.threads.iter().map(ReviewThread::to_value).collect::<Vec<_>>(),
            "truncated": self.completeness.is_truncated(),
        })
    }

    fn into_value(self) -> Value {
        self.to_value()
    }
}

pub(super) fn bound_scrubbed_review_threads_value(
    bounds: CodeHostNumericBounds,
    value: &mut Value,
) -> Option<()> {
    let text_budget = bounds
        .result_text_bytes()
        .map_or(MAX_ENCODED_RESULT_BYTES, |configured| {
            configured.min(MAX_ENCODED_RESULT_BYTES)
        });

    loop {
        let encoded = serde_json::to_vec(value).ok()?;
        if encoded.len() <= text_budget {
            return Some(());
        }

        let threads = value.get_mut("threads")?.as_array_mut()?;
        let last = threads.last_mut()?;
        let comments = last.get_mut("comments")?.as_array_mut()?;
        if comments.pop().is_some() {
            *last.get_mut("comments_truncated")? = Value::Bool(true);
            continue;
        }

        threads.pop();
        *value.get_mut("truncated")? = Value::Bool(true);
    }
}

/// Typed result of `change_request_thread_reply`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadReplyResult {
    id: String,
    url: CodeHostUrl,
}

impl ThreadReplyResult {
    /// Validates the created reply identity and URL.
    pub fn try_new(id: String, url: String) -> Option<Self> {
        let url = CodeHostUrl::try_new(url)?;
        valid_opaque_id(&id).then_some(Self { id, url })
    }

    fn into_value(self) -> Value {
        json!({"id": self.id, "url": self.url.into_string()})
    }
}

/// Typed result of `change_request_thread_resolve`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadResolveResult {
    thread_id: String,
    resolution: ReviewThreadResolution,
}

impl ThreadResolveResult {
    /// Validates the resolved thread identity.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        thread_id: String,
        resolution: ReviewThreadResolution,
    ) -> Option<Self> {
        (valid_required_text(bounds, &thread_id) && resolution == ReviewThreadResolution::Resolved)
            .then_some(Self {
                thread_id,
                resolution,
            })
    }

    fn into_value(self) -> Value {
        json!({
            "resolved": self.resolution.is_resolved(),
            "thread_id": self.thread_id,
        })
    }
}

/// Typed result of `change_request_ci_job_log`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CiJobLogResult {
    job_id: u64,
    text: String,
    completeness: CodeHostResultCompleteness,
}

impl CiJobLogResult {
    /// Validates one bounded job-log prefix.
    pub fn try_new(
        bounds: CodeHostNumericBounds,
        job_id: u64,
        text: String,
        completeness: CodeHostResultCompleteness,
    ) -> Option<Self> {
        (job_id > 0 && valid_text(bounds, &text)).then_some(Self {
            job_id,
            text,
            completeness,
        })
    }

    fn into_value(self) -> Value {
        json!({
            "job_id": self.job_id,
            "text": self.text,
            "truncated": self.completeness.is_truncated(),
        })
    }
}

/// Typed acknowledgement from `change_request_rerun_failed_jobs`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RerunFailedJobsResult {
    run_id: u64,
}

impl RerunFailedJobsResult {
    /// Validates one acknowledged workflow-run identity.
    pub const fn try_new(run_id: u64) -> Option<Self> {
        if run_id > 0 {
            Some(Self { run_id })
        } else {
            None
        }
    }

    fn into_value(self) -> Value {
        json!({"run_id": self.run_id})
    }
}

/// Closed typed result vocabulary for all fourteen code-host tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodeHostResult {
    /// Change-request summary.
    Summary(ChangeRequestSummaryResult),
    /// Bounded changed-file page.
    ChangedFiles(ChangedFilesResult),
    /// One per-file patch.
    FilePatch(FilePatchResult),
    /// Bounded exact-revision repository directory listing.
    ListDirectory(super::RepositoryListDirectoryResult),
    /// Bounded exact-revision repository file read.
    ReadFile(super::RepositoryReadFileResult),
    /// Bounded check-run page.
    ChecksStatus(ChecksStatusResult),
    /// Created top-level comment.
    Comment(ChangeRequestCommentResult),
    /// Bounded review-thread page.
    ReviewThreads(ReviewThreadsResult),
    /// Created thread reply.
    ThreadReply(ThreadReplyResult),
    /// Resolved thread.
    ThreadResolve(ThreadResolveResult),
    /// Bounded CI job log.
    CiJobLog(CiJobLogResult),
    /// Accepted rerun request.
    RerunFailedJobs(RerunFailedJobsResult),
    /// Parent and immediate-child stack ancestry evidence.
    StackState(super::StackStateResult),
    /// Structured bounded thread inventory.
    ThreadInventory(super::ThreadInventoryResult),
}

impl CodeHostResult {
    /// Converts the checked result into its exact bounded tool-output object.
    pub fn into_json_value(self) -> Value {
        match self {
            Self::Summary(result) => result.into_value(),
            Self::ChangedFiles(result) => result.into_value(),
            Self::FilePatch(result) => result.into_value(),
            Self::ListDirectory(result) => result.into_value(),
            Self::ReadFile(result) => result.into_value(),
            Self::ChecksStatus(result) => result.into_value(),
            Self::Comment(result) => result.into_value(),
            Self::ReviewThreads(result) => result.into_value(),
            Self::ThreadReply(result) => result.into_value(),
            Self::ThreadResolve(result) => result.into_value(),
            Self::CiJobLog(result) => result.into_value(),
            Self::RerunFailedJobs(result) => result.into_value(),
            Self::StackState(result) => super::review_slog::stack_into_value(result),
            Self::ThreadInventory(result) => super::review_slog::inventory_into_value(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, body: &str) -> ReviewThreadComment {
        ReviewThreadComment::try_new(
            crate::code_host::test_numeric_bounds(),
            id.to_owned(),
            Some(String::from("reviewer")),
            body.to_owned(),
            format!("https://github.example/comments/{id}"),
        )
        .expect("fixture comment is valid")
    }

    fn thread(
        id: &str,
        comments: Vec<ReviewThreadComment>,
        comments_truncated: bool,
    ) -> ReviewThread {
        ReviewThread::try_new(
            crate::code_host::test_numeric_bounds(),
            ReviewThreadFields {
                id: id.to_owned(),
                resolved: false,
                outdated: false,
                path: String::from("src/lib.rs"),
                line: Some(17),
                comments,
                comments_truncated,
            },
        )
        .expect("fixture thread is valid")
    }

    fn fixture_result(
        threads: Vec<ReviewThread>,
        completeness: CodeHostResultCompleteness,
    ) -> Value {
        ReviewThreadsResult::try_new(
            crate::code_host::test_numeric_bounds(),
            threads,
            completeness,
        )
        .expect("fixture result is valid")
        .into_value()
    }

    fn encoded_len(value: &Value) -> usize {
        serde_json::to_vec(value)
            .expect("fixture result encodes")
            .len()
    }

    /// The configured text ceiling applies to the complete encoded result
    /// while a retained thread reports its shortened comment page locally.
    #[test]
    fn review_threads_result_bounds_nested_comments_without_inventing_a_thread_suffix() {
        let complete_prefix = fixture_result(
            vec![thread(
                "PRRT_first",
                vec![comment("PRRC_first", "first finding")],
                false,
            )],
            CodeHostResultCompleteness::Complete,
        );
        let encoded_limit = encoded_len(&complete_prefix);
        let bounds =
            CodeHostNumericBounds::new(None, None, None, Some(encoded_limit), Some(100), None);
        let actual = ReviewThreadsResult::try_new(
            bounds,
            vec![thread(
                "PRRT_first",
                vec![
                    comment("PRRC_first", "first finding"),
                    comment("PRRC_second", "second finding"),
                ],
                false,
            )],
            CodeHostResultCompleteness::Complete,
        )
        .expect("a bounded prefix remains representable")
        .into_value();
        let expected = fixture_result(
            vec![thread(
                "PRRT_first",
                vec![comment("PRRC_first", "first finding")],
                true,
            )],
            CodeHostResultCompleteness::Complete,
        );

        assert_eq!(actual, expected);
        assert!(encoded_len(&actual) <= encoded_limit);
    }

    /// An aggregate ceiling that cannot retain the next thread marks the outer
    /// page truncated without rewriting the retained thread's comment posture.
    #[test]
    fn review_threads_result_marks_an_omitted_thread_suffix() {
        let complete_prefix = fixture_result(
            vec![thread(
                "PRRT_first",
                vec![comment("PRRC_first", "first finding")],
                false,
            )],
            CodeHostResultCompleteness::Complete,
        );
        let encoded_limit = encoded_len(&complete_prefix);
        let bounds =
            CodeHostNumericBounds::new(None, None, None, Some(encoded_limit), Some(100), None);
        let actual = ReviewThreadsResult::try_new(
            bounds,
            vec![
                thread(
                    "PRRT_first",
                    vec![comment("PRRC_first", "first finding")],
                    false,
                ),
                thread(
                    "PRRT_second",
                    vec![comment("PRRC_second", "second finding")],
                    false,
                ),
            ],
            CodeHostResultCompleteness::Complete,
        )
        .expect("a bounded thread prefix remains representable")
        .into_value();
        let expected = fixture_result(
            vec![thread(
                "PRRT_first",
                vec![comment("PRRC_first", "first finding")],
                false,
            )],
            CodeHostResultCompleteness::Truncated,
        );

        assert_eq!(actual, expected);
        assert!(encoded_len(&actual) <= encoded_limit);
    }
}

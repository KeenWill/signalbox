//! Typed, bounded results for exact-revision repository content reads.

use serde_json::{Value, json};

use super::arguments::valid_revision;
use super::result::{
    CodeHostResultCompleteness, MAX_RESULT_ITEMS, MAX_RESULT_TEXT_BYTES, valid_path, valid_text,
};

/// Maximum retained UTF-8 content from one repository file read.
pub(super) const MAX_REPOSITORY_FILE_CONTENT_BYTES: usize = MAX_RESULT_TEXT_BYTES;

/// Kind of one repository path observed through GitHub's contents endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryObjectKind {
    /// Regular file.
    File,
    /// Directory tree.
    Directory,
    /// Symbolic link that GitHub did not dereference to a regular file.
    Symlink,
    /// Git submodule entry.
    Submodule,
}

impl RepositoryObjectKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Submodule => "submodule",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RepositoryObjectIdentity {
    path: String,
    revision: String,
}

impl RepositoryObjectIdentity {
    fn try_new(path: String, revision: String) -> Option<Self> {
        (valid_path(&path) && valid_revision(&revision)).then_some(Self { path, revision })
    }
}

/// Complete checked fields for one successful bounded repository file read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryFileContentFields {
    /// Requested repository-relative path.
    pub path: String,
    /// Required exact revision.
    pub revision: String,
    /// Exact source blob size reported by GitHub.
    pub source_bytes: u64,
    /// Optional first requested line.
    pub requested_start_line: Option<u32>,
    /// Optional inclusive last requested line.
    pub requested_end_line: Option<u32>,
    /// First returned line, absent when no content was returned.
    pub start_line: Option<u32>,
    /// Inclusive last returned line, absent when no content was returned.
    pub end_line: Option<u32>,
    /// Number of returned lines, including a final partial line.
    pub returned_lines: u32,
    /// Whether the last returned line is complete.
    pub last_line_complete: bool,
    /// Exact retained UTF-8 content.
    pub content: String,
    /// Whether source content was discarded because of the result bound.
    pub completeness: CodeHostResultCompleteness,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepositoryFileOutcome {
    Content(RepositoryFileContentFields),
    PathNotFound,
    RevisionNotFound,
    NotAFile(RepositoryObjectKind),
    Binary { source_bytes: u64 },
}

/// Typed result of `repository_read_file`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryReadFileResult {
    identity: RepositoryObjectIdentity,
    outcome: RepositoryFileOutcome,
}

impl RepositoryReadFileResult {
    /// Validates one bounded UTF-8 file selection and its completeness facts.
    pub fn try_content(fields: RepositoryFileContentFields) -> Option<Self> {
        let identity =
            RepositoryObjectIdentity::try_new(fields.path.clone(), fields.revision.clone())?;
        let requested_range_valid = match (fields.requested_start_line, fields.requested_end_line) {
            (None, None) => true,
            (Some(start), Some(end)) => start > 0 && start <= end,
            (None, Some(_)) | (Some(_), None) => false,
        };
        let returned_range_valid = match (fields.start_line, fields.end_line) {
            (None, None) => fields.content.is_empty() && fields.returned_lines == 0,
            (Some(start), Some(end)) => {
                start > 0
                    && start <= end
                    && end.checked_sub(start).and_then(|span| span.checked_add(1))
                        == Some(fields.returned_lines)
                    && line_count(&fields.content) == fields.returned_lines
            }
            (None, Some(_)) | (Some(_), None) => false,
        };
        let returned_within_request = match (
            fields.requested_start_line,
            fields.requested_end_line,
            fields.start_line,
            fields.end_line,
        ) {
            (Some(requested_start), Some(requested_end), Some(start), Some(end)) => {
                start >= requested_start && end <= requested_end
            }
            (Some(_), Some(_), None, None) => true,
            (None, None, Some(start), Some(_)) => start == 1,
            (None, None, None, None) => true,
            _ => false,
        };
        let returned_bytes = u64::try_from(fields.content.len()).ok()?;
        let completeness_consistent = fields.completeness == CodeHostResultCompleteness::Truncated
            || fields.last_line_complete;
        (requested_range_valid
            && returned_range_valid
            && returned_within_request
            && valid_text(&fields.content)
            && fields.content.len() <= MAX_REPOSITORY_FILE_CONTENT_BYTES
            && fields.source_bytes >= returned_bytes
            && completeness_consistent)
            .then_some(Self {
                identity,
                outcome: RepositoryFileOutcome::Content(fields),
            })
    }

    /// Records that the requested path was absent at the exact revision.
    pub fn try_path_not_found(path: String, revision: String) -> Option<Self> {
        Some(Self {
            identity: RepositoryObjectIdentity::try_new(path, revision)?,
            outcome: RepositoryFileOutcome::PathNotFound,
        })
    }

    /// Records that GitHub did not recognize the requested exact revision.
    pub fn try_revision_not_found(path: String, revision: String) -> Option<Self> {
        Some(Self {
            identity: RepositoryObjectIdentity::try_new(path, revision)?,
            outcome: RepositoryFileOutcome::RevisionNotFound,
        })
    }

    /// Records that the requested path names a non-file repository object.
    pub fn try_not_a_file(
        path: String,
        revision: String,
        kind: RepositoryObjectKind,
    ) -> Option<Self> {
        if kind == RepositoryObjectKind::File {
            return None;
        }
        Some(Self {
            identity: RepositoryObjectIdentity::try_new(path, revision)?,
            outcome: RepositoryFileOutcome::NotAFile(kind),
        })
    }

    /// Records that the exact blob is not UTF-8 text.
    pub fn try_binary(path: String, revision: String, source_bytes: u64) -> Option<Self> {
        Some(Self {
            identity: RepositoryObjectIdentity::try_new(path, revision)?,
            outcome: RepositoryFileOutcome::Binary { source_bytes },
        })
    }

    pub(super) fn into_value(self) -> Value {
        let path = self.identity.path;
        let revision = self.identity.revision;
        match self.outcome {
            RepositoryFileOutcome::Content(fields) => json!({
                "content": fields.content,
                "end_line": fields.end_line,
                "last_line_complete": fields.last_line_complete,
                "outcome": "content",
                "path": path,
                "requested_line_range": fields.requested_start_line.zip(fields.requested_end_line).map(
                    |(start, end)| json!({"end": end, "start": start})
                ),
                "returned_bytes": fields.content.len(),
                "returned_lines": fields.returned_lines,
                "revision": revision,
                "source_bytes": fields.source_bytes,
                "start_line": fields.start_line,
                "truncated": fields.completeness.is_truncated(),
            }),
            RepositoryFileOutcome::PathNotFound => json!({
                "outcome": "path_not_found",
                "path": path,
                "revision": revision,
                "truncated": false,
            }),
            RepositoryFileOutcome::RevisionNotFound => json!({
                "outcome": "revision_not_found",
                "path": path,
                "revision": revision,
                "truncated": false,
            }),
            RepositoryFileOutcome::NotAFile(kind) => json!({
                "object_type": kind.as_str(),
                "outcome": "not_a_file",
                "path": path,
                "revision": revision,
                "truncated": false,
            }),
            RepositoryFileOutcome::Binary { source_bytes } => json!({
                "outcome": "binary_file",
                "path": path,
                "revision": revision,
                "source_bytes": source_bytes,
                "truncated": false,
            }),
        }
    }
}

fn line_count(content: &str) -> u32 {
    if content.is_empty() {
        return 0;
    }
    let terminated_lines = content.bytes().filter(|byte| *byte == b'\n').count();
    let lines = terminated_lines + usize::from(!content.ends_with('\n'));
    u32::try_from(lines).unwrap_or(u32::MAX)
}

/// One checked repository directory entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryDirectoryEntry {
    path: String,
    kind: RepositoryObjectKind,
    size_bytes: Option<u64>,
}

impl RepositoryDirectoryEntry {
    /// Validates one projected directory entry.
    pub fn try_new(
        path: String,
        kind: RepositoryObjectKind,
        size_bytes: Option<u64>,
    ) -> Option<Self> {
        valid_path(&path).then_some(Self {
            path,
            kind,
            size_bytes,
        })
    }

    pub(super) fn into_value(self) -> Value {
        json!({
            "object_type": self.kind.as_str(),
            "path": self.path,
            "size_bytes": self.size_bytes,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepositoryDirectoryOutcome {
    Entries {
        entries: Vec<RepositoryDirectoryEntry>,
        observed_entries: usize,
        completeness: CodeHostResultCompleteness,
    },
    PathNotFound,
    RevisionNotFound,
    NotADirectory(RepositoryObjectKind),
}

/// Typed result of `repository_list_directory`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryListDirectoryResult {
    identity: RepositoryObjectIdentity,
    outcome: RepositoryDirectoryOutcome,
}

impl RepositoryListDirectoryResult {
    /// Validates one bounded prefix of directory entries.
    pub fn try_entries(
        path: String,
        revision: String,
        entries: Vec<RepositoryDirectoryEntry>,
        observed_entries: usize,
        completeness: CodeHostResultCompleteness,
    ) -> Option<Self> {
        let count_consistent = match completeness {
            CodeHostResultCompleteness::Complete => observed_entries == entries.len(),
            CodeHostResultCompleteness::Truncated => observed_entries >= entries.len(),
        };
        let identity = RepositoryObjectIdentity::try_new(path, revision)?;
        (entries.len() <= MAX_RESULT_ITEMS && count_consistent).then_some(Self {
            identity,
            outcome: RepositoryDirectoryOutcome::Entries {
                entries,
                observed_entries,
                completeness,
            },
        })
    }

    /// Records that the requested path was absent at the exact revision.
    pub fn try_path_not_found(path: String, revision: String) -> Option<Self> {
        Some(Self {
            identity: RepositoryObjectIdentity::try_new(path, revision)?,
            outcome: RepositoryDirectoryOutcome::PathNotFound,
        })
    }

    /// Records that GitHub did not recognize the requested exact revision.
    pub fn try_revision_not_found(path: String, revision: String) -> Option<Self> {
        Some(Self {
            identity: RepositoryObjectIdentity::try_new(path, revision)?,
            outcome: RepositoryDirectoryOutcome::RevisionNotFound,
        })
    }

    /// Records that the requested path names a non-directory object.
    pub fn try_not_a_directory(
        path: String,
        revision: String,
        kind: RepositoryObjectKind,
    ) -> Option<Self> {
        if kind == RepositoryObjectKind::Directory {
            return None;
        }
        Some(Self {
            identity: RepositoryObjectIdentity::try_new(path, revision)?,
            outcome: RepositoryDirectoryOutcome::NotADirectory(kind),
        })
    }

    pub(super) fn encoded_len(&self) -> Option<usize> {
        serde_json::to_vec(&self.clone().into_value())
            .ok()
            .map(|encoded| encoded.len())
    }

    pub(super) fn into_value(self) -> Value {
        let path = self.identity.path;
        let revision = self.identity.revision;
        match self.outcome {
            RepositoryDirectoryOutcome::Entries {
                entries,
                observed_entries,
                completeness,
            } => {
                let returned_entries = entries.len();
                json!({
                    "entries": entries.into_iter().map(RepositoryDirectoryEntry::into_value).collect::<Vec<_>>(),
                    "observed_entries": observed_entries,
                    "outcome": "entries",
                    "path": path,
                    "returned_entries": returned_entries,
                    "revision": revision,
                    "truncated": completeness.is_truncated(),
                })
            }
            RepositoryDirectoryOutcome::PathNotFound => json!({
                "outcome": "path_not_found",
                "path": path,
                "revision": revision,
                "truncated": false,
            }),
            RepositoryDirectoryOutcome::RevisionNotFound => json!({
                "outcome": "revision_not_found",
                "path": path,
                "revision": revision,
                "truncated": false,
            }),
            RepositoryDirectoryOutcome::NotADirectory(kind) => json!({
                "object_type": kind.as_str(),
                "outcome": "not_a_directory",
                "path": path,
                "revision": revision,
                "truncated": false,
            }),
        }
    }
}

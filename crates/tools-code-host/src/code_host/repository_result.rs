//! Typed, bounded results for exact-revision repository content reads.

use std::collections::HashSet;

use serde_json::{Value, json};

use super::arguments::{CodeHostFilePath, CodeHostRepository, CodeHostRevision};
use super::result::{
    CodeHostResultCompleteness, MAX_ENCODED_RESULT_BYTES, MAX_RESULT_ITEMS, MAX_RESULT_TEXT_BYTES,
    valid_text,
};
use super::{RepositoryLineRange, RepositoryListDirectoryArguments, RepositoryReadFileArguments};

/// Maximum retained UTF-8 content from one repository file read.
// numeric-bound: tunable - controls retained repository-file content
pub(super) const MAX_REPOSITORY_FILE_CONTENT_BYTES: usize = MAX_RESULT_TEXT_BYTES;
/// Maximum source bytes inspected to serve one requested line range.
// numeric-bound: tunable - the ranged-read size this tool advertises serving
pub(super) const MAX_REPOSITORY_FILE_SCAN_BYTES: usize = 1024 * 1024;
/// Maximum entries GitHub can expose through one contents response.
// numeric-bound: not-a-bound - GitHub's fixed contents-endpoint entry exposure
pub(super) const MAX_OBSERVED_DIRECTORY_ENTRIES: usize = 1_000;
// numeric-bound: not-a-bound - fixed maximum UTF-8 continuation width
const MAX_UTF8_BOUNDARY_DISCARD_BYTES: usize = 3;

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
    repository: CodeHostRepository,
    path: CodeHostFilePath,
    revision: CodeHostRevision,
}

impl RepositoryObjectIdentity {
    fn from_file_arguments(arguments: &RepositoryReadFileArguments) -> Self {
        Self {
            repository: arguments.repository().clone(),
            path: arguments.path().clone(),
            revision: arguments.revision().clone(),
        }
    }

    fn from_directory_arguments(arguments: &RepositoryListDirectoryArguments) -> Self {
        Self {
            repository: arguments.repository().clone(),
            path: arguments.path().clone(),
            revision: arguments.revision().clone(),
        }
    }

    fn matches_file_arguments(&self, arguments: &RepositoryReadFileArguments) -> bool {
        self.repository == *arguments.repository()
            && self.path == *arguments.path()
            && self.revision == *arguments.revision()
    }

    fn matches_directory_arguments(&self, arguments: &RepositoryListDirectoryArguments) -> bool {
        self.repository == *arguments.repository()
            && self.path == *arguments.path()
            && self.revision == *arguments.revision()
    }
}

/// Complete checked fields for one successful bounded repository file read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryFileContentFields {
    /// Exact source blob size reported by GitHub.
    pub source_bytes: u64,
    /// Exact source bytes observed inside the requested selection.
    pub selected_source_bytes: u64,
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
    Binary {
        source_bytes: u64,
    },
    LineRangeUnavailable {
        source_bytes: u64,
        requested_start_line: u32,
        requested_end_line: u32,
    },
}

/// Typed result of `repository_read_file`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryReadFileResult {
    identity: RepositoryObjectIdentity,
    requested_line_range: Option<RepositoryLineRange>,
    outcome: RepositoryFileOutcome,
}

impl RepositoryReadFileResult {
    /// Validates one bounded UTF-8 file selection and its completeness facts.
    pub fn try_content(
        arguments: &RepositoryReadFileArguments,
        fields: RepositoryFileContentFields,
    ) -> Option<Self> {
        let requested_line_range = arguments.line_range();
        let requested_start_line = requested_line_range.map(RepositoryLineRange::start);
        let requested_end_line = requested_line_range.map(RepositoryLineRange::end);
        let path_can_be_file = arguments.path().as_str() != ".";
        let source_within_scan_limit = requested_line_range.is_none()
            || fields.source_bytes <= u64::try_from(MAX_REPOSITORY_FILE_SCAN_BYTES).ok()?;
        let empty_selection_valid = match requested_start_line {
            None | Some(1) => fields.source_bytes == 0,
            Some(_) => true,
        };
        let returned_range_valid = match (fields.start_line, fields.end_line) {
            (None, None) => {
                fields.content.is_empty() && fields.returned_lines == 0 && empty_selection_valid
            }
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
            requested_start_line,
            requested_end_line,
            fields.start_line,
            fields.end_line,
        ) {
            (Some(requested_start), Some(requested_end), Some(start), Some(end)) => {
                start == requested_start && end <= requested_end
            }
            (Some(_), Some(_), None, None) => true,
            (None, None, Some(start), Some(_)) => start == 1,
            (None, None, None, None) => true,
            _ => false,
        };
        let returned_bytes = u64::try_from(fields.content.len()).ok()?;
        let preceding_line_bytes = fields
            .start_line
            .map_or(0, |line| u64::from(line.saturating_sub(1)));
        let minimum_source_bytes = returned_bytes.checked_add(preceding_line_bytes)?;
        let source_bytes_consistent = match requested_line_range {
            None => match fields.completeness {
                CodeHostResultCompleteness::Complete => fields.source_bytes == returned_bytes,
                CodeHostResultCompleteness::Truncated => fields.source_bytes > returned_bytes,
            },
            Some(_) => match fields.completeness {
                CodeHostResultCompleteness::Complete => fields.source_bytes >= minimum_source_bytes,
                CodeHostResultCompleteness::Truncated => fields.source_bytes > minimum_source_bytes,
            },
        };
        let content_ends_at_line_boundary =
            fields.content.is_empty() || fields.content.ends_with('\n');
        let selected_source_bytes_consistent = fields.selected_source_bytes <= fields.source_bytes
            && match requested_line_range {
                None => fields.selected_source_bytes == fields.source_bytes,
                Some(_) => true,
            }
            && match fields.completeness {
                CodeHostResultCompleteness::Complete => {
                    fields.selected_source_bytes == returned_bytes
                }
                CodeHostResultCompleteness::Truncated => {
                    fields.selected_source_bytes > returned_bytes
                }
            };
        let complete_first_line_consistent = match fields.completeness {
            CodeHostResultCompleteness::Complete => match requested_start_line {
                Some(1) => content_ends_at_line_boundary || fields.source_bytes == returned_bytes,
                None | Some(_) => true,
            },
            CodeHostResultCompleteness::Truncated => true,
        };
        let range_coverage_consistent = match (
            fields.completeness,
            requested_start_line,
            requested_end_line,
            fields.end_line,
        ) {
            (
                CodeHostResultCompleteness::Complete,
                Some(1),
                Some(requested_end),
                Some(returned_end),
            ) if returned_end < requested_end => fields.source_bytes == returned_bytes,
            (
                CodeHostResultCompleteness::Truncated,
                Some(_),
                Some(requested_end),
                Some(returned_end),
            ) => returned_end < requested_end || !content_ends_at_line_boundary,
            (CodeHostResultCompleteness::Complete, _, _, _)
            | (CodeHostResultCompleteness::Truncated, _, _, _) => true,
        };
        let retention_bound_consistent = match fields.completeness {
            CodeHostResultCompleteness::Complete => true,
            CodeHostResultCompleteness::Truncated => {
                fields.content.len()
                    >= MAX_REPOSITORY_FILE_CONTENT_BYTES - MAX_UTF8_BOUNDARY_DISCARD_BYTES
            }
        };
        let last_line_complete = fields.content.is_empty()
            || fields.content.ends_with('\n')
            || fields.completeness == CodeHostResultCompleteness::Complete;
        (path_can_be_file
            && returned_range_valid
            && returned_within_request
            && valid_text(&fields.content)
            && fields.content.len() <= MAX_REPOSITORY_FILE_CONTENT_BYTES
            && source_bytes_consistent
            && selected_source_bytes_consistent
            && complete_first_line_consistent
            && range_coverage_consistent
            && retention_bound_consistent
            && source_within_scan_limit
            && fields.last_line_complete == last_line_complete)
            .then_some(Self {
                identity: RepositoryObjectIdentity::from_file_arguments(arguments),
                requested_line_range,
                outcome: RepositoryFileOutcome::Content(fields),
            })
    }

    /// Records that the requested path was absent at the exact revision.
    pub fn try_path_not_found(arguments: &RepositoryReadFileArguments) -> Option<Self> {
        (arguments.path().as_str() != ".").then_some(Self {
            identity: RepositoryObjectIdentity::from_file_arguments(arguments),
            requested_line_range: arguments.line_range(),
            outcome: RepositoryFileOutcome::PathNotFound,
        })
    }

    /// Records that GitHub did not recognize the requested exact revision.
    pub fn revision_not_found(arguments: &RepositoryReadFileArguments) -> Self {
        Self {
            identity: RepositoryObjectIdentity::from_file_arguments(arguments),
            requested_line_range: arguments.line_range(),
            outcome: RepositoryFileOutcome::RevisionNotFound,
        }
    }

    /// Records that the requested path names a non-file repository object.
    pub fn try_not_a_file(
        arguments: &RepositoryReadFileArguments,
        kind: RepositoryObjectKind,
    ) -> Option<Self> {
        let kind_matches_path = match (arguments.path().as_str() == ".", kind) {
            (true, RepositoryObjectKind::Directory) => true,
            (
                true,
                RepositoryObjectKind::File
                | RepositoryObjectKind::Symlink
                | RepositoryObjectKind::Submodule,
            ) => false,
            (false, RepositoryObjectKind::File) => false,
            (
                false,
                RepositoryObjectKind::Directory
                | RepositoryObjectKind::Symlink
                | RepositoryObjectKind::Submodule,
            ) => true,
        };
        kind_matches_path.then_some(Self {
            identity: RepositoryObjectIdentity::from_file_arguments(arguments),
            requested_line_range: arguments.line_range(),
            outcome: RepositoryFileOutcome::NotAFile(kind),
        })
    }

    /// Records that the exact blob is not UTF-8 text.
    pub fn try_binary(arguments: &RepositoryReadFileArguments, source_bytes: u64) -> Option<Self> {
        let source_within_scan_limit = arguments.line_range().is_none()
            || source_bytes <= u64::try_from(MAX_REPOSITORY_FILE_SCAN_BYTES).ok()?;
        (arguments.path().as_str() != "." && source_bytes > 0 && source_within_scan_limit)
            .then_some(Self {
                identity: RepositoryObjectIdentity::from_file_arguments(arguments),
                requested_line_range: arguments.line_range(),
                outcome: RepositoryFileOutcome::Binary { source_bytes },
            })
    }

    /// Records that bounded transport cannot inspect enough source to select
    /// the requested lines without downloading the whole oversized blob.
    pub fn try_line_range_unavailable(
        arguments: &RepositoryReadFileArguments,
        source_bytes: u64,
    ) -> Option<Self> {
        let requested_line_range = arguments.line_range()?;
        let requested_start_line = requested_line_range.start();
        let requested_end_line = requested_line_range.end();
        let scan_limit_bytes = u64::try_from(MAX_REPOSITORY_FILE_SCAN_BYTES).ok()?;
        (arguments.path().as_str() != "."
            && requested_start_line > 0
            && requested_start_line <= requested_end_line
            && source_bytes > scan_limit_bytes)
            .then_some(Self {
                identity: RepositoryObjectIdentity::from_file_arguments(arguments),
                requested_line_range: Some(requested_line_range),
                outcome: RepositoryFileOutcome::LineRangeUnavailable {
                    source_bytes,
                    requested_start_line,
                    requested_end_line,
                },
            })
    }

    pub(super) fn matches(&self, arguments: &RepositoryReadFileArguments) -> bool {
        self.identity.matches_file_arguments(arguments)
            && self.requested_line_range == arguments.line_range()
    }

    pub(super) fn into_value(self) -> Value {
        let path = self.identity.path.as_str().to_owned();
        let revision = self.identity.revision.as_str().to_owned();
        let requested_line_range = self.requested_line_range;
        match self.outcome {
            RepositoryFileOutcome::Content(fields) => json!({
                "content": fields.content,
                "end_line": fields.end_line,
                "last_line_complete": fields.last_line_complete,
                "outcome": "content",
                "path": path,
                "requested_line_range": requested_line_range.map(
                    |range| json!({"end": range.end(), "start": range.start()})
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
            RepositoryFileOutcome::LineRangeUnavailable {
                source_bytes,
                requested_start_line,
                requested_end_line,
            } => json!({
                "outcome": "line_range_unavailable",
                "path": path,
                "requested_line_range": {
                    "end": requested_end_line,
                    "start": requested_start_line,
                },
                "revision": revision,
                "scan_limit_bytes": MAX_REPOSITORY_FILE_SCAN_BYTES,
                "source_bytes": source_bytes,
                "truncated": true,
            }),
        }
    }
}

pub(super) fn line_count(content: &str) -> u32 {
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
    path: CodeHostFilePath,
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
        Some(Self {
            path: CodeHostFilePath::try_new(path).ok()?,
            kind,
            size_bytes,
        })
    }

    pub(super) fn path(&self) -> &str {
        self.path.as_str()
    }

    pub(super) fn into_value(self) -> Value {
        json!({
            "object_type": self.kind.as_str(),
            "path": self.path.as_str(),
            "size_bytes": self.size_bytes,
        })
    }
}

pub(super) fn is_immediate_repository_child(parent: &str, child: &str) -> bool {
    if parent == "." {
        return !child.is_empty() && child != "." && !child.contains('/');
    }
    child
        .strip_prefix(parent)
        .and_then(|suffix| suffix.strip_prefix('/'))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
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
        arguments: &RepositoryListDirectoryArguments,
        entries: Vec<RepositoryDirectoryEntry>,
        observed_entries: usize,
        completeness: CodeHostResultCompleteness,
    ) -> Option<Self> {
        let result =
            Self::try_entries_candidate(arguments, entries, observed_entries, completeness)?;
        (result.encoded_len()? <= MAX_ENCODED_RESULT_BYTES).then_some(result)
    }

    fn try_entries_candidate(
        arguments: &RepositoryListDirectoryArguments,
        entries: Vec<RepositoryDirectoryEntry>,
        observed_entries: usize,
        completeness: CodeHostResultCompleteness,
    ) -> Option<Self> {
        let count_consistent = match completeness {
            CodeHostResultCompleteness::Complete => observed_entries == entries.len(),
            CodeHostResultCompleteness::Truncated => observed_entries >= entries.len(),
        };
        let identity = RepositoryObjectIdentity::from_directory_arguments(arguments);
        let entries_belong_to_directory = entries.iter().all(|entry| {
            is_immediate_repository_child(identity.path.as_str(), entry.path.as_str())
        });
        let unique_entry_paths = entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<HashSet<_>>()
            .len()
            == entries.len();
        (entries.len() <= MAX_RESULT_ITEMS
            && observed_entries <= MAX_OBSERVED_DIRECTORY_ENTRIES
            && count_consistent
            && entries_belong_to_directory
            && unique_entry_paths)
            .then_some(Self {
                identity,
                outcome: RepositoryDirectoryOutcome::Entries {
                    entries,
                    observed_entries,
                    completeness,
                },
            })
    }

    pub(super) fn entries_encoded_len(
        arguments: &RepositoryListDirectoryArguments,
        entries: Vec<RepositoryDirectoryEntry>,
        observed_entries: usize,
        completeness: CodeHostResultCompleteness,
    ) -> Option<usize> {
        Self::try_entries_candidate(arguments, entries, observed_entries, completeness)?
            .encoded_len()
    }

    /// Records that the requested path was absent at the exact revision.
    pub fn try_path_not_found(arguments: &RepositoryListDirectoryArguments) -> Option<Self> {
        (arguments.path().as_str() != ".").then_some(Self {
            identity: RepositoryObjectIdentity::from_directory_arguments(arguments),
            outcome: RepositoryDirectoryOutcome::PathNotFound,
        })
    }

    /// Records that GitHub did not recognize the requested exact revision.
    pub fn revision_not_found(arguments: &RepositoryListDirectoryArguments) -> Self {
        Self {
            identity: RepositoryObjectIdentity::from_directory_arguments(arguments),
            outcome: RepositoryDirectoryOutcome::RevisionNotFound,
        }
    }

    /// Records that the requested path names a non-directory object.
    pub fn try_not_a_directory(
        arguments: &RepositoryListDirectoryArguments,
        kind: RepositoryObjectKind,
    ) -> Option<Self> {
        let kind_matches_path = match (arguments.path().as_str() == ".", kind) {
            (
                true,
                RepositoryObjectKind::File
                | RepositoryObjectKind::Directory
                | RepositoryObjectKind::Symlink
                | RepositoryObjectKind::Submodule,
            ) => false,
            (false, RepositoryObjectKind::Directory) => false,
            (
                false,
                RepositoryObjectKind::File
                | RepositoryObjectKind::Symlink
                | RepositoryObjectKind::Submodule,
            ) => true,
        };
        kind_matches_path.then_some(Self {
            identity: RepositoryObjectIdentity::from_directory_arguments(arguments),
            outcome: RepositoryDirectoryOutcome::NotADirectory(kind),
        })
    }

    pub(super) fn encoded_len(&self) -> Option<usize> {
        serde_json::to_vec(&self.clone().into_value())
            .ok()
            .map(|encoded| encoded.len())
    }

    pub(super) fn matches(&self, arguments: &RepositoryListDirectoryArguments) -> bool {
        self.identity.matches_directory_arguments(arguments)
    }

    pub(super) fn into_value(self) -> Value {
        let path = self.identity.path.as_str().to_owned();
        let revision = self.identity.revision.as_str().to_owned();
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

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_REVISION: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn file_arguments(
        path: &str,
        revision: &str,
        line_range: Option<Value>,
    ) -> RepositoryReadFileArguments {
        serde_json::from_value(json!({
            "line_range": line_range,
            "path": path,
            "repository": "owner/repository",
            "revision": revision,
        }))
        .expect("fixture file arguments are admitted")
    }

    fn directory_arguments(path: &str) -> RepositoryListDirectoryArguments {
        serde_json::from_value(json!({
            "path": path,
            "repository": "owner/repository",
            "revision": REVISION,
        }))
        .expect("fixture directory arguments are admitted")
    }

    fn oversized_encoded_directory_entries() -> Vec<RepositoryDirectoryEntry> {
        let escaped_prefix =
            "\u{0001}".repeat(crate::code_host::arguments::MAX_FILE_PATH_BYTES - 3);
        (0..MAX_RESULT_ITEMS)
            .map(|index| {
                RepositoryDirectoryEntry::try_new(
                    format!("{escaped_prefix}{index:03}"),
                    RepositoryObjectKind::File,
                    None,
                )
                .expect("fixture entry path is admitted")
            })
            .collect()
    }

    /// A nonempty ranged result cannot silently omit leading requested lines.
    #[test]
    fn ranged_content_rejects_a_start_after_the_requested_start() {
        const REQUESTED_START: u32 = 2;
        const REQUESTED_END: u32 = 4;
        const RETURNED_START: u32 = 3;
        const RETURNED_END: u32 = 4;
        const RETURNED_LINES: u32 = 2;
        const SOURCE: &str = "first\nsecond\nthird\nfourth\n";
        const RETURNED_CONTENT: &str = "third\nfourth\n";
        let source_bytes = u64::try_from(SOURCE.len()).expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_END, "start": REQUESTED_START})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: u64::try_from(RETURNED_CONTENT.len())
                    .expect("fixture selected size fits u64"),
                start_line: Some(RETURNED_START),
                end_line: Some(RETURNED_END),
                returned_lines: RETURNED_LINES,
                last_line_complete: true,
                content: String::from(RETURNED_CONTENT),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );

        assert!(result.is_none());
    }

    /// A complete first-line selection without a newline contains every source byte.
    #[test]
    fn ranged_content_rejects_an_incomplete_first_line_marked_complete() {
        const SOURCE: &str = "partial and omitted";
        const RETURNED_CONTENT: &str = "partial";
        const REQUESTED_LINE: u32 = 1;
        let source_bytes = u64::try_from(SOURCE.len()).expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_LINE, "start": REQUESTED_LINE})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: u64::try_from(RETURNED_CONTENT.len())
                    .expect("fixture selected size fits u64"),
                start_line: Some(REQUESTED_LINE),
                end_line: Some(REQUESTED_LINE),
                returned_lines: 1,
                last_line_complete: true,
                content: String::from(RETURNED_CONTENT),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );

        assert!(result.is_none());
    }

    /// A complete first-line range cannot omit a requested second line whose
    /// existence is proven by exact source bytes.
    #[test]
    fn ranged_content_rejects_a_provably_present_omitted_line() {
        const SOURCE: &str = "a\nb";
        const RETURNED_CONTENT: &str = "a\n";
        const REQUESTED_START: u32 = 1;
        const REQUESTED_END: u32 = 2;
        let source_bytes = u64::try_from(SOURCE.len()).expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_END, "start": REQUESTED_START})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: u64::try_from(RETURNED_CONTENT.len())
                    .expect("fixture selected size fits u64"),
                start_line: Some(REQUESTED_START),
                end_line: Some(REQUESTED_START),
                returned_lines: 1,
                last_line_complete: true,
                content: String::from(RETURNED_CONTENT),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );

        assert!(result.is_none());
    }

    /// Bytes before a selected range cannot impersonate truncation within that
    /// range when every selected byte was retained.
    #[test]
    fn ranged_content_rejects_truncation_not_observed_inside_selection() {
        const REQUESTED_START: u32 = 2;
        const REQUESTED_END: u32 = 3;
        let content = format!("{}\n", "x".repeat(MAX_REPOSITORY_FILE_CONTENT_BYTES - 1));
        let returned_bytes = u64::try_from(content.len()).expect("fixture content size fits u64");
        let source_bytes = returned_bytes
            .checked_add(2)
            .expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_END, "start": REQUESTED_START})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: returned_bytes,
                start_line: Some(REQUESTED_START),
                end_line: Some(REQUESTED_START),
                returned_lines: 1,
                last_line_complete: true,
                content,
                completeness: CodeHostResultCompleteness::Truncated,
            },
        );

        assert!(result.is_none());
    }

    /// Truncation cannot be claimed after the complete requested range ended
    /// at a line boundary.
    #[test]
    fn ranged_content_rejects_truncation_after_the_requested_range() {
        const REQUESTED_LINE: u32 = 1;
        let content = format!("{}\n", "a".repeat(MAX_REPOSITORY_FILE_CONTENT_BYTES - 1));
        let returned_bytes = u64::try_from(content.len()).expect("fixture content size fits u64");
        let source_bytes = returned_bytes
            .checked_add(1)
            .expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_LINE, "start": REQUESTED_LINE})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(REQUESTED_LINE),
                end_line: Some(REQUESTED_LINE),
                returned_lines: 1,
                last_line_complete: true,
                content,
                completeness: CodeHostResultCompleteness::Truncated,
            },
        );

        assert!(result.is_none());
    }

    /// A later returned line requires at least one source byte per preceding newline.
    #[test]
    fn ranged_content_rejects_impossible_later_line_source_bytes() {
        const CONTENT: &str = "abc\n";
        const REQUESTED_LINE: u32 = 2;
        let source_bytes = u64::try_from(CONTENT.len()).expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_LINE, "start": REQUESTED_LINE})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: u64::try_from(CONTENT.len())
                    .expect("fixture selected size fits u64"),
                start_line: Some(REQUESTED_LINE),
                end_line: Some(REQUESTED_LINE),
                returned_lines: 1,
                last_line_complete: true,
                content: String::from(CONTENT),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );

        assert!(result.is_none());
    }

    /// A selection beginning at the first line cannot be empty when exact source
    /// metadata proves that the blob is nonempty.
    #[test]
    fn ranged_content_rejects_an_empty_first_line_for_a_nonempty_blob() {
        const SOURCE: &str = "first\n";
        const REQUESTED_START: u32 = 1;
        const REQUESTED_END: u32 = 4;
        let source_bytes = u64::try_from(SOURCE.len()).expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_END, "start": REQUESTED_START})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: 0,
                start_line: None,
                end_line: None,
                returned_lines: 0,
                last_line_complete: true,
                content: String::new(),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );

        assert!(result.is_none());
    }

    /// Exact byte metadata alone cannot prove that a later requested line exists.
    #[test]
    fn ranged_content_accepts_an_empty_selection_beyond_the_last_line() {
        const SOURCE: &str = "only line";
        const REQUESTED_START: u32 = 2;
        const REQUESTED_END: u32 = 4;
        let source_bytes = u64::try_from(SOURCE.len()).expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_END, "start": REQUESTED_START})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: 0,
                start_line: None,
                end_line: None,
                returned_lines: 0,
                last_line_complete: true,
                content: String::new(),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );

        assert!(result.is_some());
    }

    /// A custom transport cannot return ranged content from a source whose exact
    /// size exceeds the bounded scan limit.
    #[test]
    fn ranged_content_rejects_a_source_above_the_scan_limit() {
        const CONTENT: &str = "first\n";
        const REQUESTED_START: u32 = 1;
        const REQUESTED_END: u32 = 1;
        let source_bytes = u64::try_from(MAX_REPOSITORY_FILE_SCAN_BYTES + 1)
            .expect("fixture source size fits u64");
        let arguments = file_arguments(
            "src/lib.rs",
            REVISION,
            Some(json!({"end": REQUESTED_END, "start": REQUESTED_START})),
        );
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: u64::try_from(CONTENT.len())
                    .expect("fixture selected size fits u64"),
                start_line: Some(REQUESTED_START),
                end_line: Some(REQUESTED_END),
                returned_lines: 1,
                last_line_complete: true,
                content: String::from(CONTENT),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );

        assert!(result.is_none());
    }

    /// A ranged binary result cannot claim that an oversized blob was inspected.
    #[test]
    fn ranged_binary_rejects_a_source_above_the_scan_limit() {
        const REQUESTED_LINE: u32 = 1;
        let source_bytes = u64::try_from(MAX_REPOSITORY_FILE_SCAN_BYTES + 1)
            .expect("fixture source size fits u64");
        let arguments = file_arguments(
            "assets/image.bin",
            REVISION,
            Some(json!({"end": REQUESTED_LINE, "start": REQUESTED_LINE})),
        );
        let result = RepositoryReadFileResult::try_binary(&arguments, source_bytes);

        assert!(result.is_none());
    }

    /// Binary evidence requires at least one exact source byte.
    #[test]
    fn binary_rejects_an_empty_source() {
        let arguments = file_arguments("assets/image.bin", REVISION, None);
        let result = RepositoryReadFileResult::try_binary(&arguments, 0);

        assert!(result.is_none());
    }

    /// Repository-root identity cannot carry any file-shaped successful outcome.
    #[test]
    fn root_file_read_rejects_file_shaped_outcomes() {
        const CONTENT: &str = "x";
        const REQUESTED_LINE: u32 = 1;
        let arguments = file_arguments(
            ".",
            REVISION,
            Some(json!({"end": REQUESTED_LINE, "start": REQUESTED_LINE})),
        );
        let source_bytes = u64::try_from(CONTENT.len()).expect("fixture source size fits u64");
        let content = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(REQUESTED_LINE),
                end_line: Some(REQUESTED_LINE),
                returned_lines: 1,
                last_line_complete: true,
                content: String::from(CONTENT),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );
        let binary = RepositoryReadFileResult::try_binary(&arguments, source_bytes);
        let oversized_source = u64::try_from(MAX_REPOSITORY_FILE_SCAN_BYTES + 1)
            .expect("fixture source size fits u64");
        let unavailable =
            RepositoryReadFileResult::try_line_range_unavailable(&arguments, oversized_source);

        assert!(content.is_none());
        assert!(binary.is_none());
        assert!(unavailable.is_none());
    }

    /// The repository root can only be reported as the directory kind when a
    /// file read identifies it as a non-file object.
    #[test]
    fn root_file_non_file_outcome_only_accepts_directory_kind() {
        let arguments = file_arguments(".", REVISION, None);
        let directory =
            RepositoryReadFileResult::try_not_a_file(&arguments, RepositoryObjectKind::Directory);
        let file = RepositoryReadFileResult::try_not_a_file(&arguments, RepositoryObjectKind::File);
        let symlink =
            RepositoryReadFileResult::try_not_a_file(&arguments, RepositoryObjectKind::Symlink);
        let submodule =
            RepositoryReadFileResult::try_not_a_file(&arguments, RepositoryObjectKind::Submodule);

        assert!(directory.is_some());
        assert!(file.is_none());
        assert!(symlink.is_none());
        assert!(submodule.is_none());
    }

    /// No non-directory outcome can describe the repository root.
    #[test]
    fn root_directory_rejects_every_non_directory_outcome() {
        let arguments = directory_arguments(".");
        let file = RepositoryListDirectoryResult::try_not_a_directory(
            &arguments,
            RepositoryObjectKind::File,
        );
        let directory = RepositoryListDirectoryResult::try_not_a_directory(
            &arguments,
            RepositoryObjectKind::Directory,
        );
        let symlink = RepositoryListDirectoryResult::try_not_a_directory(
            &arguments,
            RepositoryObjectKind::Symlink,
        );
        let submodule = RepositoryListDirectoryResult::try_not_a_directory(
            &arguments,
            RepositoryObjectKind::Submodule,
        );

        assert!(file.is_none());
        assert!(directory.is_none());
        assert!(symlink.is_none());
        assert!(submodule.is_none());
    }

    /// No recognized exact commit can omit its repository root tree.
    #[test]
    fn root_path_not_found_outcomes_are_unconstructible() {
        let file_arguments = file_arguments(".", REVISION, None);
        let directory_arguments = directory_arguments(".");
        let file = RepositoryReadFileResult::try_path_not_found(&file_arguments);
        let directory = RepositoryListDirectoryResult::try_path_not_found(&directory_arguments);

        assert!(file.is_none());
        assert!(directory.is_none());
    }

    /// A complete whole-file result contains every byte reported by its source
    /// metadata before evidence scrubbing changes the emitted byte count.
    #[test]
    fn complete_whole_file_rejects_a_shorter_content_body() {
        const SOURCE: &str = "partial\nremaining\n";
        const RETURNED_CONTENT: &str = "partial\n";
        let source_bytes = u64::try_from(SOURCE.len()).expect("fixture source size fits u64");
        let arguments = file_arguments("src/lib.rs", REVISION, None);
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(1),
                returned_lines: 1,
                last_line_complete: true,
                content: String::from(RETURNED_CONTENT),
                completeness: CodeHostResultCompleteness::Complete,
            },
        );

        assert!(result.is_none());
    }

    /// A typed listing cannot attribute an entry from another directory to the
    /// requested directory.
    #[test]
    fn directory_entries_reject_a_path_outside_the_requested_directory() {
        let entry = RepositoryDirectoryEntry::try_new(
            String::from("other/file.rs"),
            RepositoryObjectKind::File,
            None,
        )
        .expect("fixture entry path is admitted");
        let entries = vec![entry];
        let observed_entries = entries.len();
        let arguments = directory_arguments("src");
        let result = RepositoryListDirectoryResult::try_entries(
            &arguments,
            entries,
            observed_entries,
            CodeHostResultCompleteness::Complete,
        );

        assert!(result.is_none());
    }

    /// A typed directory result cannot claim more entries than one contents response.
    #[test]
    fn directory_entries_reject_an_observation_above_the_endpoint_ceiling() {
        let arguments = directory_arguments("src");
        let result = RepositoryListDirectoryResult::try_entries(
            &arguments,
            Vec::new(),
            MAX_OBSERVED_DIRECTORY_ENTRIES + 1,
            CodeHostResultCompleteness::Truncated,
        );

        assert!(result.is_none());
    }

    /// A directory listing cannot contain the same canonical child identity twice.
    #[test]
    fn directory_entries_reject_duplicate_paths() {
        let entry = RepositoryDirectoryEntry::try_new(
            String::from("src/file.rs"),
            RepositoryObjectKind::File,
            None,
        )
        .expect("fixture entry path is admitted");
        let entries = vec![entry.clone(), entry];
        let observed_entries = entries.len();
        let arguments = directory_arguments("src");
        let result = RepositoryListDirectoryResult::try_entries(
            &arguments,
            entries,
            observed_entries,
            CodeHostResultCompleteness::Complete,
        );

        assert!(result.is_none());
    }

    /// The generated oversized fixture passes every constructor invariant that
    /// precedes the encoded-result budget.
    #[test]
    fn oversized_encoded_directory_fixture_passes_pre_encoding_invariants() {
        let entries = oversized_encoded_directory_entries();
        let observed_entries = entries.len();
        let arguments = directory_arguments(".");
        let result = RepositoryListDirectoryResult::try_entries_candidate(
            &arguments,
            entries,
            observed_entries,
            CodeHostResultCompleteness::Complete,
        );

        assert!(result.is_some());
    }

    /// A typed directory result cannot exceed the encoded tool-result bound.
    #[test]
    fn directory_entries_reject_an_oversized_encoded_result() {
        let entries = oversized_encoded_directory_entries();
        let observed_entries = entries.len();
        let arguments = directory_arguments(".");
        let result = RepositoryListDirectoryResult::try_entries(
            &arguments,
            entries,
            observed_entries,
            CodeHostResultCompleteness::Complete,
        );

        assert!(result.is_none());
    }

    /// The repository root cannot appear as its own immediate child.
    #[test]
    fn root_directory_entries_reject_the_root_identity() {
        let entry = RepositoryDirectoryEntry::try_new(
            String::from("."),
            RepositoryObjectKind::Directory,
            None,
        )
        .expect("fixture root path is admitted");
        let entries = vec![entry];
        let observed_entries = entries.len();
        let arguments = directory_arguments(".");
        let result = RepositoryListDirectoryResult::try_entries(
            &arguments,
            entries,
            observed_entries,
            CodeHostResultCompleteness::Complete,
        );

        assert!(result.is_none());
    }

    /// A result constructed for another path and revision cannot satisfy the
    /// exact typed operation that the executor dispatched.
    #[test]
    fn file_result_rejects_another_request_identity() {
        let requested = file_arguments("src/a.rs", REVISION, None);
        let returned = file_arguments("src/b.rs", OTHER_REVISION, None);
        let result = RepositoryReadFileResult::try_path_not_found(&returned)
            .expect("fixture non-root path admits absence");

        assert!(!result.matches(&requested));
    }

    /// Directory-entry constructors share canonical path admission with tool
    /// arguments rather than accepting dot aliases through a weaker predicate.
    #[test]
    fn directory_entry_rejects_a_noncanonical_path() {
        let result = RepositoryDirectoryEntry::try_new(
            String::from("./src/lib.rs"),
            RepositoryObjectKind::File,
            None,
        );

        assert!(result.is_none());
    }

    /// Truncation cannot be claimed before the retained prefix reaches its bound.
    #[test]
    fn truncated_content_rejects_a_short_retained_prefix() {
        const CONTENT: &str = "x";
        let arguments = file_arguments("src/lib.rs", REVISION, None);
        let source_bytes = u64::try_from(CONTENT.len() + 1).expect("fixture source size fits u64");
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(1),
                returned_lines: 1,
                last_line_complete: false,
                content: String::from(CONTENT),
                completeness: CodeHostResultCompleteness::Truncated,
            },
        );

        assert!(result.is_none());
    }

    /// UTF-8 boundary repair may discard at most three bytes from a capped prefix.
    #[test]
    fn truncated_content_accepts_the_utf8_boundary_allowance() {
        let content =
            "x".repeat(MAX_REPOSITORY_FILE_CONTENT_BYTES - MAX_UTF8_BOUNDARY_DISCARD_BYTES);
        let source_bytes = u64::try_from(MAX_REPOSITORY_FILE_CONTENT_BYTES + 1)
            .expect("fixture source size fits u64");
        let arguments = file_arguments("src/lib.rs", REVISION, None);
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(1),
                returned_lines: 1,
                last_line_complete: false,
                content,
                completeness: CodeHostResultCompleteness::Truncated,
            },
        );

        assert!(result.is_some());
    }

    /// A truncated result must prove that its exact source contains bytes beyond
    /// the retained prefix.
    #[test]
    fn truncated_content_rejects_equal_source_and_returned_bytes() {
        const CONTENT: &str = "partial";
        let arguments = file_arguments("src/lib.rs", REVISION, None);
        let source_bytes = u64::try_from(CONTENT.len()).expect("fixture source size fits u64");
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(1),
                returned_lines: 1,
                last_line_complete: false,
                content: String::from(CONTENT),
                completeness: CodeHostResultCompleteness::Truncated,
            },
        );

        assert!(result.is_none());
    }

    /// A truncated prefix ending inside a line cannot claim its final line is
    /// complete.
    #[test]
    fn truncated_content_rejects_complete_final_line_metadata_mid_line() {
        const CONTENT: &str = "partial";
        let arguments = file_arguments("src/lib.rs", REVISION, None);
        let source_bytes = u64::try_from(CONTENT.len() + 1).expect("fixture source size fits u64");
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(1),
                returned_lines: 1,
                last_line_complete: true,
                content: String::from(CONTENT),
                completeness: CodeHostResultCompleteness::Truncated,
            },
        );

        assert!(result.is_none());
    }

    /// A truncated prefix ending at a newline cannot claim its final line is
    /// incomplete.
    #[test]
    fn truncated_content_rejects_incomplete_final_line_metadata_at_newline() {
        const CONTENT: &str = "complete line\n";
        let arguments = file_arguments("src/lib.rs", REVISION, None);
        let source_bytes = u64::try_from(CONTENT.len() + 1).expect("fixture source size fits u64");
        let result = RepositoryReadFileResult::try_content(
            &arguments,
            RepositoryFileContentFields {
                source_bytes,
                selected_source_bytes: source_bytes,
                start_line: Some(1),
                end_line: Some(1),
                returned_lines: 1,
                last_line_complete: false,
                content: String::from(CONTENT),
                completeness: CodeHostResultCompleteness::Truncated,
            },
        );

        assert!(result.is_none());
    }
}

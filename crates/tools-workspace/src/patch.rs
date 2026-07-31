use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    path::{Component, Path},
    str::FromStr,
};

/// Maximum accepted UTF-8 patch size.
pub const MAX_PATCH_BYTES: usize = 1024 * 1024;
/// Maximum file operations accepted in one patch.
pub const MAX_PATCH_OPERATIONS: usize = 64;
/// Maximum update hunks accepted across one patch.
pub const MAX_PATCH_HUNKS: usize = 256;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const HUNK_MARKER: &str = "@@";

/// One parsed, bounded workspace patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePatch {
    operations: Vec<PatchOperation>,
}

impl WorkspacePatch {
    /// Parses the structured patch format.
    pub fn parse(input: &str) -> Result<Self, PatchParseError> {
        parse_patch(input)
    }

    /// Returns file operations in patch order.
    pub fn operations(&self) -> &[PatchOperation] {
        &self.operations
    }
}

impl FromStr for WorkspacePatch {
    type Err = PatchParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        parse_patch(input)
    }
}

/// One file operation in a parsed patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchOperation {
    /// Creates one file from `+` lines.
    Add {
        /// Normalized relative workspace path.
        path: String,
        /// Complete UTF-8 file content.
        content: String,
    },
    /// Rewrites one existing file through uniquely matched hunks.
    Update {
        /// Normalized relative workspace path.
        path: String,
        /// Ordered update hunks.
        hunks: Vec<PatchHunk>,
    },
    /// Deletes one existing file.
    Delete {
        /// Normalized relative workspace path.
        path: String,
    },
}

impl PatchOperation {
    /// Returns the normalized relative workspace path touched by the operation.
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Update { path, .. } | Self::Delete { path } => path,
        }
    }
}

/// One update hunk represented as exact old and replacement text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchHunk {
    before: String,
    after: String,
}

impl PatchHunk {
    /// Returns the exact source text this hunk must uniquely match.
    pub fn before(&self) -> &str {
        &self.before
    }

    /// Returns the replacement text produced by this hunk.
    pub fn after(&self) -> &str {
        &self.after
    }
}

/// One-based source location in a patch parse failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PatchLocation {
    /// One-based input line.
    pub line: usize,
    /// One-based file operation, when an operation header was admitted.
    pub operation: Option<usize>,
    /// One-based hunk within that operation, when a hunk marker was admitted.
    pub hunk: Option<usize>,
}

/// Boundary expected when a patch ended early.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpectedPatchSyntax {
    /// The opening patch boundary.
    BeginPatch,
    /// At least one file operation.
    FileOperation,
    /// At least one `+` content line.
    AddLine,
    /// At least one `@@` hunk.
    UpdateHunk,
    /// At least one hunk body line.
    HunkLine,
    /// The closing patch boundary.
    EndPatch,
}

/// Why otherwise line-oriented patch input was malformed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedPatchReason {
    /// Content appeared where an operation header was required.
    ExpectedFileOperation,
    /// An unrecognized `***` directive appeared.
    UnknownDirective,
    /// An add-file body contained a line without a `+` prefix.
    InvalidAddLine,
    /// An update body contained content before its first `@@`.
    ExpectedHunkMarker,
    /// A hunk body line had no context, removal, or addition prefix.
    InvalidHunkLine,
    /// A hunk had no source-side context or removal line.
    HunkHasNoSource,
    /// A hunk contained no removal or addition.
    HunkMakesNoChange,
    /// A delete operation contained body content.
    DeleteHasBody,
}

/// Lexical rejection of a path carried by a patch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchPathRejection {
    /// The supplied path was absolute.
    Absolute,
    /// The supplied path contained a parent-directory component.
    ParentTraversal,
    /// The supplied path was empty, contained NUL, or exceeded its bounded shape.
    Invalid,
}

/// Typed reason a structured patch could not be parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchParseErrorKind {
    /// The patch exceeded [`MAX_PATCH_BYTES`].
    TooLarge {
        /// Observed UTF-8 byte length.
        actual_bytes: usize,
    },
    /// Input ended before required syntax appeared.
    Truncated {
        /// Syntax required at the truncation point.
        expected: ExpectedPatchSyntax,
    },
    /// Input violated the line-oriented grammar.
    Malformed {
        /// Specific malformed shape.
        reason: MalformedPatchReason,
    },
    /// The patch exceeded [`MAX_PATCH_OPERATIONS`].
    TooManyOperations,
    /// The patch exceeded [`MAX_PATCH_HUNKS`].
    TooManyHunks,
    /// One operation path failed lexical validation.
    PathRejected {
        /// Original path text from the operation header.
        path: String,
        /// Typed lexical rejection.
        reason: PatchPathRejection,
    },
    /// Two operations touch equal or ancestor/descendant paths.
    PathOverlap {
        /// Earlier one-based operation.
        first_operation: usize,
        /// Earlier normalized path.
        first_path: String,
        /// Later normalized path.
        path: String,
    },
}

/// Parse error with typed reason and operation/hunk coordinates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchParseError {
    /// One-based position at which parsing failed.
    pub location: PatchLocation,
    /// Typed failure reason.
    pub kind: PatchParseErrorKind,
}

impl fmt::Display for PatchParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "patch parse failed at line {}",
            self.location.line
        )?;
        if let Some(operation) = self.location.operation {
            write!(formatter, ", operation {operation}")?;
        }
        if let Some(hunk) = self.location.hunk {
            write!(formatter, ", hunk {hunk}")?;
        }
        formatter.write_str(": ")?;
        match &self.kind {
            PatchParseErrorKind::TooLarge { actual_bytes } => write!(
                formatter,
                "patch has {actual_bytes} bytes; maximum is {MAX_PATCH_BYTES}"
            ),
            PatchParseErrorKind::Truncated { expected } => {
                write!(formatter, "truncated before {expected:?}")
            }
            PatchParseErrorKind::Malformed { reason } => write!(formatter, "malformed {reason:?}"),
            PatchParseErrorKind::TooManyOperations => write!(
                formatter,
                "more than {MAX_PATCH_OPERATIONS} file operations"
            ),
            PatchParseErrorKind::TooManyHunks => {
                write!(formatter, "more than {MAX_PATCH_HUNKS} update hunks")
            }
            PatchParseErrorKind::PathRejected { path, reason } => {
                write!(formatter, "path {path:?} rejected: {reason:?}")
            }
            PatchParseErrorKind::PathOverlap {
                first_operation,
                first_path,
                path,
            } => write!(
                formatter,
                "path {path:?} overlaps operation {first_operation} path {first_path:?}"
            ),
        }
    }
}

impl Error for PatchParseError {}

/// Parses the bounded structured patch format.
pub fn parse_patch(input: &str) -> Result<WorkspacePatch, PatchParseError> {
    if input.len() > MAX_PATCH_BYTES {
        return Err(parse_error(
            1,
            None,
            None,
            PatchParseErrorKind::TooLarge {
                actual_bytes: input.len(),
            },
        ));
    }
    let lines: Vec<&str> = input.lines().collect();
    if lines.first().copied() != Some(BEGIN_PATCH) {
        return Err(parse_error(
            1,
            None,
            None,
            PatchParseErrorKind::Truncated {
                expected: ExpectedPatchSyntax::BeginPatch,
            },
        ));
    }

    let mut cursor = 1;
    let mut total_hunks = 0;
    let mut operations = Vec::new();
    while cursor < lines.len() && lines[cursor] != END_PATCH {
        let operation_number = operations.len() + 1;
        if operation_number > MAX_PATCH_OPERATIONS {
            return Err(parse_error(
                cursor + 1,
                Some(operation_number),
                None,
                PatchParseErrorKind::TooManyOperations,
            ));
        }
        let header = lines[cursor];
        let (operation, next_cursor, added_hunks) =
            if let Some(path) = header.strip_prefix(ADD_FILE) {
                parse_add(&lines, cursor, operation_number, path)?
            } else if let Some(path) = header.strip_prefix(UPDATE_FILE) {
                parse_update(&lines, cursor, operation_number, path, total_hunks)?
            } else if let Some(path) = header.strip_prefix(DELETE_FILE) {
                parse_delete(&lines, cursor, operation_number, path)?
            } else {
                let reason = if header.starts_with("***") {
                    MalformedPatchReason::UnknownDirective
                } else {
                    MalformedPatchReason::ExpectedFileOperation
                };
                return Err(parse_error(
                    cursor + 1,
                    Some(operation_number),
                    None,
                    PatchParseErrorKind::Malformed { reason },
                ));
            };
        reject_path_overlap(&operations, operation.path(), operation_number, cursor + 1)?;
        operations.push(operation);
        cursor = next_cursor;
        total_hunks += added_hunks;
    }

    if cursor == lines.len() {
        return Err(parse_error(
            lines.len() + 1,
            None,
            None,
            PatchParseErrorKind::Truncated {
                expected: ExpectedPatchSyntax::EndPatch,
            },
        ));
    }
    if operations.is_empty() {
        return Err(parse_error(
            cursor + 1,
            None,
            None,
            PatchParseErrorKind::Truncated {
                expected: ExpectedPatchSyntax::FileOperation,
            },
        ));
    }
    if cursor + 1 != lines.len() {
        return Err(parse_error(
            cursor + 2,
            None,
            None,
            PatchParseErrorKind::Malformed {
                reason: MalformedPatchReason::ExpectedFileOperation,
            },
        ));
    }
    Ok(WorkspacePatch { operations })
}

fn parse_add(
    lines: &[&str],
    header_index: usize,
    operation_number: usize,
    supplied_path: &str,
) -> Result<(PatchOperation, usize, usize), PatchParseError> {
    let path = checked_path(supplied_path, header_index, operation_number)?;
    let mut cursor = header_index + 1;
    let mut content = String::new();
    while cursor < lines.len() && !is_directive(lines[cursor]) {
        let Some(line) = lines[cursor].strip_prefix('+') else {
            return Err(parse_error(
                cursor + 1,
                Some(operation_number),
                None,
                PatchParseErrorKind::Malformed {
                    reason: MalformedPatchReason::InvalidAddLine,
                },
            ));
        };
        content.push_str(line);
        content.push('\n');
        cursor += 1;
    }
    if content.is_empty() {
        return Err(parse_error(
            cursor + 1,
            Some(operation_number),
            None,
            PatchParseErrorKind::Truncated {
                expected: ExpectedPatchSyntax::AddLine,
            },
        ));
    }
    Ok((PatchOperation::Add { path, content }, cursor, 0))
}

fn parse_update(
    lines: &[&str],
    header_index: usize,
    operation_number: usize,
    supplied_path: &str,
    prior_hunks: usize,
) -> Result<(PatchOperation, usize, usize), PatchParseError> {
    let path = checked_path(supplied_path, header_index, operation_number)?;
    let mut cursor = header_index + 1;
    if cursor == lines.len() || is_directive(lines[cursor]) {
        return Err(parse_error(
            cursor + 1,
            Some(operation_number),
            None,
            PatchParseErrorKind::Truncated {
                expected: ExpectedPatchSyntax::UpdateHunk,
            },
        ));
    }
    if lines[cursor] != HUNK_MARKER {
        return Err(parse_error(
            cursor + 1,
            Some(operation_number),
            None,
            PatchParseErrorKind::Malformed {
                reason: MalformedPatchReason::ExpectedHunkMarker,
            },
        ));
    }

    let mut hunks = Vec::new();
    while cursor < lines.len() && lines[cursor] == HUNK_MARKER {
        let hunk_number = hunks.len() + 1;
        if prior_hunks + hunk_number > MAX_PATCH_HUNKS {
            return Err(parse_error(
                cursor + 1,
                Some(operation_number),
                Some(hunk_number),
                PatchParseErrorKind::TooManyHunks,
            ));
        }
        cursor += 1;
        let (hunk, next_cursor) = parse_hunk(lines, cursor, operation_number, hunk_number)?;
        hunks.push(hunk);
        cursor = next_cursor;
    }
    let added_hunks = hunks.len();
    Ok((PatchOperation::Update { path, hunks }, cursor, added_hunks))
}

fn parse_hunk(
    lines: &[&str],
    mut cursor: usize,
    operation_number: usize,
    hunk_number: usize,
) -> Result<(PatchHunk, usize), PatchParseError> {
    let body_start = cursor;
    let mut before = String::new();
    let mut after = String::new();
    let mut has_source = false;
    let mut has_change = false;
    while cursor < lines.len() && lines[cursor] != HUNK_MARKER && !is_directive(lines[cursor]) {
        let line = lines[cursor];
        if let Some(context) = line.strip_prefix(' ') {
            before.push_str(context);
            before.push('\n');
            after.push_str(context);
            after.push('\n');
            has_source = true;
        } else if let Some(removal) = line.strip_prefix('-') {
            before.push_str(removal);
            before.push('\n');
            has_source = true;
            has_change = true;
        } else if let Some(addition) = line.strip_prefix('+') {
            after.push_str(addition);
            after.push('\n');
            has_change = true;
        } else {
            return Err(parse_error(
                cursor + 1,
                Some(operation_number),
                Some(hunk_number),
                PatchParseErrorKind::Malformed {
                    reason: MalformedPatchReason::InvalidHunkLine,
                },
            ));
        }
        cursor += 1;
    }
    if cursor == body_start {
        return Err(parse_error(
            cursor + 1,
            Some(operation_number),
            Some(hunk_number),
            PatchParseErrorKind::Truncated {
                expected: ExpectedPatchSyntax::HunkLine,
            },
        ));
    }
    if !has_source {
        return Err(parse_error(
            body_start + 1,
            Some(operation_number),
            Some(hunk_number),
            PatchParseErrorKind::Malformed {
                reason: MalformedPatchReason::HunkHasNoSource,
            },
        ));
    }
    if !has_change {
        return Err(parse_error(
            body_start + 1,
            Some(operation_number),
            Some(hunk_number),
            PatchParseErrorKind::Malformed {
                reason: MalformedPatchReason::HunkMakesNoChange,
            },
        ));
    }
    Ok((PatchHunk { before, after }, cursor))
}

fn parse_delete(
    lines: &[&str],
    header_index: usize,
    operation_number: usize,
    supplied_path: &str,
) -> Result<(PatchOperation, usize, usize), PatchParseError> {
    let path = checked_path(supplied_path, header_index, operation_number)?;
    let cursor = header_index + 1;
    if cursor < lines.len() && !is_directive(lines[cursor]) {
        return Err(parse_error(
            cursor + 1,
            Some(operation_number),
            None,
            PatchParseErrorKind::Malformed {
                reason: MalformedPatchReason::DeleteHasBody,
            },
        ));
    }
    Ok((PatchOperation::Delete { path }, cursor, 0))
}

fn checked_path(
    supplied: &str,
    header_index: usize,
    operation_number: usize,
) -> Result<String, PatchParseError> {
    normalize_path(supplied).map_err(|reason| {
        parse_error(
            header_index + 1,
            Some(operation_number),
            None,
            PatchParseErrorKind::PathRejected {
                path: String::from(supplied),
                reason,
            },
        )
    })
}

fn parse_error(
    line: usize,
    operation: Option<usize>,
    hunk: Option<usize>,
    kind: PatchParseErrorKind,
) -> PatchParseError {
    PatchParseError {
        location: PatchLocation {
            line,
            operation,
            hunk,
        },
        kind,
    }
}

fn is_directive(line: &str) -> bool {
    line.starts_with("***")
}

fn normalize_path(supplied: &str) -> Result<String, PatchPathRejection> {
    if supplied.is_empty()
        || supplied.chars().count() > crate::path::MAX_WORKSPACE_PATH_CHARACTERS
        || supplied.len() > crate::path::MAX_WORKSPACE_PATH_BYTES
        || supplied.contains('\0')
    {
        return Err(PatchPathRejection::Invalid);
    }
    let path = Path::new(supplied);
    if path.is_absolute() {
        return Err(PatchPathRejection::Absolute);
    }
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part.to_string_lossy()),
            Component::CurDir => {}
            Component::ParentDir => return Err(PatchPathRejection::ParentTraversal),
            Component::RootDir | Component::Prefix(_) => return Err(PatchPathRejection::Absolute),
        }
    }
    if normalized.is_empty() {
        return Err(PatchPathRejection::Invalid);
    }
    Ok(normalized.join("/"))
}

fn reject_path_overlap(
    operations: &[PatchOperation],
    path: &str,
    operation_number: usize,
    line: usize,
) -> Result<(), PatchParseError> {
    let overlapping = operations
        .iter()
        .enumerate()
        .find(|(_, existing)| paths_overlap(existing.path(), path));
    if let Some((index, existing)) = overlapping {
        return Err(parse_error(
            line,
            Some(operation_number),
            None,
            PatchParseErrorKind::PathOverlap {
                first_operation: index + 1,
                first_path: String::from(existing.path()),
                path: String::from(path),
            },
        ));
    }
    Ok(())
}

fn paths_overlap(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left == right || left.starts_with(right) || right.starts_with(left)
}

/// One fully prevalidated patch plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchPlan {
    operations: Vec<PlannedPatchOperation>,
}

impl PatchPlan {
    /// Returns the prevalidated operations in patch order.
    pub fn operations(&self) -> &[PlannedPatchOperation] {
        &self.operations
    }
}

/// One file result in a prevalidated patch plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedPatchOperation {
    /// Create a file with complete content.
    Add {
        /// Normalized relative workspace path.
        path: String,
        /// Complete resulting content.
        content: String,
    },
    /// Replace a file's complete content.
    Update {
        /// Normalized relative workspace path.
        path: String,
        /// Complete resulting content.
        content: String,
    },
    /// Delete a file.
    Delete {
        /// Normalized relative workspace path.
        path: String,
    },
}

impl PlannedPatchOperation {
    /// Returns the normalized relative workspace path touched by this result.
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Update { path, .. } | Self::Delete { path } => path,
        }
    }
}

/// Typed reason an otherwise valid patch could not apply to source content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchApplyErrorKind {
    /// An add operation targeted an existing file.
    AddTargetExists,
    /// An update or delete operation targeted a missing file.
    SourceMissing,
    /// A hunk's source text did not occur.
    ContextNotFound,
    /// A hunk's source text occurred more than once.
    ContextAmbiguous {
        /// Number of occurrences observed.
        matches: usize,
    },
    /// A hunk's source range intersects an earlier hunk.
    OverlappingHunks {
        /// Earlier one-based hunk whose source range intersects.
        first_hunk: usize,
    },
}

/// Apply failure with a normalized path and operation/hunk coordinates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchApplyError {
    /// One-based file operation.
    pub operation: usize,
    /// One-based update hunk, for hunk failures.
    pub hunk: Option<usize>,
    /// Normalized relative path.
    pub path: String,
    /// Typed failure reason.
    pub kind: PatchApplyErrorKind,
}

impl fmt::Display for PatchApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "patch apply failed at operation {}, path {:?}",
            self.operation, self.path
        )?;
        if let Some(hunk) = self.hunk {
            write!(formatter, ", hunk {hunk}")?;
        }
        write!(formatter, ": {:?}", self.kind)
    }
}

impl Error for PatchApplyError {}

/// Prevalidates every file operation and produces complete resulting contents.
///
/// The input map is not changed. Callers may inspect this plan before making
/// filesystem changes.
pub fn plan_patch(
    patch: &WorkspacePatch,
    contents: &BTreeMap<String, String>,
) -> Result<PatchPlan, PatchApplyError> {
    let operations = patch
        .operations
        .iter()
        .enumerate()
        .map(|(index, operation)| plan_operation(operation, index + 1, contents))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PatchPlan { operations })
}

/// Applies a patch atomically to an in-memory file map.
///
/// Every operation and hunk is prevalidated before the first map mutation. On
/// error, `contents` is unchanged.
pub fn apply_patch_to_contents(
    patch: &WorkspacePatch,
    contents: &mut BTreeMap<String, String>,
) -> Result<PatchPlan, PatchApplyError> {
    let plan = plan_patch(patch, contents)?;
    for operation in &plan.operations {
        match operation {
            PlannedPatchOperation::Add { path, content }
            | PlannedPatchOperation::Update { path, content } => {
                contents.insert(path.clone(), content.clone());
            }
            PlannedPatchOperation::Delete { path } => {
                contents.remove(path);
            }
        }
    }
    Ok(plan)
}

fn plan_operation(
    operation: &PatchOperation,
    operation_number: usize,
    contents: &BTreeMap<String, String>,
) -> Result<PlannedPatchOperation, PatchApplyError> {
    match operation {
        PatchOperation::Add { path, content } => {
            if contents.contains_key(path) {
                return Err(apply_error(
                    operation_number,
                    None,
                    path,
                    PatchApplyErrorKind::AddTargetExists,
                ));
            }
            Ok(PlannedPatchOperation::Add {
                path: path.clone(),
                content: content.clone(),
            })
        }
        PatchOperation::Update { path, hunks } => {
            let source = contents.get(path).ok_or_else(|| {
                apply_error(
                    operation_number,
                    None,
                    path,
                    PatchApplyErrorKind::SourceMissing,
                )
            })?;
            let content = apply_hunks(source, hunks, operation_number, path)?;
            Ok(PlannedPatchOperation::Update {
                path: path.clone(),
                content,
            })
        }
        PatchOperation::Delete { path } => {
            if !contents.contains_key(path) {
                return Err(apply_error(
                    operation_number,
                    None,
                    path,
                    PatchApplyErrorKind::SourceMissing,
                ));
            }
            Ok(PlannedPatchOperation::Delete { path: path.clone() })
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocatedHunk {
    hunk: usize,
    start: usize,
    end: usize,
    replacement: String,
}

fn apply_hunks(
    source: &str,
    hunks: &[PatchHunk],
    operation_number: usize,
    path: &str,
) -> Result<String, PatchApplyError> {
    let mut located = hunks
        .iter()
        .enumerate()
        .map(|(index, hunk)| locate_hunk(source, hunk, operation_number, index + 1, path))
        .collect::<Result<Vec<_>, _>>()?;
    reject_overlapping_hunks(&located, operation_number, path)?;
    located.sort_by_key(|hunk| std::cmp::Reverse(hunk.start));
    let mut result = String::from(source);
    for hunk in located {
        result.replace_range(hunk.start..hunk.end, &hunk.replacement);
    }
    Ok(result)
}

fn locate_hunk(
    source: &str,
    hunk: &PatchHunk,
    operation_number: usize,
    hunk_number: usize,
    path: &str,
) -> Result<LocatedHunk, PatchApplyError> {
    let starts = overlapping_match_starts(source, &hunk.before)
        .into_iter()
        .filter(|start| *start == 0 || source.as_bytes()[start - 1] == b'\n')
        .collect::<Vec<_>>();
    let start = match starts.as_slice() {
        [] => {
            return Err(apply_error(
                operation_number,
                Some(hunk_number),
                path,
                PatchApplyErrorKind::ContextNotFound,
            ));
        }
        [only] => *only,
        multiple => {
            return Err(apply_error(
                operation_number,
                Some(hunk_number),
                path,
                PatchApplyErrorKind::ContextAmbiguous {
                    matches: multiple.len(),
                },
            ));
        }
    };
    Ok(LocatedHunk {
        hunk: hunk_number,
        start,
        end: start + hunk.before.len(),
        replacement: hunk.after.clone(),
    })
}

pub(crate) fn overlapping_match_starts(source: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut starts = Vec::new();
    let mut next = 0;
    while next <= source.len() {
        let Some(relative) = source[next..].find(needle) else {
            break;
        };
        let start = next + relative;
        starts.push(start);
        next = start + 1;
        while !source.is_char_boundary(next) {
            next += 1;
        }
    }
    starts
}

fn reject_overlapping_hunks(
    located: &[LocatedHunk],
    operation_number: usize,
    path: &str,
) -> Result<(), PatchApplyError> {
    for (index, current) in located.iter().enumerate() {
        if let Some(previous) = located[..index]
            .iter()
            .find(|previous| current.start < previous.end && previous.start < current.end)
        {
            return Err(apply_error(
                operation_number,
                Some(current.hunk),
                path,
                PatchApplyErrorKind::OverlappingHunks {
                    first_hunk: previous.hunk,
                },
            ));
        }
    }
    Ok(())
}

fn apply_error(
    operation: usize,
    hunk: Option<usize>,
    path: &str,
    kind: PatchApplyErrorKind,
) -> PatchApplyError {
    PatchApplyError {
        operation,
        hunk,
        path: String::from(path),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_add_update_delete_operations() {
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Add File: new.txt\n\
             +new\n\
             *** Update File: kept.txt\n\
             @@\n\
             -before\n\
             +after\n\
             *** Delete File: gone.txt\n\
             *** End Patch",
        )
        .expect("valid patch parses");
        let mut contents = BTreeMap::from([
            (String::from("kept.txt"), String::from("before\n")),
            (String::from("gone.txt"), String::from("old\n")),
        ]);

        let plan =
            apply_patch_to_contents(&patch, &mut contents).expect("valid patch applies atomically");

        assert_eq!(
            contents,
            BTreeMap::from([
                (String::from("kept.txt"), String::from("after\n")),
                (String::from("new.txt"), String::from("new\n")),
            ])
        );
        assert_eq!(plan.operations().len(), 3);
    }

    #[test]
    fn missing_end_boundary_is_typed_truncation() {
        let error = parse_patch("*** Begin Patch\n*** Add File: new.txt\n+new")
            .expect_err("missing end boundary rejects");

        assert_eq!(
            error,
            PatchParseError {
                location: PatchLocation {
                    line: 4,
                    operation: None,
                    hunk: None,
                },
                kind: PatchParseErrorKind::Truncated {
                    expected: ExpectedPatchSyntax::EndPatch,
                },
            }
        );
    }

    #[test]
    fn add_line_without_plus_is_typed_malformed_input() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Add File: new.txt\n\
             content\n\
             *** End Patch",
        )
        .expect_err("unmarked add content rejects");

        assert_eq!(
            error,
            PatchParseError {
                location: PatchLocation {
                    line: 3,
                    operation: Some(1),
                    hunk: None,
                },
                kind: PatchParseErrorKind::Malformed {
                    reason: MalformedPatchReason::InvalidAddLine,
                },
            }
        );
    }

    #[test]
    fn update_without_hunk_is_typed_truncation() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Update File: file.txt\n\
             *** End Patch",
        )
        .expect_err("update without hunk rejects");

        assert_eq!(
            error,
            PatchParseError {
                location: PatchLocation {
                    line: 3,
                    operation: Some(1),
                    hunk: None,
                },
                kind: PatchParseErrorKind::Truncated {
                    expected: ExpectedPatchSyntax::UpdateHunk,
                },
            }
        );
    }

    #[test]
    fn hunk_without_source_is_typed_malformed_input() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Update File: file.txt\n\
             @@\n\
             +only-addition\n\
             *** End Patch",
        )
        .expect_err("source-free hunk rejects");

        assert_eq!(error.location.operation, Some(1));
        assert_eq!(error.location.hunk, Some(1));
        assert_eq!(
            error.kind,
            PatchParseErrorKind::Malformed {
                reason: MalformedPatchReason::HunkHasNoSource,
            }
        );
    }

    #[test]
    fn absolute_operation_path_has_typed_rejection() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Delete File: /outside.txt\n\
             *** End Patch",
        )
        .expect_err("absolute path rejects");

        assert_eq!(
            error.kind,
            PatchParseErrorKind::PathRejected {
                path: String::from("/outside.txt"),
                reason: PatchPathRejection::Absolute,
            }
        );
    }

    #[test]
    fn parent_traversal_operation_path_has_typed_rejection() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Delete File: src/../../outside.txt\n\
             *** End Patch",
        )
        .expect_err("parent traversal rejects");

        assert_eq!(
            error.kind,
            PatchParseErrorKind::PathRejected {
                path: String::from("src/../../outside.txt"),
                reason: PatchPathRejection::ParentTraversal,
            }
        );
    }

    #[test]
    fn multibyte_operation_path_within_character_bound_parses() {
        const CHARACTER: &str = "é";
        const CHARACTER_COUNT: usize = 3_000;

        let path = CHARACTER.repeat(CHARACTER_COUNT);
        let patch = format!("*** Begin Patch\n*** Delete File: {path}\n*** End Patch");

        assert!(path.len() > crate::path::MAX_WORKSPACE_PATH_CHARACTERS);

        let parsed = parse_patch(&patch).expect("character-bounded multibyte path parses");

        assert_eq!(parsed.operations()[0].path(), path);
    }

    #[test]
    fn operation_path_over_character_bound_has_typed_rejection() {
        const CHARACTER: &str = "x";

        let path = CHARACTER.repeat(crate::path::MAX_WORKSPACE_PATH_CHARACTERS + 1);
        let patch = format!("*** Begin Patch\n*** Delete File: {path}\n*** End Patch");
        let error = parse_patch(&patch).expect_err("over-bound path rejects");

        assert_eq!(
            error.kind,
            PatchParseErrorKind::PathRejected {
                path,
                reason: PatchPathRejection::Invalid,
            }
        );
    }

    #[test]
    fn normalized_duplicate_path_has_typed_overlap() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Add File: src/./file.txt\n\
             +new\n\
             *** Delete File: src/file.txt\n\
             *** End Patch",
        )
        .expect_err("duplicate normalized path rejects");

        assert_eq!(
            error.kind,
            PatchParseErrorKind::PathOverlap {
                first_operation: 1,
                first_path: String::from("src/file.txt"),
                path: String::from("src/file.txt"),
            }
        );
    }

    #[test]
    fn ancestor_and_descendant_paths_have_typed_overlap() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Add File: node\n\
             +new\n\
             *** Add File: node/child.txt\n\
             +child\n\
             *** End Patch",
        )
        .expect_err("ancestor and descendant path touches reject");

        assert_eq!(
            error.kind,
            PatchParseErrorKind::PathOverlap {
                first_operation: 1,
                first_path: String::from("node"),
                path: String::from("node/child.txt"),
            }
        );
    }

    #[test]
    fn context_not_found_identifies_operation_and_hunk() {
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Update File: file.txt\n\
             @@\n\
             -missing\n\
             +replacement\n\
             *** End Patch",
        )
        .expect("structured patch parses");
        let contents = BTreeMap::from([(String::from("file.txt"), String::from("present\n"))]);

        let error = plan_patch(&patch, &contents).expect_err("missing context rejects");

        assert_eq!(
            error,
            PatchApplyError {
                operation: 1,
                hunk: Some(1),
                path: String::from("file.txt"),
                kind: PatchApplyErrorKind::ContextNotFound,
            }
        );
    }

    #[test]
    fn context_matching_twice_is_typed_ambiguity() {
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Update File: file.txt\n\
             @@\n\
             -same\n\
             +replacement\n\
             *** End Patch",
        )
        .expect("structured patch parses");
        let contents = BTreeMap::from([(
            String::from("file.txt"),
            String::from("same\nmiddle\nsame\n"),
        )]);

        let error = plan_patch(&patch, &contents).expect_err("ambiguous context rejects");

        assert_eq!(
            error,
            PatchApplyError {
                operation: 1,
                hunk: Some(1),
                path: String::from("file.txt"),
                kind: PatchApplyErrorKind::ContextAmbiguous { matches: 2 },
            }
        );
    }

    #[test]
    fn self_overlapping_context_is_typed_ambiguity() {
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Update File: file.txt\n\
             @@\n\
             \x20a\n\
             -a\n\
             +b\n\
             *** End Patch",
        )
        .expect("structured patch parses");
        let contents = BTreeMap::from([(String::from("file.txt"), String::from("a\na\na\n"))]);

        let error = plan_patch(&patch, &contents).expect_err("overlapping context rejects");

        assert_eq!(
            error,
            PatchApplyError {
                operation: 1,
                hunk: Some(1),
                path: String::from("file.txt"),
                kind: PatchApplyErrorKind::ContextAmbiguous { matches: 2 },
            }
        );
    }

    #[test]
    fn intersecting_source_ranges_are_typed_hunk_overlap() {
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Update File: file.txt\n\
             @@\n\
             \x20alpha\n\
             -beta\n\
             +first\n\
             @@\n\
             -beta\n\
             \x20gamma\n\
             +second\n\
             *** End Patch",
        )
        .expect("structured patch parses");
        let contents = BTreeMap::from([(
            String::from("file.txt"),
            String::from("alpha\nbeta\ngamma\n"),
        )]);

        let error = plan_patch(&patch, &contents).expect_err("overlapping hunks reject");

        assert_eq!(
            error,
            PatchApplyError {
                operation: 1,
                hunk: Some(2),
                path: String::from("file.txt"),
                kind: PatchApplyErrorKind::OverlappingHunks { first_hunk: 1 },
            }
        );
    }

    #[test]
    fn later_prevalidation_failure_leaves_every_file_unchanged() {
        let patch = parse_patch(
            "*** Begin Patch\n\
             *** Update File: first.txt\n\
             @@\n\
             -old\n\
             +new\n\
             *** Update File: second.txt\n\
             @@\n\
             -absent\n\
             +replacement\n\
             *** End Patch",
        )
        .expect("structured patch parses");
        let original = BTreeMap::from([
            (String::from("first.txt"), String::from("old\n")),
            (String::from("second.txt"), String::from("present\n")),
        ]);
        let mut contents = original.clone();

        let error = apply_patch_to_contents(&patch, &mut contents)
            .expect_err("later invalid hunk rejects whole patch");

        assert_eq!(error.operation, 2);
        assert_eq!(error.hunk, Some(1));
        assert_eq!(error.kind, PatchApplyErrorKind::ContextNotFound);
        assert_eq!(contents, original);
    }

    #[test]
    fn patch_byte_bound_has_typed_rejection() {
        let input = format!("{}x", " ".repeat(MAX_PATCH_BYTES));

        let error = parse_patch(&input).expect_err("oversized patch rejects");

        assert_eq!(
            error.kind,
            PatchParseErrorKind::TooLarge {
                actual_bytes: MAX_PATCH_BYTES + 1,
            }
        );
    }

    #[test]
    fn operation_count_bound_has_typed_rejection() {
        let input = patch_with_delete_operations(MAX_PATCH_OPERATIONS + 1);

        let error = parse_patch(&input).expect_err("too many operations reject");

        assert_eq!(error.location.operation, Some(MAX_PATCH_OPERATIONS + 1));
        assert_eq!(error.kind, PatchParseErrorKind::TooManyOperations);
    }

    #[test]
    fn hunk_count_bound_has_typed_rejection() {
        let input = patch_with_hunks(MAX_PATCH_HUNKS + 1);

        let error = parse_patch(&input).expect_err("too many hunks reject");

        assert_eq!(error.location.operation, Some(1));
        assert_eq!(error.location.hunk, Some(MAX_PATCH_HUNKS + 1));
        assert_eq!(error.kind, PatchParseErrorKind::TooManyHunks);
    }

    fn patch_with_delete_operations(count: usize) -> String {
        let operations = (0..count)
            .map(|index| format!("*** Delete File: file-{index}.txt\n"))
            .collect::<String>();
        format!("{BEGIN_PATCH}\n{operations}{END_PATCH}")
    }

    fn patch_with_hunks(count: usize) -> String {
        let hunks = (0..count)
            .map(|index| format!("@@\n-old-{index}\n+new-{index}\n"))
            .collect::<String>();
        format!("{BEGIN_PATCH}\n*** Update File: file.txt\n{hunks}{END_PATCH}")
    }
}

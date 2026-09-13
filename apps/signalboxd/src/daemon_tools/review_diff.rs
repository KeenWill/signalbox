//! Read-only diff hunks from a judgment session's prepared head and base.

use std::{
    io::Read,
    path::{Component, Path},
};

use signalbox_application::{CompiledTool, CompiledToolCatalog, ToolExecutorEvidence};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_tool_contract::{ToolContract, compile_contract_definition};
use signalbox_tool_schema_derive::ToolSchema;
use signalbox_tools_workspace::{WorkspaceFileSystem, WorkspaceRoot};

pub(super) const NAME: &str = "read_diff";

#[derive(serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[tool_schema(description = "Exact repository-relative path in the reviewed change.")]
    path: String,
    #[tool_schema(
        description = "One-based head-side line; returns the containing or nearest hunk."
    )]
    line: u32,
}

struct Contract;
impl ToolContract for Contract {
    type Arguments = Arguments;
    const NAME: &'static str = NAME;
    const DESCRIPTION: &'static str = "Reads the nearest diff hunk for a path and head-side line from the prepared review checkout. Output uses the existing file-read byte ceiling and reports truncation. No code-host request is made.";
}

fn decode(arguments: &NormalizedToolArguments) -> Result<Arguments, &'static str> {
    let parsed = serde_json::from_str::<Arguments>(arguments.as_str())
        .ok()
        .filter(|value| {
            value.line > 0
                && !value.path.is_empty()
                && Path::new(&value.path)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        });
    parsed.ok_or("expected a relative diff path and positive head line")
}

pub(super) fn catalog() -> Result<CompiledToolCatalog, super::DaemonToolsConstructionError> {
    let definition = compile_contract_definition::<Contract>(
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    )
    .map_err(|_| super::DaemonToolsConstructionError::WorkspaceRead)?;
    let detail = ToolExecutionErrorDetail::try_new(String::from(
        "expected a relative diff path and positive head line",
    ))
    .map_err(|_| super::DaemonToolsConstructionError::WorkspaceRead)?;
    CompiledToolCatalog::try_new([CompiledTool::new(
        definition,
        move |arguments: &NormalizedToolArguments| {
            decode(arguments).map(|_| ()).map_err(|_| detail.clone())
        },
    )])
    .map_err(|_| super::DaemonToolsConstructionError::WorkspaceRead)
}

pub(super) fn read<FileSystem: WorkspaceFileSystem>(
    filesystem: &FileSystem,
    root: &Path,
    arguments: &NormalizedToolArguments,
) -> ToolExecutorEvidence {
    let result = decode(arguments)
        .map_err(|error| error.to_string())
        .and_then(|arguments| {
            let root =
                WorkspaceRoot::try_new(filesystem, root).map_err(|error| error.to_string())?;
            let mut reader = filesystem
                .open_file_stream(&root, Path::new("change.patch"))
                .map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            read_hunk(&bytes, arguments).map_err(|error| error.to_string())
        });
    match result {
        Ok(value) => ToolExecutorEvidence::CompletedText(value.to_string()),
        Err(error) => ToolExecutorEvidence::KnownFailed {
            detail: ToolExecutionErrorDetail::try_new(error).ok(),
        },
    }
}

fn read_hunk(bytes: &[u8], arguments: Arguments) -> Result<serde_json::Value, git2::Error> {
    if bytes.is_empty() {
        return Ok(serde_json::json!({"hunk": null}));
    }
    let diff = git2::Diff::from_buffer(bytes)?;
    let mut selected = None;
    for (index, delta) in diff.deltas().enumerate() {
        if delta.new_file().path() != Some(Path::new(&arguments.path))
            && delta.old_file().path() != Some(Path::new(&arguments.path))
        {
            continue;
        }
        let Some(patch) = git2::Patch::from_diff(&diff, index)? else {
            continue;
        };
        for hunk_index in 0..patch.num_hunks() {
            let (hunk, lines) = patch.hunk(hunk_index)?;
            let start = hunk.new_start();
            let end = start.saturating_add(hunk.new_lines().saturating_sub(1));
            let distance = start
                .saturating_sub(arguments.line)
                .max(arguments.line.saturating_sub(end));
            if selected.as_ref().is_some_and(|(best, _)| *best <= distance) {
                continue;
            }
            let mut text = String::from_utf8_lossy(hunk.header()).into_owned();
            let mut truncated = false;
            for line_index in 0..lines {
                let line = patch.line_in_hunk(hunk_index, line_index)?;
                let content = String::from_utf8_lossy(line.content());
                if text.len() + content.len() + 1
                    > signalbox_tools_workspace::MAX_WORKSPACE_READ_BYTES
                {
                    truncated = true;
                    break;
                }
                text.push(line.origin());
                text.push_str(&content);
            }
            selected = Some((
                distance,
                serde_json::json!({"path": arguments.path, "head_start": start, "head_end": end, "text": text, "truncated": truncated}),
            ));
        }
    }
    Ok(serde_json::json!({"hunk": selected.map(|(_, value)| value)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_PATCH: &[u8] = b"diff --git a/source.txt b/source.txt\n--- a/source.txt\n+++ b/source.txt\n@@ -1 +1 @@\n-before\n+after\n";

    #[test]
    fn diff_read_selects_the_hunk_near_the_requested_head_line() {
        let before = (1..=80)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let after = before
            .replace("line 6\n", "first change\n")
            .replace("line 70\n", "second change\n");
        let bytes = git2::Patch::from_buffers(
            before.as_bytes(),
            Some(Path::new("source.txt")),
            after.as_bytes(),
            Some(Path::new("source.txt")),
            None,
        )
        .unwrap()
        .to_buf()
        .unwrap();
        let result = read_hunk(
            &bytes,
            Arguments {
                path: String::from("source.txt"),
                line: 70,
            },
        )
        .unwrap();
        let text = result["hunk"]["text"].as_str().unwrap();
        assert!(text.contains("+second change\n"));
        assert!(!text.contains("first change"));
        assert_eq!(result["hunk"]["truncated"], false);
    }

    #[test]
    fn diff_read_rejects_a_patch_symlink_outside_the_session_root() {
        use signalbox_tools_workspace::LocalWorkspaceFileSystem;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("session");
        std::fs::create_dir(&root).unwrap();
        let outside = directory.path().join("other.patch");
        std::fs::write(&outside, VALID_PATCH).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("change.patch")).unwrap();
        let arguments = NormalizedToolArguments::try_from_provider_text(String::from(
            r#"{"path":"source.txt","line":1}"#,
        ))
        .unwrap();
        assert!(matches!(
            read(&LocalWorkspaceFileSystem, &root, &arguments),
            ToolExecutorEvidence::KnownFailed { .. }
        ));
    }

    #[test]
    fn diff_read_accepts_the_prepared_regular_patch() {
        use signalbox_tools_workspace::LocalWorkspaceFileSystem;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("change.patch"), VALID_PATCH).unwrap();
        let arguments = NormalizedToolArguments::try_from_provider_text(String::from(
            r#"{"path":"source.txt","line":1}"#,
        ))
        .unwrap();
        let ToolExecutorEvidence::CompletedText(text) =
            read(&LocalWorkspaceFileSystem, root.path(), &arguments)
        else {
            panic!("regular prepared patch should be readable")
        };
        assert!(text.contains("+after"));
    }

    #[test]
    fn diff_read_preserves_rename_edits_through_either_path() {
        use signalbox_tools_workspace::LocalWorkspaceFileSystem;

        // A rename with a small edit deep in the file must keep the prepared
        // edit hunk instead of treating the destination as a whole-file addition.
        const RENAME_PATCH: &str = "diff --git a/before.txt b/after.txt\nsimilarity index 99%\nrename from before.txt\nrename to after.txt\nindex 064705c..f190835 100644\n--- a/before.txt\n+++ b/after.txt\n@@ -4997,7 +4997,7 @@ line 4996\n line 4997\n line 4998\n line 4999\n-before\n+after\n line 5001\n line 5002\n line 5003\n";
        const EDIT_HUNK: &str = "@@ -4997,7 +4997,7 @@ line 4996\n line 4997\n line 4998\n line 4999\n-before\n+after\n line 5001\n line 5002\n line 5003\n";
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("change.patch"), RENAME_PATCH).unwrap();

        for path in ["before.txt", "after.txt"] {
            let arguments = NormalizedToolArguments::try_from_provider_text(
                serde_json::json!({"path": path, "line": 5000}).to_string(),
            )
            .unwrap();
            let evidence = read(&LocalWorkspaceFileSystem, root.path(), &arguments);
            let ToolExecutorEvidence::CompletedText(text) = evidence else {
                panic!("prepared rename edit should be readable through {path}: {evidence:?}")
            };
            let result: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                result,
                serde_json::json!({"hunk": {
                    "path": path,
                    "head_start": 4997,
                    "head_end": 5003,
                    "text": EDIT_HUNK,
                    "truncated": false,
                }}),
                "rename evidence through {path}",
            );
        }
    }
}

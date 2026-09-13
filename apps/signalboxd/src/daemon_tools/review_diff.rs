//! Read-only diff hunks from a judgment session's prepared head and base.

use std::path::{Component, Path};

use signalbox_application::{CompiledTool, CompiledToolCatalog, ToolExecutorEvidence};
use signalbox_domain::{
    NormalizedToolArguments, ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault,
};
use signalbox_tool_contract::{ToolContract, compile_contract_definition};
use signalbox_tool_schema_derive::ToolSchema;

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

pub(super) fn read(root: &Path, arguments: &NormalizedToolArguments) -> ToolExecutorEvidence {
    let result = decode(arguments)
        .map_err(|error| error.to_string())
        .and_then(|arguments| read_hunk(root, arguments).map_err(|error| error.to_string()));
    match result {
        Ok(value) => ToolExecutorEvidence::CompletedText(value.to_string()),
        Err(error) => ToolExecutorEvidence::KnownFailed {
            detail: ToolExecutionErrorDetail::try_new(error).ok(),
        },
    }
}

fn read_hunk(root: &Path, arguments: Arguments) -> Result<serde_json::Value, git2::Error> {
    let repository = git2::Repository::open(root.join("head"))?;
    let head = repository.head()?.peel_to_tree()?;
    let base = repository
        .find_reference("refs/review/base")?
        .peel_to_tree()?;
    let mut options = git2::DiffOptions::new();
    options
        .pathspec(&arguments.path)
        .disable_pathspec_match(true);
    let diff = repository.diff_tree_to_tree(Some(&base), Some(&head), Some(&mut options))?;
    let mut selected = None;
    for index in 0..diff.deltas().len() {
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

    fn commit(repository: &git2::Repository, text: &str) -> git2::Oid {
        std::fs::write(repository.workdir().unwrap().join("source.txt"), text).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("source.txt")).unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.com").unwrap();
        let parent = repository
            .head()
            .ok()
            .map(|head| head.peel_to_commit().unwrap());
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Fixture",
                &tree,
                &parent.iter().collect::<Vec<_>>(),
            )
            .unwrap()
    }

    #[test]
    fn diff_read_selects_the_hunk_near_the_requested_head_line() {
        let root = tempfile::tempdir().unwrap();
        let repository = git2::Repository::init(root.path().join("head")).unwrap();
        let before = (1..=80)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let base = commit(&repository, &before);
        repository
            .reference("refs/review/base", base, false, "Fixture base")
            .unwrap();
        commit(
            &repository,
            &before
                .replace("line 6\n", "first change\n")
                .replace("line 70\n", "second change\n"),
        );
        let result = read_hunk(
            root.path(),
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
}

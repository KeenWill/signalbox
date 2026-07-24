use std::{
    env, fs,
    path::{Path, PathBuf},
};

use signalbox_application::ImportedConversationConverter;
use signalbox_conversation_import_codex::{
    CodexRolloutJsonlConversionFailure, CodexRolloutJsonlConverter,
};
use signalbox_domain::{ImportedConversationId, ImportedTranscriptEntryId};
use uuid::Uuid;

#[derive(Debug, Eq, PartialEq)]
enum RealRolloutValidationFailure {
    InputsUnavailable,
    NoInputs,
    ReadFailed,
    ConversionFailed(CodexRolloutJsonlConversionFailure),
}

#[test]
#[ignore = "requires explicit local real-rollout opt-in"]
fn s28_inv038_opt_in_real_rollouts_convert_without_content_output() {
    assert_eq!(validate_opt_in_real_rollouts(), Ok(()));
}

fn validate_opt_in_real_rollouts() -> Result<(), RealRolloutValidationFailure> {
    if env::var("SIGNALBOX_RUN_REAL_CODEX_IMPORT").as_deref() != Ok("1") {
        return Ok(());
    }
    let Some(paths) = env::var_os("SIGNALBOX_REAL_CODEX_ROLLOUTS") else {
        return Err(RealRolloutValidationFailure::NoInputs);
    };
    let roots = env::split_paths(&paths).collect::<Vec<_>>();
    if roots.is_empty() {
        return Err(RealRolloutValidationFailure::NoInputs);
    }
    let mut paths = Vec::new();
    for root in roots {
        collect_rollouts(&root, &mut paths)
            .map_err(|()| RealRolloutValidationFailure::InputsUnavailable)?;
    }
    paths.sort();
    if paths.is_empty() {
        return Err(RealRolloutValidationFailure::NoInputs);
    }
    for (file_index, path) in paths.into_iter().enumerate() {
        let source = fs::read(path).map_err(|_| RealRolloutValidationFailure::ReadFailed)?;
        let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(
            u128::try_from(file_index)
                .ok()
                .and_then(|value| value.checked_add(1))
                .unwrap_or(u128::MAX),
        ));
        let mut entry_index = 1_u128;
        CodexRolloutJsonlConverter
            .convert(conversation, &source, || {
                let identity = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(entry_index));
                entry_index = entry_index.saturating_add(1);
                identity
            })
            .map_err(|error| RealRolloutValidationFailure::ConversionFailed(error.failure()))?;
    }
    Ok(())
}

fn collect_rollouts(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), ()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for child in fs::read_dir(path).map_err(|_| ())? {
        let child = child.map_err(|_| ())?.path();
        let child_metadata = fs::symlink_metadata(&child).map_err(|_| ())?;
        if child_metadata.is_dir() {
            collect_rollouts(&child, files)?;
        } else if child_metadata.is_file()
            && child.extension().and_then(|value| value.to_str()) == Some("jsonl")
        {
            files.push(child);
        }
    }
    Ok(())
}

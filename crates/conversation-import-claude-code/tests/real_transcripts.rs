use std::{
    env, fs,
    path::{Path, PathBuf},
};

use signalbox_application::ImportedConversationConverter;
use signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter;
use signalbox_domain::{ImportedConversationId, ImportedTranscriptEntryId};
use uuid::Uuid;

#[test]
#[ignore = "requires explicit local real-transcript opt-in"]
fn opt_in_real_transcripts_convert_without_content_output() {
    if env::var("SIGNALBOX_RUN_REAL_CLAUDE_IMPORT").as_deref() != Ok("1") {
        return;
    }
    let Some(paths) = env::var_os("SIGNALBOX_REAL_CLAUDE_TRANSCRIPTS") else {
        return;
    };
    let roots = env::split_paths(&paths).collect::<Vec<_>>();
    assert!(
        !roots.is_empty(),
        "real-transcript path list must not be empty"
    );
    let mut paths = Vec::new();
    for root in roots {
        assert!(
            collect_transcripts(&root, &mut paths).is_ok(),
            "real Claude transcript inputs could not be enumerated"
        );
    }
    paths.sort();
    assert!(
        !paths.is_empty(),
        "real Claude transcript inputs contained no files"
    );
    for (file_index, path) in paths.into_iter().enumerate() {
        let source =
            fs::read(path).unwrap_or_else(|_| panic!("real Claude transcript could not be read"));
        let conversation = ImportedConversationId::from_uuid(Uuid::from_u128(
            u128::try_from(file_index)
                .ok()
                .and_then(|value| value.checked_add(1))
                .unwrap_or(u128::MAX),
        ));
        let mut entry_index = 1_u128;
        let imported = ClaudeCodeJsonlConverter.convert(conversation, &source, || {
            let identity = ImportedTranscriptEntryId::from_uuid(Uuid::from_u128(entry_index));
            entry_index = entry_index.saturating_add(1);
            identity
        });
        assert!(
            imported.is_ok(),
            "real Claude transcript failed content-silent conversion"
        );
    }
}

fn collect_transcripts(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), ()> {
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
            collect_transcripts(&child, files)?;
        } else if child_metadata.is_file()
            && child.extension().and_then(|value| value.to_str()) == Some("jsonl")
        {
            files.push(child);
        }
    }
    Ok(())
}

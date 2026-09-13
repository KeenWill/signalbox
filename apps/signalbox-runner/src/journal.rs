//! Atomically published private journal and its reconnect projection.

use crate::state::{
    DocumentKind, MAX_STATE_BYTES, PERMISSION_MASK, RunnerStateError, STATE_MODE, StateOperation,
    StateResource, write_document,
};
use rustix::{
    fs::{Mode, OFlags, openat},
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use signalbox_runner_wire::ReconnectInventory;
use std::{
    fs::File,
    io::{self, Read as _},
    os::unix::fs::MetadataExt as _,
};

const JOURNAL_VERSION: u64 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    entries: Vec<JournalEntry>,
}

// No operation entry is admitted by the registration-only runner.
#[derive(Debug, Serialize, Deserialize)]
enum JournalEntry {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalDocument {
    version: u64,
    journal: Journal,
}

impl Journal {
    pub(crate) fn initialize(directory: &File) -> Result<Self, RunnerStateError> {
        let document = JournalDocument {
            version: JOURNAL_VERSION,
            journal: Self::default(),
        };
        let encoded = serde_json::to_vec(&document).map_err(|_| RunnerStateError::CorruptState)?;
        write_document(directory, DocumentKind::Journal, &encoded)?;
        Ok(document.journal)
    }

    pub(crate) fn open(directory: &File) -> Result<Self, RunnerStateError> {
        let descriptor = openat(
            directory,
            DocumentKind::Journal.file_name(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| journal_io(StateOperation::Open, error.into()))?;
        let mut file = File::from(descriptor);
        let metadata = file
            .metadata()
            .map_err(|error| journal_io(StateOperation::Inspect, error))?;
        if !metadata.is_file()
            || metadata.uid() != geteuid().as_raw()
            || metadata.mode() & PERMISSION_MASK != STATE_MODE
        {
            return Err(RunnerStateError::InvalidStateIdentity);
        }
        if metadata.len() > MAX_STATE_BYTES {
            return Err(RunnerStateError::StateTooLarge);
        }
        let mut encoded = Vec::new();
        file.by_ref()
            .take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut encoded)
            .map_err(|error| journal_io(StateOperation::Read, error))?;
        if encoded.len() as u64 > MAX_STATE_BYTES {
            return Err(RunnerStateError::StateTooLarge);
        }
        let document: JournalDocument =
            serde_json::from_slice(&encoded).map_err(|_| RunnerStateError::CorruptState)?;
        if document.version != JOURNAL_VERSION {
            return Err(RunnerStateError::CorruptState);
        }
        Ok(document.journal)
    }

    pub(crate) fn reconnect_inventory(&self) -> ReconnectInventory {
        match self.entries.first() {
            None => ReconnectInventory::default(),
            Some(entry) => match *entry {},
        }
    }
}

fn journal_io(operation: StateOperation, source: io::Error) -> RunnerStateError {
    RunnerStateError::Io {
        operation,
        resource: StateResource::Journal,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunnerStateRoot;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt as _, symlink},
    };
    use tempfile::TempDir;

    #[test]
    fn private_journal_replays_after_root_reopen() {
        let parent = TempDir::new().expect("temporary parent");
        let path = parent.path().join("runner");
        let state = RunnerStateRoot::open(&path).expect("initialize the journal");
        let inventory = state.reconnect_inventory();
        let metadata =
            fs::metadata(path.join(DocumentKind::Journal.file_name())).expect("journal published");
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(inventory, ReconnectInventory::default());
        drop(state);
        let reopened = RunnerStateRoot::open(&path).expect("replay the journal");
        assert_eq!(reopened.reconnect_inventory(), inventory);
    }

    #[test]
    fn missing_journal_in_an_initialized_root_prevents_reconnect() {
        let parent = TempDir::new().expect("temporary parent");
        let path = parent.path().join("runner");
        drop(RunnerStateRoot::open(&path).expect("initialize the journal"));
        fs::remove_file(path.join(DocumentKind::Journal.file_name())).expect("remove journal");

        let error = RunnerStateRoot::open(&path).expect_err("missing history prevents startup");

        assert!(matches!(
            error,
            RunnerStateError::Io {
                resource: StateResource::Journal,
                ..
            }
        ));
        assert!(!path.join(DocumentKind::Journal.file_name()).exists());
    }

    #[test]
    fn incomplete_temporary_write_does_not_replace_published_journal() {
        let parent = TempDir::new().expect("temporary parent");
        let path = parent.path().join("runner");
        let state = RunnerStateRoot::open(&path).expect("initialize the journal");
        let inventory = state.reconnect_inventory();
        drop(state);
        fs::write(path.join(".operation-journal.json-interrupted.tmp"), b"{")
            .expect("interrupted temporary write");
        let reopened =
            RunnerStateRoot::open(&path).expect("only the published document is replayed");
        assert_eq!(reopened.reconnect_inventory(), inventory);
    }

    #[test]
    fn corrupt_or_unsupported_journal_never_becomes_empty_inventory() {
        for content in [
            "{",
            r#"{"version":2,"journal":{"entries":[]}}"#,
            r#"{"version":1,"journal":{"entries":[{}]}}"#,
            r#"{"version":1,"journal":{"entries":[],"lease":{}}}"#,
        ] {
            let parent = TempDir::new().expect("temporary parent");
            let path = parent.path().join("runner");
            drop(RunnerStateRoot::open(&path).expect("initialize the journal"));
            fs::write(path.join(DocumentKind::Journal.file_name()), content)
                .expect("corrupt journal fixture");
            let error =
                RunnerStateRoot::open(&path).expect_err("corrupt journal must prevent startup");
            assert!(
                matches!(error, RunnerStateError::CorruptState),
                "{content}: {error}"
            );
        }
    }

    #[test]
    fn journal_symlink_is_not_followed() {
        let parent = TempDir::new().expect("temporary parent");
        let path = parent.path().join("runner");
        drop(RunnerStateRoot::open(&path).expect("initialize the journal"));
        let journal = path.join(DocumentKind::Journal.file_name());
        let target = parent.path().join("external");
        fs::rename(&journal, &target).expect("move journal outside private root");
        symlink(&target, &journal).expect("substitute symlink");
        let error = RunnerStateRoot::open(&path).expect_err("symlink must prevent startup");
        assert!(matches!(
            error,
            RunnerStateError::Io {
                operation: StateOperation::Open,
                resource: StateResource::Journal,
                ..
            }
        ));
    }

    #[test]
    fn public_journal_permissions_prevent_startup() {
        let parent = TempDir::new().expect("temporary parent");
        let path = parent.path().join("runner");
        drop(RunnerStateRoot::open(&path).expect("initialize the journal"));
        fs::set_permissions(
            path.join(DocumentKind::Journal.file_name()),
            fs::Permissions::from_mode(0o644),
        )
        .expect("make journal publicly readable");
        assert!(matches!(
            RunnerStateRoot::open(&path).expect_err("journal must remain private"),
            RunnerStateError::InvalidStateIdentity
        ));
    }

    #[test]
    fn publication_replaces_the_inode_without_changing_an_open_reader() {
        let parent = TempDir::new().expect("temporary parent");
        let path = parent.path().join("runner");
        drop(RunnerStateRoot::open(&path).expect("initialize the journal"));
        let published = path.join(DocumentKind::Journal.file_name());
        let mut prior_reader = File::open(&published).expect("open the published journal");
        let prior_inode = prior_reader.metadata().expect("prior inode").ino();
        let directory = File::open(&path).expect("open directory for durable publication");
        let next = br#"{ "version": 1, "journal": {"entries": []} }"#;
        write_document(&directory, DocumentKind::Journal, next)
            .expect("fsync and publish replacement");
        assert_ne!(
            fs::metadata(&published).expect("replacement inode").ino(),
            prior_inode
        );
        assert_eq!(fs::read(&published).expect("replacement bytes"), next);
        let mut prior = Vec::new();
        prior_reader
            .read_to_end(&mut prior)
            .expect("read original open inode");
        assert_ne!(
            prior, next,
            "atomic replacement must not overwrite an existing reader's inode"
        );
        let reopened = RunnerStateRoot::open(&path).expect("published journal replays");
        assert_eq!(
            reopened.reconnect_inventory(),
            ReconnectInventory::default()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_directory_fsync_reports_ambiguous_publication() {
        let parent = TempDir::new().expect("temporary parent");
        let path = parent.path().join("runner");
        drop(RunnerStateRoot::open(&path).expect("initialize the journal"));
        // O_PATH allows descriptor-relative create/rename but refuses fsync.
        let descriptor = rustix::fs::open(
            &path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open path-only directory");
        let directory = File::from(descriptor);
        let next = br#"{ "version": 1, "journal": {"entries": []} }"#;
        let error = write_document(&directory, DocumentKind::Journal, next)
            .expect_err("a failed directory fsync cannot report a durable commit");
        assert!(matches!(error, RunnerStateError::CommitAmbiguous { .. }));
        assert_eq!(
            fs::read(path.join(DocumentKind::Journal.file_name()))
                .expect("rename already published the complete document"),
            next
        );
    }
}

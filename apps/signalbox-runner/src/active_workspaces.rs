//! Acknowledged workspace receipts are independent of the bounded operation journal.

use crate::{
    RunnerState, RunnerStateError, StateOperation, StateResource,
    state::{DocumentKind, PERMISSION_MASK, STATE_MODE, write_document},
    workspace::DirectoryIdentity,
};
use rustix::{
    fs::{Mode, OFlags, openat},
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use signalbox_runner_wire::{CanonicalUuid, Message, WorkspaceReady};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader},
    os::unix::fs::MetadataExt as _,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActiveWorkspaces {
    pub(crate) records: BTreeMap<CanonicalUuid, ActiveWorkspace>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActiveWorkspace {
    pub(crate) ready: WorkspaceReady,
    pub(crate) directory_identity: DirectoryIdentity,
}

impl ActiveWorkspaces {
    pub(crate) fn initialize(directory: &File) -> Result<Self, RunnerStateError> {
        match Self::open(directory) {
            Ok(records) if records.records.is_empty() => Ok(records),
            Ok(_) => Err(RunnerStateError::CorruptState),
            Err(RunnerStateError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                let mut records = Self::default();
                records.publish(directory, Self::default())?;
                Ok(records)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn open(directory: &File) -> Result<Self, RunnerStateError> {
        let descriptor = openat(
            directory,
            DocumentKind::ActiveWorkspaces.file_name(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| io_error(error.into()))?;
        let file = File::from(descriptor);
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file()
            || metadata.uid() != geteuid().as_raw()
            || metadata.mode() & PERMISSION_MASK != STATE_MODE
        {
            return Err(RunnerStateError::InvalidStateIdentity);
        }
        let records: Self = serde_json::from_reader(BufReader::new(file))
            .map_err(|_| RunnerStateError::CorruptState)?;
        records.validate()?;
        Ok(records)
    }

    pub(crate) fn validate_owner(&self, state: &RunnerState) -> Result<(), RunnerStateError> {
        for active in self.records.values() {
            if !state.receipt().is_some_and(|receipt| {
                receipt.runner_id() == active.ready.correlation.runner_id
                    && receipt.registration_revision()
                        >= active.ready.correlation.registration_revision
            }) {
                return Err(RunnerStateError::CorruptState);
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), RunnerStateError> {
        for (manifest, active) in &self.records {
            if *manifest != active.ready.ready.manifest.manifest_id
                || Message::WorkspaceReady(active.ready.clone())
                    .validate()
                    .is_err()
            {
                return Err(RunnerStateError::CorruptState);
            }
        }
        Ok(())
    }

    pub(crate) fn record(
        &mut self,
        directory: &File,
        ready: WorkspaceReady,
        directory_identity: DirectoryIdentity,
    ) -> Result<(), RunnerStateError> {
        let key = ready.ready.manifest.manifest_id;
        let active = ActiveWorkspace {
            ready,
            directory_identity,
        };
        if let Some(prior) = self.records.get(&key) {
            return if prior == &active {
                Ok(())
            } else {
                Err(RunnerStateError::InvalidTransition)
            };
        }
        let mut next = self.clone();
        next.records.insert(key, active);
        self.publish(directory, next)
    }

    pub(crate) fn remove(
        &mut self,
        directory: &File,
        manifest: CanonicalUuid,
    ) -> Result<(), RunnerStateError> {
        let mut next = self.clone();
        next.records.remove(&manifest);
        self.publish(directory, next)
    }

    fn publish(&mut self, directory: &File, next: Self) -> Result<(), RunnerStateError> {
        next.validate()?;
        let encoded = serde_json::to_vec(&next).map_err(|_| RunnerStateError::CorruptState)?;
        write_document(directory, DocumentKind::ActiveWorkspaces, &encoded)?;
        *self = next;
        Ok(())
    }
}

fn io_error(source: io::Error) -> RunnerStateError {
    RunnerStateError::Io {
        operation: StateOperation::Open,
        resource: StateResource::ActiveWorkspaces,
        source,
    }
}

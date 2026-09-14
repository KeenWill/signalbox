//! Atomically published private journal and its reconnect projection.

use crate::state::{
    DocumentKind, PERMISSION_MASK, RunnerStateError, STATE_MODE, StateOperation, StateResource,
    write_document,
};
use rustix::{
    fs::{Mode, OFlags, openat},
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use signalbox_runner_wire::{
    LeaseCorrelation, LeasePhase, LeasePhaseKind, MAX_FRAME_BYTES, Message, OperationCorrelation,
    OperationFailure, ProvisionPhase, ReconnectInventory, ReleaseCorrelation, ReleasePhase,
    RetainedResult, WorkspaceOperation, WorkspaceProvision, WorkspaceReady, WorkspaceRecorded,
};
use std::{
    fs::File,
    io::{self, Read as _},
    os::unix::fs::MetadataExt as _,
};

const JOURNAL_VERSION: u64 = 1;
// One bounded wire inventory fits the frame ceiling, including JSON escaping of
// the fixed 1 MiB terminal text. Enrollment retains its separate 16 KiB bound.
const MAX_JOURNAL_BYTES: u64 = MAX_FRAME_BYTES as u64;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    entries: Vec<JournalEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum JournalEntry {
    Release {
        correlation: ReleaseCorrelation,
        phase: ReleasePhase,
    },
    Lease {
        phase: LeasePhase,
        result: Option<RetainedResult>,
    },
    Provision {
        request: WorkspaceProvision,
        ready: Option<WorkspaceReady>,
        failure: Option<Box<OperationFailure>>,
    },
}

/// Cleanup capability obtained only from a durably accepted journal entry.
#[derive(Clone, Debug)]
pub(crate) struct AcceptedWorkspaceRelease(ReleaseCorrelation);
impl AcceptedWorkspaceRelease {
    pub(crate) fn correlation(&self) -> &ReleaseCorrelation {
        &self.0
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalDocument {
    version: u64,
    journal: Journal,
}

impl Journal {
    pub(crate) fn validate_owner(
        &self,
        state: &crate::RunnerState,
    ) -> Result<(), RunnerStateError> {
        if let Some(JournalEntry::Lease { phase, .. }) = self.entries.first() {
            match state {
                crate::RunnerState::Enrolled { receipt }
                    if receipt.runner_id() == phase.correlation.runner_id
                        && receipt.registration_revision()
                            >= phase.correlation.registration_revision
                        && receipt.authority() == crate::EnrollmentAuthority::Active => {}
                _ => return Err(RunnerStateError::CorruptState),
            }
        }
        if let Some(JournalEntry::Provision { request, .. }) = self.entries.first() {
            match state {
                crate::RunnerState::Enrolled { receipt }
                    if receipt.runner_id() == request.correlation.runner_id
                        && receipt.registration_revision()
                            >= request.correlation.registration_revision => {}
                _ => return Err(RunnerStateError::CorruptState),
            }
        }
        if let Some((correlation, _)) = self.release()
            && !state
                .receipt()
                .is_some_and(|receipt| receipt.runner_id() == correlation.runner_id)
        {
            return Err(RunnerStateError::CorruptState);
        }
        Ok(())
    }

    pub(crate) fn initialize(directory: &File) -> Result<Self, RunnerStateError> {
        match Self::open(directory) {
            Ok(journal) if journal.entries.is_empty() => {
                return Ok(journal);
            }
            Ok(_) => return Err(RunnerStateError::CorruptState),
            Err(RunnerStateError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
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
        if metadata.len() > MAX_JOURNAL_BYTES {
            return Err(RunnerStateError::StateTooLarge);
        }
        let mut encoded = Vec::new();
        file.by_ref()
            .take(MAX_JOURNAL_BYTES + 1)
            .read_to_end(&mut encoded)
            .map_err(|error| journal_io(StateOperation::Read, error))?;
        if encoded.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(RunnerStateError::StateTooLarge);
        }
        let document: JournalDocument =
            serde_json::from_slice(&encoded).map_err(|_| RunnerStateError::CorruptState)?;
        if document.version != JOURNAL_VERSION {
            return Err(RunnerStateError::CorruptState);
        }
        document.journal.validate()?;
        Ok(document.journal)
    }

    pub(crate) fn reconnect_inventory(&self) -> ReconnectInventory {
        match self.entries.first() {
            Some(JournalEntry::Release { correlation, phase }) => ReconnectInventory {
                workspace_operation: Some(WorkspaceOperation::Release {
                    correlation: correlation.clone(),
                    phase: *phase,
                }),
                ..ReconnectInventory::default()
            },
            None => ReconnectInventory::default(),
            Some(JournalEntry::Lease { phase, result }) => ReconnectInventory {
                lease: Some(phase.clone()),
                result: result.clone(),
                ..ReconnectInventory::default()
            },
            Some(JournalEntry::Provision {
                request,
                ready,
                failure,
            }) => ReconnectInventory {
                operation_failure: failure.as_deref().cloned(),
                workspace_operation: Some(WorkspaceOperation::Provision {
                    correlation: request.correlation.clone(),
                    phase: if ready.is_some() {
                        ProvisionPhase::ReadyUnrecorded
                    } else {
                        ProvisionPhase::Provisioning
                    },
                }),
                ..ReconnectInventory::default()
            },
        }
    }

    fn validate(&self) -> Result<(), RunnerStateError> {
        if self.entries.len() > 1 {
            return Err(RunnerStateError::CorruptState);
        }
        if let Some(JournalEntry::Provision {
            request,
            ready,
            failure,
        }) = self.entries.first()
            && (failure.as_ref().is_some_and(|failure| {
                ready.is_some()
                    || failure.correlation
                        != OperationCorrelation::Provision(request.correlation.clone())
                    || failure.validate().is_err()
            }) || Message::WorkspaceProvision(request.clone())
                .validate()
                .is_err()
                || ready.as_ref().is_some_and(|ready| {
                    ready.correlation != request.correlation
                        || Message::WorkspaceReady(ready.clone()).validate().is_err()
                        || request.recovery.as_ref().is_some_and(|recovery| {
                            ready.ready.manifest.recovery.as_ref() != Some(recovery)
                        })
                }))
        {
            return Err(RunnerStateError::CorruptState);
        }
        if let Some(JournalEntry::Lease {
            phase,
            result: Some(result),
        }) = self.entries.first()
            && (phase.phase != LeasePhaseKind::ExecutionMayHaveStarted
                || phase.correlation != result.correlation
                || result.result.validate().is_err())
        {
            return Err(RunnerStateError::CorruptState);
        }
        Ok(())
    }

    fn without_operation(&self) -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn publish(&mut self, directory: &File, next: Self) -> Result<(), RunnerStateError> {
        next.validate()?;
        let document = JournalDocument {
            version: JOURNAL_VERSION,
            journal: next,
        };
        let encoded = serde_json::to_vec(&document).map_err(|_| RunnerStateError::CorruptState)?;
        if encoded.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(RunnerStateError::StateTooLarge);
        }
        write_document(directory, DocumentKind::Journal, &encoded)?;
        *self = document.journal;
        Ok(())
    }

    pub(crate) fn release(&self) -> Option<(&ReleaseCorrelation, ReleasePhase)> {
        match self.entries.first() {
            Some(JournalEntry::Release { correlation, phase }) => Some((correlation, *phase)),
            _ => None,
        }
    }
    pub(crate) fn accepted_release(&self) -> Option<AcceptedWorkspaceRelease> {
        self.release().and_then(|(correlation, phase)| {
            (phase == ReleasePhase::ReleaseAccepted)
                .then(|| AcceptedWorkspaceRelease(correlation.clone()))
        })
    }
    pub(crate) fn record_release(
        &mut self,
        directory: &File,
        correlation: ReleaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        match self.release() {
            Some((prior, _)) if prior == &correlation => return Ok(()),
            Some(_) => return Err(RunnerStateError::InvalidTransition),
            None if !self.entries.is_empty() => return Err(RunnerStateError::InvalidTransition),
            None => {}
        }
        self.publish(
            directory,
            Self {
                entries: vec![JournalEntry::Release {
                    correlation,
                    phase: ReleasePhase::ReleaseAccepted,
                }],
            },
        )
    }
    pub(crate) fn complete_release(
        &mut self,
        directory: &File,
        correlation: &ReleaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        if !self
            .release()
            .is_some_and(|(prior, _)| prior == correlation)
        {
            return Err(RunnerStateError::InvalidTransition);
        }
        self.publish(
            directory,
            Self {
                entries: vec![JournalEntry::Release {
                    correlation: correlation.clone(),
                    phase: ReleasePhase::ReleaseCompleted,
                }],
            },
        )
    }
    pub(crate) fn acknowledge_release(
        &mut self,
        directory: &File,
        correlation: &ReleaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        if self.release() != Some((correlation, ReleasePhase::ReleaseCompleted)) {
            return Err(RunnerStateError::InvalidTransition);
        }
        self.publish(directory, self.without_operation())
    }

    pub(crate) fn provision(&self) -> Option<(&WorkspaceProvision, Option<&WorkspaceReady>)> {
        match self.entries.first() {
            Some(JournalEntry::Provision { request, ready, .. }) => Some((request, ready.as_ref())),
            _ => None,
        }
    }

    pub(crate) fn record_provision(
        &mut self,
        directory: &File,
        request: WorkspaceProvision,
    ) -> Result<(), RunnerStateError> {
        match self.entries.first() {
            None => self.publish(
                directory,
                Self {
                    entries: vec![JournalEntry::Provision {
                        request,
                        ready: None,
                        failure: None,
                    }],
                },
            ),
            Some(JournalEntry::Provision { request: prior, .. }) if prior == &request => Ok(()),
            _ => Err(RunnerStateError::InvalidTransition),
        }
    }

    pub(crate) fn record_workspace_ready(
        &mut self,
        directory: &File,
        ready: WorkspaceReady,
    ) -> Result<(), RunnerStateError> {
        let Some(JournalEntry::Provision {
            request,
            ready: prior,
            failure: None,
        }) = self.entries.first()
        else {
            return Err(RunnerStateError::InvalidTransition);
        };
        if request.correlation != ready.correlation {
            return Err(RunnerStateError::InvalidTransition);
        }
        if let Some(prior) = prior {
            return if prior == &ready {
                Ok(())
            } else {
                Err(RunnerStateError::InvalidTransition)
            };
        }
        self.publish(
            directory,
            Self {
                entries: vec![JournalEntry::Provision {
                    request: request.clone(),
                    ready: Some(ready),
                    failure: None,
                }],
            },
        )
    }

    pub(crate) fn provision_failure(&self) -> Option<&OperationFailure> {
        match self.entries.first() {
            Some(JournalEntry::Provision { failure, .. }) => failure.as_deref(),
            _ => None,
        }
    }
    pub(crate) fn record_provision_failure(
        &mut self,
        directory: &File,
        failure: OperationFailure,
    ) -> Result<(), RunnerStateError> {
        let Some(JournalEntry::Provision {
            request,
            ready: None,
            failure: prior,
        }) = self.entries.first()
        else {
            return Err(RunnerStateError::InvalidTransition);
        };
        if failure.correlation != OperationCorrelation::Provision(request.correlation.clone()) {
            return Err(RunnerStateError::InvalidTransition);
        }
        if let Some(prior) = prior {
            return if prior.as_ref() == &failure {
                Ok(())
            } else {
                Err(RunnerStateError::InvalidTransition)
            };
        }
        self.publish(
            directory,
            Self {
                entries: vec![JournalEntry::Provision {
                    request: request.clone(),
                    ready: None,
                    failure: Some(Box::new(failure)),
                }],
            },
        )
    }
    pub(crate) fn acknowledge_provision_failure(
        &mut self,
        directory: &File,
        correlation: &OperationCorrelation,
    ) -> Result<(), RunnerStateError> {
        if !self
            .provision_failure()
            .is_some_and(|failure| &failure.correlation == correlation)
        {
            return Err(RunnerStateError::InvalidTransition);
        }
        self.publish(directory, self.without_operation())
    }

    pub(crate) fn acknowledge_workspace(
        &mut self,
        directory: &File,
        recorded: &WorkspaceRecorded,
    ) -> Result<(), RunnerStateError> {
        match self.entries.first() {
            Some(JournalEntry::Provision {
                ready: Some(ready), ..
            }) if ready.correlation == recorded.correlation
                && ready.ready.manifest.manifest_id == recorded.manifest_id
                && ready.ready.manifest_digest == recorded.manifest_digest =>
            {
                self.publish(directory, self.without_operation())
            }
            _ => Err(RunnerStateError::InvalidTransition),
        }
    }

    pub(crate) fn record_phase(
        &mut self,
        directory: &File,
        phase: LeasePhase,
    ) -> Result<(), RunnerStateError> {
        match self.entries.first() {
            None if phase.phase == LeasePhaseKind::WaitingDispatch => {}
            Some(JournalEntry::Lease {
                phase: prior,
                result: None,
            }) if prior.correlation == phase.correlation => {
                if prior.phase == phase.phase {
                    return Ok(());
                }
                if !matches!(
                    (prior.phase, phase.phase),
                    (
                        LeasePhaseKind::WaitingDispatch,
                        LeasePhaseKind::DispatchReceived
                    ) | (
                        LeasePhaseKind::DispatchReceived,
                        LeasePhaseKind::ExecutionMayHaveStarted
                    )
                ) {
                    return Err(RunnerStateError::InvalidTransition);
                }
            }
            _ => return Err(RunnerStateError::InvalidTransition),
        }
        self.publish(
            directory,
            Self {
                entries: vec![JournalEntry::Lease {
                    phase,
                    result: None,
                }],
            },
        )
    }

    pub(crate) fn record_result(
        &mut self,
        directory: &File,
        result: RetainedResult,
    ) -> Result<(), RunnerStateError> {
        let Some(JournalEntry::Lease {
            phase,
            result: prior,
        }) = self.entries.first()
        else {
            return Err(RunnerStateError::InvalidTransition);
        };
        if phase.phase != LeasePhaseKind::ExecutionMayHaveStarted
            || phase.correlation != result.correlation
        {
            return Err(RunnerStateError::InvalidTransition);
        }
        if let Some(prior) = prior {
            return if prior == &result {
                Ok(())
            } else {
                Err(RunnerStateError::InvalidTransition)
            };
        }
        self.publish(
            directory,
            Self {
                entries: vec![JournalEntry::Lease {
                    phase: phase.clone(),
                    result: Some(result),
                }],
            },
        )
    }

    pub(crate) fn acknowledge_result(
        &mut self,
        directory: &File,
        correlation: &LeaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        match self.entries.first() {
            Some(JournalEntry::Lease {
                result: Some(result),
                ..
            }) if &result.correlation == correlation => {
                self.publish(directory, self.without_operation())
            }
            _ => Err(RunnerStateError::InvalidTransition),
        }
    }

    pub(crate) fn discard_lease(
        &mut self,
        directory: &File,
        correlation: &LeaseCorrelation,
    ) -> Result<(), RunnerStateError> {
        match self.entries.first() {
            Some(JournalEntry::Lease { phase, .. }) if &phase.correlation == correlation => {
                self.publish(directory, self.without_operation())
            }
            _ => Err(RunnerStateError::InvalidTransition),
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

    fn correlation() -> LeaseCorrelation {
        use signalbox_runner_wire::{
            CanonicalUuid, PositiveU64, SandboxProfile, WireToolName, WorkingDirectory,
        };
        let identity = |value| CanonicalUuid::from_uuid(uuid::Uuid::from_u128(value));
        let first = PositiveU64::try_new(1).expect("first revision");
        LeaseCorrelation {
            registration_revision: first,
            placement_revision: first,
            lease_id: identity(1),
            lease_generation: first,
            runner_id: identity(2),
            working_directory: WorkingDirectory::try_new("/tmp/runner-work".to_owned())
                .expect("fixture directory"),
            sandbox_profile: SandboxProfile::Ambient,
            tool_name: WireToolName::try_new("echo".to_owned()).expect("compiled tool"),
            session_id: identity(3),
            turn_id: identity(4),
            tool_request_id: identity(5),
            tool_attempt_id: identity(6),
            issuing_turn_attempt_id: identity(7),
            tool_dispatch_generation: first,
        }
    }

    fn phase(kind: LeasePhaseKind) -> LeasePhase {
        LeasePhase {
            correlation: correlation(),
            phase: kind,
        }
    }

    fn started_journal(directory: &File) -> Journal {
        let mut journal = Journal::initialize(directory).expect("empty journal");
        journal
            .record_phase(directory, phase(LeasePhaseKind::WaitingDispatch))
            .expect("claim persisted");
        journal
            .record_phase(directory, phase(LeasePhaseKind::DispatchReceived))
            .expect("dispatch persisted");
        journal
            .record_phase(directory, phase(LeasePhaseKind::ExecutionMayHaveStarted))
            .expect("executor gate persisted");
        journal
    }

    #[test]
    fn initialization_cannot_erase_retained_execution_without_enrollment() {
        let parent = TempDir::new().expect("temporary journal root");
        let directory = File::open(parent.path()).expect("directory descriptor");
        let journal = started_journal(&directory);
        let before = journal.reconnect_inventory();
        assert!(matches!(
            Journal::initialize(&directory),
            Err(RunnerStateError::CorruptState)
        ));
        assert_eq!(
            Journal::open(&directory)
                .expect("retained execution survives")
                .reconnect_inventory(),
            before
        );
    }

    #[test]
    fn phases_advance_durably_and_cannot_skip_the_claim() {
        let parent = TempDir::new().expect("temporary journal root");
        let directory = File::open(parent.path()).expect("directory descriptor");
        let mut journal = Journal::initialize(&directory).expect("empty journal");
        assert!(
            journal
                .record_phase(&directory, phase(LeasePhaseKind::DispatchReceived))
                .is_err()
        );
        assert!(
            journal
                .record_phase(&directory, phase(LeasePhaseKind::ExecutionMayHaveStarted))
                .is_err()
        );
        journal
            .record_phase(&directory, phase(LeasePhaseKind::WaitingDispatch))
            .expect("claim persisted");
        assert!(
            journal
                .record_phase(&directory, phase(LeasePhaseKind::ExecutionMayHaveStarted))
                .is_err()
        );
        assert_eq!(
            Journal::open(&directory)
                .expect("replay")
                .reconnect_inventory()
                .lease,
            Some(phase(LeasePhaseKind::WaitingDispatch))
        );
        journal
            .record_phase(&directory, phase(LeasePhaseKind::DispatchReceived))
            .expect("dispatch persisted");
        assert_eq!(
            Journal::open(&directory)
                .expect("replay")
                .reconnect_inventory()
                .lease,
            Some(phase(LeasePhaseKind::DispatchReceived))
        );
        journal
            .record_phase(&directory, phase(LeasePhaseKind::ExecutionMayHaveStarted))
            .expect("executor gate persisted");
        assert_eq!(
            Journal::open(&directory)
                .expect("replay")
                .reconnect_inventory()
                .lease,
            Some(phase(LeasePhaseKind::ExecutionMayHaveStarted))
        );
        assert!(
            journal
                .record_phase(&directory, phase(LeasePhaseKind::WaitingDispatch))
                .is_err()
        );
    }

    #[test]
    fn every_correlation_member_fences_phases_results_and_acknowledgements() {
        let original = serde_json::to_value(correlation()).expect("wire correlation");
        for (member, value) in original.as_object().expect("object").iter() {
            let mut changed = original.clone();
            changed[member] = match value {
                serde_json::Value::Number(_) => serde_json::json!(2),
                serde_json::Value::String(_) if member == "sandbox_profile" => {
                    serde_json::json!("workspace_restricted")
                }
                serde_json::Value::String(_) if member == "working_directory" => {
                    serde_json::json!("/tmp/other-runner-work")
                }
                serde_json::Value::String(_) if member == "tool_name" => {
                    serde_json::json!("other_tool")
                }
                serde_json::Value::String(_) => {
                    serde_json::json!("00000000-0000-0000-0000-000000000099")
                }
                _ => panic!("lease correlation members are scalars"),
            };
            let changed: LeaseCorrelation =
                serde_json::from_value(changed).expect("individually valid changed member");
            let parent = TempDir::new().expect("temporary journal root");
            let directory = File::open(parent.path()).expect("directory descriptor");
            let mut journal = Journal::initialize(&directory).expect("empty journal");
            journal
                .record_phase(&directory, phase(LeasePhaseKind::WaitingDispatch))
                .expect("claim persisted");
            assert!(
                journal
                    .record_phase(
                        &directory,
                        LeasePhase {
                            correlation: changed.clone(),
                            phase: LeasePhaseKind::DispatchReceived
                        }
                    )
                    .is_err(),
                "{member}"
            );
            journal
                .record_phase(&directory, phase(LeasePhaseKind::DispatchReceived))
                .expect("dispatch persisted");
            journal
                .record_phase(&directory, phase(LeasePhaseKind::ExecutionMayHaveStarted))
                .expect("executor gate persisted");
            let terminal = signalbox_runner_wire::TerminalResult::Success {
                text: "echo".to_owned(),
            };
            assert!(
                journal
                    .record_result(
                        &directory,
                        RetainedResult {
                            correlation: changed.clone(),
                            result: terminal.clone()
                        }
                    )
                    .is_err(),
                "{member}"
            );
            journal
                .record_result(
                    &directory,
                    RetainedResult {
                        correlation: correlation(),
                        result: terminal,
                    },
                )
                .expect("matching result");
            assert!(
                journal.acknowledge_result(&directory, &changed).is_err(),
                "{member}"
            );
            assert!(
                Journal::open(&directory)
                    .expect("replay")
                    .reconnect_inventory()
                    .result
                    .is_some(),
                "{member}"
            );
        }
    }

    #[test]
    fn maximal_escaped_result_is_retained_until_exact_acknowledgement() {
        let parent = TempDir::new().expect("temporary journal root");
        let directory = File::open(parent.path()).expect("directory descriptor");
        let mut journal = started_journal(&directory);
        let result = RetainedResult {
            correlation: correlation(),
            result: signalbox_runner_wire::TerminalResult::Success {
                text: "\u{1}".repeat(signalbox_runner_wire::SUCCESS_TEXT_BYTES as usize),
            },
        };
        journal
            .record_result(&directory, result.clone())
            .expect("the wire text ceiling fits its journal including JSON escaping");
        journal
            .record_result(&directory, result.clone())
            .expect("exact duplicate is unchanged");
        let reopened = Journal::open(&directory).expect("replay maximal result");
        assert_eq!(
            reopened.reconnect_inventory().result.as_ref(),
            Some(&result)
        );
        assert!(
            journal
                .record_result(
                    &directory,
                    RetainedResult {
                        correlation: correlation(),
                        result: signalbox_runner_wire::TerminalResult::Ambiguous
                    }
                )
                .is_err()
        );
        assert!(
            journal
                .record_phase(&directory, phase(LeasePhaseKind::WaitingDispatch))
                .is_err()
        );
        journal
            .acknowledge_result(&directory, &correlation())
            .expect("exact acknowledgement clears the slot");
        assert_eq!(
            Journal::open(&directory)
                .expect("replay acknowledged journal")
                .reconnect_inventory(),
            ReconnectInventory::default()
        );
        journal
            .record_phase(&directory, phase(LeasePhaseKind::WaitingDispatch))
            .expect("next lease can acquire the slot");
    }

    #[test]
    fn terminal_evidence_requires_the_executor_gate() {
        let parent = TempDir::new().expect("temporary journal root");
        let directory = File::open(parent.path()).expect("directory descriptor");
        let mut journal = Journal::initialize(&directory).expect("empty journal");
        let result = RetainedResult {
            correlation: correlation(),
            result: signalbox_runner_wire::TerminalResult::Ambiguous,
        };
        assert!(journal.record_result(&directory, result.clone()).is_err());
        journal
            .record_phase(&directory, phase(LeasePhaseKind::WaitingDispatch))
            .expect("claim persisted");
        assert!(journal.record_result(&directory, result.clone()).is_err());
        journal
            .record_phase(&directory, phase(LeasePhaseKind::DispatchReceived))
            .expect("dispatch persisted");
        assert!(journal.record_result(&directory, result).is_err());
    }

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
                operation: StateOperation::Open,
                resource: StateResource::Journal,
                source,
            } if source.kind() == std::io::ErrorKind::NotFound
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

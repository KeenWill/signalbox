//! Process-local routing to the task that owns one established runner socket.

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

use signalbox_domain::{RunnerEnrollmentId, RunnerId};
use signalbox_persistence::runner_protocol::RunnerConnectionEpoch;
use signalbox_runner_wire::{CanonicalUuid, Message, OperationCorrelation};
use tokio::sync::mpsc;

// Durable operation state stays in PostgreSQL. The process-local queue only
// hands one frame to the connection task and applies backpressure to later work.
const OUTBOUND_OPERATION_CAPACITY: usize = 1;

/// Exact established physical runner connection selected for outbound work.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RunnerConnectionAddress {
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    epoch: RunnerConnectionEpoch,
}

impl RunnerConnectionAddress {
    /// Names one exact enrollment, runner, and physical connection epoch.
    pub const fn new(
        enrollment: RunnerEnrollmentId,
        runner: RunnerId,
        epoch: RunnerConnectionEpoch,
    ) -> Self {
        Self {
            enrollment,
            runner,
            epoch,
        }
    }

    /// Returns the owning enrollment.
    pub const fn enrollment(self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the logical runner.
    pub const fn runner(self) -> RunnerId {
        self.runner
    }

    /// Returns the exact physical connection epoch.
    pub const fn epoch(self) -> RunnerConnectionEpoch {
        self.epoch
    }
}

/// Process-local outbound routing failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerConnectionBrokerError {
    /// Broker bookkeeping was poisoned by an unwinding caller.
    StateUnavailable,
    /// The exact durable connection address has no attached socket task.
    ConnectionUnavailable,
    /// Another socket task already owns the exact address.
    ConnectionAlreadyAttached,
    /// The supplied message is not a daemon-to-runner operation frame.
    UnsupportedMessage,
    /// The operation correlation names another runner.
    RunnerMismatch,
    /// The exact connection's bounded outbound queue is full.
    QueueFull,
}

impl fmt::Display for RunnerConnectionBrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::StateUnavailable => "runner connection broker state is unavailable",
            Self::ConnectionUnavailable => "runner connection is unavailable",
            Self::ConnectionAlreadyAttached => "runner connection is already attached",
            Self::UnsupportedMessage => "message is not an outbound runner operation",
            Self::RunnerMismatch => "runner operation names another connection",
            Self::QueueFull => "runner connection outbound queue is full",
        })
    }
}

impl Error for RunnerConnectionBrokerError {}

#[derive(Debug)]
struct BrokerEntry {
    generation: u64,
    sender: mpsc::Sender<Message>,
}

#[derive(Debug, Default)]
struct BrokerState {
    next_generation: u64,
    connections: HashMap<RunnerConnectionAddress, BrokerEntry>,
}

/// Cloneable routing handle shared with daemon runner-operation producers.
#[derive(Clone, Debug, Default)]
pub struct RunnerConnectionBroker {
    state: Arc<Mutex<BrokerState>>,
}

impl RunnerConnectionBroker {
    /// Constructs an empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueues one closed daemon-to-runner operation on its exact live socket.
    pub fn send(
        &self,
        address: RunnerConnectionAddress,
        message: Message,
    ) -> Result<(), RunnerConnectionBrokerError> {
        let scope = outbound_operation_scope(&message)
            .ok_or(RunnerConnectionBrokerError::UnsupportedMessage)?;
        if let OutboundOperationScope::Runner(runner) = scope
            && runner.into_uuid() != address.runner().into_uuid()
        {
            return Err(RunnerConnectionBrokerError::RunnerMismatch);
        }
        let sender = self
            .lock_state()?
            .connections
            .get(&address)
            .map(|entry| entry.sender.clone())
            .ok_or(RunnerConnectionBrokerError::ConnectionUnavailable)?;
        sender.try_send(message).map_err(|failure| match failure {
            mpsc::error::TrySendError::Full(_) => RunnerConnectionBrokerError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => {
                RunnerConnectionBrokerError::ConnectionUnavailable
            }
        })
    }

    pub(crate) fn attach(
        &self,
        address: RunnerConnectionAddress,
    ) -> Result<RunnerConnectionAttachment, RunnerConnectionBrokerError> {
        let (sender, receiver) = mpsc::channel(OUTBOUND_OPERATION_CAPACITY);
        let mut state = self.lock_state()?;
        if state.connections.contains_key(&address) {
            return Err(RunnerConnectionBrokerError::ConnectionAlreadyAttached);
        }
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .ok_or(RunnerConnectionBrokerError::StateUnavailable)?;
        let generation = state.next_generation;
        state
            .connections
            .insert(address, BrokerEntry { generation, sender });
        Ok(RunnerConnectionAttachment {
            receiver,
            _lease: RunnerConnectionLease {
                broker: self.clone(),
                address,
                generation,
            },
        })
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, BrokerState>, RunnerConnectionBrokerError> {
        self.state
            .lock()
            .map_err(|_| RunnerConnectionBrokerError::StateUnavailable)
    }
}

pub(crate) struct RunnerConnectionAttachment {
    receiver: mpsc::Receiver<Message>,
    _lease: RunnerConnectionLease,
}

impl RunnerConnectionAttachment {
    pub(crate) async fn receive(&mut self) -> Option<Message> {
        self.receiver.recv().await
    }
}

struct RunnerConnectionLease {
    broker: RunnerConnectionBroker,
    address: RunnerConnectionAddress,
    generation: u64,
}

impl Drop for RunnerConnectionLease {
    fn drop(&mut self) {
        let Ok(mut state) = self.broker.state.lock() else {
            return;
        };
        let owns_entry = state
            .connections
            .get(&self.address)
            .is_some_and(|entry| entry.generation == self.generation);
        if owns_entry {
            state.connections.remove(&self.address);
        }
    }
}

enum OutboundOperationScope {
    ExactConnection,
    Runner(CanonicalUuid),
}

fn outbound_operation_scope(message: &Message) -> Option<OutboundOperationScope> {
    match message {
        Message::WorkspaceLeakRecorded(_) => Some(OutboundOperationScope::ExactConnection),
        Message::WorkspaceProvision(value) => {
            Some(OutboundOperationScope::Runner(value.correlation.runner_id))
        }
        Message::WorkspaceRecorded(value) => {
            Some(OutboundOperationScope::Runner(value.correlation.runner_id))
        }
        Message::WorkspaceRelease(value) => {
            Some(OutboundOperationScope::Runner(value.correlation.runner_id))
        }
        Message::WorkspaceReleaseRecorded(value) => {
            Some(OutboundOperationScope::Runner(value.correlation.runner_id))
        }
        Message::LeaseOffer(value) => {
            Some(OutboundOperationScope::Runner(value.correlation.runner_id))
        }
        Message::LeaseClaimed(value) => {
            Some(OutboundOperationScope::Runner(value.correlation.runner_id))
        }
        Message::Dispatch(value) => {
            Some(OutboundOperationScope::Runner(value.correlation.runner_id))
        }
        Message::ResultRecorded(value) => {
            Some(OutboundOperationScope::Runner(value.correlation.runner_id))
        }
        Message::OperationFailureRecorded(value) => {
            Some(OutboundOperationScope::Runner(match &value.correlation {
                OperationCorrelation::Provision(correlation) => correlation.runner_id,
                OperationCorrelation::Release(correlation) => correlation.runner_id,
                OperationCorrelation::LeaseOffer(correlation) => correlation.runner_id,
            }))
        }
        Message::Enroll(_)
        | Message::Enrolled(_)
        | Message::Resume(_)
        | Message::Resumed(_)
        | Message::ReplacementPending(_)
        | Message::Advertise(_)
        | Message::Registered(_)
        | Message::Heartbeat(_)
        | Message::HeartbeatAck(_)
        | Message::WorkspaceLeakPage(_)
        | Message::WorkspaceReady(_)
        | Message::WorkspaceReleased(_)
        | Message::LeaseClaim(_)
        | Message::Result(_)
        | Message::OperationFailed(_)
        | Message::Shutdown(_)
        | Message::Rejected(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{RunnerEnrollmentId, RunnerId};
    use signalbox_persistence::runner_protocol::RunnerConnectionEpoch;
    use signalbox_runner_wire::{
        CanonicalUuid, Digest, LeakPageCorrelation, Message, ReleaseCorrelation,
        WorkspaceLeakRecorded, WorkspaceRelease,
    };
    use uuid::Uuid;

    use super::{
        OUTBOUND_OPERATION_CAPACITY, RunnerConnectionAddress, RunnerConnectionBroker,
        RunnerConnectionBrokerError,
    };

    const ENROLLMENT: u128 = 0xa100;
    const OTHER_ENROLLMENT: u128 = 0xa101;
    const RUNNER: u128 = 0xa200;
    const OTHER_RUNNER: u128 = 0xa201;
    const SESSION: u128 = 0xa300;
    const MANIFEST: u128 = 0xa400;

    fn address() -> RunnerConnectionAddress {
        RunnerConnectionAddress::new(
            RunnerEnrollmentId::from_uuid(Uuid::from_u128(ENROLLMENT)),
            RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
            RunnerConnectionEpoch::try_from_u64(3).expect("the fixture epoch is positive"),
        )
    }

    fn other_address() -> RunnerConnectionAddress {
        RunnerConnectionAddress::new(
            RunnerEnrollmentId::from_uuid(Uuid::from_u128(OTHER_ENROLLMENT)),
            RunnerId::from_uuid(Uuid::from_u128(OTHER_RUNNER)),
            RunnerConnectionEpoch::try_from_u64(4).expect("the fixture epoch is positive"),
        )
    }

    fn release_for(runner: u128) -> Message {
        Message::WorkspaceRelease(WorkspaceRelease {
            correlation: ReleaseCorrelation {
                session_id: CanonicalUuid::from_uuid(Uuid::from_u128(SESSION)),
                placement_revision: signalbox_runner_wire::PositiveU64::try_new(4)
                    .expect("the fixture placement revision is positive"),
                runner_id: CanonicalUuid::from_uuid(Uuid::from_u128(runner)),
                manifest_id: CanonicalUuid::from_uuid(Uuid::from_u128(MANIFEST)),
            },
        })
    }

    #[tokio::test]
    async fn exact_connection_receives_the_enqueued_operation() {
        let broker = RunnerConnectionBroker::new();
        let mut attachment = broker
            .attach(address())
            .expect("the exact connection attaches once");
        let expected = release_for(RUNNER);

        broker
            .send(address(), expected.clone())
            .expect("the matching runner operation is queued");

        assert_eq!(attachment.receive().await, Some(expected));
    }

    #[tokio::test]
    async fn exact_connection_receives_address_scoped_leak_acknowledgement() {
        let broker = RunnerConnectionBroker::new();
        let mut attachment = broker
            .attach(address())
            .expect("the exact connection attaches once");
        let expected = Message::WorkspaceLeakRecorded(WorkspaceLeakRecorded {
            correlation: LeakPageCorrelation {
                registration_revision: signalbox_runner_wire::PositiveU64::try_new(4)
                    .expect("the fixture registration revision is positive"),
                report_digest: Digest::try_new("0".repeat(64))
                    .expect("the fixture report digest is canonical"),
                page: signalbox_runner_wire::PositiveU64::try_new(1)
                    .expect("the fixture page is positive"),
            },
            page_digest: Digest::try_new("1".repeat(64))
                .expect("the fixture page digest is canonical"),
        });

        broker
            .send(address(), expected.clone())
            .expect("the address-scoped acknowledgement is queued");

        assert_eq!(attachment.receive().await, Some(expected));
    }

    #[test]
    fn duplicate_exact_connection_address_is_rejected() {
        let broker = RunnerConnectionBroker::new();
        let _attachment = broker
            .attach(address())
            .expect("the first exact connection attaches");

        let duplicate = broker.attach(address());

        assert_eq!(
            duplicate.err(),
            Some(RunnerConnectionBrokerError::ConnectionAlreadyAttached)
        );
    }

    #[tokio::test]
    async fn dropped_attachment_retires_only_its_exact_route() {
        let broker = RunnerConnectionBroker::new();
        let attachment = broker
            .attach(address())
            .expect("the exact connection attaches");
        let mut retained_attachment = broker
            .attach(other_address())
            .expect("the other connection attaches");
        broker
            .send(address(), release_for(RUNNER))
            .expect("the attached route accepts an operation");

        drop(attachment);

        assert_eq!(
            broker.send(address(), release_for(RUNNER)),
            Err(RunnerConnectionBrokerError::ConnectionUnavailable)
        );
        let retained_operation = release_for(OTHER_RUNNER);
        broker
            .send(other_address(), retained_operation.clone())
            .expect("the other exact route remains attached");
        assert_eq!(
            retained_attachment.receive().await,
            Some(retained_operation)
        );
    }

    #[test]
    fn operation_for_another_runner_is_rejected_before_queueing() {
        let broker = RunnerConnectionBroker::new();
        let _attachment = broker
            .attach(address())
            .expect("the exact connection attaches");

        assert_eq!(
            broker.send(address(), release_for(OTHER_RUNNER)),
            Err(RunnerConnectionBrokerError::RunnerMismatch)
        );
    }

    #[test]
    fn bounded_connection_queue_reports_backpressure() {
        let broker = RunnerConnectionBroker::new();
        let _attachment = broker
            .attach(address())
            .expect("the exact connection attaches");
        let operation = release_for(RUNNER);
        broker
            .send(address(), operation.clone())
            .expect("queue member one is admitted");

        assert_eq!(OUTBOUND_OPERATION_CAPACITY, 1);
        assert_eq!(
            broker.send(address(), operation),
            Err(RunnerConnectionBrokerError::QueueFull)
        );
    }

    #[test]
    fn lifecycle_frame_is_not_an_outbound_broker_operation() {
        let broker = RunnerConnectionBroker::new();
        let _attachment = broker
            .attach(address())
            .expect("the exact connection attaches");

        assert_eq!(
            broker.send(
                address(),
                Message::Heartbeat(signalbox_runner_wire::Heartbeat {
                    sequence: signalbox_runner_wire::PositiveU64::try_new(1)
                        .expect("the fixture sequence is positive"),
                    last_accepted_peer_sequence: 0,
                })
            ),
            Err(RunnerConnectionBrokerError::UnsupportedMessage)
        );
    }
}

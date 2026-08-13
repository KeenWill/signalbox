//! Atomic authorization boundary for dispatch through an existing runner pin.

use std::future::Future;

use signalbox_domain::{
    RunnerEnrollmentId, RunnerGeneration, RunnerLease, RunnerLeaseId, SessionId, ToolAttemptId,
    TurnId,
};

/// Exact prepared attempt and frozen runner locus selected for dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedRunnerDispatchRequest {
    session: SessionId,
    turn: TurnId,
    attempt: ToolAttemptId,
    enrollment: RunnerEnrollmentId,
    registration_revision: RunnerGeneration,
}

impl PinnedRunnerDispatchRequest {
    /// Binds one prepared attempt to the exact runner registration selected by
    /// its executable-tool snapshot.
    pub const fn new(
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        enrollment: RunnerEnrollmentId,
        registration_revision: RunnerGeneration,
    ) -> Self {
        Self {
            session,
            turn,
            attempt,
            enrollment,
            registration_revision,
        }
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the active logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the prepared physical attempt.
    pub const fn attempt(&self) -> ToolAttemptId {
        self.attempt
    }

    /// Returns the exact selected enrollment.
    pub const fn enrollment(&self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the frozen registration revision.
    pub const fn registration_revision(&self) -> RunnerGeneration {
        self.registration_revision
    }
}

/// Supplies fresh lease identities before transaction entry.
pub trait RunnerLeaseIdGenerator {
    /// Returns one fresh logical lease identity.
    fn next_lease_id(&mut self) -> RunnerLeaseId;
}

/// Production UUIDv7 generator for logical runner leases.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7RunnerLeaseIdGenerator;

impl RunnerLeaseIdGenerator for UuidV7RunnerLeaseIdGenerator {
    fn next_lease_id(&mut self) -> RunnerLeaseId {
        RunnerLeaseId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Atomic durable authorization for one prepared attempt on an existing pin.
pub trait PinnedRunnerDispatchTransaction {
    /// Adapter-specific transaction failure.
    type Error;

    /// Atomically marks the attempt in flight and stores its offered lease.
    fn authorize(
        &mut self,
        request: PinnedRunnerDispatchRequest,
        lease: RunnerLeaseId,
    ) -> impl Future<Output = Result<RunnerLease, Self::Error>> + Send;
}

/// Allocates a lease identity and delegates the complete durable transition.
#[derive(Debug)]
pub struct PinnedRunnerDispatchService<Transaction, Ids> {
    transaction: Transaction,
    ids: Ids,
}

impl<Transaction, Ids> PinnedRunnerDispatchService<Transaction, Ids> {
    /// Uses the supplied durable boundary and lease-identity source.
    pub const fn new(transaction: Transaction, ids: Ids) -> Self {
        Self { transaction, ids }
    }
}

impl<Transaction, Ids> PinnedRunnerDispatchService<Transaction, Ids>
where
    Transaction: PinnedRunnerDispatchTransaction,
    Ids: RunnerLeaseIdGenerator,
{
    /// Authorizes one exact pinned runner dispatch.
    pub async fn execute(
        &mut self,
        request: PinnedRunnerDispatchRequest,
    ) -> Result<RunnerLease, Transaction::Error> {
        let lease = self.ids.next_lease_id();
        self.transaction.authorize(request, lease).await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, future::ready};

    use signalbox_domain::{
        RunnerEnrollmentId, RunnerGeneration, RunnerLease, RunnerLeaseId, SessionId, ToolAttemptId,
        TurnId,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        PinnedRunnerDispatchRequest, PinnedRunnerDispatchService, PinnedRunnerDispatchTransaction,
        RunnerLeaseIdGenerator, UuidV7RunnerLeaseIdGenerator,
    };

    const SESSION: u128 = 1;
    const TURN: u128 = 2;
    const ATTEMPT: u128 = 3;
    const ENROLLMENT: u128 = 4;
    const LEASE: u128 = 5;

    #[derive(Debug)]
    struct ScriptedIds {
        leases: VecDeque<RunnerLeaseId>,
    }

    impl RunnerLeaseIdGenerator for ScriptedIds {
        fn next_lease_id(&mut self) -> RunnerLeaseId {
            self.leases
                .pop_front()
                .expect("the service requests one scripted lease identity")
        }
    }

    #[derive(Debug)]
    struct RejectingTransaction;

    impl PinnedRunnerDispatchTransaction for RejectingTransaction {
        type Error = &'static str;

        fn authorize(
            &mut self,
            _request: PinnedRunnerDispatchRequest,
            _lease: RunnerLeaseId,
        ) -> impl Future<Output = Result<RunnerLease, Self::Error>> + Send {
            ready(Err("rejected"))
        }
    }

    fn request() -> PinnedRunnerDispatchRequest {
        PinnedRunnerDispatchRequest::new(
            SessionId::from_uuid(Uuid::from_u128(SESSION)),
            TurnId::from_uuid(Uuid::from_u128(TURN)),
            ToolAttemptId::from_uuid(Uuid::from_u128(ATTEMPT)),
            RunnerEnrollmentId::from_uuid(Uuid::from_u128(ENROLLMENT)),
            RunnerGeneration::one(),
        )
    }

    #[tokio::test]
    async fn service_propagates_the_transaction_refusal() {
        let mut service = PinnedRunnerDispatchService::new(
            RejectingTransaction,
            ScriptedIds {
                leases: VecDeque::from([RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE))]),
            },
        );

        assert_eq!(service.execute(request()).await, Err("rejected"));
    }

    #[test]
    fn production_lease_ids_are_uuid_v7_rfc4122() {
        let lease = UuidV7RunnerLeaseIdGenerator.next_lease_id().into_uuid();

        assert_eq!(lease.get_version(), Some(Version::SortRand));
        assert_eq!(lease.get_variant(), Variant::RFC4122);
    }
}

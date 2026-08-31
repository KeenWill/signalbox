//! Atomic authorization boundary for dispatch through an existing runner pin.

use std::future::Future;

use signalbox_domain::{
    RunnerEnrollmentId, RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseId, SessionId,
    ToolAttemptId, TurnId,
};

/// Exact prepared attempt and frozen runner locus selected for dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedRunnerDispatchRequest {
    session: SessionId,
    turn: TurnId,
    attempt: ToolAttemptId,
    runner: RunnerId,
    registration_revision: RunnerGeneration,
}

impl PinnedRunnerDispatchRequest {
    /// Binds one prepared attempt to the exact runner registration selected by
    /// its executable-tool snapshot.
    pub const fn new(
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        runner: RunnerId,
        registration_revision: RunnerGeneration,
    ) -> Self {
        Self {
            session,
            turn,
            attempt,
            runner,
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

    /// Returns the exact selected runner.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the frozen registration revision.
    pub const fn registration_revision(&self) -> RunnerGeneration {
        self.registration_revision
    }
}

/// Durable offer authority plus the enrollment needed for socket routing.
#[derive(Debug, Eq, PartialEq)]
pub struct PinnedRunnerLeaseOffer {
    enrollment: RunnerEnrollmentId,
    lease: RunnerLease,
}

/// Exact prepared attempt and frozen runner locus selected for the first pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitialRunnerDispatchRequest {
    session: SessionId,
    turn: TurnId,
    attempt: ToolAttemptId,
    runner: RunnerId,
    registration_revision: RunnerGeneration,
}

/// Closed durable authorization request for one runner lease offer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerDispatchRequest {
    /// The first offer also installs the session placement pin.
    Initial(InitialRunnerDispatchRequest),
    /// The session already carries the exact pinned runner placement.
    Pinned(PinnedRunnerDispatchRequest),
}

impl RunnerDispatchRequest {
    /// Returns the owning session.
    pub const fn session(self) -> SessionId {
        match self {
            Self::Initial(request) => request.session(),
            Self::Pinned(request) => request.session(),
        }
    }

    /// Returns the active logical turn.
    pub const fn turn(self) -> TurnId {
        match self {
            Self::Initial(request) => request.turn(),
            Self::Pinned(request) => request.turn(),
        }
    }

    /// Returns the prepared physical attempt.
    pub const fn attempt(self) -> ToolAttemptId {
        match self {
            Self::Initial(request) => request.attempt(),
            Self::Pinned(request) => request.attempt(),
        }
    }

    /// Returns the exact selected runner.
    pub const fn runner(self) -> RunnerId {
        match self {
            Self::Initial(request) => request.runner(),
            Self::Pinned(request) => request.runner(),
        }
    }

    /// Returns the frozen registration revision.
    pub const fn registration_revision(self) -> RunnerGeneration {
        match self {
            Self::Initial(request) => request.registration_revision(),
            Self::Pinned(request) => request.registration_revision(),
        }
    }
}

impl From<InitialRunnerDispatchRequest> for RunnerDispatchRequest {
    fn from(request: InitialRunnerDispatchRequest) -> Self {
        Self::Initial(request)
    }
}

impl From<PinnedRunnerDispatchRequest> for RunnerDispatchRequest {
    fn from(request: PinnedRunnerDispatchRequest) -> Self {
        Self::Pinned(request)
    }
}

impl InitialRunnerDispatchRequest {
    /// Binds one prepared attempt to an exact pre-pin runner registration.
    pub const fn new(
        session: SessionId,
        turn: TurnId,
        attempt: ToolAttemptId,
        runner: RunnerId,
        registration_revision: RunnerGeneration,
    ) -> Self {
        Self {
            session,
            turn,
            attempt,
            runner,
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

    /// Returns the exact selected runner.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the frozen registration revision.
    pub const fn registration_revision(&self) -> RunnerGeneration {
        self.registration_revision
    }
}

impl PinnedRunnerLeaseOffer {
    /// Binds one committed lease to the enrollment that authorized it.
    pub const fn new(enrollment: RunnerEnrollmentId, lease: RunnerLease) -> Self {
        Self { enrollment, lease }
    }

    /// Returns the enrollment whose current connection may receive the offer.
    pub const fn enrollment(&self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Borrows the canonical committed lease.
    pub const fn lease(&self) -> &RunnerLease {
        &self.lease
    }

    /// Returns the routing identity and canonical committed lease.
    pub fn into_parts(self) -> (RunnerEnrollmentId, RunnerLease) {
        (self.enrollment, self.lease)
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
    ) -> impl Future<Output = Result<PinnedRunnerLeaseOffer, Self::Error>> + Send;
}

/// Atomic durable authorization for a workspace-free exact-directory first pin.
pub trait InitialRunnerDispatchTransaction {
    /// Adapter-specific transaction failure.
    type Error;

    /// Atomically pins the placement, marks the attempt in flight, and offers its lease.
    fn authorize_initial(
        &mut self,
        request: InitialRunnerDispatchRequest,
        lease: RunnerLeaseId,
    ) -> impl Future<Output = Result<PinnedRunnerLeaseOffer, Self::Error>> + Send;
}

/// Allocates a lease identity and delegates the complete durable transition.
#[derive(Debug)]
pub struct PinnedRunnerDispatchService<Transaction, Ids> {
    transaction: Transaction,
    ids: Ids,
}

/// Allocates lease identities for the exact-directory initial-pin transaction.
#[derive(Debug)]
pub struct InitialRunnerDispatchService<Transaction, Ids> {
    transaction: Transaction,
    ids: Ids,
}

/// Selects the exact initial-pin or existing-pin durable offer transaction.
#[derive(Debug)]
pub struct RunnerDispatchService<Transaction, Ids> {
    transaction: Transaction,
    ids: Ids,
}

impl<Transaction, Ids> RunnerDispatchService<Transaction, Ids> {
    /// Uses one shared durable adapter and lease-identity source for both paths.
    pub const fn new(transaction: Transaction, ids: Ids) -> Self {
        Self { transaction, ids }
    }
}

impl<Transaction, Ids> RunnerDispatchService<Transaction, Ids>
where
    Transaction: InitialRunnerDispatchTransaction
        + PinnedRunnerDispatchTransaction<
            Error = <Transaction as InitialRunnerDispatchTransaction>::Error,
        >,
    Ids: RunnerLeaseIdGenerator,
{
    /// Authorizes one first-pin or existing-pin runner dispatch.
    pub async fn execute(
        &mut self,
        request: RunnerDispatchRequest,
    ) -> Result<PinnedRunnerLeaseOffer, <Transaction as InitialRunnerDispatchTransaction>::Error>
    {
        let lease = self.ids.next_lease_id();
        match request {
            RunnerDispatchRequest::Initial(request) => {
                self.transaction.authorize_initial(request, lease).await
            }
            RunnerDispatchRequest::Pinned(request) => {
                self.transaction.authorize(request, lease).await
            }
        }
    }
}

impl<Transaction, Ids> InitialRunnerDispatchService<Transaction, Ids> {
    /// Uses the supplied durable boundary and lease-identity source.
    pub const fn new(transaction: Transaction, ids: Ids) -> Self {
        Self { transaction, ids }
    }
}

impl<Transaction, Ids> InitialRunnerDispatchService<Transaction, Ids>
where
    Transaction: InitialRunnerDispatchTransaction,
    Ids: RunnerLeaseIdGenerator,
{
    /// Authorizes one exact initial runner dispatch.
    pub async fn execute(
        &mut self,
        request: InitialRunnerDispatchRequest,
    ) -> Result<PinnedRunnerLeaseOffer, Transaction::Error> {
        let lease = self.ids.next_lease_id();
        self.transaction.authorize_initial(request, lease).await
    }
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
    ) -> Result<PinnedRunnerLeaseOffer, Transaction::Error> {
        let lease = self.ids.next_lease_id();
        self.transaction.authorize(request, lease).await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, future::ready};

    use signalbox_domain::{
        RunnerGeneration, RunnerId, RunnerLeaseId, SessionId, ToolAttemptId, TurnId,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        InitialRunnerDispatchRequest, InitialRunnerDispatchService,
        InitialRunnerDispatchTransaction, PinnedRunnerDispatchRequest, PinnedRunnerDispatchService,
        PinnedRunnerDispatchTransaction, PinnedRunnerLeaseOffer, RunnerDispatchRequest,
        RunnerDispatchService, RunnerLeaseIdGenerator, UuidV7RunnerLeaseIdGenerator,
    };

    const SESSION: u128 = 1;
    const TURN: u128 = 2;
    const ATTEMPT: u128 = 3;
    const RUNNER: u128 = 4;
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
        ) -> impl Future<Output = Result<PinnedRunnerLeaseOffer, Self::Error>> + Send {
            ready(Err("rejected"))
        }
    }

    #[derive(Debug)]
    struct RejectingInitialTransaction;

    impl InitialRunnerDispatchTransaction for RejectingInitialTransaction {
        type Error = &'static str;

        fn authorize_initial(
            &mut self,
            _request: InitialRunnerDispatchRequest,
            _lease: RunnerLeaseId,
        ) -> impl Future<Output = Result<PinnedRunnerLeaseOffer, Self::Error>> + Send {
            ready(Err("initial rejected"))
        }
    }

    #[derive(Debug)]
    struct RejectingCombinedTransaction;

    impl InitialRunnerDispatchTransaction for RejectingCombinedTransaction {
        type Error = &'static str;

        fn authorize_initial(
            &mut self,
            _request: InitialRunnerDispatchRequest,
            _lease: RunnerLeaseId,
        ) -> impl Future<Output = Result<PinnedRunnerLeaseOffer, Self::Error>> + Send {
            ready(Err("initial selected"))
        }
    }

    impl PinnedRunnerDispatchTransaction for RejectingCombinedTransaction {
        type Error = &'static str;

        fn authorize(
            &mut self,
            _request: PinnedRunnerDispatchRequest,
            _lease: RunnerLeaseId,
        ) -> impl Future<Output = Result<PinnedRunnerLeaseOffer, Self::Error>> + Send {
            ready(Err("pinned selected"))
        }
    }

    fn request() -> PinnedRunnerDispatchRequest {
        PinnedRunnerDispatchRequest::new(
            SessionId::from_uuid(Uuid::from_u128(SESSION)),
            TurnId::from_uuid(Uuid::from_u128(TURN)),
            ToolAttemptId::from_uuid(Uuid::from_u128(ATTEMPT)),
            RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
            RunnerGeneration::one(),
        )
    }

    fn initial_request() -> InitialRunnerDispatchRequest {
        InitialRunnerDispatchRequest::new(
            SessionId::from_uuid(Uuid::from_u128(SESSION)),
            TurnId::from_uuid(Uuid::from_u128(TURN)),
            ToolAttemptId::from_uuid(Uuid::from_u128(ATTEMPT)),
            RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
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

    #[tokio::test]
    async fn initial_service_propagates_the_transaction_refusal() {
        let mut service = InitialRunnerDispatchService::new(
            RejectingInitialTransaction,
            ScriptedIds {
                leases: VecDeque::from([RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE))]),
            },
        );

        assert_eq!(
            service.execute(initial_request()).await,
            Err("initial rejected")
        );
    }

    #[tokio::test]
    async fn combined_service_selects_the_initial_transaction() {
        let mut service = RunnerDispatchService::new(
            RejectingCombinedTransaction,
            ScriptedIds {
                leases: VecDeque::from([RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE))]),
            },
        );

        assert_eq!(
            service
                .execute(RunnerDispatchRequest::Initial(initial_request()))
                .await,
            Err("initial selected")
        );
    }

    #[tokio::test]
    async fn combined_service_selects_the_pinned_transaction() {
        let mut service = RunnerDispatchService::new(
            RejectingCombinedTransaction,
            ScriptedIds {
                leases: VecDeque::from([RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE))]),
            },
        );

        assert_eq!(
            service
                .execute(RunnerDispatchRequest::Pinned(request()))
                .await,
            Err("pinned selected")
        );
    }

    #[test]
    fn production_lease_ids_are_uuid_v7_rfc4122() {
        let lease = UuidV7RunnerLeaseIdGenerator.next_lease_id().into_uuid();

        assert_eq!(lease.get_version(), Some(Version::SortRand));
        assert_eq!(lease.get_variant(), Variant::RFC4122);
    }
}

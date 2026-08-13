//! Application runner-offer handoff backed by durable daemon dispatch.

use std::{fmt, future::Future, pin::Pin};

use signalbox_application::{
    ClassifyOperatorFailure, InitialRunnerDispatchRequest, OperatorFailureClass,
    PinnedRunnerDispatchRequest, RunnerDispatchRequest, RunnerToolOffer, RunnerToolOfferError,
    RunnerToolOfferReceipt, RunnerToolOfferRequest, RunnerToolOfferStatus,
};
use signalbox_domain::{
    RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseId, SelectedToolExecutionLocus, SessionId,
    SessionRunnerPlacementState, ToolAttemptId, TurnId,
};
use signalbox_persistence::runner_protocol::{RunnerProtocolStore, RunnerProtocolStoreError};

use crate::{
    runner_connection_broker::RunnerConnectionBroker,
    runner_dispatch::{
        RunnerLeaseOfferAuthority, RunnerLeaseOfferDispatchError, RunnerLeaseOfferDispatcher,
        RunnerLeaseOfferRouteSource, RunnerLeaseOfferTransport,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunnerToolOfferPlacementStatus {
    Unpinned,
    Pinned,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DurableRunnerOfferEvidence {
    session: SessionId,
    turn: TurnId,
    attempt: ToolAttemptId,
    runner: RunnerId,
    registration_revision: RunnerGeneration,
    lease: RunnerLeaseId,
}

impl DurableRunnerOfferEvidence {
    fn from_lease(lease: &RunnerLease) -> Self {
        let correlation = lease.correlation();
        Self {
            session: correlation.dispatch.session(),
            turn: correlation.dispatch.turn(),
            attempt: correlation.dispatch.attempt(),
            runner: correlation.runner,
            registration_revision: correlation.registration_revision,
            lease: correlation.lease,
        }
    }
}

type RunnerToolOfferPlacementFuture<'a, Error> = Pin<
    Box<dyn Future<Output = Result<Option<RunnerToolOfferPlacementStatus>, Error>> + Send + 'a>,
>;
type RunnerToolOfferRereadFuture<'a, Error> =
    Pin<Box<dyn Future<Output = Result<Option<DurableRunnerOfferEvidence>, Error>> + Send + 'a>>;

trait RunnerToolOfferStateSource {
    type Error: ClassifyOperatorFailure;

    fn placement_status(
        &self,
        session: SessionId,
    ) -> RunnerToolOfferPlacementFuture<'_, Self::Error>;

    fn current_offer(&self, attempt: ToolAttemptId)
    -> RunnerToolOfferRereadFuture<'_, Self::Error>;
}

impl RunnerToolOfferStateSource for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    fn placement_status(
        &self,
        session: SessionId,
    ) -> RunnerToolOfferPlacementFuture<'_, Self::Error> {
        Box::pin(async move {
            Ok(self
                .load_placement(session)
                .await?
                .map(|stored| match stored.placement().state() {
                    SessionRunnerPlacementState::Unpinned => {
                        RunnerToolOfferPlacementStatus::Unpinned
                    }
                    SessionRunnerPlacementState::Pinned(_) => {
                        RunnerToolOfferPlacementStatus::Pinned
                    }
                    SessionRunnerPlacementState::RunnerLostBeforePin(_)
                    | SessionRunnerPlacementState::RunnerLost(_)
                    | SessionRunnerPlacementState::RunnerAbandoned(_) => {
                        RunnerToolOfferPlacementStatus::Unavailable
                    }
                }))
        })
    }

    fn current_offer(
        &self,
        attempt: ToolAttemptId,
    ) -> RunnerToolOfferRereadFuture<'_, Self::Error> {
        Box::pin(async move {
            Ok(self
                .load_current_lease_for_attempt(attempt)
                .await?
                .as_ref()
                .map(DurableRunnerOfferEvidence::from_lease))
        })
    }
}

type RunnerToolOfferDispatchFuture<'a, Error> =
    Pin<Box<dyn Future<Output = Result<DurableRunnerOfferEvidence, Error>> + Send + 'a>>;

trait RunnerToolOfferDispatch {
    type Error: ClassifyOperatorFailure;

    fn dispatch(
        &mut self,
        request: RunnerDispatchRequest,
    ) -> RunnerToolOfferDispatchFuture<'_, Self::Error>;
}

impl<Authority, Routes, Transport> RunnerToolOfferDispatch
    for RunnerLeaseOfferDispatcher<Authority, Routes, Transport>
where
    Authority: RunnerLeaseOfferAuthority + Send,
    Authority::Error: ClassifyOperatorFailure,
    Routes: RunnerLeaseOfferRouteSource + Send,
    Routes::Error: fmt::Display,
    Transport: RunnerLeaseOfferTransport + Send,
    Transport::Error: fmt::Display,
{
    type Error = RunnerLeaseOfferDispatchError<Authority::Error>;

    fn dispatch(
        &mut self,
        request: RunnerDispatchRequest,
    ) -> RunnerToolOfferDispatchFuture<'_, Self::Error> {
        Box::pin(async move {
            let outcome = RunnerLeaseOfferDispatcher::dispatch(self, request).await?;
            Ok(DurableRunnerOfferEvidence::from_lease(outcome.lease()))
        })
    }
}

#[derive(Debug)]
struct RunnerToolOfferAdapter<State, Dispatch> {
    state: State,
    dispatch: Dispatch,
}

impl<State, Dispatch> RunnerToolOfferAdapter<State, Dispatch>
where
    State: RunnerToolOfferStateSource + Send,
    Dispatch: RunnerToolOfferDispatch + Send,
{
    async fn offer(
        &mut self,
        request: RunnerToolOfferRequest,
    ) -> Result<RunnerToolOfferReceipt, RunnerToolOfferError> {
        let (runner, registration_revision) = exact_runner_locus(&request)?;
        let placement = self
            .state
            .placement_status(request.session())
            .await
            .map_err(|error| state_error(&error, "runner_tool_offer_placement_load"))?
            .ok_or_else(|| invariant_error("runner_tool_offer_placement_missing"))?;
        let dispatch = match placement {
            RunnerToolOfferPlacementStatus::Unpinned => InitialRunnerDispatchRequest::new(
                request.session(),
                request.turn(),
                request.attempt(),
                runner,
                registration_revision,
            )
            .into(),
            RunnerToolOfferPlacementStatus::Pinned => PinnedRunnerDispatchRequest::new(
                request.session(),
                request.turn(),
                request.attempt(),
                runner,
                registration_revision,
            )
            .into(),
            RunnerToolOfferPlacementStatus::Unavailable => {
                return Err(invariant_error("runner_tool_offer_placement_unavailable"));
            }
        };
        let evidence = self
            .dispatch
            .dispatch(dispatch)
            .await
            .map_err(|error| state_error(&error, "runner_tool_offer_dispatch"))?;
        receipt(request, evidence)
    }

    async fn reread(
        &mut self,
        request: &RunnerToolOfferRequest,
    ) -> Result<RunnerToolOfferStatus, RunnerToolOfferError> {
        exact_runner_locus(request)?;
        let evidence = self
            .state
            .current_offer(request.attempt())
            .await
            .map_err(|error| state_error(&error, "runner_tool_offer_lease_reread"))?;
        match evidence {
            Some(evidence) => {
                receipt(request.clone(), evidence).map(RunnerToolOfferStatus::Offered)
            }
            // The caller retains the exact turn dispatch gate. Runner offer
            // authorization changes the attempt to InFlight and inserts its
            // lease binding in one transaction, so no binding under that gate
            // is authenticated non-consumption of this offer.
            None => Ok(RunnerToolOfferStatus::Prepared),
        }
    }
}

/// PostgreSQL-backed exact-runner offer handoff for tool-loop composition.
#[derive(Clone, Debug)]
pub struct PostgresRunnerToolOffer {
    store: RunnerProtocolStore,
    broker: RunnerConnectionBroker,
}

impl PostgresRunnerToolOffer {
    /// Shares durable runner state and the established-connection broker.
    pub fn new(store: RunnerProtocolStore, broker: RunnerConnectionBroker) -> Self {
        Self { store, broker }
    }
}

impl RunnerToolOffer for PostgresRunnerToolOffer {
    fn offer(
        &mut self,
        request: RunnerToolOfferRequest,
    ) -> impl Future<Output = Result<RunnerToolOfferReceipt, RunnerToolOfferError>> + Send {
        let store = self.store.clone();
        let broker = self.broker.clone();
        async move {
            RunnerToolOfferAdapter {
                state: store.clone(),
                dispatch: RunnerLeaseOfferDispatcher::postgres(store, broker),
            }
            .offer(request)
            .await
        }
    }

    fn reread(
        &mut self,
        request: &RunnerToolOfferRequest,
    ) -> impl Future<Output = Result<RunnerToolOfferStatus, RunnerToolOfferError>> + Send {
        let store = self.store.clone();
        let broker = self.broker.clone();
        let request = request.clone();
        async move {
            RunnerToolOfferAdapter {
                state: store.clone(),
                dispatch: RunnerLeaseOfferDispatcher::postgres(store, broker),
            }
            .reread(&request)
            .await
        }
    }
}

fn exact_runner_locus(
    request: &RunnerToolOfferRequest,
) -> Result<(RunnerId, RunnerGeneration), RunnerToolOfferError> {
    match request.execution_locus() {
        SelectedToolExecutionLocus::ExactRunner {
            runner,
            registration_revision,
        } => Ok((*runner, *registration_revision)),
        SelectedToolExecutionLocus::RunnerCapabilityClass { .. } => Err(invariant_error(
            "runner_tool_offer_capability_selection_unavailable",
        )),
        SelectedToolExecutionLocus::Daemon => {
            Err(invariant_error("runner_tool_offer_daemon_locus"))
        }
    }
}

fn receipt(
    request: RunnerToolOfferRequest,
    evidence: DurableRunnerOfferEvidence,
) -> Result<RunnerToolOfferReceipt, RunnerToolOfferError> {
    let (runner, registration_revision) = exact_runner_locus(&request)?;
    if evidence.session != request.session()
        || evidence.turn != request.turn()
        || evidence.attempt != request.attempt()
        || evidence.runner != runner
        || evidence.registration_revision != registration_revision
    {
        return Err(invariant_error("runner_tool_offer_evidence_mismatch"));
    }
    Ok(RunnerToolOfferReceipt::new(request, evidence.lease))
}

fn state_error(
    error: &impl ClassifyOperatorFailure,
    cause_code: &'static str,
) -> RunnerToolOfferError {
    RunnerToolOfferError::new(error.operator_failure_class(), cause_code)
}

const fn invariant_error(cause_code: &'static str) -> RunnerToolOfferError {
    RunnerToolOfferError::new(OperatorFailureClass::CallerOrHubBug, cause_code)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, error::Error};

    use signalbox_application::ClassifyOperatorFailure;
    use signalbox_domain::{RunnerCapabilityClass, RunnerDomainError};
    use uuid::Uuid;

    use super::*;

    const SESSION: u128 = 1;
    const TURN: u128 = 2;
    const ATTEMPT: u128 = 3;
    const RUNNER: u128 = 4;
    const LEASE: u128 = 5;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        Infrastructure,
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fixture runner offer failure")
        }
    }

    impl Error for FakeError {}

    impl ClassifyOperatorFailure for FakeError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        }

        fn operator_failure_cause_code(&self) -> &'static str {
            "fixture_runner_offer"
        }
    }

    #[derive(Debug)]
    struct FakeState {
        placements: VecDeque<Result<Option<RunnerToolOfferPlacementStatus>, FakeError>>,
        offers: VecDeque<Result<Option<DurableRunnerOfferEvidence>, FakeError>>,
    }

    impl RunnerToolOfferStateSource for FakeState {
        type Error = FakeError;

        fn placement_status(
            &self,
            _session: SessionId,
        ) -> RunnerToolOfferPlacementFuture<'_, Self::Error> {
            let result = self
                .placements
                .front()
                .copied()
                .expect("the fixture supplies one placement result");
            Box::pin(std::future::ready(result))
        }

        fn current_offer(
            &self,
            _attempt: ToolAttemptId,
        ) -> RunnerToolOfferRereadFuture<'_, Self::Error> {
            let result = self
                .offers
                .front()
                .copied()
                .expect("the fixture supplies one offer result");
            Box::pin(std::future::ready(result))
        }
    }

    #[derive(Debug)]
    struct FakeDispatch {
        requests: Vec<RunnerDispatchRequest>,
        results: VecDeque<Result<DurableRunnerOfferEvidence, FakeError>>,
    }

    impl RunnerToolOfferDispatch for FakeDispatch {
        type Error = FakeError;

        fn dispatch(
            &mut self,
            request: RunnerDispatchRequest,
        ) -> RunnerToolOfferDispatchFuture<'_, Self::Error> {
            self.requests.push(request);
            Box::pin(std::future::ready(
                self.results
                    .pop_front()
                    .expect("the fixture supplies one dispatch result"),
            ))
        }
    }

    fn request(locus: SelectedToolExecutionLocus) -> RunnerToolOfferRequest {
        RunnerToolOfferRequest::try_new(
            SessionId::from_uuid(Uuid::from_u128(SESSION)),
            TurnId::from_uuid(Uuid::from_u128(TURN)),
            ToolAttemptId::from_uuid(Uuid::from_u128(ATTEMPT)),
            locus,
        )
        .expect("the fixture locus selects runner execution")
    }

    fn exact_request() -> RunnerToolOfferRequest {
        request(SelectedToolExecutionLocus::ExactRunner {
            runner: RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
            registration_revision: RunnerGeneration::one(),
        })
    }

    fn evidence() -> DurableRunnerOfferEvidence {
        DurableRunnerOfferEvidence {
            session: SessionId::from_uuid(Uuid::from_u128(SESSION)),
            turn: TurnId::from_uuid(Uuid::from_u128(TURN)),
            attempt: ToolAttemptId::from_uuid(Uuid::from_u128(ATTEMPT)),
            runner: RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
            registration_revision: RunnerGeneration::one(),
            lease: RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE)),
        }
    }

    fn adapter(
        placement: RunnerToolOfferPlacementStatus,
        dispatch: Result<DurableRunnerOfferEvidence, FakeError>,
    ) -> RunnerToolOfferAdapter<FakeState, FakeDispatch> {
        RunnerToolOfferAdapter {
            state: FakeState {
                placements: VecDeque::from([Ok(Some(placement))]),
                offers: VecDeque::from([Ok(Some(evidence()))]),
            },
            dispatch: FakeDispatch {
                requests: Vec::new(),
                results: VecDeque::from([dispatch]),
            },
        }
    }

    /// INV-024 / INV-043: an unpinned exact locus selects the atomic initial
    /// dispatch and returns only its authenticated durable receipt.
    #[tokio::test]
    async fn inv024_inv043_unpinned_exact_locus_selects_initial_dispatch() {
        let application_request = exact_request();
        let mut adapter = adapter(RunnerToolOfferPlacementStatus::Unpinned, Ok(evidence()));

        let receipt = adapter
            .offer(application_request.clone())
            .await
            .expect("the exact durable offer commits");

        assert_eq!(receipt.request(), &application_request);
        assert_eq!(
            receipt.lease(),
            RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE))
        );
        assert_eq!(
            adapter.dispatch.requests,
            [RunnerDispatchRequest::Initial(
                InitialRunnerDispatchRequest::new(
                    application_request.session(),
                    application_request.turn(),
                    application_request.attempt(),
                    RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
                    RunnerGeneration::one(),
                )
            )]
        );
    }

    /// INV-024 / INV-043: a pinned exact locus selects the existing-pin
    /// transaction rather than repeating initial placement.
    #[tokio::test]
    async fn inv024_inv043_pinned_exact_locus_selects_pinned_dispatch() {
        let application_request = exact_request();
        let mut adapter = adapter(RunnerToolOfferPlacementStatus::Pinned, Ok(evidence()));

        adapter
            .offer(application_request.clone())
            .await
            .expect("the exact durable offer commits");

        assert_eq!(
            adapter.dispatch.requests,
            [RunnerDispatchRequest::Pinned(
                PinnedRunnerDispatchRequest::new(
                    application_request.session(),
                    application_request.turn(),
                    application_request.attempt(),
                    RunnerId::from_uuid(Uuid::from_u128(RUNNER)),
                    RunnerGeneration::one(),
                )
            )]
        );
    }

    #[tokio::test]
    async fn capability_class_waits_for_its_committed_selection_boundary() {
        let class = RunnerCapabilityClass::try_new("linux.workspace".to_owned())
            .expect("the fixture class is valid");
        let application_request =
            request(SelectedToolExecutionLocus::RunnerCapabilityClass { class });
        let mut adapter = adapter(RunnerToolOfferPlacementStatus::Unpinned, Ok(evidence()));

        let error = adapter
            .offer(application_request)
            .await
            .expect_err("this slice does not choose a capability-class runner");

        assert_eq!(
            error.operator_failure_cause_code(),
            "runner_tool_offer_capability_selection_unavailable"
        );
        assert!(adapter.dispatch.requests.is_empty());
    }

    #[tokio::test]
    async fn lost_placement_fails_before_dispatch() {
        let mut adapter = adapter(RunnerToolOfferPlacementStatus::Unavailable, Ok(evidence()));

        let error = adapter
            .offer(exact_request())
            .await
            .expect_err("a lost placement cannot mint another offer");

        assert_eq!(
            error.operator_failure_cause_code(),
            "runner_tool_offer_placement_unavailable"
        );
        assert!(adapter.dispatch.requests.is_empty());
    }

    /// INV-011 / INV-024: durable reread accepts only an exact offer and never
    /// repeats dispatch.
    #[tokio::test]
    async fn inv011_inv024_reread_returns_exact_durable_offer() {
        let application_request = exact_request();
        let mut adapter = adapter(RunnerToolOfferPlacementStatus::Pinned, Ok(evidence()));

        let status = adapter
            .reread(&application_request)
            .await
            .expect("the durable offer reads back");

        assert_eq!(
            status,
            RunnerToolOfferStatus::Offered(RunnerToolOfferReceipt::new(
                application_request,
                RunnerLeaseId::from_uuid(Uuid::from_u128(LEASE)),
            ))
        );
        assert!(adapter.dispatch.requests.is_empty());
    }

    /// INV-011 / INV-024: absent durable offer evidence proves the exact
    /// attempt remains eligible for a later retry.
    #[tokio::test]
    async fn inv011_inv024_absent_reread_returns_prepared() {
        let mut adapter = adapter(RunnerToolOfferPlacementStatus::Pinned, Ok(evidence()));
        adapter.state.offers = VecDeque::from([Ok(None)]);

        let status = adapter
            .reread(&exact_request())
            .await
            .expect("absence is an authenticated non-consumption result");

        assert_eq!(status, RunnerToolOfferStatus::Prepared);
        assert!(adapter.dispatch.requests.is_empty());
    }

    /// INV-011 / INV-024: a durable lease for another physical attempt cannot
    /// satisfy offer reconciliation.
    #[tokio::test]
    async fn inv011_inv024_cross_wired_reread_fails_closed() {
        let mut foreign = evidence();
        foreign.attempt = ToolAttemptId::from_uuid(Uuid::from_u128(ATTEMPT + 1));
        let mut adapter = adapter(RunnerToolOfferPlacementStatus::Pinned, Ok(evidence()));
        adapter.state.offers = VecDeque::from([Ok(Some(foreign))]);

        let error = adapter
            .reread(&exact_request())
            .await
            .expect_err("cross-wired durable evidence is rejected");

        assert_eq!(
            error.operator_failure_cause_code(),
            "runner_tool_offer_evidence_mismatch"
        );
    }

    /// INV-011 / INV-024: durable evidence from another turn cannot satisfy
    /// offer reconciliation even when every other identity matches.
    #[tokio::test]
    async fn inv011_inv024_cross_wired_turn_reread_fails_closed() {
        let mut foreign = evidence();
        foreign.turn = TurnId::from_uuid(Uuid::from_u128(TURN + 1));
        let mut adapter = adapter(RunnerToolOfferPlacementStatus::Pinned, Ok(evidence()));
        adapter.state.offers = VecDeque::from([Ok(Some(foreign))]);

        let error = adapter
            .reread(&exact_request())
            .await
            .expect_err("cross-wired durable turn evidence is rejected");

        assert_eq!(
            error.operator_failure_cause_code(),
            "runner_tool_offer_evidence_mismatch"
        );
    }

    #[tokio::test]
    async fn dispatch_failure_preserves_commit_ambiguity_classification() {
        let mut adapter = adapter(
            RunnerToolOfferPlacementStatus::Pinned,
            Err(FakeError::Infrastructure),
        );

        let error = adapter
            .offer(exact_request())
            .await
            .expect_err("the fixture dispatch acknowledgement is ambiguous");

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        );
        assert_eq!(
            error.operator_failure_cause_code(),
            "runner_tool_offer_dispatch"
        );
    }

    #[test]
    fn runner_database_failure_is_retryable_and_decided() {
        let error = RunnerProtocolStoreError::Database(sqlx::Error::PoolClosed);

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            }
        );
        assert_eq!(
            error.operator_failure_cause_code(),
            "runner_protocol_persistence"
        );
    }

    #[test]
    fn runner_commit_failure_preserves_ambiguity() {
        let error = RunnerProtocolStoreError::CommitAmbiguous(sqlx::Error::PoolClosed);

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            }
        );
    }

    #[test]
    fn runner_corruption_fails_closed() {
        let error = RunnerProtocolStoreError::Corruption(
            signalbox_persistence::runner_protocol::RunnerProtocolCorruption::InvalidEncoding,
        );

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::FailClosedCorruption
        );
    }

    #[test]
    fn runner_domain_rejection_is_a_composition_defect() {
        let error = RunnerProtocolStoreError::Domain(RunnerDomainError::InvalidState);

        assert_eq!(
            error.operator_failure_class(),
            OperatorFailureClass::CallerOrHubBug
        );
    }
}

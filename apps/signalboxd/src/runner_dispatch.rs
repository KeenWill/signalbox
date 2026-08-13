//! Durable runner lease authorization followed by best-effort offer delivery.

use std::{error::Error, fmt, future::Future, pin::Pin};

use signalbox_application::{
    ClassifyOperatorFailure, OperatorFailureClass, PinnedRunnerDispatchRequest,
    PinnedRunnerDispatchService, PinnedRunnerDispatchTransaction, PinnedRunnerLeaseOffer,
    RunnerLeaseIdGenerator, UuidV7RunnerLeaseIdGenerator,
};
use signalbox_domain::RunnerLease;
use signalbox_persistence::runner_protocol::{
    RunnerConnectionState, RunnerProtocolStore, RunnerProtocolStoreError,
};
use signalbox_runner_wire::Message;

use crate::{
    runner_connection_broker::{
        RunnerConnectionAddress, RunnerConnectionBroker, RunnerConnectionBrokerError,
    },
    runner_dispatch_wire::RunnerDispatchWireAdapter,
};

/// Boxed future returned by one injected runner-offer authority.
pub type RunnerLeaseOfferAuthorizationFuture<'a, Error> =
    Pin<Box<dyn Future<Output = Result<PinnedRunnerLeaseOffer, Error>> + Send + 'a>>;

/// Atomic authority that turns one prepared attempt into an offered runner lease.
pub trait RunnerLeaseOfferAuthority {
    /// Adapter-specific pre-commit failure.
    type Error;

    /// Commits one exact prepared-attempt and lease-offer pair.
    fn authorize(
        &mut self,
        request: PinnedRunnerDispatchRequest,
    ) -> RunnerLeaseOfferAuthorizationFuture<'_, Self::Error>;
}

impl<Transaction, Ids> RunnerLeaseOfferAuthority for PinnedRunnerDispatchService<Transaction, Ids>
where
    Transaction: PinnedRunnerDispatchTransaction + Send,
    Ids: RunnerLeaseIdGenerator + Send,
{
    type Error = Transaction::Error;

    fn authorize(
        &mut self,
        request: PinnedRunnerDispatchRequest,
    ) -> RunnerLeaseOfferAuthorizationFuture<'_, Self::Error> {
        Box::pin(self.execute(request))
    }
}

/// Boxed future returned by one current runner-route source.
pub type RunnerLeaseOfferRouteFuture<'a, Error> =
    Pin<Box<dyn Future<Output = Result<Option<RunnerConnectionAddress>, Error>> + Send + 'a>>;

/// Resolves the best current process-local route after durable offer commit.
pub trait RunnerLeaseOfferRouteSource {
    /// Adapter-specific route lookup failure.
    type Error;

    /// Returns the current connected address, or absence when delivery must wait.
    fn resolve<'a>(
        &'a self,
        offer: &'a PinnedRunnerLeaseOffer,
    ) -> RunnerLeaseOfferRouteFuture<'a, Self::Error>;
}

impl RunnerLeaseOfferRouteSource for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    fn resolve<'a>(
        &'a self,
        offer: &'a PinnedRunnerLeaseOffer,
    ) -> RunnerLeaseOfferRouteFuture<'a, Self::Error> {
        Box::pin(async move {
            let Some(connection) = self.load_connection(offer.enrollment()).await? else {
                return Ok(None);
            };
            if connection.state() != RunnerConnectionState::Connected {
                return Ok(None);
            }
            Ok(Some(RunnerConnectionAddress::new(
                offer.enrollment(),
                offer.lease().runner(),
                connection.epoch(),
            )))
        })
    }
}

/// Process-local transport for one already-authorized offer frame.
pub trait RunnerLeaseOfferTransport {
    /// Adapter-specific routing failure.
    type Error;

    /// Attempts one nonblocking handoff to the exact established connection.
    fn send(&self, address: RunnerConnectionAddress, message: Message) -> Result<(), Self::Error>;

    /// Classifies a failed handoff without claiming the durable offer rolled back.
    fn classify_failure(error: &Self::Error) -> RunnerLeaseOfferTransportFailure;
}

impl RunnerLeaseOfferTransport for RunnerConnectionBroker {
    type Error = RunnerConnectionBrokerError;

    fn send(&self, address: RunnerConnectionAddress, message: Message) -> Result<(), Self::Error> {
        RunnerConnectionBroker::send(self, address, message)
    }

    fn classify_failure(error: &Self::Error) -> RunnerLeaseOfferTransportFailure {
        match error {
            RunnerConnectionBrokerError::ConnectionUnavailable
            | RunnerConnectionBrokerError::QueueFull => {
                RunnerLeaseOfferTransportFailure::Unavailable
            }
            RunnerConnectionBrokerError::StateUnavailable
            | RunnerConnectionBrokerError::ConnectionAlreadyAttached
            | RunnerConnectionBrokerError::UnsupportedMessage
            | RunnerConnectionBrokerError::RunnerMismatch => {
                RunnerLeaseOfferTransportFailure::Invariant
            }
        }
    }
}

/// Closed process-local classification for one failed offer handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerLeaseOfferTransportFailure {
    /// The exact route is absent or applying bounded backpressure.
    Unavailable,
    /// Broker bookkeeping or canonical message correlation disagreed.
    Invariant,
}

/// Why an offered lease remains durable but was not handed to a socket task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerLeaseOfferRetention {
    /// The transaction returned a lease for another prepared dispatch request.
    AuthorityMismatch,
    /// Canonical durable facts could not be projected to the version-one wire.
    ProjectionFailed,
    /// No live durable connection exists after authorization commit.
    ConnectionUnavailable,
    /// Current connection lookup failed after authorization commit.
    ConnectionLookupFailed,
    /// The process-local route could not accept the frame.
    TransportUnavailable,
    /// Process-local routing rejected canonical authority as an invariant failure.
    TransportInvariant,
}

/// Process-local delivery state for one durably offered lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerLeaseOfferDelivery {
    /// The offer entered the exact connection task's bounded handoff queue.
    Delivered,
    /// Durable offer authority remains for later recovery or redelivery.
    Retained(RunnerLeaseOfferRetention),
}

/// One durably committed lease plus its non-authoritative delivery observation.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerLeaseOfferOutcome {
    lease: RunnerLease,
    delivery: RunnerLeaseOfferDelivery,
}

impl RunnerLeaseOfferOutcome {
    /// Borrows the canonical durably offered lease.
    pub const fn lease(&self) -> &RunnerLease {
        &self.lease
    }

    /// Returns the process-local delivery observation.
    pub const fn delivery(&self) -> RunnerLeaseOfferDelivery {
        self.delivery
    }

    /// Returns both the durable authority and non-authoritative delivery state.
    pub fn into_parts(self) -> (RunnerLease, RunnerLeaseOfferDelivery) {
        (self.lease, self.delivery)
    }
}

/// Pre-commit failure while authorizing a runner offer.
#[derive(Debug)]
pub struct RunnerLeaseOfferDispatchError<AuthorizationError> {
    source: AuthorizationError,
}

impl<AuthorizationError> RunnerLeaseOfferDispatchError<AuthorizationError> {
    /// Borrows the exact pre-commit authorization failure.
    pub const fn authorization_error(&self) -> &AuthorizationError {
        &self.source
    }

    /// Returns the exact pre-commit authorization failure.
    pub fn into_authorization_error(self) -> AuthorizationError {
        self.source
    }
}

impl<AuthorizationError> fmt::Display for RunnerLeaseOfferDispatchError<AuthorizationError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runner lease offer authorization failed")
    }
}

impl<AuthorizationError> Error for RunnerLeaseOfferDispatchError<AuthorizationError>
where
    AuthorizationError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

impl<AuthorizationError> ClassifyOperatorFailure
    for RunnerLeaseOfferDispatchError<AuthorizationError>
where
    AuthorizationError: ClassifyOperatorFailure,
{
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.source.operator_failure_class()
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        "runner_lease_offer_authorization"
    }
}

/// Commits one runner lease offer, then attempts a non-authoritative socket handoff.
#[derive(Debug)]
pub struct RunnerLeaseOfferDispatcher<Authority, Routes, Transport> {
    authority: Authority,
    routes: Routes,
    transport: Transport,
}

impl<Authority, Routes, Transport> RunnerLeaseOfferDispatcher<Authority, Routes, Transport> {
    /// Composes durable authorization, current-route lookup, and process transport.
    pub const fn new(authority: Authority, routes: Routes, transport: Transport) -> Self {
        Self {
            authority,
            routes,
            transport,
        }
    }
}

impl
    RunnerLeaseOfferDispatcher<
        PinnedRunnerDispatchService<RunnerProtocolStore, UuidV7RunnerLeaseIdGenerator>,
        RunnerProtocolStore,
        RunnerConnectionBroker,
    >
{
    /// Composes the production PostgreSQL authority with its shared socket broker.
    pub fn postgres(store: RunnerProtocolStore, broker: RunnerConnectionBroker) -> Self {
        Self::new(
            PinnedRunnerDispatchService::new(store.clone(), UuidV7RunnerLeaseIdGenerator),
            store,
            broker,
        )
    }
}

impl<Authority, Routes, Transport> RunnerLeaseOfferDispatcher<Authority, Routes, Transport>
where
    Authority: RunnerLeaseOfferAuthority,
    Routes: RunnerLeaseOfferRouteSource,
    Routes::Error: fmt::Display,
    Transport: RunnerLeaseOfferTransport,
    Transport::Error: fmt::Display,
{
    /// Authorizes exactly once; every later failure retains the committed offer.
    pub async fn dispatch(
        &mut self,
        request: PinnedRunnerDispatchRequest,
    ) -> Result<RunnerLeaseOfferOutcome, RunnerLeaseOfferDispatchError<Authority::Error>> {
        let offer = self
            .authority
            .authorize(request)
            .await
            .map_err(|source| RunnerLeaseOfferDispatchError { source })?;
        if !lease_matches_request(offer.lease(), request) {
            tracing::error!("durable runner offer authority returned cross-wired lease facts");
            let (_, lease) = offer.into_parts();
            return Ok(retained(
                lease,
                RunnerLeaseOfferRetention::AuthorityMismatch,
            ));
        }
        let message = match RunnerDispatchWireAdapter::lease_offer(offer.lease()) {
            Ok(message) => message,
            Err(error) => {
                tracing::error!(error = %error, "durable runner offer wire projection failed");
                let (_, lease) = offer.into_parts();
                return Ok(retained(lease, RunnerLeaseOfferRetention::ProjectionFailed));
            }
        };
        let address = match self.routes.resolve(&offer).await {
            Ok(Some(address)) => address,
            Ok(None) => {
                let (_, lease) = offer.into_parts();
                return Ok(retained(
                    lease,
                    RunnerLeaseOfferRetention::ConnectionUnavailable,
                ));
            }
            Err(error) => {
                tracing::error!(error = %error, "durable runner offer route lookup failed");
                let (_, lease) = offer.into_parts();
                return Ok(retained(
                    lease,
                    RunnerLeaseOfferRetention::ConnectionLookupFailed,
                ));
            }
        };
        if address.enrollment() != offer.enrollment() || address.runner() != offer.lease().runner()
        {
            tracing::error!("runner offer route source returned a cross-wired connection address");
            let (_, lease) = offer.into_parts();
            return Ok(retained(
                lease,
                RunnerLeaseOfferRetention::AuthorityMismatch,
            ));
        }
        let (_, lease) = offer.into_parts();
        if let Err(error) = self.transport.send(address, message) {
            let retention = match Transport::classify_failure(&error) {
                RunnerLeaseOfferTransportFailure::Unavailable => {
                    tracing::warn!(error = %error, "durable runner offer awaits later delivery");
                    RunnerLeaseOfferRetention::TransportUnavailable
                }
                RunnerLeaseOfferTransportFailure::Invariant => {
                    tracing::error!(error = %error, "durable runner offer routing invariant failed");
                    RunnerLeaseOfferRetention::TransportInvariant
                }
            };
            return Ok(retained(lease, retention));
        }
        Ok(RunnerLeaseOfferOutcome {
            lease,
            delivery: RunnerLeaseOfferDelivery::Delivered,
        })
    }
}

fn lease_matches_request(lease: &RunnerLease, request: PinnedRunnerDispatchRequest) -> bool {
    let correlation = lease.correlation();
    correlation.dispatch.session() == request.session()
        && correlation.dispatch.turn() == request.turn()
        && correlation.dispatch.attempt() == request.attempt()
        && correlation.runner == request.runner()
        && correlation.registration_revision == request.registration_revision()
}

fn retained(lease: RunnerLease, reason: RunnerLeaseOfferRetention) -> RunnerLeaseOfferOutcome {
    RunnerLeaseOfferOutcome {
        lease,
        delivery: RunnerLeaseOfferDelivery::Retained(reason),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, future::ready};

    use signalbox_application::{PinnedRunnerDispatchRequest, PinnedRunnerLeaseOffer};
    use signalbox_domain::{
        NormalizedToolArguments, RunnerAdvertisement, RunnerAuthenticationId,
        RunnerCapabilityClass, RunnerCatalog, RunnerEnrollment, RunnerEnrollmentId,
        RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseCorrelation, RunnerLeaseId,
        RunnerLeaseReconstitutionInput, RunnerLeaseRetryPreparation, RunnerLeaseState,
        RunnerRepositoryEntry, RunnerSandboxProfile, RunnerSelector, RunnerToolDeclaration,
        RunnerToolEffectClass, RunnerToolModelDefinition, RunnerWorkingDirectory, SessionId,
        ToolAdmissibleLoci, ToolAttemptDispatchCorrelation,
        ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId, ToolDispatchGeneration,
        ToolName, ToolPermissionDefault, ToolRequestId, TurnAttemptId, TurnId,
        ValidatedRunnerRegistration, WorkspaceCapability,
    };
    use signalbox_persistence::runner_protocol::RunnerConnectionEpoch;
    use signalbox_runner_wire::{
        CanonicalUuid, LeaseOffer, Message, PositiveU64, ResultBounds, SandboxProfile,
        WireToolName, WorkingDirectory,
    };
    use uuid::Uuid;

    use super::{
        RunnerConnectionAddress, RunnerConnectionBroker, RunnerConnectionBrokerError,
        RunnerLeaseOfferAuthority, RunnerLeaseOfferAuthorizationFuture, RunnerLeaseOfferDelivery,
        RunnerLeaseOfferDispatcher, RunnerLeaseOfferRetention, RunnerLeaseOfferRouteFuture,
        RunnerLeaseOfferRouteSource, RunnerLeaseOfferTransport, RunnerLeaseOfferTransportFailure,
    };

    const ENROLLMENT: u128 = 1;
    const RUNNER: u128 = 2;
    const AUTHENTICATION: u128 = 3;
    const SESSION: u128 = 4;
    const TURN: u128 = 5;
    const ATTEMPT: u128 = 6;
    const REQUEST: u128 = 7;
    const TURN_ATTEMPT: u128 = 8;
    const LEASE: u128 = 9;
    const FOREIGN_ATTEMPT: u128 = 10;
    const FOREIGN_RUNNER: u128 = 11;
    const FOREIGN_ENROLLMENT: u128 = 12;

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn tool() -> ToolName {
        ToolName::try_new("sandboxed_exec".to_owned()).expect("the fixture tool is valid")
    }

    fn class() -> RunnerCapabilityClass {
        RunnerCapabilityClass::try_new("linux.workspace".to_owned())
            .expect("the fixture class is valid")
    }

    fn arguments_value() -> serde_json::Value {
        serde_json::json!({"argv": ["printf", "runner"]})
    }

    fn arguments() -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(arguments_value().to_string())
            .expect("the fixture arguments are canonical")
    }

    fn registration() -> ValidatedRunnerRegistration {
        let declaration = RunnerToolDeclaration::new(
            tool(),
            RunnerToolModelDefinition::try_new(
                "Execute one generic sandboxed command".to_owned(),
                r#"{"type":"object"}"#.to_owned(),
            )
            .expect("the fixture model definition is valid"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::CapabilityClass(class()),
            },
        );
        let catalog = RunnerCatalog::try_new(
            [class()],
            [declaration],
            [],
            Vec::<WorkspaceCapability>::new(),
            [RunnerSandboxProfile::WorkspaceRestricted],
        )
        .expect("the fixture catalog is consistent");
        RunnerEnrollment::new(
            RunnerEnrollmentId::from_uuid(id(ENROLLMENT)),
            RunnerId::from_uuid(id(RUNNER)),
            RunnerAuthenticationId::from_uuid(id(AUTHENTICATION)),
            [class()],
        )
        .register(
            RunnerAdvertisement::new(
                [class()],
                [tool()],
                [],
                [],
                [RunnerSandboxProfile::WorkspaceRestricted],
                Vec::<RunnerRepositoryEntry>::new(),
            ),
            &catalog,
        )
        .expect("the fixture advertisement is admitted")
    }

    fn correlation() -> RunnerLeaseCorrelation {
        RunnerLeaseCorrelation {
            lease: RunnerLeaseId::from_uuid(id(LEASE)),
            runner: RunnerId::from_uuid(id(RUNNER)),
            registration_revision: RunnerGeneration::one(),
            placement_revision: RunnerGeneration::one(),
            working_directory: RunnerWorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the fixture directory is valid"),
            sandbox: RunnerSandboxProfile::WorkspaceRestricted,
            tool: tool(),
            dispatch: ToolAttemptDispatchCorrelation::reconstitute(
                ToolAttemptDispatchCorrelationReconstitutionInput {
                    session: SessionId::from_uuid(id(SESSION)),
                    turn: TurnId::from_uuid(id(TURN)),
                    issuing_attempt: TurnAttemptId::from_uuid(id(TURN_ATTEMPT)),
                    request: ToolRequestId::from_uuid(id(REQUEST)),
                    attempt: ToolAttemptId::from_uuid(id(ATTEMPT)),
                    generation: ToolDispatchGeneration::first(),
                },
            ),
            generation: RunnerGeneration::one(),
        }
    }

    fn offered_lease() -> RunnerLease {
        let registration = registration();
        let correlation = correlation();
        let arguments = arguments();
        RunnerLease::reconstitute(
            RunnerLeaseReconstitutionInput {
                lease: correlation.lease,
                dispatch: correlation.dispatch,
                runner: correlation.runner,
                registration_revision: correlation.registration_revision,
                placement_revision: correlation.placement_revision,
                working_directory: correlation.working_directory.clone(),
                sandbox: correlation.sandbox,
                tool: correlation.tool.clone(),
                arguments: arguments.clone(),
                effect: RunnerToolEffectClass::Pure,
                credential_authorization: None,
                generation: correlation.generation,
                state: RunnerLeaseState::Offered,
                recorded_correlation: correlation,
                recorded_session: SessionId::from_uuid(id(SESSION)),
                recorded_effect: RunnerToolEffectClass::Pure,
                recorded_arguments: arguments,
                recorded_credential_authorization: None,
                recorded_state: RunnerLeaseState::Offered,
                retry_preparation: RunnerLeaseRetryPreparation::Available,
            },
            &registration,
        )
        .expect("the fixture lease is consistent")
    }

    fn request() -> PinnedRunnerDispatchRequest {
        PinnedRunnerDispatchRequest::new(
            SessionId::from_uuid(id(SESSION)),
            TurnId::from_uuid(id(TURN)),
            ToolAttemptId::from_uuid(id(ATTEMPT)),
            RunnerId::from_uuid(id(RUNNER)),
            RunnerGeneration::one(),
        )
    }

    fn address() -> RunnerConnectionAddress {
        RunnerConnectionAddress::new(
            RunnerEnrollmentId::from_uuid(id(ENROLLMENT)),
            RunnerId::from_uuid(id(RUNNER)),
            RunnerConnectionEpoch::try_from_u64(1).expect("the fixture epoch is positive"),
        )
    }

    fn wire_correlation() -> signalbox_runner_wire::LeaseCorrelation {
        signalbox_runner_wire::LeaseCorrelation {
            registration_revision: PositiveU64::try_new(1)
                .expect("the registration revision is positive"),
            lease_id: CanonicalUuid::from_uuid(id(LEASE)),
            lease_generation: PositiveU64::try_new(1).expect("the lease generation is positive"),
            runner_id: CanonicalUuid::from_uuid(id(RUNNER)),
            placement_revision: PositiveU64::try_new(1)
                .expect("the placement revision is positive"),
            working_directory: WorkingDirectory::try_new("/workspace/session".to_owned())
                .expect("the wire directory is valid"),
            sandbox_profile: SandboxProfile::WorkspaceRestricted,
            tool_name: WireToolName::try_new("sandboxed_exec".to_owned())
                .expect("the wire tool is valid"),
            session_id: CanonicalUuid::from_uuid(id(SESSION)),
            turn_id: CanonicalUuid::from_uuid(id(TURN)),
            tool_request_id: CanonicalUuid::from_uuid(id(REQUEST)),
            tool_attempt_id: CanonicalUuid::from_uuid(id(ATTEMPT)),
            issuing_turn_attempt_id: CanonicalUuid::from_uuid(id(TURN_ATTEMPT)),
            tool_dispatch_generation: PositiveU64::try_new(1)
                .expect("the dispatch generation is positive"),
        }
    }

    #[derive(Debug)]
    struct FixedAuthority {
        leases: VecDeque<RunnerLease>,
    }

    impl RunnerLeaseOfferAuthority for FixedAuthority {
        type Error = &'static str;

        fn authorize(
            &mut self,
            _request: PinnedRunnerDispatchRequest,
        ) -> RunnerLeaseOfferAuthorizationFuture<'_, Self::Error> {
            Box::pin(ready(Ok(PinnedRunnerLeaseOffer::new(
                RunnerEnrollmentId::from_uuid(id(ENROLLMENT)),
                self.leases
                    .pop_front()
                    .expect("the fixture supplies one lease"),
            ))))
        }
    }

    #[derive(Debug)]
    struct RejectingAuthority;

    impl RunnerLeaseOfferAuthority for RejectingAuthority {
        type Error = &'static str;

        fn authorize(
            &mut self,
            _request: PinnedRunnerDispatchRequest,
        ) -> RunnerLeaseOfferAuthorizationFuture<'_, Self::Error> {
            Box::pin(ready(Err("authorization refused")))
        }
    }

    #[derive(Debug)]
    struct FixedRoutes(Result<Option<RunnerConnectionAddress>, &'static str>);

    impl RunnerLeaseOfferRouteSource for FixedRoutes {
        type Error = &'static str;

        fn resolve<'a>(
            &'a self,
            _offer: &'a PinnedRunnerLeaseOffer,
        ) -> RunnerLeaseOfferRouteFuture<'a, Self::Error> {
            Box::pin(ready(self.0))
        }
    }

    #[derive(Debug)]
    struct RecordingTransport {
        sent: std::sync::Mutex<Vec<(RunnerConnectionAddress, Message)>>,
        result: Result<(), &'static str>,
    }

    impl RunnerLeaseOfferTransport for RecordingTransport {
        type Error = &'static str;

        fn send(
            &self,
            address: RunnerConnectionAddress,
            message: Message,
        ) -> Result<(), Self::Error> {
            self.sent
                .lock()
                .expect("the fixture transport recorder is available")
                .push((address, message));
            self.result
        }

        fn classify_failure(_error: &Self::Error) -> RunnerLeaseOfferTransportFailure {
            RunnerLeaseOfferTransportFailure::Unavailable
        }
    }

    fn dispatcher(
        routes: Result<Option<RunnerConnectionAddress>, &'static str>,
        transport_result: Result<(), &'static str>,
    ) -> RunnerLeaseOfferDispatcher<FixedAuthority, FixedRoutes, RecordingTransport> {
        RunnerLeaseOfferDispatcher::new(
            FixedAuthority {
                leases: VecDeque::from([offered_lease()]),
            },
            FixedRoutes(routes),
            RecordingTransport {
                sent: std::sync::Mutex::new(Vec::new()),
                result: transport_result,
            },
        )
    }

    /// INV-043: a runner offer is durable before any execution capability leaves the daemon.
    #[tokio::test]
    async fn s16_inv043_committed_offer_is_projected_and_handed_to_the_exact_route() {
        let mut dispatcher = dispatcher(Ok(Some(address())), Ok(()));

        let outcome = dispatcher
            .dispatch(request())
            .await
            .expect("the fixture authorization commits");
        let sent = dispatcher
            .transport
            .sent
            .lock()
            .expect("the fixture transport recorder is available")
            .pop()
            .expect("one offer was handed to transport");

        assert_eq!(outcome.delivery(), RunnerLeaseOfferDelivery::Delivered);
        assert_eq!(outcome.lease().correlation(), correlation());
        assert_eq!(sent.0, address());
        assert_eq!(
            sent.1,
            Message::LeaseOffer(LeaseOffer {
                correlation: wire_correlation(),
                effect_class: signalbox_runner_wire::EffectClass::Pure,
                credential_profile: None,
                grant_revision: None,
                normalized_arguments: arguments_value(),
                result_bounds: ResultBounds::version_one(),
            })
        );
    }

    #[tokio::test]
    async fn s16_inv043_absent_route_retains_the_committed_offer() {
        let mut dispatcher = dispatcher(Ok(None), Ok(()));

        let outcome = dispatcher
            .dispatch(request())
            .await
            .expect("the fixture authorization commits");

        assert_eq!(
            outcome.delivery(),
            RunnerLeaseOfferDelivery::Retained(RunnerLeaseOfferRetention::ConnectionUnavailable)
        );
        assert_eq!(outcome.lease().correlation(), correlation());
        assert!(
            dispatcher
                .transport
                .sent
                .lock()
                .expect("the fixture transport recorder is available")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn s16_inv043_route_lookup_failure_retains_the_committed_offer() {
        let mut dispatcher = dispatcher(Err("route unavailable"), Ok(()));

        let outcome = dispatcher
            .dispatch(request())
            .await
            .expect("the fixture authorization commits");

        assert_eq!(
            outcome.delivery(),
            RunnerLeaseOfferDelivery::Retained(RunnerLeaseOfferRetention::ConnectionLookupFailed)
        );
        assert_eq!(outcome.lease().correlation(), correlation());
    }

    #[tokio::test]
    async fn s16_inv043_transport_failure_retains_the_committed_offer() {
        let mut dispatcher = dispatcher(Ok(Some(address())), Err("queue unavailable"));

        let outcome = dispatcher
            .dispatch(request())
            .await
            .expect("the fixture authorization commits");

        assert_eq!(
            outcome.delivery(),
            RunnerLeaseOfferDelivery::Retained(RunnerLeaseOfferRetention::TransportUnavailable)
        );
        assert_eq!(outcome.lease().correlation(), correlation());
    }

    #[tokio::test]
    async fn s16_inv043_authorization_refusal_emits_no_offer_or_durable_outcome() {
        let transport = RecordingTransport {
            sent: std::sync::Mutex::new(Vec::new()),
            result: Ok(()),
        };
        let mut dispatcher = RunnerLeaseOfferDispatcher::new(
            RejectingAuthority,
            FixedRoutes(Ok(Some(address()))),
            transport,
        );

        let error = dispatcher
            .dispatch(request())
            .await
            .expect_err("a pre-commit refusal returns no durable offer");

        assert_eq!(error.authorization_error(), &"authorization refused");
        assert!(
            dispatcher
                .transport
                .sent
                .lock()
                .expect("the fixture transport recorder is available")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn s16_inv043_cross_wired_authority_retains_without_emitting_an_offer() {
        let mut dispatcher = dispatcher(Ok(Some(address())), Ok(()));
        let foreign_request = PinnedRunnerDispatchRequest::new(
            SessionId::from_uuid(id(SESSION)),
            TurnId::from_uuid(id(TURN)),
            ToolAttemptId::from_uuid(id(FOREIGN_ATTEMPT)),
            RunnerId::from_uuid(id(RUNNER)),
            RunnerGeneration::one(),
        );

        let outcome = dispatcher
            .dispatch(foreign_request)
            .await
            .expect("the injected authority returned a durable lease");

        assert_eq!(
            outcome.delivery(),
            RunnerLeaseOfferDelivery::Retained(RunnerLeaseOfferRetention::AuthorityMismatch)
        );
        assert!(
            dispatcher
                .transport
                .sent
                .lock()
                .expect("the fixture transport recorder is available")
                .is_empty()
        );
    }

    /// INV-043: the frozen runner identity is part of offer authority.
    #[tokio::test]
    async fn s16_inv043_cross_wired_runner_locus_retains_without_emitting_an_offer() {
        let mut dispatcher = dispatcher(Ok(Some(address())), Ok(()));
        let foreign_request = PinnedRunnerDispatchRequest::new(
            SessionId::from_uuid(id(SESSION)),
            TurnId::from_uuid(id(TURN)),
            ToolAttemptId::from_uuid(id(ATTEMPT)),
            RunnerId::from_uuid(id(FOREIGN_RUNNER)),
            RunnerGeneration::one(),
        );

        let outcome = dispatcher
            .dispatch(foreign_request)
            .await
            .expect("the injected authority returned a durable lease");

        assert_eq!(
            outcome.delivery(),
            RunnerLeaseOfferDelivery::Retained(RunnerLeaseOfferRetention::AuthorityMismatch)
        );
        assert!(
            dispatcher
                .transport
                .sent
                .lock()
                .expect("the fixture transport recorder is available")
                .is_empty()
        );
    }

    /// INV-043: routing may not substitute another enrollment after commit.
    #[tokio::test]
    async fn s16_inv043_cross_wired_route_retains_without_emitting_an_offer() {
        let cross_wired_route = RunnerConnectionAddress::new(
            RunnerEnrollmentId::from_uuid(id(FOREIGN_ENROLLMENT)),
            RunnerId::from_uuid(id(RUNNER)),
            RunnerConnectionEpoch::try_from_u64(1).expect("the fixture epoch is positive"),
        );
        let mut dispatcher = dispatcher(Ok(Some(cross_wired_route)), Ok(()));

        let outcome = dispatcher
            .dispatch(request())
            .await
            .expect("the injected authority returned a durable lease");

        assert_eq!(
            outcome.delivery(),
            RunnerLeaseOfferDelivery::Retained(RunnerLeaseOfferRetention::AuthorityMismatch)
        );
        assert!(
            dispatcher
                .transport
                .sent
                .lock()
                .expect("the fixture transport recorder is available")
                .is_empty()
        );
    }

    #[test]
    fn broker_backpressure_is_a_retainable_transport_failure() {
        assert_eq!(
            <RunnerConnectionBroker as RunnerLeaseOfferTransport>::classify_failure(
                &RunnerConnectionBrokerError::QueueFull,
            ),
            RunnerLeaseOfferTransportFailure::Unavailable
        );
    }

    #[test]
    fn broker_correlation_disagreement_is_an_invariant_transport_failure() {
        assert_eq!(
            <RunnerConnectionBroker as RunnerLeaseOfferTransport>::classify_failure(
                &RunnerConnectionBrokerError::RunnerMismatch,
            ),
            RunnerLeaseOfferTransportFailure::Invariant
        );
    }
}

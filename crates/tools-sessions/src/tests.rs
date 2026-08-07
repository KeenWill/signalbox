use std::{
    error::Error,
    future::{Future, ready},
    num::NonZeroU64,
    pin::pin,
    task::{Context, Poll, Waker},
};

use serde_json::{Value, json};
use signalbox_application::{ToolCatalog, ToolCatalogValidationFailure};
use signalbox_domain::{
    ContextFrontierId, DescendantTerminationScope, DurableCommandId, GoalGeneration, ModelCallId,
    ResolvedContextFrontierReconstitutionInput, SessionDelegationReconstitutionInput,
    ToolApprovalResolutionReconstitutionInput, ToolAttemptId, ToolAttemptReconstitutionInput,
    ToolAttemptReconstitutionState, ToolBatchPhaseReconstitutionInput,
    ToolBatchReconstitutionInput, ToolDispatchGeneration, ToolName, ToolRequestOrdinal,
    ToolRequestReconstitutionInput, TurnAttemptId, TurnId,
};
use signalbox_tool_contract::rendered_contract_schema;

use super::*;

const TASK: &str = "Inspect the delegated subsystem";
const MESSAGE: &str = "The child found one actionable issue";
const RETURNED_CONTENT: &str = "The delegated task is complete";

fn session(seed: u128) -> SessionId {
    SessionId::from_uuid(uuid::Uuid::from_u128(seed))
}

fn turn(seed: u128) -> TurnId {
    TurnId::from_uuid(uuid::Uuid::from_u128(seed))
}

fn request_id(seed: u128) -> ToolRequestId {
    ToolRequestId::from_uuid(uuid::Uuid::from_u128(seed))
}

fn message_id(seed: u128) -> DelegationMessageId {
    DelegationMessageId::from_uuid(uuid::Uuid::from_u128(seed))
}

fn dispatch(request: &ToolRequest, effect: ToolEffectClass) -> ToolDispatchAuthority {
    let attempt_id = ToolAttemptId::from_uuid(uuid::Uuid::from_u128(0xf01));
    let turn_attempt = TurnAttemptId::from_uuid(uuid::Uuid::from_u128(0xf02));
    let approval = ToolApprovalResolutionReconstitutionInput::policy_auto(request.id())
        .reconstitute()
        .expect("automatic request approval reconstitutes");
    let attempt = ToolAttemptReconstitutionInput::new(
        attempt_id,
        request.id(),
        request.session(),
        request.turn(),
        turn_attempt,
        effect,
        ToolDispatchGeneration::first(),
        ToolAttemptReconstitutionState::Prepared,
    )
    .reconstitute()
    .expect("prepared attempt reconstitutes");
    let snapshot = ResolvedContextFrontierReconstitutionInput::new(
        request.session(),
        ContextFrontierId::from_uuid(uuid::Uuid::from_u128(0xf03)),
        Vec::new(),
    )
    .reconstitute()
    .expect("empty context snapshot reconstitutes");
    let batch = ToolBatchReconstitutionInput::new(
        request.session(),
        request.turn(),
        request.producing_call(),
        snapshot,
        vec![request.clone()],
        vec![approval],
        vec![attempt],
        ToolBatchPhaseReconstitutionInput::Executing { turn_attempt },
    )
    .reconstitute()
    .expect("single-request batch reconstitutes");
    batch
        .authorize_dispatch(attempt_id)
        .expect("prepared attempt authorizes dispatch")
}

/// Canonical logical request fixture: the seed identifies the request and
/// derives its distinct session, turn, and producing-call identities.
fn request(seed: u128, name: &str, arguments: Value) -> ToolRequest {
    request_for_session(seed, session(seed + 100), name, arguments)
}

fn request_for_session(seed: u128, source: SessionId, name: &str, arguments: Value) -> ToolRequest {
    ToolRequestReconstitutionInput::new(
        request_id(seed),
        source,
        turn(seed + 200),
        ModelCallId::from_uuid(uuid::Uuid::from_u128(seed + 300)),
        ToolRequestOrdinal::from_u32(0),
        ToolName::try_new(name.to_owned()).expect("fixture tool name is admitted"),
        NormalizedToolArguments::try_from_provider_text(arguments.to_string())
            .expect("fixture arguments are normalized"),
    )
    .into_request()
}

fn background_spawn_for_parent(seed: u128, parent: SessionId) -> ToolRequest {
    request_for_session(
        seed,
        parent,
        SPAWN_SESSION_NAME,
        json!({
            "relationship": { "kind": "background" },
            "task": TASK,
        }),
    )
}

fn bound_spawn_for_parent(seed: u128, parent: SessionId) -> ToolRequest {
    request_for_session(
        seed,
        parent,
        SPAWN_SESSION_NAME,
        json!({
            "relationship": {
                "kind": "bound",
                "on_parent_cancelled": "cancel",
                "on_parent_stopped": "stop",
            },
            "task": TASK,
        }),
    )
}

fn background_spawn(seed: u128) -> ToolRequest {
    request(
        seed,
        SPAWN_SESSION_NAME,
        json!({
            "relationship": { "kind": "background" },
            "task": TASK,
        }),
    )
}

fn bound_spawn(seed: u128) -> ToolRequest {
    request(
        seed,
        SPAWN_SESSION_NAME,
        json!({
            "relationship": {
                "kind": "bound",
                "on_parent_cancelled": "cancel",
                "on_parent_stopped": "stop",
            },
            "task": TASK,
        }),
    )
}

fn await_request(seed: u128, child: SessionId, mode: &str) -> ToolRequest {
    request(
        seed,
        AWAIT_SESSION_NAME,
        json!({
            "child_session_id": child.as_uuid().to_string(),
            "mode": mode,
        }),
    )
}

fn message_request(seed: u128, peer: SessionId) -> ToolRequest {
    request(
        seed,
        SEND_SESSION_MESSAGE_NAME,
        json!({
            "content": MESSAGE,
            "peer_session_id": peer.as_uuid().to_string(),
        }),
    )
}

fn arguments(value: Value) -> NormalizedToolArguments {
    NormalizedToolArguments::try_from_provider_text(value.to_string())
        .expect("fixture arguments are normalized")
}

fn decoded_spawn(request: &ToolRequest) -> DelegatedSpawnRequest {
    let SessionDelegationOperation::Spawn(spawn) =
        decode_operation(request).expect("fixture spawn request is canonical")
    else {
        panic!("fixture operation is a spawn")
    };
    spawn
}

fn decoded_await(request: &ToolRequest) -> DelegationAwaitRequest {
    let SessionDelegationOperation::Await(awaiting) =
        decode_operation(request).expect("fixture await request is canonical")
    else {
        panic!("fixture operation is an await")
    };
    awaiting
}

fn decoded_message(request: &ToolRequest) -> DelegationMessageRequest {
    let SessionDelegationOperation::SendMessage(message) =
        decode_operation(request).expect("fixture message request is canonical")
    else {
        panic!("fixture operation is a message")
    };
    message
}

fn run_ready<Output>(future: impl Future<Output = Output>) -> Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("fake delegation port must return immediately"),
    }
}

fn durably_completed_text(disposition: UnboundExecutionDisposition) -> String {
    let UnboundExecutionDisposition::DurableCompletion(ToolExecutorEvidence::CompletedText(result)) =
        disposition
    else {
        panic!("fixture operation completes durably with text")
    };
    result
}

fn foreground_result(disposition: UnboundExecutionDisposition) -> DeliveredChildResult {
    let UnboundExecutionDisposition::ForegroundDelivered(result) = disposition else {
        panic!("fixture operation delivers one typed foreground result")
    };
    result
}

#[track_caller]
fn assert_port_contract(
    result: Result<UnboundExecutionDisposition, SessionDelegationExecutorError<FakeError>>,
) {
    assert!(matches!(
        result,
        Err(SessionDelegationExecutorError::PortContract)
    ));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FakeError;

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fake session-delegation port failed")
    }
}

impl Error for FakeError {}

impl ClassifyOperatorFailure for FakeError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        OperatorFailureClass::Infrastructure {
            commit_ambiguous: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FakeMessageDelivery {
    tool_request: ToolRequestId,
    message: DelegationMessageId,
    direction: DelegationMessageDirection,
    ordinal: DelegationEventOrdinal,
    delivery_sequence: NonZeroU64,
}

impl DelegationMessageDeliveryProjection for FakeMessageDelivery {
    fn tool_request(&self) -> ToolRequestId {
        self.tool_request
    }

    fn message(&self) -> DelegationMessageId {
        self.message
    }

    fn direction(&self) -> DelegationMessageDirection {
        self.direction
    }

    fn ordinal(&self) -> DelegationEventOrdinal {
        self.ordinal
    }

    fn delivery_sequence(&self) -> NonZeroU64 {
        self.delivery_sequence
    }
}

#[derive(Debug)]
struct FakePort {
    spawn_result: Option<SessionDelegationPortOutcome<SpawnSessionReceipt>>,
    await_result: Option<AwaitSessionPortOutcome>,
    message_result: Option<SessionDelegationPortOutcome<FakeMessageDelivery>>,
    spawn_requests: Vec<DelegatedSpawnRequest>,
    await_requests: Vec<DelegationAwaitRequest>,
    message_requests: Vec<DelegationMessageRequest>,
}

impl FakePort {
    fn spawning(receipt: SpawnSessionReceipt) -> Self {
        Self {
            spawn_result: Some(SessionDelegationPortOutcome::Applied(receipt)),
            await_result: None,
            message_result: None,
            spawn_requests: Vec::new(),
            await_requests: Vec::new(),
            message_requests: Vec::new(),
        }
    }

    fn awaiting(result: AwaitSessionPortOutcome) -> Self {
        Self {
            spawn_result: None,
            await_result: Some(result),
            message_result: None,
            spawn_requests: Vec::new(),
            await_requests: Vec::new(),
            message_requests: Vec::new(),
        }
    }

    fn messaging(delivery: FakeMessageDelivery) -> Self {
        Self {
            spawn_result: None,
            await_result: None,
            message_result: Some(SessionDelegationPortOutcome::Applied(delivery)),
            spawn_requests: Vec::new(),
            await_requests: Vec::new(),
            message_requests: Vec::new(),
        }
    }
}

impl SessionDelegationPort for FakePort {
    type Error = FakeError;
    type MessageDelivery = FakeMessageDelivery;

    fn spawn_session(
        &mut self,
        request: DelegatedSpawnRequest,
        dispatch: ToolDispatchAuthority,
    ) -> impl Future<Output = Result<SessionDelegationPortOutcome<SpawnSessionReceipt>, Self::Error>>
    + Send {
        assert_eq!(dispatch.request(), request.request());
        self.spawn_requests.push(request);
        ready(Ok(self
            .spawn_result
            .take()
            .expect("fixture spawn result is configured")))
    }

    fn await_session(
        &mut self,
        request: DelegationAwaitRequest,
        dispatch: ToolDispatchAuthority,
    ) -> impl Future<Output = Result<AwaitSessionPortOutcome, Self::Error>> + Send {
        assert_eq!(dispatch.request(), request.request());
        self.await_requests.push(request);
        ready(Ok(self
            .await_result
            .take()
            .expect("fixture await result is configured")))
    }

    fn send_session_message(
        &mut self,
        request: DelegationMessageRequest,
        dispatch: ToolDispatchAuthority,
    ) -> impl Future<
        Output = Result<SessionDelegationPortOutcome<Self::MessageDelivery>, Self::Error>,
    > + Send {
        assert_eq!(dispatch.request(), request.request());
        self.message_requests.push(request);
        ready(Ok(self
            .message_result
            .take()
            .expect("fixture message result is configured")))
    }
}

fn catalog() -> CompiledToolCatalog {
    let child = session(800);
    let receipt = AwaitSessionReceipt {
        tool_request: request_id(700),
        child,
        mode: DelegationWaitMode::Background,
    };
    SessionDelegationTools::try_new(FakePort::awaiting(
        AwaitSessionPortOutcome::BackgroundRegistered(receipt),
    ))
    .expect("static delegation tools compile")
    .into_parts()
    .0
}

#[track_caller]
fn assert_definition(catalog: &CompiledToolCatalog, name: &str, effect: ToolEffectClass) {
    let definition = catalog
        .definition(&ToolName::try_new(name.to_owned()).expect("fixture name is admitted"))
        .expect("fixture definition exists");
    assert_eq!(definition.permission_default(), ToolPermissionDefault::Auto);
    assert_eq!(definition.effect_class(), effect);
}

#[track_caller]
fn single_spawn_request(port: &FakePort) -> &DelegatedSpawnRequest {
    assert_eq!(port.spawn_requests.len(), 1);
    &port.spawn_requests[0]
}

#[track_caller]
fn single_message_request(port: &FakePort) -> &DelegationMessageRequest {
    assert_eq!(port.message_requests.len(), 1);
    &port.message_requests[0]
}

fn terminal_relation(
    spawning_request: ToolRequest,
    child: SessionId,
    child_turn: TurnId,
    outcome: DelegationOutcome,
) -> (SessionDelegation, DelegationEvent) {
    let spawning_request = decoded_spawn(&spawning_request);
    let spawned = DelegationEvent::Spawned {
        ordinal: DelegationEventOrdinal::new(NonZeroU64::MIN),
        provenance: DelegationProvenance::from_spawn(&spawning_request),
    };
    let outcome_event = DelegationEvent::OutcomeRecorded {
        ordinal: DelegationEventOrdinal::new(
            NonZeroU64::new(2).expect("fixture outcome ordinal is positive"),
        ),
        outcome,
    };
    let relation = SessionDelegationReconstitutionInput::new(
        spawning_request,
        child,
        child_turn,
        vec![spawned, outcome_event.clone()],
    )
    .reconstitute()
    .expect("fixture terminal relationship reconstitutes");
    (relation, outcome_event)
}

fn delivered_result(
    spawning_request: ToolRequest,
    child: SessionId,
    child_turn: TurnId,
    outcome: DelegationOutcome,
    awaiting: &DelegationAwaitRequest,
) -> DeliveredChildResult {
    let (relation, event) = terminal_relation(spawning_request, child, child_turn, outcome);
    let wait = DelegationWait::reconstitute(&relation, awaiting)
        .expect("fixture foreground wait reconstitutes");
    DeliveredChildResult::try_new(wait, &relation, &event)
        .expect("fixture relationship result is deliverable")
}

fn returned_result(awaiting: &DelegationAwaitRequest) -> DeliveredChildResult {
    let child = awaiting.child();
    let child_turn = turn(900);
    let content = DelegationContent::try_new(RETURNED_CONTENT.to_owned())
        .expect("fixture returned content is bounded");
    let outcome = DelegationOutcome::reconstitute(
        DelegationOutcomeKind::ResultReturned,
        Some(content),
        DelegationOutcomeReason::ChildCompleted,
        signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn {
            session: child,
            turn: child_turn,
        },
    )
    .expect("fixture child result is sealed");
    delivered_result(
        background_spawn_for_parent(901, awaiting.request().session()),
        child,
        child_turn,
        outcome,
        awaiting,
    )
}

fn failed_outcome(child: SessionId, child_turn: TurnId) -> DelegationOutcome {
    DelegationOutcome::reconstitute(
        DelegationOutcomeKind::ChildFailed,
        None,
        DelegationOutcomeReason::ChildResultUnavailable,
        signalbox_domain::DelegationProvenanceReconstitutionInput::ChildTurn {
            session: child,
            turn: child_turn,
        },
    )
    .expect("fixture child failure is sealed")
}

#[test]
fn catalog_declares_the_exact_automatic_effect_classes() {
    let catalog = catalog();
    let definitions = catalog.definitions();
    let names: Vec<_> = definitions
        .iter()
        .map(|definition| definition.name().as_str())
        .collect();

    assert_eq!(
        names,
        [
            AWAIT_SESSION_NAME,
            SEND_SESSION_MESSAGE_NAME,
            SPAWN_SESSION_NAME,
        ]
    );
    assert_definition(
        &catalog,
        SPAWN_SESSION_NAME,
        ToolEffectClass::ExternalEffect,
    );
    assert_definition(&catalog, AWAIT_SESSION_NAME, ToolEffectClass::EffectFree);
    assert_definition(
        &catalog,
        SEND_SESSION_MESSAGE_NAME,
        ToolEffectClass::ExternalEffect,
    );
}

#[test]
fn spawn_schema_requires_and_bounds_task() {
    let schema = rendered_contract_schema::<SpawnSessionContract>();

    assert!(
        schema["required"]
            .as_array()
            .expect("spawn required fields are an array")
            .contains(&json!("task"))
    );
    assert_eq!(schema["properties"]["task"]["minLength"], json!(1));
    assert_eq!(
        schema["properties"]["task"]["maxLength"],
        json!(MAX_DELEGATION_CONTENT_BYTES)
    );
}

#[test]
fn spawn_schema_closes_the_root_argument_object() {
    let schema = rendered_contract_schema::<SpawnSessionContract>();

    assert_eq!(schema["additionalProperties"], json!(false));
}

#[test]
fn spawn_schema_requires_and_closes_relationship_variants() {
    let schema = rendered_contract_schema::<SpawnSessionContract>();
    let variants = schema["$defs"]["ChildRelationshipArguments"]["oneOf"]
        .as_array()
        .expect("relationship variants are an array");

    assert!(
        schema["required"]
            .as_array()
            .expect("spawn required fields are an array")
            .contains(&json!("relationship"))
    );
    assert_eq!(
        schema["properties"]["relationship"]["$ref"],
        json!("#/$defs/ChildRelationshipArguments")
    );
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0]["additionalProperties"], json!(false));
    assert_eq!(variants[1]["additionalProperties"], json!(false));
}

#[test]
fn bound_spawn_preserves_the_two_distinct_parent_actions() {
    let raw = bound_spawn(1);
    let spawn = decoded_spawn(&raw);

    assert_eq!(spawn.task().as_str(), TASK);
    assert_eq!(
        spawn.policy(),
        ChildRelationshipPolicy::Bound {
            on_parent_stopped: BoundChildAction::Stop,
            on_parent_cancelled: BoundChildAction::Cancel,
        }
    );
    assert_eq!(spawn.request(), &raw);
}

#[test]
fn await_seals_child_mode_and_logical_request_provenance() {
    let child = session(2);
    let raw = await_request(3, child, "foreground");
    let awaiting = decoded_await(&raw);

    assert_eq!(awaiting.request(), &raw);
    assert_eq!(awaiting.child(), child);
    assert_eq!(awaiting.mode(), DelegationWaitMode::Foreground);
}

#[test]
fn message_seals_peer_content_and_logical_request_provenance() {
    let peer = session(4);
    let raw = message_request(5, peer);
    let message = decoded_message(&raw);

    assert_eq!(message.request(), &raw);
    assert_eq!(message.peer(), peer);
    assert_eq!(message.content().as_str(), MESSAGE);
}

#[test]
fn foreground_decoder_excludes_a_background_wait() {
    let child = session(6);
    let raw = await_request(7, child, "background");

    assert_eq!(foreground_await_request(&raw), None);
}

#[test]
fn foreground_decoder_returns_the_exact_sealed_request() {
    let child = session(8);
    let raw = await_request(9, child, "foreground");
    let awaiting = foreground_await_request(&raw).expect("foreground await is intercepted");

    assert_eq!(awaiting.request(), &raw);
    assert_eq!(awaiting.child(), child);
    assert_eq!(awaiting.mode(), DelegationWaitMode::Foreground);
}

#[test]
fn background_receipt_rejects_a_foreground_wait() {
    let child = session(810);
    let raw = await_request(811, child, "foreground");
    let awaiting = decoded_await(&raw);
    let child_turn = turn(812);
    let (relation, _) = terminal_relation(
        background_spawn_for_parent(813, awaiting.request().session()),
        child,
        child_turn,
        failed_outcome(child, child_turn),
    );
    let wait = DelegationWait::reconstitute(&relation, &awaiting)
        .expect("fixture foreground wait reconstitutes");

    assert_eq!(AwaitSessionReceipt::from_wait(&awaiting, wait), None);
}

#[test]
fn validator_rejects_noncanonical_child_uuid_text() {
    let catalog = catalog();
    let name = ToolName::try_new(AWAIT_SESSION_NAME.to_owned()).expect("fixture name is admitted");
    let invalid = arguments(json!({
        "child_session_id": "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA",
        "mode": "foreground",
    }));

    assert_eq!(
        catalog.validate_arguments(&name, &invalid),
        Err(ToolCatalogValidationFailure::InvalidArguments {
            detail: Some(
                ToolExecutionErrorDetail::try_new(INVALID_ARGUMENTS_DETAIL.to_owned())
                    .expect("static detail is admitted")
            )
        })
    );
}

#[test]
fn validator_rejects_empty_message_content() {
    let catalog = catalog();
    let name =
        ToolName::try_new(SEND_SESSION_MESSAGE_NAME.to_owned()).expect("fixture name is admitted");
    let invalid = arguments(json!({
        "content": "",
        "peer_session_id": session(10).as_uuid().to_string(),
    }));

    assert!(catalog.validate_arguments(&name, &invalid).is_err());
}

#[test]
fn spawn_executor_returns_durable_child_receipt_and_forwards_sealed_request() {
    let raw = background_spawn(11);
    let tool_request = raw.id();
    let child = session(12);
    let receipt = SpawnSessionReceipt {
        tool_request,
        child,
        policy: ChildRelationshipPolicy::Background,
    };
    let (_catalog, mut executor) = SessionDelegationTools::try_new(FakePort::spawning(receipt))
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(&raw).expect("fixture spawn request is canonical");

    let authority = dispatch(&raw, ToolEffectClass::ExternalEffect);
    let disposition =
        run_ready(executor.execute_operation(operation, authority)).expect("spawn succeeds");
    let output: Value = serde_json::from_str(&durably_completed_text(disposition))
        .expect("spawn receipt is compact JSON");
    let port = executor.into_port();
    let observed = single_spawn_request(&port);

    assert_eq!(observed.request(), &raw);
    assert_eq!(output["result"], json!("session_spawned"));
    assert_eq!(
        output["tool_request_id"],
        tool_request.as_uuid().to_string()
    );
    assert_eq!(output["child_session_id"], child.as_uuid().to_string());
    assert_eq!(output["relationship"]["kind"], json!("background"));
}

#[test]
fn background_await_returns_registration_without_child_content() {
    let child = session(13);
    let raw = await_request(14, child, "background");
    let receipt = AwaitSessionReceipt {
        tool_request: raw.id(),
        child,
        mode: DelegationWaitMode::Background,
    };
    let (_catalog, mut executor) = SessionDelegationTools::try_new(FakePort::awaiting(
        AwaitSessionPortOutcome::BackgroundRegistered(receipt),
    ))
    .expect("fixture tools compile")
    .into_parts();
    let operation = decode_operation(&raw).expect("fixture background await is canonical");

    let authority = dispatch(&raw, ToolEffectClass::EffectFree);
    let disposition = run_ready(executor.execute_operation(operation, authority))
        .expect("background registration succeeds");
    let output: Value = serde_json::from_str(&durably_completed_text(disposition))
        .expect("await receipt is compact JSON");

    assert_eq!(output["result"], json!("session_await_registered"));
    assert_eq!(output["tool_request_id"], raw.id().as_uuid().to_string());
    assert_eq!(output["child_session_id"], child.as_uuid().to_string());
    assert_eq!(output["mode"], json!("background"));
    assert_eq!(output.get("content"), None);
}

#[test]
fn already_delivered_foreground_result_retains_exact_child_content() {
    let child = session(15);
    let raw = await_request(16, child, "foreground");
    let awaiting = decoded_await(&raw);
    let result = returned_result(&awaiting);
    let (_catalog, mut executor) = SessionDelegationTools::try_new(FakePort::awaiting(
        AwaitSessionPortOutcome::Delivered(result),
    ))
    .expect("fixture tools compile")
    .into_parts();
    let operation = decode_operation(&raw).expect("fixture foreground await is canonical");

    let authority = dispatch(&raw, ToolEffectClass::EffectFree);
    let disposition = run_ready(executor.execute_operation(operation, authority))
        .expect("already-delivered result succeeds");
    let delivered = foreground_result(disposition);

    assert_eq!(delivered.kind(), DelegationOutcomeKind::ResultReturned);
    assert_eq!(
        delivered
            .content()
            .expect("returned result has content")
            .as_str(),
        RETURNED_CONTENT
    );
}

#[test]
fn delivered_foreground_result_rejects_another_wait_for_the_same_child() {
    let child = session(160);
    let raw = await_request(161, child, "foreground");
    let other_raw = request_for_session(
        162,
        raw.session(),
        AWAIT_SESSION_NAME,
        json!({
            "child_session_id": child.as_uuid().to_string(),
            "mode": "foreground",
        }),
    );
    let other_awaiting = decoded_await(&other_raw);
    let result = returned_result(&other_awaiting);
    let (_catalog, mut executor) = SessionDelegationTools::try_new(FakePort::awaiting(
        AwaitSessionPortOutcome::Delivered(result),
    ))
    .expect("fixture tools compile")
    .into_parts();
    let operation = decode_operation(&raw).expect("fixture foreground await is canonical");

    let authority = dispatch(&raw, ToolEffectClass::EffectFree);
    let result = run_ready(executor.execute_operation(operation, authority));

    assert_port_contract(result);
}

#[test]
fn failed_child_result_retains_reason_and_turn_provenance() {
    let child = session(17);
    let awaiting_raw = await_request(906, child, "foreground");
    let awaiting = decoded_await(&awaiting_raw);
    let outcome = failed_outcome(child, turn(900));
    let result = delivered_result(
        background_spawn_for_parent(902, awaiting.request().session()),
        child,
        turn(900),
        outcome,
        &awaiting,
    );
    let (_catalog, mut executor) = SessionDelegationTools::try_new(FakePort::awaiting(
        AwaitSessionPortOutcome::Delivered(result),
    ))
    .expect("fixture tools compile")
    .into_parts();
    let operation = decode_operation(&awaiting_raw).expect("fixture foreground await is canonical");
    let authority = dispatch(&awaiting_raw, ToolEffectClass::EffectFree);
    let disposition = run_ready(executor.execute_operation(operation, authority))
        .expect("typed child failure succeeds");
    let delivered = foreground_result(disposition);
    let terminal_turn = delivered
        .provenance()
        .child_turn()
        .expect("fixture provenance is a child turn");

    let output: Value = serde_json::from_str(
        &render_delivered_child_result(delivered).expect("typed child failure renders"),
    )
    .expect("child outcome is compact JSON");

    assert_eq!(output["outcome"], json!("failed"));
    assert_eq!(output["reason"], json!("child_result_unavailable"));
    assert_eq!(output["provenance"]["type"], json!("child_turn"));
    assert_eq!(
        output["provenance"]["child_session_id"],
        child.as_uuid().to_string()
    );
    assert_eq!(
        output["provenance"]["child_turn_id"],
        terminal_turn.1.as_uuid().to_string()
    );
}

#[test]
fn delivered_result_rejects_another_relationships_terminal_turn() {
    let child = session(170);
    let local_turn = turn(900);
    let foreign_turn = turn(901);
    let (relation, _) = terminal_relation(
        background_spawn(904),
        child,
        local_turn,
        failed_outcome(child, local_turn),
    );
    let (_, foreign_event) = terminal_relation(
        background_spawn(905),
        child,
        foreign_turn,
        failed_outcome(child, foreign_turn),
    );
    let awaiting_raw = request_for_session(
        907,
        relation.parent(),
        AWAIT_SESSION_NAME,
        json!({
            "child_session_id": child.as_uuid().to_string(),
            "mode": "foreground",
        }),
    );
    let awaiting = decoded_await(&awaiting_raw);
    let wait = DelegationWait::reconstitute(&relation, &awaiting)
        .expect("fixture foreground wait reconstitutes");

    let error = DeliveredChildResult::try_new(wait, &relation, &foreign_event)
        .expect_err("another relationship event is rejected");

    assert_eq!(error.into_parts(), (wait, foreign_event));
}

#[test]
fn stopped_child_result_retains_goal_command_provenance() {
    let child = session(18);
    let awaiting_raw = await_request(908, child, "foreground");
    let awaiting = decoded_await(&awaiting_raw);
    let parent = awaiting.request().session();
    let spawning_request = bound_spawn_for_parent(903, parent);
    let command = DurableCommandId::from_uuid(uuid::Uuid::from_u128(20));
    let generation =
        GoalGeneration::new(NonZeroU64::new(2).expect("fixture generation is positive"));
    let outcome = DelegationOutcome::reconstitute(
        DelegationOutcomeKind::ChildStopped,
        None,
        DelegationOutcomeReason::ParentStopped {
            scope: DescendantTerminationScope::ParentAndDescendants,
        },
        signalbox_domain::DelegationProvenanceReconstitutionInput::ParentGoalCommand {
            session: parent,
            generation,
            command,
        },
    )
    .expect("fixture parent goal command is sealed");
    let result = delivered_result(spawning_request, child, turn(901), outcome, &awaiting);

    let output: Value = serde_json::from_str(
        &render_delivered_child_result(result).expect("typed child stop renders"),
    )
    .expect("child outcome is compact JSON");

    assert_eq!(output["outcome"], json!("stopped"));
    assert_eq!(output["reason"], json!("parent_stopped"));
    assert_eq!(output["provenance"]["type"], json!("parent_goal_command"));
    assert_eq!(
        output["provenance"]["parent_session_id"],
        parent.as_uuid().to_string()
    );
    assert_eq!(
        output["provenance"]["goal_generation"],
        generation.get().to_string()
    );
    assert_eq!(
        output["provenance"]["command_id"],
        command.as_uuid().to_string()
    );
    assert_eq!(
        output["provenance"]["descendant_scope"],
        json!("parent_and_descendants")
    );
}

#[test]
fn message_executor_returns_identity_direction_ordinal_and_delivery_sequence() {
    let peer = session(18);
    let raw = message_request(19, peer);
    let durable_message = message_id(20);
    let ordinal = DelegationEventOrdinal::new(
        NonZeroU64::new(7).expect("fixture message ordinal is positive"),
    );
    let delivery_sequence = NonZeroU64::new(11).expect("fixture delivery sequence is positive");
    let delivery = FakeMessageDelivery {
        tool_request: raw.id(),
        message: durable_message,
        direction: DelegationMessageDirection::ChildToParent,
        ordinal,
        delivery_sequence,
    };
    let (_catalog, mut executor) = SessionDelegationTools::try_new(FakePort::messaging(delivery))
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(&raw).expect("fixture message request is canonical");

    let authority = dispatch(&raw, ToolEffectClass::ExternalEffect);
    let disposition =
        run_ready(executor.execute_operation(operation, authority)).expect("message send succeeds");
    let output: Value = serde_json::from_str(&durably_completed_text(disposition))
        .expect("message receipt is compact JSON");
    let port = executor.into_port();
    let observed = single_message_request(&port);

    assert_eq!(observed.request(), &raw);
    assert_eq!(output["result"], json!("session_message_sent"));
    assert_eq!(output["tool_request_id"], raw.id().as_uuid().to_string());
    assert_eq!(output["message_id"], durable_message.as_uuid().to_string());
    assert_eq!(output["direction"], json!("child_to_parent"));
    assert_eq!(output["ordinal"], json!(ordinal.get()));
    assert_eq!(output["delivery_sequence"], json!(delivery_sequence.get()));
}

#[test]
fn message_executor_rejects_a_delivery_for_another_tool_request() {
    let raw = message_request(29, session(28));
    let delivery = FakeMessageDelivery {
        tool_request: request_id(30),
        message: message_id(31),
        direction: DelegationMessageDirection::ParentToChild,
        ordinal: DelegationEventOrdinal::new(
            NonZeroU64::new(2).expect("fixture message ordinal is positive"),
        ),
        delivery_sequence: NonZeroU64::new(3).expect("fixture delivery sequence is positive"),
    };
    let (_catalog, mut executor) = SessionDelegationTools::try_new(FakePort::messaging(delivery))
        .expect("fixture tools compile")
        .into_parts();
    let operation = decode_operation(&raw).expect("fixture message request is canonical");
    let authority = dispatch(&raw, ToolEffectClass::ExternalEffect);

    assert_port_contract(run_ready(executor.execute_operation(operation, authority)));
}

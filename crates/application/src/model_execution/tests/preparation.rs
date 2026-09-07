use super::{
    AttachmentFailureProvider, AttachmentPreparationFailure, CapturedTelemetry,
    DangerousToolAutoApproval, FakeError, FakeFailure, FakePrepare, FixedIds,
    InProcessAttemptDispatchGate, ModelCallExecutionError, ModelCallExecutionOutcome,
    ModelCallExecutionService, ModelCallId, NormalizedToolArguments, PrepareModelCallOutcome,
    PreparedModelCallFailureCause, RetainedModelCallExecutionState,
    RetainedModelCallExecutionStateKind, RetainedPreparedFailureStatus, ScriptedFailure,
    ScriptedModelCallProvider, ScriptedModelCallStep, SessionId, SessionSystemPrompt,
    ToolEffectClass, ToolName, ToolPermissionDefault, UnusedAuthorization, UnusedFailure,
    UnusedObservation, UnusedProvider, credential_reference, failed_turn_fixture, identity,
    prepared_fixture, ready, ready_with_tool_evidence, tool_round_saturated_fixture,
};

use tracing::instrument::WithSubscriber as _;

/// a newly committed Prepared checkpoint ends the invocation before capability preparation or
/// authorization.
#[tokio::test]
async fn checkpoint_stops_before_every_later_port() {
    let checkpoint = identity(70, ModelCallId::from_uuid);
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(PrepareModelCallOutcome::Checkpointed(checkpoint))].into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        UnusedProvider,
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        service
            .execute(identity(1, SessionId::from_uuid))
            .await
            .expect("checkpointing succeeds"),
        ModelCallExecutionOutcome::Checkpointed(checkpoint)
    );
    let (_, prepare, ..) = service.into_parts();
    assert_eq!(prepare.calls, 1);
}

/// a proven fresh-identity collision retries only the rolled-back prepare transaction with
/// fresh candidates.
#[tokio::test]
async fn prepare_identity_collision_retries_transaction_only() {
    let checkpoint = identity(71, ModelCallId::from_uuid);
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [
                Err(FakeError::IdentityCollision),
                Ok(PrepareModelCallOutcome::Checkpointed(checkpoint)),
            ]
            .into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        UnusedProvider,
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        service
            .execute(identity(1, SessionId::from_uuid))
            .await
            .expect("proven collision is retryable"),
        ModelCallExecutionOutcome::Checkpointed(checkpoint)
    );
    let (_, prepare, ..) = service.into_parts();
    assert_eq!(prepare.calls, 2);
}

/// durable cancellation during capability preparation is
/// authoritative no-work, not a local capability failure to terminalize.
#[tokio::test]
async fn capability_preparation_cancellation_stops_without_failure_commit() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("durable cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, prepare, _, _, _, provider, ..) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
}

#[tokio::test]
async fn prepared_capability_receives_the_configured_tool_catalog_snapshot() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let definition = crate::ToolDefinition::new(
        ToolName::try_new(String::from("current_time")).expect("fixture tool name"),
        String::from("Returns the current UTC time."),
        crate::ToolInputSchema::try_new(String::from(
            r#"{"additionalProperties":false,"properties":{},"type":"object"}"#,
        ))
        .expect("fixture schema"),
        ToolPermissionDefault::Auto,
        ToolEffectClass::EffectFree,
    );
    let catalog = crate::CompiledToolCatalog::try_new([crate::CompiledTool::new(
        definition.clone(),
        |_arguments: &NormalizedToolArguments| Ok(()),
    )])
    .expect("fixture catalog");
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
        InProcessAttemptDispatchGate::default(),
        None,
    )
    .with_tool_catalog(catalog);

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("durable cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, _, _, provider, ..) = service.into_parts();
    assert_eq!(
        provider.last_prepared_tools(),
        Some([definition].as_slice())
    );
}

/// the execution loop presents the prepare transaction's exact frozen-epoch system prompt to
/// the provider port with the capability operation; a promptless epoch presents none.
#[tokio::test]
async fn prepared_capability_receives_the_frozen_epoch_system_prompt() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let prompt = SessionSystemPrompt::try_new(String::from("exact session instructions"))
        .expect("fixture prompt is admissible");
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(PrepareModelCallOutcome::Ready {
                reasoning_provenance: Box::new([]),
                request: Box::new(request.clone()),
                credential_reference: credential_reference(),
                dangerous_tool_auto_approval: DangerousToolAutoApproval::Disabled,
                recorded_user_overrides: Box::new([]),
                system_prompt: Some(prompt.clone()),
                tool_entries: Box::new([]),
            })]
            .into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        service
            .execute(session)
            .await
            .expect("durable cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, _, _, provider, ..) = service.into_parts();
    assert_eq!(
        provider.last_prepared_system_prompt(),
        Some(Some(prompt.as_str()))
    );

    let mut promptless_service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityCancelled]),
        InProcessAttemptDispatchGate::default(),
        None,
    );
    assert_eq!(
        promptless_service
            .execute(session)
            .await
            .expect("durable cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, _, _, provider, ..) = promptless_service.into_parts();
    assert_eq!(provider.last_prepared_system_prompt(), Some(None));
}

/// docs/spec/model-call-execution.md: a trustworthy capability failure
/// survives a failed guarded closure and explicit service decomposition,
/// then resubmits without repeating capability preparation.
#[tokio::test]
async fn capability_failure_commit_retains_evidence_across_handoff() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let turn = request.turn();
    let call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        FakeFailure {
            errors: [FakeError::Infrastructure, FakeError::Infrastructure].into(),
            rereads: [Ok(RetainedPreparedFailureStatus::Pending)].into(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::PreparedFailureCommit(
            FakeError::Infrastructure
        ))
    ));
    assert_eq!(
        service.retained_state(),
        Some(&RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::PreparedFailure {
                session,
                turn,
                call,
                cause: PreparedModelCallFailureCause::CapabilityKnownFailure,
                attachment_failure: None,
            },
        })
    );

    let (
        ids,
        prepare,
        failure,
        authorization,
        observation,
        provider,
        gate,
        catalog,
        retained,
        tool_round_limit,
    ) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.reread_calls, 0);
    assert_eq!(provider.capability_preparation_count(), 1);
    let mut resumed = ModelCallExecutionService::from_parts(
        ids,
        prepare,
        failure,
        authorization,
        observation,
        provider,
        gate,
        catalog,
        retained,
        tool_round_limit,
    );
    assert!(matches!(
        resumed.execute(identity(99, SessionId::from_uuid)).await,
        Err(ModelCallExecutionError::PreparedFailureCommit(
            FakeError::Infrastructure
        ))
    ));
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = resumed.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 2);
    assert_eq!(failure.reread_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(
        retained,
        Some(RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::PreparedFailure {
                session,
                turn,
                call,
                cause: PreparedModelCallFailureCause::CapabilityKnownFailure,
                attachment_failure: None,
            },
        })
    );
}

/// A capability failure that commits terminalizes the turn: the service
/// returns the exact failed turn the transaction recorded and retains
/// nothing to resubmit. Without this the turn stops with no terminal
/// outcome recorded and stays non-terminal forever.
#[tokio::test]
async fn committed_capability_failure_returns_the_recorded_terminal_turn() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    // Captured before the request moves into the fake. `ScriptedFailure`
    // returns the scripted failed turn whatever call it is handed, so a
    // commit addressed to a stale or unrelated call of the same session
    // would satisfy both the outcome comparison and a session-only
    // assertion while terminalizing the wrong call.
    let prepared_call = request.call().id();
    let failed = failed_turn_fixture();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the capability failure commits"),
        ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    // The failure must belong to the prepared turn's own session *and*
    // name the call that was prepared. Counting the attempt alone would
    // accept a terminal turn written against an unrelated fixture session,
    // and checking the session alone would accept one written against
    // another call of this session.
    assert_eq!(failure.recorded.len(), 1);
    let committed = &failure.recorded[0];
    assert_eq!(committed.session, session);
    assert_eq!(
        committed.call, prepared_call,
        "the committed failure must terminalize the prepared call"
    );
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// a typed attachment failure terminalizes the prepared call
/// before durable send authorization or provider interaction, and the
/// exact attachment evidence reaches the guarded failure transaction.
#[tokio::test]
async fn attachment_failure_closes_before_durable_authorization() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let failed = failed_turn_fixture();
    let attachment_failure = AttachmentPreparationFailure::Missing;
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        AttachmentFailureProvider {
            failure: attachment_failure,
            preparation_count: 0,
        },
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the typed attachment failure commits before authorization"),
        ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
    );
    let (_, _, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(failure.calls, 1);
    let committed = &failure.recorded[0];
    assert_eq!(
        committed.cause,
        PreparedModelCallFailureCause::CapabilityKnownFailure
    );
    assert_eq!(committed.attachment_failure, Some(attachment_failure));
    assert_eq!(provider.preparation_count, 1);
    assert!(retained.is_none());
}

/// unavailable attachment verification leaves the exact call
/// prepared without durable failure or send authorization.
#[tokio::test]
async fn attachment_unavailable_leaves_prepared_without_authorization() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        AttachmentFailureProvider {
            failure: AttachmentPreparationFailure::Unavailable,
            preparation_count: 0,
        },
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("unavailable attachment verification is a typed retryable outcome"),
        ModelCallExecutionOutcome::AttachmentUnavailable
    );
    let (_, _, failure, _, _, _, _, _, retained, _) = service.into_parts();
    assert_eq!(failure.calls, 0);
    assert!(retained.is_none());
}

/// An identity collision inside failure closure is retried with fresh
/// identities instead of surfacing as an operator failure, so a raced
/// terminalization still records exactly one terminal turn.
#[tokio::test]
async fn capability_failure_commit_retries_an_identity_collision() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    // Captured before the request moves into the fake, so the retry is
    // compared against the call that was actually prepared rather than only
    // against itself.
    let prepared_call = request.call().id();
    let failed = failed_turn_fixture();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Err(FakeError::IdentityCollision), Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::CapabilityKnownFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the retried capability failure commits"),
        ModelCallExecutionOutcome::CapabilityKnownFailure(Box::new(failed))
    );
    let (_, _, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(failure.calls, 2);
    // Both attempts must address the prepared call — agreeing only with
    // each other would still pass if the service handed the same stale or
    // unrelated call to both — and the second must carry *fresh*
    // identities, which is the whole point of retrying a collision. A
    // retry reusing the colliding identities would collide again forever.
    let [first, second] = failure.recorded.as_slice() else {
        panic!(
            "expected exactly two failure-commit attempts, got {}",
            failure.recorded.len()
        )
    };
    assert_eq!(first.session, session);
    assert_eq!(second.session, session);
    assert_eq!(first.call, prepared_call);
    assert_eq!(second.call, prepared_call);
    // Every component of the bundle has to be refreshed, not just one.
    // Whole-bundle inequality passes when a retry mints a new failure
    // entry but reuses the terminal frontier (or the reverse), and if the
    // reused component is the one that collided the real transaction
    // rejects every retry and the turn stays wedged forever.
    assert_ne!(
        first.identities.failure_entry(),
        second.identities.failure_entry(),
        "a retried identity collision must mint a fresh failure entry"
    );
    assert_ne!(
        first.identities.terminal_frontier(),
        second.identities.terminal_frontier(),
        "a retried identity collision must mint a fresh terminal frontier"
    );
    assert_eq!(provider.capability_preparation_count(), 1);
    assert!(retained.is_none());
}

/// a turn that reaches the automatic tool-round limit closes with its distinct terminal reason
/// before provider entry. This prevents a runaway paid provider loop without misreporting
/// saturation as a capability failure.
#[tokio::test]
async fn tool_round_limit_fires_before_provider_entry() {
    const CONFIGURED_TOOL_ROUND_LIMIT: usize = 7;
    let (request, tool_entries, failed) = tool_round_saturated_fixture(CONFIGURED_TOOL_ROUND_LIMIT);
    let session = request.session();
    // Captured before the request moves into the fake: the committed
    // terminalization has to name *this* saturated call.
    let saturated_call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready_with_tool_evidence(request, tool_entries))].into(),
            calls: 0,
        },
        ScriptedFailure {
            results: [Ok(failed.clone())].into(),
            calls: 0,
            recorded: Vec::new(),
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([]),
        InProcessAttemptDispatchGate::default(),
        Some(CONFIGURED_TOOL_ROUND_LIMIT),
    );
    let captured = CapturedTelemetry::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();

    assert_eq!(
        service
            .execute(session)
            .with_subscriber(subscriber)
            .await
            .expect("the saturated turn closes with its own terminal reason"),
        ModelCallExecutionOutcome::ToolRoundLimitReached(Box::new(failed))
    );
    assert!(
        captured
            .text()
            .contains("terminal_outcome=\"tool_round_limit_reached\""),
        "the service terminalization must expose the tool_round_limit_reached label"
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    // A count alone accepts `fail_prepared` called with an unrelated
    // session or call, which would terminalize something other than the
    // saturated turn while this test still claimed it was closed.
    assert_eq!(failure.recorded.len(), 1);
    let committed = &failure.recorded[0];
    assert_eq!(committed.session, session);
    assert_eq!(committed.call, saturated_call);
    assert_eq!(
        committed.cause,
        PreparedModelCallFailureCause::ToolRoundLimitReached
    );
    assert_eq!(
        provider.capability_preparation_count(),
        0,
        "a saturated turn must not reach provider capability preparation"
    );
    assert_eq!(
        provider.interaction_count(),
        0,
        "a saturated turn must not reach provider interaction"
    );
    assert!(retained.is_none());
}

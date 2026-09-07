use super::{
    Arc, AssistantText, AttemptDispatchGate, BoundaryBlockingProvider, FakeAuthorization,
    FakeError, FakeFailure, FakeObservation, FakePrepare, FixedIds, InProcessAttemptDispatchGate,
    ModelCallAuthorizationReread, ModelCallExecutionError, ModelCallExecutionOutcome,
    ModelCallExecutionService, ModelCallTerminalObservation, NoSendAuthorization,
    PreparedModelCallFailureCause, RetainedModelCallExecutionState,
    RetainedModelCallExecutionStateKind, RetainedModelCallObservationStatus,
    RetainedPreparedFailureStatus, ScriptedModelCallError, ScriptedModelCallProvider,
    ScriptedModelCallStep, SessionId, TurnAttemptId, UnusedAuthorization, UnusedFailure,
    UnusedObservation, VecDeque, identity, prepared_fixture, ready, ready_with_tool_evidence,
    tool_round_saturated_fixture,
};

/// an ambiguous tool-round-limit closure retains its exact cause,
/// then an authoritative reread maps the landed closure to the distinct
/// already-committed outcome without entering the provider.
#[tokio::test]
async fn tool_round_limit_ambiguous_commit_round_trips_retained_cause() {
    const CONFIGURED_TOOL_ROUND_LIMIT: usize = 5;
    let (request, tool_entries, _) = tool_round_saturated_fixture(CONFIGURED_TOOL_ROUND_LIMIT);
    let session = request.session();
    let turn = request.turn();
    let call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready_with_tool_evidence(request, tool_entries))].into(),
            calls: 0,
        },
        FakeFailure {
            errors: [FakeError::CommitAmbiguous].into(),
            rereads: [Ok(RetainedPreparedFailureStatus::AlreadyCommitted)].into(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedAuthorization,
        UnusedObservation,
        ScriptedModelCallProvider::new([]),
        InProcessAttemptDispatchGate::default(),
        Some(CONFIGURED_TOOL_ROUND_LIMIT),
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::PreparedFailureCommit(
            FakeError::CommitAmbiguous
        ))
    ));
    assert_eq!(
        service.retained_state(),
        Some(&RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::PreparedFailure {
                session,
                turn,
                call,
                cause: PreparedModelCallFailureCause::ToolRoundLimitReached,
                attachment_failure: None,
            },
        })
    );
    assert_eq!(
        service
            .execute(identity(99, SessionId::from_uuid))
            .await
            .expect("the reread proves the tool-round closure landed"),
        ModelCallExecutionOutcome::ToolRoundLimitAlreadyCommitted(call)
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.reread_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 0);
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// if an interrupt wins after capability preparation reported a
/// known failure, the retained reread accepts the durable cancellation as
/// authoritative no-work rather than retrying failure closure forever.
#[tokio::test]
async fn capability_failure_race_rereads_cancellation_as_no_work() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        FakeFailure {
            errors: [FakeError::Infrastructure].into(),
            rereads: [Ok(RetainedPreparedFailureStatus::Cancelled)].into(),
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
        service
            .execute(identity(99, SessionId::from_uuid))
            .await
            .expect("the cancellation reread is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.reread_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert!(retained.is_none());
}

/// docs/spec/model-call-execution.md: a commit-ambiguous
/// capability-failure closure is reread before any resubmission, and a
/// landed closure ends reconciliation without repeating credential
/// preparation or the guarded transaction.
#[tokio::test]
async fn ambiguous_capability_failure_commit_is_reread_before_resubmission() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        FakeFailure {
            errors: [FakeError::CommitAmbiguous].into(),
            rereads: [Ok(RetainedPreparedFailureStatus::AlreadyCommitted)].into(),
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
            FakeError::CommitAmbiguous
        ))
    ));
    assert_eq!(
        service
            .execute(identity(99, SessionId::from_uuid))
            .await
            .expect("the authoritative reread proves the closure landed"),
        ModelCallExecutionOutcome::CapabilityFailureAlreadyCommitted(call)
    );
    let (_, prepare, failure, _, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(failure.calls, 1);
    assert_eq!(failure.reread_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert!(retained.is_none());
}

/// docs/spec/model-call-execution.md: send authorization has no fresh
/// candidate to replace after an identity-collision classification, so
/// the same session/call pair is not retried in place.
#[tokio::test]
async fn authorization_identity_collision_returns_without_retrying_same_call() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::IdentityCollision)].into(),
            rereads: VecDeque::new(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::Authorization(
            FakeError::IdentityCollision
        ))
    ));
    let (_, prepare, _, authorization, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 0);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// docs/spec/model-call-execution.md: stale or stopped authority is an
/// ordinary no-send result, not a caller/hub defect and never provider
/// entry.
#[tokio::test]
async fn stale_authorization_returns_no_work_without_provider_entry() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        NoSendAuthorization { calls: 0 },
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("stale authority is a normal no-send result"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, authorization, _, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(authorization.calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// A resumed prepared call constructs one opaque
/// capability, commits InFlight first, and invokes the provider once. An
/// operator failure produces no fabricated observation commit.
#[tokio::test]
async fn resumed_provider_failure_stays_at_provider_stage() {
    let (request, authorized) = prepared_fixture();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Ok(authorized)].into(),
            rereads: VecDeque::new(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedObservation,
        ScriptedModelCallProvider::new([ScriptedModelCallStep::InteractionOperatorFailure]),
        InProcessAttemptDispatchGate::default(),
        None,
    );
    let error = service
        .execute(identity(1, SessionId::from_uuid))
        .await
        .expect_err("the script reports no trustworthy observation");
    assert!(matches!(
        error,
        ModelCallExecutionError::Provider(ScriptedModelCallError::InteractionOperatorFailure)
    ));
    let (_, prepare, _, authorization, _, provider, _, _, _, _) = service.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(authorization.calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 1);
}

/// a non-collision observation failure retains the exact result; later passes authoritatively
/// resubmit it unchanged while absent and stop once the original commit is observed.
#[tokio::test]
async fn failed_observation_commit_is_retained_and_reread() {
    let (request, authorized) = prepared_fixture();
    let call = authorized.call().id();
    let session = authorized.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Ok(authorized)].into(),
            rereads: VecDeque::new(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: [FakeError::Infrastructure, FakeError::Infrastructure].into(),
            rereads: [
                Ok(RetainedModelCallObservationStatus::Pending),
                Ok(RetainedModelCallObservationStatus::AlreadyCommitted),
            ]
            .into(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    let error = service
        .execute(session)
        .await
        .expect_err("the first observation commit fails");
    let retained = match error {
        ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            retained_observation,
        } => retained_observation,
        error => panic!("unexpected failure: {error}"),
    };
    assert_eq!(retained.call(), call);
    assert_eq!(service.retained_observation(), Some(&retained));

    let error = service
        .execute(session)
        .await
        .expect_err("the unchanged resubmission also fails");
    let resubmitted = match error {
        ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            retained_observation,
        } => retained_observation,
        error => panic!("unexpected failure: {error}"),
    };
    assert_eq!(resubmitted, retained);

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the authoritative reread proves the retained commit landed"),
        ModelCallExecutionOutcome::ObservationAlreadyCommitted(call)
    );
    assert!(service.retained_observation().is_none());
    let (_, _, _, authorization, observation, provider, _, _, _, _) = service.into_parts();
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 0);
    assert_eq!(observation.commit_calls, 2);
    assert_eq!(observation.reread_calls, 2);
    assert_eq!(observation.observed, vec![retained.clone(), retained]);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 1);
}

/// when authorization acknowledgement is lost, the still-owned capability proves `invoke` was
/// never entered. An authoritative InFlight reread becomes a correlated known-failure
/// observation without any provider interaction.
#[tokio::test]
async fn ambiguous_authorization_classifies_unconsumed_in_flight() {
    let (request, authorized) = prepared_fixture();
    let call = authorized.call().id();
    let session = authorized.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::CommitAmbiguous)].into(),
            rereads: [Ok(ModelCallAuthorizationReread::InFlight(Box::new(
                authorized,
            )))]
            .into(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: [FakeError::Infrastructure].into(),
            rereads: VecDeque::new(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(String::from("must not be sent"))
                        .expect("fixture text is valid"),
                ],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    let error = service
        .execute(session)
        .await
        .expect_err("the fake non-consumption commit fails visibly");
    let retained = match error {
        ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            retained_observation,
        } => retained_observation,
        error => panic!("unexpected failure: {error}"),
    };
    assert_eq!(retained.call(), call);
    assert_eq!(
        retained.observation(),
        &ModelCallTerminalObservation::KnownFailed
    );
    let (_, _, _, authorization, observation, provider, _, _, _, _) = service.into_parts();
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 1);
    assert_eq!(observation.commit_calls, 1);
    assert_eq!(observation.observed, vec![retained]);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
}

/// an ambiguous authorization reread accepts a complete
/// concurrent direct cancellation of the exact unsent call as
/// authoritative no-work without entering the provider.
#[tokio::test]
async fn ambiguous_authorization_accepts_terminal_cancellation() {
    let (request, _) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::CommitAmbiguous)].into(),
            rereads: [Ok(ModelCallAuthorizationReread::Cancelled)].into(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: VecDeque::new(),
            rereads: VecDeque::new(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert_eq!(
        service
            .execute(session)
            .await
            .expect("the complete terminal cancellation is authoritative"),
        ModelCallExecutionOutcome::NoWork
    );
    let (_, _, _, authorization, observation, provider, _, _, retained, _) = service.into_parts();
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 1);
    assert_eq!(observation.commit_calls, 0);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
    assert!(retained.is_none());
}

/// docs/spec/model-call-execution.md: a failed ambiguous-authorization
/// reread retains the exact non-consumption proof across handoff and
/// later classifies a committed `InFlight` authorization without invoking
/// the provider.
#[tokio::test]
async fn ambiguous_authorization_reread_retains_non_consumption_across_handoff() {
    let (request, authorized) = prepared_fixture();
    let session = request.session();
    let call = request.call().id();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request.clone()))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::CommitAmbiguous)].into(),
            rereads: [
                Err(FakeError::Infrastructure),
                Ok(ModelCallAuthorizationReread::InFlight(Box::new(authorized))),
            ]
            .into(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: [FakeError::Infrastructure].into(),
            rereads: VecDeque::new(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::Completed {
                assistant_text: vec![
                    AssistantText::try_new(String::from("must not be sent"))
                        .expect("fixture text is valid"),
                ],
            },
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::AuthorizationReread {
            authorization_error: FakeError::CommitAmbiguous,
            reread_error: FakeError::Infrastructure,
        })
    ));
    assert!(matches!(
        service.retained_state(),
        Some(RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::AuthorizationNonConsumption {
                session: retained_session,
                prepared,
            },
        }) if *retained_session == session && **prepared == request
    ));

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
    let error = resumed
        .execute(identity(99, SessionId::from_uuid))
        .await
        .expect_err("the retained known-failure observation commit is visible");
    let retained_observation = match error {
        ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            retained_observation,
        } => retained_observation,
        error => panic!("unexpected reconciliation error: {error}"),
    };
    assert_eq!(retained_observation.call(), call);
    assert_eq!(
        retained_observation.observation(),
        &ModelCallTerminalObservation::KnownFailed
    );
    let (_, prepare, _, authorization, observation, provider, _, _, retained, _) =
        resumed.into_parts();
    assert_eq!(prepare.calls, 1);
    assert_eq!(authorization.calls, 1);
    assert_eq!(authorization.reread_calls, 2);
    assert_eq!(observation.commit_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 1);
    assert_eq!(provider.interaction_count(), 0);
    assert!(matches!(
        retained,
        Some(RetainedModelCallExecutionState {
            state: RetainedModelCallExecutionStateKind::TerminalObservation {
                observation,
                ..
            },
        }) if observation.as_ref() == &retained_observation
    ));
}

/// when an ambiguous authorization is proven to have rolled
/// back to Prepared, the unconsumed scripted interaction action can
/// prepare again and still produces exactly one physical interaction.
#[tokio::test]
async fn authorization_rollback_reprepares_one_scripted_interaction_action() {
    let (request, authorized) = prepared_fixture();
    let session = request.session();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request.clone())), Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Err(FakeError::CommitAmbiguous), Ok(authorized)].into(),
            rereads: [
                Err(FakeError::Infrastructure),
                Ok(ModelCallAuthorizationReread::Prepared),
            ]
            .into(),
            calls: 0,
            reread_calls: 0,
        },
        FakeObservation {
            commit_errors: [FakeError::Infrastructure].into(),
            rereads: VecDeque::new(),
            observed: Vec::new(),
            commit_calls: 0,
            reread_calls: 0,
        },
        ScriptedModelCallProvider::new([ScriptedModelCallStep::Return(
            ModelCallTerminalObservation::KnownFailed,
        )]),
        InProcessAttemptDispatchGate::default(),
        None,
    );

    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::AuthorizationReread {
            authorization_error: FakeError::CommitAmbiguous,
            reread_error: FakeError::Infrastructure,
        })
    ));
    assert!(matches!(
        service.execute(session).await,
        Err(ModelCallExecutionError::ObservationCommit {
            error: FakeError::Infrastructure,
            ..
        })
    ));

    let (_, prepare, _, authorization, observation, provider, _, _, _, _) = service.into_parts();
    assert_eq!(prepare.calls, 2);
    assert_eq!(authorization.calls, 2);
    assert_eq!(authorization.reread_calls, 2);
    assert_eq!(observation.commit_calls, 1);
    assert_eq!(provider.capability_preparation_count(), 2);
    assert_eq!(provider.interaction_count(), 1);
    assert_eq!(provider.remaining_step_count(), 0);
}

/// the attempt gate transfers into the provider interaction and is released at its
/// acceptance-capable boundary while the slow terminal response remains pending.
#[tokio::test]
async fn dispatch_gate_releases_at_acceptance_boundary() {
    let (request, authorized) = prepared_fixture();
    let session = authorized.session();
    let attempt = authorized.attempt().id();
    let crossed = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let gate = InProcessAttemptDispatchGate::default();
    let gate_probe = gate.clone();
    let mut service = ModelCallExecutionService::new(
        FixedIds::baseline(),
        FakePrepare {
            outcomes: [Ok(ready(request))].into(),
            calls: 0,
        },
        UnusedFailure,
        FakeAuthorization {
            outcomes: [Ok(authorized)].into(),
            rereads: VecDeque::new(),
            calls: 0,
            reread_calls: 0,
        },
        UnusedObservation,
        BoundaryBlockingProvider {
            crossed: Arc::clone(&crossed),
            finish: Arc::clone(&finish),
            interaction_count: 0,
        },
        gate,
        None,
    );
    {
        let execution = service.execute(session);
        tokio::pin!(execution);

        tokio::select! {
            () = crossed.notified() => {}
            result = &mut execution => panic!("provider returned before boundary probe: {result:?}"),
        }
        let after_boundary = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            gate_probe.acquire(attempt),
        )
        .await
        .expect("the same-attempt gate is released at provider acceptance");
        drop(after_boundary);
        finish.notify_one();
        assert!(matches!(
            execution.as_mut().await,
            Err(ModelCallExecutionError::Provider(FakeError::Infrastructure))
        ));
    }
    let (_, _, _, _, _, provider, _, _, _, _) = service.into_parts();
    assert_eq!(provider.interaction_count, 1);
}

#[test]
fn in_process_gate_serializes_the_same_attempt_but_not_distinct_attempts() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime builds");
    runtime.block_on(async {
        let gate = InProcessAttemptDispatchGate::default();
        let attempt = identity(80, TurnAttemptId::from_uuid);
        let other = identity(81, TurnAttemptId::from_uuid);
        let first = gate.acquire(attempt).await;
        let same = gate.acquire(attempt);
        tokio::pin!(same);
        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut same)
                .await
                .is_err()
        );
        let distinct =
            tokio::time::timeout(std::time::Duration::from_millis(10), gate.acquire(other))
                .await
                .expect("distinct attempts do not block one another");
        drop(distinct);
        drop(first);
        tokio::time::timeout(std::time::Duration::from_millis(10), same)
            .await
            .expect("same attempt proceeds after permit release");
    });
}

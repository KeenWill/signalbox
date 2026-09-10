//! Command replay and pending-successor authority under PostgreSQL transactions.

use super::*;
use signalbox_domain::{
    AbandonLostRunner, AbandonLostRunnerResult, PromotePendingRunner, PromotePendingRunnerResult,
    RunnerEnrollmentRequestId, RunnerEnrollmentState, RunnerRecoveryRejection,
};
use signalbox_persistence::runner_protocol::{
    IssuedRunnerEnrollmentIdentities, PristineRunnerEnrollmentRequest, RunnerRecoveryOutcome,
};

pub(super) fn enrollment_request() -> PristineRunnerEnrollmentRequest {
    PristineRunnerEnrollmentRequest::new(
        RunnerEnrollmentRequestId::from_uuid(Uuid::now_v7()),
        IssuedRunnerEnrollmentIdentities::new(
            RunnerEnrollmentId::from_uuid(Uuid::now_v7()),
            RunnerId::from_uuid(Uuid::now_v7()),
            RunnerAuthenticationId::from_uuid(Uuid::now_v7()),
        ),
        [class()],
        advertisement(),
    )
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_promotion_replays_after_candidate_disconnects() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let predecessor_connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            predecessor_connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let pending_request = enrollment_request();
    let pending_request_id = pending_request.request();
    let candidate = store
        .enroll_pristine(pending_request.clone())
        .await?
        .into_receipt();
    let candidate_connection = store
        .open_connection(candidate.identities().enrollment())
        .await?;
    assert_eq!(
        candidate.enrollment().state(),
        RunnerEnrollmentState::Pending
    );
    let replay = store.enroll_pristine(pending_request).await?.into_receipt();
    assert_eq!(candidate.identities(), replay.identities());
    let command = PromotePendingRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        enrollment_request: pending_request_id,
    };

    let result = store.promote_pending_runner(command.clone()).await?;

    let active_receipt = store
        .promoted_runner_receipt(candidate.identities().enrollment())
        .await?
        .expect("the promoted candidate has a deliverable active receipt");
    assert_eq!(active_receipt.request(), pending_request_id);
    assert_eq!(active_receipt.identities(), candidate.identities());
    assert_eq!(
        active_receipt.enrollment().state(),
        RunnerEnrollmentState::Active
    );

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(PromotePendingRunnerResult::Promoted {
            runner: candidate.identities().runner()
        })
    );
    assert_eq!(
        store
            .load_enrollment(predecessor.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Revoked
    );
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Active
    );
    store
        .transition_connection(
            candidate.identities().enrollment(),
            candidate_connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    assert_eq!(store.promote_pending_runner(command).await?, result);
    let placements: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_session_placement_record")
            .fetch_one(&pool)
            .await?;
    assert_eq!(placements, 0);
    let mut promoted = store
        .load_enrollment(candidate.identities().enrollment())
        .await?
        .unwrap();
    assert!(store.revoke_enrollment(&mut promoted).await?);
    assert_eq!(promoted.state(), RunnerEnrollmentState::Revoked);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pending_enrollment_cannot_change_its_advertisement() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool, catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let changed = RunnerAdvertisement::new([], [], [], [], [], []);

    let result = store
        .resume_registration(
            candidate.request(),
            candidate.identities(),
            candidate.registration().revision(),
            changed,
        )
        .await;

    assert!(matches!(result, Err(RunnerProtocolStoreError::EnrollmentRequest(
        signalbox_persistence::runner_protocol::RunnerEnrollmentRequestFailure::ReplayAdvertisementMismatch { .. }
    ))));
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Pending
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_abandonment_replays_the_original_rejection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let command = AbandonLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: SessionId::from_uuid(Uuid::now_v7()),
    };
    let result = store.abandon_lost_runner(command.clone()).await?;
    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(AbandonLostRunnerResult::Rejected(
            RunnerRecoveryRejection::SessionNotFound
        ))
    );
    insert_session_for(&pool, command.session.into_uuid()).await?;

    assert_eq!(store.abandon_lost_runner(command.clone()).await?, result);
    let conflicting = AbandonLostRunner {
        session: SessionId::from_uuid(Uuid::now_v7()),
        ..command.clone()
    };
    assert_eq!(
        store.abandon_lost_runner(conflicting).await?,
        RunnerRecoveryOutcome::ConflictingReuse
    );
    let other_kind = PromotePendingRunner {
        command_id: command.command_id,
        enrollment_request: RunnerEnrollmentRequestId::from_uuid(Uuid::now_v7()),
    };
    assert_eq!(
        store.promote_pending_runner(other_kind).await?,
        RunnerRecoveryOutcome::ConflictingReuse
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pre_pin_replacement_promotes_and_replays_without_provisioning()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let session = SessionId::from_uuid(uuid(SESSION));
    let placement = SessionRunnerPlacement::new(
        session,
        exact_runner_request(predecessor.identities().runner()),
    );
    store.store_placement(&placement, None, None).await?;
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: None,
    };

    let result = store.replace_lost_runner(command.clone()).await?;

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Replaced {
            runner: candidate.identities().runner(),
            placement_revision: placement.revision().checked_next().unwrap(),
        })
    );
    assert_eq!(store.replace_lost_runner(command).await?, result);
    let stored = store.load_placement(session).await?.unwrap();
    assert_eq!(
        stored.placement().state(),
        &SessionRunnerPlacementState::Unpinned
    );
    assert_eq!(
        stored.placement().request().selector,
        RunnerSelector::Identity(candidate.identities().runner())
    );
    assert_eq!(
        store
            .load_enrollment(candidate.identities().enrollment())
            .await?
            .unwrap()
            .state(),
        RunnerEnrollmentState::Active
    );
    let authorizations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_replacement_provisioning_authorization")
            .fetch_one(&pool)
            .await?;
    assert_eq!(authorizations, 0);
    assert_recovery_event(
        &pool,
        session,
        candidate.identities().runner(),
        stored.placement().revision(),
        DispatchedRunnerState::Replaced,
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_abandonment_terminalizes_the_lost_pre_pin_placement() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    insert_session(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let session = SessionId::from_uuid(uuid(SESSION));
    let placement = SessionRunnerPlacement::new(
        session,
        exact_runner_request(RunnerId::from_uuid(uuid(RUNNER))),
    );
    store.store_placement(&placement, None, None).await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let command = AbandonLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
    };

    let result = store.abandon_lost_runner(command.clone()).await?;

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(AbandonLostRunnerResult::Abandoned)
    );
    assert_eq!(store.abandon_lost_runner(command).await?, result);
    assert!(matches!(
        store
            .load_placement(session)
            .await?
            .unwrap()
            .placement()
            .state(),
        SessionRunnerPlacementState::RunnerAbandoned(_)
    ));
    assert_recovery_event(
        &pool,
        session,
        RunnerId::from_uuid(uuid(RUNNER)),
        placement.revision(),
        DispatchedRunnerState::Abandoned,
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pinned_installation_commits_one_reference_boundary_and_replays()
-> Result<(), Box<dyn Error>> {
    pinned_installation_preserves_seed(PinnedInstallationCase::Ordinary).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pinned_installation_preserves_the_imported_seed_before_the_first_turn()
-> Result<(), Box<dyn Error>> {
    pinned_installation_preserves_seed(PinnedInstallationCase::Imported).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_replaces_a_delegated_session_after_its_runtime_terminal_boundary()
-> Result<(), Box<dyn Error>> {
    pinned_installation_preserves_seed(PinnedInstallationCase::Delegated).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn same_runner_replacement_preserves_an_unchanged_default_directory_transition()
-> Result<(), Box<dyn Error>> {
    same_runner_default_directory_transition(false).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn same_runner_replacement_reports_a_changed_default_directory() -> Result<(), Box<dyn Error>>
{
    same_runner_default_directory_transition(true).await
}

async fn same_runner_default_directory_transition(changed: bool) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, runner, _, pin) = stored_pin_fixture(&pool).await?;
    sqlx::query("INSERT INTO runner_enrollment_request_receipt (request_id, enrollment_id, runner_id, authentication_reference_id, registration_revision) SELECT $1, enrollment_id, runner_id, authentication_reference_id, 1 FROM runner_enrollment WHERE enrollment_id = $2")
        .bind(Uuid::now_v7())
        .bind(runner.enrollment().into_uuid())
        .execute(&pool)
        .await?;
    store.register(&runner, narrowed_advertisement()).await?;
    append_runner_registration_loss_projection(&pool, pin.placement.session()).await?;
    let old_directory = match pin.placement.state() {
        SessionRunnerPlacementState::Pinned(pinned) => pinned.working_directory.clone(),
        _ => panic!("the fixture is pinned"),
    };
    let successor = if changed {
        advertisement()
    } else {
        advertisement().with_default_working_directory(Some(old_directory))
    };
    store.register(&runner, successor).await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: pin.placement.session(),
        revision: None,
    };
    let revision = pin
        .placement
        .revision()
        .checked_next()
        .expect("successor revision fits");
    let result = store.replace_lost_runner(command.clone()).await?;
    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Replaced {
            runner: runner.runner(),
            placement_revision: revision,
        })
    );
    assert_eq!(store.replace_lost_runner(command).await?, result);
    assert_recovery_event(
        &pool,
        pin.placement.session(),
        runner.runner(),
        revision,
        if changed {
            DispatchedRunnerState::WorkingDirectoryChanged
        } else {
            DispatchedRunnerState::Replaced
        },
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn staged_replacement_waits_for_explicit_compaction_observation() -> Result<(), Box<dyn Error>>
{
    pinned_installation_preserves_seed(PinnedInstallationCase::Compaction).await
}

enum PinnedInstallationCase {
    Ordinary,
    Imported,
    Delegated,
    Compaction,
}

async fn pinned_installation_preserves_seed(
    case: PinnedInstallationCase,
) -> Result<(), Box<dyn Error>> {
    let imported = matches!(
        case,
        PinnedInstallationCase::Imported | PinnedInstallationCase::Compaction
    );
    let runtime_terminal = matches!(case, PinnedInstallationCase::Delegated);
    let (_container, pool) = migrated_postgres().await?;
    let seed = ContextFrontierId::from_uuid(Uuid::now_v7());
    let (store, predecessor, _, pin) = if imported {
        use signalbox_application::{
            ImportedConversationConverter, ImportedConversationDropFacts, ImportedConversationStore,
        };
        use signalbox_domain::{
            CreateSessionFromImportedFrontier, ImportedConversationId, ImportedSessionRelationship,
            ImportedTranscriptEntryId,
        };
        use signalbox_persistence::{
            conversation_import::ImportedConversationRepository,
            create_session_from_imported_frontier::ImportedSessionRepository,
        };
        let conversation = signalbox_conversation_import_claude_code::ClaudeCodeJsonlConverter
            .convert(
                ImportedConversationId::from_uuid(Uuid::now_v7()),
                b"{\"type\":\"summary\",\"value\":null}\n{\"type\":\"summary\",\"value\":null}",
                || ImportedTranscriptEntryId::from_uuid(Uuid::now_v7()),
            )
            .expect("the recovery fixture retains its required correlated fact");
        ImportedConversationStore::resolve_or_insert_with_drop_facts(
            &mut ImportedConversationRepository::new(pool.clone()),
            conversation.clone(),
            ImportedConversationDropFacts::none(),
        )
        .await?;
        let command = CreateSessionFromImportedFrontier::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            conversation
                .frontiers()
                .last()
                .expect("the recovery fixture retains its required correlated fact"),
            ImportedSessionRelationship::Resume,
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(uuid(0xa101)),
            )),
        );
        ImportedSessionRepository::new(
            pool.clone(),
            SessionCredentialPin::try_new(vec![SessionModelCredential::new(
                "fixture-model-family",
                "fixture-credential-reference",
            )])
            .expect("the recovery fixture retains its required correlated fact"),
        )
        .handle(command, SessionId::from_uuid(uuid(SESSION)), seed, || {
            SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())
        })
        .await?;
        let (store, predecessor, registration, pin) = prepared_pin_fixture_for_stored_session(
            &pool,
            authorized,
            catalog(),
            no_permission_overrides(),
            "effect_free",
        )
        .await?;
        store.open_connection(predecessor.enrollment()).await?;
        store.store_pin(&pin, &registration).await?;
        (store, predecessor, registration, pin)
    } else {
        stored_pin_fixture(&pool).await?
    };
    let connection = store
        .load_connection(predecessor.enrollment())
        .await?
        .expect("the recovery fixture retains its required correlated fact");
    store
        .transition_connection(
            predecessor.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    if runtime_terminal {
        insert_retired_delegated_wait(
            &pool,
            pin.placement.session(),
            predecessor.runner(),
            pin.placement.revision(),
        )
        .await?;
    }
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: pin.placement.session(),
        revision: None,
    };
    if matches!(case, PinnedInstallationCase::Compaction) {
        let compactions =
            signalbox_persistence::context_compaction::ContextCompactionRepository::new(
                pool.clone(),
            );
        let prepared = prepare_imported_compaction(&pool, command.session).await?;
        assert_eq!(
            store.replace_lost_runner(command.clone()).await?,
            RunnerRecoveryOutcome::Pending
        );
        compactions.authorize(&prepared).await?;
        assert_eq!(
            store.resume_runner_replacement(command.command_id).await?,
            RunnerRecoveryOutcome::Pending
        );
        let entries: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runner_placement_boundary WHERE session_id = $1",
        )
        .bind(command.session.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(entries, 0);
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await?;
        listener.listen("runner_recovery").await?;
        compactions
            .complete(
                &prepared,
                "Imported conversation summary",
                signalbox_domain::ContextCompactionTokenUsage::unreported(),
            )
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv()).await??;
        store.resume_runner_replacements().await?;
    }
    let result = store.replace_lost_runner(command.clone()).await?;

    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Replaced {
            runner: candidate.identities().runner(),
            placement_revision: pin
                .placement
                .revision()
                .checked_next()
                .expect("the recovery fixture retains its required correlated fact")
        })
    );
    assert_eq!(store.replace_lost_runner(command.clone()).await?, result);
    assert_eq!(
        store.resume_runner_replacement(command.command_id).await?,
        result
    );
    let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM semantic_transcript_entry WHERE payload_kind = 'runner_placement_changed'").fetch_one(&pool).await?;
    assert_eq!(entries, 1);
    let member_count: Decimal = sqlx::query_scalar("SELECT frontier.member_count FROM runner_session_placement_frontier AS head JOIN runner_placement_boundary AS boundary USING (session_id, placement_revision) JOIN context_frontier AS frontier ON frontier.owning_session_id = boundary.session_id AND frontier.context_frontier_id = boundary.context_frontier_id WHERE head.session_id = $1").bind(command.session.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(
        member_count,
        Decimal::from(if matches!(case, PinnedInstallationCase::Compaction) {
            4
        } else if imported {
            3
        } else if runtime_terminal {
            2
        } else {
            1
        })
    );
    if runtime_terminal {
        let kinds: Vec<String> = sqlx::query_scalar("SELECT entry.payload_kind FROM runner_session_placement_frontier AS head JOIN runner_placement_boundary AS boundary USING (session_id, placement_revision) CROSS JOIN LATERAL resolve_context_frontier_members(head.session_id, boundary.context_frontier_id) AS member JOIN semantic_transcript_entry AS entry ON entry.source_session_id = member.source_session_id AND entry.semantic_entry_id = member.semantic_entry_id WHERE head.session_id = $1 ORDER BY member.member_position")
            .bind(command.session.into_uuid()).fetch_all(&pool).await?;
        assert_eq!(kinds, ["turn_cancelled", "runner_placement_changed"]);
    }
    if imported {
        let preserved: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM resolve_context_frontier_members($1, $2) AS seed LEFT JOIN runner_session_placement_frontier AS head ON head.session_id = $1 LEFT JOIN runner_placement_boundary AS boundary USING (session_id, placement_revision) LEFT JOIN LATERAL resolve_context_frontier_members($1, boundary.context_frontier_id) AS next ON next.member_position = seed.member_position WHERE seed.source_session_id IS DISTINCT FROM next.source_session_id OR seed.semantic_entry_id IS DISTINCT FROM next.semantic_entry_id)")
            .bind(command.session.into_uuid()).bind(seed.into_uuid()).fetch_one(&pool).await?;
        assert!(preserved);
    }
    let placement = store
        .load_placement(command.session)
        .await?
        .expect("replacement placement is retained");
    let SessionRunnerPlacementState::Pinned(pinned) = placement.placement().state() else {
        panic!("replacement is pinned");
    };
    assert_eq!(
        pinned.working_directory.as_str(),
        "/workspace/successor-default"
    );
    assert!(
        matches!(store.load_placement(command.session).await?.expect("the recovery fixture retains its required correlated fact").placement().state(), SessionRunnerPlacementState::Pinned(pinned) if pinned.runner == candidate.identities().runner())
    );
    assert_recovery_event(
        &pool,
        command.session,
        candidate.identities().runner(),
        placement.placement().revision(),
        DispatchedRunnerState::Replaced,
    )
    .await?;
    if matches!(case, PinnedInstallationCase::Compaction) {
        let suffix: Vec<String> = sqlx::query_scalar("SELECT entry.payload_kind FROM runner_session_placement_frontier AS head JOIN runner_placement_boundary AS boundary USING (session_id, placement_revision) CROSS JOIN LATERAL resolve_context_frontier_members(head.session_id, boundary.context_frontier_id) AS member JOIN semantic_transcript_entry AS entry ON entry.source_session_id = member.source_session_id AND entry.semantic_entry_id = member.semantic_entry_id WHERE head.session_id = $1 ORDER BY member.member_position DESC LIMIT 2")
            .bind(command.session.into_uuid()).fetch_all(&pool).await?;
        assert_eq!(suffix, ["runner_placement_changed", "context_summary"]);
    } else if imported {
        compact_replaced_imported_session(&pool, command.session).await?;
        reject_malformed_placement_entries(&pool, command.session).await?;
    }
    Ok(())
}

async fn prepare_imported_compaction(
    pool: &PgPool,
    session: SessionId,
) -> Result<signalbox_persistence::context_compaction::PreparedContextCompaction, Box<dyn Error>> {
    use signalbox_domain::{ContextCompactionId, ProviderModelIdentity, ResolvedProviderTarget};
    use signalbox_persistence::context_compaction::{
        ContextCompactionRepository, PrepareContextCompactionOutcome,
        PrepareContextCompactionRequest,
    };
    let repository = ContextCompactionRepository::new(pool.clone());
    let PrepareContextCompactionOutcome::Prepared(prepared) = repository
        .prepare(PrepareContextCompactionRequest {
            command: DurableCommandId::from_uuid(Uuid::now_v7()),
            session,
            requested_through_position: Some(2),
            automatic_for_turn: None,
            defaults_version: SessionConfigurationDefaultsVersion::first(),
            selection: DirectModelSelection::from_uuid(uuid(0xa101)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(uuid(0xa159))),
            input_includes_cache_tokens: false,
            credential_reference: String::from("fixture-credential-reference"),
            call: ModelCallId::from_uuid(Uuid::now_v7()),
            compaction: ContextCompactionId::from_uuid(Uuid::now_v7()),
            summary_entry: SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
            result_frontier: ContextFrontierId::from_uuid(Uuid::now_v7()),
        })
        .await?
    else {
        panic!("the imported prefix can be compacted")
    };
    Ok(*prepared)
}

async fn compact_replaced_imported_session(
    pool: &PgPool,
    session: SessionId,
) -> Result<(), Box<dyn Error>> {
    use signalbox_domain::ContextCompactionTokenUsage;
    use signalbox_persistence::context_compaction::ContextCompactionRepository;
    let repository = ContextCompactionRepository::new(pool.clone());
    let prepared = prepare_imported_compaction(pool, session).await?;
    repository.authorize(&prepared).await?;
    repository
        .complete(
            &prepared,
            "Imported conversation summary",
            ContextCompactionTokenUsage::unreported(),
        )
        .await?;
    let kinds: Vec<String> = sqlx::query_scalar("SELECT entry.payload_kind FROM context_compaction AS compaction CROSS JOIN LATERAL resolve_context_frontier_members(compaction.session_id, compaction.result_frontier_id) AS member JOIN semantic_transcript_entry AS entry USING (source_session_id, semantic_entry_id) WHERE compaction.session_id = $1 ORDER BY member.member_position DESC LIMIT 2")
        .bind(session.into_uuid()).fetch_all(pool).await?;
    assert_eq!(kinds, ["context_summary", "runner_placement_changed"]);
    assert!(
        ProcessReadRepository::new(pool.clone())
            .read_transcript(session)
            .await?
            .is_some()
    );
    Ok(())
}

async fn reject_malformed_placement_entries(
    pool: &PgPool,
    session: SessionId,
) -> Result<(), Box<dyn Error>> {
    sqlx::raw_sql("ALTER TABLE semantic_transcript_entry ALTER COLUMN runner_placement_revision TYPE numeric;
        ALTER TABLE semantic_transcript_entry DROP CONSTRAINT semantic_transcript_entry_payload_shape;
        ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;").execute(pool).await?;
    let reader = ProcessReadRepository::new(pool.clone());
    let scheduler = StartEligibleTurnRepository::new(pool.clone());
    let identities = AcceptedInputTurnActivationIdentities::new(
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
        ContextFrontierId::from_uuid(Uuid::now_v7()),
        TurnAttemptId::from_uuid(Uuid::now_v7()),
    );
    assert!(scheduler.preview(session, identities).await?.is_none());
    sqlx::query("UPDATE semantic_transcript_entry SET runner_placement_revision = runner_placement_revision + 0.5 WHERE source_session_id = $1 AND payload_kind = 'runner_placement_changed'")
        .bind(session.into_uuid()).execute(pool).await?;
    assert!(matches!(
        reader.read_transcript(session).await,
        Err(signalbox_persistence::process_read::ProcessReadError::Corruption(_))
    ));
    assert!(matches!(scheduler.preview(session, identities).await,
        Err(signalbox_persistence::start_eligible_turn::StartEligibleTurnRepositoryError::Corruption(_))));
    sqlx::query("UPDATE semantic_transcript_entry SET runner_placement_revision = trunc(runner_placement_revision), assistant_text_value = 'mixed payload' WHERE source_session_id = $1 AND payload_kind = 'runner_placement_changed'")
        .bind(session.into_uuid()).execute(pool).await?;
    assert!(matches!(
        reader.read_transcript(session).await,
        Err(signalbox_persistence::process_read::ProcessReadError::Corruption(_))
    ));
    assert!(matches!(scheduler.preview(session, identities).await,
        Err(signalbox_persistence::start_eligible_turn::StartEligibleTurnRepositoryError::Corruption(_))));
    Ok(())
}

async fn assert_recovery_event(
    pool: &PgPool,
    session: SessionId,
    runner: RunnerId,
    revision: RunnerGeneration,
    state: DispatchedRunnerState,
) -> Result<(), Box<dyn Error>> {
    let mut events = Vec::new();
    let dispatcher = OutboxDispatcher::new(pool.clone());
    loop {
        let outcome = dispatcher
            .dispatch_next(|event| {
                if event.session() == Some(session)
                    && let DispatchedOutboxEventKind::RunnerStateTransition {
                        runner,
                        placement_revision,
                        state,
                        ..
                    } = event.kind()
                    && matches!(
                        state,
                        DispatchedRunnerState::Replaced
                            | DispatchedRunnerState::WorkingDirectoryChanged
                            | DispatchedRunnerState::Abandoned
                    )
                {
                    events.push((*runner, *placement_revision, *state));
                }
                OutboxDeliveryDecision::Delivered
            })
            .await?;
        if outcome == OutboxDispatchOutcome::Idle {
            break;
        }
    }
    assert_eq!(events, [(runner, revision, state)]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_replacement_claims_and_replays_while_a_turn_is_active()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (session, _, _) = insert_running_turn(&pool).await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let predecessor = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let connection = store
        .open_connection(predecessor.identities().enrollment())
        .await?;
    store
        .store_placement(
            &SessionRunnerPlacement::new(
                session,
                exact_runner_request(predecessor.identities().runner()),
            ),
            None,
            None,
        )
        .await?;
    store
        .transition_connection(
            predecessor.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_before_pin_projection(&pool, session).await?;
    let candidate = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session,
        revision: None,
    };
    let result = store.replace_lost_runner(command.clone()).await?;
    assert_eq!(result, RunnerRecoveryOutcome::Pending);
    assert_eq!(store.replace_lost_runner(command).await?, result);
    let stages: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runner_replacement_stage WHERE session_id = $1")
            .bind(session.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(stages, 1);
    Ok(())
}

async fn insert_retired_delegated_wait(
    pool: &PgPool,
    session: SessionId,
    runner: RunnerId,
    revision: RunnerGeneration,
) -> Result<(), sqlx::Error> {
    let turn = TurnId::from_uuid(Uuid::now_v7());
    super::runner_recovery::insert_runner_recovery_turn(
        pool, session, turn, runner, revision, None, None,
    )
    .await?;
    let mut transaction = pool.begin().await?;
    sqlx::query("ALTER TABLE turn_lifecycle DISABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE turn_lifecycle SET delegation_runtime_terminal = true WHERE session_id = $1 AND turn_id = $2").bind(session.into_uuid()).bind(turn.into_uuid()).execute(&mut *transaction).await?;
    let terminal_frontier = Uuid::now_v7();
    let terminal_entry = Uuid::now_v7();
    sqlx::raw_sql(
        "ALTER TABLE session_delegation_logical_terminal DISABLE TRIGGER ALL;
        ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query("INSERT INTO semantic_transcript_entry (source_session_id, semantic_entry_id, payload_kind, cancelled_turn_id) VALUES ($1, $2, 'turn_cancelled', $3)")
        .bind(session.into_uuid()).bind(terminal_entry).bind(turn.into_uuid()).execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO context_frontier (owning_session_id, context_frontier_id, member_count) VALUES ($1, $2, 1)")
        .bind(session.into_uuid()).bind(terminal_frontier).execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO context_frontier_delta (owning_session_id, context_frontier_id, member_position, source_session_id, semantic_entry_id) VALUES ($1, $2, 1, $1, $3)")
        .bind(session.into_uuid()).bind(terminal_frontier).bind(terminal_entry).execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO session_delegation_logical_terminal (spawning_tool_request_id, child_session_id, child_turn_id, root_command_id, terminal_frontier_id, disposition_kind) VALUES ($1, $2, $3, $4, $5, 'cancelled')")
        .bind(Uuid::now_v7()).bind(session.into_uuid()).bind(turn.into_uuid()).bind(Uuid::now_v7()).bind(terminal_frontier).execute(&mut *transaction).await?;
    sqlx::raw_sql(
        "ALTER TABLE session_delegation_logical_terminal ENABLE TRIGGER ALL;
        ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query("ALTER TABLE turn_lifecycle ENABLE TRIGGER ALL")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_abandons_a_delegated_session_after_its_runtime_terminal_boundary()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, predecessor, _, pin) = stored_pin_fixture(&pool).await?;
    let connection = store
        .load_connection(predecessor.enrollment())
        .await?
        .expect("predecessor connection");
    store
        .transition_connection(
            predecessor.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    insert_retired_delegated_wait(
        &pool,
        pin.placement.session(),
        predecessor.runner(),
        pin.placement.revision(),
    )
    .await?;
    let command = AbandonLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: pin.placement.session(),
    };
    let result = store.abandon_lost_runner(command.clone()).await?;
    assert_eq!(
        result,
        RunnerRecoveryOutcome::Recorded(AbandonLostRunnerResult::Abandoned)
    );
    assert_eq!(store.abandon_lost_runner(command).await?, result);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_orderly_shutdown_does_not_admit_a_pristine_successor()
-> Result<(), Box<dyn Error>> {
    for transition in [
        RunnerConnectionTransition::RunnerShutdown,
        RunnerConnectionTransition::DaemonShutdown,
    ] {
        let (_container, pool) = migrated_postgres().await?;
        let store = RunnerProtocolStore::new(pool, catalog());
        let predecessor = store
            .enroll_pristine(enrollment_request())
            .await?
            .into_receipt();
        let connection = store
            .open_connection(predecessor.identities().enrollment())
            .await?;
        store
            .transition_connection(
                predecessor.identities().enrollment(),
                connection.epoch(),
                transition,
            )
            .await?;
        assert!(matches!(store.enroll_pristine(enrollment_request()).await,
            Err(RunnerProtocolStoreError::EnrollmentRequest(
                signalbox_persistence::runner_protocol::RunnerEnrollmentRequestFailure::ActiveEnrollmentExists { .. }
            ))));
        assert_eq!(
            store
                .load_enrollment(predecessor.identities().enrollment())
                .await?
                .expect("the shut-down enrollment remains authoritative")
                .state(),
            RunnerEnrollmentState::Active
        );
    }
    Ok(())
}

enum SuccessorChainCandidate {
    Pending,
    Promoted,
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pinned_session_uses_pending_successor_after_two_losses()
-> Result<(), Box<dyn Error>> {
    replace_through_successor_chain(SuccessorChainCandidate::Pending).await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_pinned_session_uses_promoted_successor_after_two_losses()
-> Result<(), Box<dyn Error>> {
    replace_through_successor_chain(SuccessorChainCandidate::Promoted).await
}

async fn replace_through_successor_chain(
    candidate_state: SuccessorChainCandidate,
) -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, original, _, pin) = stored_pin_fixture(&pool).await?;
    let connection = store
        .load_connection(original.enrollment())
        .await?
        .expect("the original runner is connected");
    store
        .transition_connection(
            original.enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    append_runner_lost_projection(&pool, pin.placement.session()).await?;
    let intermediate_request = enrollment_request();
    let intermediate_request_id = intermediate_request.request();
    let intermediate = store
        .enroll_pristine(intermediate_request)
        .await?
        .into_receipt();
    let connection = store
        .open_connection(intermediate.identities().enrollment())
        .await?;
    assert_eq!(
        store
            .promote_pending_runner(PromotePendingRunner {
                command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                enrollment_request: intermediate_request_id,
            })
            .await?,
        RunnerRecoveryOutcome::Recorded(PromotePendingRunnerResult::Promoted {
            runner: intermediate.identities().runner()
        })
    );
    store
        .transition_connection(
            intermediate.identities().enrollment(),
            connection.epoch(),
            RunnerConnectionTransition::TransportClosed,
        )
        .await?;
    let candidate_request = enrollment_request();
    let candidate_request_id = candidate_request.request();
    let candidate = store
        .enroll_pristine(candidate_request)
        .await?
        .into_receipt();
    store
        .open_connection(candidate.identities().enrollment())
        .await?;
    if matches!(candidate_state, SuccessorChainCandidate::Promoted) {
        assert_eq!(
            store
                .promote_pending_runner(PromotePendingRunner {
                    command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
                    enrollment_request: candidate_request_id,
                })
                .await?,
            RunnerRecoveryOutcome::Recorded(PromotePendingRunnerResult::Promoted {
                runner: candidate.identities().runner()
            })
        );
    }
    let command = signalbox_domain::ReplaceLostRunner {
        command_id: DurableCommandId::from_uuid(Uuid::now_v7()),
        session: pin.placement.session(),
        revision: None,
    };
    let expected =
        RunnerRecoveryOutcome::Recorded(signalbox_domain::ReplaceLostRunnerResult::Replaced {
            runner: candidate.identities().runner(),
            placement_revision: pin
                .placement
                .revision()
                .checked_next()
                .expect("the successor revision fits"),
        });
    assert_eq!(store.replace_lost_runner(command.clone()).await?, expected);
    assert_eq!(store.replace_lost_runner(command).await?, expected);
    let placement = store
        .load_placement(pin.placement.session())
        .await?
        .expect("the replacement placement is durable");
    assert!(
        matches!(placement.placement().state(), SessionRunnerPlacementState::Pinned(pinned) if pinned.runner == candidate.identities().runner())
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn recovery_candidate_authority_changes_publish_committed_wakeups()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = RunnerProtocolStore::new(pool.clone(), catalog());
    let receipt = store
        .enroll_pristine(enrollment_request())
        .await?
        .into_receipt();
    let enrollment = receipt.identities().enrollment();
    let connection = store.open_connection(enrollment).await?;
    let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await?;
    listener.listen("runner_recovery").await?;
    for transition in [
        RunnerConnectionTransition::HeartbeatMissed,
        RunnerConnectionTransition::HeartbeatRecovered,
        RunnerConnectionTransition::TransportClosed,
    ] {
        store
            .transition_connection(enrollment, connection.epoch(), transition)
            .await?;
        let notification = tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, listener.recv()).await??;
        assert_eq!(notification.channel(), "runner_recovery");
    }
    store.open_connection(enrollment).await?;
    tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, listener.recv()).await??;
    store
        .register(receipt.enrollment(), advertisement())
        .await?;
    tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, listener.recv()).await??;
    let mut current = store
        .load_enrollment(enrollment)
        .await?
        .expect("the registration retains its enrollment");
    assert!(store.revoke_enrollment(&mut current).await?);
    tokio::time::timeout(LOCK_COMPLETION_TIMEOUT, listener.recv()).await??;
    Ok(())
}

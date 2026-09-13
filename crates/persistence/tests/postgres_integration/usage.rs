//! PostgreSQL integration proof for bounded dedicated usage projections.

use std::collections::BTreeMap;

use crate::*;
use signalbox_application::{
    UsageAggregateCompleteness, UsageAggregateReport, UsageCacheNormalization, UsageCallEvidence,
    UsageCallOrder, UsageCallPage, UsageCallPageLimit, UsageCallQuery, UsageProvenance, UsageQuery,
    UsageSelection, UsageTimeFromInclusive, UsageTimeRange,
};
use signalbox_persistence::usage::UsageRepository;

const FIRST_INPUT_TOKENS: u64 = 11;
const SECOND_INPUT_TOKENS: u64 = 17;
const THIRD_INPUT_TOKENS: u64 = 23;
const SELECTED_OUTPUT_TOKENS: u64 = 29;

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn initial_session_title_is_claimed_once_preserves_manual_names_and_records_usage()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::{UsageCallKind, UsageCallScope, UsageTokenAxes};
    use signalbox_domain::{Actor, ReplaceSessionMetadata, SessionMetadataContent};
    use signalbox_persistence::{
        session_metadata::SessionMetadataRepository,
        session_titles::{PrepareSessionTitleOutcome, SessionTitleCall, SessionTitleRepository},
    };
    let (container, pool, _) = migrated_postgres().await?;
    let seed = 0x98_000;
    let (fixture, mut model_repository, authorized) =
        authorize_checkpointed_model_call(&pool, seed).await?;
    let titles = SessionTitleRepository::new(pool.clone());
    let mut call = SessionTitleCall {
        call: ModelCallId::from_uuid(Uuid::now_v7()),
        session: fixture.session,
        selection: DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
        target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
            seed + 6,
        ))),
        credential_reference: "codex-title-fixture".to_owned(),
        input_includes_cache_tokens: false,
        initial_for_turn: Some(fixture.turn),
    };
    assert!(
        titles.prepare(&mut call, &Default::default()).await?
            == PrepareSessionTitleOutcome::Ineligible,
        "an active turn must not trigger a title call"
    );
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation(ModelCallTerminalObservation::Completed {
            assistant_text: vec![
                AssistantText::try_new("Database indexing is complete".to_owned())
                    .expect("assistant text"),
            ],
        });
    model_repository
        .commit_observation(
            fixture.session,
            observation,
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                )),
            ),
            |_| TurnId::from_uuid(Uuid::now_v7()),
        )
        .await?;
    let later_turn = TurnId::from_uuid(Uuid::now_v7());
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                seed + 0x80,
                seed + 1,
                "Check the completed indexing work",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::now_v7()),
            Some(later_turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: fixture.session.into_uuid(),
            origin_entry: Uuid::now_v7(),
            starting_frontier: Uuid::now_v7(),
            initial_attempt: Uuid::now_v7(),
        },
    )
    .await?;
    let later_call = ModelCallId::from_uuid(Uuid::now_v7());
    assert!(matches!(
        model_repository
            .prepare_initial_call(
                fixture.session,
                later_call,
                FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                ),
                ContextFrontierId::from_uuid(Uuid::now_v7()),
                |_| (
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    TurnId::from_uuid(Uuid::now_v7())
                ),
            )
            .await?,
        PrepareInitialModelCallOutcome::Checkpointed(_)
    ));
    let AuthorizeModelCallOutcome::Authorized(later_authorized) = model_repository
        .authorize_send(fixture.session, later_call)
        .await?
    else {
        panic!("later call authorizes");
    };
    model_repository
        .commit_observation(
            fixture.session,
            later_authorized
                .observation_correlation()
                .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                    assistant_text: vec![
                        AssistantText::try_new("Index validation complete".to_owned())
                            .expect("assistant text"),
                    ],
                }),
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                    vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                    SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                    ContextFrontierId::from_uuid(Uuid::now_v7()),
                )),
            ),
            |_| TurnId::from_uuid(Uuid::now_v7()),
        )
        .await?;
    for startup in [false, true] {
        let mut abandoned = SessionTitleCall {
            call: ModelCallId::from_uuid(Uuid::now_v7()),
            ..call.clone()
        };
        assert!(
            titles.prepare(&mut abandoned, &Default::default()).await?
                == PrepareSessionTitleOutcome::Prepared
        );
        titles.authorize(abandoned.call).await?;
        if startup {
            titles.abandon_incomplete().await?;
        } else {
            titles.abandon(abandoned.call).await?;
        }
        let retained: (bool, Uuid) = sqlx::query_as(
            "SELECT abandoned, initial_for_turn FROM session_title_model_call
             WHERE model_call_id = $1",
        )
        .bind(abandoned.call.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(retained, (true, fixture.turn.into_uuid()));
    }
    call.initial_for_turn = Some(later_turn);
    assert!(
        titles.prepare(&mut call, &Default::default()).await?
            == PrepareSessionTitleOutcome::Prepared,
        "a later completion claims the initial title after earlier calls were abandoned"
    );
    assert!(
        titles
            .prepare(
                &mut SessionTitleCall {
                    call: ModelCallId::from_uuid(Uuid::now_v7()),
                    ..call.clone()
                },
                &Default::default()
            )
            .await?
            == PrepareSessionTitleOutcome::Ineligible,
        "duplicate completion delivery must not call the model again"
    );
    assert!(
        titles
            .conversation(fixture.session, 4096)
            .await?
            .contains("Database indexing is complete")
    );
    let metadata = SessionMetadataRepository::new(pool.clone());
    let content = SessionMetadataContent::try_new(
        None,
        vec!["work".to_owned()],
        vec![("source".to_owned(), "fixture".to_owned())],
        true,
    )
    .expect("metadata");
    metadata
        .handle(ReplaceSessionMetadata::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            fixture.session,
            content,
        ))
        .await?;
    titles.authorize(call.call).await?;
    let usage = UsageTokenAxes {
        input: Some(17),
        output: Some(5),
        cache_creation_input: None,
        cache_read_input: None,
    };
    let command = DurableCommandId::from_uuid(Uuid::now_v7());
    sqlx::query("ALTER TABLE session_title_model_call ADD CONSTRAINT fixture_reject_title CHECK (title IS NULL)")
        .execute(&pool).await?;
    assert!(
        titles
            .finish_generated(
                command,
                call.call,
                Some("Database indexing work".to_owned()),
                usage
            )
            .await
            .is_err()
    );
    assert!(
        metadata
            .load_session_metadata(fixture.session)
            .await?
            .expect("session")
            .content()
            .title()
            .is_none(),
        "a failed terminal write must roll back the installed title"
    );
    assert!(
        SessionMetadataRepository::for_title_update(pool.clone())
            .load_command(command)
            .await?
            .is_none(),
        "the metadata receipt rolls back with the title"
    );
    let state: String = sqlx::query_scalar(
        "SELECT state_kind FROM session_title_model_call WHERE model_call_id = $1",
    )
    .bind(call.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(state, "in_flight");
    sqlx::query("ALTER TABLE session_title_model_call DROP CONSTRAINT fixture_reject_title")
        .execute(&pool)
        .await?;
    // Hold the metadata lock until both settlements are waiting. Without a call-row
    // lock both readers see in_flight, and the second update loses the terminal race.
    let mut held = pool.begin().await?;
    sqlx::query("SELECT session_id FROM session WHERE session_id = $1 FOR UPDATE")
        .bind(fixture.session.into_uuid())
        .execute(&mut *held)
        .await?;
    let first_titles = titles.clone();
    let first = tokio::spawn(async move {
        first_titles
            .finish_generated(
                command,
                call.call,
                Some("Database indexing work".to_owned()),
                usage,
            )
            .await
    });
    let second_titles = titles.clone();
    let second = tokio::spawn(async move {
        second_titles
            .finish_generated(
                command,
                call.call,
                Some("Database indexing work".to_owned()),
                usage,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'")
                .fetch_one(&pool).await?;
            if waiting >= 2 { break Ok::<_, sqlx::Error>(()); }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await??;
    held.commit().await?;
    assert_eq!(first.await??, Some("Database indexing work".to_owned()));
    assert_eq!(second.await??, Some("Database indexing work".to_owned()));
    let snapshot = metadata
        .load_session_metadata(fixture.session)
        .await?
        .expect("session");
    assert_eq!(snapshot.content().title(), Some("Database indexing work"));
    assert_eq!(snapshot.content().tags().collect::<Vec<_>>(), vec!["work"]);
    assert_eq!(
        snapshot.content().attributes().collect::<Vec<_>>(),
        vec![("source", "fixture")]
    );
    assert!(snapshot.content().archived());
    assert_eq!(
        SessionMetadataRepository::for_title_update(pool.clone())
            .load_command(command)
            .await?
            .expect("generated receipt")
            .command()
            .actor(),
        Actor::Core
    );
    let manual = SessionMetadataContent::try_new(
        Some("My chosen name".to_owned()),
        Vec::new(),
        Vec::new(),
        false,
    )
    .expect("manual name");
    metadata
        .handle(ReplaceSessionMetadata::new(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            fixture.session,
            manual,
        ))
        .await?;
    let mut suggestion = SessionTitleCall {
        call: ModelCallId::from_uuid(Uuid::now_v7()),
        initial_for_turn: None,
        ..call
    };
    assert!(
        titles.prepare(&mut suggestion, &Default::default()).await?
            == PrepareSessionTitleOutcome::Prepared
    );
    titles.authorize(suggestion.call).await?;
    titles
        .finish_generated(
            DurableCommandId::from_uuid(Uuid::now_v7()),
            suggestion.call,
            Some("Suggested new name".to_owned()),
            usage,
        )
        .await?;
    assert_eq!(
        metadata
            .load_session_metadata(fixture.session)
            .await?
            .expect("session")
            .content()
            .title(),
        Some("My chosen name")
    );
    let page = UsageRepository::new(pool.clone())
        .calls(UsageCallQuery {
            scope: UsageQuery {
                time: UsageTimeRange::all(),
                selection: UsageSelection {
                    session: Some(fixture.session),
                    call_kind: Some(UsageCallKind::SessionTitle),
                    ..UsageSelection::all()
                },
            },
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(2).expect("page limit"),
            after: None,
        })
        .await?;
    assert_eq!(page.calls().len(), 2);
    assert_eq!(page.calls()[0].scope, UsageCallScope::SessionTitle);
    assert_eq!(page.calls()[0].tokens.input, Some(17));
    assert_eq!(page.calls()[0].tokens.output, Some(5));
    // Title calls compete for the same registered profiles as ordinary calls.
    use crate::model_call_execution_and_recovery::{
        active_credential_pool_fixture, prepare_and_authorize_pool_call,
    };
    use signalbox_persistence::{
        credential_invocations, model_execution::CredentialPoolRuntimeExhaustion,
    };
    use std::num::NonZeroU32;
    credential_invocations::replace_registrations(
        &pool,
        &[("available-title-home".to_owned(), NonZeroU32::new(1))],
    )
    .await?;
    sqlx::query("INSERT INTO credential_exclusion (kind, profile, origin) VALUES ('profile_quarantine', 'quarantined-title-home', 'codex_home')").execute(&pool).await?;
    let policy = CredentialPoolRuntimePolicy::new(
        "title-pool",
        [
            "quarantined-title-home",
            "displaced-title-home",
            "available-title-home",
        ]
        .into_iter()
        .map(|reference| {
            CredentialPoolRuntimeMember::new(reference, NonZeroU32::new(1).expect("priority"))
        })
        .collect::<Vec<_>>(),
        CredentialPoolRuntimeExhaustion::Fail,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    );
    let pool_seed = seed + 0x1000;
    let (pool_session, pool_turn, pool_repository) = active_credential_pool_fixture(
        &pool,
        pool_seed,
        "title-pool",
        &[
            "displaced-title-home",
            "quarantined-title-home",
            "available-title-home",
        ],
        CredentialPoolRuntimeAction::Stay,
        CredentialPoolRuntimeAction::Stay,
    )
    .await?;
    let pool_target = ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
        Uuid::from_u128(pool_seed + 4),
    ));
    let pools = std::collections::HashMap::from([(pool_target, policy)]);
    let pool_repository = pool_repository.with_credential_pools(pools.clone());
    let (ordinary, _) =
        prepare_and_authorize_pool_call(&pool_repository, pool_session, pool_seed + 100).await?;
    sqlx::query("INSERT INTO credential_pool_chain_exclusion (session_id, turn_id, credential_reference, predecessor_model_call_id, cause_kind) VALUES ($1, $2, 'displaced-title-home', $3, 'overloaded')")
        .bind(pool_session.into_uuid()).bind(pool_turn.into_uuid()).bind(ordinary.call().id().into_uuid()).execute(&pool).await?;
    let mut outside_turn = SessionTitleCall {
        call: ModelCallId::from_uuid(Uuid::now_v7()),
        session: pool_session,
        target: pool_target,
        ..suggestion.clone()
    };
    assert!(
        titles.prepare(&mut outside_turn, &pools).await? == PrepareSessionTitleOutcome::Prepared
    );
    assert_eq!(
        outside_turn.credential_reference, "displaced-title-home",
        "a session-level title does not inherit a turn chain's exclusions"
    );
    titles.finish(outside_turn.call, None, usage).await?;
    sqlx::query("INSERT INTO credential_pool_member_action (pool_name, credential_reference, action_kind, observed_session_id, observed_turn_id, observation_model_call_id, cause_kind) VALUES ('title-pool','displaced-title-home','switch_next_turn',$1,$2,$3,'credential_rejected')")
        .bind(pool_session.into_uuid()).bind(pool_turn.into_uuid()).bind(ordinary.call().id().into_uuid()).execute(&pool).await?;
    suggestion.session = pool_session;
    suggestion.target = pool_target;

    let mut admitted = SessionTitleCall {
        call: ModelCallId::from_uuid(Uuid::now_v7()),
        ..suggestion.clone()
    };
    assert!(titles.prepare(&mut admitted, &pools).await? == PrepareSessionTitleOutcome::Prepared);
    assert_eq!(admitted.credential_reference, "available-title-home");
    let mut contended = SessionTitleCall {
        call: ModelCallId::from_uuid(Uuid::now_v7()),
        ..suggestion
    };
    assert!(
        titles.prepare(&mut contended, &pools).await? == PrepareSessionTitleOutcome::Unavailable,
        "a saturated pool cannot invoke a title model"
    );
    titles
        .finish(
            admitted.call,
            None,
            UsageTokenAxes {
                input: None,
                output: None,
                cache_creation_input: None,
                cache_read_input: None,
            },
        )
        .await?;
    assert!(
        titles.prepare(&mut contended, &pools).await? == PrepareSessionTitleOutcome::Prepared,
        "an uninvoked terminal title releases its reservation"
    );
    titles.authorize(contended.call).await?;
    credential_invocations::register_process(&pool, contended.call, 42, "title-process-fixture")
        .await?;
    titles.finish(contended.call, None, usage).await?;
    assert_eq!(
        credential_invocations::process_group(&pool, contended.call).await?,
        Some((42, "title-process-fixture".to_owned()))
    );
    credential_invocations::release(&pool, contended.call).await?;
    assert_eq!(
        credential_invocations::process_group(&pool, contended.call).await?,
        None
    );
    let mut unsent = SessionTitleCall {
        call: ModelCallId::from_uuid(Uuid::now_v7()),
        ..contended.clone()
    };
    assert!(titles.prepare(&mut unsent, &pools).await? == PrepareSessionTitleOutcome::Prepared);
    titles.authorize(unsent.call).await?;
    titles
        .finish(
            unsent.call,
            None,
            UsageTokenAxes {
                input: None,
                output: None,
                cache_creation_input: None,
                cache_read_input: None,
            },
        )
        .await?;
    let released: bool = sqlx::query_scalar(
        "SELECT released_at IS NOT NULL FROM credential_invocation_reservation WHERE model_call_id = $1",
    )
    .bind(unsent.call.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(
        released,
        "closing an authorized but unsent title releases capacity"
    );
    for in_flight in [false, true] {
        let mut abandoned = SessionTitleCall {
            call: ModelCallId::from_uuid(Uuid::now_v7()),
            ..contended.clone()
        };
        assert!(
            titles.prepare(&mut abandoned, &pools).await? == PrepareSessionTitleOutcome::Prepared
        );
        if in_flight {
            titles.authorize(abandoned.call).await?;
        }
        titles.abandon_incomplete().await?;
        let retained: (String, bool) = sqlx::query_as("SELECT title.state_kind, reservation.released_at IS NOT NULL FROM session_title_model_call title JOIN credential_invocation_reservation reservation USING (model_call_id) WHERE model_call_id = $1")
            .bind(abandoned.call.into_uuid()).fetch_one(&pool).await?;
        assert_eq!(retained, ("terminal".to_owned(), true));
    }
    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn generated_titles_validate_preserved_metadata_and_keep_concurrent_manual_names()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::UsageTokenAxes;
    use signalbox_domain::{ReplaceSessionMetadata, SessionMetadataContent};
    use signalbox_persistence::{
        session_metadata::SessionMetadataRepository,
        session_titles::{PrepareSessionTitleOutcome, SessionTitleCall, SessionTitleRepository},
    };
    let (container, pool, _) = migrated_postgres().await?;
    let titles = SessionTitleRepository::new(pool.clone());
    let metadata = SessionMetadataRepository::new(pool.clone());
    for (index, manual) in [false, true].into_iter().enumerate() {
        let seed = 0x99_000 + index as u128 * 0x100;
        let (fixture, mut model_repository, authorized) =
            authorize_checkpointed_model_call(&pool, seed).await?;
        model_repository
            .commit_observation(
                fixture.session,
                authorized
                    .observation_correlation()
                    .bind_terminal_observation(ModelCallTerminalObservation::Completed {
                        assistant_text: vec![
                            AssistantText::try_new("Indexing complete".to_owned()).expect("text"),
                        ],
                    }),
                signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                    ModelCallTerminalIdentities::Completed(CompletedModelCallIdentities::new(
                        vec![SemanticTranscriptEntryId::from_uuid(Uuid::now_v7())],
                        SemanticTranscriptEntryId::from_uuid(Uuid::now_v7()),
                        ContextFrontierId::from_uuid(Uuid::now_v7()),
                    )),
                ),
                |_| TurnId::from_uuid(Uuid::now_v7()),
            )
            .await?;
        let mut call = SessionTitleCall {
            call: ModelCallId::from_uuid(Uuid::now_v7()),
            session: fixture.session,
            selection: DirectModelSelection::from_uuid(Uuid::from_u128(seed + 5)),
            target: ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(
                Uuid::from_u128(seed + 6),
            )),
            credential_reference: "codex-title-fixture".to_owned(),
            input_includes_cache_tokens: false,
            initial_for_turn: Some(fixture.turn),
        };
        assert!(
            titles.prepare(&mut call, &Default::default()).await?
                == PrepareSessionTitleOutcome::Prepared
        );
        // The metadata writer wins after the initial claim, before model completion.
        let preserved = if manual {
            SessionMetadataContent::try_new(
                Some("My chosen name".to_owned()),
                vec!["work".to_owned()],
                vec![],
                true,
            )
        } else {
            SessionMetadataContent::try_new(
                None,
                vec!["work".to_owned()],
                vec![(
                    "source".to_owned(),
                    "x".repeat(
                        SessionMetadataContent::MAX_TOTAL_UTF8_BYTES
                            - "work".len()
                            - "source".len(),
                    ),
                )],
                true,
            )
        }
        .expect("preserved metadata fits exactly");
        metadata
            .handle(ReplaceSessionMetadata::new(
                DurableCommandId::from_uuid(Uuid::now_v7()),
                fixture.session,
                preserved.clone(),
            ))
            .await?;
        // Both automatic completion and an on-demand suggestion validate the full snapshot.
        for initial in [true, false] {
            if !initial {
                call.call = ModelCallId::from_uuid(Uuid::now_v7());
                call.initial_for_turn = None;
                assert!(
                    titles.prepare(&mut call, &Default::default()).await?
                        == PrepareSessionTitleOutcome::Prepared
                );
            }
            titles.authorize(call.call).await?;
            let result = titles
                .finish_generated(
                    DurableCommandId::from_uuid(Uuid::now_v7()),
                    call.call,
                    Some("Database indexing work".to_owned()),
                    UsageTokenAxes {
                        input: Some(17),
                        output: Some(5),
                        cache_creation_input: None,
                        cache_read_input: None,
                    },
                )
                .await?;
            assert_eq!(
                result.as_deref(),
                manual.then_some("Database indexing work")
            );
            let recorded: (String, Option<String>, Option<rust_decimal::Decimal>) = sqlx::query_as(
                "SELECT state_kind, title, output_tokens FROM session_title_model_call WHERE model_call_id = $1")
                .bind(call.call.into_uuid()).fetch_one(&pool).await?;
            assert_eq!(
                recorded,
                (
                    "terminal".to_owned(),
                    result,
                    Some(rust_decimal::Decimal::from(5))
                )
            );
            assert_eq!(
                metadata
                    .load_session_metadata(fixture.session)
                    .await?
                    .expect("session")
                    .content(),
                &preserved
            );
        }
    }
    pool.close().await;
    drop(container);
    Ok(())
}

async fn terminal_reported_usage_call(
    pool: &PgPool,
    seed: u128,
    usage: ProviderReportedTokenUsage,
) -> Result<RestartModelCallFixture, Box<dyn Error>> {
    let (fixture, mut repository, authorized) =
        authorize_checkpointed_model_call(pool, seed).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation_with_usage(ModelCallTerminalObservation::KnownFailed, usage);
    let outcome = repository
        .commit_observation(
            fixture.session,
            observation,
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x40)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x41)),
                )),
            ),
            |_| TurnId::from_uuid(Uuid::from_u128(seed + 0x42)),
        )
        .await?;
    assert!(matches!(
        outcome,
        Some(ModelCallObservationCommitOutcome::Terminal(_))
    ));
    Ok(fixture)
}

async fn terminal_estimated_usage_call(
    pool: &PgPool,
    seed: u128,
    usage: ProviderReportedTokenUsage,
) -> Result<RestartModelCallFixture, Box<dyn Error>> {
    let (fixture, mut repository, authorized) =
        authorize_checkpointed_model_call(pool, seed).await?;
    install_estimator_fixture(pool, fixture.call).await?;
    let observation = authorized
        .observation_correlation()
        .bind_terminal_observation_with_usage(ModelCallTerminalObservation::KnownFailed, usage);
    let outcome = repository
        .commit_observation(
            fixture.session,
            observation,
            signalbox_application::ModelCallTerminalIdentityCandidates::Exact(
                ModelCallTerminalIdentities::Failed(FailedModelCallTurnIdentities::new(
                    SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0x40)),
                    ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0x41)),
                )),
            ),
            |_| TurnId::from_uuid(Uuid::from_u128(seed + 0x42)),
        )
        .await?;
    assert!(matches!(
        outcome,
        Some(ModelCallObservationCommitOutcome::Terminal(_))
    ));
    Ok(fixture)
}

async fn install_estimator_fixture(pool: &PgPool, call: ModelCallId) -> Result<(), sqlx::Error> {
    let function = format!(
        "CREATE FUNCTION fixture_mark_{suffix}_estimated() RETURNS trigger
         LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.model_call_id = '{call_id}'::uuid AND NEW.state_kind = 'terminal' THEN
                 NEW.usage_provenance_kind = 'estimated';
             END IF;
             RETURN NEW;
         END;
         $$",
        suffix = call.into_uuid().simple(),
        call_id = call.into_uuid(),
    );
    let trigger = format!(
        "CREATE TRIGGER aaa_fixture_mark_{suffix}_estimated
         BEFORE UPDATE ON model_call FOR EACH ROW
         EXECUTE FUNCTION fixture_mark_{suffix}_estimated()",
        suffix = call.into_uuid().simple(),
    );
    // The only interpolated values are canonical UUID renderings produced by
    // this fixture; no caller-provided SQL text reaches either statement.
    sqlx::query(sqlx::AssertSqlSafe(function.as_str()))
        .execute(pool)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(trigger.as_str()))
        .execute(pool)
        .await?;
    Ok(())
}

fn all_usage_query() -> UsageQuery {
    UsageQuery {
        time: UsageTimeRange::all(),
        selection: UsageSelection::all(),
    }
}

fn call_query(limit: u16, after: Option<signalbox_application::UsageCallCursor>) -> UsageCallQuery {
    UsageCallQuery {
        scope: all_usage_query(),
        order: UsageCallOrder::NewestFirst,
        limit: UsageCallPageLimit::new(limit).expect("fixture page limit fits"),
        after,
    }
}

fn evidence_signature(
    calls: &[UsageCallEvidence],
) -> BTreeMap<ModelCallId, (UsageProvenance, Option<u64>)> {
    calls
        .iter()
        .map(|call| (call.call, (call.provenance, call.tokens.input)))
        .collect()
}

fn aggregate_signature(
    report: &UsageAggregateReport,
) -> BTreeMap<(ProviderModelIdentity, UsageProvenance), (u64, Option<u128>)> {
    report
        .groups()
        .iter()
        .map(|group| {
            (
                (group.key().model.identity(), group.key().provenance),
                (group.call_count(), group.tokens().input),
            )
        })
        .collect()
}

fn paged_evidence_signature(
    first: &UsageCallPage,
    second: &UsageCallPage,
) -> BTreeMap<ModelCallId, (UsageProvenance, Option<u64>)> {
    first
        .calls()
        .iter()
        .chain(second.calls())
        .map(|call| (call.call, (call.provenance, call.tokens.input)))
        .collect()
}

fn expected_aggregate_signature(
    calls: &[UsageCallEvidence],
) -> BTreeMap<(ProviderModelIdentity, UsageProvenance), (u64, Option<u128>)> {
    calls
        .iter()
        .map(|call| {
            (
                (call.model.identity(), call.provenance),
                (1, call.tokens.input.map(u128::from)),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn mixed_provenance_aggregates_reconcile_with_exact_paged_call_evidence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first = terminal_reported_usage_call(
        &pool,
        0x91_000,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(FIRST_INPUT_TOKENS)),
    )
    .await?;
    let second = terminal_estimated_usage_call(
        &pool,
        0x92_000,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(SECOND_INPUT_TOKENS)),
    )
    .await?;
    let third = terminal_reported_usage_call(
        &pool,
        0x93_000,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(THIRD_INPUT_TOKENS)),
    )
    .await?;
    let repository = UsageRepository::new(pool.clone());
    let first_page = repository.calls(call_query(2, None)).await?;
    let second_page = repository.calls(call_query(2, first_page.next())).await?;
    let report = repository.aggregate(all_usage_query()).await?;
    let all_calls = [
        first_page.calls()[0].clone(),
        first_page.calls()[1].clone(),
        second_page.calls()[0].clone(),
    ];

    assert_eq!(first_page.calls().len(), 2);
    assert!(first_page.next().is_some());
    assert_eq!(second_page.calls().len(), 1);
    assert_eq!(second_page.next(), None);
    assert_eq!(
        paged_evidence_signature(&first_page, &second_page),
        evidence_signature(&all_calls)
    );
    assert_eq!(
        aggregate_signature(&report),
        expected_aggregate_signature(&all_calls)
    );
    assert_eq!(
        evidence_signature(&all_calls),
        BTreeMap::from([
            (
                first.call,
                (UsageProvenance::Reported, Some(FIRST_INPUT_TOKENS))
            ),
            (
                second.call,
                (UsageProvenance::Estimated, Some(SECOND_INPUT_TOKENS))
            ),
            (
                third.call,
                (UsageProvenance::Reported, Some(THIRD_INPUT_TOKENS))
            ),
        ])
    );
    assert_eq!(report.completeness(), UsageAggregateCompleteness::Complete);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_exact_selection_filters_call_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = terminal_reported_usage_call(
        &pool,
        0x94_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let repository = UsageRepository::new(pool.clone());
    let page = repository
        .calls(UsageCallQuery {
            scope: UsageQuery {
                time: UsageTimeRange::all(),
                selection: UsageSelection {
                    session: Some(fixture.session),
                    turn: Some(fixture.turn),
                    model: None,
                    provenance: Some(UsageProvenance::Reported),
                    call_kind: Some(signalbox_application::UsageCallKind::ModelCall),
                },
            },
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;
    assert_eq!(page.calls().len(), 1);
    assert_eq!(page.calls()[0].call, fixture.call);
    assert_eq!(page.calls()[0].tokens.output, Some(SELECTED_OUTPUT_TOKENS));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn mismatched_session_and_turn_selection_reads_empty_bounded_evidence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let owning = terminal_reported_usage_call(
        &pool,
        0x99_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let foreign = terminal_reported_usage_call(
        &pool,
        0x9a_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let repository = UsageRepository::new(pool.clone());
    let mismatched_scope = UsageQuery {
        time: UsageTimeRange::all(),
        selection: UsageSelection {
            session: Some(foreign.session),
            turn: Some(owning.turn),
            model: None,
            provenance: None,
            call_kind: None,
        },
    };
    let mismatched_page = repository
        .calls(UsageCallQuery {
            scope: mismatched_scope,
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;
    let mismatched_report = repository.aggregate(mismatched_scope).await?;
    let matched_page = repository
        .calls(UsageCallQuery {
            scope: UsageQuery {
                time: UsageTimeRange::all(),
                selection: UsageSelection {
                    session: Some(owning.session),
                    turn: Some(owning.turn),
                    model: None,
                    provenance: None,
                    call_kind: None,
                },
            },
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;

    assert_eq!(mismatched_page.calls().to_vec(), Vec::new());
    assert_eq!(mismatched_page.next(), None);
    assert_eq!(mismatched_report.groups().to_vec(), Vec::new());
    assert_eq!(
        mismatched_report.completeness(),
        UsageAggregateCompleteness::Complete
    );
    assert_eq!(matched_page.calls()[0].call, owning.call);
    assert_eq!(matched_page.calls()[0].session, owning.session);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_half_open_time_range_excludes_earlier_evidence() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = terminal_reported_usage_call(
        &pool,
        0x95_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let repository = UsageRepository::new(pool.clone());
    let page = repository
        .calls(UsageCallQuery {
            scope: all_usage_query(),
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;
    let next_microsecond =
        signalbox_application::UsageTimestampMicros::new(page.calls()[0].recorded_at.get() + 1)?;
    let excluded = repository
        .aggregate(UsageQuery {
            time: UsageTimeRange::new(Some(UsageTimeFromInclusive(next_microsecond)), None)?,
            selection: UsageSelection::all(),
        })
        .await?;

    assert_eq!(page.calls()[0].call, fixture.call);
    assert_eq!(excluded.groups().to_vec(), Vec::new());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn incomplete_cache_inclusive_aggregates_preserve_independent_axes()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x95_100;
    let fixture = terminal_reported_usage_call(
        &pool,
        seed,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(FIRST_INPUT_TOKENS)),
    )
    .await?;
    let source_frontier = Uuid::from_u128(seed + 0x80);
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(fixture.session.into_uuid())
    .bind(source_frontier)
    .execute(&pool)
    .await?;
    let mut connection = pool.acquire().await?;
    insert_completed_context_compaction_call(
        &mut connection,
        Uuid::from_u128(seed + 0x81),
        fixture.session.into_uuid(),
        Uuid::from_u128(seed + 0x82),
        Uuid::from_u128(seed + 0x83),
        source_frontier,
    )
    .await?;
    drop(connection);

    let report = UsageRepository::new(pool.clone())
        .aggregate(UsageQuery {
            time: UsageTimeRange::all(),
            selection: UsageSelection {
                session: Some(fixture.session),
                turn: None,
                model: None,
                provenance: None,
                call_kind: Some(signalbox_application::UsageCallKind::ContextCompaction),
            },
        })
        .await?;

    assert_eq!(report.groups().len(), 1);
    assert_eq!(
        report.groups()[0].cache_normalization(),
        UsageCacheNormalization::Unsafe
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_projection_has_combined_selection_indexes() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_session_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;

    assert!(index_definition.contains("session_id, recorded_at DESC, model_call_id DESC"));
    let combined_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_session_kind_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(
        combined_index_definition
            .contains("session_id, call_kind, recorded_at DESC, model_call_id DESC")
    );
    let turn_kind_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_turn_kind_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(
        turn_kind_index_definition
            .contains("turn_id, call_kind, recorded_at DESC, model_call_id DESC")
    );
    let session_model_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_session_model_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(session_model_index_definition.contains(
        "session_id, resolved_provider_model_identity_id, recorded_at DESC, model_call_id DESC"
    ));
    let model_provenance_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_model_provenance_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(model_provenance_index_definition.contains(
        "resolved_provider_model_identity_id, usage_provenance_kind, recorded_at DESC, model_call_id DESC"
    ));
    let model_kind_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_model_kind_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(model_kind_index_definition.contains(
        "resolved_provider_model_identity_id, call_kind, recorded_at DESC, model_call_id DESC"
    ));
    let session_model_provenance_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_session_model_provenance_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(session_model_provenance_index_definition.contains(
        "session_id, resolved_provider_model_identity_id, usage_provenance_kind, \
         recorded_at DESC, model_call_id DESC"
    ));
    let session_full_selection_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_session_model_provenance_kind_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(session_full_selection_index_definition.contains(
        "session_id, resolved_provider_model_identity_id, usage_provenance_kind, \
         call_kind, recorded_at DESC, model_call_id DESC"
    ));
    let turn_full_selection_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_turn_model_provenance_kind_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(turn_full_selection_index_definition.contains(
        "turn_id, resolved_provider_model_identity_id, usage_provenance_kind, \
         call_kind, recorded_at DESC, model_call_id DESC"
    ));
    let model_provenance_kind_index_definition: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_call_projection'
            AND indexname = 'web_usage_by_model_provenance_kind_recorded_call'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(model_provenance_kind_index_definition.contains(
        "resolved_provider_model_identity_id, usage_provenance_kind, call_kind, \
         recorded_at DESC, model_call_id DESC"
    ));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn projection_rejects_call_kind_contradicting_global_identity() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _repository, _authorized) =
        authorize_checkpointed_model_call(&pool, 0x9c_000).await?;
    let error = sqlx::query(
        "INSERT INTO web_usage_call_projection (
             model_call_id, call_kind, session_id, turn_id,
             resolved_provider_model_identity_id, credential_profile_label,
             usage_provenance_kind, usage_input_includes_cache_tokens
         )
         VALUES ($1, 'context_compaction', $2, NULL, $3, 'exact:guard-test', 'reported', true)",
    )
    .bind(fixture.call.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(0x9c_0f0))
    .execute(&pool)
    .await
    .expect_err("contradicted call kind must be rejected");

    assert!(error.to_string().contains("contradicts identity kind"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn projection_rejects_ownership_contradicting_the_source_call() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let owning = terminal_reported_usage_call(
        &pool,
        0x9d_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let foreign = terminal_reported_usage_call(
        &pool,
        0x9e_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let error = sqlx::query(
        "INSERT INTO web_usage_call_projection (
             model_call_id, call_kind, session_id, turn_id,
             resolved_provider_model_identity_id, credential_profile_label,
             usage_provenance_kind, usage_input_includes_cache_tokens
         )
         VALUES ($1, 'model_call', $2, $3, $4, 'exact:guard-test', 'reported', false)",
    )
    .bind(owning.call.into_uuid())
    .bind(foreign.session.into_uuid())
    .bind(foreign.turn.into_uuid())
    .bind(Uuid::from_u128(0x9d_0f0))
    .execute(&pool)
    .await
    .expect_err("contradicted source ownership must be rejected");

    assert!(error.to_string().contains("contradicts source session"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn projection_rejects_rows_for_nonterminal_source_calls() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (fixture, _repository, _authorized) =
        authorize_checkpointed_model_call(&pool, 0xa1_000).await?;
    let error = sqlx::query(
        "INSERT INTO web_usage_call_projection (
             model_call_id, call_kind, session_id, turn_id,
             resolved_provider_model_identity_id, credential_profile_label,
             usage_provenance_kind, usage_input_includes_cache_tokens
         )
         VALUES ($1, 'model_call', $2, $3, $4, 'exact:guard-test', 'reported', false)",
    )
    .bind(fixture.call.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(Uuid::from_u128(0xa1_0f0))
    .execute(&pool)
    .await
    .expect_err("a projection for a nonterminal source call must be rejected");

    assert!(error.to_string().contains("has no terminal source record"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn projection_rejects_evidence_contradicting_the_source_call() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let fixture = terminal_reported_usage_call(
        &pool,
        0xa2_000,
        ProviderReportedTokenUsage::unreported().with_output_tokens(Some(SELECTED_OUTPUT_TOKENS)),
    )
    .await?;
    let error = sqlx::query(
        "INSERT INTO web_usage_call_projection (
             model_call_id, call_kind, session_id, turn_id,
             resolved_provider_model_identity_id, credential_profile_label,
             usage_provenance_kind, usage_input_includes_cache_tokens
         )
         VALUES ($1, 'model_call', $2, $3, $4, 'exact:fabricated', 'reported', false)",
    )
    .bind(fixture.call.into_uuid())
    .bind(fixture.session.into_uuid())
    .bind(fixture.turn.into_uuid())
    .bind(Uuid::from_u128(0xa2_0f0))
    .execute(&pool)
    .await
    .expect_err("fabricated projection evidence must be rejected");

    assert!(
        error
            .to_string()
            .contains("contradicts its terminal source record")
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn projection_timestamps_are_bounded_to_the_shared_representable_range()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let recorded_at_constraint: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
           FROM pg_constraint
          WHERE conrelid = 'web_usage_call_projection'::regclass
            AND conname = 'web_usage_recorded_at_representable'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(recorded_at_constraint.contains("1970-01-01"));
    assert!(recorded_at_constraint.contains("9999-12-31"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn context_compaction_usage_axes_have_the_canonical_u64_ceiling() -> Result<(), Box<dyn Error>>
{
    let (container, pool, _database_url) = migrated_postgres().await?;
    let compaction_usage_constraint: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid)
           FROM pg_constraint
          WHERE conrelid = 'context_compaction_model_call'::regclass
            AND conname = 'context_compaction_model_call_usage_u64'",
    )
    .fetch_one(&pool)
    .await?;
    assert!(compaction_usage_constraint.contains("18446744073709551615"));
    assert!(compaction_usage_constraint.contains("trunc(input_tokens)"));
    assert!(compaction_usage_constraint.contains("trunc(output_tokens)"));
    assert!(compaction_usage_constraint.contains("trunc(cache_read_input_tokens)"));
    assert!(compaction_usage_constraint.contains("trunc(cache_creation_input_tokens)"));

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn context_compaction_usage_axes_stay_integral_at_the_column_type()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let input_scale: i32 = sqlx::query_scalar(
        "SELECT numeric_scale FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND table_name = 'context_compaction_model_call'
            AND column_name = 'input_tokens'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(input_scale, 0);
    let output_scale: i32 = sqlx::query_scalar(
        "SELECT numeric_scale FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND table_name = 'context_compaction_model_call'
            AND column_name = 'output_tokens'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(output_scale, 0);
    let cache_read_scale: i32 = sqlx::query_scalar(
        "SELECT numeric_scale FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND table_name = 'context_compaction_model_call'
            AND column_name = 'cache_read_input_tokens'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(cache_read_scale, 0);
    let cache_creation_scale: i32 = sqlx::query_scalar(
        "SELECT numeric_scale FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND table_name = 'context_compaction_model_call'
            AND column_name = 'cache_creation_input_tokens'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(cache_creation_scale, 0);

    pool.close().await;
    drop(container);
    Ok(())
}

/// The projection reads whatever input semantics the canonical compaction call
/// pinned, so it must observe the semantics contract owned by
/// 202608210611_context_compaction_input_semantics.sql rather than restate one.
/// That migration keeps the column nullable so pre-migration rows retain their
/// unknown meaning, defaults newly inserted calls to cache-exclusive, and makes
/// the pinned value immutable. The daemon's only write path
/// (`ContextCompactionRepository::prepare`) always binds the value explicitly;
/// this exercises the raw-insert edges that path does not reach.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn context_compaction_input_semantics_default_new_calls_and_stay_immutable()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let compaction_semantics_nullable: bool = sqlx::query_scalar(
        "SELECT NOT attnotnull
           FROM pg_attribute
          WHERE attrelid = 'context_compaction_model_call'::regclass
            AND attname = 'usage_input_includes_cache_tokens'
            AND NOT attisdropped",
    )
    .fetch_one(&pool)
    .await?;
    assert!(compaction_semantics_nullable);
    let seed = 0x98_000;
    let fixture =
        terminal_reported_usage_call(&pool, seed, ProviderReportedTokenUsage::unreported()).await?;
    let source_frontier = Uuid::from_u128(seed + 0x80);
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(fixture.session.into_uuid())
    .bind(source_frontier)
    .execute(&pool)
    .await?;

    let defaulted_call = Uuid::from_u128(seed + 0x81);
    sqlx::query(
        "INSERT INTO context_compaction_model_call
            (model_call_id, session_id, direct_model_selection_id,
             resolved_provider_model_identity_id, source_frontier_id,
             credential_reference, state_kind)
         VALUES ($1, $2, $3, $4, $5, 'semantic-pin-fixture', 'prepared')",
    )
    .bind(defaulted_call)
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0x82))
    .bind(Uuid::from_u128(seed + 0x83))
    .bind(source_frontier)
    .execute(&pool)
    .await?;
    let defaulted_semantics: Option<bool> = sqlx::query_scalar(
        "SELECT usage_input_includes_cache_tokens
           FROM context_compaction_model_call
          WHERE model_call_id = $1",
    )
    .bind(defaulted_call)
    .fetch_one(&pool)
    .await?;
    assert_eq!(defaulted_semantics, Some(false));

    let call = Uuid::from_u128(seed + 0x84);
    sqlx::query(
        "INSERT INTO context_compaction_model_call
            (model_call_id, session_id, direct_model_selection_id,
             resolved_provider_model_identity_id, source_frontier_id,
             credential_reference, usage_input_includes_cache_tokens, state_kind)
         VALUES ($1, $2, $3, $4, $5, 'semantic-pin-fixture', true, 'prepared')",
    )
    .bind(call)
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0x82))
    .bind(Uuid::from_u128(seed + 0x83))
    .bind(source_frontier)
    .execute(&pool)
    .await?;
    let changed_semantics_error = sqlx::query(
        "UPDATE context_compaction_model_call
            SET usage_input_includes_cache_tokens = false
          WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(&pool)
    .await
    .expect_err("pinned compaction input semantics must be immutable");
    assert_eq!(
        changed_semantics_error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("context_compaction_input_semantics_immutable")
    );
    let retained_semantics: bool = sqlx::query_scalar(
        "SELECT usage_input_includes_cache_tokens
           FROM context_compaction_model_call
          WHERE model_call_id = $1",
    )
    .bind(call)
    .fetch_one(&pool)
    .await?;
    assert!(retained_semantics);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_projection_records_terminal_statement_time() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let recorded_at_default: String = sqlx::query_scalar(
        "SELECT pg_get_expr(adbin, adrelid)
           FROM pg_attrdef
          WHERE adrelid = 'web_usage_call_projection'::regclass
            AND adnum = (
                SELECT attnum
                  FROM pg_attribute
                 WHERE attrelid = 'web_usage_call_projection'::regclass
                   AND attname = 'recorded_at'
                   AND NOT attisdropped
            )",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(recorded_at_default, "statement_timestamp()");

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oversized_credential_references_receive_bounded_distinct_usage_labels()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let first = "a".repeat(257);
    let second = format!("{}b", "a".repeat(256));
    let labels: (String, String) =
        sqlx::query_as("SELECT bounded_web_usage_profile($1), bounded_web_usage_profile($2)")
            .bind(&first)
            .bind(&second)
            .fetch_one(&pool)
            .await?;

    assert!(labels.0.len() <= 256);
    assert!(labels.1.len() <= 256);
    assert_ne!(labels.0, labels.1);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT bounded_web_usage_profile($1)")
            .bind("within-bound")
            .fetch_one(&pool)
            .await?,
        "exact:within-bound"
    );
    let oversized = "z".repeat(257);
    let mapped_label: String = sqlx::query_scalar("SELECT bounded_web_usage_profile($1)")
        .bind(&oversized)
        .fetch_one(&pool)
        .await?;
    assert!(mapped_label.starts_with("mapped:"));
    let repeated_label: String = sqlx::query_scalar("SELECT bounded_web_usage_profile($1)")
        .bind(&oversized)
        .fetch_one(&pool)
        .await?;
    assert_eq!(repeated_label, mapped_label);
    let exact_label: String = sqlx::query_scalar("SELECT bounded_web_usage_profile($1)")
        .bind(&mapped_label)
        .fetch_one(&pool)
        .await?;
    assert_ne!(mapped_label, exact_label);
    let incompressible = (0..4_096_u32)
        .map(|value| format!("{value:08x}"))
        .collect::<String>();
    let incompressible_label: String = sqlx::query_scalar("SELECT bounded_web_usage_profile($1)")
        .bind(&incompressible)
        .fetch_one(&pool)
        .await?;
    assert!(incompressible_label.starts_with("mapped:"));
    let mapping_indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema()
            AND tablename = 'web_usage_oversized_profile_identity'",
    )
    .fetch_all(&pool)
    .await?;
    assert!(
        mapping_indexes
            .iter()
            .all(|index| !index.contains("exact_reference"))
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oversized_profile_identity_enforces_digest_and_reference_uniqueness()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let reference = "table-boundary-profile".repeat(16);
    let mismatched_digest_error = sqlx::query(
        "INSERT INTO web_usage_oversized_profile_identity
            (reference_digest, exact_reference)
         VALUES ('00000000000000000000000000000000', $1)",
    )
    .bind(&reference)
    .execute(&pool)
    .await
    .expect_err("table boundary must reject a digest unrelated to the reference");
    assert_eq!(
        mismatched_digest_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23514".into())
    );

    sqlx::query(
        "INSERT INTO web_usage_oversized_profile_identity
            (reference_digest, exact_reference)
         VALUES (md5($1), $1)",
    )
    .bind(&reference)
    .execute(&pool)
    .await?;
    let duplicate_error = sqlx::query(
        "INSERT INTO web_usage_oversized_profile_identity
            (reference_digest, exact_reference)
         VALUES (md5($1), $1)",
    )
    .bind(&reference)
    .execute(&pool)
    .await
    .expect_err("table boundary must reject a duplicate exact reference");
    assert_eq!(
        duplicate_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some("23505".into())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_projection_retains_only_bounded_credential_identity() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let projection_retains_exact_reference: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM information_schema.columns
              WHERE table_schema = current_schema()
                AND table_name = 'web_usage_call_projection'
                AND column_name = 'credential_reference'
         )",
    )
    .fetch_one(&pool)
    .await?;
    assert!(!projection_retains_exact_reference);

    let fixture = terminal_reported_usage_call(
        &pool,
        0x95_900,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(FIRST_INPUT_TOKENS)),
    )
    .await?;
    let repository = UsageRepository::new(pool.clone());
    let page = repository.calls(call_query(1, None)).await?;
    let report = repository.aggregate(all_usage_query()).await?;

    assert_eq!(page.calls()[0].call, fixture.call);
    assert_eq!(
        page.calls()[0].credential_reference.as_deref(),
        Some(model_credential_reference().as_str())
    );
    assert_eq!(
        report.groups()[0].key().credential_reference.as_deref(),
        Some(model_credential_reference().as_str())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

/// Drives one session-level compaction call terminal with the supplied
/// credential reference so the canonical trigger projects exactly that text.
/// The projection cannot be written directly: the source-correlation guard
/// fails any row without a matching terminal canonical record closed.
async fn terminal_compaction_call_with_reference(
    pool: &PgPool,
    seed: u128,
    reference: &str,
) -> Result<SessionId, Box<dyn Error>> {
    let fixture = terminal_reported_usage_call(
        pool,
        seed,
        ProviderReportedTokenUsage::unreported().with_input_tokens(Some(FIRST_INPUT_TOKENS)),
    )
    .await?;
    let source_frontier = Uuid::from_u128(seed + 0x80);
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(fixture.session.into_uuid())
    .bind(source_frontier)
    .execute(pool)
    .await?;
    let call = Uuid::from_u128(seed + 0x81);
    sqlx::query(
        "INSERT INTO context_compaction_model_call
            (model_call_id, session_id, direct_model_selection_id,
             resolved_provider_model_identity_id, source_frontier_id,
             credential_reference, usage_input_includes_cache_tokens, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6, false, 'prepared')",
    )
    .bind(call)
    .bind(fixture.session.into_uuid())
    .bind(Uuid::from_u128(seed + 0x82))
    .bind(Uuid::from_u128(seed + 0x83))
    .bind(source_frontier)
    .bind(reference)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE context_compaction_model_call
         SET state_kind = 'in_flight', in_flight_at = clock_timestamp()
         WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE context_compaction_model_call
         SET state_kind = 'terminal', terminal_at = clock_timestamp(),
             terminal_disposition_kind = 'completed',
             input_tokens = 11, output_tokens = 5
         WHERE model_call_id = $1",
    )
    .bind(call)
    .execute(pool)
    .await?;
    Ok(fixture.session)
}

fn compaction_scope_query(session: SessionId) -> UsageQuery {
    UsageQuery {
        time: UsageTimeRange::all(),
        selection: UsageSelection {
            session: Some(session),
            turn: None,
            model: None,
            provenance: None,
            call_kind: Some(signalbox_application::UsageCallKind::ContextCompaction),
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_reads_reconstruct_a_mapped_reference_within_the_profile_ceiling()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    // 251 bytes: above the 250-byte exact-label bound, within the 256-byte
    // configured-profile ceiling, so reads reconstruct it from the mapping.
    let reference = "r".repeat(251);
    let session = terminal_compaction_call_with_reference(&pool, 0x95_a00, &reference).await?;
    let repository = UsageRepository::new(pool.clone());
    let page = repository
        .calls(UsageCallQuery {
            scope: compaction_scope_query(session),
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;
    let report = repository
        .aggregate(compaction_scope_query(session))
        .await?;

    assert!(
        page.calls()[0]
            .credential_profile
            .as_str()
            .starts_with("mapped:")
    );
    assert_eq!(
        page.calls()[0].credential_reference.as_deref(),
        Some(reference.as_str())
    );
    assert_eq!(
        report.groups()[0].key().credential_reference.as_deref(),
        Some(reference.as_str())
    );

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn usage_reads_report_an_over_ceiling_reference_instead_of_materializing_it()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    // 257 bytes: beyond the 256-byte configured-profile ceiling, so no
    // configured profile can match and reads never copy the reference out of
    // the mapping.
    let reference = "s".repeat(257);
    let session = terminal_compaction_call_with_reference(&pool, 0x95_b00, &reference).await?;
    let repository = UsageRepository::new(pool.clone());
    let page = repository
        .calls(UsageCallQuery {
            scope: compaction_scope_query(session),
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;
    let report = repository
        .aggregate(compaction_scope_query(session))
        .await?;

    assert!(
        page.calls()[0]
            .credential_profile
            .as_str()
            .starts_with("mapped:")
    );
    assert_eq!(page.calls()[0].credential_reference, None);
    assert_eq!(report.groups()[0].key().credential_reference, None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_approval_judge_usage_enters_dedicated_call_evidence() -> Result<(), Box<dyn Error>>
{
    const JUDGE_INPUT_TOKENS: u64 = 31;
    const JUDGE_OUTPUT_TOKENS: u64 = 7;

    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x96_000;
    let (fixture, model_repository, _, _) = checkpoint_tool_batch_with_approval(
        &pool,
        seed,
        APPROVAL_PROPOSAL,
        InitialToolApproval::Delegated,
    )
    .await?;
    let repository = model_repository.approval_judge_repository();
    let judge_call = ModelCallId::from_uuid(Uuid::from_u128(seed + 0xe0));
    let prepared = ready_approval_judge(
        repository
            .prepare(fixture.session, fixture.turn, judge_call, None)
            .await?,
    );
    let rationale = ToolDecisionRationale::try_new(String::from(APPROVAL_JUDGE_RATIONALE))?;

    repository.authorize(&prepared).await?;
    repository
        .complete(
            &prepared,
            DelegateApprovalRecommendation::Approve,
            rationale,
            ProviderReportedTokenUsage::unreported()
                .with_input_tokens(Some(JUDGE_INPUT_TOKENS))
                .with_output_tokens(Some(JUDGE_OUTPUT_TOKENS)),
            ApprovalJudgeCompletionIdentities::new(
                TurnAttemptId::from_uuid(Uuid::from_u128(seed + 0xe1)),
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(seed + 0xe2)),
                ContextFrontierId::from_uuid(Uuid::from_u128(seed + 0xe3)),
            ),
            |request| {
                SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(
                    request.as_uuid().as_u128() + 0x2_000_000,
                ))
            },
        )
        .await?;
    let page = UsageRepository::new(pool.clone())
        .calls(UsageCallQuery {
            scope: UsageQuery {
                time: UsageTimeRange::all(),
                selection: UsageSelection {
                    session: Some(fixture.session),
                    turn: Some(fixture.turn),
                    model: None,
                    provenance: Some(UsageProvenance::Reported),
                    call_kind: Some(signalbox_application::UsageCallKind::ApprovalJudge),
                },
            },
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;

    assert_eq!(page.calls().len(), 1);
    assert_eq!(page.calls()[0].call, judge_call);
    assert_eq!(
        page.calls()[0].scope,
        signalbox_application::UsageCallScope::ApprovalJudge(fixture.turn)
    );
    assert_eq!(page.calls()[0].tokens.input, Some(JUDGE_INPUT_TOKENS));
    assert_eq!(page.calls()[0].tokens.output, Some(JUDGE_OUTPUT_TOKENS));
    assert_eq!(page.calls()[0].tokens.cache_creation_input, None);
    assert_eq!(page.calls()[0].tokens.cache_read_input, None);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_context_compaction_usage_enters_session_level_call_evidence()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let seed = 0x97_000;
    let fixture =
        terminal_reported_usage_call(&pool, seed, ProviderReportedTokenUsage::unreported()).await?;
    let source_frontier = Uuid::from_u128(seed + 0x80);
    let compaction_call = Uuid::from_u128(seed + 0x81);
    let mut connection = pool.acquire().await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, 0)",
    )
    .bind(fixture.session.into_uuid())
    .bind(source_frontier)
    .execute(&mut *connection)
    .await?;
    insert_completed_context_compaction_call(
        &mut connection,
        compaction_call,
        fixture.session.into_uuid(),
        Uuid::from_u128(seed + 0x82),
        Uuid::from_u128(seed + 0x83),
        source_frontier,
    )
    .await?;

    let page = UsageRepository::new(pool.clone())
        .calls(UsageCallQuery {
            scope: UsageQuery {
                time: UsageTimeRange::all(),
                selection: UsageSelection {
                    session: Some(fixture.session),
                    turn: None,
                    model: None,
                    provenance: Some(UsageProvenance::Reported),
                    call_kind: Some(signalbox_application::UsageCallKind::ContextCompaction),
                },
            },
            order: UsageCallOrder::NewestFirst,
            limit: UsageCallPageLimit::new(1).expect("fixture page limit fits"),
            after: None,
        })
        .await?;

    assert_eq!(page.calls().len(), 1);
    assert_eq!(page.calls()[0].call.into_uuid(), compaction_call);
    assert_eq!(
        page.calls()[0].scope,
        signalbox_application::UsageCallScope::ContextCompaction
    );
    assert_eq!(page.calls()[0].tokens.input, Some(17));
    assert_eq!(page.calls()[0].tokens.output, Some(5));
    assert_eq!(
        page.calls()[0].input_semantics,
        signalbox_application::UsageInputTokenSemantics::CacheInclusive
    );

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

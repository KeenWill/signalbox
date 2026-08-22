//! PostgreSQL integration proof for bounded indexed lexical search.

use std::error::Error;

use signalbox_application::{
    SearchArtifactId, SearchArtifactProjection, SearchArtifactProjectionClass, SearchContentClass,
    SearchPageLimit, SearchProjectionText, SearchQuery, SearchResultSource, SearchScope,
    SearchStrategy, SearchText, TimelineAddress, TimelineWindowAnchor, TimelineWindowLimits,
    max_search_projection_text_bytes,
};
use signalbox_domain::{
    AcceptedInputId, DirectModelSelection, ModelSelectionOverride, ModelTargetCatalog,
    ModelTargetDefinition, ProviderModelIdentity, ResolvedProviderTarget, SessionId,
    SubmitInputAppliedResult, SubmitInputResult, TurnId,
};
use signalbox_persistence::{
    create_session::CreateSessionRepository,
    search::SearchRepository,
    session_timeline::SessionTimelineRepository,
    submit_input::{SubmitInputHandlingOutcome, SubmitInputRepository},
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{
    EarliestQueuedTurnActivation, TestSubmitInputHandle, activate_earliest_queued_turn,
    complete_text_turn, direct, migrated_postgres, model_credential_reference, prepared,
    start_input, test_session_credential_pin,
};

const SEARCH_FIXTURE_SEED: u128 = 0x994_0000;
const LARGE_RESULT_COUNT: i32 = 251;
const LARGE_PAGE_SIZE: u16 = 100;

async fn create_search_session(pool: &PgPool, offset: u128) -> Result<SessionId, Box<dyn Error>> {
    let session_seed = SEARCH_FIXTURE_SEED + offset;
    CreateSessionRepository::new(pool.clone(), test_session_credential_pin())
        .handle(prepared(
            session_seed + 1,
            session_seed,
            direct(session_seed + 2),
        ))
        .await?;
    Ok(SessionId::from_uuid(Uuid::from_u128(session_seed)))
}

fn lexical_query(
    text: &str,
    scope: SearchScope,
    limit: u16,
    after: Option<signalbox_application::SearchCursor>,
) -> SearchQuery {
    SearchQuery {
        strategy: SearchStrategy::Lexical,
        scope,
        text: SearchText::try_new(text.to_owned()).expect("fixture query text is admitted"),
        limit: SearchPageLimit::new(limit).expect("fixture page size is bounded"),
        after,
    }
}

async fn insert_generated_projections(
    pool: &PgPool,
    session: SessionId,
    marker: &str,
    count: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO web_search_projection
            (source_kind, source_id, session_id, event_sequence,
             item_kind, item_id, turn_id, content_class, content_text)
         SELECT 'derived_artifact', md5($1 || generated::text)::uuid,
                $2, created.event_sequence,
                'derived_artifact', md5($1 || generated::text)::uuid,
                NULL, 'derived_text_artifact',
                $1 || ' result ' || generated::text
           FROM generate_series(1, $3) AS generated
           JOIN session_created_outbox_event AS created
             ON created.session_id = $2",
    )
    .bind(marker)
    .bind(session.into_uuid())
    .bind(count)
    .execute(pool)
    .await?;
    Ok(())
}

async fn session_created_address(
    pool: &PgPool,
    session: SessionId,
) -> Result<TimelineAddress, Box<dyn Error>> {
    let sequence: rust_decimal::Decimal = sqlx::query(
        "SELECT event_sequence
           FROM session_created_outbox_event
          WHERE session_id = $1",
    )
    .bind(session.into_uuid())
    .fetch_one(pool)
    .await?
    .try_get("event_sequence")?;
    let sequence = u64::try_from(sequence)?;
    Ok(TimelineAddress::new(
        std::num::NonZeroU64::new(sequence).ok_or("fixture event sequence was zero")?,
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn lexical_hit_outside_the_loaded_tail_reveals_its_exact_around_window()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = create_search_session(&pool, 0x100).await?;
    let input = AcceptedInputId::from_uuid(Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x110));
    let turn = TurnId::from_uuid(Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x111));
    let submitted = SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                SEARCH_FIXTURE_SEED + 0x112,
                session.as_uuid().as_u128(),
                "locomotive-unloaded-window",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            input,
            Some(turn),
        )
        .await?;
    let SubmitInputHandlingOutcome::Recorded(SubmitInputResult::Applied(
        SubmitInputAppliedResult::TurnOrigin(_),
    )) = submitted
    else {
        return Err("fixture input was not accepted as a turn origin".into());
    };
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                SEARCH_FIXTURE_SEED + 0x113,
                session.as_uuid().as_u128(),
                "newer unrelated tail",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x114)),
            Some(TurnId::from_uuid(Uuid::from_u128(
                SEARCH_FIXTURE_SEED + 0x115,
            ))),
        )
        .await?;
    let timeline = SessionTimelineRepository::new(pool.clone());
    let limits = TimelineWindowLimits::new(1, 256).expect("fixture limits are bounded");
    let latest = timeline
        .read_window(session, TimelineWindowAnchor::Latest, limits)
        .await?
        .expect("fixture session exists");
    let page = SearchRepository::new(pool.clone())
        .search(lexical_query(
            "locomotive unloaded",
            SearchScope::Session(session),
            10,
            None,
        ))
        .await?;
    let result = page.results.first().expect("canonical input is indexed");
    let around = timeline
        .read_window(
            session,
            TimelineWindowAnchor::Around(result.address),
            limits,
        )
        .await?
        .expect("fixture session exists");

    assert_ne!(latest.items[0].address, result.address);
    assert_eq!(around.items[0].address, result.address);
    assert_eq!(
        result.source,
        SearchResultSource::AcceptedInput { input, turn }
    );
    assert_eq!(result.content_class, SearchContentClass::UserTranscript);
    assert!(!result.highlights.is_empty());

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn derived_text_is_searchable_only_after_its_durable_publisher_runs()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = create_search_session(&pool, 0x200).await?;
    let repository = SearchRepository::new(pool.clone());
    let before = repository
        .search(lexical_query(
            "quartz derivation",
            SearchScope::Global,
            10,
            None,
        ))
        .await?;
    let artifact = SearchArtifactId::from_uuid(Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x201));
    let address = session_created_address(&pool, session).await?;
    repository
        .publish(SearchArtifactProjection {
            session,
            address,
            artifact,
            class: SearchArtifactProjectionClass::DerivedText,
            text: SearchProjectionText::try_new(String::from("quartz-derivation"))
                .expect("fixture projection text is admitted"),
        })
        .await?;
    let conflict = repository
        .publish(SearchArtifactProjection {
            session,
            address,
            artifact,
            class: SearchArtifactProjectionClass::DerivedText,
            text: SearchProjectionText::try_new(format!(
                "quartz-derivation {} conflicting-extension",
                "x".repeat(20_000)
            ))
            .expect("fixture conflicting projection is admitted"),
        })
        .await;
    let after = repository
        .search(lexical_query(
            "quartz derivation",
            SearchScope::Global,
            10,
            None,
        ))
        .await?;

    assert!(before.results.is_empty());
    assert!(conflict.is_err());
    assert_eq!(after.results.len(), 1);
    assert_eq!(
        after.results[0].content_class,
        SearchContentClass::DerivedTextArtifact
    );
    assert_eq!(
        after.results[0].source,
        SearchResultSource::DerivedArtifact { artifact }
    );
    let stored_chunks: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM web_search_projection
          WHERE source_kind = 'derived_artifact'
            AND source_id = $1
            AND content_class = 'derived_text_artifact'",
    )
    .bind(artifact.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored_chunks, 1);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oversized_canonical_assistant_text_preserves_a_searchable_tail()
-> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session_offset = 0x280;
    let session_seed = SEARCH_FIXTURE_SEED + session_offset;
    let session = create_search_session(&pool, session_offset).await?;
    let turn = TurnId::from_uuid(Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x281));
    SubmitInputRepository::new(pool.clone())
        .handle(
            start_input(
                SEARCH_FIXTURE_SEED + 0x282,
                session.as_uuid().as_u128(),
                "search an oversized assistant reply",
                1,
                ModelSelectionOverride::UseSessionDefault,
            ),
            AcceptedInputId::from_uuid(Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x283)),
            Some(turn),
        )
        .await?;
    activate_earliest_queued_turn(
        &pool,
        EarliestQueuedTurnActivation {
            session: session.into_uuid(),
            origin_entry: Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x284),
            starting_frontier: Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x285),
            initial_attempt: Uuid::from_u128(SEARCH_FIXTURE_SEED + 0x286),
        },
    )
    .await?;
    let selection = DirectModelSelection::from_uuid(Uuid::from_u128(session_seed + 2));
    let targets = ModelTargetCatalog::try_from_definitions([ModelTargetDefinition::new(
        selection,
        ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(
            SEARCH_FIXTURE_SEED + 0x287,
        ))),
    )])
    .expect("fixture selection resolves to one target");
    let boundary_lexeme = "boundarylexeme".repeat(30);
    let response = format!(
        "head-chunk-anchor {} {boundary_lexeme} {} tail-chunk-needle",
        "x".repeat(16_200),
        "x".repeat(max_search_projection_text_bytes() + 1)
    );
    complete_text_turn(
        &pool,
        session,
        targets,
        model_credential_reference(),
        SEARCH_FIXTURE_SEED + 0x290,
        &response,
    )
    .await?;
    let page = SearchRepository::new(pool.clone())
        .search(lexical_query(
            "head anchor tail needle",
            SearchScope::Session(session),
            10,
            None,
        ))
        .await?;
    let boundary_page = SearchRepository::new(pool.clone())
        .search(lexical_query(
            &boundary_lexeme,
            SearchScope::Session(session),
            10,
            None,
        ))
        .await?;
    let chunk_bounds: (i64, i32) = sqlx::query_as(
        "SELECT count(*), max(octet_length(content_text))
           FROM web_search_projection
          WHERE session_id = $1 AND content_class = 'assistant_transcript'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?;

    assert_eq!(page.results.len(), 1);
    assert_eq!(boundary_page.results.len(), 1);
    assert_eq!(
        page.results[0].content_class,
        SearchContentClass::AssistantTranscript
    );
    assert!(chunk_bounds.0 > 1);
    assert!(usize::try_from(chunk_bounds.1).expect("fixture chunk size fits") <= 65_536);

    pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn large_result_set_pages_with_stable_strict_cursors() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let session = create_search_session(&pool, 0x300).await?;
    insert_generated_projections(&pool, session, "stable-pagination", LARGE_RESULT_COUNT).await?;
    let repository = SearchRepository::new(pool.clone());
    let first = repository
        .search(lexical_query(
            "stable pagination",
            SearchScope::Session(session),
            LARGE_PAGE_SIZE,
            None,
        ))
        .await?;
    let second = repository
        .search(lexical_query(
            "stable pagination",
            SearchScope::Session(session),
            LARGE_PAGE_SIZE,
            first.next,
        ))
        .await?;
    let third = repository
        .search(lexical_query(
            "stable pagination",
            SearchScope::Session(session),
            LARGE_PAGE_SIZE,
            second.next,
        ))
        .await?;
    let repeat = repository
        .search(lexical_query(
            "stable pagination",
            SearchScope::Session(session),
            LARGE_PAGE_SIZE,
            first.next,
        ))
        .await?;

    assert_eq!(first.results.len(), usize::from(LARGE_PAGE_SIZE));
    assert_eq!(second.results.len(), usize::from(LARGE_PAGE_SIZE));
    assert_eq!(
        third.results.len(),
        usize::try_from(LARGE_RESULT_COUNT).expect("fixture count fits")
            - (usize::from(LARGE_PAGE_SIZE) * 2)
    );
    assert_eq!(second, repeat);
    assert!(first.next.is_some());
    assert!(second.next.is_some());
    assert!(third.next.is_none());
    assert_ne!(first.results.last(), second.results.first());

    let projected_count: i64 = sqlx::query(
        "SELECT count(*) AS projected_count
           FROM web_search_projection
          WHERE session_id = $1 AND content_class = 'derived_text_artifact'",
    )
    .bind(session.into_uuid())
    .fetch_one(&pool)
    .await?
    .try_get("projected_count")?;
    assert_eq!(projected_count, i64::from(LARGE_RESULT_COUNT));

    pool.close().await;
    drop(container);
    Ok(())
}

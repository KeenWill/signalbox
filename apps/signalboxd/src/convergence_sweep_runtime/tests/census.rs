use super::*;

struct CensusFixture {
    runtime: ConvergenceSweepRuntime,
    target: SweepTarget,
    _credentials: tempfile::NamedTempFile,
    _work_source: InProcessEligibilityWorkSource<PostgresEligibilitySweep>,
    server: tokio::task::JoinHandle<()>,
    requests: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for CensusFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl CensusFixture {
    async fn new(pool: &PgPool, recording_name: &str) -> Result<Self, Box<dyn Error>> {
        use std::io::Write;
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/convergence");
        let recording = signalbox_convergence::Recording::read(
            &root.join("fixtures/mutations").join(recording_name),
        )?;
        let policy = ConvergencePolicy::read(&root.join("examples/repository.toml"))?;
        let (mut runtime, work_source) = fixture_runtime(pool, Duration::from_secs(60))?;
        runtime.convergence_policy = Some(policy);
        let mut credentials = tempfile::NamedTempFile::new()?;
        credentials.write_all(b"synthetic-convergence-token")?;
        let reference = CredentialReference::new("fixture-credential");
        let target = SweepTarget {
            repository: RepositorySlug::try_new(recording.repository.to_lowercase())?,
            pull_request: PullRequestNumber::new(
                std::num::NonZeroU64::new(recording.number).expect("recorded PR is positive"),
            ),
            credentials: FileCredentialAccess::new(
                credentials.path().to_path_buf(),
                reference.clone(),
            ),
            credential_reference: reference,
        };
        runtime.convergence_history.lock().await.insert(
            format!(
                "{}#{}",
                target.repository.as_str(),
                target.pull_request.get()
            ),
            recording.previous.clone(),
        );
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fixture_requests = Arc::clone(&requests);
        let recording = Arc::new(recording);
        let router = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let recording = Arc::clone(&recording);
            let requests = Arc::clone(&fixture_requests);
            async move {
                requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(
                    request.headers()[AUTHORIZATION],
                    "Bearer synthetic-convergence-token"
                );
                let path = request.uri().path().to_owned();
                let response = if request.method() == reqwest::Method::POST {
                    let bytes = axum::body::to_bytes(request.into_body(), MAX_RESPONSE_BYTES)
                        .await
                        .expect("fixture request body is bounded");
                    let body: Value = serde_json::from_slice(&bytes).expect("request is JSON");
                    recording
                        .observations
                        .iter()
                        .flatten()
                        .find(|response| {
                            response.query == body["query"].as_str().unwrap_or_default()
                                && response.variables.get("id") == body["variables"].get("id")
                                && response.variables.get("after") == body["variables"].get("after")
                        })
                        .expect("every GraphQL request has recorded evidence")
                        .response
                        .clone()
                } else if let Some((_, comparison)) = path.split_once("/compare/") {
                    recording
                        .comparisons
                        .get(comparison)
                        .cloned()
                        .unwrap_or(Value::Null)
                } else {
                    let sha = path.rsplit('/').next().expect("REST path has a suffix");
                    recording.blobs.get(sha).cloned().unwrap_or(Value::Null)
                };
                axum::Json(response)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        runtime.rest_base = format!("http://{}/", listener.local_addr()?);
        runtime.graphql_url = format!("{}graphql", runtime.rest_base);
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("local census server runs");
        });
        Ok(Self {
            runtime,
            target,
            _credentials: credentials,
            _work_source: work_source,
            server,
            requests,
        })
    }

    async fn reconcile(&self) {
        self.runtime
            .reconcile_target(&self.target, Instant::now() + Duration::from_secs(30))
            .await;
    }

    fn enable_template(&mut self) -> Result<(), Box<dyn Error>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/session-templates.example.toml");
        self.runtime.templates =
            SessionTemplateConfiguration::read(&path, || None, &self.runtime.models)?;
        self.runtime.template =
            signalbox_domain::SessionTemplateName::try_new("merge-forward".to_owned())?;
        Ok(())
    }

    async fn observation(&self) -> Result<ConvergenceSweepObservation, Box<dyn Error>> {
        let fetched = self
            .runtime
            .fetch(&self.target)
            .await
            .map_err(|error| format!("fixture census failed: {error:?}"))?;
        Ok(ConvergenceSweepObservation::new(
            fetched.head_sha,
            fetched.evaluation.unresolved_review_threads as u64,
        ))
    }
}

async fn last_census_event(pool: &PgPool) -> Result<(String, Option<String>), Box<dyn Error>> {
    Ok(sqlx::query_as("SELECT outcome_kind, failure_kind FROM convergence_sweep_event ORDER BY recorded_at DESC, event_id DESC LIMIT 1")
        .fetch_one(pool).await?)
}

async fn census_dispatch(
    pool: &PgPool,
) -> Result<
    (
        signalbox_domain::CommissionedDispatchId,
        signalbox_domain::SessionId,
    ),
    Box<dyn Error>,
> {
    let (dispatch, session): (uuid::Uuid, uuid::Uuid) =
        sqlx::query_as("SELECT last_dispatch_id, last_session_id FROM convergence_sweep_target")
            .fetch_one(pool)
            .await?;
    Ok((
        signalbox_domain::CommissionedDispatchId::from_uuid(dispatch),
        signalbox_domain::SessionId::from_uuid(session),
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_complete_local_census_records_convergence() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = CensusFixture::new(&pool, "settled.json").await?;
    fixture.reconcile().await;
    assert_eq!(
        last_census_event(&pool).await?,
        ("converged".to_owned(), None)
    );
    assert!(
        fixture.requests.load(std::sync::atomic::Ordering::Relaxed) >= 7,
        "both three-query observations and REST evidence traverse the injected HTTP endpoints"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_nonconverged_census_records_missing_template_drift() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    fixture.reconcile().await;
    assert_eq!(
        last_census_event(&pool).await?.1.as_deref(),
        Some("template_drift")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_nonconverged_census_commissions_and_retains_a_live_session() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    fixture.enable_template()?;
    fixture.reconcile().await;
    let first = census_dispatch(&pool).await?;
    fixture.reconcile().await;
    assert_eq!(census_dispatch(&pool).await?, first);
    assert_eq!(
        last_census_event(&pool).await?,
        ("live_session".to_owned(), None)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unchanged_census_parks_a_session_without_model_activity_after_cool_off()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    fixture.enable_template()?;
    fixture.reconcile().await;
    let (_, session) = census_dispatch(&pool).await?;
    fixture.runtime.cool_off = Duration::ZERO;
    fixture.reconcile().await;
    let parked: (String, uuid::Uuid) =
        sqlx::query_as("SELECT state_kind, parked_session_id FROM convergence_sweep_target")
            .fetch_one(&pool)
            .await?;
    assert_eq!(parked, ("parked".to_owned(), session.into_uuid()));
    assert_eq!(
        last_census_event(&pool).await?.1.as_deref(),
        Some("no_model_activity")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_nonconverged_census_preserves_a_terminal_sessions_cool_off() -> Result<(), Box<dyn Error>>
{
    use signalbox_domain::{
        DescendantTerminationScope, GoalCommandResult, GoalUserAction, GoalUserCommand,
    };
    use signalbox_persistence::goal::{GoalCommandHandlingOutcome, GoalRepository};
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    fixture.enable_template()?;
    fixture.reconcile().await;
    let first = census_dispatch(&pool).await?;
    let stopped = GoalRepository::new(pool.clone())
        .handle_user_command(
            GoalUserCommand::new(
                DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
                first.1,
                GoalUserAction::Stop {
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                },
            ),
            None,
            |_| None,
        )
        .await?;
    assert!(matches!(
        stopped,
        GoalCommandHandlingOutcome::Recorded(GoalCommandResult::Applied(_))
    ));
    fixture.reconcile().await;
    assert_eq!(census_dispatch(&pool).await?, first);
    assert_eq!(
        last_census_event(&pool).await?,
        ("cooling_off".to_owned(), None)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_racing_busy_commission_records_a_live_session_decision() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    let observation = fixture.observation().await?;
    fixture
        .runtime
        .record_commission_outcome(
            &fixture.target,
            &observation,
            Ok(CommissionDispatchOutcome::TargetBusy {
                session: signalbox_domain::SessionId::from_uuid(uuid::Uuid::now_v7()),
            }),
        )
        .await;
    assert_eq!(
        last_census_event(&pool).await?,
        ("live_session".to_owned(), None)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_racing_cooling_commission_records_a_cooling_decision() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    let observation = fixture.observation().await?;
    fixture
        .runtime
        .record_commission_outcome(
            &fixture.target,
            &observation,
            Ok(CommissionDispatchOutcome::TargetCoolingOff {
                session: signalbox_domain::SessionId::from_uuid(uuid::Uuid::now_v7()),
            }),
        )
        .await;
    assert_eq!(
        last_census_event(&pool).await?,
        ("cooling_off".to_owned(), None)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_conflicting_commission_is_recorded_as_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    let observation = fixture.observation().await?;
    fixture
        .runtime
        .record_commission_outcome(
            &fixture.target,
            &observation,
            Ok(CommissionDispatchOutcome::ConflictingReuse),
        )
        .await;
    assert_eq!(
        last_census_event(&pool).await?.1.as_deref(),
        Some("commission_refused")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_unambiguous_commission_error_is_recorded_as_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    let observation = fixture.observation().await?;
    fixture.runtime.record_commission_outcome(&fixture.target, &observation,
        Err(signalbox_persistence::commissioned_dispatch::CommissionedDispatchRepositoryError::Database {
            source: sqlx::Error::PoolTimedOut, commit_ambiguous: false,
        })).await;
    assert_eq!(
        last_census_event(&pool).await?.1.as_deref(),
        Some("commission_refused")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_ambiguous_commission_error_does_not_invent_a_refusal() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    let observation = fixture.observation().await?;
    fixture.runtime.record_commission_outcome(&fixture.target, &observation,
        Err(signalbox_persistence::commissioned_dispatch::CommissionedDispatchRepositoryError::Database {
            source: sqlx::Error::PoolTimedOut, commit_ambiguous: true,
        })).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM convergence_sweep_event")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_replayed_commission_preserves_its_dispatch_projection() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let mut fixture = CensusFixture::new(&pool, "base-conflict.json").await?;
    fixture.enable_template()?;
    fixture.reconcile().await;
    let first = census_dispatch(&pool).await?;
    let observation = fixture.observation().await?;
    fixture
        .runtime
        .record_commission_outcome(
            &fixture.target,
            &observation,
            Ok(CommissionDispatchOutcome::Replayed {
                dispatch: first.0,
                session: first.1,
            }),
        )
        .await;
    assert_eq!(census_dispatch(&pool).await?, first);
    assert_eq!(
        last_census_event(&pool).await?,
        ("dispatched".to_owned(), None)
    );
    Ok(())
}
